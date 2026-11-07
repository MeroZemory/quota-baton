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

/// The well-known application-packages SID, only on synthetic directories.
/// Production native ACLs are never read or changed by this helper.
#[cfg(windows)]
pub fn native_sandbox_acl(path: &Path) -> Vec<u8> {
    assert!(path.components().any(|c| c.as_os_str() == ".sandbox"));
    let path = path
        .to_str()
        .unwrap()
        .strip_prefix(r"\\?\")
        .unwrap_or(path.to_str().unwrap());
    let output = std::process::Command::new("icacls.exe")
        .arg(path)
        .args(["/grant", "*S-1-15-2-1:(OI)(CI)F"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "synthetic ACL grant failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        qb_core::ensure_private_dir_all(Path::new(path)).is_err(),
        "fixture must reject qb private ACL validation"
    );
    native_acl_snapshot(Path::new(path))
}

#[cfg(windows)]
pub fn native_acl_snapshot(path: &Path) -> Vec<u8> {
    assert!(path.components().any(|c| c.as_os_str() == ".sandbox"));
    let path = path
        .to_str()
        .unwrap()
        .strip_prefix(r"\\?\")
        .unwrap_or(path.to_str().unwrap());
    let output = std::process::Command::new("icacls.exe")
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "synthetic ACL snapshot failed");
    output.stdout
}
