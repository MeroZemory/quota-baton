//! Future child of routing_wait_http.rs; actual retained-upload HTTP reset path.
use super::*;
use std::sync::mpsc;

const RESET_MARKER: &str = "reset hint must not wait for blocked renewal inspection";

#[tokio::test]
async fn n5a_http_original_reset_reselects_before_blocked_store_renewal_is_released() {
    let session = "synthetic-blocked-renewal-reset";
    let (h, source) = warmed(session).await;
    let mut events = h.state.subscribe_http_quota_wait();
    let (reset, limit) = reset_response();
    h.fake.state.push(limit);
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, FINAL));
    let send = h
        .client
        .post(format!("http://{}/v1/responses", h.servers.data_addr))
        .bearer_auth("synthetic-client")
        .header("content-type", "application/json")
        .header("session-id", session)
        .timeout(Duration::from_secs(30))
        .body(UPLOAD)
        .send();

    let hold_across_reset = async {
        event(
            &mut events,
            "admitted",
            &source,
            PHASE,
            "physical 429 must admit before the native metadata blocker starts",
        )
        .await;
        assert_waiting(&h, &source, session);
        assert!(
            reset - Utc::now() > ChronoDuration::seconds(4),
            "fixture needs pre-reset renewal notification margin"
        );

        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let store = h.store.clone();
        let id = source.clone();
        let worker = std::thread::spawn(move || {
            let result = store.update_metadata(&id, |_account| {
                // Native Store transaction and ordinary pre-mutation hook have
                // ALREADY run here: proof revoked + Notify published. The real
                // renewal API must take this same Store before its other gates.
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(25))
                    .expect("bounded synthetic metadata blocker abandoned");
                // Deliberate native no-op: retain account identity, credential
                // metadata and quota state; the prehook still revokes authority.
            });
            let _ = finished_tx.send(result.map(|_| ()));
        });
        entered_rx
            .recv_timeout(PHASE)
            .expect("actual metadata closure must hold Store before reset");
        assert!(Utc::now() < reset, "Store blocker must precede reset");
        let remaining = (reset - Utc::now()).to_std().unwrap();

        // Physical preselection publishes this observer BEFORE normal chooser
        // Store reads. Waiting for the HTTP response here would incorrectly
        // demand that normal selection bypass the real Store transaction.
        let observed = timeout(remaining + PHASE, events.recv()).await;
        let observed_at = Utc::now();
        let still_held = matches!(finished_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        let attempts_while_held = h.fake.state.requests().len();

        // Release even when the observer times out: the missing-timer negative
        // control must fail at RESET_MARKER, not strand a blocking task or fail
        // first on the outer HTTP timeout. No unconditional thread join.
        release_tx.send(()).unwrap();
        finished_rx
            .recv_timeout(PHASE)
            .expect("native metadata worker must return after release")
            .expect("native no-op metadata result must remain successful");
        drop(worker);

        let observed = observed
            .unwrap_or_else(|_| panic!("{RESET_MARKER}"))
            .expect("HTTP wait observer must remain available");
        assert_eq!(observed.stage, "reselect", "{RESET_MARKER}");
        assert_eq!(observed.source, source);
        assert!(observed_at >= reset, "reset hint must not reselect early");
        assert!(still_held, "reselection must precede Store closure release");
        assert_eq!(attempts_while_held, 2, "normal send must still honor Store");
    };

    let (response, ()) = timeout(Duration::from_secs(30), async {
        tokio::join!(send, hold_across_reset)
    })
    .await
    .expect("bounded blocked-renewal HTTP fixture timed out");
    let response = response.expect("actual same-account retry must complete");
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
    timeout(PHASE, idle(&h, &source))
        .await
        .expect("retry must release native request/upload/binding counters");
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
