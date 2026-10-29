use super::*;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    sync::{Arc, Barrier},
};

#[test]
fn new_leaf_is_private_and_existing_directory_or_file_is_not_reused() {
    let parent = tempfile::tempdir().unwrap();
    let leaf = parent.path().join("leaf");
    create_private_directory_new(&leaf).unwrap();
    let metadata = fs::symlink_metadata(&leaf).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
    assert_eq!(
        create_private_directory_new(&leaf).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    let file = parent.path().join("file");
    fs::write(&file, b"original").unwrap();
    assert_eq!(
        create_private_directory_new(&file).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(file).unwrap(), b"original");
}

#[test]
fn links_and_missing_parent_are_refused() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("target");
    fs::create_dir(&target).unwrap();
    let link = parent.path().join("link");
    symlink(&target, &link).unwrap();
    assert_eq!(
        create_private_directory_new(&link).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    let dangling = parent.path().join("dangling");
    symlink(parent.path().join("absent"), &dangling).unwrap();
    assert_eq!(
        create_private_directory_new(&dangling).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        create_private_directory_new(&parent.path().join("missing/leaf"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(!parent.path().join("missing").exists());
}

#[test]
fn simultaneous_creators_have_exactly_one_winner() {
    let parent = tempfile::tempdir().unwrap();
    let leaf = parent.path().join("race");
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let leaf = leaf.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                create_private_directory_new(&leaf).map_err(|e| e.kind())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Err(io::ErrorKind::AlreadyExists))
            .count(),
        1
    );
}
