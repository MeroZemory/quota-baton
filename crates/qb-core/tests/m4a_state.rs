use chrono::{Duration, TimeZone, Utc};
use qb_core::accounts::{
    Account, AccountStore, Health, HealthReason, HealthScope, HealthSignal, StoreError,
    UsageWindow, UserState, health::eligible,
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
    a.apply_health_signal(
        HealthSignal::Permission {
            model: Some("blocked".into()),
            account_level: false,
        },
        now(),
    );
    assert!(!a.eligible_at(None, now()));
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
fn c5_rows_terminal_refresh_revoked_and_relogin() {
    for signal in [
        HealthSignal::TerminalRefresh {
            reason: "invalid_grant".into(),
        },
        HealthSignal::Revoked {
            reason: "token_revoked".into(),
        },
    ] {
        let mut a = account();
        a.user_state = UserState::Paused;
        assert!(a.apply_health_signal(signal, now()));
        assert!(matches!(a.health, Health::NeedsRelogin { since, .. } if since == now()));
        a.apply_health_signal(HealthSignal::Payment, now());
        assert!(matches!(a.health, Health::NeedsRelogin { .. }));
        a.auth_failures = 4;
        a.apply_health_signal(HealthSignal::ReloginCompleted, now());
        assert_eq!(a.health, Health::Ready);
        assert_eq!(a.auth_failures, 0);
        assert_eq!(a.user_state, UserState::Paused);
    }
}

#[test]
fn c5_rows_auth_backoff_stale_token_and_expiry_probe() {
    let mut a = account();
    for minutes in [15, 30, 60, 120, 240, 240] {
        a.apply_health_signal(HealthSignal::AuthAfterRefresh, now());
        assert_cooling(&a, HealthReason::Auth, minutes, "account");
        let unchanged = a.clone();
        assert!(!a.apply_health_signal(HealthSignal::StaleToken401, now()));
        assert_eq!(a, unchanged);
        assert!(
            !a.health
                .expire(now() + Duration::minutes(minutes) - Duration::seconds(1))
        );
        assert!(a.health.expire(now() + Duration::minutes(minutes)));
        assert!(a.eligible_at(None, now() + Duration::minutes(minutes)));
    }
    a.auth_failures = u32::MAX;
    a.apply_health_signal(HealthSignal::AuthAfterRefresh, now());
    assert_cooling(&a, HealthReason::Auth, 240, "account");
    assert_eq!(a.auth_failures, u32::MAX);
}

#[test]
fn c5_rows_quota_rate_permission_payment_store_error_and_no_change() {
    let mut a = account();
    a.apply_health_signal(
        HealthSignal::Quota {
            reset_at: now() + Duration::minutes(20),
            scope: HealthScope::Model("opus".into()),
        },
        now(),
    );
    assert_cooling(&a, HealthReason::Quota, 20, "model:opus");
    for signal in [
        HealthSignal::NoChange,
        HealthSignal::Permission {
            model: None,
            account_level: false,
        },
        HealthSignal::Rate {
            retry_after_secs: 10,
            scope: HealthScope::Account,
        },
    ] {
        assert!(!a.apply_health_signal(signal, now()));
    }
    a.apply_health_signal(
        HealthSignal::Rate {
            retry_after_secs: 60,
            scope: HealthScope::Account,
        },
        now(),
    );
    assert_cooling(&a, HealthReason::Rate, 1, "account");
    a.apply_health_signal(
        HealthSignal::Permission {
            model: Some("opus".into()),
            account_level: false,
        },
        now(),
    );
    assert_cooling(&a, HealthReason::Permission, 60, "model:opus");
    a.apply_health_signal(
        HealthSignal::Permission {
            model: None,
            account_level: true,
        },
        now(),
    );
    assert_cooling(&a, HealthReason::Permission, 360, "account");
    a.apply_health_signal(
        HealthSignal::ModelUnsupported {
            model: "gpt-fixture".into(),
        },
        now(),
    );
    assert_cooling(&a, HealthReason::Permission, 360, "model:gpt-fixture");
    a.apply_health_signal(HealthSignal::Payment, now());
    assert_cooling(&a, HealthReason::Payment, 360, "account");
    a.apply_health_signal(HealthSignal::StoreFailure, now());
    assert_cooling(&a, HealthReason::StoreError, 5, "account");
}

fn window(used: f64, projected: f64, minutes: i64) -> UsageWindow {
    UsageWindow {
        used_pct: used,
        projected_pct: projected,
        reset_at: now() + Duration::minutes(minutes),
        limit_reached: false,
    }
}

#[test]
fn c5_rows_passive_exhaustion_credits_hold_and_early_reset() {
    let mut a = account();
    for (window, credits) in [
        (window(100.0, 100.0, 20), false),
        (window(98.0, 99.0, 20), false),
        (window(1.0, 1.0, 20), true),
    ] {
        a.health = Health::Ready;
        a.apply_health_signal(
            HealthSignal::UsageSnapshot {
                windows: vec![window],
                serving_credits: credits,
            },
            now(),
        );
        assert_cooling(&a, HealthReason::Quota, 20, "account");
    }
    a.apply_health_signal(
        HealthSignal::UsageSnapshot {
            windows: vec![window(1.0, 1.0, 20)],
            serving_credits: false,
        },
        now(),
    );
    assert_cooling(&a, HealthReason::Quota, 20, "account");
    a.apply_health_signal(
        HealthSignal::UsageSnapshot {
            windows: vec![window(1.0, 1.0, 30)],
            serving_credits: false,
        },
        now(),
    );
    assert_eq!(a.health, Health::Ready);
    a.allow_credits = true;
    assert!(!a.apply_health_signal(
        HealthSignal::UsageSnapshot {
            windows: vec![window(98.0, 99.0, 20)],
            serving_credits: true
        },
        now()
    ));
    let mut limited = window(1.0, 1.0, 40);
    limited.limit_reached = true;
    a.apply_health_signal(
        HealthSignal::UsageSnapshot {
            windows: vec![window(100.0, 100.0, 20), limited],
            serving_credits: false,
        },
        now(),
    );
    assert_cooling(&a, HealthReason::Quota, 40, "account");
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
            a.apply_health_signal(
                HealthSignal::TerminalRefresh {
                    reason: "invalid_grant".into(),
                },
                now(),
            );
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
