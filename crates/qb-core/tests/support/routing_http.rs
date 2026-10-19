use super::*;

fn assert_credential(h: &Harness, index: usize, id: &str) {
    let account = h.store.load(id).unwrap();
    let token = h.store.tokens(&account).unwrap();
    let request = &h.fake.state.requests()[index];
    assert_eq!(
        request.headers["authorization"].to_str().unwrap(),
        format!("Bearer {}", token.access_token.as_str())
    );
    if let qb_core::accounts::Identity::Codex { account_id, .. } = account.identity {
        assert_eq!(request.headers["chatgpt-account-id"], account_id);
    }
}

#[tokio::test]
async fn queued_request_cannot_dispatch_old_account_after_move_and_strict_edit() {
    let h = Harness::new(Provider::Codex).await;
    let headers = [("session-id", "dispatch-race")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    assert_credential(&h, 0, &first);
    h.state
        .accounts
        .observe_429(&first, Some(Duration::from_secs(2)));
    let waiting = h.send("/v1/responses", &headers, RAW);
    let edit = async {
        tokio::time::timeout(DEADLINE, async {
            while h.state.accounts.pacing_snapshot(&first).waiting == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        h.pause(&first).await;
        let next = h.turn("/v1/responses", &headers, RAW).await;
        assert_ne!(first, next);
        assert_credential(&h, 1, &next);
        h.admin(reqwest::Method::POST, "/admin/v1/routing", Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:dispatch-race"),"settings":{"mode":"strict","reset_wait_secs":0}}))).await;
        h.admin(
            reqwest::Method::POST,
            &format!("/admin/v1/accounts/{first}/resume"),
            None,
        )
        .await;
        next
    };
    let (result, next) = tokio::join!(waiting, edit);
    assert_eq!(result.0, 409);
    assert_eq!(
        h.fake.state.requests().len(),
        2,
        "old admitted request must never send A after strict B"
    );
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, next);
    assert_credential(&h, 2, &next);
}

#[tokio::test]
async fn strict_codex_pause_and_restart_never_dispatch_to_other_account() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "strict-root")], RAW)
        .await;
    assert_credential(&h, 0, &first);
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
