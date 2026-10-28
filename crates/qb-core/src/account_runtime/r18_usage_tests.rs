use super::*;
use crate::{
    accounts::{SubscriptionMetadata, Tokens},
    config::Config,
    debug_pin::DebugPins,
    relay::RelayState,
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};

struct Fixture {
    relay: Arc<RelayState>,
    fake: ScriptedUpstream,
    id: String,
    _root: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let base = std::env::var_os("QB_TEST_USAGE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../.sandbox/r18-usage-tests")
            });
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let mut config = tempfile::NamedTempFile::new_in(root.path()).unwrap();
        std::io::Write::write_all(&mut config, b"[secrets]\nlinux_backend = \"file-0600\"\n")
            .unwrap();
        config.persist(root.path().join("config.toml")).unwrap();
        let store = Arc::new(AccountStore::acquire(root.path()).unwrap());
        let mut account = crate::accounts::health::tests::fixture();
        account.plan = Some("plus".into());
        account.token_meta.expires_at = Some(Utc::now() + Duration::days(2));
        let account = store
            .save(
                &account,
                &Tokens {
                    access_token: "synthetic-r18-usage-old".into(),
                    refresh_token: "synthetic-r18-refresh".into(),
                    id_token: None,
                },
                None,
            )
            .unwrap();
        let fake = ScriptedUpstream::start().await.unwrap();
        let mut config = Config::default();
        config.quota.poll_min_interval_secs = 1;
        config.upstream.codex_usage_base = fake.url.clone();
        let obs = Arc::new(Observability::new(root.path(), None).unwrap());
        let relay =
            Arc::new(RelayState::new(store, &config, obs, Arc::new(DebugPins::default())).unwrap());
        Self {
            _root: root,
            relay,
            fake,
            id: account.id,
        }
    }

    fn account(&self) -> Account {
        self.relay.accounts.store.load(&self.id).unwrap()
    }

    async fn apply(&self, body: Value) -> (Account, Value) {
        let account = self.account();
        let started_at = Utc::now();
        let raw = serde_json::to_vec(&body).unwrap();
        let snapshot =
            crate::quota::parse_snapshot_for_test(Provider::Codex, &raw, started_at).unwrap();
        let (account, _, snapshot) = self
            .relay
            .accounts
            .apply_polled_usage_report(
                PollUsageReport {
                    access_token: zeroize::Zeroizing::new(
                        self.relay
                            .accounts
                            .store
                            .tokens(&account)
                            .unwrap()
                            .access_token
                            .clone(),
                    ),
                    account,
                    started_at,
                    snapshot,
                    complete: crate::quota::is_complete_snapshot_for_test(Provider::Codex, &raw),
                },
                None,
                None,
                false,
            )
            .await
            .unwrap();
        (
            account,
            snapshot.expect("focused helper requires a quota-applying report"),
        )
    }
}

fn usage(plan: &str, exhausted: bool, five_hour: bool) -> Value {
    let window = |seconds, used| json!({"used_percent":used,"limit_window_seconds":seconds,"reset_after_seconds":7200});
    json!({"plan_type":plan,"rate_limit":{
        "limit_reached":exhausted,
        "primary_window":if five_hour { window(18000, 100) } else { window(604800, 0) },
        "secondary_window":if five_hour { window(604800, 0) } else { Value::Null }
    }})
}

fn has_five_hour(snapshot: &Value) -> bool {
    snapshot["windows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|window| window["name"] == "5h")
}

#[tokio::test]
async fn fake_plus_to_pro_removes_obsolete_window_and_only_its_quota_hold() {
    let f = Fixture::new().await;
    let mut events = f.relay.accounts.subscribe();
    f.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        usage("plus", true, true).to_string(),
    ));
    let plus = f.relay.quota.poll(&f.id).await.unwrap();
    assert_eq!(plus.plan.as_deref(), Some("plus"));
    assert!(matches!(
        plus.health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    assert_eq!(plus.quota_cooling.as_ref().unwrap().window, "5h");
    tokio::time::sleep(StdDuration::from_millis(1100)).await;
    f.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        usage("pro", false, false).to_string(),
    ));
    let pro = f.relay.quota.poll(&f.id).await.unwrap();
    assert_eq!(pro.plan.as_deref(), Some("pro"));
    assert!(pro.plan_observed_at > plus.plan_observed_at);
    assert_eq!(pro.health, Health::Ready);
    assert!(pro.quota_cooling.is_none());
    let snapshot = f.relay.accounts.obs.quota_snapshot(&f.id).unwrap();
    assert!(!has_five_hour(&snapshot));
    assert_eq!(snapshot["windows"].as_array().unwrap().len(), 1);
    for request in f.fake.state.requests() {
        assert_eq!(request.method, axum::http::Method::GET);
        assert_eq!(request.path_and_query, "/backend-api/wham/usage");
        assert_eq!(
            request.headers["authorization"],
            "Bearer synthetic-r18-usage-old"
        );
    }
    assert_eq!(f.fake.state.requests().len(), 2);
    let mut cleared = 0;
    while let Ok(event) = events.try_recv() {
        if event["event"] == "quota_hold_cleared" {
            assert_eq!(event["data"]["reason"], "window_gone_after_plan_change");
            cleared += 1;
        }
    }
    assert_eq!(cleared, 1);
}

#[tokio::test]
async fn partial_does_not_remove_or_count_as_a_complete_omission() {
    let f = Fixture::new().await;
    f.apply(usage("plus", true, true)).await;
    let (_, first) = f.apply(usage("plus", false, false)).await;
    assert!(has_five_hour(&first));
    let mut partial = usage("plus", false, false);
    partial["rate_limit"]["primary_window"]
        .as_object_mut()
        .unwrap()
        .remove("used_percent");
    let (_, partial_snapshot) = f.apply(partial).await;
    assert!(has_five_hour(&partial_snapshot));
    let (still_cooling, first_after_partial) = f.apply(usage("plus", false, false)).await;
    assert!(has_five_hour(&first_after_partial));
    assert!(matches!(
        still_cooling.health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    let (ready, second) = f.apply(usage("plus", false, false)).await;
    assert!(!has_five_hour(&second));
    assert_eq!(ready.health, Health::Ready);
}

#[tokio::test]
async fn partial_plan_change_waits_for_complete_inventory_and_preserves_payment() {
    let f = Fixture::new().await;
    f.apply(usage("plus", true, true)).await;
    f.relay
        .accounts
        .store
        .update_metadata(&f.id, |account| {
            account.apply_health_signal(
                crate::accounts::HealthSignal::Cooling {
                    reason: HealthReason::Payment,
                    until: Utc::now() + Duration::hours(6),
                    scope: "account".into(),
                },
                Utc::now(),
            );
        })
        .unwrap();
    let payment = f.account();
    let mut partial = usage("pro", false, false);
    partial["rate_limit"]
        .as_object_mut()
        .unwrap()
        .remove("secondary_window");
    let (pro, retained) = f.apply(partial).await;
    assert_eq!(pro.plan.as_deref(), Some("pro"));
    assert!(has_five_hour(&retained));
    assert_eq!(pro.health, payment.health);
    let (pro, complete) = f.apply(usage("pro", false, false)).await;
    assert!(!has_five_hour(&complete));
    assert_eq!(pro.health, payment.health);
    assert_eq!(pro.payment_until, payment.payment_until);
}

#[tokio::test]
async fn late_old_request_cannot_override_new_plan_or_ack_end_or_remove_windows() {
    let f = Fixture::new().await;
    f.apply(usage("plus", true, true)).await;
    let account = f.account();
    let started_at = Utc::now();
    let raw = serde_json::to_vec(&usage("free", false, false)).unwrap();
    let snapshot =
        crate::quota::parse_snapshot_for_test(Provider::Codex, &raw, Utc::now()).unwrap();
    let until = started_at - Duration::minutes(1);
    f.relay
        .accounts
        .store
        .update_metadata(&f.id, |account| {
            account.plan = Some("pro".into());
            account.plan_observed_at = Some(Utc::now());
            account.subscription = Some(SubscriptionMetadata::Codex {
                active_start: None,
                active_until: Some(until),
                last_checked: None,
                end_plan_checked_for: None,
            });
        })
        .unwrap();
    let before = f.relay.accounts.obs.quota_snapshot(&f.id).unwrap();
    let result = f
        .relay
        .accounts
        .apply_polled_usage_report(
            PollUsageReport {
                access_token: zeroize::Zeroizing::new("synthetic-r18-usage-old".into()),
                account,
                started_at,
                snapshot,
                complete: true,
            },
            None,
            None,
            false,
        )
        .await;
    assert!(result.is_err());
    assert_eq!(f.account().plan.as_deref(), Some("pro"));
    assert_eq!(f.relay.accounts.obs.quota_snapshot(&f.id).unwrap(), before);
    assert!(matches!(
        f.account().subscription,
        Some(SubscriptionMetadata::Codex {
            end_plan_checked_for: None,
            ..
        })
    ));
    let (current, _) = f.apply(usage("pro", false, false)).await;
    assert!(
        matches!(current.subscription, Some(SubscriptionMetadata::Codex { end_plan_checked_for: Some(value), .. }) if value == until)
    );
}

#[tokio::test]
async fn rotated_token_fences_late_usage_even_without_new_plan_timestamp() {
    let f = Fixture::new().await;
    f.apply(usage("plus", true, true)).await;
    let account = f.account();
    let started_at = Utc::now();
    let raw = serde_json::to_vec(&usage("pro", false, false)).unwrap();
    let snapshot =
        crate::quota::parse_snapshot_for_test(Provider::Codex, &raw, started_at).unwrap();
    let mut rotated = f.relay.accounts.store.tokens(&account).unwrap();
    rotated.access_token = "synthetic-r18-new".into();
    f.relay
        .accounts
        .store
        .save(&account, &rotated, Some(account.generation))
        .unwrap();
    let before = f.relay.accounts.obs.quota_snapshot(&f.id).unwrap();
    assert!(
        f.relay
            .accounts
            .apply_polled_usage_report(
                PollUsageReport {
                    access_token: zeroize::Zeroizing::new("synthetic-r18-usage-old".into()),
                    account,
                    started_at,
                    snapshot,
                    complete: true,
                },
                None,
                None,
                false
            )
            .await
            .is_err()
    );
    assert_eq!(f.account().plan.as_deref(), Some("plus"));
    assert_eq!(f.relay.accounts.obs.quota_snapshot(&f.id).unwrap(), before);
}

#[tokio::test]
async fn pre_end_request_completed_after_end_does_not_ack_post_end_check() {
    let f = Fixture::new().await;
    let now = Utc::now();
    let started_at = now - Duration::seconds(2);
    let until = now - Duration::seconds(1);
    f.relay
        .accounts
        .store
        .update_metadata(&f.id, |account| {
            account.subscription = Some(SubscriptionMetadata::Codex {
                active_start: None,
                active_until: Some(until),
                last_checked: None,
                end_plan_checked_for: None,
            });
        })
        .unwrap();
    let raw = serde_json::to_vec(&usage("plus", false, false)).unwrap();
    let snapshot = crate::quota::parse_snapshot_for_test(Provider::Codex, &raw, now).unwrap();
    let (account, _, _) = f
        .relay
        .accounts
        .apply_polled_usage_report(
            PollUsageReport {
                access_token: zeroize::Zeroizing::new("synthetic-r18-usage-old".into()),
                account: f.account(),
                started_at,
                snapshot,
                complete: true,
            },
            None,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(account.plan_observed_at, Some(started_at));
    assert!(matches!(
        account.subscription,
        Some(SubscriptionMetadata::Codex {
            end_plan_checked_for: None,
            ..
        })
    ));
    let (post_end, _) = f.apply(usage("plus", false, false)).await;
    assert!(
        matches!(post_end.subscription, Some(SubscriptionMetadata::Codex { end_plan_checked_for: Some(value), .. }) if value == until)
    );
}

#[tokio::test]
async fn failed_native_save_then_retry_uses_durable_window_cause_and_preserves_payment() {
    for payment in [false, true] {
        let f = Fixture::new().await;
        f.apply(usage("plus", true, true)).await;
        let saved = f.account();
        let path = f
            .relay
            .accounts
            .store
            .home()
            .join("accounts")
            .join(format!("{}.json", f.id));
        let original = std::fs::read(&path).unwrap();
        let mut record: Value = serde_json::from_slice(&original).unwrap();
        // Deterministic native write_metadata_record failure after reduction:
        // checked_add(1) refuses this generation on every platform. No I/O race,
        // global permissions, store hook replacement, or source fixture writes.
        record["generation"] = json!(u64::MAX);
        let mut poisoned = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
        std::io::Write::write_all(&mut poisoned, &serde_json::to_vec(&record).unwrap()).unwrap();
        poisoned.persist(&path).unwrap();
        let account = f.account();
        assert_eq!(account.generation, u64::MAX);
        let started_at = Utc::now();
        let body = serde_json::to_vec(&usage("pro", false, false)).unwrap();
        let snapshot =
            crate::quota::parse_snapshot_for_test(Provider::Codex, &body, started_at).unwrap();
        let result = f
            .relay
            .accounts
            .apply_polled_usage_report(
                PollUsageReport {
                    account,
                    access_token: zeroize::Zeroizing::new("synthetic-r18-usage-old".into()),
                    started_at,
                    snapshot,
                    complete: true,
                },
                None,
                None,
                false,
            )
            .await;
        assert!(result.is_err());
        let unchanged = f.account();
        assert_eq!(unchanged.plan, saved.plan);
        assert_eq!(unchanged.plan_observed_at, saved.plan_observed_at);
        assert_eq!(unchanged.health, saved.health);
        assert_eq!(unchanged.quota_cooling, saved.quota_cooling);
        assert!(
            !has_five_hour(&f.relay.accounts.obs.quota_snapshot(&f.id).unwrap()),
            "exercise the cache/durable divergence, not an early request failure"
        );
        let mut restored = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
        std::io::Write::write_all(&mut restored, &original).unwrap();
        restored.persist(&path).unwrap();
        if payment {
            f.relay
                .accounts
                .store
                .update_metadata(&f.id, |account| {
                    account.apply_health_signal(
                        crate::accounts::HealthSignal::Cooling {
                            reason: HealthReason::Payment,
                            until: Utc::now() + Duration::hours(6),
                            scope: "account".into(),
                        },
                        Utc::now(),
                    );
                })
                .unwrap();
        }
        let before_retry = f.account();
        let (retried, snapshot) = f.apply(usage("pro", false, false)).await;
        assert_eq!(retried.plan.as_deref(), Some("pro"));
        assert!(!has_five_hour(&snapshot));
        assert!(retried.quota_cooling.is_none());
        if payment {
            assert_eq!(retried.health, before_retry.health);
            assert_eq!(retried.payment_until, before_retry.payment_until);
        } else {
            assert_eq!(retried.health, Health::Ready);
        }
    }
}

#[tokio::test]
async fn held_busy_real_poll_updates_only_plan_and_end_marker_without_quota_authority() {
    for held_quota in [true, false] {
        let f = Fixture::new().await;
        let mut seed = usage("plus", held_quota, true);
        if !held_quota {
            seed["rate_limit"]["primary_window"]["used_percent"] = json!(10);
        }
        f.apply(seed).await;
        let until = Utc::now() - Duration::minutes(1);
        f.relay
            .accounts
            .store
            .update_metadata(&f.id, |account| {
                account.auth_failures = 1;
                account.auth_probe_pending = true;
                account.subscription = Some(SubscriptionMetadata::Codex {
                    active_start: None,
                    active_until: Some(until),
                    last_checked: None,
                    end_plan_checked_for: None,
                });
            })
            .unwrap();
        let account = f.account();
        let dispatched_generation = account.generation;
        let rejected_origin = f.relay.accounts.capture_poll_origin(&f.id).await.unwrap();
        let earliest_dispatch = Utc::now();
        let (arrived, release) = f.fake.state.pause_next_response();
        let response =
            ScriptedResponse::json(StatusCode::OK, usage("pro", false, false).to_string());
        f.fake.state.push(response);
        let mut events = f.relay.accounts.subscribe();
        let observe = f.relay.quota.poll_ws_quota(&f.id);
        let mutate = async {
            tokio::time::timeout(StdDuration::from_secs(5), arrived)
                .await
                .unwrap()
                .unwrap();
            let mut headers = HeaderMap::new();
            headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
            headers.insert(
                "x-codex-primary-used-percent",
                if held_quota { "100" } else { "20" }.parse().unwrap(),
            );
            headers.insert(
                "x-codex-primary-reset-at",
                (Utc::now() + Duration::hours(3))
                    .timestamp()
                    .to_string()
                    .parse()
                    .unwrap(),
            );
            f.relay
                .accounts
                .observe_headers(&f.id, Provider::Codex, &headers)
                .await;
            f.relay
                .accounts
                .store
                .update_metadata(&f.id, |account| {
                    account.label = "synthetic-busy-label".into();
                })
                .unwrap();
            let before = f.account();
            let quota = f.relay.accounts.obs.quota_snapshot(&f.id).unwrap();
            assert!(before.generation > dispatched_generation);
            while events.try_recv().is_ok() {}
            release.send(()).unwrap();
            (before, quota)
        };
        let (result, (before, quota)) = tokio::join!(observe, mutate);
        let (updated, receipt) = result.unwrap();
        assert!(receipt.is_none());
        assert_eq!(updated.plan.as_deref(), Some("pro"));
        let started_at = updated.plan_observed_at.unwrap();
        assert!(started_at >= earliest_dispatch && started_at <= Utc::now());
        let mut expected = before.clone();
        expected.plan = Some("pro".into());
        expected.plan_observed_at = Some(started_at);
        expected
            .subscription
            .as_mut()
            .unwrap()
            .mark_codex_end_checked(until, started_at);
        expected.generation = updated.generation;
        assert_eq!(
            updated, expected,
            "only descriptive fields and native generation may change"
        );
        assert_eq!(f.relay.accounts.obs.quota_snapshot(&f.id).unwrap(), quota);
        assert_eq!(updated.auth_failures, 1);
        assert!(
            updated.auth_probe_pending,
            "a descriptive-only result cannot complete the held probe"
        );
        assert!(
            f.relay
                .accounts
                .wait_owner
                .accept(&f.id, rejected_origin)
                .is_none()
        );
        while let Ok(event) = events.try_recv() {
            assert_ne!(event["event"], "quota_updated");
        }
        let requests = f.fake.state.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, axum::http::Method::GET);
        assert_eq!(requests[0].path_and_query, "/backend-api/wham/usage");
    }
}
