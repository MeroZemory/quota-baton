use super::*;

#[test]
fn systemd_255_acl_modes_require_root_and_readonly_mount_flags() {
    for mode in [0o500, 0o550] {
        assert!(directory_policy(mode, 0));
        assert!(!directory_policy(mode, 1000));
    }
    for mode in [0o400, 0o440] {
        assert!(key_policy(mode, 0, 1, 32));
        assert!(!key_policy(mode, 1000, 1, 32));
    }
    let required = libc::ST_RDONLY | libc::ST_NOSUID | libc::ST_NODEV;
    assert!(mount_policy(required));
    for missing in [libc::ST_RDONLY, libc::ST_NOSUID, libc::ST_NODEV] {
        assert!(!mount_policy(required & !missing));
    }
}

#[test]
fn public_writable_executable_special_and_linked_credentials_are_refused() {
    for mode in [
        0o000, 0o600, 0o640, 0o444, 0o441, 0o450, 0o460, 0o2440, 0o4440,
    ] {
        assert!(!key_policy(mode, 0, 1, 32), "mode {mode:o}");
    }
    for mode in [0o700, 0o750, 0o555, 0o551, 0o570, 0o2550] {
        assert!(!directory_policy(mode, 0), "mode {mode:o}");
    }
    for length in [0, 31, 33, 4096] {
        assert!(!key_policy(0o440, 0, 1, length));
    }
    for links in [0, 2, 3] {
        assert!(!key_policy(0o440, 0, links, 32));
    }
}

#[test]
fn credential_paths_are_absolute_bounded_and_cannot_traverse_parents() {
    for path in [
        "",
        "credentials",
        "/../run/credentials",
        "/run/../credentials",
    ] {
        assert!(load(Path::new(path)).is_err());
    }
    assert!(load(Path::new(&format!("/{}", "a".repeat(4096)))).is_err());
}

#[test]
fn ordinary_writable_root_directory_is_not_a_credential_mount() {
    // Safe metadata-only negative fixture: no key is opened or discovered.
    assert!(load(Path::new("/")).is_err());
}

fn acl(directory: bool) -> Vec<u8> {
    let read = if directory { 5u16 } else { 4u16 };
    let mut bytes = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in [
        (1u16, read, u32::MAX),
        (2, read, 1000u32),
        (4, 0, u32::MAX),
        (16, read, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&perm.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    bytes
}

#[test]
fn acl_grant_is_only_for_the_service_user_with_no_group_or_other_access() {
    for directory in [false, true] {
        let good = acl(directory);
        assert!(acl_policy(&good, 1000, directory));
        assert!(!acl_policy(&good, 1001, directory));
        for (offset, value) in [(22, 4), (38, 4), (14, 6), (30, 7), (20, 8)] {
            let mut bad = good.clone();
            bad[offset] = value;
            assert!(!acl_policy(&bad, 1000, directory));
        }
        assert!(!acl_policy(&good[..43], 1000, directory));
        let mut extra = good.clone();
        extra.extend_from_slice(&[0; 8]);
        assert!(!acl_policy(&extra, 1000, directory));
    }
}
