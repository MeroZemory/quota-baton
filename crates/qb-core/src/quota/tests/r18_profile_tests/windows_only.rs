use super::*;

fn claude_usage() -> ScriptedResponse {
    ScriptedResponse::json(
        reqwest::StatusCode::OK,
        r#"{"five_hour":{"utilization":10,"resets_at":"2099-01-01T00:00:00Z"}}"#,
    )
}

#[tokio::test]
async fn r18_linked_failed_and_successful_usage_each_refresh_plan_without_cpa_write() {
    use crate::accounts::{ImportMode, Source, SourceMatch};
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    let id = &f.ids[0];
    let mut account = f.account(0);
    let tokens = p.store.tokens(&account).unwrap();
    let dir = f._root.path().join("r18-synthetic-cpa");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("claude-synthetic.json");
    let content = json!({
        "type":"claude","email":"usage-0@example.invalid",
        "account_uuid":"synthetic-claude-0","organization_uuid":"synthetic-organization",
        "access_token":"synthetic-r18-linked","refresh_token":"synthetic-r18-refresh",
        "last_refresh":Utc::now(),"expired":Utc::now()+chrono::Duration::days(2)
    })
    .to_string();
    std::fs::write(&path, &content).unwrap();
    account.source = Source::Cpa {
        mode: ImportMode::Linked,
        dir,
        r#match: SourceMatch::from(&account.identity),
    };
    p.store
        .save(&account, &tokens, Some(account.generation))
        .unwrap();
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    p.fetch_profile(id).await.unwrap();
    assert!(!p.profile_due(&f.account(0)));
    f.fake.state.push(ScriptedResponse::json(
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "{}",
    ));
    f.fake.state.push(free_reply());
    assert!(matches!(
        bounded(p.poll(id)).await,
        Err(PollError::Unavailable)
    ));
    let free = f.account(0);
    assert_eq!(free.plan.as_deref(), Some("free"));
    assert_eq!(free.health, Health::PlanWithoutClaudeCode);
    {
        let mut clocks = p.clocks.lock().unwrap();
        let clock = clocks.get_mut(id).unwrap();
        assert_eq!(clock.failures, 1);
        assert!(clock.failure_until.is_some());
        // Release only usage backoff to model the next actual attempt. Keep
        // the profile DAY/recovery clocks intact to prove forced freshness.
        clock.next = Some(Instant::now());
        clock.failure_until = None;
    }
    f.fake.state.push(claude_usage());
    f.fake.state.push(profile_reply("default_claude_max_5x"));
    let paid = bounded(p.poll(id)).await.unwrap();
    assert_eq!(paid.plan.as_deref(), Some("max_5x"));
    assert_eq!(paid.health, Health::Ready);
    assert!(paid.eligible(None));
    let requests = f.fake.state.requests();
    let paths: Vec<_> = requests.iter().map(|r| r.path_and_query.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/api/oauth/profile",
            "/api/oauth/usage",
            "/api/oauth/profile",
            "/api/oauth/usage",
            "/api/oauth/profile"
        ]
    );
    assert!(
        requests
            .iter()
            .all(|r| r.headers["authorization"] == "Bearer synthetic-r18-linked")
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), content);
}
