//! systemd's root-owned read-only credential mount has service-user read ACLs.
//! This policy is separate from private writable account/key storage.
use std::{
    ffi::{CStr, CString},
    fs::{File, Metadata, OpenOptions},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path},
};
use zeroize::Zeroizing;

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe systemd credential storage",
    )
}

fn ancestor(meta: &Metadata) -> bool {
    meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o7022 == 0
}

fn directory_policy(mode: u32, owner: u32) -> bool {
    owner == 0 && matches!(mode & 0o7777, 0o500 | 0o550)
}

fn key_policy(mode: u32, owner: u32, links: u64, size: u64) -> bool {
    owner == 0 && links == 1 && size == 32 && matches!(mode & 0o7777, 0o400 | 0o440)
}

fn mount_policy(flags: libc::c_ulong) -> bool {
    let required = libc::ST_RDONLY | libc::ST_NOSUID | libc::ST_NODEV;
    flags & required == required
}

// Linux POSIX ACL xattr ABI: little-endian version 2, then tag/perm/id entries.
// https://github.com/torvalds/linux/blob/master/include/uapi/linux/posix_acl_xattr.h
// Only systemd's single service-user grant is accepted, never a group reader.
fn acl_policy(bytes: &[u8], service_uid: u32, directory: bool) -> bool {
    if bytes.len() != 44 || bytes[..4] != 2u32.to_le_bytes() {
        return false;
    }
    let read = if directory { 5u16 } else { 4u16 };
    let expected = [
        (1u16, read, u32::MAX),
        (2, read, service_uid),
        (4, 0, u32::MAX),
        (16, read, u32::MAX),
        (32, 0, u32::MAX),
    ];
    bytes[4..]
        .chunks_exact(8)
        .zip(expected)
        .all(|(entry, (tag, perm, id))| {
            entry[..2] == tag.to_le_bytes()
                && entry[2..4] == perm.to_le_bytes()
                && entry[4..] == id.to_le_bytes()
        })
}

fn credential_acl(file: &File, directory: bool) -> io::Result<()> {
    let mut bytes = [0u8; 44];
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"system.posix_acl_access".as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if size < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENODATA) {
            let mode = file.metadata()?.mode() & 0o7777;
            return if mode == if directory { 0o500 } else { 0o400 } {
                Ok(())
            } else {
                Err(refused())
            };
        }
        return Err(error);
    }
    if !acl_policy(
        &bytes[..size as usize],
        unsafe { libc::geteuid() },
        directory,
    ) {
        return Err(refused());
    }
    Ok(())
}

fn readonly_mount(file: &File) -> io::Result<()> {
    let mut info: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatvfs(file.as_raw_fd(), &mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if !mount_policy(info.f_flag) {
        return Err(refused());
    }
    Ok(())
}

fn open_at(parent: &File, name: &CStr, directory: bool) -> io::Result<File> {
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn load(path: &Path) -> io::Result<Zeroizing<[u8; 32]>> {
    if !path.is_absolute() || path.as_os_str().as_bytes().len() > 4096 {
        return Err(refused());
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/")?;
    if !ancestor(&directory.metadata()?) {
        return Err(refused());
    }
    let mut depth = 0;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                depth += 1;
                if depth > 128 {
                    return Err(refused());
                }
                let name = CString::new(name.as_bytes()).map_err(|_| refused())?;
                directory = open_at(&directory, &name, true)?;
                if !ancestor(&directory.metadata()?) {
                    return Err(refused());
                }
            }
            _ => return Err(refused()),
        }
    }
    let directory_meta = directory.metadata()?;
    if !directory_policy(directory_meta.mode(), directory_meta.uid()) {
        return Err(refused());
    }
    readonly_mount(&directory)?;
    credential_acl(&directory, true)?;
    let mut key_file = open_at(&directory, c"qb-master-key", false)?;
    let meta = key_file.metadata()?;
    if !meta.is_file()
        || meta.dev() != directory_meta.dev()
        || !key_policy(meta.mode(), meta.uid(), meta.nlink(), meta.len())
    {
        return Err(refused());
    }
    readonly_mount(&key_file)?;
    credential_acl(&key_file, false)?;
    // Kernel access checks enforce the service-user ACL. Never chmod/chown the
    // credential or silently fall back to the private-file backend.
    let mut key = Zeroizing::new([0; 32]);
    key_file.read_exact(&mut key[..])?;
    let mut extra = Zeroizing::new([0; 1]);
    if key_file.read(&mut extra[..])? != 0 {
        return Err(refused());
    }
    let after = key_file.metadata()?;
    if !key_policy(after.mode(), after.uid(), after.nlink(), after.len()) {
        return Err(refused());
    }
    Ok(key)
}

#[cfg(test)]
mod tests;
