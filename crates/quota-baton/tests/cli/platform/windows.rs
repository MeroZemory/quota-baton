// Validate the native login-start trailer separately from the exact account table.
pub(super) fn status_without_platform_trailer(status: &str) -> &str {
    let (table, trailer) = status
        .strip_suffix('\n')
        .unwrap()
        .rsplit_once('\n')
        .unwrap();
    let state = trailer.strip_prefix("로그인 시 자동 시작: ").unwrap();
    assert!(
        ["등록됨", "등록 안 됨"].iter().any(|expected| {
            state == *expected
                || state
                    .strip_prefix(*expected)
                    .is_some_and(|details| details.starts_with(" (") && details.ends_with(')'))
        }),
        "unexpected login-start status: {trailer:?}"
    );
    assert!(!trailer.chars().any(char::is_control));
    &status[..table.len() + 1]
}

const TEST_REGISTRY: &str = "QB_TEST_AUTOSTART_REGISTRY";
pub(super) const TEST_REGISTRY_FILE: &str = ".r22-test-autostart.json";

/// Apply after every caller's env_clear/overrides, immediately before spawning.
pub(super) fn configure_test_registry(command: &mut std::process::Command) -> std::io::Result<()> {
    use std::{ffi::OsStr, io, path::PathBuf};
    if command.get_program() != OsStr::new(env!("CARGO_BIN_EXE_quota-baton")) {
        return Ok(());
    }
    if let Some((_, selected)) = command.get_envs().find(|(key, _)| *key == TEST_REGISTRY) {
        return selected
            .map(|_| ())
            .ok_or_else(|| io::Error::other("CLI integration command removed registry isolation"));
    }
    let home = command
        .get_envs()
        .find(|(key, _)| *key == "QB_HOME")
        .and_then(|(_, value)| value.map(PathBuf::from));
    if let Some(home) = home {
        let home = if home.is_absolute() {
            home
        } else {
            command
                .get_current_dir()
                .map(PathBuf::from)
                .unwrap_or(std::env::current_dir()?)
                .join(home)
        };
        command.env(TEST_REGISTRY, home.join(TEST_REGISTRY_FILE));
        return Ok(());
    }
    let args: Vec<_> = command.get_args().collect();
    if !args
        .iter()
        .any(|arg| *arg == "--help" || *arg == "--version")
        && args.first().is_some_and(|arg| {
            ["wire", "unwire", "status"]
                .iter()
                .any(|verb| *arg == OsStr::new(verb))
        })
    {
        return Err(io::Error::other(
            "registry-using CLI integration command requires an explicit fixture home",
        ));
    }
    Ok(())
}

type RunSnapshot = [Option<(u32, [u8; 32])>; 2];

/// Read only the two product Run values. Never create a key or retain raw data.
pub(super) fn real_run_snapshot() -> std::io::Result<RunSnapshot> {
    use sha2::{Digest, Sha256};
    use std::io;
    use windows::{
        Win32::{
            Foundation::ERROR_FILE_NOT_FOUND,
            System::Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_VALUE_TYPE, RegCloseKey,
                RegOpenKeyExW, RegQueryValueExW,
            },
        },
        core::PCWSTR,
    };
    struct ReadKey(HKEY);
    impl Drop for ReadKey {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let path = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let mut handle = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut handle,
        )
    };
    if opened == ERROR_FILE_NOT_FOUND {
        return Ok([None, None]);
    }
    if !opened.is_ok() {
        return Err(io::Error::from_raw_os_error(opened.0 as i32));
    }
    let key = ReadKey(handle);
    let mut snapshot = [None, None];
    for (index, name) in ["quota-baton", "quota-baton-tray"].into_iter().enumerate() {
        let name = wide(name);
        let mut bytes = vec![0u8; 64 * 1024];
        let mut length = bytes.len() as u32;
        let mut kind = REG_VALUE_TYPE(0);
        let read = unsafe {
            RegQueryValueExW(
                key.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(bytes.as_mut_ptr()),
                Some(&mut length),
            )
        };
        if read == ERROR_FILE_NOT_FOUND {
            continue;
        }
        if !read.is_ok() {
            return Err(io::Error::from_raw_os_error(read.0 as i32));
        }
        if length as usize > bytes.len() {
            return Err(io::Error::other("Run snapshot exceeded its metadata bound"));
        }
        snapshot[index] = Some((kind.0, Sha256::digest(&bytes[..length as usize]).into()));
    }
    Ok(snapshot)
}

pub(super) fn verify_real_run_unchanged(before: &RunSnapshot) -> std::io::Result<()> {
    if &real_run_snapshot()? != before {
        return Err(std::io::Error::other(
            "CLI integration command changed the real login autostart values",
        ));
    }
    Ok(())
}
