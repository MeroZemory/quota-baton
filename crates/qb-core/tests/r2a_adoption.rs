//! Synthetic adoption regressions; never invokes the production process probe.
#![cfg(feature = "testkit")]
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::accounts::{
    Account, AccountStore, Health, HealthReason, Identity, ImportMode, Source, SourceMatch, Tokens,
    adopt::{self, AdoptError, AdoptFrom, AdoptRequest, AdoptSource, AdoptWarning, ProbeState},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

static SERIAL: Mutex<()> = Mutex::new(());
const OLD: &str = "2026-01-01T00:00:00Z";
const NEW: &str = "2026-01-02T00:00:00Z";
const EXPIRY: &str = "2099-01-01T00:00:00Z";

struct Fixture {
    // Store drops before the directory (Windows lease handle).
    store: AccountStore,
    home: tempfile::TempDir,
    cpa: PathBuf,
    account: Account,
    provider: &'static str,
}

fn jwt(user: &str) -> String {
    format!("e30.{}.synthetic", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
        "https://api.openai.com/auth": {"chatgpt_account_id":"synthetic-account", "chatgpt_user_id":user}
    })).unwrap()))
}

fn fixture(provider: &'static str, user: Option<&str>) -> Fixture {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r2a-adoption-fixtures");
    fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(root).unwrap();
    let cpa = home.path().join("synthetic-cpa");
    fs::create_dir_all(&cpa).unwrap();
    let identity = if provider == "claude" {
        Identity::Claude {
            account_uuid: "synthetic-account".into(),
            organization_uuid: Some("synthetic-org".into()),
            email: "fixture@example.invalid".into(),
        }
    } else {
        Identity::Codex {
            account_id: "synthetic-account".into(),
            user_id: user.map(str::to_owned),
            email: "fixture@example.invalid".into(),
        }
    };
    let account: Account = serde_json::from_value(json!({
        "schema":1,"id":identity.id(),"provider":provider,"label":"fixture", "plan":null,
        "email":"fixture@example.invalid","identity":identity,
        "source":{"kind":"cpa","mode":"linked","dir":cpa,"match":SourceMatch::from(&identity)},
        "user_state":"paused","health":{"kind":"ready"},"generation":0,
        "auth_failures":3,"auth_probe_pending":true,"payment_failures":2,"payment_until":EXPIRY,
        "drain":true,"allow_credits":true,
        "token_meta":{"expires_at":EXPIRY,"last_refresh_at":OLD,"refresh_uncertain":false}
    }))
    .unwrap();
    let store = AccountStore::acquire(home.path()).unwrap();
    let tokens = Tokens {
        access_token: "synthetic-store-access".into(),
        refresh_token: "synthetic-store-refresh".into(),
        id_token: user.map(jwt),
    };
    let account = store.save(&account, &tokens, None).unwrap();
    Fixture {
        store,
        home,
        cpa,
        account,
        provider,
    }
}

fn cpa_value(f: &Fixture, refresh: &str, tag: &str, user: Option<&str>) -> Value {
    json!({"type":f.provider,"email":"fixture@example.invalid",
        "account_uuid":"synthetic-account","organization_uuid":"synthetic-org",
        "account_id":"synthetic-account","id_token":user.map(jwt),
        "access_token":format!("synthetic-{tag}-access"),"refresh_token":format!("synthetic-{tag}-refresh"),
        "last_refresh":refresh,"expired":EXPIRY})
}
fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn from_store() -> AdoptFrom {
    AdoptFrom::Source(AdoptSource::Store)
}
fn call(f: &Fixture, from: AdoptFrom, probe: ProbeState) -> Result<adopt::AdoptResult, AdoptError> {
    adopt::run_with_probe(
        &f.store,
        &f.account.id,
        AdoptRequest {
            from,
            expected_generation: f.account.generation,
        },
        || probe,
    )
}
fn record(f: &Fixture) -> Vec<u8> {
    fs::read(
        f.home
            .path()
            .join("accounts")
            .join(format!("{}.json", f.account.id)),
    )
    .unwrap()
}
// Raw metadata avoids load()'s separate expiry/payment transition.
fn saved(f: &Fixture) -> Account {
    serde_json::from_slice(&record(f)).unwrap()
}
fn assert_stable(f: &Fixture, a: &Account) {
    assert_eq!(a.id, f.account.id);
    assert_eq!(a.identity, f.account.identity);
    assert_eq!(a.generation, f.account.generation + 1);
    assert_eq!(a.user_state, f.account.user_state);
    assert_eq!(a.payment_failures, f.account.payment_failures);
    assert_eq!(a.payment_until, f.account.payment_until);
    assert_eq!(a.quota_cooling, f.account.quota_cooling);
    assert_eq!(a.drain, f.account.drain);
    assert_eq!(a.allow_credits, f.account.allow_credits);
    assert_eq!(a.auth_failures, 0);
    assert!(!a.auth_probe_pending);
    let mut source = f.account.source.clone();
    if let Source::Cpa { mode, .. } = &mut source {
        *mode = ImportMode::Adopted;
    }
    assert_eq!(a.source, source);
}

#[test]
fn adopt_from_cpa_file_keeps_id_bindings_and_resets_link_stale() {
    let _serial = SERIAL.lock().unwrap();
    for provider in ["claude", "codex"] {
        let mut f = fixture(provider, None);
        f.account.health = Health::Cooling {
            reason: HealthReason::LinkStale,
            until: EXPIRY.parse().unwrap(),
            scope: "account".into(),
        };
        let tokens = f.store.tokens(&f.account).unwrap();
        f.account = f
            .store
            .save(&f.account, &tokens, Some(f.account.generation))
            .unwrap();
        let binding = f.home.path().join("state/bindings.json");
        fs::create_dir_all(binding.parent().unwrap()).unwrap();
        let bytes = serde_json::to_vec(&json!({"schema":1,"bindings":[{
            "session_key":"synthetic-session","account":f.account.id}]}))
        .unwrap();
        fs::write(&binding, &bytes).unwrap();
        write(
            &f.cpa.join("renamed.json"),
            &cpa_value(&f, NEW, "cpa", None),
        );
        let result = call(&f, AdoptFrom::default(), ProbeState::Stopped).unwrap();
        assert_eq!(result.from, "cpa_file");
        assert_eq!(result.generation, f.account.generation + 1);
        let a = saved(&f);
        assert_stable(&f, &a);
        assert_eq!(a.health, Health::Ready);
        assert_eq!(
            f.store.tokens(&a).unwrap().access_token,
            "synthetic-cpa-access"
        );
        assert_eq!(fs::read(binding).unwrap(), bytes);
        let response = serde_json::to_string(&result).unwrap();
        for secret in [
            "synthetic-cpa-access",
            "synthetic-cpa-refresh",
            "secret_dpapi",
            "id_token",
        ] {
            assert!(!response.contains(secret));
        }
    }
}

#[test]
fn adopt_from_store_when_cpa_file_is_gone() {
    let _serial = SERIAL.lock().unwrap();
    let default_request: AdoptRequest =
        serde_json::from_value(json!({"expected_generation": 1})).unwrap();
    assert!(matches!(
        default_request.from,
        AdoptFrom::Source(AdoptSource::CpaFile)
    ));
    assert!(serde_json::from_value::<AdoptRequest>(json!({"from":"store"})).is_err());
    assert!(
        serde_json::from_value::<AdoptRequest>(json!({"from":"invalid","expected_generation":1}))
            .is_err()
    );
    for provider in ["claude", "codex"] {
        // Preserve unrelated health, including expired health, without load-side writes.
        for health in [
            Health::Ready,
            Health::Cooling {
                reason: HealthReason::Payment,
                until: EXPIRY.parse().unwrap(),
                scope: "account".into(),
            },
            Health::Cooling {
                reason: HealthReason::Quota,
                until: OLD.parse().unwrap(),
                scope: "account".into(),
            },
            Health::NeedsRelogin {
                reason: "synthetic".into(),
                since: OLD.parse().unwrap(),
            },
        ] {
            let mut f = fixture(provider, None);
            f.account.health = health;
            f.account.token_meta.refresh_uncertain = true;
            let tokens = f.store.tokens(&f.account).unwrap();
            f.account = f
                .store
                .save(&f.account, &tokens, Some(f.account.generation))
                .unwrap();
            fs::remove_dir(&f.cpa).unwrap();
            let result = adopt::run_with_probe(
                &f.store,
                &f.account.id,
                AdoptRequest {
                    from: from_store(),
                    expected_generation: f.account.generation,
                },
                || panic!("no loadable file: process probe must not run"),
            )
            .unwrap();
            let a = saved(&f);
            assert_stable(&f, &a);
            assert_eq!(a.health, f.account.health);
            assert_eq!(a.token_meta, f.account.token_meta);
            assert_eq!(result.from, "store");
            assert_eq!(result.last_refresh_at, f.account.token_meta.last_refresh_at);
            assert!(result.warnings.contains(&AdoptWarning::CpaFileMissing));
            assert!(
                result
                    .warnings
                    .contains(&AdoptWarning::StoredTokensMayNeedRelogin)
            );
            assert_eq!(
                f.store.tokens(&a).unwrap().refresh_token,
                "synthetic-store-refresh"
            );
        }
        let f = fixture(provider, None);
        fs::write(f.cpa.join("torn.json"), b"{").unwrap();
        let before = record(&f);
        for from in [
            AdoptFrom::default(),
            from_store(),
            AdoptFrom::Path {
                path: f.home.path().join("unused.json"),
            },
        ] {
            assert!(matches!(
                call(&f, from.clone(), ProbeState::Running),
                Err(AdoptError::CpaRunning)
            ));
            assert!(matches!(
                call(&f, from, ProbeState::Unknown),
                Err(AdoptError::ProbeUnavailable)
            ));
            assert_eq!(record(&f), before);
        }
        let result = call(&f, AdoptFrom::default(), ProbeState::Stopped).unwrap();
        assert_eq!(result.from, "store");
        assert!(result.warnings.contains(&AdoptWarning::CpaFileUnreadable));
        assert!(
            !result
                .warnings
                .contains(&AdoptWarning::StoredTokensMayNeedRelogin)
        );
        // No matching file, all others valid: CPA running must not block urgent store adoption.
        for from in [AdoptFrom::default(), from_store()] {
            let f = fixture(provider, None);
            let mut other = cpa_value(&f, NEW, "unrelated", None);
            other["account_id"] = json!("unrelated-account");
            other["account_uuid"] = json!("unrelated-account");
            other["email"] = json!("unrelated@example.invalid");
            write(&f.cpa.join("other.json"), &other);
            let result = call(&f, from, ProbeState::Running).unwrap();
            assert_eq!(result.from, "store");
            assert!(
                !result
                    .warnings
                    .contains(&AdoptWarning::StoredTokensMayNeedRelogin)
            );
            assert_eq!(
                f.store.tokens(&saved(&f)).unwrap().access_token,
                "synthetic-store-access"
            );
        }
        // Expired stored access is concrete warning evidence even without uncertainty.
        let mut f = fixture(provider, None);
        f.account.token_meta.expires_at = Some(OLD.parse().unwrap());
        // Synthetic initial record adjustment: save's rollback guard correctly refuses older metadata.
        // Use a later last_refresh so the test setup advances rather than rolls back.
        f.account.token_meta.last_refresh_at = Some(NEW.parse().unwrap());
        let tokens = f.store.tokens(&f.account).unwrap();
        f.account = f
            .store
            .save(&f.account, &tokens, Some(f.account.generation))
            .unwrap();
        let result = call(&f, from_store(), ProbeState::Running).unwrap();
        assert!(
            result
                .warnings
                .contains(&AdoptWarning::StoredTokensMayNeedRelogin)
        );
    }
}

#[test]
fn adopt_prefers_newer_backup_copy_over_store() {
    let _serial = SERIAL.lock().unwrap();
    for provider in ["claude", "codex"] {
        // A newer pair must not clear unrelated needs_relogin or uncertain refresh.
        let mut f = fixture(provider, None);
        f.account.health = Health::NeedsRelogin {
            reason: "synthetic-terminal".into(),
            since: OLD.parse().unwrap(),
        };
        f.account.token_meta.refresh_uncertain = true;
        let tokens = f.store.tokens(&f.account).unwrap();
        f.account = f
            .store
            .save(&f.account, &tokens, Some(f.account.generation))
            .unwrap();
        let backup = f.home.path().join("newer.json");
        write(&backup, &cpa_value(&f, NEW, "backup", None));
        call(&f, AdoptFrom::Path { path: backup }, ProbeState::Stopped).unwrap();
        let a = saved(&f);
        assert_stable(&f, &a);
        assert_eq!(a.health, f.account.health);
        assert!(a.token_meta.refresh_uncertain);
        for (stamp, expected) in [(NEW, "backup"), ("2025-01-01T00:00:00Z", "store")] {
            let f = fixture(provider, None);
            let backup = f.home.path().join("synthetic-backup");
            fs::create_dir(&backup).unwrap();
            write(
                &backup.join("copy.json"),
                &cpa_value(&f, stamp, "backup", None),
            );
            let result = call(&f, AdoptFrom::Path { path: backup }, ProbeState::Stopped).unwrap();
            assert_eq!(result.from, expected);
            let a = saved(&f);
            assert_stable(&f, &a);
            assert_eq!(a.health, f.account.health); // newer candidate must not normalize payment
            let tokens = f.store.tokens(&a).unwrap();
            assert_eq!(
                tokens.refresh_token,
                format!("synthetic-{expected}-refresh")
            );
        }
        // Newest last_refresh wins even against a later expiry; expiry breaks ties.
        let f = fixture(provider, None);
        let backup = f.home.path().join("copies");
        fs::create_dir(&backup).unwrap();
        let mut older = cpa_value(&f, OLD, "older", None);
        older["expired"] = json!("2100-01-01T00:00:00Z");
        write(&backup.join("a.json"), &older);
        write(&backup.join("b.json"), &cpa_value(&f, NEW, "newer", None));
        let mut tied = cpa_value(&f, NEW, "tie", None);
        tied["expired"] = json!("2099-01-02T00:00:00Z");
        write(&backup.join("c.json"), &tied);
        call(&f, AdoptFrom::Path { path: backup }, ProbeState::Stopped).unwrap();
        assert_eq!(
            f.store.tokens(&saved(&f)).unwrap().access_token,
            "synthetic-tie-access"
        );
        // Source priority is among usable candidates: stale CPA must not hide newer backup.
        let f = fixture(provider, None);
        write(
            &f.cpa.join("current.json"),
            &cpa_value(&f, "2025-01-01T00:00:00Z", "old", None),
        );
        let backup = f.home.path().join("backup.json");
        write(&backup, &cpa_value(&f, NEW, "backup", None));
        let before = record(&f);
        assert!(matches!(
            call(
                &f,
                AdoptFrom::Path {
                    path: backup.clone()
                },
                ProbeState::Running
            ),
            Err(AdoptError::CpaRunning)
        ));
        assert!(matches!(
            call(
                &f,
                AdoptFrom::Path {
                    path: backup.clone()
                },
                ProbeState::Unknown
            ),
            Err(AdoptError::ProbeUnavailable)
        ));
        assert_eq!(record(&f), before);
        let result = call(&f, AdoptFrom::Path { path: backup }, ProbeState::Stopped).unwrap();
        assert_eq!(result.from, "backup");
        assert!(
            result
                .warnings
                .contains(&AdoptWarning::OlderOrUnorderedCandidateIgnored)
        );
        assert_eq!(
            f.store.tokens(&saved(&f)).unwrap().refresh_token,
            "synthetic-backup-refresh"
        );
        assert_stable(&f, &saved(&f));
        // A usable CPA still wins over a newer explicit backup.
        let f = fixture(provider, None);
        write(
            &f.cpa.join("current.json"),
            &cpa_value(&f, OLD, "cpa", None),
        );
        let backup = f.home.path().join("newer-backup.json");
        write(&backup, &cpa_value(&f, NEW, "backup", None));
        let result = call(&f, AdoptFrom::Path { path: backup }, ProbeState::Stopped).unwrap();
        assert_eq!(result.from, "cpa_file");
        assert_eq!(
            f.store.tokens(&saved(&f)).unwrap().access_token,
            "synthetic-cpa-access"
        );
        // A CPA file missing refresh ordering evidence is also unusable.
        let f = fixture(provider, None);
        let mut unordered = cpa_value(&f, OLD, "unordered", None);
        unordered["last_refresh"] = Value::Null;
        write(&f.cpa.join("unordered.json"), &unordered);
        let backup = f.home.path().join("newer-backup.json");
        write(&backup, &cpa_value(&f, NEW, "backup", None));
        let result = call(&f, AdoptFrom::Path { path: backup }, ProbeState::Stopped).unwrap();
        assert_eq!(result.from, "backup");
        assert_eq!(
            f.store.tokens(&saved(&f)).unwrap().refresh_token,
            "synthetic-backup-refresh"
        );
    }
}

#[test]
fn adopt_refused_while_cpa_can_refresh() {
    let _serial = SERIAL.lock().unwrap();
    for provider in ["claude", "codex"] {
        let stopped = fixture(provider, None);
        write(
            &stopped.cpa.join("current.json"),
            &cpa_value(&stopped, NEW, "cpa", None),
        );
        let before = record(&stopped);
        stopped.store.stop_writes();
        assert!(matches!(
            adopt::run_with_probe(
                &stopped.store,
                &stopped.account.id,
                AdoptRequest {
                    from: AdoptFrom::default(),
                    expected_generation: stopped.account.generation
                },
                || panic!("stopped writes must refuse before ownership probe"),
            ),
            Err(AdoptError::Store(
                qb_core::accounts::StoreError::LeaseUnavailable
            ))
        ));
        assert_eq!(record(&stopped), before);
        let f = fixture(provider, None);
        write(
            &f.cpa.join("current.json"),
            &cpa_value(&f, NEW, "cpa", None),
        );
        let before = record(&f);
        for from in [
            AdoptFrom::default(),
            from_store(),
            AdoptFrom::Path {
                path: f.home.path().join("unused-backup"),
            },
        ] {
            assert!(matches!(
                call(&f, from.clone(), ProbeState::Running),
                Err(AdoptError::CpaRunning)
            ));
            assert!(matches!(
                call(&f, from, ProbeState::Unknown),
                Err(AdoptError::ProbeUnavailable)
            ));
            assert_eq!(record(&f), before);
        }
        assert!(matches!(
            adopt::run_with_probe(
                &f.store,
                &f.account.id,
                AdoptRequest {
                    from: from_store(),
                    expected_generation: 0,
                },
                || panic!("generation refusal precedes probe")
            ),
            Err(AdoptError::Generation { .. })
        ));
        assert_eq!(record(&f), before);
        let mut wrong = cpa_value(&f, NEW, "wrong", None);
        wrong[if provider == "claude" {
            "account_uuid"
        } else {
            "account_id"
        }] = json!("another-account");
        write(&f.cpa.join("current.json"), &wrong);
        assert!(matches!(
            call(&f, from_store(), ProbeState::Stopped),
            Err(AdoptError::IdentityMismatch)
        ));
        assert_eq!(record(&f), before);
        let mut changed = cpa_value(&f, NEW, "changed", None);
        changed["email"] = json!("changed@example.invalid");
        write(&f.cpa.join("current.json"), &changed);
        if provider == "codex" {
            assert!(matches!(
                call(&f, AdoptFrom::default(), ProbeState::Stopped),
                Err(AdoptError::IdentityMismatch)
            ));
            assert_eq!(record(&f), before);
        } else {
            let result = call(&f, AdoptFrom::default(), ProbeState::Stopped).unwrap();
            assert!(result.warnings.contains(&AdoptWarning::EmailUpdated));
            let a = saved(&f);
            assert_stable(&f, &a);
            assert_eq!(a.email, "changed@example.invalid");
        }
        // A different explicit backup cannot supply tokens for this id.
        let f = fixture(provider, None);
        let backup = f.home.path().join("foreign.json");
        wrong["email"] = json!("foreign@example.invalid");
        write(&backup, &wrong);
        let before = record(&f);
        assert!(matches!(
            call(&f, AdoptFrom::Path { path: backup }, ProbeState::Stopped),
            Err(AdoptError::IdentityMismatch)
        ));
        assert_eq!(record(&f), before);
    }
}

#[test]
fn adopt_duplicate_files_same_key_take_newest_and_different_keys_skip() {
    let _serial = SERIAL.lock().unwrap();
    for provider in ["claude", "codex"] {
        let f = fixture(provider, None);
        write(&f.cpa.join("a.json"), &cpa_value(&f, OLD, "older", None));
        write(&f.cpa.join("b.json"), &cpa_value(&f, NEW, "newest", None));
        call(&f, AdoptFrom::default(), ProbeState::Stopped).unwrap();
        assert_eq!(
            f.store.tokens(&saved(&f)).unwrap().access_token,
            "synthetic-newest-access"
        );
    }
    let f = fixture("codex", None);
    write(
        &f.cpa.join("a.json"),
        &cpa_value(&f, OLD, "one", Some("user-one")),
    );
    write(
        &f.cpa.join("b.json"),
        &cpa_value(&f, NEW, "two", Some("user-two")),
    );
    let before = record(&f);
    assert!(matches!(
        call(&f, AdoptFrom::default(), ProbeState::Stopped),
        Err(AdoptError::AmbiguousIdentity)
    ));
    assert_eq!(record(&f), before);
    // Changed-email matching must not bypass canonical ambiguity.
    let mut changed = cpa_value(&f, NEW, "two", Some("user-two"));
    changed["email"] = json!("changed@example.invalid");
    write(&f.cpa.join("b.json"), &changed);
    assert!(matches!(
        call(&f, from_store(), ProbeState::Stopped),
        Err(AdoptError::AmbiguousIdentity | AdoptError::IdentityMismatch)
    ));
    assert_eq!(record(&f), before);
    // Known user mismatch, even when ids match and email changed.
    let f = fixture("codex", Some("user-one"));
    write(&f.cpa.join("wrong-user.json"), &changed);
    let before = record(&f);
    assert!(matches!(
        call(&f, AdoptFrom::default(), ProbeState::Stopped),
        Err(AdoptError::IdentityMismatch)
    ));
    assert_eq!(record(&f), before);
    // Claude org mismatch is a different canonical identity.
    let f = fixture("claude", None);
    let mut wrong = cpa_value(&f, NEW, "wrong-org", None);
    wrong["organization_uuid"] = json!("different-org");
    write(&f.cpa.join("wrong-org.json"), &wrong);
    assert!(matches!(
        call(&f, AdoptFrom::default(), ProbeState::Stopped),
        Err(AdoptError::IdentityMismatch)
    ));
}

#[test]
fn adopt_never_changes_id_when_user_id_resolves() {
    let _serial = SERIAL.lock().unwrap();
    // Legacy persisted identity stays None even when its stored token knows a user;
    // that claim must reject another user's matching-account changed-email file.
    let mut f = fixture("codex", None);
    let tokens = Tokens {
        access_token: "synthetic-store-access".into(),
        refresh_token: "synthetic-store-refresh".into(),
        id_token: Some(jwt("known-user")),
    };
    f.account = f
        .store
        .save(&f.account, &tokens, Some(f.account.generation))
        .unwrap();
    let mut wrong = cpa_value(&f, NEW, "wrong-user", Some("other-user"));
    wrong["email"] = json!("changed@example.invalid");
    write(&f.cpa.join("wrong.json"), &wrong);
    let before = record(&f);
    assert!(matches!(
        call(&f, from_store(), ProbeState::Stopped),
        Err(AdoptError::IdentityMismatch)
    ));
    assert_eq!(record(&f), before);
    for changed_email in [false, true] {
        let f = fixture("codex", None);
        let mut value = cpa_value(&f, NEW, "resolved", Some("synthetic-user"));
        if changed_email {
            value["email"] = json!("changed@example.invalid");
        }
        write(&f.cpa.join("resolved.json"), &value);
        if changed_email {
            let before = record(&f);
            assert!(matches!(
                call(&f, AdoptFrom::default(), ProbeState::Stopped),
                Err(AdoptError::IdentityMismatch)
            ));
            assert_eq!(record(&f), before);
            continue;
        }
        call(&f, AdoptFrom::default(), ProbeState::Stopped).unwrap();
        let a = saved(&f);
        assert_stable(&f, &a);
        assert!(matches!(a.identity, Identity::Codex { user_id: None, .. }));
        assert_eq!(
            f.store.tokens(&a).unwrap().id_token.as_deref(),
            Some(jwt("synthetic-user").as_str())
        );
        assert_ne!(
            a.id,
            Identity::Codex {
                account_id: "synthetic-account".into(),
                email: "fixture@example.invalid".into(),
                user_id: Some("synthetic-user".into())
            }
            .id()
        );
    }
}

#[test]
fn unresolved_codex_never_adopts_another_workspace_members_tokens() {
    let _serial = SERIAL.lock().unwrap();
    for backup in [false, true] {
        let f = fixture("codex", None);
        let mut foreign = cpa_value(&f, NEW, "foreign", Some("foreign-user"));
        foreign["email"] = json!("foreign@example.invalid");
        let path = if backup {
            f.home.path().join("backup.json")
        } else {
            f.cpa.join("foreign.json")
        };
        write(&path, &foreign);
        let before = record(&f);
        let from = if backup {
            AdoptFrom::Path { path }
        } else {
            AdoptFrom::default()
        };
        assert!(matches!(
            call(&f, from, ProbeState::Stopped),
            Err(AdoptError::IdentityMismatch)
        ));
        assert_eq!(record(&f), before);
        assert_eq!(
            f.store.tokens(&f.account).unwrap().access_token,
            "synthetic-store-access"
        );
    }
    // A changed email is safe only with independent stored user proof.
    let mut f = fixture("codex", None);
    let tokens = Tokens {
        access_token: "synthetic-store-access".into(),
        refresh_token: "synthetic-store-refresh".into(),
        id_token: Some(jwt("proven-user")),
    };
    f.account = f
        .store
        .save(&f.account, &tokens, Some(f.account.generation))
        .unwrap();
    let mut candidate = cpa_value(&f, NEW, "proven", Some("proven-user"));
    candidate["email"] = json!("changed@example.invalid");
    write(&f.cpa.join("proven.json"), &candidate);
    let result = call(&f, AdoptFrom::default(), ProbeState::Stopped).unwrap();
    assert!(result.warnings.contains(&AdoptWarning::EmailUpdated));
    let account = saved(&f);
    assert_stable(&f, &account);
    assert_eq!(account.email, "changed@example.invalid");
    assert_eq!(
        f.store.tokens(&account).unwrap().access_token,
        "synthetic-proven-access"
    );
}
