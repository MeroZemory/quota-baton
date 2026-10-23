//! Staged child of m4b.rs: real retained-upload proof wait, not legacy C7.
use super::*;
use qb_core::relay::HttpQuotaWaitEvent;
use std::{net::Shutdown, sync::atomic::Ordering};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::broadcast,
    time::{Instant as TokioInstant, timeout},
};

#[path = "routing_wait_claude.rs"]
mod routing_wait_claude;

const PHASE: Duration = Duration::from_secs(3);
const UPLOAD: &str = r#"{ "model": "synthetic-model-http-wait", "input": "conversation-marker", "encrypted_content": "verbatim" }"#;
const FINAL: &str = r#"{"n5a_http_wait":"retry"}"#;

async fn idle(h: &Harness, source: &str) {
    // Pure counters: no pacing_snapshot/decay/insertion probe in this loop.
    while h.state.inflight_requests.load(Ordering::Acquire) != 0
        || h.state.accounts.inflight_count(source) != 0
        || h.state.bindings.pending_count(source) != 0
        || h.state.budget.used() != 0
    {
        tokio::task::yield_now().await;
    }
}

async fn warmed(session: &str) -> (Harness, String) {
    let h = Harness::with_limits_and_config(Provider::Codex, 60, 30, 60, |config| {
        config.routing.move_guard.trip_seconds = 5;
        config.guard.retain_max_per_request_mib = 1;
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
        .expect("warm request must release its native counters");
    let key = format!("codex:{session}");
    assert_eq!(
        h.state
            .bindings
            .policies
            .plan(Some(&key))
            .settings
            .reset_wait_secs,
        60
    );
    assert_eq!(h.fake.state.requests().len(), 1);
    (h, source)
}

fn reset_response() -> (chrono::DateTime<Utc>, ScriptedResponse) {
    // Choose the exact whole-second reset only after fixture warmup. This is
    // real wall time, not a paused-Tokio substitute for chrono::Utc.
    let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 10, 0).unwrap();
    let mut response = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, "");
    for (name, value) in [
        ("x-codex-primary-window-minutes", "300".to_owned()),
        ("x-codex-primary-used-percent", "100".to_owned()),
        ("x-codex-primary-reset-at", reset.timestamp().to_string()),
        ("x-codex-secondary-window-minutes", "10080".to_owned()),
        ("x-codex-secondary-used-percent", "10".to_owned()),
        (
            "x-codex-secondary-reset-at",
            (reset + ChronoDuration::hours(47)).timestamp().to_string(),
        ),
        ("x-codex-active-limit", "5h".to_owned()),
    ] {
        response.headers.insert(name, value.parse().unwrap());
    }
    // Complete empty body + full native quota headers; no direct proof mint,
    // synthetic cooling write, or x-codex-limit-reached classifier override.
    (reset, response)
}

async fn event(
    events: &mut broadcast::Receiver<HttpQuotaWaitEvent>,
    stage: &str,
    source: &str,
    bound: Duration,
    marker: &str,
) {
    let observed = timeout(bound, events.recv())
        .await
        .unwrap_or_else(|_| panic!("{marker}"))
        .unwrap_or_else(|error| panic!("HTTP proof-wait observer lost an event: {error}"));
    assert_eq!(observed.stage, stage, "{marker}");
    assert_eq!(observed.source, source, "wrong proof-wait source");
}

fn assert_waiting(h: &Harness, source: &str, session: &str) {
    assert_eq!(h.fake.state.requests().len(), 2, "retry sent before reset");
    assert_eq!(h.state.inflight_requests.load(Ordering::Acquire), 1);
    assert_eq!(h.state.accounts.inflight_count(source), 0);
    assert_eq!(h.state.bindings.pending_count(source), 0);
    assert!(
        h.state.budget.used() >= UPLOAD.len(),
        "admitted wait must retain the original upload reservation"
    );
    assert_eq!(
        h.state
            .bindings
            .current(Some(&format!("codex:{session}")))
            .as_deref(),
        Some(source)
    );
}

fn assert_exact_attempts(h: &Harness, expected: usize) {
    let attempts = h.fake.state.requests();
    assert_eq!(attempts.len(), expected);
    for attempt in &attempts {
        assert_eq!(attempt.path_and_query, "/responses");
        assert_eq!(attempt.body.as_ref(), UPLOAD.as_bytes());
        assert!(
            attempt.headers.get("authorization").is_some()
                && attempt.headers.get("authorization") == attempts[0].headers.get("authorization"),
            "retry changed the synthetic source credential"
        );
    }
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "binding_move" && !row["from"].is_null()),
        "a same-account reset wait must not create a movement"
    );
}

fn assert_no_more_events(events: &mut broadcast::Receiver<HttpQuotaWaitEvent>) {
    assert!(
        matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ),
        "unexpected duplicate admission/reselection or lost observer"
    );
}

#[tokio::test]
async fn n5a_http_proof_wait_reset_reselects_same_account_once() {
    let session = "synthetic-proof-reset";
    let (h, source) = warmed(session).await;
    // Subscription precedes the target request; broadcast keeps an early event.
    let mut events = h.state.subscribe_http_quota_wait();
    let (reset, limit) = reset_response();
    h.fake.state.push(limit);
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, FINAL));
    let (first_seen, first_release) = h.fake.state.pause_next_response();
    let send = h
        .client
        .post(format!("http://{}/v1/responses", h.servers.data_addr))
        .bearer_auth("synthetic-client")
        .header("content-type", "application/json")
        .header("session-id", session)
        // Override the existing Harness client's eight-second timeout.
        .timeout(Duration::from_secs(30))
        .body(UPLOAD)
        .send();
    let observe = async {
        timeout(PHASE, first_seen)
            .await
            .expect("target upload must reach the actual upstream")
            .expect("initial upstream gate abandoned");
        assert_eq!(h.fake.state.requests().len(), 2);
        first_release.send(()).unwrap();
        event(
            &mut events,
            "admitted",
            &source,
            PHASE,
            "proof-backed HTTP wait must be admitted before reset",
        )
        .await;
        assert!(
            reset - Utc::now() > ChronoDuration::seconds(4),
            "fixture setup consumed the pre-reset observation margin"
        );
        assert_waiting(&h, &source, session);
        let (retry_seen, retry_release) = h.fake.state.pause_next_response();
        let remaining = (reset - Utc::now()).to_std().unwrap();
        event(
            &mut events,
            "reselect",
            &source,
            remaining + PHASE,
            "reset retry must pass normal selection",
        )
        .await;
        assert!(Utc::now() >= reset, "normal reselect preceded actual reset");
        timeout(PHASE, retry_seen)
            .await
            .expect("normal reselect must dispatch the retained retry")
            .expect("retry upstream gate abandoned");
        assert_exact_attempts(&h, 3);
        retry_release.send(()).unwrap();
    };
    // No spawned request task or task-abort surrogate for caller lifetime.
    let (response, ()) = timeout(Duration::from_secs(35), async {
        tokio::join!(send, observe)
    })
    .await
    .expect("bounded reset HTTP fixture timed out");
    let response = response.expect("actual HTTP retry request failed");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-quota-baton-account"], source.as_str());
    assert_eq!(response.bytes().await.unwrap().as_ref(), FINAL.as_bytes());
    timeout(PHASE, idle(&h, &source))
        .await
        .expect("reset retry must release all native request/body reservations");
    assert_exact_attempts(&h, 3);
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
async fn n5a_http_tcp_close_cancels_admitted_wait_before_reset() {
    let session = "synthetic-proof-close";
    let (h, source) = warmed(session).await;
    let mut events = h.state.subscribe_http_quota_wait();
    let (reset, limit) = reset_response();
    h.fake.state.push(limit);
    // A buggy late retry can succeed, so missing-error fallback cannot hide it.
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, FINAL));
    let (first_seen, first_release) = h.fake.state.pause_next_response();
    let horizon =
        TokioInstant::now() + (reset - Utc::now()).to_std().unwrap() + Duration::from_millis(250);
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
    timeout(PHASE, first_seen)
        .await
        .expect("complete raw upload must reach the actual upstream")
        .expect("initial raw upstream gate abandoned");
    first_release.send(()).unwrap();
    event(
        &mut events,
        "admitted",
        &source,
        PHASE,
        "proof-backed HTTP wait must be admitted before reset",
    )
    .await;
    assert_waiting(&h, &source, session);
    assert!(
        reset - Utc::now() > ChronoDuration::seconds(4),
        "TCP cancellation bound must end strictly before the original reset"
    );
    let socket = socket.into_std().unwrap();
    socket.shutdown(Shutdown::Both).unwrap();
    drop(socket);
    let released_before_reset = timeout(Duration::from_secs(2), idle(&h, &source))
        .await
        .is_ok();
    let budget_after_close = h.state.budget.used();
    // This timer observes late traffic through the original reset. It is not
    // used to guess whether the real wait was admitted. Keep all servers,
    // accounts and policies alive, with no edits or task cancellation.
    tokio::time::sleep_until(horizon).await;
    assert_exact_attempts(&h, 2);
    assert!(
        released_before_reset,
        "actual TCP close must release the admitted proof wait before reset"
    );
    assert_eq!(
        budget_after_close, 0,
        "TCP close leaked retained upload bytes"
    );
    assert_eq!(h.state.inflight_requests.load(Ordering::Acquire), 0);
    assert_eq!(h.state.accounts.inflight_count(&source), 0);
    assert_eq!(h.state.bindings.pending_count(&source), 0);
    assert_eq!(h.state.budget.used(), 0);
    assert_no_more_events(&mut events);
    // A real independent request proves that server teardown was not the cause.
    let control = h
        .turn("/v1/responses", &[("session-id", session)], UPLOAD)
        .await;
    assert_eq!(control, source);
    timeout(PHASE, idle(&h, &source))
        .await
        .expect("independent HTTP control must release native reservations");
    assert_exact_attempts(&h, 3);
    assert_no_more_events(&mut events);
}

#[path = "routing_wait_drain.rs"]
mod routing_wait_drain;

#[path = "routing_wait_policy.rs"]
mod routing_wait_policy;

#[path = "routing_wait_http/poll_renewal.rs"]
mod poll_renewal;

#[path = "routing_wait_blocked_inspection.rs"]
mod routing_wait_blocked_inspection;

#[path = "routing_wait_http/entry.rs"]
mod entry;
