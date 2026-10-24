use std::{
    fs::{Metadata, OpenOptions},
    io,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub(crate) fn metadata_is_reparse(metadata: &Metadata) -> bool {
    metadata.file_attributes() & 0x400 != 0
}

pub(crate) fn binding_snapshot_preopen(path: &Path, metadata: &Metadata) -> io::Result<()> {
    if metadata_is_reparse(metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid binding snapshot trust or size",
        ));
    }
    super::secrets::restrict(path).map_err(io::Error::other)
}

pub(crate) fn configure_snapshot_open(options: &mut OpenOptions) {
    options.custom_flags(0x00200000);
}

pub(crate) fn binding_snapshot_handle_trusted(metadata: &Metadata) -> bool {
    !metadata_is_reparse(metadata)
}

pub(crate) fn routing_policy_preopen(path: &Path, metadata: &Metadata) -> anyhow::Result<()> {
    if metadata_is_reparse(metadata) {
        anyhow::bail!("routing policy reparse point refused");
    }
    super::secrets::restrict(path)?;
    Ok(())
}

pub(crate) fn routing_policy_handle_check(metadata: &Metadata) -> anyhow::Result<()> {
    if metadata_is_reparse(metadata) {
        anyhow::bail!("routing policy reparse handle refused");
    }
    if metadata_is_reparse(metadata) {
        anyhow::bail!("routing policy reparse handle refused");
    }
    Ok(())
}

// Only fixed local disks/RAM disks qualify. Never issue path metadata against
// UNC, a mapped network drive, unknown media, or a reparse prefix.
pub(crate) fn resolver_local_drive(letter: u8) -> Option<PathBuf> {
    use windows::{Win32::Storage::FileSystem::GetDriveTypeW, core::PCWSTR};
    let drive = [
        u16::from(letter.to_ascii_uppercase()),
        u16::from(b':'),
        u16::from(b'\\'),
        0,
    ];
    let kind = unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) };
    matches!(kind, 3 | 6)
        .then(|| PathBuf::from(format!("{}:\\", char::from(letter.to_ascii_uppercase()))))
}

pub(crate) fn resolver_local_scan_root(path: &Path, max_bytes: usize, max_depth: usize) -> bool {
    use std::path::{Component, Prefix};
    let mut parts = path.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        return false;
    };
    let letter = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
        _ => return false,
    };
    if !matches!(parts.next(), Some(Component::RootDir)) {
        return false;
    }
    if path.as_os_str().len() > max_bytes
        || path.components().count() > max_depth
        || parts.any(|part| !matches!(part, Component::Normal(_)))
    {
        return false;
    }
    resolver_local_drive(letter).is_some()
}

#[cfg(test)]
pub(crate) fn test_assert_snapshot_owner_only(_path: &Path) {}

#[cfg(test)]
pub(crate) fn test_set_snapshot_owner_only(_path: &Path) {}
