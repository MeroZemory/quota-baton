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
            "organization":{"uuid":"synthetic-organization","organization_type":"claude_free","rate_limit_tier":"synthetic_unknown_tier"}
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

async fn two_account_permission_failover(late_successor: bool) {
    let f = Fixture::with_provider(2, Provider::Claude).await;
    let store = f.relay.quota.store.clone();
    let access = ["synthetic-r18-access-a", "synthetic-r18-access-b"];
    for (n, token) in access.iter().enumerate() {
        let mut account = f.account(n);
        account.apply_health_signal(
            crate::accounts::HealthSignal::PlanObservation {
                plan: Some("max_20x".into()),
            },
            Utc::now(),
        );
        if n == 1 {
            account.user_state = UserState::Disabled;
        }
        store
            .save(
                &account,
                &Tokens {
                    access_token: (*token).into(),
                    refresh_token: format!("synthetic-r18-refresh-{n}"),
                    id_token: None,
                },
                Some(account.generation),
            )
            .unwrap();
    }
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    config.upstream.anthropic_base = f.fake.url.clone();
    config.upstream.claude_usage_base = f.fake.url.clone();
    config.upstream.codex_usage_base = f.fake.url.clone();
    config.quota.poll_min_interval_secs = 3600;
    let relay = Arc::new(
        RelayState::new(
            store.clone(),
            &config,
            f.relay.quota.obs.clone(),
            Arc::new(DebugPins::default()),
        )
        .unwrap(),
    );
    // Prime the real usage reservation, not private clocks. A later exhausted
    // selection must not add an unrelated usage/profile attempt to this case.
    f.fake.state.push(ScriptedResponse::json(
        reqwest::StatusCode::OK,
        r#"{"five_hour":{"utilization":10,"resets_at":"2099-01-01T00:00:00Z"}}"#,
    ));
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    relay.quota.poll(&f.ids[0]).await.unwrap();
    let mut servers = crate::http::spawn_with_routes(
        &config,
        crate::local_secrets::LocalSecrets {
            client_key: "synthetic-client-key".into(),
            admin_token: "synthetic-admin-key".into(),
        },
        crate::relay::router(relay.clone()),
        axum::Router::new(),
    )
    .await
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let url = format!("http://{}/v1/messages", servers.data_addr);
    let session = "synthetic-r18-bound-plan-change";
    let key = format!("claude:{session}");
    let body = json!({"model":"synthetic-model","max_tokens":1,
        "messages":[{"role":"user","content":"synthetic"}]});
    let send = || {
        client
            .post(&url)
            .bearer_auth("synthetic-client-key")
            .header("x-claude-code-session-id", session)
            .json(&body)
            .send()
    };
    f.fake.state.push(ScriptedResponse::json(
        reqwest::StatusCode::OK,
        r#"{"served":"a"}"#,
    ));
    let warm = send().await.unwrap();
    assert_eq!(warm.status(), reqwest::StatusCode::OK);
    assert_eq!(warm.headers()["x-quota-baton-account"], f.ids[0].as_str());
    assert_eq!(warm.text().await.unwrap(), r#"{"served":"a"}"#);
    assert_eq!(relay.bindings.current(Some(&key)), Some(f.ids[0].clone()));
    if !late_successor {
        relay
            .accounts
            .change(&f.ids[1], Some(UserState::Active), None, None, None)
            .await
            .unwrap();
    }
    let denial =
        r#"{"error":{"type":"permission_error","message":"not allowed for this organization"}}"#;
    f.fake.state.push(ScriptedResponse::json(
        reqwest::StatusCode::FORBIDDEN,
        denial,
    ));
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::OK, json!({
        "account":{"uuid":"synthetic-claude-0","has_claude_pro":false,"has_claude_max":false},
        "organization":{"uuid":"synthetic-organization","organization_type":"claude_free","rate_limit_tier":"default_claude_free"}
    }).to_string()));
    if !late_successor {
        f.fake.state.push(ScriptedResponse::json(
            reqwest::StatusCode::OK,
            r#"{"served":"b"}"#,
        ));
    }
    let response = send().await.unwrap();
    if late_successor {
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        assert_eq!(response.text().await.unwrap(), denial);
        assert_eq!(relay.bindings.current(Some(&key)), Some(f.ids[0].clone()));
        // The denied HTTP call is finished. Its request-scoped failure evidence
        // cannot authorize the next request; stored Free health must do so.
        relay
            .accounts
            .change(&f.ids[1], Some(UserState::Active), None, None, None)
            .await
            .unwrap();
    } else {
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.headers()["x-quota-baton-account"],
            f.ids[1].as_str()
        );
        assert_eq!(response.text().await.unwrap(), r#"{"served":"b"}"#);
        assert_eq!(relay.bindings.current(Some(&key)), Some(f.ids[1].clone()));
    }
    f.fake.state.push(ScriptedResponse::json(
        reqwest::StatusCode::OK,
        r#"{"served":"b-next"}"#,
    ));
    let next = send().await.unwrap();
    assert_eq!(next.status(), reqwest::StatusCode::OK);
    assert_eq!(next.headers()["x-quota-baton-account"], f.ids[1].as_str());
    assert_eq!(next.text().await.unwrap(), r#"{"served":"b-next"}"#);
    assert_eq!(relay.bindings.current(Some(&key)), Some(f.ids[1].clone()));
    // Inspect persisted bytes before a store load could normalize the health.
    let persisted: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            store
                .home()
                .join("accounts")
                .join(format!("{}.json", f.ids[0])),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(persisted["plan"], "free");
    assert_eq!(
        persisted["health"],
        json!({"kind":"plan_without_claude_code"})
    );
    let blocked = f.account(0);
    assert!(!blocked.eligible_at(
        Some("synthetic-model"),
        Utc::now() + chrono::Duration::days(7)
    ));
    assert_eq!(f.account(1).plan.as_deref(), Some("max_20x"));
    assert_eq!(f.account(1).health, Health::Ready);

    let requests = f.fake.state.requests();
    let mut expected = vec![
        ("/api/oauth/usage", access[0]),
        ("/api/oauth/profile", access[0]),
        ("/v1/messages", access[0]),
        ("/v1/messages", access[0]),
        ("/api/oauth/profile", access[0]),
    ];
    if !late_successor {
        expected.push(("/v1/messages", access[1]));
    }
    expected.push(("/v1/messages", access[1]));
    assert_eq!(requests.len(), expected.len());
    for (request, (path, token)) in requests.iter().zip(expected) {
        assert_eq!(request.path_and_query, path);
        assert_eq!(
            request.headers["authorization"].to_str().unwrap(),
            format!("Bearer {token}")
        );
        if path == "/v1/messages" {
            assert_eq!(request.headers["x-claude-code-session-id"], session);
            assert_eq!(request.body, requests[2].body);
        }
    }
    servers.shutdown();
    (&mut servers.data_task).await.unwrap().unwrap();
    (&mut servers.admin_task).await.unwrap().unwrap();
}

#[tokio::test]
async fn r18_actual_http_bound_free_fails_over_to_second_paid_account() {
    tokio::time::timeout(
        Duration::from_secs(30),
        two_account_permission_failover(false),
    )
    .await
    .expect("synthetic two-account failover did not settle");
}

#[tokio::test]
async fn r18_actual_http_bound_free_rebinds_on_fresh_request_when_successor_recovers() {
    tokio::time::timeout(
        Duration::from_secs(30),
        two_account_permission_failover(true),
    )
    .await
    .expect("synthetic health-evidence rebind did not settle");
}
