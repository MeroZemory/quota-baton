//! Authenticated launch intent and first real Claude project ingress.
use super::*;

#[tokio::test]
async fn n5c_claude_registration_precedes_first_project_and_preserves_strict_root() {
    let h = Harness::new(Provider::Claude).await;
    let native = "12345678-1234-4234-8234-123456789ABC";
    let key = format!("claude:{native}");
    let id = qb_core::routing::session_id(&key);
    let url = format!("http://{}/admin/v1/routing", h.servers.admin_addr);
    let intent = json!({"kind":"claude_urgent_launch","session_id":native});
    let before = h.state.bindings.policies.list();
    let denied = h.client.post(&url).json(&intent).send().await.unwrap();
    assert!(matches!(denied.status().as_u16(), 401 | 403));
    assert_eq!(h.state.bindings.policies.list(), before);
    for invalid in ["sample-label", "12345678123442348234123456789abc"] {
        let refused = h
            .client
            .post(&url)
            .bearer_auth("synthetic-admin")
            .json(&json!({"kind":"claude_urgent_launch","session_id":invalid}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status().as_u16(), 400);
        assert_eq!(h.state.bindings.policies.list(), before);
    }
    h.admin(reqwest::Method::POST, "/admin/v1/routing", Some(intent))
        .await;
    let intent_plan = h.state.bindings.routing_plan(Some(&key));
    assert_eq!(
        intent_plan.settings.on_5h_limit,
        qb_core::routing::On5hLimit::Move
    );
    assert!(intent_plan.anchor.is_none());
    assert!(h.state.bindings.current(Some(&key)).is_none());
    assert!(h.fake.state.requests().is_empty());
    let sessions = h
        .admin(reqwest::Method::GET, "/admin/v1/sessions?range=all", None)
        .await;
    assert!(
        sessions["sessions"].as_array().unwrap().is_empty(),
        "launch intent is not an observed session"
    );

    let result = h
        .admin(
            reqwest::Method::POST,
            "/admin/v1/routing",
            Some(json!({
                "kind":"project","name":"synthetic urgent project",
                "settings":{"mode":"strict","reset_wait_secs":0,"on_5h_limit":"wait"}
            })),
        )
        .await;
    let project = result["id"].as_str().unwrap();
    let account = h
        .turn(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", native),
                ("x-quota-baton-project", project),
            ],
            "{}",
        )
        .await;
    let plan = h.state.bindings.routing_plan(Some(&key));
    assert_eq!(plan.project.as_deref(), Some(project));
    assert_eq!(plan.anchor.as_deref(), Some(account.as_str()));
    assert_eq!(plan.settings.mode, qb_core::routing::Mode::Strict);
    assert_eq!(plan.settings.reset_wait_secs, 0);
    assert_eq!(plan.settings.on_5h_limit, qb_core::routing::On5hLimit::Move);
    assert_eq!(
        h.turn(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", native),
                ("x-claude-code-agent-id", "synthetic-child")
            ],
            "{}"
        )
        .await,
        account
    );
    for mode in ["wait", "move"] {
        h.admin(
            reqwest::Method::POST,
            "/admin/v1/routing",
            Some(json!({
                "kind":"urgent","id":id,"on_5h_limit":mode
            })),
        )
        .await;
        let current = h.state.bindings.routing_plan(Some(&key));
        assert_eq!(current.settings.mode, plan.settings.mode);
        assert_eq!(
            current.settings.reset_wait_secs,
            plan.settings.reset_wait_secs
        );
        assert_eq!(current.project, plan.project);
        assert_eq!(current.anchor, plan.anchor);
        assert_eq!(
            serde_json::to_value(current.settings.on_5h_limit).unwrap(),
            mode
        );
    }
    assert_eq!(h.fake.state.requests().len(), 2);
    for request in h.fake.state.requests() {
        assert_eq!(request.body.as_ref(), b"{}");
        assert!(!request.headers.contains_key("x-quota-baton-project"));
    }
}
