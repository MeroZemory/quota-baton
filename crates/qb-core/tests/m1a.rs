use chrono::{Duration, Utc};
use qb_core::accounts::{
    AccountStore, Health, HealthReason, Identity, Provider, Source, StoreError, UserState,
    import::{self, ImportAction, SkipReason},
    linked::{self, LinkError},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::Duration as StdDuration,
};

static SERIAL: Mutex<()> = Mutex::new(());

fn isolated(name: &str, test: impl FnOnce(&Path, &Path)) {
    if std::env::var("QB_M1A_CASE").as_deref() == Ok(name) {
        let home = PathBuf::from(std::env::var_os("QB_HOME").expect("synthetic home"));
        let cpa = home.parent().unwrap().join("cpa");
        fs::create_dir_all(&cpa).unwrap();
        test(&home, &cpa);
        return;
    }
    let _guard = SERIAL.lock().unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m1a-tests");
    fs::create_dir_all(&root).unwrap();
    let fixture = tempfile::Builder::new()
        .prefix(name)
        .tempdir_in(root)
        .unwrap();
    // 환경 변수는 자식 프로세스에만 설정하여 병렬 테스트와 충돌하지 않는다.
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("QB_HOME", fixture.path().join("home"))
        .env("QB_M1A_CASE", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in ["fake-access-", "fake-refresh-", "fake-id-"] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
}

fn fixture(provider: &str, version: u32) -> Value {
    json!({"type":provider, "email":"synthetic@example.invalid", "account_uuid":"fake-uuid", "organization_uuid":"fake-org", "organization_name":"Synthetic organization", "account_id":"fake-account", "plan_type":"plus", "access_token":format!("fake-access-{version}"), "refresh_token":format!("fake-refresh-{version}"), "id_token":format!("fake-id-{version}"), "expired":format!("2030-01-0{version}T02:00:00Z"), "last_refresh":format!("2030-01-0{version}T01:00:00Z"), "disabled":false, "claude_device_ids":["must-not-copy"]})
}

fn write(dir: &Path, name: &str, value: &Value) {
    fs::write(dir.join(name), serde_json::to_vec(value).unwrap()).unwrap();
}

fn verify_dpapi_entropy(encoded: &str) {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use windows::Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        },
    };
    use zeroize::Zeroize;
    let mut encrypted = STANDARD.decode(encoded).unwrap();
    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len() as u32,
        pbData: encrypted.as_mut_ptr(),
    };
    for (entropy, expected_success) in [
        (b"wrong-application".as_slice(), false),
        (b"quota-baton/v1/account-secret".as_slice(), true),
    ] {
        let entropy = CRYPT_INTEGER_BLOB {
            cbData: entropy.len() as u32,
            pbData: entropy.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // 본 구현과 독립적으로 DPAPI 엔트로피 계약을 검증한다.
        unsafe {
            let result = CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            );
            assert_eq!(result.is_ok(), expected_success);
            if result.is_ok() {
                let bytes = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
                let tokens: qb_core::accounts::Tokens = serde_json::from_slice(bytes).unwrap();
                assert!(tokens.access_token == "fake-access-1");
                bytes.zeroize();
                let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
            }
        }
    }
}

#[test]
fn mapping_and_secret_output() {
    isolated("mapping_and_secret_output", |home, cpa| {
        write(cpa, "arbitrary-a.json", &fixture("claude", 1));
        write(cpa, "arbitrary-b.json", &fixture("codex", 1));
        write(cpa, "unknown.json", &json!({"type":"unsupported"}));
        let store = AccountStore::acquire(home).unwrap();
        let rows = import::run(&store, cpa, &[], false).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .any(|row| row.reason == Some(SkipReason::UnknownType))
        );
        let accounts = store.list().unwrap();
        assert_eq!(accounts.len(), 2);
        for account in &accounts {
            assert_eq!(account.id, account.identity.id());
            assert_eq!(account.user_state, UserState::Disabled);
            assert_eq!(account.health, Health::Ready);
            assert_eq!(account.generation, 1);
            assert!(matches!(account.source, Source::Cpa { .. }));
            assert_eq!(
                account.token_meta.expires_at,
                Some("2030-01-01T02:00:00Z".parse().unwrap())
            );
            assert_eq!(
                account.token_meta.last_refresh_at,
                Some("2030-01-01T01:00:00Z".parse().unwrap())
            );
            assert!(!account.token_meta.refresh_uncertain);
            let tokens = store.tokens(account).unwrap();
            assert!(tokens.access_token == "fake-access-1");
            assert!(tokens.refresh_token == "fake-refresh-1");
            assert!(tokens.id_token.as_deref() == Some("fake-id-1"));
            let disk =
                fs::read_to_string(home.join("accounts").join(format!("{}.json", account.id)))
                    .unwrap();
            for secret in [
                "fake-access-1",
                "fake-refresh-1",
                "fake-id-1",
                "must-not-copy",
                "claude_device_ids",
                "\"disabled\":false",
            ] {
                assert!(!disk.contains(secret));
            }
            let value: Value = serde_json::from_str(&disk).unwrap();
            assert!(value["secret_dpapi_b64"].is_string());
            verify_dpapi_entropy(value["secret_dpapi_b64"].as_str().unwrap());
            if account.provider == Provider::Claude {
                assert_eq!(account.label, "Synthetic organization");
                assert!(account.plan.is_none());
            } else {
                assert_eq!(account.plan.as_deref(), Some("plus"));
            }
        }
        let output = format!(
            "{} {} {:?}",
            serde_json::to_string(&rows).unwrap(),
            serde_json::to_string(&accounts).unwrap(),
            store.tokens(&accounts[0]).unwrap()
        );
        for secret in ["fake-access-1", "fake-refresh-1", "fake-id-1"] {
            assert!(!output.contains(secret));
        }
        println!("{output}");
        // DPAPI 출력이 손상되면 비밀을 포함하지 않는 오류로 실패한다.
        let path = home
            .join("accounts")
            .join(format!("{}.json", accounts[0].id));
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["secret_dpapi_b64"] = json!("broken");
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            store.tokens(&accounts[0]),
            Err(StoreError::Crypto)
        ));
    });
}

#[test]
fn reimport_activate_equal_and_older() {
    isolated("reimport_activate_equal_and_older", |home, cpa| {
        write(cpa, "initial.json", &fixture("codex", 2));
        let store = AccountStore::acquire(home).unwrap();
        import::run(&store, cpa, &[], false).unwrap();
        let initial = store.list().unwrap().remove(0);
        fs::rename(cpa.join("initial.json"), cpa.join("renamed.json")).unwrap();
        let mut equal = fixture("codex", 2);
        equal["access_token"] = json!("fake-access-equal-rotation");
        write(cpa, "renamed.json", &equal);
        let rows = import::run(&store, cpa, &[], false).unwrap();
        assert_eq!(rows[0].action, ImportAction::Updated);
        let equal_account = store.load(&initial.id).unwrap();
        assert!(store.tokens(&equal_account).unwrap().access_token == "fake-access-equal-rotation");
        assert_eq!(equal_account.user_state, UserState::Disabled);
        write(cpa, "renamed.json", &fixture("codex", 1));
        let rows = import::run(
            &store,
            cpa,
            &["codex:synthetic@example.invalid".into()],
            false,
        )
        .unwrap();
        assert_eq!(rows[0].action, ImportAction::Skipped);
        assert_eq!(rows[0].reason, Some(SkipReason::OlderToken));
        assert!(rows[0].stored_timestamp > rows[0].file_timestamp);
        let activated = store.load(&initial.id).unwrap();
        assert_eq!(activated.user_state, UserState::Active);
        assert!(store.tokens(&activated).unwrap().access_token == "fake-access-equal-rotation");
        assert_eq!(activated.token_meta, equal_account.token_meta);
        let generation = activated.generation;
        import::run(&store, cpa, &[], false).unwrap();
        assert_eq!(store.load(&initial.id).unwrap().generation, generation);
        write(cpa, "renamed.json", &fixture("codex", 3));
        import::run(&store, cpa, &[], false).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        let updated = store.load(&initial.id).unwrap();
        assert_eq!(updated.user_state, UserState::Active);
        assert!(store.tokens(&updated).unwrap().access_token == "fake-access-3");
        let mut paused = updated.clone();
        paused.user_state = UserState::Paused;
        store
            .save(
                &paused,
                &store.tokens(&updated).unwrap(),
                Some(updated.generation),
            )
            .unwrap();
        import::run(&store, cpa, &[], false).unwrap();
        assert_eq!(
            store.load(&initial.id).unwrap().user_state,
            UserState::Paused
        );
    });
}

#[test]
fn preview_and_adopt_write_nothing() {
    isolated("preview_and_adopt_write_nothing", |home, cpa| {
        write(cpa, "source.json", &fixture("claude", 1));
        let rows =
            import::preview(home, cpa, &["claude:synthetic@example.invalid".into()]).unwrap();
        assert_eq!(rows[0].user_state, Some(UserState::Active));
        assert!(!home.exists());
        assert!(matches!(
            import::refuse_adopt(),
            Err(StoreError::AdoptUnavailable)
        ));
        assert_eq!(
            import::refuse_adopt().unwrap_err().to_string(),
            "adopt is a cutover step (M8)"
        );
        assert!(!home.exists());
        let store = AccountStore::acquire(home).unwrap();
        import::run(&store, cpa, &[], false).unwrap();
        let account = store.list().unwrap().remove(0);
        let path = home.join("accounts").join(format!("{}.json", account.id));
        let before = fs::read(&path).unwrap();
        write(cpa, "source.json", &fixture("claude", 2));
        assert_eq!(
            import::run(&store, cpa, &[], true).unwrap()[0].action,
            ImportAction::Updated
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            import::preview(home, cpa, &[]).unwrap()[0].action,
            ImportAction::Updated
        );
        assert_eq!(AccountStore::list_metadata(home).unwrap().len(), 1);
    });
}

#[test]
fn generation_cooling_and_atomic_write() {
    isolated("generation_cooling_and_atomic_write", |home, cpa| {
        write(cpa, "source.json", &fixture("codex", 1));
        let store = AccountStore::acquire(home).unwrap();
        import::run(&store, cpa, &[], false).unwrap();
        let initial = store.list().unwrap().remove(0);
        let tokens = store.tokens(&initial).unwrap();
        // 삭제 공유를 막아 원자적 교체 실패 시 원본 보존과 임시 파일 정리를 검증한다.
        use std::os::windows::fs::OpenOptionsExt;
        let account_path = home.join("accounts").join(format!("{}.json", initial.id));
        let blocker = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&account_path)
            .unwrap();
        assert!(matches!(
            store.save(&initial, &tokens, Some(initial.generation)),
            Err(StoreError::Io)
        ));
        drop(blocker);
        assert_eq!(
            store.load(&initial.id).unwrap().generation,
            initial.generation
        );
        let mut older = initial.clone();
        older.token_meta.last_refresh_at = Some("2029-01-01T00:00:00Z".parse().unwrap());
        assert!(matches!(
            store.save(&older, &tokens, Some(initial.generation)),
            Err(StoreError::OlderToken)
        ));
        let mut cooling = initial.clone();
        cooling.health = Health::Cooling {
            reason: HealthReason::LinkStale,
            until: Utc::now() - Duration::seconds(1),
            scope: "account".into(),
        };
        let updated = store
            .save(&cooling, &tokens, Some(initial.generation))
            .unwrap();
        assert_eq!(updated.generation, 2);
        assert_eq!(store.load(&initial.id).unwrap().health, Health::Ready);
        assert!(matches!(
            store.save(&initial, &tokens, Some(initial.generation)),
            Err(StoreError::GenerationConflict {
                expected: Some(1),
                actual: Some(3)
            })
        ));
        assert!(matches!(
            store.tokens(&initial),
            Err(StoreError::GenerationConflict { .. })
        ));
        assert!(matches!(
            store.save(&initial, &tokens, None),
            Err(StoreError::GenerationConflict { .. })
        ));
        assert!(matches!(
            store.load("../../credentials"),
            Err(StoreError::InvalidData)
        ));
        assert!(fs::read_dir(home.join("accounts")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        drop(store);
        let reopened = AccountStore::acquire(home).unwrap();
        assert_eq!(reopened.load(&initial.id).unwrap().generation, 3);
        assert!(
            reopened
                .tokens(&reopened.load(&initial.id).unwrap())
                .unwrap()
                .access_token
                == "fake-access-1"
        );
    });
}

#[test]
fn lease_probe() {
    if std::env::var_os("QB_M1A_PROBE").is_some() {
        let home = PathBuf::from(std::env::var_os("QB_HOME").unwrap());
        assert!(matches!(
            AccountStore::acquire(&home),
            Err(StoreError::LeaseBusy)
        ));
    }
}

#[test]
fn lease_contention_drop_and_thread_safety() {
    isolated("lease_contention_drop_and_thread_safety", |home, _| {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<AccountStore>();
        let store = Arc::new(AccountStore::acquire(home).unwrap());
        assert!(matches!(
            AccountStore::acquire(home),
            Err(StoreError::LeaseBusy)
        ));
        let other_home = home.with_file_name("other-home");
        assert!(matches!(
            AccountStore::acquire(&other_home),
            Err(StoreError::LeaseBusy)
        ));
        let home_copy = home.to_path_buf();
        assert!(
            thread::spawn(move || matches!(
                AccountStore::acquire(&home_copy),
                Err(StoreError::LeaseBusy)
            ))
            .join()
            .unwrap()
        );
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lease_probe"])
            .env("QB_M1A_PROBE", "1")
            .env("QB_HOME", home)
            .output()
            .unwrap();
        assert!(child.status.success());
        let reader = Arc::clone(&store);
        thread::spawn(move || assert!(reader.list().unwrap().is_empty()))
            .join()
            .unwrap();
        // 마지막 Arc는 획득 호출자와 다른 스레드에서 해제한다.
        thread::spawn(move || drop(store)).join().unwrap();
        let reacquired = AccountStore::acquire(home).unwrap();
        drop(reacquired);
        let other = AccountStore::acquire(&other_home).unwrap();
        drop(other);
        // 이름 있는 mutex 없이도 파일 잠금이 중복 소유를 거부한다.
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(home.join("accounts/.lock"))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        assert!(matches!(
            AccountStore::acquire(home),
            Err(StoreError::LeaseBusy)
        ));
        drop(lock);
        assert!(AccountStore::acquire(home).is_ok());
    });
}

#[test]
fn linked_rename_retry_missing_stale_and_persist() {
    isolated(
        "linked_rename_retry_missing_stale_and_persist",
        |home, cpa| {
            write(cpa, "misleading-claude-name.json", &fixture("codex", 1));
            let store = AccountStore::acquire(home).unwrap();
            import::run(&store, cpa, &[], false).unwrap();
            let initial = store.list().unwrap().remove(0);
            assert!(matches!(linked::reload(&initial), Err(LinkError::Stale)));
            fs::rename(
                cpa.join("misleading-claude-name.json"),
                cpa.join("renamed.json"),
            )
            .unwrap();
            fs::write(
                cpa.join("renamed.json"),
                b"{\"type\":\"codex\",\"access_token\":",
            )
            .unwrap();
            let cpa_copy = cpa.to_path_buf();
            let writer = thread::spawn(move || {
                thread::sleep(StdDuration::from_millis(80));
                write(&cpa_copy, "renamed.json", &fixture("codex", 2));
            });
            assert!(linked::reload(&initial).unwrap().access_token == "fake-access-2");
            writer.join().unwrap();
            assert_eq!(store.load(&initial.id).unwrap().generation, 1);
            let updated = linked::reload_and_store(&store, &initial).unwrap();
            assert_eq!(updated.generation, 2);
            assert!(store.tokens(&updated).unwrap().access_token == "fake-access-2");
            assert!(matches!(
                linked::reload_and_store(&store, &initial),
                Err(LinkError::Store(StoreError::GenerationConflict { .. }))
            ));
            assert!(matches!(linked::reload(&updated), Err(LinkError::Stale)));
            let mut wrong = fixture("codex", 3);
            wrong["account_id"] = json!("unrelated");
            write(cpa, "renamed.json", &wrong);
            assert!(matches!(linked::reload(&updated), Err(LinkError::Missing)));
            fs::remove_file(cpa.join("renamed.json")).unwrap();
            assert!(matches!(linked::reload(&updated), Err(LinkError::Missing)));
            fs::write(cpa.join("partial.json"), b"{invalid").unwrap();
            assert!(matches!(
                linked::reload(&updated),
                Err(LinkError::InvalidData)
            ));
            let mut oauth = updated;
            oauth.source = Source::Oauth;
            assert!(matches!(linked::reload(&oauth), Err(LinkError::NotLinked)));
        },
    );
}

#[test]
fn canonical_identity_and_collision_guard() {
    isolated("canonical_identity_and_collision_guard", |home, cpa| {
        let mut same = fixture("claude", 1);
        write(cpa, "one.json", &same);
        let store = AccountStore::acquire(home).unwrap();
        import::run(&store, cpa, &[], false).unwrap();
        same["email"] = json!("changed@example.invalid");
        same["last_refresh"] = json!("2030-01-02T01:00:00Z");
        write(cpa, "one.json", &same);
        import::run(&store, cpa, &[], false).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        let original = store.list().unwrap().remove(0);
        let tokens = store.tokens(&original).unwrap();
        let mut seen = HashMap::new();
        let (a, b) = (0..100_000)
            .find_map(|n| {
                let identity = Identity::Codex {
                    account_id: format!("synthetic-collision-{n}"),
                    email: "collision@example.invalid".into(),
                };
                seen.insert(identity.id(), identity.clone())
                    .map(|previous| (previous, identity))
            })
            .expect("24-bit hash collision in synthetic identities");
        let mut account = original.clone();
        account.id = a.id();
        account.identity = a;
        account.provider = Provider::Codex;
        account.generation = 0;
        store.save(&account, &tokens, None).unwrap();
        account.identity = b;
        assert!(matches!(
            store.save(&account, &tokens, None),
            Err(StoreError::IdentityCollision)
        ));
        let first = Identity::Claude {
            account_uuid: "ab".into(),
            organization_uuid: Some("c".into()),
            email: "x".into(),
        };
        let second = Identity::Claude {
            account_uuid: "a".into(),
            organization_uuid: Some("bc".into()),
            email: "x".into(),
        };
        assert_ne!(first.canonical_key(), second.canonical_key());
        let mut fallback = fixture("claude", 1);
        fallback
            .as_object_mut()
            .unwrap()
            .remove("organization_uuid");
        write(cpa, "one.json", &fallback);
        import::run(&store, cpa, &[], false).unwrap();
        assert_eq!(store.list().unwrap().len(), 3);
    });
}
