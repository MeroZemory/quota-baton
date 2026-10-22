//! Future child of tests/support/routing_wait_http.rs; requires feature=testkit.
use super::*;

#[tokio::test]
async fn n5a_http_admitted_wait_returns_original_429_while_drain_refresh_is_held() {
    let session = "synthetic-proof-drain";
    let (h, source) = warmed(session).await;
    let alternate = h.ids().into_iter().find(|id| id != &source).unwrap();

    // A separate, disabled refresh owner prevents credential serialization on
    // A or B from concealing an erroneous post-drain dispatch to B.
    let cpa = h.home.join("synthetic-drain-refresh-import");
    fs::create_dir_all(&cpa).unwrap();
    fs::write(
        cpa.join("refresh.json"),
        serde_json::to_vec(&json!({
            "type":"codex", "email":"synthetic-drain-refresh@example.invalid",
            "account_id":"synthetic-drain-refresh-account",
            "access_token":"synthetic-drain-refresh-access",
            "refresh_token":"synthetic-drain-refresh-token",
            "id_token":"synthetic-drain-refresh-id",
            "expired":"2099-01-01T00:00:00Z",
            "last_refresh":"2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let imported = import::run(&h.store, &cpa, &[], false).unwrap();
    let refresh_id = imported[0].id.clone().unwrap();
    let generation = h.store.load(&refresh_id).unwrap().generation;
    qb_core::accounts::adopt::run_with_probe(
        &h.store,
        &refresh_id,
        serde_json::from_value(json!({"from":"store", "expected_generation":generation})).unwrap(),
        || qb_core::accounts::adopt::ProbeState::Stopped,
    )
    .unwrap();
    h.pause(&refresh_id).await;
    h.admin(
        reqwest::Method::POST,
        &format!("/admin/v1/accounts/{alternate}/resume"),
        None,
    )
    .await;
    h.admin(
        reqwest::Method::POST,
        "/admin/v1/routing",
        Some(json!({
            "kind":"session", "id":qb_core::routing::session_id(&format!("codex:{session}")),
            "settings":{"mode":"recover", "reset_wait_secs":60}
        })),
    )
    .await;
    assert_eq!(h.fake.state.requests().len(), 1);
    let mut events = h.state.subscribe_http_quota_wait();
    let (reset, limit) = reset_response();
    h.fake.state.push(limit);
    let (completed, completion) = tokio::sync::oneshot::channel();

    let request = async {
        let result = timeout(
            Duration::from_secs(9),
            h.send("/v1/responses", &[("session-id", session)], UPLOAD),
        )
        .await;
        let _ = completed.send(result);
    };
    let drain_during_wait = async {
        event(
            &mut events,
            "admitted",
            &source,
            PHASE,
            "actual physical 429 must admit its retained-upload wait",
        )
        .await;
        assert_waiting(&h, &source, session);
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::OK,
            r#"{"access_token":"synthetic-drain-rotated-access","refresh_token":"synthetic-drain-rotated-refresh","expires_in":3600}"#,
        ));
        let (refresh_arrived, refresh_release) = h.fake.state.pause_next_response();
        let refresh_state = h.state.clone();
        let refreshing =
            tokio::spawn(async move { refresh_state.refresh_account(refresh_id, true).await });
        timeout(PHASE, refresh_arrived)
            .await
            .expect("real synthetic OAuth refresh must reach numeric loopback")
            .expect("refresh response gate abandoned");
        let before = h.fake.state.requests();
        assert_eq!(before.len(), 3, "warm + original 429 + separate refresh");
        assert_eq!(before[2].path_and_query, "/");
        assert_eq!(before[2].method, reqwest::Method::POST);

        let drain_state = h.state.clone();
        let draining = tokio::spawn(async move { drain_state.begin_drain().await });
        timeout(PHASE, h.state.wait_for_drain_start())
            .await
            .expect("actual drain must publish its start");

        // Await the actual HTTP result while the refresh gate remains held.
        // Capture evidence before releasing that gate, then join both owners
        // before asserting the result so an assertion cannot strand the gate.
        let result = timeout(PHASE, completion).await;
        let drain_pending = !draining.is_finished();
        let store_open = !h.store.writes_stopped();
        let before_reset = Utc::now() < reset;
        let requests_while_held = h.fake.state.requests().len();
        let released = timeout(PHASE, idle(&h, &source)).await;
        refresh_release.send(()).unwrap();
        timeout(PHASE, draining)
            .await
            .expect("released refresh must let drain finish")
            .unwrap()
            .unwrap();
        // This test observes refresh lifecycle completion, not refresh outcome
        // policy when shutdown races a successfully received rotation.
        let _refresh_outcome = timeout(PHASE, refreshing)
            .await
            .expect("refresh owner must finish")
            .unwrap();

        let (status, account, body) = result
            .expect("waiter must reply before refresh release/store freeze")
            .expect("HTTP result sender disappeared")
            .expect("actual HTTP request exceeded its bound");
        assert_eq!(status, 429, "relay must preserve the physical native 429");
        assert_eq!(account, source);
        assert!(body.is_empty(), "complete upstream empty body is verbatim");
        assert!(drain_pending, "response must not rely on completed drain");
        assert!(store_open, "response must not rely on stopped store writes");
        assert!(
            before_reset,
            "test must observe drain cancellation before reset"
        );
        assert!(
            released.is_ok(),
            "wait must release all request/upload counters"
        );
        assert_eq!(requests_while_held, 3, "drain must not dispatch A or B");
        assert_eq!(
            h.fake.state.requests().len(),
            3,
            "no dispatch after drain finishes"
        );
        assert!(h.store.writes_stopped());
        assert_no_more_events(&mut events);
    };
    timeout(Duration::from_secs(20), async {
        tokio::join!(request, drain_during_wait);
    })
    .await
    .expect("bounded actual HTTP drain regression timed out");
}
