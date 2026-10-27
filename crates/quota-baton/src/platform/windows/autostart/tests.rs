use super::*;

const OURS: &str = r#""C:\Program Files\quota-baton\quota-baton-tray.exe""#;

#[test]
fn bounded_registry_strings_reject_malformed_data_with_fixed_error() {
    for bytes in [
        vec![],
        vec![0],
        vec![65, 0],
        vec![0, 0, 0, 0],
        vec![0, 216, 0, 0],
        vec![0; MAX_VALUE_BYTES + 2],
    ] {
        assert_eq!(
            decode(&bytes, STRING_TYPE).unwrap_err().to_string(),
            MALFORMED
        );
    }
    assert_eq!(decode(&[0, 0], 3).unwrap_err().to_string(), MALFORMED);
    for kind in [STRING_TYPE, EXPAND_STRING_TYPE] {
        assert_eq!(decode(&[65, 0, 0, 0], kind).unwrap(), "A");
        assert_eq!(decode(&[0, 0], kind).unwrap(), "");
    }
}

#[test]
fn register_is_idempotent_and_preserves_other_values() {
    let mut registry = FakeRegistry::default();
    registry.set("another-app", "foreign", STRING_TYPE).unwrap();
    register(&mut registry, OURS, STRING_TYPE).unwrap();
    assert_eq!(
        registry.read(VALUE_NAME).unwrap(),
        (Some(OURS.into()), STRING_TYPE)
    );
    let writes = registry.writes;
    register(&mut registry, OURS, STRING_TYPE).unwrap();
    assert_eq!(registry.writes, writes);
    register(&mut registry, OURS, EXPAND_STRING_TYPE).unwrap();
    assert_eq!(registry.writes, writes + 1);
    assert_eq!(
        registry.read("another-app").unwrap().0.as_deref(),
        Some("foreign")
    );
}

#[test]
fn preview_shows_foreign_and_apply_preserves_it() {
    let mut registry = FakeRegistry::default();
    registry
        .set(VALUE_NAME, "foreign.exe --start", STRING_TYPE)
        .unwrap();
    let writes = registry.writes;
    let preview = remove_from(&mut registry, false).unwrap();
    assert_eq!(
        preview,
        vec!["autostart: preserve quota-baton = foreign.exe --start"]
    );
    assert_eq!(remove_from(&mut registry, true).unwrap(), preview);
    assert_eq!(registry.writes, writes);
    register(&mut registry, OURS, STRING_TYPE).unwrap();
    assert_eq!(registry.read(VALUE_NAME).unwrap().0.as_deref(), Some(OURS));
}

#[test]
fn removes_only_owned_canonical_and_legacy_values() {
    let mut registry = FakeRegistry::default();
    for name in [VALUE_NAME, LEGACY_VALUE_NAME, "another-app"] {
        registry.set(name, OURS, STRING_TYPE).unwrap();
    }
    let writes = registry.writes;
    let preview = remove_from(&mut registry, false).unwrap();
    assert_eq!(preview.len(), 2);
    assert_eq!(registry.writes, writes);
    assert_eq!(remove_from(&mut registry, true).unwrap(), preview);
    assert_eq!(registry.read(VALUE_NAME).unwrap().0, None);
    assert_eq!(registry.read(LEGACY_VALUE_NAME).unwrap().0, None);
    assert_eq!(
        registry.read("another-app").unwrap().0.as_deref(),
        Some(OURS)
    );
}

#[test]
fn foreign_legacy_survives_owned_canonical_removal() {
    let mut registry = FakeRegistry::default();
    registry.set(VALUE_NAME, OURS, STRING_TYPE).unwrap();
    registry
        .set(LEGACY_VALUE_NAME, "other.exe", STRING_TYPE)
        .unwrap();
    remove_from(&mut registry, true).unwrap();
    assert_eq!(
        registry.read(LEGACY_VALUE_NAME).unwrap().0.as_deref(),
        Some("other.exe")
    );
}

#[test]
fn register_migrates_only_our_legacy_name_without_two_login_entries() {
    let mut registry = FakeRegistry::default();
    registry.set(LEGACY_VALUE_NAME, OURS, STRING_TYPE).unwrap();
    register(&mut registry, OURS, STRING_TYPE).unwrap();
    assert_eq!(registry.read(LEGACY_VALUE_NAME).unwrap().0, None);
    assert_eq!(registry.read(VALUE_NAME).unwrap().0.as_deref(), Some(OURS));
    registry
        .set(LEGACY_VALUE_NAME, "foreign.exe", STRING_TYPE)
        .unwrap();
    register(&mut registry, OURS, STRING_TYPE).unwrap();
    assert_eq!(
        registry.read(LEGACY_VALUE_NAME).unwrap().0.as_deref(),
        Some("foreign.exe")
    );
}

#[test]
fn ownership_requires_a_complete_bare_or_quoted_executable_path() {
    for command in [
        OURS,
        r"c:\APP\QUOTA-BATON-TRAY.EXE",
        r"\\server\share\quota-baton-tray.exe",
    ] {
        assert!(owned(command), "{command}");
    }
    for command in [
        "quota-baton-tray.exe",
        r"C:quota-baton-tray.exe",
        "",
        r#""C:\app\quota-baton-tray.exe" --flag"#,
        r"C:\app\quota-baton-tray.exe --flag",
        r"C:\other.exe args\quota-baton-tray.exe",
        r"C:\Program Files\quota-baton\quota-baton-tray.exe",
        "C:\\other.exe\targs\\quota-baton-tray.exe",
        "C:\\other.exe\u{a0}args\\quota-baton-tray.exe",
        r#"cmd.exe /c "C:\app\quota-baton-tray.exe""#,
        r"C:\app\other.exe",
        r"C:\app\not-quota-baton-tray.exe",
        r"C:\app\quota-baton-tray.exe.bak",
        r"\\quota-baton-tray.exe",
        r"C:\app&shell\quota-baton-tray.exe",
        r"%APPDATA%\quota-baton-tray.exe",
        "\"C:\\app\\quota-baton-tray.exe",
        "C:\\app\\quota-baton-tray.exe\n",
    ] {
        assert!(!owned(command), "{command}");
    }
}

#[test]
fn public_functions_always_use_this_threads_fake() {
    test_set(Some("foreign.exe"));
    assert_eq!(read().unwrap(), (Some("foreign.exe".into()), STRING_TYPE));
    assert_eq!(fake_writes(), 0);
    test_set(None);
    assert_eq!(read().unwrap(), (None, STRING_TYPE));
    set(OURS, STRING_TYPE).unwrap();
    reset_fake();
    assert_eq!(read().unwrap(), (None, STRING_TYPE));
    assert!(remove_owned(false).unwrap().is_empty());
    set(OURS, STRING_TYPE).unwrap();
    set(OURS, STRING_TYPE).unwrap();
    assert_eq!(fake_writes(), 1);
    assert_eq!(read().unwrap().0.as_deref(), Some(OURS));
    remove_owned(false).unwrap();
    assert_eq!(fake_writes(), 1);
    remove_owned(true).unwrap();
    assert_eq!(fake_writes(), 2);
    assert_eq!(read().unwrap().0, None);
}

#[test]
fn status_is_localized_and_one_line() {
    set_fake(None);
    assert_eq!(status(true).unwrap(), "Login autostart: not registered");
    assert_eq!(status(false).unwrap(), "로그인 시 자동 시작: 등록 안 됨");
    set_fake(Some((OURS.into(), STRING_TYPE)));
    assert_eq!(
        status(true).unwrap(),
        format!("Login autostart: registered ({OURS})")
    );
    assert_eq!(
        status(false).unwrap(),
        format!("로그인 시 자동 시작: 등록됨 ({OURS})")
    );
    set_fake(Some(("foreign\r\ncommand".into(), STRING_TYPE)));
    assert_eq!(
        status(true).unwrap(),
        r"Login autostart: not registered (quota-baton: foreign value = foreign\r\ncommand)"
    );
    assert_eq!(fake_writes(), 0);
}

#[test]
fn unquoted_arguments_survive_removal_and_legacy_migration() {
    let foreign = r"C:\other.exe args\quota-baton-tray.exe";
    let mut registry = FakeRegistry::default();
    for name in [VALUE_NAME, LEGACY_VALUE_NAME] {
        registry.set(name, foreign, STRING_TYPE).unwrap();
    }
    let writes = registry.writes;
    assert!(
        remove_from(&mut registry, true)
            .unwrap()
            .iter()
            .all(|line| line.contains("preserve"))
    );
    assert_eq!(registry.writes, writes);
    register(&mut registry, OURS, STRING_TYPE).unwrap();
    assert_eq!(
        registry.read(LEGACY_VALUE_NAME).unwrap().0.as_deref(),
        Some(foreign)
    );
    assert_eq!(registry.read(VALUE_NAME).unwrap().0.as_deref(), Some(OURS));
}

#[test]
fn status_reports_legacy_and_coexisting_values_without_writes() {
    for canonical in [None, Some(OURS), Some("foreign\r\ncommand")] {
        for legacy in [OURS, "foreign-legacy.exe"] {
            test_set(canonical);
            FAKE.with(|registry| {
                registry
                    .borrow_mut()
                    .values
                    .insert(LEGACY_VALUE_NAME.into(), (legacy.into(), STRING_TYPE));
            });
            for english in [true, false] {
                let result = status(english).unwrap();
                let registered = canonical == Some(OURS) || legacy == OURS;
                let prefix = match (english, registered) {
                    (true, true) => "Login autostart: registered",
                    (true, false) => "Login autostart: not registered",
                    (false, true) => "로그인 시 자동 시작: 등록됨",
                    (false, false) => "로그인 시 자동 시작: 등록 안 됨",
                };
                assert!(result.starts_with(prefix), "{result}");
                assert!(result.contains(LEGACY_VALUE_NAME));
                assert!(result.contains(legacy));
                assert!(result.contains(if english { "legacy" } else { "이전 항목" }));
                if let Some(canonical) = canonical {
                    assert!(
                        result.contains(&format!("{VALUE_NAME}:"))
                            || result.contains(&format!("{VALUE_NAME} ="))
                    );
                    assert!(result.contains(&one_line(canonical)));
                }
                if canonical == Some("foreign\r\ncommand") || legacy != OURS {
                    assert!(result.contains(if english {
                        "foreign value"
                    } else {
                        "다른 값"
                    }));
                }
                assert_eq!(result.lines().count(), 1);
                assert_eq!(fake_writes(), 0);
            }
            assert_eq!(read().unwrap().0.as_deref(), canonical);
            FAKE.with(|registry| assert_eq!(registry.borrow().values[LEGACY_VALUE_NAME].0, legacy));
        }
    }
}

#[test]
fn invalid_writes_fail_before_mutating() {
    let mut registry = FakeRegistry::default();
    for (command, kind) in [
        ("bad\0value".into(), STRING_TYPE),
        ("value".into(), 3),
        ("x".repeat(MAX_VALUE_BYTES / 2), STRING_TYPE),
    ] {
        assert_eq!(
            register(&mut registry, &command, kind)
                .unwrap_err()
                .to_string(),
            MALFORMED
        );
    }
    assert_eq!(registry.writes, 0);
}
