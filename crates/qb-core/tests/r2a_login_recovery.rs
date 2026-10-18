//! Synthetic store-only login recovery, including retained owned identity keys.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::accounts::{
    Account, AccountStore, Health, Identity, ImportMode, Source, SourceMatch, StoreError, Tokens,
    login,
};
use serde_json::json;
use std::{fs, path::Path, sync::Mutex};

static SERIAL: Mutex<()> = Mutex::new(());

fn tokens(user: &str, workspace: &str) -> Tokens {
    Tokens {
        access_token: "synthetic-recovery-access".into(),
        refresh_token: "synthetic-recovery-refresh".into(),
        id_token: Some(format!(
            "e30.{}.synthetic",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({"https://api.openai.com/auth": {
                    "chatgpt_account_id":workspace,"chatgpt_user_id":user
                }}))
                .unwrap()
            )
        )),
    }
}

#[test]
fn known_users_with_same_email_keep_separate_owned_ids_and_policy() {
    let _serial = SERIAL.lock().unwrap();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r2a-adoption-fixtures");
    fs::create_dir_all(&base).unwrap();
    let home = tempfile::tempdir_in(base).unwrap();
    let store = AccountStore::acquire(home.path()).unwrap();
    let account = |user: &str| -> Account {
        let identity = Identity::Codex {
            account_id: "synthetic-recovery-workspace".into(),
            email: "same@example.invalid".into(),
            user_id: Some(user.into()),
        };
        serde_json::from_value(json!({
            "schema":1,"id":identity.id(),"identity":identity,"provider":"codex",
            "email":"same@example.invalid","label":user,"source":{"kind":"oauth"},
            "user_state":"disabled","health":{"kind":"needs_relogin",
                "reason":"invalid_grant","since":"2026-01-01T00:00:00Z"},
            "auth_failures":3,"auth_probe_pending":true,"generation":0,
            "token_meta":{"last_refresh_at":"2026-01-01T00:00:00Z",
                "expires_at":"2099-01-01T00:00:00Z","refresh_uncertain":false}
        }))
        .unwrap()
    };
    let first = store
        .save(
            &account("user-a"),
            &tokens("user-a", "synthetic-recovery-workspace"),
            None,
        )
        .unwrap();
    let other = store
        .save(
            &account("user-b"),
            &tokens("user-b", "synthetic-recovery-workspace"),
            None,
        )
        .unwrap();
    let other_path = home
        .path()
        .join("accounts")
        .join(format!("{}.json", other.id));
    let other_bytes = fs::read(&other_path).unwrap();
    let mut incoming = account("user-a");
    incoming.label = "must-not-replace-user-label".into();
    let saved = login::save(
        &store,
        &incoming,
        &tokens("user-a", "synthetic-recovery-workspace"),
        false,
    )
    .unwrap()
    .account;
    assert_eq!(saved.id, first.id);
    assert_ne!(saved.id, other.id);
    assert_eq!(saved.identity, first.identity);
    assert_eq!(saved.label, first.label);
    assert_eq!(saved.user_state, first.user_state);
    assert_eq!(saved.health, Health::Ready);
    assert_eq!(saved.auth_failures, 0);
    assert!(!saved.auth_probe_pending);
    assert!(!saved.eligible(None));
    assert_eq!(store.list().unwrap().len(), 2);
    assert_eq!(fs::read(other_path).unwrap(), other_bytes);
    assert!(!home.path().join("accounts/.login-migration").exists());
    drop(store);
}

#[test]
fn adopted_legacy_relogin_uses_stored_user_and_preserves_distinct_workspace_user() {
    let _serial = SERIAL.lock().unwrap();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r2a-adoption-fixtures");
    fs::create_dir_all(&base).unwrap();
    for source_kind in ["adopted", "oauth", "linked", "wrong-workspace", "no-proof"] {
        let home = tempfile::tempdir_in(&base).unwrap();
        let store = AccountStore::acquire(home.path()).unwrap();
        let missing_cpa = home.path().join("synthetic-cpa-gone");
        assert!(!missing_cpa.exists());
        let identity = Identity::Codex {
            account_id: "synthetic-recovery-workspace".into(),
            email: "old@example.invalid".into(),
            user_id: None,
        };
        let mut legacy: Account = serde_json::from_value(json!({
            "schema":1,"id":identity.id(),"provider":"codex","identity":identity,
            "email":"old@example.invalid","label":"preserved label","plan":"plus",
            "source":{"kind":"cpa","mode":"adopted","dir":missing_cpa,
                "match":SourceMatch::from(&identity)},
            "user_state":"paused","health":{"kind":"needs_relogin",
                "reason":"synthetic-terminal","since":"2026-01-01T00:00:00Z"},
            "auth_failures":3,"auth_probe_pending":true,
            "payment_failures":2,"payment_until":"2099-01-01T00:00:00Z",
            "drain":true,"allow_credits":true,"generation":0,
            "token_meta":{"last_refresh_at":"2026-01-01T00:00:00Z",
                "expires_at":"2026-01-02T00:00:00Z","refresh_uncertain":false}
        }))
        .unwrap();
        if source_kind == "oauth" {
            legacy.source = Source::Oauth;
        }
        if source_kind == "linked"
            && let Source::Cpa { mode, .. } = &mut legacy.source
        {
            *mode = ImportMode::Linked;
        }
        let mut current = tokens(
            "proven-user",
            if source_kind == "wrong-workspace" {
                "another-workspace"
            } else {
                "synthetic-recovery-workspace"
            },
        );
        if source_kind == "no-proof" {
            current.id_token = None;
        }
        let old = store.save(&legacy, &current, None).unwrap();
        let old_path = home
            .path()
            .join("accounts")
            .join(format!("{}.json", old.id));
        let before = fs::read(&old_path).unwrap();
        let mut incoming = old.clone();
        incoming.identity = Identity::Codex {
            account_id: "synthetic-recovery-workspace".into(),
            email: "old@example.invalid".into(),
            user_id: Some("wrong-user".into()),
        };
        incoming.id = incoming.identity.id();
        incoming.email = "old@example.invalid".into();
        let wrong_id = incoming.id.clone();
        let wrong_tokens = tokens("wrong-user", "synthetic-recovery-workspace");
        let other_path = home
            .path()
            .join("accounts")
            .join(format!("{wrong_id}.json"));
        let other_before = if matches!(source_kind, "adopted" | "oauth") {
            // Same email/workspace is not the same known user: B is independent.
            let other = login::save(&store, &incoming, &wrong_tokens, false).unwrap();
            assert_eq!(other.account.id, wrong_id);
            assert!(!other.replaced_linked);
            assert_eq!(fs::read(&old_path).unwrap(), before);
            assert_eq!(store.list().unwrap().len(), 2);
            Some(fs::read(&other_path).unwrap())
        } else {
            assert!(matches!(
                login::save(&store, &incoming, &wrong_tokens, false),
                Err(login::LoginStoreError::IdentityConflict)
            ));
            assert_eq!(fs::read(&old_path).unwrap(), before);
            assert!(!other_path.exists());
            None
        };
        assert!(!home.path().join("accounts/.login-migration").exists());
        if let Identity::Codex { user_id, .. } = &mut incoming.identity {
            *user_id = Some("proven-user".into());
        }
        if let Identity::Codex { email, .. } = &mut incoming.identity {
            *email = "changed@example.invalid".into();
        }
        incoming.email = "changed@example.invalid".into();
        incoming.id = incoming.identity.id();
        let new_tokens = tokens("proven-user", "synthetic-recovery-workspace");
        if matches!(source_kind, "linked" | "wrong-workspace" | "no-proof") {
            // Linked must still resolve fresh CPA; owned must have workspace-bound proof.
            assert!(matches!(
                login::save(&store, &incoming, &new_tokens, false),
                Err(login::LoginStoreError::IdentityConflict)
            ));
            assert_eq!(fs::read(&old_path).unwrap(), before);
            assert_eq!(store.list().unwrap().len(), 1);
            continue;
        }
        let binding = home.path().join("state/bindings.json");
        fs::create_dir_all(binding.parent().unwrap()).unwrap();
        let binding_bytes = serde_json::to_vec(&json!({"schema":1,"bindings":[{
            "key":"codex:synthetic-session","account":old.id,
            "last_used":"2026-01-01T00:00:00Z"
        }]}))
        .unwrap();
        fs::write(&binding, &binding_bytes).unwrap();
        let history = home.path().join("synthetic-history.jsonl");
        let history_bytes = format!(
            "{}\n",
            json!({"account":old.id,"event":"synthetic-history"})
        );
        fs::write(&history, &history_bytes).unwrap();
        let saved = login::save(&store, &incoming, &new_tokens, false).unwrap();
        assert!(!saved.replaced_linked);
        assert_eq!(saved.account.id, old.id);
        assert_ne!(saved.account.id, incoming.identity.id());
        assert_eq!(saved.account.identity, old.identity);
        assert_eq!(saved.account.email, incoming.email);
        assert_eq!(saved.account.generation, old.generation + 1);
        assert!(matches!(saved.account.health, Health::Cooling {
            reason: qb_core::accounts::HealthReason::Payment, until, ..
        } if Some(until) == old.payment_until));
        assert_eq!(saved.account.auth_failures, 0);
        assert!(!saved.account.auth_probe_pending);
        // Recovered auth must not undo paused/drain/payment selection policy.
        assert!(!saved.account.eligible(None));
        assert_eq!(saved.account.source, Source::Oauth);
        assert_eq!(saved.account.user_state, old.user_state);
        assert_eq!(saved.account.label, old.label);
        assert_eq!(saved.account.payment_failures, old.payment_failures);
        assert_eq!(saved.account.payment_until, old.payment_until);
        assert_eq!(saved.account.drain, old.drain);
        assert_eq!(saved.account.allow_credits, old.allow_credits);
        assert!(old_path.exists());
        assert!(
            !home
                .path()
                .join("accounts")
                .join(format!("{}.json", incoming.id))
                .exists()
        );
        assert!(!home.path().join("accounts/.login-migration").exists());
        assert!(!missing_cpa.exists());
        let accounts = store.list().unwrap();
        assert_eq!(accounts.len(), 2); // One family per proven user, no legacy duplicate.
        assert_eq!(
            accounts.iter().filter(|a| a.id == saved.account.id).count(),
            1
        );
        assert_eq!(accounts.iter().filter(|a| a.id == wrong_id).count(), 1);
        assert_eq!(
            &fs::read(&other_path).unwrap(),
            other_before.as_ref().unwrap()
        );
        assert_eq!(
            store.tokens(&saved.account).unwrap().id_token,
            new_tokens.id_token
        );
        let repeated = login::save(&store, &incoming, &new_tokens, false)
            .unwrap()
            .account;
        assert_eq!(repeated.id, old.id);
        assert_eq!(repeated.identity, old.identity);
        assert_eq!(repeated.generation, saved.account.generation + 1);
        assert_eq!(fs::read(&binding).unwrap(), binding_bytes);
        assert_eq!(fs::read_to_string(&history).unwrap(), history_bytes);
        let row: serde_json::Value = serde_json::from_slice(&fs::read(&binding).unwrap()).unwrap();
        assert_eq!(
            store
                .load(row["bindings"][0]["account"].as_str().unwrap())
                .unwrap()
                .id,
            old.id
        );
        let row: serde_json::Value = serde_json::from_str(history_bytes.trim()).unwrap();
        assert_eq!(
            store.load(row["account"].as_str().unwrap()).unwrap().id,
            old.id
        );
        drop(store); // Release the Windows lease before deleting the synthetic home.
        let store = AccountStore::acquire(home.path()).unwrap();
        let reopened = store.load(&old.id).unwrap();
        assert_eq!(reopened, repeated);
        let edited = store
            .update_metadata(&old.id, |a| a.label = "after-reopen".into())
            .unwrap();
        assert_eq!(edited.generation, reopened.generation + 1);
        assert_eq!(edited.identity, old.identity);
        assert!(matches!(
            store.save(&reopened, &new_tokens, Some(reopened.generation)),
            Err(StoreError::GenerationConflict { .. })
        ));
        let mut refreshed_tokens = tokens("proven-user", "synthetic-recovery-workspace");
        refreshed_tokens.access_token = "synthetic-next-access".into();
        let refreshed = store
            .save(&edited, &refreshed_tokens, Some(edited.generation))
            .unwrap();
        assert_eq!(refreshed.id, old.id);
        assert_eq!(refreshed.identity, old.identity);
        assert_eq!(
            store.tokens(&refreshed).unwrap().access_token,
            "synthetic-next-access"
        );
        let repeated = login::save(&store, &incoming, &new_tokens, false)
            .unwrap()
            .account;
        assert_eq!(repeated.id, old.id);
        assert_eq!(repeated.label, "after-reopen");
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(
            &fs::read(&other_path).unwrap(),
            other_before.as_ref().unwrap()
        );
        assert_eq!(fs::read(binding).unwrap(), binding_bytes);
        assert_eq!(fs::read_to_string(history).unwrap(), history_bytes);
        drop(store);
    }
}
