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
        .args(["--render-png"])
        .arg(fixture.path().join("unused.png"))
        .arg("--fixture")
        .arg(input)
        .env("QB_HOME", fixture.path())
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
