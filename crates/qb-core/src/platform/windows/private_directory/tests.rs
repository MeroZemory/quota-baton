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
