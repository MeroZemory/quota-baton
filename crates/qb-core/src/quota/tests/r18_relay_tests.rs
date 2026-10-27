use super::*;

async fn permission_request(f: &Fixture, free: bool) {
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    config.upstream.anthropic_base = f.fake.url.clone();
    config.upstream.claude_usage_base = f.fake.url.clone();
    config.upstream.codex_usage_base = f.fake.url.clone();
    let relay = Arc::new(
        RelayState::new(
            f.relay.quota.store.clone(),
            &config,
            f.relay.quota.obs.clone(),
            Arc::new(DebugPins::default()),
        )
        .unwrap(),
    );
    // A previous successful observation must not suppress the denial diagnosis.
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    relay.quota.fetch_profile(&f.ids[0]).await.unwrap();
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::FORBIDDEN,
        r#"{"error":{"type":"permission_error","message":"not allowed for this organization synthetic-secret@example.test"}}"#));
    f.fake.state.push(if free {
        ScriptedResponse::json(reqwest::StatusCode::OK, json!({
            "account":{"uuid":"synthetic-claude-0","has_claude_pro":false,"has_claude_max":false},
            "organization":{"uuid":"synthetic-organization","organization_type":"claude_free","rate_limit_tier":"default_claude_free"}
        }).to_string())
    } else { profile_reply("default_claude_max_20x") });
    let mut servers = crate::http::spawn_with_routes(
        &config,
        crate::local_secrets::LocalSecrets {
            client_key: "synthetic-client-key".into(),
            admin_token: "synthetic-admin-key".into(),
        },
        crate::relay::router(relay),
        axum::Router::new(),
    )
    .await
    .unwrap();
    let result = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(15)).build().unwrap()
        .post(format!("http://{}/v1/messages", servers.data_addr))
        .bearer_auth("synthetic-client-key")
        .json(&json!({"model":"synthetic-model","max_tokens":1,"messages":[{"role":"user","content":"synthetic"}]}))
        .send().await;
    servers.shutdown();
    let _ = result.unwrap().bytes().await.unwrap();
    (&mut servers.data_task).await.unwrap().unwrap();
    (&mut servers.admin_task).await.unwrap().unwrap();
}

#[tokio::test]
async fn r18_actual_http_permission_rereads_profile_and_blocks_free_without_expiry() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    permission_request(&f, true).await;
    let saved = f.account(0);
    assert_eq!(saved.plan.as_deref(), Some("free"));
    assert_eq!(saved.health, Health::PlanWithoutClaudeCode);
    assert!(!saved.eligible_at(None, Utc::now() + chrono::Duration::days(7)));
    let paths: Vec<_> = f
        .fake
        .state
        .requests()
        .iter()
        .map(|r| r.path_and_query.clone())
        .collect();
    let denial = paths.iter().position(|p| p == "/v1/messages").unwrap();
    assert_eq!(paths[denial + 1], "/api/oauth/profile");
}

#[tokio::test]
async fn r18_actual_http_paid_profile_retains_permission_cooling_and_fixed_diagnostic() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    permission_request(&f, false).await;
    assert!(matches!(
        f.account(0).health,
        Health::Cooling {
            reason: HealthReason::Permission,
            ..
        }
    ));
    f.relay.quota.obs.flush().unwrap();
    let mut found = false;
    for entry in std::fs::read_dir(f._root.path().join("ledger")).unwrap() {
        for line in std::fs::read_to_string(entry.unwrap().path())
            .unwrap()
            .lines()
        {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            if row["event"] == "permission_denied" {
                assert_eq!(row["error_type"], "permission_error");
                assert_eq!(row["message"], "Upstream denied organization access");
                assert!(!line.contains("synthetic-secret"));
                found = true;
            }
        }
    }
    assert!(found);
}

#[tokio::test]
async fn r18_codex_usage_poll_updates_durable_plan_without_blocking_free() {
    let f = Fixture::new(1).await;
    f.relay
        .quota
        .store
        .update_metadata(&f.ids[0], |a| a.plan = Some("plus".into()))
        .unwrap();
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::OK,
        r#"{"plan_type":"free","rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":3600}}}"#));
    f.relay.quota.poll(&f.ids[0]).await.unwrap();
    let saved = f.account(0);
    assert_eq!(saved.plan.as_deref(), Some("free"));
    assert!(saved.eligible(None));
}
