//! Proposed child of support/routing_wait_http.rs; synthetic loopback only.
use super::*;

#[tokio::test]
async fn n5a_http_effective_policy_zero_revokes_admitted_wait_before_reset() {
    let session = "synthetic-proof-policy-zero";
    let key = format!("codex:{session}");
    let (h, source) = warmed(session).await;
    // Fix mode before the target request; the live edit changes ONLY budget.
    h.admin(
        reqwest::Method::POST,
        "/admin/v1/routing",
        Some(json!({
            "kind":"session", "id":qb_core::routing::session_id(&key),
            "settings":{"mode":"strict", "reset_wait_secs":60}
        })),
    )
    .await;
    let initial = h.state.bindings.policies.plan(Some(&key));
    assert_eq!(initial.settings.reset_wait_secs, 60);
    let mut events = h.state.subscribe_http_quota_wait();
    let (reset, limit) = reset_response();
    h.fake.state.push(limit);
    // A wrong late retry could succeed instead of hiding behind missing script.
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, FINAL));
    let horizon =
        TokioInstant::now() + (reset - Utc::now()).to_std().unwrap() + Duration::from_millis(250);
    let (completed, completion) = tokio::sync::oneshot::channel();
    let request = async {
        let response = h
            .send("/v1/responses", &[("session-id", session)], UPLOAD)
            .await;
        let _ = completed.send(response);
    };
    let edit_during_wait = async {
        event(
            &mut events,
            "admitted",
            &source,
            PHASE,
            "policy test must enter actual proof-backed HTTP wait",
        )
        .await;
        assert_waiting(&h, &source, session);
        assert!(
            reset - Utc::now() > ChronoDuration::seconds(4),
            "policy edit/cleanup bound must precede original reset"
        );
        timeout(
            PHASE,
            h.admin(
                reqwest::Method::POST,
                "/admin/v1/routing",
                Some(json!({
                    "kind":"session", "id":qb_core::routing::session_id(&key),
                    "settings":{"mode":"strict", "reset_wait_secs":0}
                })),
            ),
        )
        .await
        .expect("actual effective policy publication must complete");
        let current = h.state.bindings.policies.plan(Some(&key));
        assert_eq!(current.settings.reset_wait_secs, 0);
        assert_eq!(current.settings.mode, initial.settings.mode);
        assert!(current.revision > initial.revision);

        let (status, _account, body) = timeout(PHASE, completion)
            .await
            .expect("effective zero-wait policy must stop admitted wait promptly")
            .expect("physical request result sender disappeared");
        // R13.G: live Strict forbids a new account and returns the genuine
        // bound-account limit. The policy edit must still interrupt promptly.
        assert_eq!(status, 429);
        assert!(
            body.is_empty(),
            "preserve the original empty upstream 429 body"
        );
        assert!(Utc::now() < reset, "reply must not wait for original reset");
        timeout(PHASE, idle(&h, &source))
            .await
            .expect("policy refusal must free original upload/request reservations");
        assert!(
            Utc::now() < reset,
            "body/counter cleanup must precede reset"
        );
        assert_exact_attempts(&h, 2);
        assert_no_more_events(&mut events);
        assert_eq!(
            h.state.bindings.current(Some(&key)).as_deref(),
            Some(source.as_str())
        );
    };
    // Both futures are driven directly; no request task abort substitutes for
    // native policy interruption. Completion includes consuming response body.
    timeout(Duration::from_secs(12), async {
        tokio::join!(request, edit_during_wait);
    })
    .await
    .expect("bounded actual HTTP policy interruption must complete");

    // Observe beyond ORIGINAL reset: neither reset retry nor a second legacy
    // hold may leave a detached upstream attempt after the prompt response.
    tokio::time::sleep_until(horizon).await;
    assert_exact_attempts(&h, 2);
    assert_no_more_events(&mut events);
    assert_eq!(h.state.inflight_requests.load(Ordering::Acquire), 0);
    assert_eq!(h.state.accounts.inflight_count(&source), 0);
    assert_eq!(h.state.bindings.pending_count(&source), 0);
    assert_eq!(h.state.budget.used(), 0);
}
