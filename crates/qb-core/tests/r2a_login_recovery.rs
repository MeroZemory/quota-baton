//! Install as tests/r2a_login_recovery.rs. Synthetic store-only login recovery.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::accounts::{
    Account, AccountStore, Identity, ImportMode, Source, SourceMatch, Tokens, login,
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
            email: "changed@example.invalid".into(),
            user_id: Some("wrong-user".into()),
        };
        incoming.id = incoming.identity.id();
        incoming.email = "changed@example.invalid".into();
        let wrong_id = incoming.id.clone();
        let wrong_tokens = tokens("wrong-user", "synthetic-recovery-workspace");
        let other_path = home
            .path()
            .join("accounts")
            .join(format!("{wrong_id}.json"));
        let other_before = if matches!(source_kind, "adopted" | "oauth") {
            // User B may log in independently; stored proof resolves A, not B.
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
        let saved = login::save(&store, &incoming, &new_tokens, false).unwrap();
        assert!(!saved.replaced_linked);
        assert_ne!(saved.account.id, old.id); // Existing login migration semantics.
        assert_eq!(saved.account.id, incoming.identity.id());
        assert_eq!(saved.account.identity, incoming.identity);
        assert_eq!(saved.account.source, Source::Oauth);
        assert_eq!(saved.account.user_state, old.user_state);
        assert_eq!(saved.account.label, old.label);
        assert_eq!(saved.account.payment_failures, old.payment_failures);
        assert_eq!(saved.account.payment_until, old.payment_until);
        assert_eq!(saved.account.drain, old.drain);
        assert_eq!(saved.account.allow_credits, old.allow_credits);
        assert!(!old_path.exists());
        assert!(!home.path().join("accounts/.login-migration").exists());
        assert!(!missing_cpa.exists());
        let accounts = store.list().unwrap();
        assert_eq!(accounts.len(), 2); // One family per proven user, no legacy duplicate.
        assert_eq!(
            accounts.iter().filter(|a| a.id == saved.account.id).count(),
            1
        );
        assert_eq!(accounts.iter().filter(|a| a.id == wrong_id).count(), 1);
        assert_eq!(fs::read(&other_path).unwrap(), other_before.unwrap());
        assert_eq!(
            store.tokens(&saved.account).unwrap().id_token,
            new_tokens.id_token
        );
        drop(store); // Release the Windows lease before deleting the synthetic home.
    }
}
