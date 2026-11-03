use super::*;

const OURS: &str = r#""C:\fixture\quota-baton-tray.exe""#;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    // Keep verification data inside the project, never in a live profile.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox");
    fs::create_dir_all(&root).unwrap();
    // Markers with `..` are refused, so resolve the manifest-relative root.
    let dir = tempfile::Builder::new()
        .prefix("r22-registry-")
        .tempdir_in(root.canonicalize().unwrap())
        .unwrap();
    let path = dir.path().join(".r22-test-autostart.json");
    (dir, path)
}

fn open(path: &Path) -> FileRegistry {
    FileRegistry::open(path.as_os_str()).unwrap()
}

#[track_caller]
fn rejected(path: &Path) {
    let error = FileRegistry::open(path.as_os_str())
        .err()
        .expect("must fail closed");
    assert_eq!(error.to_string(), ERROR);
}

#[test]
fn absent_preview_and_remove_create_nothing() {
    let (dir, path) = fixture();
    let mut registry = open(&path);
    assert_eq!(registry.read(VALUE_NAME).unwrap(), (None, STRING_TYPE));
    assert!(remove_from(&mut registry, false).unwrap().is_empty());
    registry.remove(VALUE_NAME).unwrap();
    assert!(!path.exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn typed_values_persist_across_opens_and_last_remove_restores_absence() {
    let (dir, path) = fixture();
    for kind in [STRING_TYPE, EXPAND_STRING_TYPE] {
        register(&mut open(&path), OURS, kind).unwrap();
        let original = fs::read(&path).unwrap();
        assert_eq!(
            open(&path).read(VALUE_NAME).unwrap(),
            (Some(OURS.into()), kind)
        );
        register(&mut open(&path), OURS, kind).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        let json: serde_json::Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(json["magic"], MAGIC);
        assert_eq!(json["version"], 1);
        assert_eq!(json["values"][VALUE_NAME]["value_type"], kind);
        assert_eq!(remove_from(&mut open(&path), false).unwrap().len(), 1);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(remove_from(&mut open(&path), true).unwrap().len(), 1);
        assert!(!path.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

#[test]
fn preserves_foreign_keys_and_foreign_autostart_values() {
    let (_dir, path) = fixture();
    {
        let mut registry = open(&path);
        registry
            .set("another-app", "public fixture", EXPAND_STRING_TYPE)
            .unwrap();
        registry
            .set(LEGACY_VALUE_NAME, "foreign fixture", STRING_TYPE)
            .unwrap();
        register(&mut registry, OURS, STRING_TYPE).unwrap();
        remove_from(&mut registry, true).unwrap();
    }
    let mut registry = open(&path);
    assert_eq!(registry.read(VALUE_NAME).unwrap().0, None);
    assert_eq!(
        registry.read("another-app").unwrap(),
        (Some("public fixture".into()), EXPAND_STRING_TYPE)
    );
    assert_eq!(
        registry.read(LEGACY_VALUE_NAME).unwrap().0.as_deref(),
        Some("foreign fixture")
    );
    registry
        .set(VALUE_NAME, "foreign canonical fixture", STRING_TYPE)
        .unwrap();
    let before = fs::read(&path).unwrap();
    remove_from(&mut registry, true).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn migrates_owned_legacy_without_losing_foreign_keys() {
    let (_dir, path) = fixture();
    let mut registry = open(&path);
    registry
        .set(LEGACY_VALUE_NAME, OURS, EXPAND_STRING_TYPE)
        .unwrap();
    registry
        .set("another-app", "public fixture", STRING_TYPE)
        .unwrap();
    register(&mut registry, OURS, STRING_TYPE).unwrap();
    assert_eq!(registry.read(LEGACY_VALUE_NAME).unwrap().0, None);
    assert_eq!(
        registry.read("another-app").unwrap().0.as_deref(),
        Some("public fixture")
    );
}

#[test]
fn malformed_or_foreign_files_are_never_overwritten() {
    let (_dir, path) = fixture();
    for bytes in [
        b"".as_slice(),
        b"not json",
        br#"{"magic":"other","version":1,"values":{}}"#,
        br#"{"magic":"quota-baton-autostart-test","version":2,"values":{}}"#,
        br#"{"magic":"quota-baton-autostart-test","version":1,"values":{},"extra":1}"#,
        br#"{"magic":"quota-baton-autostart-test","version":1,"values":{"x":{"command":"public","value_type":3}}}"#,
        br#"{"magic":"quota-baton-autostart-test","version":1,"values":{"x":{"command":"a\u0000b","value_type":1}}}"#,
    ] {
        fs::write(&path, bytes).unwrap();
        rejected(&path);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    let bytes = vec![b' '; MAX_FILE_BYTES + 1];
    fs::write(&path, &bytes).unwrap();
    rejected(&path);
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn storage_and_path_errors_are_fixed_and_fail_closed() {
    let (dir, path) = fixture();
    for marker in ["", "relative.json", r"C:relative.json", r"\rooted.json"] {
        rejected(Path::new(marker));
    }
    rejected(&dir.path().join("missing").join("store.json"));
    rejected(&dir.path().join("store.json:stream.json"));
    rejected(&dir.path().join("CON.json"));
    rejected(&dir.path().join("LPT1.json"));
    rejected(&dir.path().join(".. sinister.json "));
    // PathBuf::join folds `..` under a verbatim prefix; keep it literal.
    for suffix in [r"\..\store.json", r"\.\store.json"] {
        let mut marker = dir.path().as_os_str().to_owned();
        marker.push(suffix);
        rejected(Path::new(&marker));
    }
    fs::create_dir(&path).unwrap();
    rejected(&path);
    fs::remove_dir(&path).unwrap();
    let mut registry = open(&path);
    // A destination changed after opening must also be validated before writing.
    fs::write(&path, b"foreign public fixture").unwrap();
    assert_eq!(
        registry
            .set(VALUE_NAME, OURS, STRING_TYPE)
            .unwrap_err()
            .to_string(),
        ERROR
    );
    assert_eq!(registry.remove(VALUE_NAME).unwrap_err().to_string(), ERROR);
    assert_eq!(fs::read(&path).unwrap(), b"foreign public fixture");
}

#[test]
fn invalid_writes_preserve_absence_and_bounds_preserve_previous_store() {
    let (_dir, path) = fixture();
    let mut registry = open(&path);
    for (command, kind) in [("public", 3), ("a\0b", 1)] {
        assert_eq!(
            registry
                .set(VALUE_NAME, command, kind)
                .unwrap_err()
                .to_string(),
            ERROR
        );
        assert!(!path.exists());
    }
    let too_long = "a".repeat(MAX_VALUE_BYTES / 2);
    assert!(registry.set(VALUE_NAME, &too_long, STRING_TYPE).is_err());
    assert!(!path.exists());
    for index in 0..MAX_ENTRIES {
        registry
            .set(&format!("fixture-{index}"), "public", STRING_TYPE)
            .unwrap();
    }
    let before = fs::read(&path).unwrap();
    assert_eq!(
        registry
            .set("overflow", "public", STRING_TYPE)
            .unwrap_err()
            .to_string(),
        ERROR
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn concurrent_operations_preserve_each_writers_values() {
    let (_dir, path) = fixture();
    std::thread::scope(|scope| {
        for index in 0..8 {
            let path = &path;
            scope.spawn(move || {
                open(path)
                    .set(&format!("fixture-{index}"), "public", STRING_TYPE)
                    .unwrap();
            });
        }
    });
    let mut registry = open(&path);
    for index in 0..8 {
        assert_eq!(
            registry
                .read(&format!("fixture-{index}"))
                .unwrap()
                .0
                .as_deref(),
            Some("public")
        );
    }
}

#[test]
fn selection_policy_covers_debug_and_release_without_environment_mutation() {
    for enabled in [false, true] {
        assert_eq!(test_registry_marker(None, enabled).unwrap(), None);
    }
    for marker in ["", "relative.json", r"C:\fixture\store.json"] {
        let marker = std::ffi::OsString::from(marker);
        assert_eq!(
            test_registry_marker(Some(marker.clone()), true).unwrap(),
            Some(marker.clone())
        );
        assert_eq!(
            test_registry_marker(Some(marker), false)
                .unwrap_err()
                .to_string(),
            TEST_REGISTRY_RELEASE
        );
    }
    // cfg(test) excludes the entire native module, even in release-profile tests.
    // Existing public operations still use this thread's Fake, regardless of env.
    test_set(Some(OURS));
    assert_eq!(read().unwrap().0.as_deref(), Some(OURS));
    reset_fake();
}
