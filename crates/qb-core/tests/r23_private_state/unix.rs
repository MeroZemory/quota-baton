use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};
pub(super) fn prepare_home(path: &Path) {
    // Only the just-created, owned empty TempDir is prepared here. Do not
    // precreate state or normalize a directory supplied by the product/user.
    let meta = fs::symlink_metadata(path).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.uid(), unsafe { libc::geteuid() });
    assert!(fs::read_dir(path).unwrap().next().is_none());
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
pub(super) fn assert_private(path: &Path) {
    let meta = fs::symlink_metadata(path).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.uid(), unsafe { libc::geteuid() });
    assert_eq!(meta.permissions().mode() & 0o7777, 0o700);
}
pub(super) fn create_loose(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
pub(super) fn assert_loose(path: &Path) {
    assert_eq!(
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777,
        0o755
    );
}

#[test]
fn symlinked_intermediate_is_not_followed_or_modified() {
    let home = super::home();
    let other = super::home();
    std::os::unix::fs::symlink(other.path(), home.path().join("state")).unwrap();
    assert!(qb_core::ensure_private_dir_all(&home.path().join("state/child")).is_err());
    assert!(!other.path().join("child").exists());
    assert_private(other.path());
}
