use qb_core::accounts::AccountStore;

#[test]
fn takeover_can_release_the_home_lease_before_store_drop() {
    let parent = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent).unwrap();
    let home = root.path().join("home");
    let first = AccountStore::acquire(&home).unwrap();
    first.release_lease();

    // The runtime objects may still hold the first store while the successor
    // acquires the same home during takeover.
    let second = AccountStore::acquire(&home).unwrap();
    second.release_lease();
}
