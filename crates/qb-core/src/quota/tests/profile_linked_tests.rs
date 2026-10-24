use super::*;

#[tokio::test]
async fn profile_linked_loads_newer_synthetic_token_before_fetch() {
    use crate::accounts::{ImportMode, Source, SourceMatch};
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    let mut account = f.account(0);
    let tokens = p.store.tokens(&account).unwrap();
    let dir = f._root.path().join("synthetic-cpa");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("claude-synthetic.json"), json!({
        "type":"claude", "email":"usage-0@example.invalid",
        "account_uuid":"synthetic-claude-0", "organization_uuid":"synthetic-organization",
        "access_token":"synthetic-newer-linked", "refresh_token":"synthetic-newer-linked-refresh",
        "last_refresh":Utc::now(), "expired":Utc::now()+chrono::Duration::days(2)
    }).to_string()).unwrap();
    account.source = Source::Cpa {
        mode: ImportMode::Linked,
        dir,
        r#match: SourceMatch::from(&account.identity),
    };
    p.store
        .save(&account, &tokens, Some(account.generation))
        .unwrap();
    f.fake.state.push(profile_reply("default_claude_max_5x"));
    p.fetch_profile(&f.ids[0]).await.unwrap();
    let requests = f.fake.state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer synthetic-newer-linked"
    );
    assert_eq!(f.account(0).plan.as_deref(), Some("max_5x"));
    assert!(!p.profile_due(&f.account(0)));
}
