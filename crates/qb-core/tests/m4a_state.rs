use chrono::{Duration, TimeZone, Utc};
use qb_core::accounts::{
    Account, AccountStore, Health, HealthReason, HealthSignal, StoreError, UserState,
    health::eligible,
};
use serde_json::json;
use std::{fs, sync::Arc};

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
}

fn account() -> Account {
    let identity = qb_core::accounts::Identity::Codex {
        account_id: "synthetic-state".into(),
        email: "fixture@example.invalid".into(),
    };
    serde_json::from_value(json!({
        "schema": 1, "id": identity.id(), "provider": "codex", "label": "synthetic",
        "email": "fixture@example.invalid", "plan": null, "identity": identity,
        "source": {"kind": "oauth"}, "user_state": "active", "health": {"kind": "ready"},
        "generation": 7,
        "token_meta": {"expires_at": null, "last_refresh_at": null, "refresh_uncertain": false}
    }))
    .unwrap()
}

fn acquire(home: &std::path::Path) -> AccountStore {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match AccountStore::acquire(home) {
            Ok(store) => return store,
            Err(StoreError::LeaseBusy) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("synthetic credential lease: {error}"),
        }
    }
}

fn assert_cooling(account: &Account, reason: HealthReason, minutes: i64, scope: &str) {
    assert_eq!(
        account.health,
        Health::Cooling {
            reason,
            until: now() + Duration::minutes(minutes),
            scope: scope.into(),
        }
    );
}

#[test]
fn c5_eligibility_model_scope_tried_and_independent_user_state() {
    let mut a = account();
    assert!(!a.drain && !a.allow_credits && a.auth_failures == 0);
    a.health = Health::Cooling {
        reason: HealthReason::Permission,
        until: now() + Duration::hours(1),
        scope: "model:blocked".into(),
    };
    assert!(a.eligible_at(None, now()));
    assert!(!a.eligible_at(Some("blocked"), now()));
    assert!(a.eligible_at(Some("other"), now()));
    assert!(!eligible(&a, Some("other"), std::slice::from_ref(&a.id)));
    for state in [UserState::Paused, UserState::Disabled] {
        a.user_state = state;
        assert!(!a.eligible_at(Some("other"), now()));
    }
    a.user_state = UserState::Active;
    a.drain = true;
    assert!(a.eligible_at(Some("other"), now()));
}

#[test]
fn c5_link_stale_and_newer_token() {
    let mut a = account();
    a.apply_health_signal(
        HealthSignal::LinkStale {
            retry_at: now() + Duration::minutes(10),
        },
        now(),
    );
    assert_cooling(&a, HealthReason::LinkStale, 10, "account");
    a.apply_health_signal(HealthSignal::NewerLinkedToken, now());
    assert_eq!(a.health, Health::Ready);
}

#[test]
fn c5_persistence_metadata_generation_blob_lease_and_load_expiry() {
    // This synthetic blob deliberately cannot be decrypted. Metadata operations
    // must work without any DPAPI call or real credentials.
    let sandbox =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/state-fixtures");
    fs::create_dir_all(&sandbox).unwrap();
    let home = tempfile::tempdir_in(&sandbox).unwrap();
    fs::create_dir(home.path().join("accounts")).unwrap();
    let a = account();
    let path = home.path().join("accounts").join(format!("{}.json", a.id));
    let mut record = serde_json::to_value(&a).unwrap();
    record["secret_dpapi_b64"] = json!("synthetic-opaque-blob");
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let store = Arc::new(acquire(home.path()));
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = events.clone();
    store.set_observer(Arc::new(move |account, previous| {
        recorded.lock().unwrap().push((
            account.generation,
            previous.clone(),
            account.health.clone(),
        ));
    }));
    assert!(matches!(
        AccountStore::acquire(home.path()),
        Err(StoreError::LeaseBusy)
    ));
    let updated = store
        .update_metadata(&a.id, |a| {
            a.apply_health_signal(HealthSignal::AuthAfterRefresh, now());
            a.drain = true;
            a.allow_credits = true;
        })
        .unwrap();
    assert_eq!(updated.generation, 8);
    assert_eq!(updated.auth_failures, 1);
    assert_eq!(
        events.lock().unwrap()[0],
        (8, Health::Ready, updated.health.clone())
    );
    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["secret_dpapi_b64"], "synthetic-opaque-blob");
    assert!(matches!(
        store.save(
            &a,
            &qb_core::accounts::Tokens {
                access_token: "synthetic".into(),
                refresh_token: "synthetic".into(),
                id_token: None
            },
            Some(a.generation)
        ),
        Err(StoreError::GenerationConflict { .. })
    ));
    assert!(matches!(
        store.update_metadata(&a.id, |a| a.token_meta.refresh_uncertain = true),
        Err(StoreError::InvalidData)
    ));
    assert!(matches!(
        store.update_metadata(&a.id, |a| a.health = Health::Cooling {
            reason: HealthReason::Rate,
            until: now(),
            scope: "invalid".into()
        }),
        Err(StoreError::InvalidData)
    ));
    assert_eq!(store.load(&a.id).unwrap(), updated);
    assert_eq!(events.lock().unwrap().len(), 1);
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            let id = a.id.clone();
            std::thread::spawn(move || {
                store
                    .update_metadata(&id, |a| a.auth_failures += 1)
                    .unwrap()
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(store.load(&a.id).unwrap().auth_failures, 9);
    store
        .update_metadata(&a.id, |a| {
            a.health = Health::Cooling {
                reason: HealthReason::Auth,
                until: Utc::now() - Duration::seconds(1),
                scope: "account".into(),
            }
        })
        .unwrap();
    drop(store);
    let store = acquire(home.path());
    let recorded = events.clone();
    store.set_observer(Arc::new(move |account, previous| {
        recorded.lock().unwrap().push((
            account.generation,
            previous.clone(),
            account.health.clone(),
        ));
    }));
    let loaded = store.load(&a.id).unwrap();
    assert_eq!(loaded.health, Health::Ready);
    assert_eq!(loaded.generation, 18);
    assert_eq!(loaded.auth_failures, 9);
    assert!(matches!(
        events.lock().unwrap().last(),
        Some((
            18,
            Health::Cooling {
                reason: HealthReason::Auth,
                ..
            },
            Health::Ready
        ))
    ));
    let persisted: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(persisted["health"]["kind"], "ready");
    assert_eq!(persisted["generation"], 18);
    assert_eq!(persisted["secret_dpapi_b64"], "synthetic-opaque-blob");
    assert_eq!(store.list().unwrap()[0].health, Health::Ready);
    assert_eq!(
        AccountStore::list_metadata(home.path()).unwrap()[0].health,
        Health::Ready
    );
    store
        .update_metadata(&a.id, |a| {
            a.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: Utc::now() - Duration::seconds(1),
                scope: "account".into(),
            }
        })
        .unwrap();
    // Read-only preview cannot write or advance generation.
    assert_eq!(
        AccountStore::list_metadata(home.path()).unwrap()[0].generation,
        19
    );
    let listed = store.list().unwrap();
    assert_eq!(listed[0].health, Health::Ready);
    assert_eq!(listed[0].generation, 20);
    assert_eq!(store.list().unwrap()[0].generation, 20);
}

#[test]
fn c5_reimport_preserves_metadata_and_only_newer_tokens_clear_needs_relogin() {
    let sandbox =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/state-fixtures");
    fs::create_dir_all(&sandbox).unwrap();
    let home = tempfile::tempdir_in(&sandbox).unwrap();
    let source = home.path().join("synthetic-cpa");
    fs::create_dir(&source).unwrap();
    let file = source.join("fixture.json");
    let mut fixture = json!({
        "type": "codex", "account_id": "synthetic-state", "email": "fixture@example.invalid",
        "access_token": "synthetic-access", "refresh_token": "synthetic-refresh", "id_token": null,
        "last_refresh": now(), "expired": now() + Duration::hours(1)
    });
    fs::write(&file, serde_json::to_vec(&fixture).unwrap()).unwrap();
    let store = acquire(home.path());
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = events.clone();
    store.set_observer(Arc::new(move |account, previous| {
        recorded
            .lock()
            .unwrap()
            .push((previous.clone(), account.health.clone()));
    }));
    qb_core::accounts::import::run(&store, &source, &[], false).unwrap();
    let id = account().id;
    let terminal = store
        .update_metadata(&id, |a| {
            // Legacy persisted state from before linked revocations were distinguished.
            a.health = Health::NeedsRelogin {
                reason: "token_revoked".into(),
                since: now(),
            };
            a.user_state = UserState::Paused;
            a.auth_failures = 3;
            a.drain = true;
            a.allow_credits = true;
            a.label = "User label".into();
        })
        .unwrap();
    // Equal timestamps may import tokens, but cannot clear terminal health.
    qb_core::accounts::import::run(&store, &source, &[], false).unwrap();
    let equal = store.load(&id).unwrap();
    assert_eq!(equal.health, terminal.health);
    assert_eq!(equal.auth_failures, 3);
    assert_eq!(equal.label, "User label");
    assert!(equal.drain && equal.allow_credits);
    assert_eq!(equal.user_state, UserState::Paused);
    fixture["last_refresh"] = json!(now() - Duration::minutes(1));
    fs::write(&file, serde_json::to_vec(&fixture).unwrap()).unwrap();
    qb_core::accounts::import::run(&store, &source, &[], false).unwrap();
    assert_eq!(store.load(&id).unwrap(), equal);
    fixture["last_refresh"] = json!(now() + Duration::minutes(1));
    fs::write(&file, serde_json::to_vec(&fixture).unwrap()).unwrap();
    qb_core::accounts::import::run(&store, &source, &[], false).unwrap();
    let newer = store.load(&id).unwrap();
    assert_eq!(newer.health, Health::Ready);
    assert_eq!(newer.auth_failures, 0);
    assert!(newer.drain && newer.allow_credits);
    assert_eq!(newer.user_state, UserState::Paused);
    assert_eq!(newer.label, "User label");
    assert!(matches!(
        events.lock().unwrap().last(),
        Some((Health::NeedsRelogin { .. }, Health::Ready))
    ));
}

mod runtime_regressions {
    //! Synthetic regressions through the runtime that production relay calls.
    use axum::http::{HeaderMap, StatusCode};
    use chrono::{DateTime, Duration, Utc};
    use qb_core::{
        account_runtime::{AccountRuntime, HealthSignal},
        accounts::{
            Account, AccountStore, Health, HealthReason, Identity, Provider, StoreError, UserState,
        },
        observability::Observability,
    };
    use serde_json::{Value, json};
    use std::{fs, sync::Arc};

    struct Fixture {
        runtime: Arc<AccountRuntime>,
        ids: Vec<String>,
        // Drop the store/runtime before removing synthetic files.
        _home: tempfile::TempDir,
    }
    impl Fixture {
        fn new(providers: &[Provider], allowance: f64) -> Self {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/m4a1-runtime");
            fs::create_dir_all(&root).unwrap();
            let home = tempfile::tempdir_in(&root).unwrap();
            fs::create_dir(home.path().join("accounts")).unwrap();
            let ids = providers.iter().enumerate().map(|(index, provider)| {
            let identity = match provider {
                Provider::Codex => Identity::Codex { account_id: format!("synthetic-{index}"), email: "fixture@example.invalid".into() },
                Provider::Claude => Identity::Claude { account_uuid: format!("synthetic-{index}"), organization_uuid: None, email: "fixture@example.invalid".into() },
            };
            let id = identity.id();
            let record = json!({
                "schema":1,"id":id,"provider":provider,"label":"synthetic",
                "email":"fixture@example.invalid","plan":null,"identity":identity,
                "source":{"kind":"oauth"},"user_state":"active","health":{"kind":"ready"},
                "generation":1,"token_meta":{"expires_at":null,"last_refresh_at":null,"refresh_uncertain":false},
                "secret_dpapi_b64":"synthetic-opaque-blob"
            });
            fs::write(home.path().join("accounts").join(format!("{id}.json")), serde_json::to_vec(&record).unwrap()).unwrap();
            id
        }).collect();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let store = loop {
                match AccountStore::acquire(home.path()) {
                    Ok(store) => break Arc::new(store),
                    Err(StoreError::LeaseBusy) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(error) => panic!("synthetic lease: {error}"),
                }
            };
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            Self {
                runtime: AccountRuntime::new(store, obs, allowance),
                ids,
                _home: home,
            }
        }
        fn load(&self, index: usize) -> Account {
            self.runtime.store.load(&self.ids[index]).unwrap()
        }
        async fn cool(
            &self,
            index: usize,
            reason: HealthReason,
            until: DateTime<Utc>,
            scope: &str,
        ) -> Account {
            self.runtime
                .transition(
                    &self.ids[index],
                    HealthSignal::Cooling {
                        reason,
                        until,
                        scope: scope.into(),
                    },
                )
                .await
                .unwrap()
        }
        async fn snapshot(
            &self,
            windows: Vec<Value>,
            signals: Value,
            model: Option<&str>,
        ) -> Account {
            self.runtime
                .apply_quota_snapshot(
                    &self.ids[0],
                    json!({"windows":windows,"signals":signals}),
                    model,
                )
                .await
                .unwrap()
        }
    }
    fn window(name: &str, used: f64, reset: DateTime<Utc>, observed: DateTime<Utc>) -> Value {
        json!({"name":name,"used_pct":used,"reset_at":reset,"observed_at":observed})
    }
    fn assert_cooling(account: &Account, reason: HealthReason, until: DateTime<Utc>, scope: &str) {
        assert_eq!(
            account.health,
            Health::Cooling {
                reason,
                until,
                scope: scope.into()
            }
        );
    }
    fn codex_headers(primary: f64, secondary: f64, now: DateTime<Utc>) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (slot, used, minutes, reset) in [
            ("primary", primary, 300, now + Duration::hours(1)),
            ("secondary", secondary, 10080, now + Duration::days(5)),
        ] {
            h.insert(
                format!("x-codex-{slot}-used-percent")
                    .parse::<axum::http::HeaderName>()
                    .unwrap(),
                used.to_string().parse().unwrap(),
            );
            h.insert(
                format!("x-codex-{slot}-window-minutes")
                    .parse::<axum::http::HeaderName>()
                    .unwrap(),
                minutes.to_string().parse().unwrap(),
            );
            h.insert(
                format!("x-codex-{slot}-reset-at")
                    .parse::<axum::http::HeaderName>()
                    .unwrap(),
                reset.timestamp().to_string().parse().unwrap(),
            );
        }
        h
    }

    #[tokio::test]
    async fn m2_two_window_credits_hold_uses_only_triggering_future_reset() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        let now = Utc::now();
        f.runtime
            .observe_headers(&f.ids[0], Provider::Codex, &codex_headers(99.0, 40.0, now))
            .await;
        assert_cooling(
            &f.load(0),
            HealthReason::Quota,
            DateTime::from_timestamp((now + Duration::hours(1)).timestamp(), 0).unwrap(),
            "account",
        );
        f.runtime
            .transition(&f.ids[0], HealthSignal::Ready)
            .await
            .unwrap();
        f.runtime
            .observe_headers(&f.ids[0], Provider::Codex, &codex_headers(40.0, 99.0, now))
            .await;
        assert_cooling(
            &f.load(0),
            HealthReason::Quota,
            DateTime::from_timestamp((now + Duration::days(5)).timestamp(), 0).unwrap(),
            "account",
        );
    }

    #[tokio::test]
    async fn m2_exhausted_windows_latest_trigger_and_no_trigger_soonest() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        let now = Utc::now();
        let short = now + Duration::hours(1);
        let long = now + Duration::days(5);
        for (primary, secondary, signals, expected) in [
            (100.0, 40.0, json!({}), short),
            (40.0, 100.0, json!({}), long),
            (100.0, 100.0, json!({}), long),
            (40.0, 40.0, json!({"limit_reached":true}), short),
            (
                40.0,
                40.0,
                json!({"limit_reached":true,"active_limit":"secondary"}),
                long,
            ),
            (
                40.0,
                40.0,
                json!({"serving_from_credits":true,"active_limit":"primary"}),
                short,
            ),
            (40.0, 40.0, json!({"unified_status":"rejected"}), short),
        ] {
            f.runtime
                .transition(&f.ids[0], HealthSignal::Ready)
                .await
                .unwrap();
            let a = f
                .snapshot(
                    vec![
                        window("5h", primary, short, now),
                        window("7d", secondary, long, now),
                    ],
                    signals,
                    None,
                )
                .await;
            assert_cooling(&a, HealthReason::Quota, expected, "account");
        }
    }

    #[tokio::test]
    async fn m2_stale_merged_windows_do_not_recool_or_write() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        let now = Utc::now();
        let before = f.load(0);
        let a = f
            .snapshot(
                vec![
                    window("5h", 100.0, now - Duration::seconds(1), now),
                    window("7d", 20.0, now + Duration::days(5), now),
                ],
                json!({}),
                None,
            )
            .await;
        assert_eq!(a.health, Health::Ready);
        assert_eq!(a.generation, before.generation);
        let a = f
            .snapshot(
                vec![window("5h", 100.0, now - Duration::seconds(1), now)],
                json!({"limit_reached":true}),
                None,
            )
            .await;
        assert_eq!(a, before);
    }

    #[tokio::test]
    async fn s2_concurrent_coolings_keep_later_until_and_account_scope() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        let now = Utc::now();
        let long = now + Duration::hours(5);
        let (a, b) = tokio::join!(
            f.cool(0, HealthReason::Quota, long, "account"),
            f.cool(
                0,
                HealthReason::Rate,
                now + Duration::seconds(11),
                "model:x"
            )
        );
        assert!(matches!(a.health, Health::Cooling { .. }));
        assert!(matches!(b.health, Health::Cooling { .. }));
        assert_cooling(&f.load(0), HealthReason::Quota, long, "account");
        let later = long + Duration::hours(1);
        f.cool(0, HealthReason::Permission, later, "model:x").await;
        assert_cooling(&f.load(0), HealthReason::Permission, later, "account");
        f.runtime
            .transition(&f.ids[0], HealthSignal::Revoked("token_revoked".into()))
            .await
            .unwrap();
        f.cool(
            0,
            HealthReason::Quota,
            later + Duration::hours(1),
            "account",
        )
        .await;
        assert!(matches!(f.load(0).health, Health::NeedsRelogin { .. }));
    }

    #[tokio::test]
    async fn s3_early_reset_requires_fresh_causing_window_and_shift_over_300_seconds() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        let now = Utc::now();
        let reset = now + Duration::hours(1);
        let weekly = now + Duration::days(5);
        let cooled = f
            .snapshot(
                vec![
                    window("5h", 100.0, reset, now),
                    window("7d", 20.0, weekly, now),
                ],
                json!({}),
                None,
            )
            .await;
        let evidence = cooled.quota_cooling.clone().unwrap();
        assert_eq!(evidence.window, "5h");
        assert_eq!(evidence.reset_at, reset);
        // Evidence survives a disk reload and unrelated metadata changes.
        assert_eq!(f.load(0).quota_cooling, Some(evidence.clone()));
        f.runtime
            .change(&f.ids[0], None, Some("new label".into()), None, None)
            .await
            .unwrap();
        let fresh = evidence.started_at + Duration::seconds(1);
        for windows in [
            vec![window(
                "5h",
                20.0,
                reset + Duration::seconds(301),
                evidence.started_at,
            )],
            vec![window("7d", 20.0, weekly + Duration::hours(1), fresh)],
            vec![window("5h", 20.0, reset + Duration::seconds(1), fresh)],
            vec![window("5h", 20.0, reset + Duration::seconds(300), fresh)],
            vec![window("5h", 100.0, reset + Duration::seconds(301), fresh)],
        ] {
            let a = f.snapshot(windows, json!({}), None).await;
            assert!(matches!(a.health, Health::Cooling { .. }));
        }
        // The depleted observation above extends cooling and starts new evidence.
        let current = f.load(0).quota_cooling.unwrap();
        let a = f
            .snapshot(
                vec![window(
                    "5h",
                    20.0,
                    current.reset_at - Duration::seconds(301),
                    current.started_at + Duration::seconds(1),
                )],
                json!({}),
                None,
            )
            .await;
        assert_eq!(a.health, Health::Ready);
        assert!(a.quota_cooling.is_none());
    }

    #[tokio::test]
    async fn s3_quota_rejection_without_window_evidence_cannot_reset_early() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        let now = Utc::now();
        f.cool(0, HealthReason::Quota, now + Duration::hours(1), "account")
            .await;
        let a = f
            .snapshot(
                vec![window(
                    "5h",
                    10.0,
                    now + Duration::hours(2),
                    now + Duration::seconds(1),
                )],
                json!({}),
                None,
            )
            .await;
        assert!(matches!(a.health, Health::Cooling { .. }));
        assert!(a.quota_cooling.is_none());
    }

    #[tokio::test]
    async fn s4_unknown_model_eligible_but_matching_model_blocked() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        f.cool(
            0,
            HealthReason::Permission,
            Utc::now() + Duration::hours(1),
            "model:x",
        )
        .await;
        assert!(f.runtime.eligible(&f.ids[0], None).await);
        assert!(f.runtime.eligible(&f.ids[0], Some("other")).await);
        assert!(!f.runtime.eligible(&f.ids[0], Some("x")).await);
        f.runtime
            .change(&f.ids[0], Some(UserState::Paused), None, None, None)
            .await
            .unwrap();
        assert!(!f.runtime.eligible(&f.ids[0], None).await);
    }

    #[tokio::test]
    async fn s5_subbucket_passive_rejection_only_cools_known_request_model() {
        let f = Fixture::new(&[Provider::Claude], 0.0);
        let now = Utc::now();
        let reset = now + Duration::days(5);
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-7d_oi-utilization",
            "1".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d_oi-status",
            "rejected".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d_oi-reset",
            reset.timestamp().to_string().parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-status",
            "rejected".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-representative-claim",
            "7d_oi".parse().unwrap(),
        );
        f.runtime
            .observe_headers(&f.ids[0], Provider::Claude, &h)
            .await;
        assert_eq!(f.load(0).health, Health::Ready);
        f.runtime
            .observe_headers_for_model(&f.ids[0], Provider::Claude, &h, Some("opus"))
            .await;
        assert_cooling(
            &f.load(0),
            HealthReason::Quota,
            DateTime::from_timestamp(reset.timestamp(), 0).unwrap(),
            "model:opus",
        );
        assert!(f.runtime.eligible(&f.ids[0], Some("sonnet")).await);
        assert!(f.runtime.eligible(&f.ids[0], None).await);
    }

    #[tokio::test]
    async fn s6_mixed_terminal_and_cooling_candidates_use_row14() {
        let f = Fixture::new(&[Provider::Codex, Provider::Codex, Provider::Codex], 0.0);
        f.runtime
            .transition(&f.ids[0], HealthSignal::Revoked("token_revoked".into()))
            .await
            .unwrap();
        f.cool(
            1,
            HealthReason::Quota,
            Utc::now() + Duration::hours(1),
            "account",
        )
        .await;
        f.cool(
            2,
            HealthReason::Permission,
            Utc::now() + Duration::hours(2),
            "account",
        )
        .await;
        for ws in [false, true] {
            let response = f.runtime.unavailable(Provider::Codex, ws, None).await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(response.headers().contains_key("retry-after"));
        }
        f.runtime
            .change(&f.ids[1], Some(UserState::Paused), None, None, None)
            .await
            .unwrap();
        // Permission cooling is also temporary, so row 15 remains inappropriate.
        assert_eq!(
            f.runtime
                .unavailable(Provider::Codex, false, None)
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        f.runtime
            .change(&f.ids[2], Some(UserState::Disabled), None, None, None)
            .await
            .unwrap();
        assert_eq!(
            f.runtime
                .unavailable(Provider::Codex, false, None)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn s6_all_cooling_reasons_and_volatile_store_error_use_native_row14() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        // A stale cached limit must not be replayed for a different reason.
        f.runtime.cache_limit(
            &f.ids[0],
            StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new(),
            b"old cached quota",
        );
        for reason in [
            HealthReason::LinkStale,
            HealthReason::Permission,
            HealthReason::Payment,
            HealthReason::StoreError,
        ] {
            f.runtime
                .transition(&f.ids[0], HealthSignal::Ready)
                .await
                .unwrap();
            f.cool(0, reason, Utc::now() + Duration::minutes(5), "account")
                .await;
            for ws in [false, true] {
                let response = f.runtime.unavailable(Provider::Codex, ws, None).await;
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
                assert!(response.headers().contains_key("retry-after"));
                let raw = axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap();
                let body: Value = serde_json::from_slice(&raw).unwrap();
                assert_eq!(body["error"]["type"], "usage_limit_reached");
            }
        }
        f.runtime
            .transition(&f.ids[0], HealthSignal::Ready)
            .await
            .unwrap();
        let mut warning = f.load(0);
        warning.health = Health::Cooling {
            reason: HealthReason::StoreError,
            until: Utc::now() + Duration::minutes(5),
            scope: "account".into(),
        };
        f.runtime.emit_account(&warning);
        assert_eq!(f.load(0).health, Health::Ready);
        for ws in [false, true] {
            let response = f.runtime.unavailable(Provider::Codex, ws, None).await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(response.headers().contains_key("retry-after"));
            let raw = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(body["error"]["type"], "usage_limit_reached");
        }
    }

    #[tokio::test]
    async fn s6_genuine_limit_cache_wins_over_sooner_nonlimit_cooling() {
        let f = Fixture::new(&[Provider::Codex, Provider::Codex], 0.0);
        f.cool(
            0,
            HealthReason::LinkStale,
            Utc::now() + Duration::minutes(1),
            "account",
        )
        .await;
        let until = Utc::now() + Duration::hours(1);
        f.cool(1, HealthReason::Quota, until, "account").await;
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "gzip".parse().unwrap());
        f.runtime.cache_limit(
            &f.ids[1],
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            b"synthetic encoded quota",
        );
        let response = f.runtime.unavailable(Provider::Codex, false, None).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["content-encoding"], "gzip");
        assert!(
            response.headers()["retry-after"]
                .to_str()
                .unwrap()
                .parse::<i64>()
                .unwrap()
                > 3000
        );
        let raw = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert_eq!(&raw[..], b"synthetic encoded quota");
    }

    #[tokio::test]
    async fn n5_inflight_counts_each_request_but_only_observed_usage_cools() {
        let f = Fixture::new(&[Provider::Codex, Provider::Codex], 0.5);
        let now = Utc::now();
        let reset = now + Duration::hours(1);
        let other = f.runtime.track_inflight(&f.ids[1]);
        let first = f.runtime.track_inflight(&f.ids[0]);
        assert_eq!(f.runtime.inflight_count(&f.ids[0]), 1);
        let a = f
            .snapshot(vec![window("5h", 98.0, reset, now)], json!({}), None)
            .await;
        assert_eq!(a.health, Health::Ready);
        let second = f.runtime.track_inflight(&f.ids[0]);
        assert_eq!(f.runtime.inflight_count(&f.ids[0]), 2);
        let a = f
            .snapshot(vec![window("5h", 98.0, reset, now)], json!({}), None)
            .await;
        assert_eq!(a.health, Health::Ready);
        assert_eq!(a.quota_cooling, None);
        let a = f
            .snapshot(vec![window("5h", 99.0, reset, now)], json!({}), None)
            .await;
        assert_cooling(&a, HealthReason::Quota, reset, "account");
        drop(second);
        drop(first);
        drop(other);
        assert_eq!(f.runtime.inflight_count(&f.ids[0]), 0);
        assert_eq!(f.runtime.inflight_count(&f.ids[1]), 0);
    }

    #[tokio::test]
    async fn n6_expired_auth_remains_probe_pending_until_success_and_uses_row14() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        // Transient refresh cooling deliberately does not increment auth_failures.
        f.cool(
            0,
            HealthReason::Auth,
            Utc::now() - Duration::seconds(1),
            "account",
        )
        .await;
        let account = f.load(0);
        assert_eq!(account.health, Health::Ready);
        assert_eq!(account.auth_failures, 0);
        assert!(account.auth_probe_pending);
        let views = f.runtime.views().await.unwrap();
        assert_eq!(views[0]["health"]["probe_pending"], true);
        assert_eq!(views[0]["probe_pending"], true);
        let guard = f.runtime.begin_probe(&account).unwrap().unwrap();
        assert!(f.runtime.begin_probe(&account).is_err());
        assert!(!f.runtime.eligible(&account.id, None).await);
        assert_eq!(
            f.runtime
                .unavailable(Provider::Codex, false, None)
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        f.runtime.probe_success(&account.id).await.unwrap();
        drop(guard);
        assert!(!f.load(0).auth_probe_pending);
        assert_eq!(
            f.runtime.views().await.unwrap()[0]["health"]["kind"],
            "ready"
        );
    }

    #[tokio::test]
    async fn c5_runtime_auth_backoff_saturates_and_terminal_state_is_not_resurrected() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        for (index, minutes) in [15, 30, 60, 120, 240, 240].into_iter().enumerate() {
            let before = Utc::now();
            let a = f
                .runtime
                .transition(&f.ids[0], HealthSignal::AuthFailure)
                .await
                .unwrap();
            assert_eq!(a.auth_failures, index as u32 + 1);
            let Health::Cooling {
                reason: HealthReason::Auth,
                until,
                scope,
            } = a.health
            else {
                panic!("expected auth cooling")
            };
            assert_eq!(scope, "account");
            assert!(until >= before + Duration::minutes(minutes));
            assert!(until <= Utc::now() + Duration::minutes(minutes));
            f.runtime
                .transition(&f.ids[0], HealthSignal::Ready)
                .await
                .unwrap();
        }
        f.runtime
            .transition(&f.ids[0], HealthSignal::Revoked("token_revoked".into()))
            .await
            .unwrap();
        f.runtime
            .transition(&f.ids[0], HealthSignal::Ready)
            .await
            .unwrap();
        assert!(matches!(f.load(0).health, Health::NeedsRelogin { .. }));
    }

    #[tokio::test]
    async fn c5_runtime_reason_transitions_preserve_independent_user_state() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        f.runtime
            .change(&f.ids[0], Some(UserState::Paused), None, None, None)
            .await
            .unwrap();
        for reason in [
            HealthReason::Quota,
            HealthReason::Rate,
            HealthReason::Permission,
            HealthReason::Payment,
            HealthReason::StoreError,
            HealthReason::LinkStale,
        ] {
            f.runtime
                .transition(&f.ids[0], HealthSignal::Ready)
                .await
                .unwrap();
            let until = Utc::now() + Duration::hours(1);
            let a = f.cool(0, reason, until, "account").await;
            assert_cooling(&a, reason, until, "account");
            assert_eq!(a.user_state, UserState::Paused);
        }
    }

    #[tokio::test]
    async fn n8_terminal_row15_uses_normative_message() {
        let f = Fixture::new(&[], 0.0);
        let response = f.runtime.unavailable(Provider::Codex, false, None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let raw = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(
            body["error"]["message"],
            "quota-baton: no usable codex account — open the tray"
        );
    }
    #[tokio::test]
    async fn s3_same_limit_timestamp_jitter_preserves_causing_window_evidence() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        let now = Utc::now();
        let reset = now + Duration::hours(1);
        let cooled = f
            .snapshot(vec![window("5h", 100.0, reset, now)], json!({}), None)
            .await;
        let evidence = cooled.quota_cooling.clone().unwrap();
        f.cool(
            0,
            HealthReason::Quota,
            reset + Duration::milliseconds(20),
            "account",
        )
        .await;
        assert_eq!(f.load(0).quota_cooling, Some(evidence.clone()));
        let fresh = evidence.started_at + Duration::seconds(1);
        f.snapshot(
            vec![window("5h", 20.0, reset + Duration::hours(1), fresh)],
            json!({}),
            None,
        )
        .await;
        assert_eq!(f.load(0).health, Health::Ready);
    }
    #[tokio::test]
    async fn m2_reset_only_headers_preserve_known_active_window_reset() {
        let f = Fixture::new(&[Provider::Codex], 0.0);
        let reset = Utc::now() + Duration::hours(1);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-codex-limit-reached", "true".parse().unwrap());
        headers.insert("x-codex-active-limit", "primary".parse().unwrap());
        headers.insert(
            "x-codex-primary-reset-at",
            reset.timestamp().to_string().parse().unwrap(),
        );
        f.runtime
            .observe_headers(&f.ids[0], Provider::Codex, &headers)
            .await;
        assert!(
            matches!(f.load(0).health,Health::Cooling {until,..} if (until-reset).num_seconds().abs()<=1)
        );
    }
}
