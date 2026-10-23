//! Actual synthetic Store persistence; no deferred-resume activation claim.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use qb_core::accounts::{
    Account, AccountStore, Health, HealthReason, Identity, ImportMode, Source, SourceMatch,
    StoreError, Tokens, UserState, linked,
};
use serde_json::json;
use std::{fs, sync::Mutex, time::Instant};

static SERIAL: Mutex<()> = Mutex::new(());

fn fixture() -> (tempfile::TempDir, AccountStore, Account) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.sandbox/n5d-credential-continuity-tests");
    fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(root).unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let store = loop {
        match AccountStore::acquire(home.path()) {
            Ok(store) => break store,
            Err(StoreError::LeaseBusy) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("synthetic credential store: {error}"),
        }
    };
    let identity = Identity::Codex {
        account_id: "synthetic-continuity-account".into(),
        email: "continuity@example.invalid".into(),
        user_id: Some("synthetic-continuity-user".into()),
    };
    let account: Account = serde_json::from_value(json!({
        "schema":1,"id":identity.id(),"identity":identity,"provider":"codex",
        "label":"synthetic-continuity","email":"continuity@example.invalid",
        "source":{"kind":"oauth"},"user_state":"active",
        "health":{"kind":"ready"},"generation":0,
        "token_meta":{"last_refresh_at":"2026-01-01T00:00:00Z",
                      "expires_at":"2099-01-01T00:00:00Z","refresh_uncertain":false}
    }))
    .unwrap();
    let saved = store.save(&account, &tokens("initial"), None).unwrap();
    (home, store, saved)
}

fn tokens(suffix: &str) -> Tokens {
    Tokens {
        access_token: format!("synthetic-continuity-access-{suffix}"),
        refresh_token: format!("synthetic-continuity-refresh-{suffix}"),
        id_token: None,
    }
}

#[test]
fn n5d_credential_writeahead_restore_rotation_preserve_metadata_and_persist() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store, before) = fixture();
    let mut pending = before.clone();
    pending.token_meta.refresh_uncertain = true;
    let pending = store
        .save_credentials(&pending, &tokens("initial"), before.generation)
        .unwrap();
    assert!(pending.token_meta.refresh_uncertain);
    assert_eq!(pending.generation, before.generation + 1);
    // A not-sent refresh can restore uncertainty through the same narrow API.
    let mut restored = pending.clone();
    restored.token_meta.refresh_uncertain = false;
    let restored = store
        .save_credentials(&restored, &tokens("initial"), pending.generation)
        .unwrap();
    let mut rotated = restored.clone();
    rotated.token_meta.last_refresh_at = Some(Utc::now());
    let rotated = store
        .save_credentials(&rotated, &tokens("rotated"), restored.generation)
        .unwrap();
    let mut expected = before;
    expected.token_meta = rotated.token_meta.clone();
    expected.generation = rotated.generation;
    assert_eq!(rotated, expected);
    assert!(store.tokens(&rotated).unwrap().access_token == tokens("rotated").access_token);
    drop(store);
    let reopened = AccountStore::acquire(home.path()).unwrap();
    let durable = reopened.load(&rotated.id).unwrap();
    assert_eq!(durable, rotated);
    assert!(reopened.tokens(&durable).unwrap().refresh_token == tokens("rotated").refresh_token);
}

#[test]
fn n5d_credential_save_rejects_metadata_edits_without_persisting_tokens() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (_home, store, before) = fixture();
    let mut cases = Vec::new();
    let mut changed = before.clone();
    changed.drain = true;
    cases.push(changed);
    let mut changed = before.clone();
    changed.allow_credits = true;
    cases.push(changed);
    let mut changed = before.clone();
    changed.user_state = UserState::Paused;
    cases.push(changed);
    let mut changed = before.clone();
    changed.source = Source::Takeover {
        from: "synthetic-source".into(),
        at: Utc::now(),
    };
    cases.push(changed);
    let mut changed = before.clone();
    changed.identity = Identity::Codex {
        account_id: "synthetic-other-account".into(),
        email: "other@example.invalid".into(),
        user_id: Some("synthetic-other-user".into()),
    };
    cases.push(changed);
    let mut changed = before.clone();
    changed.label = "synthetic-other-label".into();
    cases.push(changed);
    for reason in [HealthReason::Auth, HealthReason::Payment] {
        let mut changed = before.clone();
        changed.health = Health::Cooling {
            reason,
            until: Utc::now() + Duration::hours(1),
            scope: "account".into(),
        };
        cases.push(changed);
    }
    let mut changed = before.clone();
    changed.auth_failures = 1;
    cases.push(changed);
    let mut changed = before.clone();
    changed.payment_until = Some(Utc::now() + Duration::hours(1));
    cases.push(changed);
    for changed in cases {
        assert!(
            store
                .save_credentials(&changed, &tokens("forbidden"), before.generation)
                .is_err()
        );
        let durable = store.load(&before.id).unwrap();
        assert_eq!(durable, before);
        assert!(store.tokens(&durable).unwrap().access_token == tokens("initial").access_token);
    }
}

#[test]
fn n5d_credential_save_preserves_generation_older_token_and_stop_errors() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (_home, store, before) = fixture();
    assert!(matches!(
        store.save_credentials(&before, &tokens("stale"), before.generation + 1),
        Err(StoreError::GenerationConflict { .. })
    ));
    let mut older = before.clone();
    older.token_meta.last_refresh_at = Some(
        chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    );
    assert!(matches!(
        store.save_credentials(&older, &tokens("older"), before.generation),
        Err(StoreError::OlderToken)
    ));
    assert_eq!(store.load(&before.id).unwrap(), before);
    store.stop_writes();
    assert!(matches!(
        store.save_credentials(&before, &tokens("stopped"), before.generation),
        Err(StoreError::LeaseUnavailable)
    ));
    assert!(store.tokens(&before).unwrap().access_token == tokens("initial").access_token);
}

#[test]
fn n5d_credential_only_store_error_recovery_requires_new_certain_metadata() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (_home, store, before) = fixture();
    let mut failed = before.clone();
    failed.health = Health::Cooling {
        reason: HealthReason::StoreError,
        until: Utc::now() + Duration::hours(1),
        scope: "account".into(),
    };
    let failed = store
        .save(&failed, &tokens("initial"), Some(before.generation))
        .unwrap();
    let mut recovered = failed.clone();
    recovered.health = Health::Ready;
    assert!(matches!(
        store.save_credentials(&recovered, &tokens("rotated"), failed.generation),
        Err(StoreError::InvalidData)
    ));
    recovered.token_meta.last_refresh_at = Some(Utc::now());
    let saved = store
        .save_credentials(&recovered, &tokens("rotated"), failed.generation)
        .unwrap();
    assert_eq!(saved.health, Health::Ready);
    assert_eq!(saved.auth_failures, failed.auth_failures);
    assert_eq!(saved.payment_until, failed.payment_until);
}

#[test]
fn n5d_credential_normalized_health_change_requires_full_save() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (_home, store, before) = fixture();
    let mut held = before.clone();
    held.health = Health::NeedsRelogin {
        reason: "synthetic-relogin".into(),
        since: Utc::now(),
    };
    let held = store
        .save(&held, &tokens("initial"), Some(before.generation))
        .unwrap();
    let mut rotated = held.clone();
    rotated.token_meta.last_refresh_at = Some(Utc::now());
    assert!(matches!(
        store.save_credentials(&rotated, &tokens("rotated"), held.generation),
        Err(StoreError::CredentialHealthChange)
    ));
    assert_eq!(store.load(&held.id).unwrap(), held);
    assert!(store.tokens(&held).unwrap().access_token == tokens("initial").access_token);
    let saved = store
        .save(&rotated, &tokens("rotated"), Some(held.generation))
        .unwrap();
    assert_eq!(
        saved.health,
        Health::Ready,
        "ordinary save retains native linked-token recovery"
    );
}

#[test]
fn n5d_linked_reload_keeps_source_settings_and_native_health_recovery() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let (home, store, before) = fixture();
    let dir = home.path().join("synthetic-linked-source");
    fs::create_dir_all(&dir).unwrap();
    let id_token = format!(
        "e30.{}.synthetic-signature",
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "email":"continuity@example.invalid",
                "https://api.openai.com/auth":{
                    "chatgpt_account_id":"synthetic-continuity-account",
                    "chatgpt_user_id":"synthetic-continuity-user"
                }
            }))
            .unwrap()
        )
    );
    fs::write(dir.join("account.json"), serde_json::to_vec(&json!({
        "type":"codex","account_id":"synthetic-continuity-account",
        "email":"continuity@example.invalid","id_token":id_token,
        "access_token":"synthetic-linked-new-access","refresh_token":"synthetic-linked-new-refresh",
        "last_refresh":"2026-02-01T00:00:00Z","expired":"2099-01-01T00:00:00Z"
    })).unwrap()).unwrap();
    let mut linked_account = before.clone();
    linked_account.source = Source::Cpa {
        mode: ImportMode::Linked,
        dir: dir.clone(),
        r#match: SourceMatch::from(&before.identity),
    };
    linked_account.drain = true;
    linked_account.allow_credits = true;
    let linked_account = store
        .save(&linked_account, &tokens("initial"), Some(before.generation))
        .unwrap();
    let reloaded = linked::reload_and_store(&store, &linked_account).unwrap();
    assert_eq!(reloaded.source, linked_account.source);
    assert_eq!(reloaded.identity, linked_account.identity);
    assert!(reloaded.drain && reloaded.allow_credits);
    assert!(store.tokens(&reloaded).unwrap().access_token == "synthetic-linked-new-access");

    let mut stale = reloaded.clone();
    stale.health = Health::Cooling {
        reason: HealthReason::LinkStale,
        until: Utc::now() + Duration::hours(1),
        scope: "account".into(),
    };
    let stale = store
        .save(&stale, &tokens("initial"), Some(reloaded.generation))
        .unwrap();
    // A genuinely newer candidate requires native health recovery and therefore
    // must retain the ordinary FullSave path rather than fail the linked reload.
    let path = dir.join("account.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["last_refresh"] = json!("2026-03-01T00:00:00Z");
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    let recovered = linked::reload_and_store(&store, &stale).unwrap();
    assert_eq!(recovered.health, Health::Ready);
    assert_eq!(recovered.source, stale.source);
    assert!(recovered.drain && recovered.allow_credits);
}
