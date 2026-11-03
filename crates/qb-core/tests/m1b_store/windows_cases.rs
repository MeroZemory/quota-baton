// Preserve the original live-CPA scenarios; portable stored-proof recovery stays in the parent.
use super::*;

#[test]
fn c6_4_login_save_preserves_payment_counter_and_deadline() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    for linked in [false, true] {
        let (home, store) = fixture();
        let mut old = account(
            if linked { None } else { Some("synthetic-user") },
            "fixture@example.invalid",
        );
        old.user_state = qb_core::accounts::UserState::Active;
        if linked {
            let dir = home.path().join("synthetic-cpa");
            write_cpa(&dir, Some("synthetic-user"));
            old.source = Source::Cpa {
                mode: qb_core::accounts::ImportMode::Linked,
                dir,
                r#match: qb_core::accounts::SourceMatch::from(&old.identity),
            };
        }
        let now = chrono::Utc::now();
        for _ in 0..2 {
            old.apply_health_signal(
                qb_core::accounts::HealthSignal::Cooling {
                    reason: qb_core::accounts::HealthReason::Payment,
                    until: now + chrono::Duration::hours(6),
                    scope: "account".into(),
                },
                now,
            );
        }
        let old = store.save(&old, &tokens(), None).unwrap();
        let deadline = Some(now + chrono::Duration::hours(12));
        assert_eq!(old.payment_failures, 2);
        assert_eq!(old.payment_until, deadline);

        let new = account(Some("synthetic-user"), "fixture@example.invalid");
        assert_eq!(new.payment_failures, 0);
        assert_eq!(new.payment_until, None);
        let mut rotated = tokens();
        rotated.access_token = "synthetic-login-access-new".into();
        let saved = login::save(&store, &new, &rotated, linked).unwrap();
        assert_eq!(saved.replaced_linked, linked);
        assert_eq!(saved.account.source, Source::Oauth);
        assert_eq!(
            saved.account.user_state,
            qb_core::accounts::UserState::Active
        );
        assert_eq!(saved.account.payment_failures, 2);
        assert_eq!(saved.account.payment_until, deadline);
        assert_eq!(saved.account.health, old.health);
        assert!(!saved.account.eligible(None));
        assert!(store.tokens(&saved.account).unwrap().access_token == "synthetic-login-access-new");
        assert_eq!(saved.account.id != old.id, linked);
        let id = saved.account.id;
        drop(store);
        let restarted = AccountStore::acquire(home.path()).unwrap();
        assert_eq!(restarted.list().unwrap().len(), 1);
        let persisted = restarted.load(&id).unwrap();
        assert_eq!(persisted.payment_failures, 2);
        assert_eq!(persisted.payment_until, deadline);
        assert_eq!(persisted.health, old.health);
        assert!(!persisted.eligible(None));
    }
}

#[test]
fn linked_conflict_requires_override_and_migrates_legacy_identity() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store) = fixture();
    let mut old = account(None, "fixture@example.invalid");
    old.source = Source::Cpa {
        mode: qb_core::accounts::ImportMode::Linked,
        dir: home.path().join("synthetic-unused-cpa"),
        r#match: qb_core::accounts::SourceMatch::from(&old.identity),
    };
    if let Source::Cpa { dir, .. } = &old.source {
        write_cpa(dir, Some("synthetic-user"));
    }
    let old = store.save(&old, &tokens(), None).unwrap();
    let new = account(Some("synthetic-user"), "fixture@example.invalid");
    assert!(matches!(
        login::save(&store, &new, &tokens(), false),
        Err(login::LoginStoreError::LinkedConflict)
    ));
    assert_eq!(store.list().unwrap(), vec![old.clone()]);
    let saved = login::save(&store, &new, &tokens(), true).unwrap();
    assert!(saved.replaced_linked);
    assert!(matches!(saved.account.source, Source::Oauth));
    assert_ne!(saved.account.id, old.id);
    assert_eq!(store.list().unwrap(), vec![saved.account]);
    assert!(!home.path().join("accounts/.login-migration").exists());
}
