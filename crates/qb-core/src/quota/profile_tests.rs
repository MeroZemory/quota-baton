// Included in quota::tests to reuse its isolated project-local synthetic store.
fn profile_reply(tier: &str) -> ScriptedResponse {
    ScriptedResponse::json(reqwest::StatusCode::OK, json!({
        "account":{"uuid":"synthetic-claude-0"},
        "organization":{"uuid":"synthetic-organization","organization_type":"claude_max","rate_limit_tier":tier}
    }).to_string())
}

fn expire_profile(poller: &UsagePoller, id: &str) {
    poller.clocks.lock().unwrap().get_mut(id).unwrap().profile.next = Some(Instant::now());
}

#[tokio::test]
async fn profile_daily_unknown_and_error_attempts_do_not_change_usage() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let poller = &f.relay.quota;
    let id = &f.ids[0];
    let usage = json!({"windows":[],"source":"poll","observed_at":Utc::now()});
    poller.obs.observe_poll(id, usage, true).unwrap();
    let before = poller.obs.quota_snapshot(id);
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    let saved = poller.fetch_profile(id).await.unwrap();
    assert_eq!(saved.plan.as_deref(), Some("max_20x"));
    assert!(saved.generation > 0);
    assert!(!poller.profile_due(&saved));
    assert!(matches!(poller.fetch_profile(id).await, Err(PollError::TooSoon { .. })));
    let requests = f.fake.state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path_and_query, "/api/oauth/profile");
    assert_eq!(requests[0].headers["authorization"], "Bearer synthetic-usage-access");
    assert_eq!(requests[0].headers["anthropic-beta"], "oauth-2025-04-20");
    assert_eq!(requests[0].headers["user-agent"], concat!("quota-baton/", env!("CARGO_PKG_VERSION")));
    assert_eq!(poller.obs.quota_snapshot(id), before);
    // Crossing exactly one day enables the next ordinary attempt.
    expire_profile(poller, id);
    assert!(poller.profile_due(&saved));
    f.fake.state.push(profile_reply("future_unknown_tier"));
    assert_eq!(poller.fetch_profile(id).await.unwrap().plan, None);
    assert!(!poller.profile_due(&f.account(0)));
    expire_profile(poller, id);
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::SERVICE_UNAVAILABLE, "{}"));
    assert!(matches!(poller.fetch_profile(id).await, Err(PollError::Unavailable)));
    assert!(!poller.profile_due(&f.account(0)));
    assert_eq!(f.fake.state.requests().len(), 3);
    assert_eq!(poller.obs.quota_snapshot(id), before);
    // The ordinary attempt deadline stays a full day, independent of usage backoff.
    let clocks = poller.clocks.lock().unwrap();
    let remaining = clocks[id].profile.next.unwrap().duration_since(Instant::now());
    assert!(remaining > Duration::from_secs(86_390));
}

#[tokio::test]
async fn profile_lifecycle_forces_are_explicit_and_metadata_saves_do_not_loop() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    let id = &f.ids[0];
    f.fake.state.push(profile_reply("default_claude_max_5x"));
    p.fetch_profile(id).await.unwrap();
    let current = f.account(0);
    assert!(!p.profile_due(&current));
    // A normal owned refresh rotates generation/token without forcing profile.
    let mut rotated = current.clone();
    rotated.token_meta.last_refresh_at = Some(Utc::now());
    let tokens = Tokens { access_token:"synthetic-rotated".into(), refresh_token:"synthetic-rotated-refresh".into(), id_token:None };
    let rotated = p.store.save(&rotated, &tokens, Some(current.generation)).unwrap();
    assert!(!p.profile_due(&rotated));
    p.store.update_metadata(id, |a| a.label = "synthetic renamed".into()).unwrap();
    assert!(!p.profile_due(&f.account(0)));
    // The import/adopt hook coalesces repeated lifecycle events.
    p.queue_profile(&f.account(0));
    p.queue_profile(&f.account(0));
    assert!(p.profile_due(&f.account(0)));
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    p.fetch_profile(id).await.unwrap();
    assert!(!p.profile_due(&f.account(0)));
    assert_eq!(f.fake.state.requests().len(), 2);
    assert_eq!(f.fake.state.requests()[1].headers["authorization"], "Bearer synthetic-rotated");
    // Startup queues work but performs no profile request before listeners bind.
    p.startup_reload().await.unwrap();
    assert!(p.profile_due(&f.account(0)));
    assert_eq!(f.fake.state.requests().len(), 2);
}

#[tokio::test]
async fn profile_stale_response_preserves_admin_changes_and_midflight_force() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let id = f.ids[0].clone();
    let p = f.relay.quota.clone();
    let mut reply = profile_reply("default_claude_max_20x");
    reply.chunks[0].0 = Duration::from_millis(200);
    f.fake.state.push(reply);
    let task_p = p.clone();
    let task_id = id.clone();
    let task = tokio::spawn(async move { task_p.fetch_profile(&task_id).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.fake.state.requests().is_empty() { tokio::task::yield_now().await; }
    }).await.unwrap();
    p.store.update_metadata(&id, |a| { a.plan = Some("pro".into()); a.label = "synthetic admin change".into(); }).unwrap();
    p.queue_profile(&f.account(0));
    task.await.unwrap().unwrap();
    let current = f.account(0);
    assert_eq!(current.plan.as_deref(), Some("pro"));
    assert_eq!(current.label, "synthetic admin change");
    assert!(p.profile_due(&current));
    f.fake.state.push(profile_reply("default_claude_max_5x"));
    p.fetch_profile(&id).await.unwrap();
    assert!(!p.profile_due(&f.account(0)));
}

#[tokio::test]
async fn profile_tick_fetches_paused_claude_without_usage_and_excludes_needs_relogin_codex() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    p.store.update_metadata(&f.ids[0], |a| a.user_state = UserState::Paused).unwrap();
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    p.tick().await;
    assert_eq!(f.fake.state.requests().len(), 1);
    assert_eq!(f.account(0).plan.as_deref(), Some("max_20x"));
    p.tick().await;
    assert_eq!(f.fake.state.requests().len(), 1);
    let mut account = f.account(0);
    account.health = Health::NeedsRelogin { reason:"synthetic".into(), since:Utc::now() };
    p.queue_profile(&account);
    assert!(!p.profile_due(&account));
    let codex = Fixture::new(1).await;
    codex.relay.quota.queue_profile(&codex.account(0));
    assert!(!codex.relay.quota.profile_due(&codex.account(0)));
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn profile_daily_401_uses_existing_owned_refresh_retry_and_current_token() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    let gates = TokenGates::new(reqwest::Client::builder().no_proxy().build().unwrap(), f.relay.accounts.clone())
        .with_token_endpoint(Provider::Claude, format!("{}/synthetic-token", f.fake.url));
    p.set_tokens(gates);
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::UNAUTHORIZED, "{}"));
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::OK,
        r#"{"access_token":"synthetic-after-401","refresh_token":"synthetic-after-401-refresh","expires_in":86400}"#));
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    p.fetch_profile(&f.ids[0]).await.unwrap();
    let requests = f.fake.state.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].path_and_query, "/api/oauth/profile");
    assert_eq!(requests[1].path_and_query, "/synthetic-token");
    assert_eq!(requests[2].headers["authorization"], "Bearer synthetic-after-401");
    assert_eq!(f.account(0).plan.as_deref(), Some("max_20x"));
    assert!(!p.profile_due(&f.account(0)));
}

#[tokio::test]
async fn profile_linked_401_uses_link_stale_path_without_owned_refresh() {
    use crate::accounts::{ImportMode, Source, SourceMatch};
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    let mut account = f.account(0);
    let tokens = p.store.tokens(&account).unwrap();
    // Explicit absent synthetic directory, never the real CPA directory.
    account.source = Source::Cpa { mode:ImportMode::Linked,
        dir:f._root.path().join("absent-synthetic-cpa"), r#match:SourceMatch::from(&account.identity) };
    p.store.save(&account, &tokens, Some(account.generation)).unwrap();
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::UNAUTHORIZED, "{}"));
    assert!(matches!(p.fetch_profile(&f.ids[0]).await, Err(PollError::Unavailable)));
    assert_eq!(f.fake.state.requests().len(), 1);
    assert!(matches!(f.account(0).health, Health::Cooling { reason:HealthReason::LinkStale, .. }));
    assert!(!p.profile_due(&f.account(0)));
}

#[tokio::test]
async fn profile_account_events_include_plan_and_generation_without_body() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let mut events = f.relay.accounts.subscribe();
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    let saved = f.relay.quota.fetch_profile(&f.ids[0]).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().unwrap();
    assert_eq!(event["event"], "account_changed");
    assert_eq!(event["data"]["plan"], "max_20x");
    f.relay.quota.obs.flush().unwrap();
    let ledger: String = std::fs::read_dir(f._root.path().join("ledger")).unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap()).collect();
    let account_event = ledger.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "account_changed" && event["plan"] == "max_20x").unwrap();
    assert_eq!(account_event["generation"], saved.generation);
    assert!(!ledger.contains("synthetic-usage-access"));
    assert!(!ledger.contains("rate_limit_tier"));
}

#[test]
fn profile_test_endpoint_guard_rejects_production_names_and_cpa_management() {
    use crate::oauth::claude_profile::synthetic_endpoint;
    assert!(synthetic_endpoint("http://127.0.0.1:23456/api/oauth/profile"));
    assert!(synthetic_endpoint("http://[::1]:23456/api/oauth/profile"));
    for endpoint in ["https://api.anthropic.com/api/oauth/profile", "http://localhost:23456/",
        "http://127.0.0.1:10100/", "http://127.0.0.1:10101/", "http://192.0.2.1:23456/",
        "http://user@127.0.0.1:23456/", "http://127.0.0.1:23456/?key=synthetic"] {
        assert!(!synthetic_endpoint(endpoint));
    }
}


#[tokio::test]
async fn profile_cancellation_consumes_daily_attempt_but_keeps_explicit_force() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = f.relay.quota.clone();
    let id = f.ids[0].clone();
    let mut reply = profile_reply("default_claude_max_20x");
    reply.chunks[0].0 = Duration::from_secs(10);
    f.fake.state.push(reply);
    let task_p = p.clone();
    let task_id = id.clone();
    let task = tokio::spawn(async move { task_p.fetch_profile(&task_id).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.fake.state.requests().is_empty() { tokio::task::yield_now().await; }
    }).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!p.profile_due(&f.account(0)));
    assert_eq!(p.profile_semaphore.available_permits(), 1);
    assert_eq!(p.semaphore.available_permits(), 2);
    p.queue_profile(&f.account(0));
    assert!(p.profile_due(&f.account(0)));
}

#[tokio::test]
async fn profile_identity_mismatch_is_an_error_and_counts_as_daily_attempt() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    p.store.update_metadata(&f.ids[0], |a| a.plan = Some("pro".into())).unwrap();
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::OK,
        r#"{"account":{"uuid":"other-synthetic-account"},"organization":{"uuid":"synthetic-organization","rate_limit_tier":"default_claude_max_20x"}}"#));
    assert!(matches!(p.fetch_profile(&f.ids[0]).await, Err(PollError::Unavailable)));
    assert_eq!(f.account(0).plan.as_deref(), Some("pro"));
    assert!(!p.profile_due(&f.account(0)));
}


#[cfg(all(windows, feature = "testkit"))]
#[path = "profile_import_adopt_windows.rs"]
mod profile_import_adopt_windows;


#[tokio::test]
async fn profile_busy_slot_leaves_other_account_due_without_occupying_usage_slots() {
    let f = Fixture::with_provider(2, Provider::Claude).await;
    let p = f.relay.quota.clone();
    let mut reply = profile_reply("default_claude_max_5x");
    reply.chunks[0].0 = Duration::from_millis(200);
    f.fake.state.push(reply);
    let task_p = p.clone();
    let id = f.ids[0].clone();
    let task = tokio::spawn(async move { task_p.fetch_profile(&id).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.fake.state.requests().is_empty() { tokio::task::yield_now().await; }
    }).await.unwrap();
    assert!(matches!(p.fetch_profile(&f.ids[1]).await, Err(PollError::Unavailable)));
    assert!(p.profile_due(&f.account(1)));
    assert_eq!(p.semaphore.available_permits(), 2);
    task.await.unwrap().unwrap();
    assert!(!p.profile_due(&f.account(0)));
    assert!(p.profile_due(&f.account(1)));
}


#[cfg(debug_assertions)]
#[tokio::test]
async fn profile_second_401_exhausts_c9_retry_without_a_second_refresh() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    p.set_tokens(TokenGates::new(reqwest::Client::builder().no_proxy().build().unwrap(), f.relay.accounts.clone())
        .with_token_endpoint(Provider::Claude, format!("{}/synthetic-token", f.fake.url)));
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::UNAUTHORIZED, "{}"));
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::OK,
        r#"{"access_token":"synthetic-after-401","refresh_token":"synthetic-after-401-refresh","expires_in":86400}"#));
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::UNAUTHORIZED, "{}"));
    assert!(matches!(p.fetch_profile(&f.ids[0]).await, Err(PollError::Unavailable)));
    assert_eq!(f.fake.state.requests().len(), 3);
    assert_eq!(f.account(0).auth_failures, 1);
    assert!(!p.profile_due(&f.account(0)));
}

#[tokio::test]
async fn profile_is_never_fetched_by_request_unavailable_hook() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::OK,
        r#"{"five_hour":{"utilization":10,"resets_at":"2099-01-01T00:00:00Z"}}"#));
    f.relay.quota.before_unavailable(Provider::Claude).await;
    let requests = f.fake.state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path_and_query, "/api/oauth/usage");
    assert!(f.relay.quota.profile_due(&f.account(0)));
}


#[tokio::test]
async fn profile_tick_fetches_disabled_claude_without_usage_polling_or_enabling() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    let id = &f.ids[0];
    p.store.update_metadata(id, |a| a.user_state = UserState::Disabled).unwrap();
    assert!(!p.scheduled(&f.account(0)));
    assert!(p.profile_due(&f.account(0)));
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    p.tick().await;
    let requests = f.fake.state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, reqwest::Method::GET);
    assert_eq!(requests[0].path_and_query, "/api/oauth/profile");
    let account = f.account(0);
    assert_eq!(account.user_state, UserState::Disabled);
    assert_eq!(account.plan.as_deref(), Some("max_20x"));
    assert!(!p.profile_due(&account));
    assert!(p.obs.quota_snapshot(id).is_none());
    assert!(matches!(p.poll(id).await, Err(PollError::Excluded)));
    p.tick().await;
    assert_eq!(f.fake.state.requests().len(), 1);
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn profile_disabled_adopted_claude_401_uses_d17_owned_refresh_only() {
    use crate::accounts::{ImportMode, Source, SourceMatch};
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    let mut account = f.account(0);
    let tokens = p.store.tokens(&account).unwrap();
    account.user_state = UserState::Disabled;
    account.source = Source::Cpa { mode:ImportMode::Adopted,
        dir:f._root.path().join("absent-adopted-synthetic-cpa"),
        r#match:SourceMatch::from(&account.identity) };
    p.store.save(&account, &tokens, Some(account.generation)).unwrap();
    p.set_tokens(TokenGates::new(reqwest::Client::builder().no_proxy().build().unwrap(), f.relay.accounts.clone())
        .with_token_endpoint(Provider::Claude, format!("{}/synthetic-token", f.fake.url)));
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::UNAUTHORIZED, "{}"));
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::OK,
        r#"{"access_token":"synthetic-disabled-after-401","refresh_token":"synthetic-disabled-refresh","expires_in":86400}"#));
    f.fake.state.push(profile_reply("default_claude_max_5x"));
    p.tick().await;
    let requests = f.fake.state.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].path_and_query, "/api/oauth/profile");
    assert_eq!(requests[1].method, reqwest::Method::POST);
    assert_eq!(requests[1].path_and_query, "/synthetic-token");
    assert_eq!(requests[2].path_and_query, "/api/oauth/profile");
    assert_eq!(requests[2].headers["authorization"], "Bearer synthetic-disabled-after-401");
    assert!(requests.iter().all(|request| request.path_and_query != "/api/oauth/usage"));
    let current = f.account(0);
    assert_eq!(current.user_state, UserState::Disabled);
    assert_eq!(current.health, Health::Ready);
    assert_eq!(current.plan.as_deref(), Some("max_5x"));
    assert_eq!(p.store.tokens(&current).unwrap().access_token, "synthetic-disabled-after-401");
    assert!(matches!(current.source, Source::Cpa { mode:ImportMode::Adopted, .. }));
    assert!(!p.profile_due(&current));
    assert!(p.obs.quota_snapshot(&f.ids[0]).is_none());
    assert!(matches!(p.poll(&f.ids[0]).await, Err(PollError::Excluded)));
    p.tick().await;
    assert_eq!(f.fake.state.requests().len(), 3);
}

#[tokio::test]
async fn profile_account_ledger_preserves_canonical_codex_plans_including_free_plus() {
    let f = Fixture::new(1).await;
    let p = &f.relay.quota;
    let plans = ["free", "plus", "pro", "team", "business", "enterprise", "edu", "go"];
    for plan in plans {
        p.store.update_metadata(&f.ids[0], |a| a.plan = Some(plan.into())).unwrap();
    }
    p.obs.flush().unwrap();
    let ledger: String = std::fs::read_dir(f._root.path().join("ledger")).unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap()).collect();
    let observed: std::collections::BTreeSet<String> = ledger.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["event"] == "account_changed" && event["provider"] == "codex")
        .filter_map(|event| event["plan"].as_str().map(str::to_owned))
        .collect();
    let expected: std::collections::BTreeSet<String> = plans.into_iter().map(str::to_owned).collect();
    assert_eq!(observed, expected);
    assert!(f.fake.state.requests().is_empty());
}
