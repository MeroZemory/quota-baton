//! Shared Unix private filesystem and key policy.
//! All access after the initial directory walk is relative to checked descriptors.
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path},
};
use zeroize::Zeroizing;

/// Only descriptor metadata and a bounded, escaped path may cross the CLI's
/// diagnostic boundary. Never include file contents or arbitrary error strings.
#[derive(Clone, Debug)]
struct PermissionRefusal {
    path: String,
    mode: Option<u32>,
    reason: &'static str,
    repair: &'static str,
}

impl std::fmt::Display for PermissionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "permission refusal at '{}': {}", self.path, self.reason)?;
        if let Some(mode) = self.mode {
            write!(f, "; found mode {mode:04o}")?;
        }
        write!(f, "; {}", self.repair)
    }
}

impl std::error::Error for PermissionRefusal {}

pub(super) fn map_storage_error(
    error: io::Error,
    fallback: crate::accounts::StoreError,
) -> crate::accounts::StoreError {
    if startup_permission_diagnostic(&error).is_some() {
        crate::accounts::StoreError::StoragePermission(error)
    } else {
        fallback
    }
}

pub(super) fn credential_permission(
    error: io::Error,
    path: &Path,
    meta: Option<&std::fs::Metadata>,
) -> io::Error {
    if error.kind() != io::ErrorKind::PermissionDenied {
        return error;
    }
    let error = permission_path(error, path);
    let mut refusal = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<PermissionRefusal>())
        .expect("permission_path produces a typed refusal")
        .clone();
    refusal.mode = meta.map(|meta| meta.mode() & 0o7777);
    refusal.reason = "systemd credential ownership, mode, ACL or read-only mount policy refused";
    refusal.repair = "reinstall the credential with LoadCredentialEncrypted=qb-master-key and restart the service; do not change permissions on the read-only credential";
    io::Error::new(io::ErrorKind::PermissionDenied, refusal)
}

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

fn permission_refusal(meta: &std::fs::Metadata, reason: &'static str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        PermissionRefusal {
            path: "<private entry>".into(),
            mode: Some(meta.mode() & 0o7777),
            reason,
            repair: if meta.is_dir() {
                "verify ownership and directory type; use chmod 700 on this directory; no permissions were changed"
            } else {
                "verify ownership, regular file type and single link; recreate with install -m 0600; no permissions were changed"
            },
        },
    )
}

pub(super) fn permission_path(error: io::Error, path: &Path) -> io::Error {
    if error.kind() != io::ErrorKind::PermissionDenied {
        return error;
    }
    let mut refusal = match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<PermissionRefusal>())
    {
        Some(refusal) if refusal.path != "<private entry>" => return error,
        Some(refusal) => refusal.clone(),
        None => PermissionRefusal {
            path: String::new(),
            mode: None,
            reason: "access refused or unsafe path",
            repair: "verify ownership, permissions and path components; no permissions were changed",
        },
    };
    // Bound before escaping, so even hostile names cannot emit unbounded logs
    // or inject terminal/journal control characters.
    refusal.path = path
        .to_string_lossy()
        .chars()
        .take(256)
        .flat_map(char::escape_default)
        .collect();
    if path.to_string_lossy().chars().count() > 256 {
        refusal.path.push_str("...");
    }
    io::Error::new(io::ErrorKind::PermissionDenied, refusal)
}

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe account key storage",
    )
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid account master key")
}
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn owner_ok(owner: u32, allow_root: bool) -> bool {
    owner == uid() || (allow_root && owner == 0)
}
fn trusted_directory(file: &File) -> io::Result<()> {
    let meta = file.metadata()?;
    if !trusted_ancestor_policy(meta.is_dir(), meta.uid(), uid(), meta.mode()) {
        return Err(permission_refusal(
            &meta,
            "untrusted ancestor: expected root or current owner and no group/world write",
        ));
    }
    Ok(())
}

// A readonly alias does not establish backing namespace integrity. Never use
// mount flags to authorize a group/world-writable ancestor.
fn trusted_ancestor_policy(is_directory: bool, owner: u32, effective_uid: u32, mode: u32) -> bool {
    is_directory && (owner == 0 || owner == effective_uid) && mode & 0o022 == 0
}

fn private(file: &File, allow_root: bool) -> io::Result<()> {
    let meta = file.metadata()?;
    if !meta.is_dir()
        || !owner_ok(meta.uid(), allow_root)
        || meta.mode() & 0o077 != 0
        || meta.mode() & 0o7000 != 0
        || meta.mode() & 0o500 != 0o500
    {
        return Err(permission_refusal(
            &meta,
            "expected private directory owned by the current user (or permitted root owner)",
        ));
    }
    Ok(())
}

/// Caller supplies a prepared private directory. Relative paths are anchored to cwd
/// once; parent traversal, symlinks and group/world-writable ancestors are refused.
pub(super) fn private_directory(path: &Path, allow_root: bool) -> io::Result<File> {
    walk_directory(path, false, allow_root).map_err(|error| permission_path(error, path))
}

pub(crate) fn check_private_data_home(path: &Path) -> io::Result<()> {
    let mut anchors = vec![private_directory(path, false)?];
    for name in ["accounts", "state", "logs"] {
        let child = path.join(name);
        match private_directory(&child, false) {
            Ok(directory) => anchors.push(directory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) fn prepare_private_directory(path: &Path) -> io::Result<File> {
    walk_directory(path, true, false).map_err(|error| permission_path(error, path))
}

/// Create a private directory tree without following aliases or repairing modes.
/// Existing system ancestors may be readable, but must remain trusted. Once a
/// missing component is reached, every component in the new suffix is private.
pub(crate) fn ensure_private_dir_all(path: &Path) -> io::Result<()> {
    ensure_private_tree(path).map_err(|error| permission_path(error, path))
}

fn ensure_private_tree(path: &Path) -> io::Result<()> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    let mut checked_path = std::path::PathBuf::from("/");
    trusted_directory(&directory).map_err(|error| permission_path(error, &checked_path))?;
    let mut new_suffix = false;
    for component in absolute.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let child_path = checked_path.join(name);
                let name = CString::new(name.as_bytes()).map_err(|_| refused())?;
                directory = match open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                {
                    Ok(child) => child,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        // An existing data directory above a missing descendant
                        // must already be private; never normalize it implicitly.
                        private(&directory, false)
                            .map_err(|error| permission_path(error, &checked_path))?;
                        new_suffix = true;
                        mkdir_private(&directory, &name)
                            .map_err(|error| permission_path(error, &child_path))?
                    }
                    Err(error) => return Err(permission_path(error, &child_path)),
                };
                checked_path = child_path;
                trusted_directory(&directory)
                    .map_err(|error| permission_path(error, &checked_path))?;
                if new_suffix {
                    private(&directory, false)
                        .map_err(|error| permission_path(error, &checked_path))?;
                }
            }
            _ => return Err(refused()),
        }
    }
    private(&directory, false).map_err(|error| permission_path(error, &checked_path))
}

fn walk_directory(path: &Path, create: bool, allow_root: bool) -> io::Result<File> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    let mut checked_path = std::path::PathBuf::from("/");
    trusted_directory(&directory).map_err(|error| permission_path(error, &checked_path))?;
    for component in absolute.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                checked_path.push(name);
                let name = CString::new(name.as_bytes()).map_err(|_| refused())?;
                directory = match open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                {
                    Ok(child) => child,
                    Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                        mkdir_private(&directory, &name)
                            .map_err(|error| permission_path(error, &checked_path))?
                    }
                    Err(error) => return Err(permission_path(error, &checked_path)),
                };
                trusted_directory(&directory)
                    .map_err(|error| permission_path(error, &checked_path))?;
            }
            _ => return Err(refused()),
        }
    }
    private(&directory, allow_root).map_err(|error| permission_path(error, path))?;
    Ok(directory)
}

fn mkdir_private(parent: &File, name: &CStr) -> io::Result<File> {
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if created != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    let child = open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    private(&child, false)?;
    if created == 0 {
        parent.sync_all()?;
    }
    Ok(child)
}

pub(super) fn private_child(parent: &File, name: &str, allow_root: bool) -> io::Result<File> {
    let name = name_cstr(name)?;
    let child = open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    private(&child, allow_root)?;
    Ok(child)
}

pub(super) fn prepare_private_child(parent: &File, name: &str) -> io::Result<File> {
    let name = name_cstr(name)?;
    match open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
        Ok(child) => {
            private(&child, false)?;
            Ok(child)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => mkdir_private(parent, &name),
        Err(error) => Err(error),
    }
}

pub(super) fn parent_and_name(path: &Path) -> io::Result<(File, CString)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(refused)?;
    let name = CString::new(name.as_bytes()).map_err(|_| refused())?;
    if name.as_bytes() == b"." || name.as_bytes() == b".." {
        return Err(refused());
    }
    Ok((private_directory(parent, false)?, name))
}

fn name_cstr(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(refused());
    }
    CString::new(name).map_err(|_| refused())
}

fn open_at(
    directory: &File,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            mode,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn checked_file(
    directory: &File,
    name: &CStr,
    flags: libc::c_int,
    allow_root: bool,
) -> io::Result<File> {
    let file = open_at(directory, name, flags, 0)?;
    check_file(&file, allow_root)?;
    Ok(file)
}

fn check_file(file: &File, allow_root: bool) -> io::Result<()> {
    let meta = file.metadata()?;
    if !meta.is_file()
        || !owner_ok(meta.uid(), allow_root)
        || meta.nlink() != 1
        || meta.mode() & 0o077 != 0
        || meta.mode() & 0o7111 != 0
        || meta.mode() & 0o400 == 0
    {
        return Err(permission_refusal(
            &meta,
            "expected private regular file owned by the current user (or permitted root owner), with one link",
        ));
    }
    Ok(())
}

pub(super) fn validate_leaf(directory: &File, name: &CStr) -> io::Result<()> {
    checked_file(directory, name, libc::O_RDONLY, false).map(|_| ())
}

fn bounded_read(file: File, max: usize) -> io::Result<Zeroizing<Vec<u8>>> {
    let limit = max
        .checked_add(1)
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(invalid)?;
    if file.metadata()?.len() > max as u64 {
        return Err(invalid());
    }
    let mut result = Zeroizing::new(Vec::new());
    file.take(limit).read_to_end(&mut result)?;
    if result.len() > max {
        return Err(invalid());
    }
    Ok(result)
}

pub(super) fn read_private_leaf(
    directory: &File,
    name: &CStr,
    max: usize,
) -> io::Result<Zeroizing<Vec<u8>>> {
    bounded_read(checked_file(directory, name, libc::O_RDONLY, false)?, max)
}

pub(super) fn read_key(
    directory: &File,
    name: &str,
    allow_root: bool,
) -> io::Result<Zeroizing<[u8; 32]>> {
    let name = name_cstr(name)?;
    let bytes = bounded_read(
        checked_file(directory, &name, libc::O_RDONLY, allow_root)?,
        32,
    )?;
    if bytes.len() != 32 {
        return Err(invalid());
    }
    let mut key = Zeroizing::new([0; 32]);
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Caller holds the account-store lease across the missing-key/data check and publish.
pub(super) fn load_or_create_key(
    home: &File,
    accounts: &File,
    home_path: &Path,
) -> io::Result<Zeroizing<[u8; 32]>> {
    let key_path = home_path.join("master.key");
    match read_key(home, "master.key", false).map_err(|error| permission_path(error, &key_path)) {
        Ok(key) => return Ok(key),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if has_account_data(accounts)
        .map_err(|error| permission_path(error, &home_path.join("accounts")))?
    {
        return Err(invalid());
    }
    let mut key = Zeroizing::new([0; 32]);
    SystemRandom::new()
        .fill(&mut key[..])
        .map_err(|_| invalid())?;
    match create_private_leaf(home, c"master.key", &key[..])
        .map_err(|error| permission_path(error, &key_path))
    {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    read_key(home, "master.key", false).map_err(|error| permission_path(error, &key_path))
}

fn has_account_data(accounts: &File) -> io::Result<bool> {
    // A new open description + fdopendir avoids path scans, shared directory
    // offsets and /proc magic links. Errors are
    // terminal; readdir's errno is per-thread, never shared test configuration.
    let fd = open_at(accounts, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?.into_raw_fd();
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Directory(stream);
    loop {
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(0) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." && name != b".lock" {
            return Ok(true);
        }
    }
}

pub(super) fn create_private_leaf(directory: &File, name: &CStr, bytes: &[u8]) -> io::Result<()> {
    publish(directory, name, bytes, false)
}

pub(super) fn replace_private_leaf(directory: &File, name: &CStr, bytes: &[u8]) -> io::Result<()> {
    publish(directory, name, bytes, true)
}

fn publish(directory: &File, name: &CStr, bytes: &[u8], replace: bool) -> io::Result<()> {
    match validate_leaf(directory, name) {
        Ok(()) if !replace => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut random = [0u8; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| invalid())?;
    let mut temporary = String::from(".qb-write-");
    for byte in random {
        use std::fmt::Write as _;
        write!(temporary, "{byte:02x}").map_err(|_| invalid())?;
    }
    temporary.push_str(".tmp");
    let temporary = name_cstr(&temporary)?;
    let mut file = open_at(
        directory,
        &temporary,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    struct Staging<'a> {
        directory: &'a File,
        name: CString,
        live: bool,
    }
    impl Staging<'_> {
        fn cleanup(&mut self) -> io::Result<()> {
            if self.live {
                if unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                self.live = false;
                self.directory.sync_all()?;
            }
            Ok(())
        }
    }
    impl Drop for Staging<'_> {
        fn drop(&mut self) {
            if self.live {
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
                }
            }
        }
    }
    let mut staging = Staging {
        directory,
        name: temporary,
        live: true,
    };
    let result = (|| {
        check_file(&file, false)?;
        if file.metadata()?.mode() & 0o7777 != 0o600 {
            return Err(refused());
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        let flags = if replace { 0 } else { libc::RENAME_NOREPLACE };
        let result = unsafe {
            libc::renameat2(
                directory.as_raw_fd(),
                staging.name.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
                flags,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::AlreadyExists {
                validate_leaf(directory, name)?;
            }
            return Err(error);
        }
        staging.live = false;
        directory.sync_all()
    })();
    // Explicit normal-error cleanup reports failures. Drop is only an unwind fallback.
    staging.cleanup()?;
    result
}

pub(super) fn open_private_lock(directory: &File, name: &CStr) -> io::Result<File> {
    let flags = libc::O_RDWR;
    let file = match checked_file(directory, name, flags, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match open_at(directory, name, flags | libc::O_CREAT | libc::O_EXCL, 0o600) {
                Ok(file) => {
                    check_file(&file, false)?;
                    directory.sync_all()?;
                    file
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    checked_file(directory, name, flags, false)?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    let meta = file.metadata()?;
    if meta.mode() & 0o200 == 0 {
        return Err(permission_refusal(
            &meta,
            "expected writable private lock file",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod strict_ancestor_policy_tests;

#[cfg(test)]
mod permission_tests;

#[cfg(test)]
pub(super) mod tests;
