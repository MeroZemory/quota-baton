//! Child of routing_wait_http; proof is produced before either target request.
use super::*;

async fn entry_fixture(session: &str, hold: u64) -> (Harness, String, String, ScriptedUpstream) {
    let usage = timeout(PHASE, ScriptedUpstream::start())
        .await
        .unwrap()
        .unwrap();
    let h = Harness::with_limits_and_config(Provider::Codex, hold, 30, 60, |config| {
        config.routing.move_guard.trip_seconds = 5;
        config.guard.retain_max_per_request_mib = 1;
        config.upstream.codex_usage_base = usage.url.clone();
        config.routing.validate().unwrap();
    })
    .await;
    assert!(h.servers.data_addr.ip().is_loopback());
    let source = h
        .turn("/v1/responses", &[("session-id", session)], UPLOAD)
        .await;
    let alternate = h.ids().into_iter().find(|id| id != &source).unwrap();
    h.admin(
        reqwest::Method::POST,
        "/admin/v1/routing",
        Some(json!({
            "kind":"session", "id":qb_core::routing::session_id(&format!("codex:{session}")),
            "settings":{"mode":"recover", "reset_wait_secs":hold}
        })),
    )
    .await;
    timeout(PHASE, idle(&h, &source)).await.unwrap();
    assert_eq!(h.fake.state.requests().len(), 1);
    assert!(usage.state.requests().is_empty());
    assert_eq!(
        h.state
            .bindings
            .policies
            .plan(Some(&format!("codex:{session}")))
            .settings
            .reset_wait_secs,
        hold
    );
    (h, source, alternate, usage)
}

async fn full_poll_before_request(
    h: &Harness,
    usage: &ScriptedUpstream,
    source: &str,
    reset: chrono::DateTime<Utc>,
) {
    // Same complete account-scoped wham shape as the passing real-poll test.
    // No model field, header producer, direct apply, or saved Cooling injection.
    usage.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({"rate_limit": {
            "primary_window":{"used_percent":100, "limit_window_seconds":18000,
                "reset_at":reset.timestamp()},
            "secondary_window":{"used_percent":10, "limit_window_seconds":604800,
                "reset_at":(reset + ChronoDuration::hours(47)).timestamp()},
            "active_limit":"5h"
        }})
        .to_string(),
    ));
    let account = timeout(PHASE, h.state.quota.poll(source))
        .await
        .expect("actual full poll must finish before target HTTP request")
        .expect("full poll must be accepted by the native reducer");
    assert!(matches!(account.health, Health::Cooling {
        reason: HealthReason::Quota, until, ref scope,
    } if until == reset && scope == "account"));
    let cooling = account.quota_cooling.unwrap();
    assert_eq!(cooling.window, "5h");
    assert_eq!(cooling.reset_at, reset);
    let requests = usage.state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, axum::http::Method::GET);
    assert_eq!(requests[0].path_and_query, "/backend-api/wham/usage");
    assert!(requests[0].body.is_empty());
    assert!(requests[0].headers.get("authorization").is_some());
    assert!(
        requests[0].headers.get("authorization")
            == h.fake.state.requests()[0].headers.get("authorization")
    );
    assert_eq!(
        h.fake.state.requests().len(),
        1,
        "poll is not a data attempt"
    );
}

#[tokio::test]
async fn n5a_http_full_poll_before_request_admits_initial_wait_then_sends_same_account_once() {
    let session = "synthetic-proof-entry-reset";
    let (h, source, alternate, usage) = entry_fixture(session, 60).await;
    h.pause(&alternate).await;
    let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 10, 0).unwrap();
    full_poll_before_request(&h, &usage, &source, reset).await;
    assert!(reset - Utc::now() > ChronoDuration::seconds(4));
    // Sample monotonic first: this conservative lower bound cannot move the
    // asserted reset later merely because the paired clock reads take time.
    let sampled_mono = Instant::now();
    let earliest_send = sampled_mono + (reset - Utc::now()).to_std().unwrap();
    let mut events = h.state.subscribe_http_quota_wait();
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, FINAL));
    let (sent, release) = h.fake.state.pause_next_response();
    let send = h
        .client
        .post(format!("http://{}/v1/responses", h.servers.data_addr))
        .bearer_auth("synthetic-client")
        .header("content-type", "application/json")
        .header("session-id", session)
        .timeout(Duration::from_secs(30))
        .body(UPLOAD)
        .send();
    let observe = async {
        event(
            &mut events,
            "admitted",
            &source,
            PHASE,
            "pre-existing full poll must admit initial HTTP wait before any data send",
        )
        .await;
        assert!(Utc::now() < reset);
        assert_eq!(
            h.fake.state.requests().len(),
            1,
            "zero target attempts before reset"
        );
        assert_eq!(h.state.inflight_requests.load(Ordering::Acquire), 1);
        assert_eq!(h.state.accounts.inflight_count(&source), 0);
        assert_eq!(h.state.bindings.pending_count(&source), 0);
        event(
            &mut events,
            "reselect",
            &source,
            (reset - Utc::now()).to_std().unwrap() + PHASE,
            "initial proof wait must return to normal selection at reset",
        )
        .await;
        assert!(Utc::now() >= reset);
        timeout(PHASE, sent)
            .await
            .expect("normal chooser must send once after reset")
            .unwrap();
        assert_exact_attempts(&h, 2);
        assert!(
            h.fake.state.requests()[1].received_at >= earliest_send,
            "target upstream request arrived before reset"
        );
        release.send(()).unwrap();
    };
    let (response, ()) = timeout(Duration::from_secs(30), async {
        tokio::join!(send, observe)
    })
    .await
    .unwrap();
    let response = response.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-quota-baton-account"], source.as_str());
    assert_eq!(
        timeout(PHASE, response.bytes())
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        FINAL.as_bytes()
    );
    timeout(PHASE, idle(&h, &source)).await.unwrap();
    assert_exact_attempts(&h, 2);
    assert_eq!(usage.state.requests().len(), 1);
    assert_eq!(
        h.state
            .bindings
            .current(Some(&format!("codex:{session}")))
            .as_deref(),
        Some(source.as_str())
    );
    assert_no_more_events(&mut events);
}

#[tokio::test]
async fn n5a_http_full_poll_reset_beyond_hold_declines_initial_wait_and_normally_sends_b() {
    let session = "synthetic-proof-entry-beyond-bound";
    const HOLD: u64 = 5;
    let (h, source, alternate, usage) = entry_fixture(session, HOLD).await;
    let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 20, 0).unwrap();
    full_poll_before_request(&h, &usage, &source, reset).await;
    assert!((reset - Utc::now()).to_std().unwrap() > Duration::from_secs(HOLD) + PHASE);
    assert!((reset - Utc::now()).to_std().unwrap() < Duration::from_secs(30));
    let b = h.store.load(&alternate).unwrap();
    assert_eq!(b.user_state, qb_core::accounts::UserState::Active);
    assert!(matches!(b.health, Health::Ready));
    let b_tokens = h.store.tokens(&b).unwrap();
    let b_access = &b_tokens.access_token;
    let mut events = h.state.subscribe_http_quota_wait();
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, FINAL));
    let start = Instant::now();
    // Client's real budget remains 30s. The independent observation bound is
    // shorter than HOLD, so an unwanted native five-second hold cannot pass.
    let response = timeout(
        PHASE,
        h.client
            .post(format!("http://{}/v1/responses", h.servers.data_addr))
            .bearer_auth("synthetic-client")
            .header("content-type", "application/json")
            .header("session-id", session)
            .timeout(Duration::from_secs(30))
            .body(UPLOAD)
            .send(),
    )
    .await
    .expect("over-bound proof must fall back without holding")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-quota-baton-account"],
        alternate.as_str()
    );
    assert_eq!(
        timeout(PHASE, response.bytes())
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        FINAL.as_bytes()
    );
    assert!(start.elapsed() < Duration::from_secs(HOLD));
    assert!(Utc::now() < reset);
    let attempts = h.fake.state.requests();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].path_and_query, "/responses");
    assert_eq!(attempts[1].body.as_ref(), UPLOAD.as_bytes());
    assert!(
        attempts[1].headers.get("authorization").is_some_and(
            |header| header.to_str().ok() == Some(format!("Bearer {b_access}").as_str())
        )
    );
    assert!(attempts[1].headers.get("authorization") != attempts[0].headers.get("authorization"));
    assert_no_more_events(&mut events);
    timeout(PHASE, idle(&h, &source)).await.unwrap();
    assert_eq!(h.state.accounts.inflight_count(&alternate), 0);
    assert_eq!(h.state.bindings.pending_count(&alternate), 0);
    assert_eq!(
        h.state
            .bindings
            .current(Some(&format!("codex:{session}")))
            .as_deref(),
        Some(alternate.as_str())
    );
    assert_eq!(usage.state.requests().len(), 1);
    assert_eq!(h.fake.state.requests().len(), 2);
}

#[path = "n5c_policy_wake.rs"]
mod n5c_policy_wake;
