//! Child of entry::n5c_policy_wake; synthetic actual HTTP, no direct proof mint.
use super::*;

const MARKER: &str = "consumed Move must reject the first native receipt before admitted sleep";

#[tokio::test]
async fn n5c_http_move_consumed_before_first_snapshot_rejects_delayed_native_result() {
    let session = "synthetic-n5c-initial-result-publication-race";
    let (h, source, alternate, usage, reset) = strict_fixture(session).await;
    let key = format!("codex:{session}");
    let initial = h.state.bindings.policies.plan(Some(&key));
    let mut gate = h.state.hold_next_http_quota_wait_result(&key, &source);
    let mut events = h.state.subscribe_http_quota_wait();
    let mut phases = h.state.subscribe_http_quota_wait_inspections();
    let start = TokioInstant::now();
    let headers = [("session-id", session)];
    let send = h.send("/v1/responses", &headers, UPLOAD);
    let race = async {
        event(
            &mut phases,
            "inspection_started",
            &source,
            PHASE,
            "target HTTP must start its genuine first inspection",
        )
        .await;
        let bounds = timeout(PHASE, gate.entered())
            .await
            .expect("native receipt must reach the post-inspection gate")
            .expect("only a real successful native receipt may enter this gate");
        assert_eq!(bounds.source, source);
        assert_eq!(bounds.reset_at, reset);
        assert!(bounds.wake > TokioInstant::now());
        assert!(bounds.wake < bounds.deadline);
        assert!(bounds.deadline > start);
        assert!(bounds.deadline <= TokioInstant::now() + Duration::from_secs(30));
        assert_no_more_events(&mut events); // receipt exists, helper not admitted
        assert_eq!(h.fake.state.requests().len(), 1);

        // Native Store/runtime/binding/publication guards have all been dropped
        // by the actual inspection API BEFORE this gate can report entered.
        urgency(&h, session, "move").await;
        event(
            &mut phases,
            "policy_before_snapshot",
            &source,
            PHASE,
            "Move must be consumed and rearmed while the first result is held",
        )
        .await;
        let current = h.state.bindings.policies.plan(Some(&key));
        assert!(current.revision > initial.revision);
        assert_eq!(current.settings.on_5h_limit, On5hLimit::Move);
        assert_eq!(current.settings.mode, Mode::Strict);
        assert_eq!(current.settings.reset_wait_secs, 60);
        assert_eq!(current.anchor, initial.anchor);
        assert_no_more_events(&mut events);
        assert_no_more_events(&mut phases); // no replacement native Store job
        assert!(TokioInstant::now() < bounds.wake);
        assert!(TokioInstant::now() < bounds.deadline);

        // No second publication or proof/account notification is supplied.
        gate.release();
        event(&mut phases, "result_policy_revoked", &source, PHASE, MARKER).await;
        assert_no_more_events(&mut events); // reject BEFORE admitted/sleep
        assert!(TokioInstant::now() < bounds.wake);
        assert!(TokioInstant::now() < bounds.deadline);
        assert!(Utc::now() < reset);
        assert_eq!(h.fake.state.requests().len(), 1);
    };
    let ((status, _account, body), ()) =
        timeout(Duration::from_secs(12), async { tokio::join!(send, race) })
            .await
            .expect("bounded actual HTTP first-result policy race");
    assert_eq!(
        status, 429,
        "Revoked cannot override native Strict or force B"
    );
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["type"], "usage_limit_reached");
    assert!(
        Utc::now() < reset,
        "old reset must not become another retained wait"
    );
    timeout(PHASE, idle(&h, &source)).await.unwrap();
    assert_exact_attempts(&h, 1); // warm A only; neither target A nor eligible B
    assert_eq!(usage.state.requests().len(), 1);
    assert_eq!(h.state.accounts.inflight_count(&alternate), 0);
    assert_eq!(h.state.bindings.pending_count(&alternate), 0);
    assert_eq!(
        h.state.bindings.current(Some(&key)).as_deref(),
        Some(source.as_str())
    );
    assert_no_more_events(&mut events);
    assert_no_more_events(&mut phases);
}
