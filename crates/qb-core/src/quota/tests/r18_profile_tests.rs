use super::*;

#[tokio::test]
async fn r18_subscription_creation_uses_existing_fenced_profile_and_preserves_absence() {
    let f = Fixture::with_provider(1, Provider::Claude).await;
    let id = &f.ids[0];
    let created_at = chrono::DateTime::parse_from_rfc3339("2026-04-07T02:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    f.fake.state.push(ScriptedResponse::json(reqwest::StatusCode::OK, json!({
        "account":{"uuid":"synthetic-claude-0","has_claude_max":true},
        "organization":{"uuid":"synthetic-organization","rate_limit_tier":"default_claude_max_5x",
            "subscription_created_at":"2026-04-07T02:00:00Z"}
    }).to_string()));
    let account = f.relay.quota.refresh_permission_plan(id).await.unwrap();
    assert_eq!(
        account.subscription,
        Some(crate::accounts::SubscriptionMetadata::Claude { created_at })
    );
    assert_eq!(f.account(0).subscription, account.subscription);
    f.fake.state.push(profile_reply("default_claude_max_5x"));
    let next = f.relay.quota.refresh_permission_plan(id).await.unwrap();
    assert_eq!(next.subscription, account.subscription);
    assert_eq!(f.fake.state.requests().len(), 2);
}

#[tokio::test]
async fn r18_subscription_end_poll_due_is_per_exact_endpoint_and_respects_disabled() {
    let f = Fixture::new(1).await;
    let mut account = f.account(0);
    let until = Utc::now() - chrono::Duration::minutes(1);
    // A fresh usage snapshot and closed tray must not postpone the end check.
    f.relay
        .quota
        .obs
        .observe_poll(
            &account.id,
            json!({"observed_at":Utc::now(),"windows":[],"source":"poll"}),
            true,
        )
        .unwrap();
    account.subscription = Some(crate::accounts::SubscriptionMetadata::Codex {
        active_start: None,
        active_until: Some(until),
        last_checked: None,
        end_plan_checked_for: None,
    });
    assert!(f.relay.quota.scheduled(&account));
    if let Some(crate::accounts::SubscriptionMetadata::Codex {
        end_plan_checked_for,
        ..
    }) = &mut account.subscription
    {
        *end_plan_checked_for = Some(until);
    }
    assert!(!f.relay.quota.scheduled(&account));
    if let Some(crate::accounts::SubscriptionMetadata::Codex { active_until, .. }) =
        &mut account.subscription
    {
        *active_until = Some(until - chrono::Duration::days(1));
    }
    assert!(f.relay.quota.scheduled(&account));
    account.user_state = UserState::Disabled;
    assert!(!f.relay.quota.scheduled(&account));
}

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
mod windows_only;
