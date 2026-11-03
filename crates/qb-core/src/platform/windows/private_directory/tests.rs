use super::*;
use std::sync::{Arc, Barrier};
use windows::Win32::Security::{
    Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
    PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
};

fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    super::super::secrets::restrict(root.path()).unwrap();
    root
}

fn set_dacl(path: &Path, sddl: &str) {
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .unwrap();
        let _descriptor = Allocation(HLOCAL(descriptor.0));
        SetFileSecurityW(
            PCWSTR(path.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
        .ok()
        .unwrap();
    }
}

fn dacl_bytes(path: &Path) -> Vec<u8> {
    let directory = open_directory(path).unwrap();
    let security = Security::read(&directory).unwrap();
    unsafe {
        std::slice::from_raw_parts(
            security.dacl.cast::<u8>(),
            usize::from((*security.dacl).AclSize),
        )
        .to_vec()
    }
}

fn current_sid() -> String {
    let user = User::current().unwrap();
    unsafe {
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.sid(), &mut sid).unwrap();
        let _allocation = Allocation(HLOCAL(sid.0.cast()));
        sid.to_string().unwrap()
    }
}

#[test]
fn refusal_names_path_and_bounded_untrusted_sids_without_acl_repair() {
    let home = root();
    let mut sddl = format!("D:P(A;OICI;FA;;;{})", current_sid());
    for rid in 1000..1020 {
        sddl.push_str(&format!("(A;OICI;FR;;;S-1-5-21-111-222-333-{rid})"));
    }
    set_dacl(home.path(), &sddl);
    let before = dacl_bytes(home.path());
    let error = ensure_private_dir_all(home.path()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    let message = startup_permission_diagnostic(&error).unwrap();
    assert!(message.contains(home.path().to_str().unwrap()));
    assert!(message.contains("S-1-5-21-111-222-333-1000"));
    assert_eq!(message.matches("S-1-5-21-111-222-333-").count(), 16);
    assert!(message.contains("additional principals omitted"));
    assert!(message.contains("updated installer"));
    assert!(message.contains("owner, SYSTEM and Administrators"));
    assert!(!message.contains("D:P"));
    assert!(!message.contains("(A;"));
    assert!(message.len() < 8192);
    assert_eq!(dacl_bytes(home.path()), before);
}

#[test]
fn readonly_preflight_accepts_private_home_and_does_not_create_missing_children() {
    let home = root();
    let before = dacl_bytes(home.path());
    let names_before: Vec<_> = fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    check_private_data_home(home.path()).unwrap();
    let names_after: Vec<_> = fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names_before, names_after);
    assert_eq!(dacl_bytes(home.path()), before);
    for name in ["accounts", "state", "logs", ".lock"] {
        assert!(!home.path().join(name).exists());
    }
    let missing = home.path().join("missing-home");
    assert_eq!(
        check_private_data_home(&missing).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(!missing.exists());
}

#[test]
fn readonly_preflight_checks_each_existing_known_child_and_keeps_acl_and_bytes() {
    let home = root();
    let sid = current_sid();
    for name in ["accounts", "state", "logs"] {
        let path = home.path().join(name);
        ensure_private_dir_all(&path).unwrap();
        fs::write(path.join("sentinel"), b"synthetic-private-do-not-print").unwrap();
        check_private_data_home(home.path()).unwrap();
        set_dacl(&path, &format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FR;;;WD)"));
        let before = dacl_bytes(&path);
        let error = check_private_data_home(home.path()).unwrap_err();
        let message = startup_permission_diagnostic(&error).unwrap();
        assert!(message.contains(path.to_str().unwrap()));
        assert!(message.contains("S-1-1-0"));
        assert!(!message.contains("synthetic-private-do-not-print"));
        assert_eq!(dacl_bytes(&path), before);
        assert_eq!(
            fs::read(path.join("sentinel")).unwrap(),
            b"synthetic-private-do-not-print"
        );
        super::super::secrets::restrict(&path).unwrap();
    }
}

#[test]
fn readonly_preflight_rejects_known_child_junction_without_touching_target() {
    let home = root();
    let target = root();
    fs::write(target.path().join("sentinel"), b"unchanged").unwrap();
    let before = dacl_bytes(target.path());
    let link = home.path().join("accounts");
    junction(&link, target.path());
    let error = check_private_data_home(home.path()).unwrap_err();
    let message = startup_permission_diagnostic(&error).unwrap();
    assert!(message.contains(link.to_str().unwrap()));
    assert_eq!(dacl_bytes(target.path()), before);
    assert_eq!(
        fs::read(target.path().join("sentinel")).unwrap(),
        b"unchanged"
    );
    assert!(!target.path().join(".lock").exists());
}

#[test]
fn typed_account_permission_propagates_but_arbitrary_io_errors_stay_io() {
    let home = root();
    set_dacl(
        home.path(),
        &format!("D:P(A;OICI;FA;;;{})(A;OICI;FR;;;WD)", current_sid()),
    );
    let before = dacl_bytes(home.path());
    let error = super::super::secrets::prepare_account_directory(home.path()).unwrap_err();
    let inner = std::error::Error::source(&error).expect("typed io::Error retained as source");
    let message = startup_permission_diagnostic(inner).unwrap();
    assert!(message.contains(home.path().to_str().unwrap()));
    assert!(message.contains("S-1-1-0"));
    assert_eq!(dacl_bytes(home.path()), before);
    assert!(!home.path().join("accounts").exists());
    for kind in [
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::NotFound,
        io::ErrorKind::Other,
    ] {
        let raw = io::Error::new(kind, "synthetic-private-error");
        assert!(startup_permission_diagnostic(&raw).is_none());
        assert!(matches!(
            map_storage_error(raw),
            crate::accounts::StoreError::Io
        ));
    }
}

#[test]
fn diagnostic_bounds_and_escapes_untrusted_paths_without_raw_errors() {
    let raw = io::Error::new(io::ErrorKind::PermissionDenied, "synthetic-private-error");
    let path = format!("C:\\private\\\n\x1b{}", "x".repeat(4096));
    let error = permission_path(raw, Path::new(&path));
    let message = startup_permission_diagnostic(&error).unwrap();
    assert!(message.contains("C:\\private\\"));
    assert!(!message.contains('\n'));
    assert!(!message.contains('\x1b'));
    assert!(!message.contains("synthetic-private-error"));
    assert!(message.contains("..."));
    assert!(message.len() < 8192);
}

#[test]
fn readonly_preflight_accepts_trusted_system_and_administrators_grants() {
    let home = root();
    set_dacl(
        home.path(),
        &format!(
            "D:P(A;OICI;FA;;;{})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
            current_sid()
        ),
    );
    let before = dacl_bytes(home.path());
    check_private_data_home(home.path()).unwrap();
    assert_eq!(dacl_bytes(home.path()), before);
}

// Junctions do not require Developer Mode or symlink privileges. This test-only
// kernel32 binding avoids adding a System_IO feature solely for fixture setup.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        device: *mut std::ffi::c_void,
        code: u32,
        input: *const u8,
        input_size: u32,
        output: *mut u8,
        output_size: u32,
        returned: *mut u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
}

fn junction(path: &Path, target: &Path) {
    fs::create_dir(path).unwrap();
    let target = fs::canonicalize(target).unwrap();
    let target = target.to_str().unwrap();
    let target = target.strip_prefix(r"\\?\").unwrap_or(target);
    let substitute: Vec<u16> = format!(r"\??\{target}").encode_utf16().collect();
    let print: Vec<u16> = target.encode_utf16().collect();
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&0xa0000003u32.to_le_bytes()); // mount point tag
    let data_size = 8 + 2 * (substitute.len() + 1 + print.len() + 1);
    buffer.extend_from_slice(&(u16::try_from(data_size).unwrap()).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    for value in [
        0,
        substitute.len() * 2,
        (substitute.len() + 1) * 2,
        print.len() * 2,
    ] {
        buffer.extend_from_slice(&u16::try_from(value).unwrap().to_le_bytes());
    }
    for value in substitute.into_iter().chain([0]).chain(print).chain([0]) {
        buffer.extend_from_slice(&value.to_le_bytes());
    }
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let handle = Handle(
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0x4000_0000,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .unwrap(),
        );
        let mut returned = 0;
        assert_ne!(
            DeviceIoControl(
                handle.0.0,
                0x0009_00a4,
                buffer.as_ptr(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut()
            ),
            0,
            "synthetic junction creation failed"
        );
    }
}

#[test]
fn existing_junction_is_refused_without_target_traversal_or_acl_change() {
    let root = root();
    let target = root.path().join("target");
    fs::create_dir(&target).unwrap();
    super::super::secrets::restrict(&target).unwrap();
    fs::write(target.join("record"), b"unchanged").unwrap();
    let before = dacl_bytes(&target);
    let link = root.path().join("link");
    junction(&link, &target);
    assert!(ensure_private_dir_all(&link).is_err());
    assert!(ensure_private_dir_all(&link.join("child")).is_err());
    assert!(!target.join("child").exists());
    assert_eq!(fs::read(target.join("record")).unwrap(), b"unchanged");
    assert_eq!(dacl_bytes(&target), before);
}

#[test]
fn nested_creation_and_idempotence_verify_each_real_acl() {
    let root = root();
    let leaf = root.path().join("state/forecast/epochs");
    ensure_private_dir_all(&leaf).unwrap();
    ensure_private_dir_all(&leaf).unwrap();
    let user = User::current().unwrap();
    for path in [
        root.path().join("state"),
        root.path().join("state/forecast"),
        leaf,
    ] {
        let directory = open_directory(&path).unwrap();
        let security = Security::read(&directory).unwrap();
        security.private(&user).unwrap();
        unsafe {
            assert_eq!((*security.dacl).AceCount, 1);
            let mut ace = std::ptr::null_mut();
            GetAce(security.dacl, 0, &mut ace).unwrap();
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0);
            assert!(user.matches(PSID((&raw const ace.SidStart).cast_mut().cast())));
        }
    }
}

#[test]
fn existing_broad_acl_and_missing_child_are_refused_without_repair() {
    let root = root();
    let state = root.path().join("state");
    fs::create_dir(&state).unwrap();
    set_dacl(&state, "D:P(A;;FA;;;OW)(A;;FR;;;WD)");
    let before = dacl_bytes(&state);
    assert!(ensure_private_dir_all(&state).is_err());
    assert!(ensure_private_dir_all(&state.join("child")).is_err());
    assert!(!state.join("child").exists());
    assert_eq!(dacl_bytes(&state), before);
    let directory = open_directory(&state).unwrap();
    let security = Security::read(&directory).unwrap();
    assert!(security.private(&User::current().unwrap()).is_err());
    unsafe {
        assert_eq!((*security.dacl).AceCount, 2);
    }
}

#[test]
fn existing_file_and_lexical_alias_do_not_create_suffixes() {
    let root = root();
    let file = root.path().join("record");
    fs::write(&file, b"unchanged").unwrap();
    assert!(ensure_private_dir_all(&file).is_err());
    assert!(ensure_private_dir_all(&file.join("child")).is_err());
    assert_eq!(fs::read(&file).unwrap(), b"unchanged");
    assert!(ensure_private_dir_all(&root.path().join("new/../child")).is_err());
    assert!(!root.path().join("new").exists());
}

#[test]
fn concurrent_creation_never_repairs_an_existing_directory() {
    let root = root();
    let leaf = root.path().join("state/child");
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let leaf = leaf.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                ensure_private_dir_all(&leaf)
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(results.iter().any(Result::is_ok));
    let directory = open_directory(&leaf).unwrap();
    Security::read(&directory)
        .unwrap()
        .private(&User::current().unwrap())
        .unwrap();
}
