use std::{io, os::windows::ffi::OsStrExt};
use tempfile::TempDir;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
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
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid).map_err(io::Error::other)?;
        let _sid = Allocation(HLOCAL(sid.0.cast()));
        let sddl = format!(
            "D:P(A;;FA;;;{})",
            sid.to_string().map_err(io::Error::other)?
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
        SetFileSecurityW(
            PCWSTR(path.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
        .ok()
        .map_err(io::Error::other)?;
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
            let mut control = 0u16;
            let mut revision = 0;
            GetSecurityDescriptorControl(descriptor, &mut control, &mut revision).unwrap();
            assert_ne!(control & SE_DACL_PROTECTED.0, 0);
            assert_eq!((*dacl).AceCount, 1);
            let mut ace = std::ptr::null_mut();
            GetAce(dacl, 0, &mut ace).unwrap();
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0);
            assert_eq!(ace.Header.AceFlags, 0);
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
            .tempdir_in(parent.path())
            .unwrap();
        let original_path = builder.path().to_owned();
        let builder = crate::fixture::prepare_tempdir(builder).unwrap();
        assert_eq!(builder.path(), original_path);
        assert_owner_only(ordinary.path());
        assert_owner_only(builder.path());
        assert_eq!(dacl(parent.path()), before);
    }
}
