use super::*;

#[tokio::test]
async fn profile_admin_import_and_adopt_queue_only_after_successful_writes() {
    use crate::accounts::{ImportMode, Source, SourceMatch, adopt::ProbeState};
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    let mut account = f.account(0);
    let tokens = p.store.tokens(&account).unwrap();
    let dir = f._root.path().join("admin-synthetic-cpa");
    std::fs::create_dir_all(&dir).unwrap();
    account.source = Source::Cpa { mode:ImportMode::Linked, dir:dir.clone(), r#match:SourceMatch::from(&account.identity) };
    p.store.save(&account, &tokens, Some(account.generation)).unwrap();
    f.fake.state.push(profile_reply("default_claude_max_5x"));
    p.fetch_profile(&f.ids[0]).await.unwrap();
    assert!(!p.profile_due(&f.account(0)));
    std::fs::write(dir.join("synthetic.json"), json!({
        "type":"claude", "email":"usage-0@example.invalid",
        "account_uuid":"synthetic-claude-0", "organization_uuid":"synthetic-organization",
        "access_token":"synthetic-admin-imported", "refresh_token":"synthetic-admin-refresh",
        "last_refresh":Utc::now(), "expired":Utc::now()+chrono::Duration::days(2)
    }).to_string()).unwrap();
    let routes = crate::admin::account_routes_with_poller(p.store.clone(), p.clone())
        .merge(crate::admin::runtime_routes_with_adoption_probe(f.relay.clone(), 0, || ProbeState::Stopped));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap(); });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let request = json!({"source":"cpa","mode":"linked","dir":dir,"dry_run":true});
    assert!(client.post(format!("{url}/admin/v1/accounts/import")).json(&request).send().await.unwrap().status().is_success());
    assert!(!p.profile_due(&f.account(0)));
    let mut request = request;
    request["dry_run"] = false.into();
    assert!(client.post(format!("{url}/admin/v1/accounts/import")).json(&request).send().await.unwrap().status().is_success());
    assert!(p.profile_due(&f.account(0)));
    // Hooks queue only: there has been no second profile network call yet.
    assert_eq!(f.fake.state.requests().len(), 1);
    f.fake.state.push(profile_reply("default_claude_max_5x"));
    p.fetch_profile(&f.ids[0]).await.unwrap();
    let current = f.account(0);
    let adopt_url = format!("{url}/admin/v1/accounts/{}/adopt", current.id);
    let failed = client.post(&adopt_url).json(&json!({"from":"store","expected_generation":0})).send().await.unwrap();
    assert_eq!(failed.status(), reqwest::StatusCode::CONFLICT);
    assert!(!p.profile_due(&f.account(0)));
    let response = client.post(adopt_url).json(&json!({"from":"store","expected_generation":current.generation})).send().await.unwrap();
    assert!(response.status().is_success());
    assert!(p.profile_due(&f.account(0)));
    assert_eq!(f.fake.state.requests().len(), 2);
    server.abort();
    let _ = server.await;
}
