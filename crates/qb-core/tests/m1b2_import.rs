use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::accounts::{
    Account, AccountStore, Health, ImportMode, Source, Tokens, UserState,
    import::{self, ImportAction},
    login,
};
use serde_json::{Value, json};
use std::{fs, path::Path};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const EMAIL: &str = "synthetic@example.invalid";

fn isolated(test: impl FnOnce(&AccountStore, &Path, &Path)) {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m1b2-import-tests");
    fs::create_dir_all(&root).unwrap();
    let root = tempfile::tempdir_in(root).unwrap();
    let home = root.path().join("home");
    let cpa = root.path().join("synthetic-cpa");
    fs::create_dir(&cpa).unwrap();
    let store = AccountStore::acquire(&home).unwrap();
    test(&store, &cpa, &home);
}

fn fixture(provider: &str, user: Option<&str>, version: u32) -> Value {
    let id_token = user.map(|user| {
        format!(
            "e30.{}.synthetic-signature",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "synthetic-workspace", "chatgpt_user_id": user
                    }
                }))
                .unwrap()
            )
        )
    });
    json!({
        "type": provider, "email": EMAIL,
        "account_uuid": "synthetic-account", "organization_uuid": "synthetic-org",
        "account_id": "synthetic-workspace", "id_token": id_token,
        "access_token": format!("synthetic-cpa-access-{version}"),
        "refresh_token": format!("synthetic-cpa-refresh-{version}"),
        "last_refresh": format!("2030-01-0{version}T01:00:00Z"),
        "expired": format!("2030-01-0{version}T02:00:00Z")
    })
}

fn write(cpa: &Path, name: &str, value: &Value) {
    fs::write(cpa.join(name), serde_json::to_vec(value).unwrap()).unwrap();
}

fn imported(store: &AccountStore, cpa: &Path, provider: &str, user: Option<&str>) -> Account {
    write(cpa, "account.json", &fixture(provider, user, 1));
    import::run(store, cpa, &[], false).unwrap();
    store.list().unwrap().remove(0)
}

fn owned_tokens() -> Tokens {
    Tokens {
        access_token: "synthetic-owned-access".into(),
        refresh_token: "synthetic-owned-refresh".into(),
        id_token: Some("synthetic-owned-id".into()),
    }
}

fn assert_owned_tokens(store: &AccountStore, account: &Account) {
    let tokens = store.tokens(account).unwrap();
    assert!(tokens.access_token == "synthetic-owned-access");
    assert!(tokens.refresh_token == "synthetic-owned-refresh");
    assert!(tokens.id_token.as_deref() == Some("synthetic-owned-id"));
}

fn assert_owned_row(rows: &[import::ImportRow], account: &Account) {
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.as_deref(), Some(account.id.as_str()));
    assert_eq!(rows[0].action, ImportAction::Skipped);
    // Keep these tests compilable against the pre-fix enum for the red run.
    assert_eq!(
        serde_json::to_value(rows[0].reason).unwrap(),
        json!("owned_account")
    );
}

fn protected_import(provider: &str, adopted: bool, legacy: bool) {
    isolated(|store, cpa, home| {
        let mut account = imported(store, cpa, provider, (!legacy).then_some("synthetic-user"));
        if adopted {
            if let Source::Cpa { mode, .. } = &mut account.source {
                *mode = ImportMode::Adopted;
            }
        } else {
            account.source = Source::Oauth;
        }
        account.health = Health::NeedsRelogin {
            reason: "synthetic-revocation".into(),
            since: "2030-01-01T00:00:00Z".parse().unwrap(),
        };
        account.auth_failures = 2;
        account.auth_probe_pending = true;
        account.token_meta.refresh_uncertain = true;
        account.label = "Synthetic owned label".into();
        account.drain = true;
        account.allow_credits = true;
        let before = store
            .save(&account, &owned_tokens(), Some(account.generation))
            .unwrap();
        write(
            cpa,
            "account.json",
            &fixture(provider, Some("synthetic-user"), 3),
        );
        assert_owned_row(&import::preview(home, cpa, &[]).unwrap(), &before);
        assert_eq!(store.load(&before.id).unwrap(), before);
        assert_owned_row(&import::run(store, cpa, &[], false).unwrap(), &before);
        assert_eq!(store.list().unwrap(), vec![before.clone()]);
        assert_owned_tokens(store, &before);

        let activate = vec![format!("{provider}:{EMAIL}")];
        let preview = import::preview(home, cpa, &activate).unwrap();
        assert_owned_row(&preview, &before);
        assert_eq!(preview[0].user_state, Some(UserState::Active));
        assert_eq!(store.load(&before.id).unwrap(), before);
        assert_owned_row(&import::run(store, cpa, &activate, false).unwrap(), &before);
        let mut expected = before;
        expected.user_state = UserState::Active;
        expected.generation += 1;
        assert_eq!(store.load(&expected.id).unwrap(), expected);
        assert_owned_tokens(store, &expected);
    });
}

#[test]
fn newer_cpa_preserves_oauth_claude() {
    protected_import("claude", false, false);
}

#[test]
fn newer_cpa_preserves_oauth_codex() {
    protected_import("codex", false, false);
}

#[test]
fn newer_cpa_preserves_adopted_claude() {
    protected_import("claude", true, false);
}

#[test]
fn newer_cpa_preserves_adopted_codex() {
    protected_import("codex", true, false);
}

#[test]
fn resolved_cpa_preserves_legacy_oauth_codex() {
    protected_import("codex", false, true);
}

#[test]
fn resolved_cpa_preserves_legacy_adopted_codex() {
    protected_import("codex", true, true);
}

fn replace_then_reimport(provider: &str) {
    isolated(|store, cpa, _| {
        let linked = imported(store, cpa, provider, Some("synthetic-user"));
        let mut incoming = linked.clone();
        incoming.source = Source::Oauth;
        let saved = login::save(store, &incoming, &owned_tokens(), true).unwrap();
        assert!(saved.replaced_linked);
        assert_eq!(saved.account.source, Source::Oauth);
        write(
            cpa,
            "account.json",
            &fixture(provider, Some("synthetic-user"), 3),
        );
        assert_owned_row(
            &import::run(store, cpa, &[], false).unwrap(),
            &saved.account,
        );
        assert_eq!(store.list().unwrap(), vec![saved.account.clone()]);
        assert_owned_tokens(store, &saved.account);
    });
}

#[test]
fn replace_linked_then_reimport_preserves_claude_login() {
    replace_then_reimport("claude");
}

#[test]
fn replace_linked_then_reimport_preserves_codex_login() {
    replace_then_reimport("codex");
}

fn identity_error_continues(kind: &str) {
    isolated(|store, cpa, home| {
        let mut old = imported(
            store,
            cpa,
            "codex",
            (kind == "missing_claim").then_some("synthetic-user"),
        );
        fs::remove_file(cpa.join("account.json")).unwrap();
        let mut bad = fixture("codex", Some("synthetic-user"), 3);
        match kind {
            "missing_claim" => bad["id_token"] = Value::Null,
            "missing_source" => bad["email"] = json!("changed@example.invalid"),
            "ambiguous_source" => {
                write(
                    cpa,
                    "n-other.json",
                    &fixture("codex", Some("synthetic-other-user"), 3),
                );
            }
            "unresolved_source" => {
                let unresolved = home.parent().unwrap().join("synthetic-unresolved-source");
                fs::create_dir(&unresolved).unwrap();
                write(&unresolved, "account.json", &fixture("codex", None, 1));
                let tokens = store.tokens(&old).unwrap();
                if let Source::Cpa { dir, .. } = &mut old.source {
                    *dir = unresolved;
                }
                old = store.save(&old, &tokens, Some(old.generation)).unwrap();
                bad["email"] = json!("changed@example.invalid");
            }
            _ => unreachable!(),
        }
        // Both sides of the rejected file must be processed.
        let mut before = fixture("claude", None, 2);
        before["account_uuid"] = json!("synthetic-before");
        let mut after = fixture("claude", None, 2);
        after["account_uuid"] = json!("synthetic-after");
        write(cpa, "a-valid.json", &before);
        write(cpa, "m-bad.json", &bad);
        write(cpa, "z-valid.json", &after);
        for rows in [
            import::preview(home, cpa, &[]).unwrap(),
            import::run(store, cpa, &[], false).unwrap(),
        ] {
            assert_eq!(rows.first().unwrap().action, ImportAction::Created);
            assert_eq!(rows[1].action, ImportAction::Skipped);
            assert_eq!(
                serde_json::to_value(rows[1].reason).unwrap(),
                json!("invalid_data")
            );
            assert_eq!(rows.last().unwrap().action, ImportAction::Created);
        }
        assert_eq!(store.load(&old.id).unwrap(), old);
        assert!(store.tokens(&old).unwrap().access_token == "synthetic-cpa-access-1");
        assert_eq!(store.list().unwrap().len(), 3);
    });
}

#[test]
fn missing_user_claim_skips_file_and_imports_neighbors() {
    identity_error_continues("missing_claim");
}

#[test]
fn missing_legacy_source_skips_file_and_imports_neighbors() {
    identity_error_continues("missing_source");
}

#[test]
fn ambiguous_legacy_source_skips_file_and_imports_neighbors() {
    identity_error_continues("ambiguous_source");
}

#[test]
fn unresolved_legacy_source_skips_file_and_imports_neighbors() {
    identity_error_continues("unresolved_source");
}
