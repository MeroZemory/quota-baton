use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::accounts::{
    Account, AccountStore, Identity, Source, SourceMatch, StoreError, Tokens,
    import::{self, ImportAction, SkipReason},
    linked::{self, LinkError},
    login::{self, LoginStoreError},
};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Mutex};

static SERIAL: Mutex<()> = Mutex::new(());
const EMAIL: &str = "synthetic@example.invalid";

fn isolated(test: impl FnOnce(&AccountStore, &Path, &Path)) {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m1b1-identity-tests");
    fs::create_dir_all(&root).unwrap();
    let root = tempfile::tempdir_in(root).unwrap();
    let home = root.path().join("home");
    let cpa = root.path().join("synthetic-cpa");
    fs::create_dir(&cpa).unwrap();
    let store = AccountStore::acquire(&home).unwrap();
    test(&store, &cpa, &home);
}

fn fixture(user: Option<&str>, email: &str, version: u32) -> Value {
    let id_token = user.map(|user| {
        format!(
            "e30.{}.synthetic-signature",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({
                    "email": email,
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "synthetic-workspace", "chatgpt_user_id": user
                    }
                }))
                .unwrap()
            )
        )
    });
    json!({
        "type": "codex", "account_id": "synthetic-workspace", "email": email,
        "access_token": format!("synthetic-access-{version}"),
        "refresh_token": format!("synthetic-refresh-{version}"), "id_token": id_token,
        "last_refresh": format!("2030-01-0{version}T01:00:00Z"),
        "expired": format!("2030-01-0{version}T02:00:00Z")
    })
}

fn write(cpa: &Path, name: &str, data: &Value) {
    fs::write(cpa.join(name), serde_json::to_vec(data).unwrap()).unwrap();
}

fn imported(store: &AccountStore, cpa: &Path, user: Option<&str>, version: u32) -> Account {
    write(cpa, "account.json", &fixture(user, EMAIL, version));
    import::run(store, cpa, &[], false).unwrap();
    store.list().unwrap().remove(0)
}

fn incoming(base: &Account, user: &str, email: &str) -> (Account, Tokens) {
    let mut account = base.clone();
    account.identity = Identity::Codex {
        account_id: "synthetic-workspace".into(),
        user_id: Some(user.into()),
        email: email.into(),
    };
    account.id = account.identity.id();
    account.email = email.into();
    account.source = Source::Oauth;
    account.generation = 0;
    let tokens = Tokens {
        access_token: "synthetic-oauth-access".into(),
        refresh_token: "synthetic-oauth-refresh".into(),
        id_token: None,
    };
    (account, tokens)
}

#[test]
fn import_distinguishes_workspace_users_even_with_equal_email() {
    isolated(|store, cpa, _| {
        write(cpa, "a.json", &fixture(Some("synthetic-user-a"), EMAIL, 1));
        write(cpa, "b.json", &fixture(Some("synthetic-user-b"), EMAIL, 1));
        let rows = import::run(store, cpa, &[], false).unwrap();
        assert!(rows.iter().all(|row| row.action == ImportAction::Created));
        let accounts = store.list().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_ne!(
            accounts[0].identity.canonical_key(),
            accounts[1].identity.canonical_key()
        );
        assert!(accounts.iter().all(|a| matches!(
            a.identity,
            Identity::Codex {
                user_id: Some(_),
                ..
            }
        )));
    });
}

#[test]
fn same_user_email_change_reimport_keeps_one_id_and_rejects_older_tokens() {
    isolated(|store, cpa, _| {
        let old = imported(store, cpa, Some("synthetic-user-a"), 2);
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), "changed@example.invalid", 3),
        );
        import::run(store, cpa, &[], false).unwrap();
        let updated = store.load(&old.id).unwrap();
        assert_eq!(updated.email, "changed@example.invalid");
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), EMAIL, 1),
        );
        let rows = import::run(store, cpa, &[], false).unwrap();
        assert_eq!(rows[0].reason, Some(SkipReason::OlderToken));
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(
            store
                .tokens(&store.load(&old.id).unwrap())
                .unwrap()
                .access_token
                == "synthetic-access-3"
        );
    });
}

#[test]
fn linked_matching_uses_user_claims_and_preserves_source_and_newer_gates() {
    isolated(|store, cpa, _| {
        let old = imported(store, cpa, Some("synthetic-user-a"), 2);
        write(
            cpa,
            "other.json",
            &fixture(Some("synthetic-user-b"), EMAIL, 4),
        );
        assert!(matches!(linked::reload(&old), Err(LinkError::Stale)));
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), "changed@example.invalid", 3),
        );
        let updated = linked::reload_and_store(store, &old).unwrap();
        assert!(store.tokens(&updated).unwrap().access_token == "synthetic-access-3");
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), EMAIL, 1),
        );
        assert!(matches!(linked::reload(&updated), Err(LinkError::Stale)));
        let mut wrong_source = updated;
        if let Source::Cpa {
            r#match: SourceMatch::Codex { email, .. },
            ..
        } = &mut wrong_source.source
        {
            *email = "wrong@example.invalid".into();
        }
        assert!(matches!(
            linked::reload(&wrong_source),
            Err(LinkError::NotLinked)
        ));
    });
}

#[test]
fn legacy_login_allows_distinct_user_without_copying_older_cpa_tokens() {
    isolated(|store, cpa, _| {
        let old = imported(store, cpa, None, 3);
        // Identity can be resolved even when the current CPA token is older.
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), EMAIL, 1),
        );
        let (account, tokens) = incoming(&old, "synthetic-user-b", "other@example.invalid");
        let saved = login::save(store, &account, &tokens, false).unwrap();
        assert!(!saved.replaced_linked);
        assert_eq!(store.list().unwrap().len(), 2);
        assert!(
            store
                .tokens(&store.load(&old.id).unwrap())
                .unwrap()
                .access_token
                == "synthetic-access-3"
        );
        assert!(matches!(linked::reload(&old), Err(LinkError::Stale)));
    });
}

#[test]
fn legacy_same_user_changed_email_requires_replace_then_migrates_one_family() {
    isolated(|store, cpa, _| {
        let old = imported(store, cpa, None, 1);
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), EMAIL, 1),
        );
        let (account, tokens) = incoming(&old, "synthetic-user-a", "changed@example.invalid");
        assert!(matches!(
            login::save(store, &account, &tokens, false),
            Err(LoginStoreError::LinkedConflict)
        ));
        assert_eq!(store.list().unwrap().len(), 1);
        let saved = login::save(store, &account, &tokens, true).unwrap();
        assert!(saved.replaced_linked);
        assert_eq!(saved.account.email, "changed@example.invalid");
        assert!(matches!(store.load(&old.id), Err(StoreError::NotFound)));
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.tokens(&saved.account).unwrap().access_token == "synthetic-oauth-access");
    });
}

#[test]
fn legacy_reimport_does_not_duplicate_family_or_roll_back_tokens() {
    isolated(|store, cpa, _| {
        let old = imported(store, cpa, None, 3);
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), EMAIL, 1),
        );
        let rows = import::run(store, cpa, &[], false).unwrap();
        assert_eq!(rows[0].id.as_deref(), Some(old.id.as_str()));
        assert_eq!(rows[0].reason, Some(SkipReason::OlderToken));
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(
            store
                .tokens(&store.load(&old.id).unwrap())
                .unwrap()
                .access_token
                == "synthetic-access-3"
        );
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), EMAIL, 4),
        );
        import::run(store, cpa, &[], false).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(
            store
                .tokens(&store.load(&old.id).unwrap())
                .unwrap()
                .access_token
                == "synthetic-access-4"
        );
    });
}

#[test]
fn unresolved_or_ambiguous_legacy_identity_is_refused_even_with_replace() {
    isolated(|store, cpa, _| {
        let old = imported(store, cpa, None, 1);
        let (account, tokens) = incoming(&old, "synthetic-user-a", EMAIL);
        assert!(matches!(
            login::save(store, &account, &tokens, true),
            Err(LoginStoreError::IdentityConflict)
        ));
        assert_eq!(
            LoginStoreError::IdentityConflict.to_string(),
            login::IDENTITY_CONFLICT_REASON
        );
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), EMAIL, 1),
        );
        write(
            cpa,
            "other.json",
            &fixture(Some("synthetic-user-b"), EMAIL, 2),
        );
        assert!(matches!(
            login::save(store, &account, &tokens, true),
            Err(LoginStoreError::IdentityConflict)
        ));
        assert!(matches!(
            import::run(store, cpa, &[], false),
            Err(StoreError::IdentityCollision)
        ));
        assert_eq!(store.list().unwrap().len(), 1);
    });
}

#[test]
fn legacy_login_does_not_bypass_source_match_gate() {
    isolated(|store, cpa, _| {
        let old = imported(store, cpa, None, 1);
        let mut wrong_source = old.clone();
        if let Source::Cpa {
            r#match: SourceMatch::Codex { email, .. },
            ..
        } = &mut wrong_source.source
        {
            *email = "wrong@example.invalid".into();
        }
        store
            .save(
                &wrong_source,
                &store.tokens(&old).unwrap(),
                Some(old.generation),
            )
            .unwrap();
        write(
            cpa,
            "account.json",
            &fixture(Some("synthetic-user-a"), EMAIL, 2),
        );
        let (account, tokens) = incoming(&old, "synthetic-user-a", "changed@example.invalid");
        assert!(matches!(
            login::save(store, &account, &tokens, true),
            Err(LoginStoreError::IdentityConflict)
        ));
        assert_eq!(store.list().unwrap().len(), 1);
    });
}

#[test]
fn mismatched_workspace_claim_cannot_supply_user_id() {
    isolated(|store, cpa, _| {
        let mut data = fixture(Some("synthetic-user-a"), EMAIL, 1);
        data["account_id"] = json!("different-workspace");
        write(cpa, "account.json", &data);
        import::run(store, cpa, &[], false).unwrap();
        assert!(matches!(
            store.list().unwrap()[0].identity,
            Identity::Codex { user_id: None, .. }
        ));
    });
}

#[test]
fn missing_user_claim_on_reimport_cannot_duplicate_known_family() {
    isolated(|store, cpa, _| {
        let old = imported(store, cpa, Some("synthetic-user-a"), 1);
        write(cpa, "account.json", &fixture(None, EMAIL, 2));
        assert!(matches!(
            import::run(store, cpa, &[], false),
            Err(StoreError::IdentityCollision)
        ));
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(
            store
                .tokens(&store.load(&old.id).unwrap())
                .unwrap()
                .access_token
                == "synthetic-access-1"
        );
    });
}

#[test]
fn journal_recovery_finishes_legacy_replacement_after_email_change() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m1b1-identity-tests");
    fs::create_dir_all(&root).unwrap();
    let root = tempfile::tempdir_in(root).unwrap();
    let home = root.path().join("home");
    let cpa = root.path().join("synthetic-cpa");
    fs::create_dir(&cpa).unwrap();
    let store = AccountStore::acquire(&home).unwrap();
    let old = imported(&store, &cpa, None, 1);
    write(
        &cpa,
        "account.json",
        &fixture(Some("synthetic-user-a"), EMAIL, 1),
    );
    let (account, tokens) = incoming(&old, "synthetic-user-a", "changed@example.invalid");
    // Simulate the crash after a resolved replacement was written, before the
    // old legacy record was removed. The lease protects this synthetic setup.
    store.save(&account, &tokens, None).unwrap();
    fs::write(
        home.join("accounts/.login-migration"),
        serde_json::to_vec(&json!({"old": old.id, "new": account.id})).unwrap(),
    )
    .unwrap();
    drop(store);
    let store = AccountStore::acquire(&home).unwrap();
    assert_eq!(store.list().unwrap().len(), 1);
    assert!(matches!(store.load(&old.id), Err(StoreError::NotFound)));
    assert!(
        store
            .tokens(&store.load(&account.id).unwrap())
            .unwrap()
            .access_token
            == "synthetic-oauth-access"
    );
    assert!(!home.join("accounts/.login-migration").exists());
}
