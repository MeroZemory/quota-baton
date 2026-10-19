use super::*;

#[tokio::test]
async fn strict_codex_pause_and_restart_never_dispatch_to_other_account() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "strict-root")], RAW)
        .await;
    h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:strict-root"),"settings":{"mode":"strict","reset_wait_secs":0}}))).await;
    h.pause(&first).await;
    let count = h.fake.state.requests().len();
    let (status, _, body) = h
        .send("/v1/responses", &[("session-id", "strict-root")], RAW)
        .await;
    assert_eq!(status, 409);
    assert!(String::from_utf8_lossy(&body).contains("routing_policy_blocked"));
    assert_eq!(
        h.fake.state.requests().len(),
        count,
        "paused strict group must not hit a different upstream credential"
    );
    let h = h
        .restart_with_fixture(|home| {
            fs::write(
                home.join("state/bindings.json"),
                br#"{"schema":1,"bindings":[]}"#,
            )
            .unwrap();
        })
        .await;
    assert_eq!(
        h.send("/v1/responses", &[("session-id", "strict-root")], RAW)
            .await
            .0,
        409
    );
    assert_eq!(
        h.fake.state.requests().len(),
        count,
        "durable strict intent outlives binding eviction"
    );
    let other = h
        .turn("/v1/responses", &[("session-id", "new-root")], RAW)
        .await;
    assert_ne!(other, first);
}

#[tokio::test]
async fn registered_project_applies_before_first_dispatch_and_claude_child_inherits() {
    let h = Harness::new(Provider::Claude).await;
    let result=h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"project","name":"합성 프로젝트","settings":{"mode":"strict","reset_wait_secs":0}}))).await;
    let project = result["id"].as_str().unwrap();
    let first = h
        .turn(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", "project-root"),
                ("x-quota-baton-project", project),
            ],
            "{}",
        )
        .await;
    assert_eq!(
        h.turn(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", "project-root"),
                ("x-claude-code-agent-id", "child")
            ],
            "{}"
        )
        .await,
        first
    );
    for recorded in h.fake.state.requests() {
        assert!(!recorded.headers.contains_key("x-quota-baton-project"));
        assert_eq!(recorded.body.as_ref(), b"{}");
    }
    h.pause(&first).await;
    let before = h.fake.state.requests().len();
    assert_eq!(
        h.send(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", "project-root"),
                ("x-claude-code-agent-id", "child")
            ],
            "{}"
        )
        .await
        .0,
        409
    );
    assert_eq!(h.fake.state.requests().len(), before);
}

#[tokio::test]
async fn recover_mode_moves_only_after_real_account_unavailability() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "recover-root")], RAW)
        .await;
    h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:recover-root"),"settings":{"mode":"recover","reset_wait_secs":0}}))).await;
    h.drain(&first).await;
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "recover-root")], RAW)
            .await,
        first
    );
    h.pause(&first).await;
    assert_ne!(
        h.turn("/v1/responses", &[("session-id", "recover-root")], RAW)
            .await,
        first
    );
}
