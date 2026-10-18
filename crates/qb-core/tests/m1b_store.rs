use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::accounts::{Account, AccountStore, Health, Identity, Source, Tokens, login};
use serde_json::json;
use std::{
    fs,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());

fn account(user: Option<&str>, email: &str) -> Account {
    let identity = Identity::Codex {
        account_id: "synthetic-login-account".into(),
        email: email.into(),
        user_id: user.map(str::to_owned),
    };
    serde_json::from_value(json!({"schema":1,"id":identity.id(),"identity":identity,
        "provider":"codex","label":"사용자 이름","email":email,"source":{"kind":"oauth"},
        "user_state":"paused","health":{"kind":"ready"},"generation":0,
        "token_meta":{"last_refresh_at":"2026-01-01T00:00:00Z","expires_at":"2099-01-01T00:00:00Z","refresh_uncertain":false}})).unwrap()
}
fn tokens() -> Tokens {
    Tokens {
        access_token: "synthetic-login-access".into(),
        refresh_token: "synthetic-login-refresh".into(),
        id_token: None,
    }
}
fn write_cpa(dir: &Path, user: Option<&str>) {
    fs::create_dir_all(dir).unwrap();
    let id_token = user.map(|user| {
        format!(
            "e30.{}.synthetic-signature",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({
                    "email": "fixture@example.invalid",
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "synthetic-login-account", "chatgpt_user_id": user
                    }
                }))
                .unwrap()
            )
        )
    });
    fs::write(
        dir.join("account.json"),
        serde_json::to_vec(&json!({
            "type": "codex", "account_id": "synthetic-login-account",
            "email": "fixture@example.invalid", "id_token": id_token,
            "access_token": "synthetic-cpa-access", "refresh_token": "synthetic-cpa-refresh",
            "last_refresh": "2026-01-01T00:00:00Z", "expired": "2099-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
}
fn fixture() -> (tempfile::TempDir, AccountStore) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m1b-store");
    fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(root).unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    let store = loop {
        match AccountStore::acquire(home.path()) {
            Ok(store) => break store,
            Err(qb_core::accounts::StoreError::LeaseBusy) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => panic!("synthetic login lease: {error}"),
        }
    };
    (home, store)
}
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
fn owned_identity_uses_account_and_user_across_email_change() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (_home, store) = fixture();
    let old = account(Some("synthetic-user"), "old@example.invalid");
    let old = store.save(&old, &tokens(), None).unwrap();
    store
        .update_metadata(&old.id, |a| {
            a.health = Health::NeedsRelogin {
                reason: "invalid_grant".into(),
                since: chrono::Utc::now(),
            };
            a.drain = true;
        })
        .unwrap();
    let new = account(Some("synthetic-user"), "new@example.invalid");
    let saved = login::save(&store, &new, &tokens(), false).unwrap();
    assert_eq!(saved.account.id, old.id);
    assert_eq!(saved.account.health, Health::Ready);
    assert!(saved.account.drain);
    assert_eq!(saved.account.user_state, old.user_state);
    assert_eq!(store.list().unwrap().len(), 1);
    assert_ne!(
        account(Some("other-user"), "new@example.invalid").id,
        new.id
    );
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
#[test]
fn interrupted_identity_migration_recovers_under_lease() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store) = fixture();
    let old = account(None, "fixture@example.invalid");
    store.save(&old, &tokens(), None).unwrap();
    let new = account(Some("synthetic-user"), "fixture@example.invalid");
    store.save(&new, &tokens(), None).unwrap();
    fs::write(
        home.path().join("accounts/.login-migration"),
        json!({"old":old.id,"new":new.id}).to_string(),
    )
    .unwrap();
    drop(store);
    let restarted = AccountStore::acquire(home.path()).unwrap();
    assert_eq!(restarted.list().unwrap().len(), 1);
    assert_eq!(restarted.list().unwrap()[0].id, new.id);
    assert!(!home.path().join("accounts/.login-migration").exists());
}

#[test]
fn migration_before_new_write_retains_old_and_invalid_marker_refuses_start() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store) = fixture();
    let old = account(None, "fixture@example.invalid");
    let old = store.save(&old, &tokens(), None).unwrap();
    let new = account(Some("synthetic-user"), "fixture@example.invalid");
    let marker = home.path().join("accounts/.login-migration");
    fs::write(&marker, json!({"old":old.id,"new":new.id}).to_string()).unwrap();
    drop(store);
    let restarted = AccountStore::acquire(home.path()).unwrap();
    assert_eq!(restarted.list().unwrap(), vec![old.clone()]);
    assert!(!marker.exists());
    drop(restarted);
    fs::write(
        &marker,
        json!({"old":"../outside","new":new.id}).to_string(),
    )
    .unwrap();
    assert!(matches!(
        AccountStore::acquire(home.path()),
        Err(qb_core::accounts::StoreError::InvalidData)
    ));
    assert!(
        home.path()
            .join("accounts")
            .join(format!("{}.json", old.id))
            .exists()
    );
}

#[test]
fn migration_never_deletes_an_unrelated_identity() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store) = fixture();
    let old = account(None, "fixture@example.invalid");
    store.save(&old, &tokens(), None).unwrap();
    let new = account(Some("synthetic-user"), "unrelated@example.invalid");
    store.save(&new, &tokens(), None).unwrap();
    fs::write(
        home.path().join("accounts/.login-migration"),
        json!({"old":old.id,"new":new.id}).to_string(),
    )
    .unwrap();
    drop(store);
    assert!(matches!(
        AccountStore::acquire(home.path()),
        Err(qb_core::accounts::StoreError::IdentityCollision)
    ));
    for id in [&old.id, &new.id] {
        assert!(
            home.path()
                .join("accounts")
                .join(format!("{id}.json"))
                .exists()
        );
    }
}

#[test]
fn legacy_email_change_cannot_bypass_linked_conflict() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    for claude in [false, true] {
        let (home, store) = fixture();
        let mut old = account(None, "old@example.invalid");
        let mut new = account(Some("synthetic-user"), "new@example.invalid");
        if claude {
            old.identity = Identity::Claude {
                account_uuid: "synthetic-uuid".into(),
                organization_uuid: None,
                email: old.email.clone(),
            };
            new.identity = Identity::Claude {
                account_uuid: "synthetic-uuid".into(),
                organization_uuid: Some("synthetic-org".into()),
                email: new.email.clone(),
            };
            for a in [&mut old, &mut new] {
                a.id = a.identity.id();
                a.provider = a.identity.provider();
            }
        }
        old.source = Source::Cpa {
            mode: qb_core::accounts::ImportMode::Linked,
            dir: home.path().join("synthetic-unused-cpa"),
            r#match: qb_core::accounts::SourceMatch::from(&old.identity),
        };
        let old = store.save(&old, &tokens(), None).unwrap();
        for replace in [false, true] {
            assert!(matches!(
                login::save(&store, &new, &tokens(), replace),
                Err(login::LoginStoreError::IdentityConflict)
            ));
            assert_eq!(store.list().unwrap(), vec![old.clone()]);
        }
    }
}

#[test]
fn corrupt_replacement_credentials_never_remove_legacy_record() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store) = fixture();
    let old = account(None, "fixture@example.invalid");
    store.save(&old, &tokens(), None).unwrap();
    let new = account(Some("synthetic-user"), "fixture@example.invalid");
    store.save(&new, &tokens(), None).unwrap();
    let path = home
        .path()
        .join("accounts")
        .join(format!("{}.json", new.id));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["secret_dpapi_b64"] = json!("invalid-synthetic-ciphertext");
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let marker = home.path().join("accounts/.login-migration");
    fs::write(&marker, json!({"old":old.id,"new":new.id}).to_string()).unwrap();
    drop(store);
    assert!(matches!(
        AccountStore::acquire(home.path()),
        Err(qb_core::accounts::StoreError::Crypto)
    ));
    assert!(
        home.path()
            .join("accounts")
            .join(format!("{}.json", old.id))
            .exists()
    );
    assert!(marker.exists());
}

#[test]
fn unresolved_migration_cannot_be_overwritten_by_another_login() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store) = fixture();
    let old = account(None, "fixture@example.invalid");
    let old = store.save(&old, &tokens(), None).unwrap();
    let new = account(Some("synthetic-user"), "fixture@example.invalid");
    let marker = home.path().join("accounts/.login-migration");
    let journal = json!({"old":old.id,"new":new.id}).to_string();
    fs::write(&marker, &journal).unwrap();
    assert!(matches!(
        login::save(&store, &new, &tokens(), true),
        Err(login::LoginStoreError::Store)
    ));
    assert_eq!(fs::read_to_string(marker).unwrap(), journal);
    assert_eq!(store.list().unwrap(), vec![old]);
}

#[test]
fn old_changed_email_journal_retains_linked_records_without_live_cpa_reads() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    for case in 0..4 {
        let (home, store) = fixture();
        let mut old = account(None, "fixture@example.invalid");
        let dir = home.path().join("synthetic-cpa");
        write_cpa(
            &dir,
            if case == 1 {
                None
            } else if case == 0 {
                Some("other-user")
            } else {
                Some("synthetic-user")
            },
        );
        let mut source_match = qb_core::accounts::SourceMatch::from(&old.identity);
        if case == 2 {
            source_match = qb_core::accounts::SourceMatch::Codex {
                account_id: "synthetic-login-account".into(),
                email: "wrong@example.invalid".into(),
            };
        }
        old.source = Source::Cpa {
            mode: if case == 3 {
                qb_core::accounts::ImportMode::Adopted
            } else {
                qb_core::accounts::ImportMode::Linked
            },
            dir,
            r#match: source_match,
        };
        store.save(&old, &tokens(), None).unwrap();
        let new = account(Some("synthetic-user"), "changed@example.invalid");
        store.save(&new, &tokens(), None).unwrap();
        let marker = home.path().join("accounts/.login-migration");
        fs::write(&marker, json!({"old": old.id, "new": new.id}).to_string()).unwrap();
        drop(store);
        if case == 3 {
            // An adopted family is unrelated to linked migration recovery.
            assert!(matches!(
                AccountStore::acquire(home.path()),
                Err(qb_core::accounts::StoreError::IdentityCollision)
            ));
            assert!(marker.exists());
        } else {
            let reopened = AccountStore::acquire(home.path()).unwrap();
            assert_eq!(reopened.list().unwrap().len(), 2);
            assert_eq!(
                reopened.load(&old.id).unwrap().user_state,
                qb_core::accounts::UserState::Disabled
            );
            assert_eq!(
                reopened
                    .tokens(&reopened.load(&old.id).unwrap())
                    .unwrap()
                    .access_token,
                tokens().access_token
            );
            assert!(!marker.exists());
        }
        for id in [&old.id, &new.id] {
            assert!(
                home.path()
                    .join("accounts")
                    .join(format!("{id}.json"))
                    .exists(),
                "case {case}"
            );
        }
    }
}

#[test]
fn authorized_changed_email_journal_survives_missing_or_rewritten_cpa() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    for missing in [true, false] {
        let (home, store) = fixture();
        let mut old = account(None, "fixture@example.invalid");
        let dir = home.path().join("synthetic-cpa");
        write_cpa(&dir, Some("synthetic-user"));
        old.source = Source::Cpa {
            mode: qb_core::accounts::ImportMode::Linked,
            dir: dir.clone(),
            r#match: qb_core::accounts::SourceMatch::from(&old.identity),
        };
        let old = store.save(&old, &tokens(), None).unwrap();
        let resolved = account(Some("synthetic-user"), "fixture@example.invalid");
        let new = account(Some("synthetic-user"), "changed@example.invalid");
        let new = store.save(&new, &tokens(), None).unwrap();
        fs::write(
            home.path().join("accounts/.login-migration"),
            json!({
                "old": old.id, "new": new.id,
                "proof": {"old_identity": old.identity, "resolved_identity": resolved.identity}
            })
            .to_string(),
        )
        .unwrap();
        if missing {
            fs::remove_dir_all(&dir).unwrap();
        } else {
            write_cpa(&dir, Some("other-user"));
        }
        drop(store);
        let restarted = AccountStore::acquire(home.path()).unwrap();
        assert_eq!(restarted.list().unwrap(), vec![new.clone()]);
        assert_eq!(
            restarted.tokens(&new).unwrap().access_token,
            tokens().access_token
        );
        assert!(!home.path().join("accounts/.login-migration").exists());
    }
}

#[test]
fn migration_proof_cannot_authorize_an_unrelated_stored_user() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store) = fixture();
    let old = account(Some("other-user"), "fixture@example.invalid");
    let old = store.save(&old, &tokens(), None).unwrap();
    let resolved = account(Some("synthetic-user"), "fixture@example.invalid");
    let new = account(Some("synthetic-user"), "changed@example.invalid");
    store.save(&new, &tokens(), None).unwrap();
    fs::write(
        home.path().join("accounts/.login-migration"),
        json!({
            "old":old.id,"new":new.id,"proof":{
                "old_identity":old.identity,"resolved_identity":resolved.identity
            }
        })
        .to_string(),
    )
    .unwrap();
    drop(store);
    assert!(matches!(
        AccountStore::acquire(home.path()),
        Err(qb_core::accounts::StoreError::IdentityCollision)
    ));
    for id in [&old.id, &new.id] {
        assert!(
            home.path()
                .join("accounts")
                .join(format!("{id}.json"))
                .exists()
        );
    }
}
