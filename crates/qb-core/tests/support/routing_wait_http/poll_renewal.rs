//! Child of routing_wait_http; real poller, no new observer or producer seam.
use super::*;

async fn warmed_with_usage(session: &str) -> (Harness, String, ScriptedUpstream) {
    let usage = timeout(PHASE, ScriptedUpstream::start())
        .await
        .unwrap()
        .unwrap();
    let h = Harness::with_limits_and_config(Provider::Codex, 60, 30, 60, |config| {
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
    for id in h.ids() {
        if id != source {
            h.pause(&id).await;
        }
    }
    timeout(PHASE, idle(&h, &source))
        .await
        .expect("warm HTTP request must release native reservations");
    assert_eq!(h.fake.state.requests().len(), 1);
    assert!(usage.state.requests().is_empty(), "unexpected warmup poll");
    assert_eq!(
        h.state
            .bindings
            .policies
            .plan(Some(&format!("codex:{session}")))
            .settings
            .reset_wait_secs,
        60
    );
    (h, source, usage)
}

#[tokio::test]
async fn n5a_http_full_account_poll_preserves_model_wait_and_one_reset_retry() {
    let session = "synthetic-proof-full-poll";
    let (h, source, usage) = warmed_with_usage(session).await;
    let key = format!("codex:{session}");
    assert_eq!(
        serde_json::from_str::<Value>(UPLOAD).unwrap()["model"],
        "synthetic-model-http-wait"
    );
    let mut events = h.state.subscribe_http_quota_wait();
    let (reset, limit) = reset_response();
    h.fake.state.push(limit);
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, FINAL));
    // Genuine wham shape: both windows, matching limiting reset, no model.
    usage.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({"rate_limit": {
            "primary_window": {"used_percent":100, "limit_window_seconds":18000,
                "reset_at":reset.timestamp()},
            "secondary_window": {"used_percent":10, "limit_window_seconds":604800,
                "reset_at":(reset + ChronoDuration::hours(47)).timestamp()},
            "active_limit":"5h"
        }})
        .to_string(),
    ));
    let (first_seen, first_release) = h.fake.state.pause_next_response();
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
        timeout(PHASE, first_seen)
            .await
            .expect("retained model request must reach the actual upstream")
            .expect("initial HTTP response gate abandoned");
        first_release.send(()).unwrap();
        event(
            &mut events,
            "admitted",
            &source,
            PHASE,
            "model HTTP proof wait must be admitted before the full poll",
        )
        .await;
        assert_waiting(&h, &source, session);
        assert!(
            reset - Utc::now() > ChronoDuration::seconds(6),
            "fixture needs a real pre-reset poll/reselection margin"
        );
        let (poll_seen, poll_release) = usage.state.pause_next_response();
        let poll = h.state.quota.poll(&source);
        let verify_get = async {
            timeout(PHASE, poll_seen)
                .await
                .expect("public poll must issue the actual usage GET")
                .expect("usage response gate abandoned");
            let requests = usage.state.requests();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.method, axum::http::Method::GET);
            assert_eq!(request.path_and_query, "/backend-api/wham/usage");
            assert!(request.body.is_empty());
            let data = h.fake.state.requests();
            assert!(
                request.headers.get("authorization").is_some()
                    && request.headers.get("authorization") == data[0].headers.get("authorization"),
                "usage GET must use the same synthetic source credential"
            );
            assert_waiting(&h, &source, session);
            // Do not clear/suppress native probes, defer notifications, or call
            // a direct apply bridge. The real poll owns all native sequencing.
            poll_release.send(()).unwrap();
        };
        let (reduced, ()) = timeout(PHASE, async { tokio::join!(poll, verify_get) })
            .await
            .expect("real full usage poll must finish before reset");
        let reduced = reduced.expect("actual full usage poll was not accepted");
        assert!(
            matches!(reduced.health, Health::Cooling {
                reason: HealthReason::Quota, until, ref scope,
            } if until == reset && scope == "account"),
            "real model=None poll must preserve account-scoped quota cooling"
        );
        assert_eq!(reduced.quota_cooling.unwrap().reset_at, reset);
        assert_eq!(usage.state.requests().len(), 1);
        assert_waiting(&h, &source, session);
        assert!(Utc::now() < reset, "full poll must finish before reset");
        let (retry_seen, retry_release) = h.fake.state.pause_next_response();
        event(
            &mut events,
            "reselect",
            &source,
            (reset - Utc::now()).to_std().unwrap() + PHASE,
            "accepted full account poll must preserve the model HTTP wait to normal reset reselection",
        )
        .await;
        assert!(Utc::now() >= reset, "normal reset reselect happened early");
        timeout(PHASE, retry_seen)
            .await
            .expect("renewed native wait must dispatch its retained retry")
            .expect("retry response gate abandoned");
        assert_exact_attempts(&h, 3);
        retry_release.send(()).unwrap();
    };
    let (response, ()) = timeout(Duration::from_secs(35), async {
        tokio::join!(send, observe)
    })
    .await
    .expect("bounded actual HTTP/full poll fixture timed out");
    let response = response.expect("model HTTP request failed after full poll");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-quota-baton-account"], source.as_str());
    assert_eq!(response.bytes().await.unwrap().as_ref(), FINAL.as_bytes());
    timeout(PHASE, idle(&h, &source))
        .await
        .expect("request must release native upload/dispatch reservations");
    assert_eq!(
        h.state.bindings.current(Some(&key)).as_deref(),
        Some(source.as_str())
    );
    assert_exact_attempts(&h, 3);
    assert_eq!(usage.state.requests().len(), 1);
    assert_no_more_events(&mut events);
}
