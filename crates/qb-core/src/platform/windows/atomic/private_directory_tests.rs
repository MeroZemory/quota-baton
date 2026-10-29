use super::*;
use std::{
    io,
    os::windows::process::CommandExt,
    process::Command,
    sync::{Arc, Barrier},
};

#[test]
fn new_leaf_has_one_protected_owner_ace_and_existing_paths_are_refused() {
    let parent = tempfile::tempdir().unwrap();
    let leaf = parent.path().join("leaf");
    create_private_directory_new(&leaf).unwrap();
    assert!(fs::symlink_metadata(&leaf).unwrap().is_dir());
    let saved = parent.path().join("acl.txt");
    let output = Command::new("icacls.exe")
        .arg(&leaf)
        .arg("/save")
        .arg(&saved)
        .arg("/q")
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "synthetic directory ACL inspection failed"
    );
    let bytes = fs::read(saved).unwrap();
    let text = if bytes.starts_with(&[255, 254]) || bytes.contains(&0) {
        let offset = usize::from(bytes.starts_with(&[255, 254])) * 2;
        String::from_utf16(
            &bytes[offset..]
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    } else {
        String::from_utf8(bytes).unwrap()
    };
    assert!(text.contains("D:P"));
    assert_eq!(text.matches("(A;").count(), 1);
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
fn junction_and_missing_parent_are_refused() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("record"), b"original").unwrap();
    let link = parent.path().join("link");
    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "owned synthetic junction creation failed"
    );
    assert_eq!(
        create_private_directory_new(&link).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(target.join("record")).unwrap(), b"original");
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
