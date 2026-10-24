use super::*;
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
    path::PathBuf,
};

const ID: &str = "cx-012abc";
const OTHER_ID: &str = "cx-012abd";

// Generated synthetic fixtures only; no temp-home/profile copying, env mutation,
// or systemd credential discovery. Main's Linux harness must make .sandbox writable.
struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    credentials: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/l1-p1-secrets-candidate/fixtures");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("synthetic-")
            .tempdir_in(base)
            .unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.path().join("home");
        let credentials = root.path().join("credentials");
        for path in [&home, &home.join("accounts"), &credentials] {
            DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        Self {
            _root: root,
            home,
            credentials,
        }
    }
    fn key(&self, bytes: &[u8]) {
        write_private(&self.credentials.join("qb-master-key"), bytes);
    }
    fn file(&self) -> SecretProtector {
        SecretProtector::new_with_credentials(&self.home, &file_config(), None).unwrap()
    }
    fn systemd(&self) -> Result<SecretProtector, StoreError> {
        SecretProtector::new_with_credentials(
            &self.home,
            &systemd_config(),
            Some(&self.credentials),
        )
    }
}
fn file_config() -> SecretsConfig {
    SecretsConfig {
        linux_backend: LinuxSecretBackend::File0600,
    }
}
fn systemd_config() -> SecretsConfig {
    SecretsConfig {
        linux_backend: LinuxSecretBackend::SystemdCredential,
    }
}
fn write_private(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}
fn tokens() -> Tokens {
    Tokens {
        access_token: "synthetic-access".into(),
        refresh_token: "synthetic-refresh".into(),
        id_token: None,
    }
}
fn assert_roundtrip(protector: &SecretProtector, ciphertext: &str) {
    let value = protector.decrypt(ID, ciphertext).unwrap();
    assert_eq!(value.access_token, "synthetic-access");
    assert_eq!(value.refresh_token, "synthetic-refresh");
    assert!(value.id_token.is_none());
}

#[test]
fn file_backend_generates_owner_only_key_and_reopens() {
    let fixture = Fixture::new();
    let first = fixture.file();
    let ciphertext = first.encrypt(ID, &tokens()).unwrap();
    assert!(ciphertext.starts_with(PREFIX));
    let key = fs::read(fixture.home.join("master.key")).unwrap();
    assert_eq!(key.len(), 32);
    assert_eq!(
        fs::metadata(fixture.home.join("master.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    drop(first);
    assert_roundtrip(&fixture.file(), &ciphertext);
    assert!(key == fs::read(fixture.home.join("master.key")).unwrap());
    assert_eq!(fs::read_dir(&fixture.home).unwrap().count(), 2); // accounts + master.key, no staging
}

#[test]
fn systemd_backend_reads_raw_0400_key_without_creating_a_local_key() {
    let fixture = Fixture::new();
    fixture.key(&[42; 32]);
    fs::set_permissions(
        fixture.credentials.join("qb-master-key"),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    let protector = fixture.systemd().unwrap();
    let ciphertext = protector.encrypt(ID, &tokens()).unwrap();
    assert_roundtrip(&protector, &ciphertext);
    assert!(!fixture.home.join("master.key").exists());
}

#[test]
fn missing_systemd_credentials_never_fall_back() {
    let fixture = Fixture::new();
    assert!(SecretProtector::new_with_credentials(&fixture.home, &systemd_config(), None).is_err());
    assert!(fixture.systemd().is_err());
    assert!(!fixture.home.join("master.key").exists());
}

#[test]
fn invalid_systemd_lengths_and_public_mode_are_refused() {
    for length in [0, 31, 33, 128] {
        let fixture = Fixture::new();
        fixture.key(&vec![42; length]);
        assert!(fixture.systemd().is_err());
        assert!(!fixture.home.join("master.key").exists());
    }
    let fixture = Fixture::new();
    fixture.key(&[42; 32]);
    let path = fixture.credentials.join("qb-master-key");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(fixture.systemd().is_err());
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn selected_file_backend_ignores_even_invalid_systemd_directory() {
    let fixture = Fixture::new();
    let missing = fixture.credentials.join("not-present");
    let protector =
        SecretProtector::new_with_credentials(&fixture.home, &file_config(), Some(&missing))
            .unwrap();
    assert_roundtrip(&protector, &protector.encrypt(ID, &tokens()).unwrap());
}

#[test]
fn missing_local_key_with_accounts_or_recovery_data_never_creates_replacement() {
    for name in [
        "cx-012abc.json",
        ".takeover-synthetic.journal",
        ".login-migration",
        "unexpected.tmp",
    ] {
        let fixture = Fixture::new();
        write_private(
            &fixture.home.join("accounts").join(name),
            b"synthetic-record",
        );
        assert!(
            SecretProtector::new_with_credentials(&fixture.home, &file_config(), None).is_err()
        );
        assert!(!fixture.home.join("master.key").exists());
        assert_eq!(
            fs::read(fixture.home.join("accounts").join(name)).unwrap(),
            b"synthetic-record"
        );
    }
}

#[test]
fn empty_store_lock_file_does_not_prevent_initial_key_creation() {
    let fixture = Fixture::new();
    // The AccountStore owns/checks the actual lease. This backend only classifies names.
    write_private(&fixture.home.join("accounts/.lock"), b"");
    let _protector = fixture.file();
    assert!(fixture.home.join("master.key").is_file());
}

#[test]
fn malformed_or_public_existing_local_key_is_not_repaired_or_replaced() {
    for (bytes, mode) in [(vec![8; 31], 0o600), (vec![8; 32], 0o644), (vec![], 0o600)] {
        let fixture = Fixture::new();
        let path = fixture.home.join("master.key");
        write_private(&path, &bytes);
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            SecretProtector::new_with_credentials(&fixture.home, &file_config(), None).is_err()
        );
        assert!(fs::read(&path).unwrap() == bytes);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
}

#[test]
fn separate_homes_and_account_ids_do_not_share_ciphertext() {
    let first = Fixture::new();
    let second = Fixture::new();
    let a = first.file();
    let b = second.file();
    let ciphertext = a.encrypt(ID, &tokens()).unwrap();
    assert!(matches!(
        b.decrypt(ID, &ciphertext),
        Err(StoreError::Crypto)
    ));
    assert!(matches!(
        a.decrypt(OTHER_ID, &ciphertext),
        Err(StoreError::Crypto)
    ));
    assert!(a.encrypt("../../invalid", &tokens()).is_err());
}

#[test]
fn fresh_encryptions_have_distinct_nonces_and_roundtrip() {
    let fixture = Fixture::new();
    let protector = fixture.file();
    let first = protector.encrypt(ID, &tokens()).unwrap();
    let second = protector.encrypt(ID, &tokens()).unwrap();
    let a = STANDARD
        .decode(first.strip_prefix(PREFIX).unwrap())
        .unwrap();
    let b = STANDARD
        .decode(second.strip_prefix(PREFIX).unwrap())
        .unwrap();
    assert_ne!(&a[..NONCE_LEN], &b[..NONCE_LEN]);
    assert_roundtrip(&protector, &first);
    assert_roundtrip(&protector, &second);
}

#[test]
fn tampering_nonce_ciphertext_tag_version_and_base64_fails_authentication() {
    let fixture = Fixture::new();
    let protector = fixture.file();
    let ciphertext = protector.encrypt(ID, &tokens()).unwrap();
    let original = STANDARD
        .decode(ciphertext.strip_prefix(PREFIX).unwrap())
        .unwrap();
    for offset in [0, NONCE_LEN, original.len() - 1] {
        let mut changed = original.clone();
        changed[offset] ^= 1;
        let changed = format!("{PREFIX}{}", STANDARD.encode(changed));
        assert!(matches!(
            protector.decrypt(ID, &changed),
            Err(StoreError::Crypto)
        ));
    }
    for value in [
        ciphertext.replacen("v1:", "v2:", 1),
        format!("{PREFIX}!invalid"),
        "untagged-dpapi".into(),
    ] {
        assert!(matches!(
            protector.decrypt(ID, &value),
            Err(StoreError::Crypto)
        ));
    }
}

#[test]
fn truncated_and_oversized_ciphertexts_are_bounded() {
    let fixture = Fixture::new();
    let protector = fixture.file();
    for length in [0, NONCE_LEN, NONCE_LEN + TAG_LEN - 1] {
        let value = format!("{PREFIX}{}", STANDARD.encode(vec![0; length]));
        assert!(matches!(
            protector.decrypt(ID, &value),
            Err(StoreError::Crypto)
        ));
    }
    let too_long = format!("{PREFIX}{}", "A".repeat(MAX_BASE64 + 4));
    assert!(matches!(
        protector.decrypt(ID, &too_long),
        Err(StoreError::Crypto)
    ));
    let huge = Tokens {
        access_token: "x".repeat(MAX_PLAINTEXT),
        refresh_token: "r".into(),
        id_token: None,
    };
    assert!(matches!(
        protector.encrypt(ID, &huge),
        Err(StoreError::InvalidData)
    ));
}

#[test]
fn exact_plaintext_limit_and_authenticated_invalid_json_have_distinct_results() {
    let fixture = Fixture::new();
    let protector = fixture.file();
    // Same leaf as production; each deterministic test nonce is used only once.
    let invalid = protector
        .seal_bytes(ID, Zeroizing::new(b"not-json".to_vec()), [7; NONCE_LEN])
        .unwrap();
    assert!(matches!(
        protector.decrypt(ID, &invalid),
        Err(StoreError::InvalidData)
    ));
    let mut buffer = BoundedPlaintext(Zeroizing::new(Vec::new()));
    buffer.write_all(&vec![0; MAX_PLAINTEXT]).unwrap();
    assert!(buffer.write_all(b"x").is_err());
    assert_eq!(buffer.0.len(), MAX_PLAINTEXT);
}

#[test]
fn broad_home_accounts_and_credential_directory_are_refused_unchanged() {
    for component in ["home", "accounts", "credentials"] {
        let fixture = Fixture::new();
        fixture.key(&[42; 32]);
        let path = match component {
            "home" => fixture.home.clone(),
            "accounts" => fixture.home.join("accounts"),
            _ => fixture.credentials.clone(),
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(fixture.systemd().is_err());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

#[test]
fn symlinked_home_accounts_credential_directory_and_key_are_refused() {
    let fixture = Fixture::new();
    fixture.key(&[42; 32]);
    let alias = fixture.home.with_file_name("home-alias");
    symlink(&fixture.home, &alias).unwrap();
    assert!(SecretProtector::new_with_credentials(&alias, &file_config(), None).is_err());
    let alias = fixture.credentials.with_file_name("credential-alias");
    symlink(&fixture.credentials, &alias).unwrap();
    assert!(
        SecretProtector::new_with_credentials(&fixture.home, &systemd_config(), Some(&alias))
            .is_err()
    );
    fs::rename(
        fixture.home.join("accounts"),
        fixture.home.join("original-accounts"),
    )
    .unwrap();
    symlink(
        fixture.home.join("original-accounts"),
        fixture.home.join("accounts"),
    )
    .unwrap();
    assert!(SecretProtector::new_with_credentials(&fixture.home, &file_config(), None).is_err());

    let fixture = Fixture::new();
    fixture.key(&[42; 32]);
    symlink(
        fixture.credentials.join("qb-master-key"),
        fixture.home.join("master.key"),
    )
    .unwrap();
    assert!(SecretProtector::new_with_credentials(&fixture.home, &file_config(), None).is_err());
    assert!(
        fs::symlink_metadata(fixture.home.join("master.key"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn hardlinked_or_nonregular_keys_are_refused() {
    let fixture = Fixture::new();
    fixture.key(&[42; 32]);
    fs::hard_link(
        fixture.credentials.join("qb-master-key"),
        fixture.home.join("master.key"),
    )
    .unwrap();
    assert!(fixture.systemd().is_err());
    assert!(SecretProtector::new_with_credentials(&fixture.home, &file_config(), None).is_err());
    let fixture = Fixture::new();
    DirBuilder::new()
        .mode(0o700)
        .create(fixture.home.join("master.key"))
        .unwrap();
    assert!(SecretProtector::new_with_credentials(&fixture.home, &file_config(), None).is_err());
}

#[test]
fn fifo_key_refuses_without_waiting_for_a_writer() {
    let fixture = Fixture::new();
    let path = fixture.home.join("master.key");
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert!(SecretProtector::new_with_credentials(&fixture.home, &file_config(), None).is_err());
}

#[test]
fn established_key_is_not_replaced_after_files_appear() {
    let fixture = Fixture::new();
    let protector = fixture.file();
    let ciphertext = protector.encrypt(ID, &tokens()).unwrap();
    let key = fs::read(fixture.home.join("master.key")).unwrap();
    write_private(
        &fixture.home.join("accounts/cx-012abc.json"),
        b"synthetic-record",
    );
    assert_roundtrip(&fixture.file(), &ciphertext);
    assert!(key == fs::read(fixture.home.join("master.key")).unwrap());
    // This is synthetic fixture data, never an installed profile or a real key.
    fs::remove_file(fixture.home.join("master.key")).unwrap();
    assert!(SecretProtector::new_with_credentials(&fixture.home, &file_config(), None).is_err());
    assert!(!fixture.home.join("master.key").exists());
}

#[test]
fn constructor_key_snapshot_survives_later_disk_replacement() {
    let fixture = Fixture::new();
    let protector = fixture.file();
    let ciphertext = protector.encrypt(ID, &tokens()).unwrap();
    let path = fixture.home.join("master.key");
    fs::remove_file(&path).unwrap();
    write_private(&path, &[99; 32]);
    assert_roundtrip(&protector, &ciphertext);
    assert!(fixture.file().decrypt(ID, &ciphertext).is_err());
}

#[test]
fn read_only_0400_local_key_is_accepted_without_mode_repair() {
    let fixture = Fixture::new();
    write_private(&fixture.home.join("master.key"), &[42; 32]);
    fs::set_permissions(
        fixture.home.join("master.key"),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    let _protector = fixture.file();
    assert_eq!(
        File::open(fixture.home.join("master.key"))
            .unwrap()
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
}

#[test]
fn relative_or_empty_credential_path_is_not_resolved_against_current_directory() {
    let fixture = Fixture::new();
    for path in [Path::new(""), Path::new("credentials")] {
        assert!(
            SecretProtector::new_with_credentials(&fixture.home, &systemd_config(), Some(path))
                .is_err()
        );
    }
}

#[test]
fn injected_nonce_rng_failure_returns_no_ciphertext_and_preserves_key() {
    let fixture = Fixture::new();
    let protector = fixture.file();
    let before = fs::read(fixture.home.join("master.key")).unwrap();
    assert!(matches!(
        protector.encrypt_with_nonce_source(ID, &tokens(), |_| Err(StoreError::Crypto)),
        Err(StoreError::Crypto)
    ));
    assert!(before == fs::read(fixture.home.join("master.key")).unwrap());
    assert_eq!(
        fs::read_dir(fixture.home.join("accounts")).unwrap().count(),
        0
    );
}
