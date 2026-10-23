//! Child of entry::n5c_policy_wake. Actual full poll and authenticated policy;
//! no provider rejection, saved Cooling, proof constructor or receipt injection.
use super::*;

#[path = "n5c_native_owner.rs"]
pub(super) mod native_owner;
use native_owner::NativeOwner;

const INITIAL_FAR_MARKER: &str =
    "connected native owner must retain D26 typed 429 before selecting B";

async fn initial_fixture(
    session: &str,
    mode: &str,
    hold: u64,
    urgency_value: &str,
) -> (Harness, String, String, ScriptedUpstream) {
    let (h, a, b, usage) = entry_fixture(session, hold).await;
    timeout(
        PHASE,
        h.admin(
            reqwest::Method::POST,
            "/admin/v1/routing",
            Some(json!({
                "kind":"session", "id":qb_core::routing::session_id(&format!("codex:{session}")),
                "settings":{"mode":mode, "reset_wait_secs":hold}
            })),
        ),
    )
    .await
    .unwrap();
    urgency(&h, session, urgency_value).await;
    let alternate = h.store.load(&b).unwrap();
    assert_eq!(alternate.user_state, qb_core::accounts::UserState::Active);
    assert!(matches!(alternate.health, Health::Ready));
    (h, a, b, usage)
}

async fn initial_request(h: &Harness, session: &str) -> reqwest::Response {
    timeout(
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
    .expect("initial fallback/native move must finish within the observation bound")
    .unwrap()
}

async fn assert_initial_limit(mode: &str, hold: u64, reset_seconds: i64) {
    let session = "34567890-3456-4cde-8fab-34567890abcd".to_owned();
    let (h, a, b, usage) = initial_fixture(&session, mode, hold, "wait").await;
    let owner = NativeOwner::connect(&h, &session, &a).await;
    let reset =
        chrono::DateTime::from_timestamp(Utc::now().timestamp() + reset_seconds, 0).unwrap();
    native_owner::full_poll(&h, &usage, &a, reset).await;
    if reset_seconds == 45 {
        assert!((reset - Utc::now()).to_std().unwrap() < Duration::from_secs(hold));
        assert!((reset - Utc::now()).to_std().unwrap() > Duration::from_secs(30) + PHASE);
    }
    let mut events = h.state.subscribe_http_quota_wait();
    let mut phases = h.state.subscribe_http_quota_wait_inspections();
    // B is eligible and has a genuine response available to expose old fallback.
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, FINAL));
    let start = Instant::now();
    let response = owner.request(&h, &session).await;
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "{INITIAL_FAR_MARKER}"
    );
    assert!(start.elapsed() < PHASE);
    assert!(Utc::now() < reset);
    // This response is not upstream bytes or invented provider headers.
    assert!(response.headers().get("x-codex-primary-reset-at").is_none());
    assert!(response.headers().get("x-quota-baton-account").is_none());
    assert_eq!(response.headers()["cache-control"], "no-store");
    let retry = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    assert!(retry > 0 && retry <= reset_seconds + 1);
    let body = timeout(PHASE, response.bytes()).await.unwrap().unwrap();
    assert!(body.len() < 2048);
    owner.assert_private_body(&body);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["type"], "quota_baton_wait");
    assert_eq!(value["error"]["source"], "quota-baton");
    assert_eq!(value["error"]["window"], "5h");
    assert_eq!(value["error"]["on_5h_limit"], "wait");
    assert_eq!(value["error"]["resets_at"], reset.timestamp());
    assert_eq!(value["error"]["resets_in_seconds"], retry);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("quota-baton:")
    );
    let text = std::str::from_utf8(&body).unwrap();
    for private in [
        a.as_str(),
        b.as_str(),
        session.as_str(),
        "synthetic-model-http-wait",
        "conversation-marker",
    ] {
        assert!(
            !text.contains(private),
            "qb wait response must not disclose request/account identity"
        );
    }
    timeout(PHASE, idle(&h, &a)).await.unwrap();
    assert_eq!(h.state.accounts.inflight_count(&b), 0);
    assert_eq!(h.state.bindings.pending_count(&b), 0);
    native_owner::assert_same_source_attempts(&h, 1); // warm A only
    assert_eq!(usage.state.requests().len(), 1);
    assert_eq!(
        h.state
            .bindings
            .current(Some(&format!("codex:{session}")))
            .as_deref(),
        Some(a.as_str())
    );
    assert_no_more_events(&mut events); // no WAIT receipt/admission/reselection
    assert_no_more_events(&mut phases); // G2 engine is not entered on typed fallback
    owner.finish(&session, reset).await;
}

#[tokio::test]
async fn n5c_http_initial_far_connected_native_owner_keeps_typed_429_without_data_attempt() {
    for mode in ["recover", "rebalance"] {
        for hold in [5, 0] {
            assert_initial_limit(mode, hold, 14400).await;
        }
    }
}

#[tokio::test]
async fn n5c_http_initial_connected_owner_deadline_keeps_typed_429() {
    for mode in ["recover", "rebalance"] {
        // Native hold is 60s; original request is 30s. Neither is restarted.
        assert_initial_limit(mode, 60, 45).await;
    }
}

#[tokio::test]
async fn n5c_http_initial_far_move_or_revoked_full_report_uses_native_b() {
    // D26 amendment 1: ordinary Wait has no connected resume owner, so a far
    // reset moves just like urgent Move. Retain revoked-proof coverage too.
    for (mode, cause, hold) in [
        ("recover", "wait", 5),
        ("rebalance", "wait", 5),
        ("recover", "wait", 0),
        ("rebalance", "wait", 0),
        ("recover", "move", 5),
        ("rebalance", "move", 5),
        ("recover", "revoked", 5),
    ] {
        let session = format!("synthetic-initial-native-{mode}-{cause}");
        let (h, a, b, usage) = initial_fixture(
            &session,
            mode,
            hold,
            if cause == "move" { "move" } else { "wait" },
        )
        .await;
        let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 14400, 0).unwrap();
        full_poll_before_request(&h, &usage, &a, reset).await;
        if cause == "revoked" {
            let store = h.store.clone();
            let source = a.clone();
            // Actual native no-op mutation still invokes the prehook and cuts
            // full-report proof. Cooling alone must not produce the typed 429.
            timeout(
                PHASE,
                tokio::task::spawn_blocking(move || store.update_metadata(&source, |_| {})),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
            assert!(
                matches!(h.store.load(&a).unwrap().health, Health::Cooling { until, .. } if until == reset)
            );
        }
        let mut events = h.state.subscribe_http_quota_wait();
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, FINAL));
        let response = initial_request(&h, &session).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-quota-baton-account"], b.as_str());
        assert_eq!(
            timeout(PHASE, response.bytes())
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            FINAL.as_bytes()
        );
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].path_and_query, "/responses");
        assert_eq!(requests[1].body.as_ref(), UPLOAD.as_bytes());
        let b_account = h.store.load(&b).unwrap();
        let b_tokens = h.store.tokens(&b_account).unwrap();
        assert!(requests[1].headers.get("authorization").is_some_and(
            |v| v.to_str().ok() == Some(format!("Bearer {}", b_tokens.access_token).as_str())
        ));
        assert_eq!(
            h.state
                .bindings
                .current(Some(&format!("codex:{session}")))
                .as_deref(),
            Some(b.as_str())
        );
        assert_eq!(
            h.ledger()
                .iter()
                .filter(|row| row["event"] == "binding_move" && row["from"] == a && row["to"] == b)
                .count(),
            1
        );
        if cause == "wait" {
            assert!(h.ledger().iter().any(|row| row["event"] == "binding_move"
                && row["from"] == a
                && row["to"] == b
                && row["reason"] == "five_hour_far_move"));
        }
        timeout(PHASE, idle(&h, &a)).await.unwrap();
        assert_eq!(h.state.accounts.inflight_count(&b), 0);
        assert_eq!(h.state.bindings.pending_count(&b), 0);
        assert_eq!(usage.state.requests().len(), 1);
        assert_no_more_events(&mut events);
    }
}

#[tokio::test]
async fn n5c_http_ordinary_30s_reset_admits_hold_then_tcp_close_cancels_without_move() {
    use std::net::Shutdown;
    use tokio::{io::AsyncWriteExt, net::TcpStream};

    for mode in ["recover", "rebalance"] {
        let session = format!("synthetic-r13g-near30-{mode}");
        let (h, a, b, usage) = initial_fixture(&session, mode, 60, "wait").await;
        // Whole-second floor makes the reset just inside the original 30s
        // request deadline. No launch token or native owner is connected.
        let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 30, 0).unwrap();
        full_poll_before_request(&h, &usage, &a, reset).await;
        assert!(reset - Utc::now() < ChronoDuration::seconds(30));
        assert!(reset - Utc::now() > ChronoDuration::seconds(20));
        let mut events = h.state.subscribe_http_quota_wait();
        // An erroneous early move/send has a successful response available.
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, FINAL));
        let mut socket = timeout(PHASE, TcpStream::connect(h.servers.data_addr))
            .await
            .unwrap()
            .unwrap();
        let request = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer synthetic-client\r\nContent-Type: application/json\r\nsession-id: {session}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{UPLOAD}",
            h.servers.data_addr,
            UPLOAD.len()
        );
        timeout(PHASE, socket.write_all(request.as_bytes()))
            .await
            .unwrap()
            .unwrap();
        timeout(PHASE, socket.flush()).await.unwrap().unwrap();
        event(
            &mut events,
            "admitted",
            &a,
            PHASE,
            "ordinary 30s reset must admit the native hold despite eligible B",
        )
        .await;
        assert!(reset - Utc::now() > ChronoDuration::seconds(15));
        assert_exact_attempts(&h, 1); // warm A only; no target send or move
        assert_eq!(h.state.inflight_requests.load(Ordering::Acquire), 1);
        assert_eq!(h.state.accounts.inflight_count(&a), 0);
        assert_eq!(h.state.bindings.pending_count(&a), 0);
        assert!(h.state.budget.used() >= UPLOAD.len());
        assert_eq!(
            h.state
                .bindings
                .current(Some(&format!("codex:{session}")))
                .as_deref(),
            Some(a.as_str())
        );

        // Close the real client socket immediately after causal admission;
        // cancellation, rather than a 30s sleep or server teardown, ends it.
        let socket = socket.into_std().unwrap();
        socket.shutdown(Shutdown::Both).unwrap();
        drop(socket);
        timeout(PHASE, idle(&h, &a))
            .await
            .expect("TCP close must release the admitted near-reset hold");
        assert!(Utc::now() < reset);
        assert_eq!(h.state.accounts.inflight_count(&b), 0);
        assert_eq!(h.state.bindings.pending_count(&b), 0);
        assert_exact_attempts(&h, 1);
        assert_eq!(usage.state.requests().len(), 1);
        assert_no_more_events(&mut events);
    }
}
