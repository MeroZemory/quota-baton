use std::path::Path;

/// The well-known application-packages SID, only on synthetic directories.
/// Production native ACLs are never read or changed by this helper.
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
