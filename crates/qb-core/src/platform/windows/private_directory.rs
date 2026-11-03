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

#[derive(Clone, Debug)]
struct PermissionRefusal {
    path: String,
    untrusted_sids: Vec<String>,
    more: bool,
}

impl std::fmt::Display for PermissionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "data directory is not private at '{}'; untrusted principals: ",
            self.path
        )?;
        if self.untrusted_sids.is_empty() {
            write!(
                f,
                "unavailable (ownership, access, reparse point or unsupported ACL)"
            )?;
        } else {
            write!(f, "{}", self.untrusted_sids.join(", "))?;
        }
        if self.more {
            write!(f, ", ... (additional principals omitted)")?;
        }
        write!(
            f,
            "; run the updated installer, or manually protect the DACL granting only the owner, SYSTEM and Administrators; no existing ACL was changed"
        )
    }
}

impl std::error::Error for PermissionRefusal {}

pub(crate) fn startup_permission_diagnostic(
    error: &(dyn std::error::Error + 'static),
) -> Option<String> {
    let refusal = error.downcast_ref::<PermissionRefusal>().or_else(|| {
        error
            .downcast_ref::<io::Error>()?
            .get_ref()?
            .downcast_ref::<PermissionRefusal>()
    })?;
    Some(refusal.to_string())
}

pub(super) fn map_storage_error(error: io::Error) -> crate::accounts::StoreError {
    if startup_permission_diagnostic(&error).is_some() {
        crate::accounts::StoreError::StoragePermission(error)
    } else {
        crate::accounts::StoreError::Io
    }
}

fn permission_path(error: io::Error, path: &Path) -> io::Error {
    if error.kind() != io::ErrorKind::PermissionDenied {
        return error;
    }
    let mut refusal = match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<PermissionRefusal>())
    {
        Some(refusal) if !refusal.path.is_empty() => return error,
        Some(refusal) => refusal.clone(),
        None => PermissionRefusal {
            path: String::new(),
            untrusted_sids: Vec::new(),
            more: false,
        },
    };
    refusal.path = path
        .to_string_lossy()
        .chars()
        .take(256)
        .map(|ch| {
            if ch == '\\' {
                ch.to_string()
            } else {
                ch.escape_debug().to_string()
            }
        })
        .collect();
    if path.to_string_lossy().chars().count() > 256 {
        refusal.path.push_str("...");
    }
    io::Error::new(io::ErrorKind::PermissionDenied, refusal)
}

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
    // This second, metadata-only pass explains an already-refused descriptor.
    // It never participates in authorization or exposes raw descriptor bytes.
    fn refusal(&self, user: &User) -> io::Error {
        let mut refusal = PermissionRefusal {
            path: String::new(),
            untrusted_sids: Vec::new(),
            more: false,
        };
        let mut add_sid = |sid: PSID| unsafe {
            if !IsValidSid(sid).as_bool() {
                return;
            }
            let mut text = PWSTR::null();
            if ConvertSidToStringSidW(sid, &mut text).is_err() {
                return;
            }
            let _allocation = Allocation(HLOCAL(text.0.cast()));
            let Ok(text) = text.to_string() else {
                return;
            };
            let text: String = text
                .chars()
                .take(192)
                .filter(|ch| ch.is_ascii_digit() || *ch == 'S' || *ch == '-')
                .collect();
            if refusal.untrusted_sids.contains(&text) {
                return;
            }
            if refusal.untrusted_sids.len() < 16 {
                refusal.untrusted_sids.push(text);
            } else {
                refusal.more = true;
            }
        };
        if !user.matches(self.owner) {
            add_sid(self.owner);
        }
        unsafe {
            for index in 0..u32::from((*self.dacl).AceCount) {
                let mut pointer = std::ptr::null_mut();
                if GetAce(self.dacl, index, &mut pointer).is_err() {
                    break;
                }
                let header = &*pointer.cast::<ACE_HEADER>();
                if header.AceType != 0 {
                    continue;
                }
                let offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
                let length = usize::from(header.AceSize);
                if length < offset + 8 {
                    continue;
                }
                let sid = pointer.cast::<u8>().add(offset);
                let sid_length = 8 + 4 * usize::from(*sid.add(1));
                if sid_length > length - offset {
                    continue;
                }
                let sid = PSID(sid.cast());
                if IsValidSid(sid).as_bool()
                    && GetLengthSid(sid) as usize == sid_length
                    && !user.trusted(sid)
                {
                    add_sid(sid);
                }
            }
        }
        io::Error::new(io::ErrorKind::PermissionDenied, refusal)
    }

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
            return Err(self.refusal(user));
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
                    _ => return Err(self.refusal(user)),
                }
                let offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
                let length = usize::from(header.AceSize);
                if length < offset + 8 {
                    return Err(self.refusal(user));
                }
                let sid = pointer.cast::<u8>().add(offset);
                let sid_length = 8 + 4 * usize::from(*sid.add(1));
                if sid_length > length - offset {
                    return Err(self.refusal(user));
                }
                let sid = PSID(sid.cast());
                // Also check inherit-only grants: they may expose descendants.
                if !IsValidSid(sid).as_bool()
                    || GetLengthSid(sid) as usize != sid_length
                    || !user.trusted(sid)
                {
                    return Err(self.refusal(user));
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

fn check_security(directory: &Handle, user: &User, path: &Path) -> io::Result<()> {
    Security::read(directory)
        .and_then(|security| security.private(user))
        .map_err(|error| permission_path(error, path))
}

/// Inspect existing directories only. Keep no-reparse ancestor handles alive
/// across the probe, without creating directories, opening a lease or repairing ACLs.
pub(crate) fn check_private_data_home(home: &Path) -> io::Result<()> {
    let paths = absolute_components(home).map_err(|error| permission_path(error, home))?;
    let user = User::current().map_err(|error| permission_path(error, home))?;
    let anchors = paths
        .iter()
        .map(|path| open_directory(path).map_err(|error| permission_path(error, path)))
        .collect::<io::Result<Vec<_>>>()?;
    check_security(anchors.last().ok_or_else(refused)?, &user, home)?;
    let mut children = Vec::new();
    for name in ["accounts", "state", "logs"] {
        let path = home.join(name);
        let directory = match open_directory(&path) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(permission_path(error, &path)),
        };
        check_security(&directory, &user, &path)?;
        children.push(directory);
    }
    Ok(())
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
    let paths = absolute_components(path).map_err(|error| permission_path(error, path))?;
    let (leaf, parents) = paths.split_last().ok_or_else(refused)?;
    let _anchors = parents
        .iter()
        .map(|parent| open_directory(parent).map_err(|error| permission_path(error, parent)))
        .collect::<io::Result<Vec<_>>>()?;
    let user = User::current().map_err(|error| permission_path(error, path))?;
    let _directory = create_leaf(leaf, &user).map_err(|error| permission_path(error, leaf))?;
    Ok(())
}

/// Existing leaf/nearest parent must already be private. Every new suffix
/// component is private from birth; an AlreadyExists race never repairs ACLs.
/// Errors may leave empty private prefixes, validated rather than repaired later.
pub(crate) fn ensure_private_dir_all(path: &Path) -> io::Result<()> {
    let paths = absolute_components(path).map_err(|error| permission_path(error, path))?;
    let user = User::current().map_err(|error| permission_path(error, path))?;
    let mut anchors = Vec::with_capacity(paths.len());
    let mut new_suffix = false;
    let mut last_path = path.to_owned();
    for path in paths {
        let directory = match open_directory(&path) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = anchors.last().ok_or_else(refused)?;
                check_security(parent, &user, &last_path)?;
                let directory =
                    create_leaf(&path, &user).map_err(|error| permission_path(error, &path))?;
                new_suffix = true;
                directory
            }
            Err(error) => return Err(permission_path(error, &path)),
        };
        if new_suffix {
            check_security(&directory, &user, &path)?;
        }
        anchors.push(directory);
        last_path = path;
    }
    check_security(anchors.last().ok_or_else(refused)?, &user, &last_path)
}

#[cfg(test)]
mod tests;
