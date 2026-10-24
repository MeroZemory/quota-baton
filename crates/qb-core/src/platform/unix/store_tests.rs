//! Actual store/protector integration, using only synthetic private homes.
use super::*;
use crate::accounts::{Account, AccountStore, Identity, StoreError, Tokens};
use std::{fs, os::unix::fs::PermissionsExt};

fn account(name: &str) -> Account {
    let mut account = crate::accounts::health::tests::fixture();
    account.identity = Identity::Codex {
        user_id: None,
        account_id: name.into(),
        email: format!("{name}@example.test"),
    };
    account.id = account.identity.id();
    account.email = format!("{name}@example.test");
    account.label = name.into();
    account
}
fn tokens() -> Tokens {
    Tokens {
        access_token: "synthetic-store-access".into(),
        refresh_token: "synthetic-store-refresh".into(),
        id_token: None,
    }
}
fn record(home: &std::path::Path, account: &Account) -> std::path::PathBuf {
    home.join("accounts").join(format!("{}.json", account.id))
}

#[test]
fn actual_store_encrypts_reopens_and_metadata_preserves_ciphertext() {
    let home = private_fs::tests::fixture();
    let store = AccountStore::acquire(home.path()).unwrap();
    let saved = store
        .save(&account("sample-acct"), &tokens(), None)
        .unwrap();
    let path = record(home.path(), &saved);
    let before: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let cipher = before["secret_dpapi_b64"].as_str().unwrap();
    assert!(cipher.starts_with("qb-aes256gcm-v1:"));
    assert!(
        !fs::read_to_string(&path)
            .unwrap()
            .contains("synthetic-store-access")
    );
    let changed = store
        .update_metadata(&saved.id, |a| a.label = "new label".into())
        .unwrap();
    let after: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(after["secret_dpapi_b64"], before["secret_dpapi_b64"]);
    drop(store);
    let reopened = AccountStore::acquire(home.path()).unwrap();
    assert_eq!(
        reopened.tokens(&changed).unwrap().access_token,
        "synthetic-store-access"
    );
    reopened.stop_writes();
    assert!(matches!(
        reopened.save(&changed, &tokens(), Some(changed.generation)),
        Err(StoreError::LeaseUnavailable)
    ));
}

#[test]
fn actual_store_rejects_account_cipher_swap_and_unsafe_record_without_repair() {
    let home = private_fs::tests::fixture();
    let store = AccountStore::acquire(home.path()).unwrap();
    let first = store.save(&account("sample-one"), &tokens(), None).unwrap();
    let second = store.save(&account("sample-two"), &tokens(), None).unwrap();
    let first_json: serde_json::Value =
        serde_json::from_slice(&fs::read(record(home.path(), &first)).unwrap()).unwrap();
    let path = record(home.path(), &second);
    let mut second_json: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    second_json["secret_dpapi_b64"] = first_json["secret_dpapi_b64"].clone();
    fs::write(&path, serde_json::to_vec(&second_json).unwrap()).unwrap();
    assert!(matches!(store.tokens(&second), Err(StoreError::Crypto)));
    assert_eq!(AccountStore::list_metadata(home.path()).unwrap().len(), 2);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(store.tokens(&second), Err(StoreError::Io)));
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn actual_explicit_file_backend_reopens_and_never_replaces_a_lost_key() {
    let home = private_fs::tests::fixture();
    prepare_runtime_home(home.path()).unwrap();
    create_private_secret(
        &home.path().join("config.toml"),
        b"[secrets]\nlinux_backend = \"file-0600\"\n",
    )
    .unwrap();
    let store = AccountStore::acquire(home.path()).unwrap();
    let saved = store
        .save(&account("sample-file"), &tokens(), None)
        .unwrap();
    let bytes = fs::read(record(home.path(), &saved)).unwrap();
    let key_path = home.path().join("master.key");
    let original_key = zeroize::Zeroizing::new(fs::read(&key_path).unwrap());
    assert_eq!(original_key.len(), 32);
    drop(store);
    let reopened = AccountStore::acquire(home.path()).unwrap();
    assert_eq!(
        reopened.tokens(&saved).unwrap().refresh_token,
        "synthetic-store-refresh"
    );
    assert_eq!(&*original_key, &fs::read(&key_path).unwrap());
    drop(reopened);
    fs::remove_file(&key_path).unwrap();
    assert!(matches!(
        AccountStore::acquire(home.path()),
        Err(StoreError::Crypto)
    ));
    assert!(!key_path.exists());
    assert_eq!(fs::read(record(home.path(), &saved)).unwrap(), bytes);
}
