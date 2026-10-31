use super::*;

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
    store
        .update_metadata(&id, |a| {
            a.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: now() + Duration::hours(1),
                scope: "account".into(),
            };
            a.quota_credit_releasable = true;
        })
        .unwrap();
    qb_core::accounts::import::run(&store, &source, &[], false).unwrap();
    assert!(store.load(&id).unwrap().quota_credit_releasable);
}
