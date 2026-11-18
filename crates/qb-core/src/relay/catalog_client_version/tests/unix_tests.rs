use super::*;

#[test]
fn native_reader_rejects_record_alias_and_leaves_target_unchanged() {
    let home = home();
    let store = CatalogClientVersion::open(home.path()).unwrap();
    let target = home.path().join("untouched");
    qb_testkit::fixture::write_private(&target, b"0.199.0").unwrap();
    std::os::unix::fs::symlink(&target, store.directory.join(RECORD)).unwrap();
    assert!(CatalogClientVersion::open(home.path()).is_err());
    assert!(store.observe(version("0.200.0")).is_err());
    assert_eq!(std::fs::read(target).unwrap(), b"0.199.0");
}
