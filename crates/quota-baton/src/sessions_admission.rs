//! Home-wide managed admission. A launch holds a shared gate for its entire
//! contained child lifetime; moves hold exclusive gates in every affected home.
use anyhow::Result;
use serde::Deserialize;
use std::{
    ffi::{OsStr, OsString},
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
const UNSUPPORTED: &str = "이 Codex 명령이나 옵션은 관리 실행에서 지원하지 않습니다. daemon과 app-server 직접 실행은 사용할 수 없습니다";
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
    install_completed: bool,
    home: PathBuf,
    #[cfg_attr(test, allow(dead_code))]
    codex_command: Option<PathBuf>,
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
    let wiring: Wiring = serde_json::from_slice(&bytes).map_err(|_| refusal(WIRING))?;
    if wiring.schema != 3
        || wiring.mode != "dedicated"
        || !wiring.install_completed
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

pub(crate) fn shared_for(home: &Path) -> Result<Guard> {
    platform::acquire(&[home.to_path_buf()], false)
}

pub(crate) fn exclusive(homes: &[PathBuf]) -> Result<Guard> {
    platform::acquire(homes, true)
}

/// The caller checks the durable journals again while this permit is held.
/// Explicit auto-import runs before this function to avoid shared/exclusive
/// self-deadlock. The permit must outlive launcher_label::run, including errors.
pub(crate) fn prepare_launch(args: &[OsString], command: &Path) -> Result<(Guard, Vec<OsString>)> {
    let installed = wiring(&qb_core::home()?)?;
    let permit = shared_for(&installed.home)?;
    let args = launch_plan(args, command, &installed)?;
    Ok((permit, args))
}

/// Read-only validation before explicit auto-import, repeated under the shared
/// gate afterward. Bad arguments/home/version must not trigger a move first.
pub(crate) fn preflight_launch(args: &[OsString], command: &Path) -> Result<()> {
    launch_plan(args, command, &wiring(&qb_core::home()?)?).map(drop)
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
    let args = managed_args(args)?;
    verify_version(command)?;
    Ok(args)
}

/// A deliberately bounded grammar for the pinned CLI. In particular, config
/// overrides cannot change home/daemon/features, and switches are never passed
/// through merely because the native CLI might understand them.
fn managed_args(args: &[OsString]) -> Result<Vec<OsString>> {
    let mut output = vec![OsString::from("--no-daemon")];
    let mut command = None;
    let mut positional = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            output.extend_from_slice(&args[index..]);
            break;
        }
        let text = arg.to_str().ok_or_else(|| refusal(UNSUPPORTED))?;
        if text.starts_with('-') && text != "-" {
            let (flag, inline) = text
                .split_once('=')
                .map_or((text, None), |(k, v)| (k, Some(v)));
            if flag == "--no-daemon" && inline.is_none() {
                index += 1;
                continue;
            }
            let value = matches!(
                flag,
                "--model"
                    | "-m"
                    | "--sandbox"
                    | "-s"
                    | "--ask-for-approval"
                    | "-a"
                    | "--cd"
                    | "-C"
                    | "--add-dir"
                    | "--image"
                    | "-i"
                    | "--config"
                    | "-c"
                    | "--output-last-message"
                    | "-o"
                    | "--output-schema"
                    | "--color"
            );
            if value {
                let option_value = match inline {
                    Some(value) if !value.is_empty() => OsStr::new(value),
                    Some(_) => return Err(refusal(UNSUPPORTED)),
                    None => {
                        index += 1;
                        args.get(index)
                            .ok_or_else(|| refusal(UNSUPPORTED))?
                            .as_os_str()
                    }
                };
                if matches!(flag, "--config" | "-c") && !safe_config(option_value) {
                    return Err(refusal(UNSUPPORTED));
                }
                if inline.is_none() {
                    output.push(arg.clone());
                    output.push(option_value.to_owned());
                } else {
                    output.push(arg.clone());
                }
            } else if inline.is_none()
                && matches!(
                    flag,
                    "--help"
                        | "-h"
                        | "--version"
                        | "-V"
                        | "--no-alt-screen"
                        | "--full-auto"
                        | "--dangerously-bypass-approvals-and-sandbox"
                        | "--search"
                        | "--last"
                        | "--all"
                        | "--json"
                        | "--skip-git-repo-check"
                        | "--ephemeral"
                )
            {
                output.push(arg.clone());
            } else {
                return Err(refusal(UNSUPPORTED));
            }
        } else if command.is_none() {
            command = Some(text);
            if !matches!(text, "resume" | "fork" | "exec" | "e") {
                // An explicit -- permits free-form TUI prompts without allowing
                // an unknown native subcommand to bypass this admission grammar.
                return Err(refusal(UNSUPPORTED));
            }
            output.push(arg.clone());
        } else {
            if !positional && matches!(command, Some("exec" | "e")) && text == "review" {
                return Err(refusal(UNSUPPORTED));
            }
            positional = true;
            output.push(arg.clone());
        }
        index += 1;
    }
    Ok(output)
}

fn safe_config(value: &OsStr) -> bool {
    value
        .to_str()
        .and_then(|value| value.split_once('='))
        .is_some_and(|(key, value)| {
            if key.trim() == "model_provider" {
                return matches!(value.trim(), "openai" | "\"openai\"" | "'openai'");
            }
            matches!(
                key.trim(),
                "model"
                    | "model_reasoning_effort"
                    | "model_verbosity"
                    | "approval_policy"
                    | "sandbox_mode"
                    | "personality"
            )
        })
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

    pub(crate) fn acquire(homes: &[PathBuf], exclusive: bool) -> Result<Guard> {
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
            let file = options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(home.join(GATE))
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
    pub(crate) fn acquire(_: &[PathBuf], _: bool) -> Result<Guard> {
        Err(refusal(INSPECTION))
    }
}
pub(crate) use platform::Guard;

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
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
    fn every_supported_path_gets_global_no_daemon() {
        for input in [
            vec![],
            args(&["resume"]),
            args(&["resume", "--last"]),
            args(&["exec", "resume", "--last"]),
            args(&["--", "a prompt"]),
        ] {
            let output = managed_args(&input).unwrap();
            assert_eq!(output[0], "--no-daemon");
            assert_eq!(&output[1..], input);
        }
        assert_eq!(
            managed_args(&args(&["resume", "--no-daemon"])).unwrap(),
            args(&["--no-daemon", "resume"])
        );
    }

    #[test]
    fn daemon_unknown_flags_and_home_overrides_refuse() {
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
            assert!(managed_args(&input).is_err(), "{input:?}");
        }
        let input = args(&["-c", "model_reasoning_effort='high'", "resume", "--last"]);
        assert!(managed_args(&input).is_ok());
    }

    #[test]
    fn provider_override_accepts_only_literal_openai_in_every_option_form() {
        for (value, allowed) in [
            ("openai", true),
            ("'openai'", true),
            ("\"openai\"", true),
            (" 'openai' ", true),
            ("other", false),
            ("'other'", false),
            ("\"other\"", false),
            ("openai_other", false),
            ("\" openai \"", false),
            ("'openai'\nfeatures.daemon=true", false),
        ] {
            let config = format!("model_provider={value}");
            let inline = format!("--config={config}");
            for input in [
                args(&["-c", &config, "resume"]),
                args(&["--config", &config, "resume"]),
                args(&[&inline, "resume"]),
            ] {
                assert_eq!(managed_args(&input).is_ok(), allowed, "{input:?}");
            }
        }
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
        let first = shared_for(&a).unwrap();
        let second = shared_for(&a).unwrap();
        assert!(exclusive(&[b.clone(), a.clone()]).is_err());
        drop((first, second));
        let moving = exclusive(&[a.clone(), b.clone(), a.clone()]).unwrap();
        assert!(shared_for(&a).is_err());
        assert!(shared_for(&b).is_err());
        assert!(exclusive(&[b.clone(), a.clone()]).is_err());
        drop(moving);
        assert!(shared_for(&a).is_ok());
        assert!(shared_for(&b).is_ok());
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
                exclusive(std::slice::from_ref(&home)).unwrap()
            } else {
                shared_for(&home).unwrap()
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
                exclusive(std::slice::from_ref(&home)).unwrap()
            } else {
                shared_for(&home).unwrap()
            };
            assert_eq!(
                set_mount_point(&writer, &target)
                    .unwrap_err()
                    .raw_os_error(),
                Some(145)
            );
            drop(held);
            fs::remove_file(home.join(GATE)).unwrap();
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
