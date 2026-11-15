use super::*;
use crate::catalog::{RefreshFailure, RefreshMode};

fn runtime() -> (Arc<AccountRuntime>, tempfile::TempDir) {
    let base =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r30-runtime-tests");
    std::fs::create_dir_all(&base).unwrap();
    let home = qb_testkit::fixture::home_in(base).unwrap();
    let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
    let obs = Arc::new(Observability::new(home.path(), None).unwrap());
    (AccountRuntime::new(store, obs, 0.0), home)
}

fn account(id: &str, provider: Provider) -> Account {
    let mut account = crate::accounts::health::tests::fixture();
    account.id = id.to_owned();
    account.provider = provider;
    account
}

fn catalog(runtime: &AccountRuntime, id: &str, models: &[&str]) {
    runtime
        .catalogs
        .begin_refresh(id, RefreshMode::Force)
        .unwrap()
        .unwrap()
        .finish(Ok(
            json!({"data":models.iter().map(|id|json!({"id":id})).collect::<Vec<_>>()}),
        ))
        .unwrap();
}

fn ids(accounts: &[&str]) -> BTreeSet<String> {
    accounts.iter().map(|id| (*id).to_owned()).collect()
}

#[test]
fn unmanaged_unknown_is_preserved_but_known_codex_misses_are_excluded() {
    let (runtime, _home) = runtime();
    let accounts = [
        account("unknown", Provider::Codex),
        account("known", Provider::Codex),
        account("claude", Provider::Claude),
    ];
    catalog(&runtime, "known", &["a"]);
    assert_eq!(
        runtime.model_entitled_accounts(&accounts, Provider::Codex, Some("a")),
        ids(&["unknown", "known"])
    );
    assert_eq!(
        runtime.model_entitled_accounts(&accounts, Provider::Codex, Some("absent")),
        ids(&["unknown"])
    );
    runtime.catalogs.deny_model("unknown", "absent").unwrap();
    assert!(
        runtime
            .model_entitled_accounts(&accounts, Provider::Codex, Some("absent"))
            .is_empty()
    );
    assert_eq!(
        runtime.model_entitled_accounts(&accounts, Provider::Codex, None),
        ids(&["unknown", "known"])
    );
    assert!(
        runtime
            .model_entitled_accounts(&accounts, Provider::Codex, Some("bad model"))
            .is_empty()
    );
}

#[test]
fn claude_nowhere_fallback_never_bypasses_denial_of_a_known_model() {
    let (runtime, _home) = runtime();
    let accounts = [
        account("a", Provider::Claude),
        account("b", Provider::Claude),
    ];
    catalog(&runtime, "a", &["only-a"]);
    catalog(&runtime, "b", &["only-b"]);
    assert_eq!(
        runtime.model_entitled_accounts(&accounts, Provider::Claude, Some("new")),
        ids(&["a", "b"])
    );
    runtime.catalogs.deny_model("a", "new").unwrap();
    assert_eq!(
        runtime.model_entitled_accounts(&accounts, Provider::Claude, Some("new")),
        ids(&["b"])
    );
    runtime.catalogs.deny_model("a", "only-a").unwrap();
    assert!(
        runtime
            .model_entitled_accounts(&accounts, Provider::Claude, Some("only-a"))
            .is_empty()
    );
    runtime
        .catalogs
        .begin_refresh("a", RefreshMode::Force)
        .unwrap()
        .unwrap()
        .finish(Err(RefreshFailure::Transport))
        .unwrap();
    assert!(
        runtime
            .model_entitled_accounts(&accounts, Provider::Claude, Some("only-a"))
            .is_empty()
    );
    catalog(&runtime, "a", &["only-a"]);
    assert_eq!(
        runtime.model_entitled_accounts(&accounts, Provider::Claude, Some("only-a")),
        ids(&["a"])
    );
}

#[test]
fn named_models_exclude_unassignable_accounts_without_excluding_quota_cooling() {
    let (runtime, _home) = runtime();
    let now = Utc::now();
    let mut accounts: Vec<_> = ["cooling", "disabled", "drained", "relogin", "free"]
        .iter()
        .map(|id| account(id, Provider::Claude))
        .collect();
    accounts[0].health = Health::Cooling {
        reason: HealthReason::Quota,
        until: now + Duration::hours(1),
        scope: "model:a".into(),
    };
    accounts[1].user_state = UserState::Disabled;
    accounts[2].drain = true;
    accounts[3].health = Health::NeedsRelogin {
        reason: "fixture".into(),
        since: now,
    };
    accounts[4].plan = Some("free".into());
    assert_eq!(
        runtime.model_entitled_accounts(&accounts, Provider::Claude, Some("a")),
        ids(&["cooling"])
    );
    assert_eq!(
        runtime
            .model_entitled_accounts(&accounts, Provider::Claude, None)
            .len(),
        5
    );
}

#[test]
fn model_deadline_combines_only_applicable_native_and_volatile_holds() {
    let (runtime, _home) = runtime();
    let now = Utc::now();
    let mut account = account("a", Provider::Claude);
    let persisted = now + Duration::minutes(20);
    let volatile = now + Duration::minutes(30);
    account.health = Health::Cooling {
        reason: HealthReason::Quota,
        until: persisted,
        scope: "model:a".into(),
    };
    runtime.volatile.lock().unwrap().insert(
        account.id.clone(),
        Health::Cooling {
            reason: HealthReason::Quota,
            until: volatile,
            scope: "model:a".into(),
        },
    );
    assert_eq!(
        runtime.model_limited_until(&account, Some("a"), now),
        Some(volatile)
    );
    assert_eq!(runtime.model_limited_until(&account, Some("b"), now), None);
    assert_eq!(runtime.model_limited_until(&account, None, now), None);
    runtime.volatile.lock().unwrap().clear();
    assert_eq!(
        runtime.model_limited_until(&account, Some("a"), now),
        Some(persisted)
    );
    account.payment_until = Some(volatile);
    assert_eq!(
        runtime.model_limited_until(&account, Some("b"), now),
        Some(volatile)
    );
    account.payment_until = None;
    assert_eq!(
        runtime.model_limited_until(&account, Some("a"), volatile),
        None
    );
}

#[test]
fn cached_other_family_windows_do_not_supply_model_deadline_or_erase_native_hold() {
    let (runtime, _home) = runtime();
    let now = Utc::now();
    let mut account = account("a", Provider::Claude);
    let reset = now + Duration::days(1);
    runtime
        .obs
        .observe_poll(
            &account.id,
            json!({"observed_at":now,"source":"poll","windows":[
        {"name":"5h","used_pct":10.0,"observed_at":now,"reset_at":now+Duration::hours(2)},
        {"name":"7d_oi","used_pct":100.0,"observed_at":now,"reset_at":reset}
    ],"signals":{"unified_status":"rejected"}}),
            false,
        )
        .unwrap();
    assert_eq!(
        runtime.model_limited_until(&account, Some("claude-sonnet-5-5"), now),
        None
    );
    assert_eq!(
        runtime.model_limited_until(&account, Some("claude-haiku-4-5"), now),
        None
    );
    assert_eq!(
        runtime.model_limited_until(&account, Some("claude-fable-5-2"), now),
        Some(reset)
    );
    let native = now + Duration::minutes(15);
    account.health = Health::Cooling {
        reason: HealthReason::Quota,
        until: native,
        scope: "model:claude-sonnet-5-5".into(),
    };
    assert_eq!(
        runtime.model_limited_until(&account, Some("claude-sonnet-5-5"), now),
        Some(native)
    );
    assert!(matches!(account.health, Health::Cooling{until,..} if until == native));
}

#[test]
fn draining_incumbent_is_preserved_but_other_drained_targets_and_denials_are_not() {
    for provider in [Provider::Claude, Provider::Codex] {
        let (runtime, _home) = runtime();
        let mut accounts = [
            account("incumbent", provider),
            account("drained", provider),
            account("target", provider),
        ];
        accounts[0].drain = true;
        accounts[1].drain = true;
        for account in &accounts {
            catalog(&runtime, &account.id, &["a"]);
        }
        assert_eq!(
            runtime.model_entitled_accounts(&accounts, provider, Some("a")),
            ids(&["target"])
        );
        assert_eq!(
            runtime.model_entitled_accounts_with_incumbent(
                &accounts,
                provider,
                Some("a"),
                Some("incumbent")
            ),
            ids(&["incumbent", "target"])
        );
        runtime.catalogs.deny_model("incumbent", "a").unwrap();
        assert_eq!(
            runtime.model_entitled_accounts_with_incumbent(
                &accounts,
                provider,
                Some("a"),
                Some("incumbent")
            ),
            ids(&["target"])
        );
        catalog(&runtime, "incumbent", &["a"]);
        accounts[0].user_state = UserState::Disabled;
        assert_eq!(
            runtime.model_entitled_accounts_with_incumbent(
                &accounts,
                provider,
                Some("a"),
                Some("incumbent")
            ),
            ids(&["target"])
        );
        if provider == Provider::Claude {
            accounts[0].user_state = UserState::Active;
            accounts[0].plan = Some("free".into());
            assert_eq!(
                runtime.model_entitled_accounts_with_incumbent(
                    &accounts,
                    provider,
                    Some("a"),
                    Some("incumbent")
                ),
                ids(&["target"])
            );
        }
    }
}

fn cached_fable(
    runtime: &AccountRuntime,
    id: &str,
    observed: DateTime<Utc>,
    reset: DateTime<Utc>,
    used: f64,
) {
    runtime
        .obs
        .observe_poll(
            id,
            json!({"observed_at":observed,"source":"poll","windows":[
                {"name":"7d_oi","used_pct":used,"observed_at":observed,"reset_at":reset}
            ]}),
            true,
        )
        .unwrap();
}

#[test]
fn ready_cached_model_status_recovers_without_mutating_observation_or_native_hold() {
    let (runtime, _home) = runtime();
    let mut account = account("a", Provider::Claude);
    let now = Utc::now();
    let observed = now - Duration::seconds(37);
    let reset = now + Duration::hours(3);
    let model = Some("claude-fable-5-2");
    cached_fable(&runtime, &account.id, observed, reset, 100.0);
    let before = runtime.obs.quota_snapshot(&account.id).unwrap();
    let status = runtime.model_quota_status(&account, model, now).unwrap();
    assert_eq!(status.window.as_deref(), Some("7d_oi"));
    assert_eq!(status.reset_at, reset);
    assert!(status.model_limited);
    assert_eq!(
        Some(status.reset_at),
        runtime.model_limited_until(&account, model, now)
    );
    assert!(
        runtime
            .model_quota_status(&account, Some("claude-sonnet-5-5"), now)
            .is_none()
    );
    assert_eq!(account.health, Health::Ready);
    assert_eq!(runtime.obs.quota_snapshot(&account.id).unwrap(), before);
    cached_fable(&runtime, &account.id, now, reset, 20.0);
    assert!(runtime.model_quota_status(&account, model, now).is_none());
    assert_eq!(runtime.model_limited_until(&account, model, now), None);
    account.health = Health::Cooling {
        reason: HealthReason::Quota,
        until: reset,
        scope: "model:claude-fable-5-2".into(),
    };
    let status = runtime.model_quota_status(&account, model, now).unwrap();
    assert_eq!(status.reset_at, reset);
    assert!(status.model_limited);
    assert_eq!(
        status.window, None,
        "no window attribution without native evidence"
    );
}

fn save_account(runtime: &AccountRuntime, label: &str, provider: Provider) -> Account {
    let mut account = crate::accounts::health::tests::fixture();
    account.provider = provider;
    account.identity = match provider {
        Provider::Claude => crate::accounts::Identity::Claude {
            account_uuid: label.into(),
            organization_uuid: None,
            email: format!("{label}@example.invalid"),
        },
        Provider::Codex => crate::accounts::Identity::Codex {
            user_id: None,
            account_id: label.into(),
            email: format!("{label}@example.invalid"),
        },
    };
    account.id = account.identity.id();
    runtime
        .store
        .save(
            &account,
            &crate::accounts::Tokens {
                access_token: "synthetic-r30".into(),
                refresh_token: "synthetic-r30-refresh".into(),
                id_token: None,
            },
            None,
        )
        .unwrap()
}

#[tokio::test]
async fn ready_cached_notice_recomputes_entitled_recovery_and_disappears_after_recovery() {
    let (runtime, _home) = runtime();
    let unrelated = save_account(&runtime, "unrelated", Provider::Claude);
    let account = save_account(&runtime, "entitled", Provider::Claude);
    let model = "claude-fable-5-2";
    catalog(&runtime, &unrelated.id, &["claude-sonnet-5-5"]);
    catalog(&runtime, &account.id, &[model]);
    let now = Utc::now();
    let wrong_reset = now + Duration::minutes(10);
    let reset = now + Duration::hours(3);
    cached_fable(&runtime, &unrelated.id, now, wrong_reset, 100.0);
    cached_fable(&runtime, &account.id, now, reset, 100.0);
    runtime.set_quota_notice(&account, json!({"action":"exhausted","window":"7d_oi","next_account":null,
        "reset_at":wrong_reset,"earliest_account":unrelated.id,"model_limited":false,"observed_at":now}), Some(model));
    let views = runtime.views().await.unwrap();
    let row = views.iter().find(|row| row["id"] == account.id).unwrap();
    assert_eq!(row["quota_notice"]["earliest_account"], account.id);
    assert_eq!(row["quota_notice"]["reset_at"], json!(reset));
    assert_eq!(row["quota_notice"]["model_limited"], true);
    assert_eq!(
        runtime.store.load(&account.id).unwrap().health,
        Health::Ready
    );
    cached_fable(&runtime, &account.id, Utc::now(), reset, 20.0);
    let views = runtime.views().await.unwrap();
    let row = views.iter().find(|row| row["id"] == account.id).unwrap();
    assert!(row["quota_notice"].is_null());
}

#[tokio::test]
async fn mixed_entitled_native_later_and_cached_ready_earlier_uses_earlier_429() {
    let (runtime, _home) = runtime();
    let cached = save_account(&runtime, "cached", Provider::Codex);
    let native = save_account(&runtime, "native", Provider::Codex);
    let model = "a";
    for account in [&cached, &native] {
        catalog(&runtime, &account.id, &[model]);
    }
    let now = Utc::now();
    let earlier = now + Duration::minutes(20);
    let later = now + Duration::hours(1);
    runtime
        .obs
        .observe_poll(
            &cached.id,
            json!({"observed_at":now,"source":"poll","windows":[
                {"name":"5h","used_pct":100.0,"observed_at":now,"reset_at":earlier}
            ]}),
            true,
        )
        .unwrap();
    runtime
        .store
        .update_metadata(&native.id, |account| {
            account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: later,
                scope: "account".into(),
            }
        })
        .unwrap();
    let response = runtime
        .unavailable_known(Provider::Codex, false, Some(model))
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    assert!((1..=1200).contains(&retry));
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["resets_at"], earlier.timestamp());
}
