use std::{
    fs::{Metadata, OpenOptions},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub(crate) fn metadata_is_reparse(_metadata: &Metadata) -> bool {
    false
}

pub(crate) fn binding_snapshot_preopen(_path: &Path, _metadata: &Metadata) -> io::Result<()> {
    Ok(())
}

pub(crate) fn configure_snapshot_open(options: &mut OpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
}

pub(crate) fn binding_snapshot_handle_trusted(metadata: &Metadata) -> bool {
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0
}

pub(crate) fn routing_policy_preopen(_path: &Path, _metadata: &Metadata) -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn routing_policy_handle_check(metadata: &Metadata) -> anyhow::Result<()> {
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        anyhow::bail!("routing policy requires owner-only permissions");
    }
    Ok(())
}

pub(crate) fn resolver_local_drive(_letter: u8) -> Option<PathBuf> {
    None
}

pub(crate) fn resolver_local_scan_root(path: &Path, max_bytes: usize, max_depth: usize) -> bool {
    resolver_scan::eligible(path, max_bytes, max_depth)
}

#[cfg(target_os = "linux")]
mod resolver_scan {
    use std::{
        ffi::CString,
        fs::{File, OpenOptions},
        mem::MaybeUninit,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt, fs::OpenOptionsExt},
        },
        path::{Component, Path},
    };

    fn local_filesystem(kind: libc::c_long) -> bool {
        // Fail closed for network, userspace, pseudo and unknown filesystems.
        // Overlay is needed for local container roots; mounted backing storage
        // and the mount namespace remain the administrator's responsibility.
        matches!(
            kind,
            libc::EXT4_SUPER_MAGIC
                | libc::XFS_SUPER_MAGIC
                | libc::BTRFS_SUPER_MAGIC
                | libc::TMPFS_MAGIC
                | libc::OVERLAYFS_SUPER_MAGIC
        )
    }

    fn trusted_directory(directory: &File) -> bool {
        let mut filesystem = MaybeUninit::<libc::statfs>::uninit();
        // Inspect the held directory's filesystem before inode metadata or
        // descent. O_PATH does not open directory contents or trigger automount.
        if unsafe { libc::fstatfs(directory.as_raw_fd(), filesystem.as_mut_ptr()) } != 0
            || !local_filesystem(unsafe { filesystem.assume_init() }.f_type)
        {
            return false;
        }
        let Ok(metadata) = directory.metadata() else {
            return false;
        };
        metadata.is_dir()
            && (metadata.uid() == 0 || metadata.uid() == unsafe { libc::geteuid() })
            && metadata.mode() & 0o022 == 0
    }

    pub(super) fn eligible(path: &Path, max_bytes: usize, max_depth: usize) -> bool {
        let bytes = path.as_os_str().as_bytes();
        if !path.is_absolute()
            || bytes.starts_with(b"//")
            || bytes.contains(&0)
            || bytes.len() > max_bytes
            || path.components().count() > max_depth
            || path
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return false;
        }
        let Ok(mut directory) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")
        else {
            return false;
        };
        if !trusted_directory(&directory) {
            return false;
        }
        for component in path.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            let Ok(name) = CString::new(name.as_bytes()) else {
                return false;
            };
            let descriptor = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if descriptor < 0 {
                return false;
            }
            // Own each successful descriptor immediately, including refusal paths.
            directory = unsafe { File::from_raw_fd(descriptor) };
            if !trusted_directory(&directory) {
                return false;
            }
        }
        true
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{fs, os::unix::fs::PermissionsExt};

        fn fixture() -> tempfile::TempDir {
            // Use the gate's native Linux project scratch, never shared /tmp.
            let base =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/l1-resolver-fs-tests");
            fs::create_dir_all(&base).unwrap();
            tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap()
        }

        #[test]
        fn local_directory_is_eligible_at_exact_path_limits() {
            let temp = fixture();
            let root = temp.path().join("projects");
            fs::create_dir(&root).unwrap();
            let bytes = root.as_os_str().as_bytes().len();
            let depth = root.components().count();
            assert!(eligible(&root, bytes, depth));
            assert!(!eligible(&root, bytes - 1, depth));
            assert!(!eligible(&root, bytes, depth - 1));
            assert!(super::super::super::resolver_local_drive(b'C').is_none());
        }

        #[test]
        fn malformed_relative_and_parent_paths_are_refused() {
            for path in [
                "",
                "projects",
                "../projects",
                "/../projects",
                "//server/share",
                "/bad\0path",
            ] {
                assert!(!eligible(Path::new(path), 1024, 32), "{path:?}");
            }
        }

        #[test]
        fn symlink_leaf_and_parent_are_refused() {
            let temp = fixture();
            let root = temp.path().join("real");
            fs::create_dir_all(root.join("projects")).unwrap();
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            assert!(!eligible(&alias, 1024, 32));
            assert!(!eligible(&alias.join("projects"), 1024, 32));
            assert!(eligible(&root.join("projects"), 1024, 32));
        }

        #[test]
        fn files_missing_paths_and_fifo_are_refused_without_reading() {
            let temp = fixture();
            let file = temp.path().join("transcript");
            fs::write(&file, [0xff, 0xfe]).unwrap();
            assert!(!eligible(&file, 1024, 32));
            assert!(!eligible(&temp.path().join("missing"), 1024, 32));
            let fifo = temp.path().join("fifo");
            let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            assert!(!eligible(&fifo, 1024, 32));
        }

        #[test]
        fn writable_leaf_and_ancestor_are_refused() {
            let temp = fixture();
            let parent = temp.path().join("parent");
            let root = parent.join("projects");
            fs::create_dir_all(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
            assert!(!eligible(&root, 1024, 32));
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o775)).unwrap();
            assert!(!eligible(&root, 1024, 32));
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(eligible(&root, 1024, 32));
        }

        #[test]
        fn network_userspace_pseudo_and_unknown_filesystems_are_refused() {
            for kind in [
                libc::NFS_SUPER_MAGIC,
                libc::SMB_SUPER_MAGIC,
                0xff534d42, // CIFS/SMB2
                0x01021997, // 9p (including Windows-host Docker source mounts)
                libc::FUSE_SUPER_MAGIC,
                libc::PROC_SUPER_MAGIC,
                libc::SYSFS_MAGIC,
                0,
            ] {
                assert!(!local_filesystem(kind), "{kind:#x}");
            }
            for kind in [
                libc::EXT4_SUPER_MAGIC,
                libc::XFS_SUPER_MAGIC,
                libc::BTRFS_SUPER_MAGIC,
                libc::TMPFS_MAGIC,
                libc::OVERLAYFS_SUPER_MAGIC,
            ] {
                assert!(local_filesystem(kind), "{kind:#x}");
            }
            assert!(!eligible(Path::new("/proc"), 1024, 32));
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod resolver_scan {
    pub(super) fn eligible(_path: &std::path::Path, _max_bytes: usize, _max_depth: usize) -> bool {
        false
    }
}

#[cfg(test)]
pub(crate) fn test_assert_snapshot_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(test)]
pub(crate) fn test_set_snapshot_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
