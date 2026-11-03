use std::{io, os::windows::ffi::OsStrExt};
use tempfile::TempDir;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
            },
            DACL_SECURITY_INFORMATION, EqualSid, GetTokenInformation, IsValidSid,
            OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            PSID, SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{PCWSTR, PWSTR},
};

struct Token(HANDLE);
impl Drop for Token {
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

fn owner_matches(path: &[u16], user: PSID) -> io::Result<bool> {
    unsafe {
        let mut owner = PSID::default();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        GetNamedSecurityInfoW(
            PCWSTR(path.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut descriptor,
        )
        .ok()
        .map_err(io::Error::other)?;
        let _descriptor = Allocation(HLOCAL(descriptor.0));
        if owner.0.is_null() || !IsValidSid(owner).as_bool() {
            return Err(io::Error::other("invalid fixture directory owner"));
        }
        Ok(EqualSid(owner, user).is_ok())
    }
}

/// Restrict only the freshly created empty TempDir checked by fixture code.
/// Unlike product directory creation, this never publishes user data.
pub(crate) fn prepare_new_fixture_directory(home: &TempDir) -> io::Result<()> {
    unsafe {
        let mut handle = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle)
            .map_err(io::Error::other)?;
        let token = Token(handle);
        let mut size = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut size);
        if size < size_of::<TOKEN_USER>() as u32 {
            return Err(io::Error::other("cannot determine fixture user token size"));
        }
        let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
        .map_err(io::Error::other)?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        if !IsValidSid(user.User.Sid).as_bool() {
            return Err(io::Error::other("invalid fixture user SID"));
        }
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid).map_err(io::Error::other)?;
        let _sid = Allocation(HLOCAL(sid.0.cast()));
        let sddl = format!(
            "O:{sid}D:P(A;OICI;FA;;;{sid})",
            sid = sid.to_string().map_err(io::Error::other)?
        );
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(io::Error::other)?;
        let _descriptor = Allocation(HLOCAL(descriptor.0));
        let path: Vec<u16> = home
            .path()
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let already_owned = owner_matches(&path, user.User.Sid)?;
        // An owner has WRITE_DAC even when its inherited Modify grant lacks
        // WRITE_OWNER. Do not request an unnecessary owner rewrite. If the
        // default owner differs, grant full access before changing that owner.
        SetFileSecurityW(
            PCWSTR(path.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
        .ok()
        .map_err(io::Error::other)?;
        if !already_owned {
            SetFileSecurityW(
                PCWSTR(path.as_ptr()),
                OWNER_SECURITY_INFORMATION,
                descriptor,
            )
            .ok()
            .map_err(io::Error::other)?;
        }
        if !owner_matches(&path, user.User.Sid)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "fixture owner must be the current user",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use windows::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL,
        Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
        EqualSid, GetAce, GetSecurityDescriptorControl, IsValidAcl, IsValidSid, PSID,
        SE_DACL_PROTECTED,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, WRITE_OWNER,
    };

    fn dacl(path: &Path) -> Vec<u8> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            GetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut descriptor,
            )
            .ok()
            .unwrap();
            let _descriptor = Allocation(HLOCAL(descriptor.0));
            assert!(!dacl.is_null() && IsValidAcl(dacl).as_bool());
            std::slice::from_raw_parts(dacl.cast::<u8>(), usize::from((*dacl).AclSize)).to_vec()
        }
    }

    fn assert_owner_only(path: &Path) {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            let mut owner = PSID::default();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            GetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                Some(&mut dacl),
                None,
                &mut descriptor,
            )
            .ok()
            .unwrap();
            let _descriptor = Allocation(HLOCAL(descriptor.0));
            assert!(!dacl.is_null() && IsValidAcl(dacl).as_bool());
            let mut control = 0u16;
            let mut revision = 0;
            GetSecurityDescriptorControl(descriptor, &mut control, &mut revision).unwrap();
            assert_ne!(control & SE_DACL_PROTECTED.0, 0);
            assert_eq!((*dacl).AceCount, 1);
            let mut ace = std::ptr::null_mut();
            GetAce(dacl, 0, &mut ace).unwrap();
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0);
            assert_eq!(ace.Header.AceFlags, 0x03); // object/container inheritance
            assert_eq!(ace.Mask, 0x001f01ff); // FILE_ALL_ACCESS
            let sid = PSID((&raw const ace.SidStart).cast_mut().cast());
            assert!(IsValidSid(sid).as_bool());
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).unwrap();
            let token = Token(token);
            let mut size = 0;
            let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut size);
            let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                size,
                &mut size,
            )
            .unwrap();
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            assert!(
                EqualSid(owner, user.User.Sid).is_ok(),
                "fixture owner must be the current user"
            );
            assert!(
                EqualSid(sid, user.User.Sid).is_ok(),
                "fixture ACL must name only the current user"
            );
        }
    }

    #[test]
    fn fresh_fixture_and_builder_leaf_have_protected_user_acl_without_parent_changes() {
        let parent = tempfile::tempdir().unwrap();
        let before = dacl(parent.path());
        let ordinary = crate::fixture::tempdir_in(parent.path()).unwrap();
        let builder = tempfile::Builder::new()
            .prefix("case-")
            .tempdir_in(parent.path().canonicalize().unwrap())
            .unwrap();
        let original_path = builder.path().to_owned();
        let builder = crate::fixture::prepare_tempdir(builder).unwrap();
        assert_eq!(builder.path(), original_path);
        assert_owner_only(ordinary.path());
        assert_owner_only(builder.path());
        assert_eq!(dacl(parent.path()), before);
    }

    #[test]
    fn fresh_modify_inherited_child_does_not_require_write_owner() {
        let parent = crate::fixture::tempdir_in(std::env::temp_dir()).unwrap();
        let parent_path: Vec<u16> = parent
            .path()
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe {
            let mut owner = PSID::default();
            let mut original = PSECURITY_DESCRIPTOR::default();
            GetNamedSecurityInfoW(
                PCWSTR(parent_path.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                None,
                None,
                &mut original,
            )
            .ok()
            .unwrap();
            let _original = Allocation(HLOCAL(original.0));
            let mut sid = PWSTR::null();
            ConvertSidToStringSidW(owner, &mut sid).unwrap();
            let _sid = Allocation(HLOCAL(sid.0.cast()));
            // Current-user Modify, inherited by children, excludes WRITE_OWNER.
            let sddl = format!("D:P(A;OICI;0x1301bf;;;{})", sid.to_string().unwrap());
            let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
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
                PCWSTR(parent_path.as_ptr()),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
            .ok()
            .unwrap();
            let parent_before = dacl(parent.path());
            let child = tempfile::tempdir_in(parent.path()).unwrap();
            let child_path: Vec<u16> = child
                .path()
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect();
            assert!(owner_matches(&child_path, owner).unwrap());
            let error = match CreateFileW(
                PCWSTR(child_path.as_ptr()),
                WRITE_OWNER.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            ) {
                Err(error) => error,
                Ok(handle) => {
                    CloseHandle(handle).unwrap();
                    panic!("Modify-inherited fixture unexpectedly allows WRITE_OWNER");
                }
            };
            assert_eq!(error.code().0 as u32, 0x80070005);
            let child = crate::fixture::prepare_tempdir(child).unwrap();
            assert_owner_only(child.path());
            assert_eq!(std::fs::read_dir(child.path()).unwrap().count(), 0);
            assert_eq!(dacl(parent.path()), parent_before);
        }
    }

    #[test]
    fn populated_fixture_retains_its_existing_acl_and_contents() {
        let parent = tempfile::tempdir().unwrap();
        let populated = tempfile::tempdir_in(parent.path().canonicalize().unwrap()).unwrap();
        let path = populated.path().to_owned();
        std::fs::write(path.join("keep"), b"synthetic-owned-data").unwrap();
        let before = dacl(&path);
        let error = crate::fixture::prepare_tempdir(populated).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(dacl(&path), before);
        assert_eq!(
            std::fs::read(path.join("keep")).unwrap(),
            b"synthetic-owned-data"
        );
    }
}
