//! Data-directory creation without ACL repair of existing paths.
//!
//! Ancestors are opened one component at a time without following reparse
//! points. Retained handles deny write/delete sharing while path-based creation
//! supplies a protected owner-only ACL at birth. This is not an NT relative-handle mkdir:
//! callers must keep the drive mapping and trusted principals stable.
#[cfg(test)]
use std::fs;
use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::{Component, Path, PathBuf, Prefix},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
            },
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid, GetTokenInformation,
            IsValidAcl, IsValidSid, IsWellKnownSid, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
            WinBuiltinAdministratorsSid, WinLocalSystemSid,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_READ_ATTRIBUTES, FILE_SHARE_READ, GetFileInformationByHandle, OPEN_EXISTING,
            READ_CONTROL,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{PCWSTR, PWSTR},
};

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "data directory is not private or has an unsupported security descriptor; no existing ACL was changed",
    )
}

fn win_error(error: windows::core::Error) -> io::Error {
    let code = error.code().0 as u32;
    if code & 0xffff_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xffff) as i32)
    } else {
        io::Error::other(error)
    }
}

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

struct User {
    // TOKEN_USER and its inline SID need pointer alignment and a stable buffer.
    _token: Handle,
    buffer: Vec<usize>,
}

impl User {
    fn current() -> io::Result<Self> {
        unsafe {
            let mut handle = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle).map_err(win_error)?;
            let token = Handle(handle);
            let mut size = 0;
            let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut size);
            if size < size_of::<TOKEN_USER>() as u32 {
                return Err(refused());
            }
            let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                size,
                &mut size,
            )
            .map_err(win_error)?;
            let user = Self {
                _token: token,
                buffer,
            };
            if !IsValidSid(user.sid()).as_bool() {
                return Err(refused());
            }
            Ok(user)
        }
    }

    fn sid(&self) -> PSID {
        unsafe { (*self.buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn matches(&self, sid: PSID) -> bool {
        unsafe { EqualSid(self.sid(), sid).is_ok() }
    }

    fn trusted(&self, sid: PSID) -> bool {
        self.matches(sid)
            || unsafe {
                IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
                    || IsWellKnownSid(sid, WinBuiltinAdministratorsSid).as_bool()
            }
    }
}

struct Security {
    _descriptor: Allocation,
    owner: PSID,
    dacl: *mut ACL,
}

impl Security {
    fn read(handle: &Handle) -> io::Result<Self> {
        unsafe {
            let mut owner = PSID::default();
            let mut dacl = std::ptr::null_mut();
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            GetSecurityInfo(
                handle.0,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                Some(&mut dacl),
                None,
                Some(&mut descriptor),
            )
            .ok()
            .map_err(win_error)?;
            let security = Self {
                _descriptor: Allocation(HLOCAL(descriptor.0)),
                owner,
                dacl,
            };
            if owner.0.is_null()
                || dacl.is_null()
                || !IsValidSid(owner).as_bool()
                || !IsValidAcl(dacl).as_bool()
            {
                return Err(refused());
            }
            Ok(security)
        }
    }

    fn private(&self, user: &User) -> io::Result<()> {
        if !user.matches(self.owner) {
            return Err(refused());
        }
        // Reject NULL DACLs and unknown granting ACE forms rather than guessing
        // about object, callback or conditional access. Deny ACEs grant nothing.
        unsafe {
            for index in 0..u32::from((*self.dacl).AceCount) {
                let mut pointer = std::ptr::null_mut();
                GetAce(self.dacl, index, &mut pointer).map_err(win_error)?;
                let header = &*pointer.cast::<ACE_HEADER>();
                match header.AceType {
                    1 => continue, // ACCESS_DENIED_ACE_TYPE
                    0 => {}        // ACCESS_ALLOWED_ACE_TYPE
                    _ => return Err(refused()),
                }
                let offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
                let length = usize::from(header.AceSize);
                if length < offset + 8 {
                    return Err(refused());
                }
                let sid = pointer.cast::<u8>().add(offset);
                let sid_length = 8 + 4 * usize::from(*sid.add(1));
                if sid_length > length - offset {
                    return Err(refused());
                }
                let sid = PSID(sid.cast());
                // Also check inherit-only grants: they may expose descendants.
                if !IsValidSid(sid).as_bool()
                    || GetLengthSid(sid) as usize != sid_length
                    || !user.trusted(sid)
                {
                    return Err(refused());
                }
            }
        }
        Ok(())
    }
}

fn open_directory(path: &Path) -> io::Result<Handle> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_READ_ATTRIBUTES.0 | READ_CONTROL.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(win_error)?;
    let handle = Handle(handle);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(handle.0, &mut info).map_err(win_error)?;
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err(refused());
    }
    Ok(handle)
}

fn absolute_components(path: &Path) -> io::Result<Vec<PathBuf>> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut components = absolute.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(refused());
    };
    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        || !matches!(components.next(), Some(Component::RootDir))
    {
        return Err(refused());
    }
    let mut current = PathBuf::from(prefix.as_os_str());
    current.push("\\");
    let mut paths = vec![current.clone()];
    for component in components {
        let Component::Normal(name) = component else {
            return Err(refused());
        };
        let wide: Vec<u16> = name.encode_wide().collect();
        // Reject stream names and Win32-normalized trailing aliases up front.
        if wide.iter().any(|c| *c == 0 || *c == u16::from(b':'))
            || matches!(wide.last(), Some(32 | 46))
        {
            return Err(refused());
        }
        current.push(name);
        paths.push(current.clone());
    }
    Ok(paths)
}

/// Create exactly one directory with a protected current-user DACL at birth.
/// CreateDirectoryW's failure (including AlreadyExists) never adopts a winner.
fn create_leaf(path: &Path, user: &User) -> io::Result<Handle> {
    unsafe {
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.sid(), &mut sid).map_err(win_error)?;
        let _sid = Allocation(HLOCAL(sid.0.cast()));
        let sid = sid.to_string().map_err(io::Error::other)?;
        // Explicit owner also avoids an elevated token's Administrators default
        // owner. Protected DACL prevents inherited/default grants at creation.
        let sddl = format!("O:{sid}D:P(A;;FA;;;{sid})");
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(win_error)?;
        let _descriptor = Allocation(HLOCAL(descriptor.0));
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        CreateDirectoryW(PCWSTR(wide_path.as_ptr()), Some(&attributes)).map_err(win_error)?;
    }
    let directory = open_directory(path)?;
    Security::read(&directory)?.private(user)?;
    Ok(directory)
}

/// Exclusive leaf-only variant; parent components must already exist. Hold
/// no-reparse ancestor handles through creation just as the recursive variant.
pub(super) fn create_new_private_directory(path: &Path) -> io::Result<()> {
    let paths = absolute_components(path)?;
    let (leaf, parents) = paths.split_last().ok_or_else(refused)?;
    let _anchors = parents
        .iter()
        .map(|parent| open_directory(parent))
        .collect::<io::Result<Vec<_>>>()?;
    let user = User::current()?;
    let _directory = create_leaf(leaf, &user)?;
    Ok(())
}

/// Existing leaf/nearest parent must already be private. Every new suffix
/// component is private from birth; an AlreadyExists race never repairs ACLs.
/// Errors may leave empty private prefixes, validated rather than repaired later.
pub(crate) fn ensure_private_dir_all(path: &Path) -> io::Result<()> {
    let paths = absolute_components(path)?;
    let user = User::current()?;
    let mut anchors = Vec::with_capacity(paths.len());
    let mut new_suffix = false;
    for path in paths {
        let directory = match open_directory(&path) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = anchors.last().ok_or_else(refused)?;
                Security::read(parent)?.private(&user)?;
                let directory = create_leaf(&path, &user)?;
                new_suffix = true;
                directory
            }
            Err(error) => return Err(error),
        };
        if new_suffix {
            Security::read(&directory)?.private(&user)?;
        }
        anchors.push(directory);
    }
    Security::read(anchors.last().ok_or_else(refused)?)?.private(&user)
}

#[cfg(test)]
mod tests;
