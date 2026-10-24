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

pub(crate) fn resolver_local_scan_root(_path: &Path, _max_bytes: usize, _max_depth: usize) -> bool {
    false
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
