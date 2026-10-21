//! Per-session managed admission. Named launches hold a shared session gate;
//! moves exclude that session across affected homes without stopping other work.
use anyhow::Result;
use serde::Deserialize;
use std::{
    ffi::OsString,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

#[cfg(windows)]
const GATE: &str = ".quota-baton-admission.lock";
#[cfg(windows)]
const BUSY: &str = "다른 관리 Codex 실행 또는 대화 이동이 진행 중입니다. 먼저 해당 실행을 종료한 뒤 다시 시도하세요";
const INSPECTION: &str = "대화 실행 잠금을 안전하게 확인하지 못했습니다";
const WIRING: &str = "전용 Codex 홈 연결을 확인하지 못했습니다. 먼저 wire를 실행하세요";
const WRONG_HOME: &str = "관리 Codex 실행의 CODEX_HOME이 연결된 전용 홈과 다릅니다";
const COMMAND: &str = "관리 Codex 실행 파일을 확인하지 못했습니다";
const VERSION: &str =
    "이 대화 기능은 Codex CLI 0.163.0만 지원합니다. 설치된 Codex 버전을 확인하세요";

#[derive(Debug)]
struct Refusal(&'static str);

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for Refusal {}

fn refusal(message: &'static str) -> anyhow::Error {
    anyhow::Error::new(Refusal(message))
}

pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    error.downcast_ref::<Refusal>().map(ToString::to_string)
}

#[derive(Deserialize)]
struct Wiring {
    schema: u32,
    mode: String,
    home: PathBuf,
    #[cfg_attr(test, allow(dead_code))]
    codex_command: Option<PathBuf>,
}

/// R3 journals predate the terminal install marker. Completed entry metadata
/// may retain a historical scratch path; `pending` alone does not mean that a
/// done entry is incomplete. Active upgrade/restore markers do.
pub(crate) fn completed_wiring(value: &serde_json::Value) -> bool {
    use serde_json::Value;
    if value["schema"] != 3 || value["mode"] != "dedicated" {
        return false;
    }
    // claude_restored is independent: dedicated Claude upgrades legitimately
    // restore the old default settings while the Codex installation stays live.
    for marker in ["retained", "optional_restored", "partial"] {
        if value
            .get(marker)
            .is_some_and(|v| !v.is_null() && v != false)
        {
            return false;
        }
    }
    let entries = value.get("entries").and_then(Value::as_array);
    if value.get("entries").is_some() && entries.is_none() {
        return false;
    }
    if entries.is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry["done"] != true
                || entry.get("removed").is_some_and(|v| v != false)
                || entry.get("upgrade").is_some_and(|v| !v.is_null())
                || entry["original"]["restoring"] == true
        })
    }) {
        return false;
    }
    match value.get("install_completed") {
        Some(Value::Bool(true)) => true,
        None => entries.is_some_and(|entries| !entries.is_empty()),
        _ => false,
    }
}

/// Deliberately read-only: no admission lock, version probe, configuration or
/// credential access, journal migration, launcher spawn, or auto-import.
pub(crate) fn check_wiring(home: &Path) -> Result<()> {
    wiring(home).map(drop)
}

fn wiring(home: &Path) -> Result<Wiring> {
    let path = home.join("journal/wiring.json");
    crate::sessions_guard::safe_path(home, &path).map_err(|_| refusal(WIRING))?;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|_| refusal(WIRING))?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(refusal(WIRING));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| refusal(WIRING))?;
    if !completed_wiring(&value) {
        return Err(refusal(WIRING));
    }
    let wiring: Wiring = serde_json::from_value(value).map_err(|_| refusal(WIRING))?;
    if wiring.schema != 3
        || wiring.mode != "dedicated"
        || !wiring.home.is_absolute()
        || !wiring.home.is_dir()
    {
        return Err(refusal(WIRING));
    }
    crate::sessions_guard::safe_path(&wiring.home, &wiring.home).map_err(|_| refusal(WIRING))?;
    Ok(wiring)
}

/// Compatibility only, not executable authentication. Never accept a supplied
/// version string, testkit environment variable, or cached probe result.
pub(crate) fn verify_version(command: &Path) -> Result<()> {
    if !command.is_absolute() || !command.is_file() {
        return Err(refusal(VERSION));
    }
    let output = crate::launcher_label::version_output(command).map_err(|_| refusal(VERSION))?;
    if !pinned_version(&output) {
        return Err(refusal(VERSION));
    }
    Ok(())
}

fn pinned_version(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"codex-cli 0.163.0" | b"codex-cli 0.163.0\n" | b"codex-cli 0.163.0\r\n"
    )
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn verify_installed_version() -> Result<()> {
    let installed = wiring(&qb_core::home()?)?;
    let command = match installed.codex_command {
        Some(command) => command,
        None => PathBuf::from(std::env::var_os("APPDATA").ok_or_else(|| refusal(VERSION))?)
            .join("npm/codex.cmd"),
    };
    verify_version(&command)
}

pub(crate) fn shared_for(home: &Path, id: Option<&str>) -> Result<Guard> {
    platform::acquire(&[home.to_path_buf()], false, id)
}

pub(crate) fn exclusive(homes: &[PathBuf], id: &str) -> Result<Guard> {
    platform::acquire(homes, true, Some(id))
}

/// The caller checks the durable journals again while this permit is held.
/// Explicit auto-import runs before this function to avoid shared/exclusive
/// self-deadlock. The permit must outlive launcher_label::run, including errors.
pub(crate) fn prepare_launch(args: &[OsString], command: &Path) -> Result<(Guard, Vec<OsString>)> {
    let installed = wiring(&qb_core::home()?)?;
    let id = crate::sessions::resume_uuid(args)?;
    let permit = shared_for(&installed.home, id.as_deref())?;
    let args = launch_plan(args, command, &installed)?;
    Ok((permit, args))
}

/// Read-only validation before explicit auto-import, repeated under the shared
/// gate afterward. Invalid command/home wiring must not trigger a move first.
/// Version compatibility is checked separately by mutating move callers.
pub(crate) fn preflight_launch(args: &[OsString], command: &Path) -> Result<()> {
    launch_plan(args, command, &wiring(&qb_core::home()?)?).map(drop)
}

/// Validate the normal launcher home first, then acquire the journaled Qi gate.
/// The caller supplies Qi only after checking the completed wiring metadata.
pub(crate) fn preflight_images(
    args: &[OsString],
    command: &Path,
    image: &crate::image_launch::ImageLaunch,
) -> Result<()> {
    let installed = wiring(&qb_core::home()?)?;
    launch_plan(args, command, &installed)?;
    if fs::canonicalize(&installed.home)? != fs::canonicalize(&image.ordinary_home)? {
        return Err(refusal(WRONG_HOME));
    }
    Ok(())
}

pub(crate) fn prepare_images(
    args: &[OsString],
    command: &Path,
    image: &crate::image_launch::ImageLaunch,
) -> Result<(Guard, Vec<OsString>)> {
    preflight_images(args, command, image)?;
    let id = crate::sessions::resume_uuid(args)?;
    let permit = shared_for(&image.home, id.as_deref())?;
    Ok((permit, managed_args(args)))
}

fn launch_plan(args: &[OsString], command: &Path, installed: &Wiring) -> Result<Vec<OsString>> {
    let actual = PathBuf::from(std::env::var_os("CODEX_HOME").ok_or_else(|| refusal(WRONG_HOME))?);
    if !actual.is_absolute() {
        return Err(refusal(WRONG_HOME));
    }
    crate::sessions_guard::safe_path(&actual, &actual).map_err(|_| refusal(WRONG_HOME))?;
    if fs::canonicalize(&actual).map_err(|_| refusal(WRONG_HOME))?
        != fs::canonicalize(&installed.home).map_err(|_| refusal(WRONG_HOME))?
    {
        return Err(refusal(WRONG_HOME));
    }
    if !command.is_absolute() || !command.is_file() {
        return Err(refusal(COMMAND));
    }
    Ok(managed_args(args))
}

/// Own flags and resume UUIDs are handled by the caller. Native arguments,
/// including unknown options, config overrides and literal boundaries, are opaque.
fn managed_args(args: &[OsString]) -> Vec<OsString> {
    args.to_vec()
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        fs::{OpenOptions, TryLockError},
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE, GetFileInformationByHandle,
        },
    };

    // File's Debug can expose paths. Keep these handles private and opaque.
    pub(crate) struct Guard {
        _locks: Vec<File>,
        _paths: Vec<crate::sessions_guard::PublicationGuard>,
    }

    fn info(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
            .map_err(|_| refusal(INSPECTION))?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(refusal(INSPECTION));
        }
        Ok(info)
    }

    fn options() -> OpenOptions {
        let mut options = OpenOptions::new();
        options
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0);
        options
    }

    pub(crate) fn acquire(homes: &[PathBuf], exclusive: bool, id: Option<&str>) -> Result<Guard> {
        if id.is_some_and(|id| !crate::sessions_discovery::valid_id(id)) {
            return Err(refusal(INSPECTION));
        }
        if homes.is_empty() {
            return Err(refusal(INSPECTION));
        }
        let mut pins = Vec::new();
        let mut pending = Vec::new();
        for home in homes {
            // Keep the same path checks, and use the shared anchored-parent
            // protocol so effective directory pins also permit publication.
            crate::sessions_guard::safe_path(home, home).map_err(|_| refusal(INSPECTION))?;
            let home = std::path::absolute(home).map_err(|_| refusal(INSPECTION))?;
            if !home.is_dir() {
                return Err(refusal(INSPECTION));
            }
            pins.push(
                crate::sessions_guard::prepare_parent(&home, &home)
                    .map_err(|_| refusal(INSPECTION))?,
            );
            let Some(id) = id else {
                continue;
            };
            let file = options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(home.join(format!("{GATE}.{id}")))
                .map_err(|_| refusal(INSPECTION))?;
            let info = info(&file)?;
            if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                return Err(refusal(INSPECTION));
            }
            let identity = (
                info.dwVolumeSerialNumber,
                info.nFileIndexHigh,
                info.nFileIndexLow,
            );
            pending.push((identity, file));
        }
        // Identity order handles aliases and opposing source/target order.
        pending.sort_by_key(|(identity, _)| *identity);
        pending.dedup_by_key(|(identity, _)| *identity);
        let mut held = Vec::new();
        for (_, file) in pending {
            let result = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            match result {
                Ok(()) => held.push(file),
                Err(TryLockError::WouldBlock) => return Err(refusal(BUSY)),
                Err(TryLockError::Error(_)) => return Err(refusal(INSPECTION)),
            }
        }
        Ok(Guard {
            _locks: held,
            _paths: pins,
        })
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub(crate) struct Guard;
    pub(crate) fn acquire(_: &[PathBuf], _: bool, _: Option<&str>) -> Result<Guard> {
        Err(refusal(INSPECTION))
    }
}
pub(crate) use platform::Guard;

#[cfg(test)]
mod tests {
    use super::*;
    const SESSION: &str = "11111111-1111-1111-1111-111111111111";

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn r92_released_completion_shapes_accept_only_terminal_legacy_entries() {
        for raw in [
            include_str!("../tests/fixtures/wiring/r3.json"),
            include_str!("../tests/fixtures/wiring/r3_1.json"),
            include_str!("../tests/fixtures/wiring/r3_2.json"),
        ] {
            let value: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert!(completed_wiring(&value));
            for marker in ["retained", "optional_restored", "partial"] {
                let mut incomplete = value.clone();
                incomplete[marker] = true.into();
                assert!(!completed_wiring(&incomplete));
            }
            for marker in [
                serde_json::json!(false),
                serde_json::Value::Null,
                serde_json::json!("true"),
            ] {
                let mut incomplete = value.clone();
                incomplete["install_completed"] = marker;
                assert!(!completed_wiring(&incomplete));
            }
            for entries in [
                serde_json::json!([]),
                serde_json::json!({}),
                serde_json::Value::Null,
            ] {
                let mut incomplete = value.clone();
                incomplete["entries"] = entries;
                assert!(!completed_wiring(&incomplete));
            }
        }
    }

    #[test]
    fn pinned_version_is_exact_and_not_a_substring() {
        for version in [
            "codex-cli 0.163.0",
            "codex-cli 0.163.0\n",
            "codex-cli 0.163.0\r\n",
        ] {
            assert!(pinned_version(version.as_bytes()));
        }
        for version in [
            "0.163.0",
            "codex-cli 0.163.1\n",
            "codex-cli 0.163.0-beta\n",
            "x\ncodex-cli 0.163.0\n",
            "codex-cli 0.163.0\nextra",
        ] {
            assert!(!pinned_version(version.as_bytes()));
        }
    }

    #[test]
    fn native_arguments_pass_through_without_injected_flags() {
        for input in [
            vec![],
            args(&["resume"]),
            args(&["resume", "--last"]),
            args(&["exec", "resume", "--last"]),
            args(&["--", "a prompt"]),
            args(&["fix X"]),
            args(&["--profile", "work", "--oss", "--enable", "new_feature"]),
            args(&[
                "-c",
                "openai_base_url='http://synthetic.invalid/v1'",
                "--config=model_catalog_url='http://synthetic.invalid/catalog'",
                "-i",
                "one.png",
                "-i",
                "two.png",
                "--",
                "--literal",
            ]),
        ] {
            assert_eq!(managed_args(&input), input);
        }
        assert_eq!(
            managed_args(&args(&["resume", "--no-daemon"])),
            args(&["resume", "--no-daemon"])
        );
    }

    #[test]
    fn native_commands_unknown_flags_and_config_overrides_pass_through() {
        for input in [
            args(&["app-server"]),
            args(&["daemon"]),
            args(&["--future-option"]),
            args(&["resume", "--no-daemon=false"]),
            args(&["--config", "codex_home='elsewhere'", "resume"]),
            args(&["-c", "features.daemon=true"]),
            args(&["--enable", "daemon"]),
            args(&["--model"]),
        ] {
            assert_eq!(managed_args(&input), input);
        }
        let input = args(&["-c", "model_reasoning_effort='high'", "resume", "--last"]);
        assert_eq!(managed_args(&input), input);
    }

    #[test]
    fn provider_override_is_opaque_in_every_option_form() {
        for value in [
            "openai",
            "'openai'",
            "\"openai\"",
            " 'openai' ",
            "other",
            "'other'",
            "\"other\"",
            "openai_other",
            "\" openai \"",
            "'openai'\nfeatures.daemon=true",
        ] {
            let config = format!("model_provider={value}");
            let inline = format!("--config={config}");
            for input in [
                args(&["-c", &config, "resume"]),
                args(&["--config", &config, "resume"]),
                args(&[&inline, "resume"]),
            ] {
                assert_eq!(managed_args(&input), input);
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_native_arguments_are_preserved() {
        use std::os::windows::ffi::OsStringExt;
        let input = vec![OsString::from_wide(&[0xd800, 0x61])];
        assert_eq!(managed_args(&input), input);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "isolated child of launch_validation_keeps_wiring_without_version_probe"]
    fn launch_validation_probe() {
        let home = qb_core::home().unwrap();
        let installed = wiring(&home).unwrap();
        let command = home.join("codex.cmd");
        for input in [
            vec![],
            args(&["fix X"]),
            args(&["resume", "11111111-1111-1111-1111-111111111111"]),
        ] {
            preflight_launch(&input, &command).unwrap();
            let (permit, output) = prepare_launch(&input, &command).unwrap();
            assert_eq!(output, input);
            drop(permit);
        }
        assert!(!home.join("version-probed").exists());
        for command in [
            PathBuf::from("relative.cmd"),
            home.join("missing.cmd"),
            home.clone(),
        ] {
            let error = launch_plan(&[], &command, &installed).unwrap_err();
            assert_eq!(friendly_error(&error).as_deref(), Some(COMMAND));
        }
        let mut wrong = wiring(&home).unwrap();
        wrong.home = home.clone();
        let error = launch_plan(&[], &command, &wrong).unwrap_err();
        assert_eq!(friendly_error(&error).as_deref(), Some(WRONG_HOME));
        fs::write(home.join("journal/wiring.json"), b"{}").unwrap();
        let error = preflight_launch(&[], &command).unwrap_err();
        assert_eq!(friendly_error(&error).as_deref(), Some(WIRING));
        // Positive control: the explicit move API still probes and refuses.
        let error = verify_version(&command).unwrap_err();
        assert_eq!(friendly_error(&error).as_deref(), Some(VERSION));
        assert!(home.join("version-probed").exists());
    }

    #[cfg(windows)]
    #[test]
    fn launch_validation_keeps_wiring_without_version_probe() {
        let root = pin_fixture();
        let home = root.path().join("qb");
        let target = root.path().join("target");
        fs::create_dir_all(home.join("journal")).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(
            home.join("journal/wiring.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema":3,"mode":"dedicated","install_completed":true,"home":target
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(home.join("codex.cmd"), b"@echo off\r\necho probe>\"%QB_HOME%\\version-probed\"\r\necho codex-cli 0.999.0\r\nexit /b 0\r\n").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "sessions_admission::tests::launch_validation_probe",
                "--test-threads=1",
            ])
            .env("QB_HOME", &home)
            .env("CODEX_HOME", &target)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            home.join("version-probed").exists(),
            "validation helper did not run"
        );
    }

    #[cfg(windows)]
    #[test]
    fn actual_batch_version_probe_checks_output_and_exit_status() {
        let scratch =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/session-admission-tests");
        fs::create_dir_all(&scratch).unwrap();
        let root = tempfile::tempdir_in(scratch).unwrap();
        let command = root.path().join("codex.cmd");
        fs::write(
            &command,
            b"@echo off\r\necho codex-cli 0.163.0\r\nexit /b 0\r\n",
        )
        .unwrap();
        let verbatim = fs::canonicalize(&command).unwrap();
        assert!(!command.as_os_str().as_encoded_bytes().starts_with(br"\\?\"));
        assert!(
            verbatim
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(br"\\?\")
        );
        for command in [&command, &verbatim] {
            let output = crate::launcher_label::version_output(command).unwrap_or_else(|error| {
                panic!(
                    "batch capture failed: {command:?}: {error:?}, OS={:?}",
                    error.raw_os_error()
                )
            });
            assert_eq!(output, b"codex-cli 0.163.0\r\n", "{command:?}");
            verify_version(command).unwrap();
        }
        fs::write(
            &command,
            b"@echo off\r\necho codex-cli 0.163.1\r\nexit /b 0\r\n",
        )
        .unwrap();
        for command in [&command, &verbatim] {
            assert!(verify_version(command).is_err());
        }
        fs::write(
            &command,
            b"@echo off\r\necho codex-cli 0.163.0\r\nexit /b 1\r\n",
        )
        .unwrap();
        for command in [&command, &verbatim] {
            assert!(verify_version(command).is_err());
        }
        fs::write(
            &command,
            b"@echo off\r\necho codex-cli 0.163.0\r\necho extra\r\nexit /b 0\r\n",
        )
        .unwrap();
        for command in [&command, &verbatim] {
            assert!(verify_version(command).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn shared_launches_exclude_moves_and_both_home_orders() {
        let scratch =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/session-admission-tests");
        fs::create_dir_all(&scratch).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(scratch).unwrap()).unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let first = shared_for(&a, Some(SESSION)).unwrap();
        let second = shared_for(&a, Some(SESSION)).unwrap();
        assert!(exclusive(&[b.clone(), a.clone()], SESSION).is_err());
        drop((first, second));
        let moving = exclusive(&[a.clone(), b.clone(), a.clone()], SESSION).unwrap();
        assert!(shared_for(&a, Some(SESSION)).is_err());
        assert!(shared_for(&b, Some(SESSION)).is_err());
        assert!(exclusive(&[b.clone(), a.clone()], SESSION).is_err());
        drop(moving);
        assert!(shared_for(&a, Some(SESSION)).is_ok());
        assert!(shared_for(&b, Some(SESSION)).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn exclusive_session_allows_fresh_and_other_ids_without_releasing_directory_pins() {
        const OTHER: &str = "22222222-2222-2222-2222-222222222222";
        for id in [None, Some(OTHER)] {
            let root = pin_fixture();
            let parent = root.path().join("parent");
            let a = parent.join("a");
            let b = parent.join("b");
            fs::create_dir_all(&a).unwrap();
            fs::create_dir_all(&b).unwrap();
            let moving = exclusive(&[a.clone(), b.clone()], SESSION).unwrap();
            let first = shared_for(&a, id).unwrap();
            let second = shared_for(&b, id).unwrap();
            for home in [&a, &b] {
                assert_eq!(
                    shared_for(home, Some(SESSION)).err().unwrap().to_string(),
                    BUSY
                );
            }
            drop(moving);
            for home in [&a, &b] {
                assert!(shared_for(home, Some(SESSION)).is_ok());
                assert_eq!(
                    fs::rename(home, parent.join("renamed-home"))
                        .unwrap_err()
                        .raw_os_error(),
                    Some(32)
                );
            }
            assert_eq!(
                fs::rename(&parent, root.path().join("renamed-parent"))
                    .unwrap_err()
                    .raw_os_error(),
                Some(32)
            );
            drop((first, second));
            fs::rename(&a, parent.join("renamed-home")).unwrap();
            fs::rename(&parent, root.path().join("renamed-parent")).unwrap();
        }
    }

    #[cfg(windows)]
    fn pin_fixture() -> tempfile::TempDir {
        let scratch =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/session-admission-tests");
        fs::create_dir_all(&scratch).unwrap();
        tempfile::tempdir_in(fs::canonicalize(scratch).unwrap()).unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn admission_pins_block_home_and_ancestor_rename_but_allow_child_publication() {
        for moving in [false, true] {
            let root = pin_fixture();
            let parent = root.path().join("parent");
            let home = parent.join("home");
            fs::create_dir_all(&home).unwrap();
            let source = home.join("staged");
            let target = home.join("published");
            fs::write(&source, b"synthetic rollout").unwrap();
            let held = if moving {
                exclusive(std::slice::from_ref(&home), SESSION).unwrap()
            } else {
                shared_for(&home, Some(SESSION)).unwrap()
            };
            assert_eq!(
                fs::rename(&home, parent.join("moved-home"))
                    .unwrap_err()
                    .raw_os_error(),
                Some(32)
            );
            assert_eq!(
                fs::rename(&parent, root.path().join("moved-parent"))
                    .unwrap_err()
                    .raw_os_error(),
                Some(32)
            );
            fs::hard_link(&source, home.join("linked")).unwrap();
            fs::rename(&source, &target).unwrap();
            assert_eq!(fs::read(&target).unwrap(), b"synthetic rollout");
            drop(held);
            fs::rename(&home, parent.join("moved-home")).unwrap();
            fs::rename(&parent, root.path().join("moved-parent")).unwrap();
        }
    }

    #[cfg(windows)]
    fn metadata_writer(path: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES deliberately bypasses data-sharing checks.
        // This is stronger than testing a GENERIC_WRITE open that gets OS 32.
        fs::OpenOptions::new()
            .access_mode(0x100)
            .share_mode(7)
            .custom_flags(0x02200000) // OPEN_REPARSE_POINT | BACKUP_SEMANTICS
            .open(path)
            .unwrap()
    }

    #[cfg(windows)]
    fn set_mount_point(writer: &File, target: &Path) -> std::io::Result<()> {
        use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn DeviceIoControl(
                device: *mut std::ffi::c_void,
                code: u32,
                input: *const u8,
                input_size: u32,
                output: *mut u8,
                output_size: u32,
                returned: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
        }
        let canonical = fs::canonicalize(target)?;
        let display: Vec<u16> = canonical.as_os_str().encode_wide().skip(4).collect();
        let substitute: Vec<u16> = r"\??\"
            .encode_utf16()
            .chain(display.iter().copied())
            .collect();
        let substitute_bytes = u16::try_from(substitute.len() * 2).unwrap();
        let display_bytes = u16::try_from(display.len() * 2).unwrap();
        let data_bytes = 8 + substitute_bytes + display_bytes + 4;
        let mut data = 0xA0000003u32.to_le_bytes().to_vec(); // MOUNT_POINT tag
        for field in [
            data_bytes,
            0,
            0,
            substitute_bytes,
            substitute_bytes + 2,
            display_bytes,
        ] {
            data.extend(field.to_le_bytes());
        }
        for unit in substitute.into_iter().chain([0]).chain(display).chain([0]) {
            data.extend(unit.to_le_bytes());
        }
        let mut returned = 0;
        // Both paths/handle belong exclusively to this synthetic fixture.
        if unsafe {
            DeviceIoControl(
                writer.as_raw_handle(),
                0x900A4,
                data.as_ptr(),
                data.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn admission_anchor_blocks_in_place_retag_through_preopened_metadata_writer() {
        for moving in [false, true] {
            let root = pin_fixture();
            let home = root.path().join("home");
            let empty = root.path().join("unprotected");
            let target = root.path().join("target");
            for path in [&home, &empty, &target] {
                fs::create_dir(path).unwrap();
            }
            fs::write(target.join("witness"), b"keep target").unwrap();
            // Positive control proves this metadata-only access and packet can
            // retag an unprotected empty directory without elevated privileges.
            set_mount_point(&metadata_writer(&empty), &target).unwrap();
            fs::remove_dir(&empty).unwrap();
            let writer = metadata_writer(&home);
            let held = if moving {
                exclusive(std::slice::from_ref(&home), SESSION).unwrap()
            } else {
                shared_for(&home, Some(SESSION)).unwrap()
            };
            assert_eq!(
                set_mount_point(&writer, &target)
                    .unwrap_err()
                    .raw_os_error(),
                Some(145)
            );
            drop(held);
            fs::remove_file(home.join(format!("{GATE}.{SESSION}"))).unwrap();
            assert_eq!(
                fs::read_dir(&home).unwrap().count(),
                0,
                "directory anchor leaked"
            );
            set_mount_point(&writer, &target).unwrap();
            drop(writer);
            fs::remove_dir(&home).unwrap();
            assert_eq!(fs::read(target.join("witness")).unwrap(), b"keep target");
        }
    }
}
