//! Positive test setup only. Never use on real homes or to repair unsafe input.
#![allow(dead_code)] // Shared by test targets that need different fixture pieces.
use std::{fs, io, path::Path};

/// Preserve a fixture's fixed directory names while making only missing owned
/// components private. Existing directories are never chmodded or ACL-repaired.
pub fn private_directory(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => return Ok(()),
        Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    private_directory(parent)?;
    let directory = qb_testkit::fixture::tempdir_in(parent)?;
    // On failure TempDir removes only its newly created leaf; on success its
    // original name no longer exists. The surrounding owned root owns cleanup.
    fs::rename(directory.path(), path)
}

/// Same input convenience as fs::write; ordinary positive files get the frozen
/// testkit's private publication rather than the process default file mode/ACL.
pub fn write_private(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    qb_testkit::fixture::write_private(path, bytes.as_ref())
}

#[cfg(windows)]
#[path = "fixture_support/windows.rs"]
pub mod windows;
