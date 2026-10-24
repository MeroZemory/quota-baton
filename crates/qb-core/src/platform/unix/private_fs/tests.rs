use super::*;
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
};

pub(crate) fn fixture() -> tempfile::TempDir {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/l1-p1-private-fs-tests");
    fs::create_dir_all(&base).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("synthetic-")
        .tempdir_in(base)
        .unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

#[test]
fn prepare_creates_private_nested_directories_and_refuses_existing_broad_leaf() {
    let fixture = fixture();
    let home = fixture.path().join("nested/home");
    let parent = prepare_private_directory(&home).unwrap();
    prepare_private_child(&parent, "accounts").unwrap();
    for path in [&home, &home.join("accounts")] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(prepare_private_directory(&home).is_err());
    assert_eq!(
        fs::metadata(home).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn bounded_private_reads_reject_surplus_unsafe_modes_and_hardlinks() {
    let fixture = fixture();
    let directory = private_directory(fixture.path(), false).unwrap();
    create_private_leaf(&directory, c"record", b"data").unwrap();
    assert_eq!(
        &*read_private_leaf(&directory, c"record", 4).unwrap(),
        b"data"
    );
    assert!(read_private_leaf(&directory, c"record", 3).is_err());
    assert!(read_private_leaf(&directory, c"record", usize::MAX).is_err());
    fs::set_permissions(
        fixture.path().join("record"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(read_private_leaf(&directory, c"record", 4).is_err());
    assert!(replace_private_leaf(&directory, c"record", b"changed").is_err());
    assert_eq!(fs::read(fixture.path().join("record")).unwrap(), b"data");
    fs::set_permissions(
        fixture.path().join("record"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fs::hard_link(fixture.path().join("record"), fixture.path().join("alias")).unwrap();
    assert!(validate_leaf(&directory, c"record").is_err());
}

#[test]
fn safe_publication_is_noreplace_and_atomic_replace_leaves_no_staging_files() {
    let fixture = fixture();
    let directory = private_directory(fixture.path(), false).unwrap();
    create_private_leaf(&directory, c"record", b"first").unwrap();
    assert_eq!(
        create_private_leaf(&directory, c"record", b"second")
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(fixture.path().join("record")).unwrap(), b"first");
    replace_private_leaf(&directory, c"record", b"replacement").unwrap();
    assert_eq!(
        fs::read(fixture.path().join("record")).unwrap(),
        b"replacement"
    );
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
}

#[test]
fn unsafe_symlink_parent_and_leaf_are_not_followed_or_repaired() {
    let fixture = fixture();
    let directory = private_directory(fixture.path(), false).unwrap();
    let target = fixture.path().join("target");
    create_private_leaf(&directory, c"target", b"unchanged").unwrap();
    symlink(&target, fixture.path().join("alias")).unwrap();
    assert!(replace_private_leaf(&directory, c"alias", b"attack").is_err());
    assert!(create_private_leaf(&directory, c"alias", b"attack").is_err());
    assert!(validate_leaf(&directory, c"alias").is_err());
    assert_eq!(fs::read(&target).unwrap(), b"unchanged");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(fixture.path().join("child"))
        .unwrap();
    symlink(
        fixture.path().join("child"),
        fixture.path().join("child-alias"),
    )
    .unwrap();
    assert!(private_directory(&fixture.path().join("child-alias"), false).is_err());
}

#[test]
fn parent_traversal_and_foreign_owner_policy_are_refused() {
    let fixture = fixture();
    assert!(prepare_private_directory(&fixture.path().join("../escape")).is_err());
    let foreign = if uid() == 1 { 2 } else { 1 };
    assert!(!owner_ok(foreign, false));
    assert!(!owner_ok(foreign, true));
}

#[test]
fn concurrent_noreplace_creators_leave_one_complete_winner() {
    let fixture = fixture();
    let root = fixture.path();
    std::thread::scope(|scope| {
        let tasks: Vec<_> = [b"first".as_slice(), b"second".as_slice()]
            .into_iter()
            .map(|bytes| {
                scope.spawn(move || {
                    let directory = private_directory(root, false).unwrap();
                    create_private_leaf(&directory, c"winner", bytes)
                })
            })
            .collect();
        let results: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| matches!(result, Err(error) if error.kind() == io::ErrorKind::AlreadyExists)).count(), 1);
    });
    let winner = fs::read(root.join("winner")).unwrap();
    assert!(winner == b"first" || winner == b"second");
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}
