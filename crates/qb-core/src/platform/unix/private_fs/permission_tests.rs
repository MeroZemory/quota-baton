use super::*;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn readonly_home_preflight_preserves_absence_and_refuses_existing_loose_children() {
    let home = tests::fixture();
    check_private_data_home(home.path()).unwrap();
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    for name in ["accounts", "state", "logs"] {
        let child = home.path().join(name);
        fs::create_dir(&child).unwrap();
        set_mode(&child, 0o755);
        let error = check_private_data_home(home.path()).unwrap_err();
        let message = diagnostic(&error, &child, "0755");
        assert!(!message.contains("<private entry>"));
        assert_eq!(
            fs::metadata(&child).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        fs::remove_dir(&child).unwrap();
    }
    assert!(check_private_data_home(&home.path().join("missing")).is_err());
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn diagnostic(error: &io::Error, path: &Path, mode: &str) -> String {
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    let message = startup_permission_diagnostic(error).expect("typed permission refusal");
    assert!(message.contains(path.to_str().unwrap()), "{message}");
    assert!(message.contains(mode), "{message}");
    message
}

#[test]
fn config_0664_refusal_names_file_mode_and_repair_without_contents() {
    let home = tests::fixture();
    let path = home.path().join("config.toml");
    let contents = b"synthetic-private-content-do-not-print";
    fs::write(&path, contents).unwrap();
    set_mode(&path, 0o664);

    let error = crate::config::Config::load(home.path()).unwrap_err();
    let io_error = error.downcast_ref::<io::Error>().unwrap();
    let message = diagnostic(io_error, &path, "found mode 0664");
    assert!(message.contains("install -m 0600"));
    assert!(!message.contains(std::str::from_utf8(contents).unwrap()));
    assert_eq!(fs::read(&path).unwrap(), contents);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o664);

    set_mode(&path, 0o600);
    fs::write(&path, b"").unwrap();
    assert!(crate::config::Config::load(home.path()).is_ok());
}

#[test]
fn runtime_home_and_logs_refusals_name_the_actual_directory() {
    let root = tests::fixture();
    let home = root.path().join("home");
    fs::create_dir(&home).unwrap();
    set_mode(&home, 0o755);
    let error = super::super::secrets::prepare_runtime_home(&home).unwrap_err();
    assert!(diagnostic(&error, &home, "found mode 0755").contains("chmod 700"));
    assert!(!home.join("logs").exists());
    assert_eq!(fs::metadata(&home).unwrap().mode() & 0o7777, 0o755);

    set_mode(&home, 0o700);
    let logs = home.join("logs");
    fs::create_dir(&logs).unwrap();
    set_mode(&logs, 0o755);
    let error = super::super::secrets::prepare_runtime_home(&home).unwrap_err();
    diagnostic(&error, &logs, "found mode 0755");
    assert_eq!(fs::metadata(&logs).unwrap().mode() & 0o7777, 0o755);
}

#[test]
fn writable_ancestor_is_reported_instead_of_requested_descendant() {
    let root = tests::fixture();
    let ancestor = root.path().join("ancestor");
    let home = ancestor.join("home");
    fs::create_dir_all(&home).unwrap();
    set_mode(&ancestor, 0o775);
    set_mode(&home, 0o700);

    for error in [
        prepare_private_directory(&home).unwrap_err(),
        ensure_private_dir_all(&home).unwrap_err(),
    ] {
        let message = diagnostic(&error, &ancestor, "found mode 0775");
        assert!(!message.contains(home.to_str().unwrap()), "{message}");
    }
    assert_eq!(fs::metadata(&ancestor).unwrap().mode() & 0o7777, 0o775);
}

#[test]
fn existing_nonprivate_parent_is_named_before_creating_a_suffix() {
    let root = tests::fixture();
    let parent = root.path().join("parent");
    fs::create_dir(&parent).unwrap();
    set_mode(&parent, 0o755);
    let leaf = parent.join("missing/leaf");
    let error = ensure_private_dir_all(&leaf).unwrap_err();
    diagnostic(&error, &parent, "found mode 0755");
    assert!(!parent.join("missing").exists());
}

#[test]
fn startup_token_refusal_keeps_file_bytes_out_of_the_diagnostic() {
    let root = tests::fixture();
    let path = root.path().join("admin.token");
    let contents = b"synthetic-secret-never-print";
    fs::write(&path, contents).unwrap();
    set_mode(&path, 0o644);
    let error = super::super::secrets::load_secret_file(&path).unwrap_err();
    let message = diagnostic(error.downcast_ref::<io::Error>().unwrap(), &path, "0644");
    assert!(message.contains("install -m 0600"));
    assert!(!message.contains(std::str::from_utf8(contents).unwrap()));
    assert_eq!(fs::read(&path).unwrap(), contents);
}

#[test]
fn diagnostic_is_bounded_escapes_controls_and_does_not_allow_arbitrary_io_text() {
    let raw = io::Error::new(io::ErrorKind::PermissionDenied, "synthetic-private-error");
    assert!(startup_permission_diagnostic(&raw).is_none());
    let path = format!("/private/\n\x1b[31m{}", "x".repeat(4096));
    let error = permission_path(raw, Path::new(&path));
    let message = startup_permission_diagnostic(&error).unwrap();
    assert!(!message.contains("synthetic-private-error"));
    assert!(!message.contains('\n'));
    assert!(!message.contains('\x1b'));
    assert!(message.contains("\\n\\u{1b}"));
    assert!(message.contains("..."));
    assert!(message.len() < 4096);
    assert!(!message.contains("found mode"));
    let unrelated = io::Error::new(io::ErrorKind::InvalidData, "synthetic-private-error");
    let unrelated = permission_path(unrelated, Path::new("/private/file"));
    assert_eq!(unrelated.kind(), io::ErrorKind::InvalidData);
    assert!(startup_permission_diagnostic(&unrelated).is_none());
}

#[test]
fn storage_mapping_preserves_only_typed_refusals_and_keeps_other_fallbacks() {
    for fallback in [
        crate::accounts::StoreError::Io,
        crate::accounts::StoreError::Crypto,
        crate::accounts::StoreError::LeaseUnavailable,
    ] {
        let expected = std::mem::discriminant(&fallback);
        let raw = io::Error::new(io::ErrorKind::PermissionDenied, "synthetic-private-error");
        let actual = map_storage_error(raw, fallback);
        assert_eq!(std::mem::discriminant(&actual), expected);
    }
    let root = tests::fixture();
    let accounts = root.path().join("accounts");
    fs::create_dir(&accounts).unwrap();
    set_mode(&accounts, 0o755);
    let error = super::super::secrets::prepare_account_directory(root.path()).unwrap_err();
    let crate::accounts::StoreError::StoragePermission(error) = error else {
        panic!("expected typed accounts permission refusal");
    };
    diagnostic(&error, &accounts, "0755");
}
