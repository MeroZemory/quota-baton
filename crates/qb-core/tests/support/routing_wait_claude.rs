//! Real Claude HTTP headers -> retained proof wait -> one same-source retry.
use super::*;

const CLAUDE_UPLOAD: &str = r#"{ "model": "synthetic-claude-http-wait", "max_tokens": 8, "messages": [{"role":"user","content":"conversation-marker"}], "metadata": {"user_id":"synthetic-retained"} }"#;
const CLAUDE_FINAL: &str = r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"synthetic-retry"}],"stop_reason":"end_turn"}"#;

async fn claude_warmed(session: &str) -> (Harness, String) {
    let h = Harness::with_initial_usage(
        Provider::Claude,
        60,
        30,
        60,
        |config| {
            config.routing.move_guard.trip_seconds = 5;
            config.guard.retain_max_per_request_mib = 1;
            config.routing.validate().unwrap();
        },
        false,
    )
    .await;
    assert!(h.servers.data_addr.ip().is_loopback());
    let source = h
        .turn(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", session),
                ("anthropic-version", "2023-06-01"),
            ],
            CLAUDE_UPLOAD,
        )
        .await;
    for id in h.ids() {
        if id != source {
            h.pause(&id).await;
        }
    }
    timeout(PHASE, idle(&h, &source))
        .await
        .expect("Claude warm request must release its native counters");
    assert!(
        h.obs.quota_snapshot(&source).is_none(),
        "Claude proof fixture must start without seeded or cached quota evidence"
    );
    assert_eq!(
        h.state
            .bindings
            .policies
            .plan(Some(&format!("claude:{session}")))
            .settings
            .reset_wait_secs,
        60
    );
    assert_eq!(h.fake.state.requests().len(), 1);
    (h, source)
}

fn claude_limit_response() -> (chrono::DateTime<Utc>, ScriptedResponse) {
    let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 10, 0).unwrap();
    let mut response = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, "");
    for (name, value) in [
        ("anthropic-ratelimit-unified-5h-utilization", "1".to_owned()),
        (
            "anthropic-ratelimit-unified-5h-status",
            "rejected".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-5h-reset",
            reset.timestamp().to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-utilization",
            "0.2".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-7d-status",
            "allowed".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-7d-reset",
            (reset + ChronoDuration::days(1)).timestamp().to_string(),
        ),
    ] {
        response.headers.insert(name, value.parse().unwrap());
    }
    // Actual provider headers on a complete empty 429; no raw-store changes,
    // direct proof mint, cached report, or synthetic native outcome override.
    (reset, response)
}

fn assert_claude_waiting(
    h: &Harness,
    source: &str,
    session: &str,
    reset: chrono::DateTime<Utc>,
    sent_after: chrono::DateTime<Utc>,
) {
    assert_eq!(h.fake.state.requests().len(), 2, "Claude retry sent early");
    assert_eq!(h.state.inflight_requests.load(Ordering::Acquire), 1);
    assert_eq!(h.state.accounts.inflight_count(source), 0);
    assert_eq!(h.state.bindings.pending_count(source), 0);
    assert!(
        h.state.budget.used() >= CLAUDE_UPLOAD.len(),
        "Claude wait must retain the original upload reservation"
    );
    assert_eq!(
        h.state
            .bindings
            .current(Some(&format!("claude:{session}")))
            .as_deref(),
        Some(source)
    );
    let account = h.store.load(source).unwrap();
    assert_eq!(account.provider, Provider::Claude);
    assert!(matches!(account.health,
        Health::Cooling { reason: HealthReason::Quota, until, ref scope }
            if until == reset && scope == "account"
    ));
    assert!(!account.quota_credit_releasable);
    let hold = account.quota_cooling.as_ref().unwrap();
    assert_eq!(hold.window, "5h");
    assert_eq!(hold.reset_at, reset);
    assert!(hold.started_at >= sent_after && hold.started_at <= Utc::now());
    let snapshot = h.obs.quota_snapshot(source).unwrap();
    let windows = snapshot["windows"].as_array().unwrap();
    assert_eq!(
        windows.len(),
        2,
        "only the actual full Claude report is used"
    );
    for (name, used, status, expected_reset) in [
        ("5h", 100.0, "rejected", reset),
        ("7d", 20.0, "allowed", reset + ChronoDuration::days(1)),
    ] {
        let window = windows.iter().find(|w| w["name"] == name).unwrap();
        assert_eq!(window["used_pct"].as_f64(), Some(used));
        assert_eq!(window["status"], status);
        assert_eq!(
            window["reset_at"]
                .as_str()
                .unwrap()
                .parse::<chrono::DateTime<Utc>>()
                .unwrap(),
            expected_reset
        );
        let observed = window["observed_at"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        assert!(observed >= sent_after && observed <= Utc::now());
    }
}

fn assert_claude_attempts(h: &Harness, session: &str, expected: usize) {
    let attempts = h.fake.state.requests();
    assert_eq!(attempts.len(), expected);
    let credential = attempts[0].headers.get("authorization").unwrap();
    assert!(!credential.is_empty());
    for attempt in &attempts {
        assert_eq!(attempt.method, reqwest::Method::POST);
        assert_eq!(attempt.path_and_query, "/v1/messages");
        assert_eq!(attempt.body.as_ref(), CLAUDE_UPLOAD.as_bytes());
        assert_eq!(attempt.headers["x-claude-code-session-id"], session);
        assert_eq!(attempt.headers["anthropic-version"], "2023-06-01");
        assert!(
            attempt.headers.get("authorization") == Some(credential),
            "Claude retry changed the synthetic source credential"
        );
    }
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "binding_move" && !row["from"].is_null()),
        "Claude same-account reset wait must not move its binding"
    );
}

#[tokio::test]
async fn n5a_claude_http_proof_wait_reset_reselects_same_account_once() {
    let session = "98dfe746-6fde-44eb-8e12-e2c539d45ac1";
    let (h, source) = claude_warmed(session).await;
    let mut events = h.state.subscribe_http_quota_wait();
    let (reset, limit) = claude_limit_response();
    h.fake.state.push(limit);
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, CLAUDE_FINAL));
    let (first_seen, first_release) = h.fake.state.pause_next_response();
    let sent_after = Utc::now();
    // The first sample precedes the wall-clock sample, so this is a lower
    // monotonic bound on reset, checked against actual upstream receipt time.
    let reset_clock = Instant::now();
    let earliest_retry = reset_clock + (reset - Utc::now()).to_std().unwrap();
    let send = h
        .client
        .post(format!("http://{}/v1/messages", h.servers.data_addr))
        .bearer_auth("synthetic-client")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("x-claude-code-session-id", session)
        .timeout(Duration::from_secs(30))
        .body(CLAUDE_UPLOAD)
        .send();
    let observe = async {
        timeout(PHASE, first_seen)
            .await
            .expect("Claude target upload must reach the actual upstream")
            .expect("Claude initial upstream gate abandoned");
        assert_eq!(h.fake.state.requests().len(), 2);
        first_release.send(()).unwrap();
        event(
            &mut events,
            "admitted",
            &source,
            PHASE,
            "Claude full native headers must admit the HTTP proof wait before reset",
        )
        .await;
        assert!(
            reset - Utc::now() > ChronoDuration::seconds(4),
            "Claude fixture setup consumed the pre-reset observation margin"
        );
        assert_claude_waiting(&h, &source, session, reset, sent_after);
        let (retry_seen, retry_release) = h.fake.state.pause_next_response();
        let remaining = (reset - Utc::now()).to_std().unwrap();
        event(
            &mut events,
            "reselect",
            &source,
            remaining + PHASE,
            "Claude reset retry must pass normal selection",
        )
        .await;
        assert!(Utc::now() >= reset, "Claude reselect preceded actual reset");
        timeout(PHASE, retry_seen)
            .await
            .expect("Claude normal selection must dispatch the retained retry")
            .expect("Claude retry upstream gate abandoned");
        assert_claude_attempts(&h, session, 3);
        assert!(
            h.fake.state.requests()[2].received_at >= earliest_retry,
            "Claude retry reached upstream before the original reset"
        );
        retry_release.send(()).unwrap();
    };
    let (response, ()) = timeout(Duration::from_secs(35), async {
        tokio::join!(send, observe)
    })
    .await
    .expect("bounded Claude HTTP reset fixture timed out");
    let response = response.expect("actual Claude HTTP retry request failed");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-quota-baton-account"], source.as_str());
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        CLAUDE_FINAL.as_bytes()
    );
    timeout(PHASE, idle(&h, &source))
        .await
        .expect("Claude retry must release every native request/body reservation");
    assert_claude_attempts(&h, session, 3);
    assert_eq!(
        h.state
            .bindings
            .current(Some(&format!("claude:{session}")))
            .as_deref(),
        Some(source.as_str())
    );
    assert_no_more_events(&mut events);
}
