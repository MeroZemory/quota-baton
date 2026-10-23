//! Verify the product PE, rather than the console-subsystem test harness.
#![cfg(windows)]

use std::{fs, os::windows::process::CommandExt, process::Command};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

#[test]
fn tray_product_pe_uses_gui_subsystem() {
    let pe = fs::read(env!("CARGO_BIN_EXE_quota-baton-tray")).unwrap();
    assert_eq!(&pe[..2], b"MZ");
    let signature = u32::from_le_bytes(pe[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&pe[signature..signature + 4], b"PE\0\0");
    let optional = signature + 4 + 20;
    let magic = u16::from_le_bytes(pe[optional..optional + 2].try_into().unwrap());
    assert!(matches!(magic, 0x10b | 0x20b), "PE32 or PE32+ required");
    // Subsystem is at offset 68 in both PE32 and PE32+ optional headers.
    let subsystem = u16::from_le_bytes(pe[optional + 68..optional + 70].try_into().unwrap());
    assert_eq!(subsystem, 2, "tray must be GUI, never console subsystem 3");
}

#[test]
fn console_free_startup_failure_reaches_file_log_without_fixture_contents() {
    let sandbox = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r83-tests");
    fs::create_dir_all(&sandbox).unwrap();
    let fixture = tempfile::tempdir_in(sandbox).unwrap();
    let input = fixture.path().join("invalid.json");
    fs::write(&input, b"SYNTHETIC-SECRET-MUST-NOT-BE-LOGGED").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_quota-baton-tray"))
        .args(["--no-supervise", "--fixture"])
        .arg(input)
        .env("QB_HOME", fixture.path())
        .env("LOCALAPPDATA", fixture.path())
        .env("QB_LOG", "info")
        .creation_flags(CREATE_NO_WINDOW.0)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let logs = fs::read_dir(fixture.path().join("logs"))
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(logs.contains("tray_startup_failed"));
    assert!(!logs.contains("SYNTHETIC-SECRET"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SYNTHETIC-SECRET"));
}

fn assert_argument_failure_is_logged(args: &[&str]) {
    let sandbox = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r83-tests");
    fs::create_dir_all(&sandbox).unwrap();
    let home = tempfile::tempdir_in(sandbox).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_quota-baton-tray"))
        .args(args)
        .env("QB_HOME", home.path())
        .env("QB_LOG", "info")
        .creation_flags(CREATE_NO_WINDOW.0)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("tray_startup_failed"));
    let logs = fs::read_dir(home.path().join("logs"))
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(logs.contains("tray_startup_failed"));
    for arg in args {
        assert!(!logs.contains(arg));
        assert!(!stderr.contains(arg));
    }
}

#[test]
fn console_free_unknown_flag_reaches_safe_startup_log() {
    assert_argument_failure_is_logged(&["--unknown-option"]);
}

#[test]
fn console_free_missing_value_reaches_safe_startup_log() {
    assert_argument_failure_is_logged(&["--theme"]);
}

#[test]
fn console_free_parser_failure_never_exposes_secret_arguments() {
    assert_argument_failure_is_logged(&["--theme", "SYNTHETIC-SECRET-MUST-NOT-BE-LOGGED"]);
    assert_argument_failure_is_logged(&["--SYNTHETIC-SECRET-MUST-NOT-BE-LOGGED"]);
}

#[test]
fn console_free_help_exits_successfully_without_startup_failure() {
    let sandbox = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r83-tests");
    fs::create_dir_all(&sandbox).unwrap();
    let home = tempfile::tempdir_in(sandbox).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_quota-baton-tray"))
        .arg("--help")
        .env("QB_HOME", home.path())
        .env("QB_LOG", "info")
        .creation_flags(CREATE_NO_WINDOW.0)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
    assert!(output.stderr.is_empty());
    assert!(!home.path().join("logs").exists());
}
