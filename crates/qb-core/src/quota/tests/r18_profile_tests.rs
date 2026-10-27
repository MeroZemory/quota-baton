use super::*;

async fn bounded<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), work)
        .await
        .expect("synthetic profile work did not settle")
}

fn free_reply() -> ScriptedResponse {
    // Synthetic candidate schema, not evidence about the live provider shape.
    ScriptedResponse::json(reqwest::StatusCode::OK, json!({
        "account":{"uuid":"synthetic-claude-0","has_claude_max":false,"has_claude_pro":false},
        "organization":{"uuid":"synthetic-organization","organization_type":"claude_free","rate_limit_tier":"default_claude_free"}
    }).to_string())
}

#[cfg(windows)]
fn claude_usage() -> ScriptedResponse {
    ScriptedResponse::json(
        reqwest::StatusCode::OK,
        r#"{"five_hour":{"utilization":10,"resets_at":"2099-01-01T00:00:00Z"}}"#,
    )
}

#[tokio::test]
async fn r18_ordinary_follower_releases_permit_and_shares_forced_result() {
    for succeeds in [true, false] {
        let f = Fixture::with_provider(1, Provider::Claude).await;
        let p = f.relay.quota.clone();
        let id = f.ids[0].clone();
        let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        let joined = Arc::new(tokio::sync::Notify::new());
        {
            let mut clocks = p.clocks.lock().unwrap();
            let clock = &mut clocks.entry(id.clone()).or_default().profile;
            clock.test_before_permit = Some(gate.clone());
            clock.test_follower_joined = Some(joined.clone());
        }
        f.fake.state.push(if succeeds {
            profile_reply("default_claude_max_5x")
        } else {
            ScriptedResponse::json(reqwest::StatusCode::SERVICE_UNAVAILABLE, "{}")
        });
        let leader_p = p.clone();
        let leader_id = id.clone();
        let leader =
            tokio::spawn(async move { leader_p.refresh_permission_plan(&leader_id).await });
        bounded(gate.0.notified()).await;
        assert_eq!(p.profile_semaphore.available_permits(), 1);
        let follower_p = p.clone();
        let follower = tokio::spawn(async move { follower_p.fetch_profile(&id).await });
        bounded(joined.notified()).await;
        // The ordinary caller has joined the actual leader flight. Without
        // drop(permit), it retains this sole slot and deadlocks the leader.
        assert_eq!(p.profile_semaphore.available_permits(), 1);
        gate.1.notify_one();
        let first = bounded(leader).await.unwrap();
        let second = bounded(follower).await.unwrap();
        if succeeds {
            let first = first.unwrap();
            let second = second.unwrap();
            assert_eq!(first.plan.as_deref(), Some("max_5x"));
            assert_eq!(first.generation, second.generation);
            assert_eq!(first.plan, second.plan);
        } else {
            assert!(matches!(first, Err(PollError::Unavailable)));
            assert!(matches!(second, Err(PollError::Unavailable)));
        }
        assert_eq!(f.fake.state.requests().len(), 1);
        assert!(!p.clocks.lock().unwrap()[&f.ids[0]].profile.running);
        assert_eq!(p.profile_semaphore.available_permits(), 1);
    }
}

#[tokio::test]
async fn r18_cancelled_leader_wakes_follower_and_allows_new_forced_flight() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = f.relay.quota.clone();
    let id = f.ids[0].clone();
    let joined = Arc::new(tokio::sync::Notify::new());
    p.clocks
        .lock()
        .unwrap()
        .entry(id.clone())
        .or_default()
        .profile
        .test_follower_joined = Some(joined.clone());
    let (arrived, release) = f.fake.state.pause_next_response();
    let leader_p = p.clone();
    let leader_id = id.clone();
    let leader = tokio::spawn(async move { leader_p.refresh_permission_plan(&leader_id).await });
    bounded(arrived).await.unwrap();
    let follower_p = p.clone();
    let follower_id = id.clone();
    let follower =
        tokio::spawn(async move { follower_p.refresh_permission_plan(&follower_id).await });
    bounded(joined.notified()).await;
    leader.abort();
    assert!(bounded(leader).await.unwrap_err().is_cancelled());
    assert!(matches!(
        bounded(follower).await.unwrap(),
        Err(PollError::Unavailable)
    ));
    {
        let clocks = p.clocks.lock().unwrap();
        assert!(!clocks[&id].profile.running);
    }
    assert_eq!(p.profile_semaphore.available_permits(), 1);
    // Abandoning the fake response gate fails that old request. It cannot
    // consume the next scripted response or mutate a durable plan.
    drop(release);
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    let saved = bounded(p.refresh_permission_plan(&id)).await.unwrap();
    assert_eq!(saved.plan.as_deref(), Some("max_20x"));
    assert_eq!(f.fake.state.requests().len(), 2);
}

#[tokio::test]
async fn r18_free_recovers_on_normal_profile_cadence_with_tray_closed() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let p = &f.relay.quota;
    let id = &f.ids[0];
    f.fake.state.push(free_reply());
    let free = p.fetch_profile(id).await.unwrap();
    assert_eq!(free.health, Health::PlanWithoutClaudeCode);
    assert!(!free.eligible(None));
    assert!(!p.tray_open());
    assert!(!p.scheduled(&free));
    assert!(!p.profile_due(&free));
    // Move only the monotonic scheduling clock; no daily deadline is erased.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(p.min_interval)).await;
    tokio::time::resume();
    assert!(p.profile_due(&free));
    assert!(p.clocks.lock().unwrap()[id].profile.next.unwrap() > Instant::now());
    f.fake.state.push(profile_reply("default_claude_max_20x"));
    bounded(p.tick()).await;
    let paid = f.account(0);
    assert_eq!(paid.plan.as_deref(), Some("max_20x"));
    assert_eq!(paid.health, Health::Ready);
    assert!(paid.eligible(None));
    let requests = f.fake.state.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|r| r.path_and_query == "/api/oauth/profile")
    );
}

#[cfg(windows)]
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
