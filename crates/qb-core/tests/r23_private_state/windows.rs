//! Independent Win32 security oracle for the cross-platform integration tests.
//! Neither privacy assertion calls the production directory-creation helper.
use std::{fs, os::windows::ffi::OsStrExt, path::Path};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
            },
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation, IsValidAcl,
            IsValidSid, IsWellKnownSid, OWNER_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SetFileSecurityW,
            TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid,
            WinCreatorOwnerRightsSid, WinLocalSystemSid, WinWorldSid,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, GetFileInformationByHandle,
            OPEN_EXISTING, READ_CONTROL,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{PCWSTR, PWSTR},
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Allocation(HLOCAL);
impl Drop for Allocation {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(self.0));
        }
    }
}

fn inspect(path: &Path, check: impl FnOnce(*mut ACL, PSID)) {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let directory = Handle(
            CreateFileW(
                PCWSTR(path.as_ptr()),
                READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
            .expect("open synthetic directory security"),
        );
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        GetFileInformationByHandle(directory.0, &mut info).unwrap();
        assert_ne!(info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0, 0);
        assert_eq!(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0, 0);
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).unwrap();
        let token = Handle(token);
        let mut size = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut size);
        assert!(size as usize >= size_of::<TOKEN_USER>());
        let mut user_buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(user_buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
        .unwrap();
        let user = (*user_buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let mut owner = PSID::default();
        let mut dacl = std::ptr::null_mut();
        GetSecurityInfo(
            directory.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut descriptor),
        )
        .ok()
        .unwrap();
        let _descriptor = Allocation(HLOCAL(descriptor.0));
        assert!(!owner.0.is_null() && IsValidSid(owner).as_bool());
        assert!(
            EqualSid(owner, user).is_ok(),
            "directory owner is not the current user"
        );
        assert!(
            !dacl.is_null() && IsValidAcl(dacl).as_bool(),
            "missing or invalid directory DACL"
        );
        check(dacl, user);
    }
}

// Read a plain granting ACE only after checking its layout and SID extent.
unsafe fn allowed_ace(dacl: *mut ACL, index: u32) -> (u32, u8, PSID) {
    unsafe {
        let mut pointer = std::ptr::null_mut();
        GetAce(dacl, index, &mut pointer).unwrap();
        let header = &*pointer.cast::<ACE_HEADER>();
        assert_eq!(header.AceType, 0, "unexpected granting ACE form");
        let offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        assert!(usize::from(header.AceSize) >= offset + 8);
        let sid = pointer.cast::<u8>().add(offset);
        assert!(offset + 8 + 4 * usize::from(*sid.add(1)) <= usize::from(header.AceSize));
        let sid = PSID(sid.cast());
        assert!(IsValidSid(sid).as_bool());
        let ace = &*pointer.cast::<ACCESS_ALLOWED_ACE>();
        (ace.Mask, header.AceFlags, sid)
    }
}

pub(super) fn assert_private(path: &Path) {
    inspect(path, |dacl, user| unsafe {
        assert!(
            (*dacl).AceCount > 0,
            "directory must grant its owner access"
        );
        for index in 0..u32::from((*dacl).AceCount) {
            let (_, _, sid) = allowed_ace(dacl, index);
            assert!(
                EqualSid(sid, user).is_ok()
                    || IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
                    || IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool(),
                "directory ACL grants an untrusted principal access"
            );
        }
    });
}

pub(super) fn create_loose(path: &Path) {
    fs::create_dir(path).unwrap();
    // OW is OWNER RIGHTS, not a captured/logged user SID. Everyone gets read
    // access only; keep owner access so the fixture can inspect and clean up.
    set_dacl(path, "D:P(A;;FA;;;OW)(A;;FR;;;WD)");
    assert_loose(path);
}

fn set_dacl(path: &Path, sddl: &str) {
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
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
            PCWSTR(wide_path.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
        .ok()
        .unwrap();
    }
}

/// Only for the common test's freshly created, empty synthetic home.
pub(super) fn prepare_home(path: &Path) {
    assert!(
        fs::read_dir(path).unwrap().next().is_none(),
        "fixture home must be new and empty"
    );
    let mut sddl = String::new();
    inspect(path, |_, user| unsafe {
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user, &mut sid).unwrap();
        let _sid = Allocation(HLOCAL(sid.0.cast()));
        sddl = format!("D:P(A;;FA;;;{})", sid.to_string().unwrap());
    });
    set_dacl(path, &sddl);
    assert_private(path);
}

pub(super) fn assert_loose(path: &Path) {
    inspect(path, |dacl, _| unsafe {
        assert_eq!((*dacl).AceCount, 2, "loose fixture DACL changed");
        let (mask, flags, sid) = allowed_ace(dacl, 0);
        assert_eq!(mask, FILE_ALL_ACCESS.0);
        assert_eq!(flags, 0);
        assert!(IsWellKnownSid(sid, WinCreatorOwnerRightsSid).as_bool());
        let (mask, flags, sid) = allowed_ace(dacl, 1);
        assert_eq!(mask, FILE_GENERIC_READ.0);
        assert_eq!(flags, 0);
        assert!(
            IsWellKnownSid(sid, WinWorldSid).as_bool(),
            "Everyone read grant was changed"
        );
    });
}
