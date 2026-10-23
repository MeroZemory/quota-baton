#![cfg(windows)]

use std::{
    fs,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        System::{
            Console::{
                CONSOLE_SCREEN_BUFFER_INFO, COORD, GetConsoleScreenBufferInfo, GetStdHandle,
                INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0,
                ReadConsoleOutputCharacterW, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
                WriteConsoleInputW,
            },
            Threading::{
                CREATE_NEW_CONSOLE, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
                GetExitCodeProcess, PROCESS_INFORMATION, ResumeThread, STARTF_USESHOWWINDOW,
                STARTUPINFOW, TerminateProcess, WaitForSingleObject,
            },
        },
    },
    core::{PCWSTR, PWSTR},
};

// JobObjects is not a workspace windows feature; these three bindings keep the
// timeout cleanup confined to this host and its descendants without Cargo edits.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const std::ffi::c_void, name: *const u16) -> HANDLE;
    fn AssignProcessToJobObject(job: HANDLE, process: HANDLE) -> i32;
    fn TerminateJobObject(job: HANDLE, code: u32) -> i32;
}

const ERROR: &str = "실패했습니다: 실행 옵션을 안전하게 해석할 수 없습니다.";
const PROMPT: &str = "실행하지 못했습니다. 위 오류를 확인한 뒤 Enter 키를 누르면 닫습니다.";

fn unspaced(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Host {
    process: PROCESS_INFORMATION,
    job: HANDLE,
}
impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            // Also covers assertions/panics and a host killed while its child waits.
            let _ = TerminateJobObject(self.job, 1);
            let _ = TerminateProcess(self.process.hProcess, 1);
            let _ = WaitForSingleObject(self.process.hProcess, 1000);
            let _ = CloseHandle(self.process.hThread);
            let _ = CloseHandle(self.process.hProcess);
            let _ = CloseHandle(self.job);
        }
    }
}

fn launcher(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
    command
        .env_clear()
        .current_dir(root)
        .args(["launch-client", "--client", "codex", "--command"])
        .arg(root.join("never-native.exe"))
        .args(["--", "--r13-g-unknown-option"]);
    if let Some(system) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system);
    }
    for key in [
        "QB_HOME",
        "HOME",
        "USERPROFILE",
        "CODEX_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        command.env(key, root.join("untouched-profile"));
    }
    command
}

fn screen() -> String {
    unsafe {
        let output = GetStdHandle(STD_OUTPUT_HANDLE).unwrap();
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        GetConsoleScreenBufferInfo(output, &mut info).unwrap();
        let mut text = String::new();
        // Strip console row padding so a Korean sentence wrapping at any column
        // still compares as Unicode text, independently of the output code page.
        for y in 0..=info.dwCursorPosition.Y {
            let mut row = vec![0; info.dwSize.X as usize];
            let mut read = 0;
            ReadConsoleOutputCharacterW(output, &mut row, COORD { X: 0, Y: y }, &mut read).unwrap();
            text.push_str(String::from_utf16_lossy(&row[..read as usize]).trim_end());
        }
        text
    }
}

fn wait_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "launcher did not exit within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// This helper alone touches console handles, in a new hidden console, never the
// user's console. The ordinary test invokes this exact ignored libtest entry.
#[test]
#[ignore = "invoked by r13_g_launcher_error_visibility_in_hidden_console"]
fn r13_g_launcher_error_host() {
    let root = PathBuf::from(std::env::var_os("QB_R13_G_FIXTURE").unwrap());
    let mut child = OwnedChild(launcher(&root).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(8);
    let text = loop {
        let text = screen();
        if unspaced(&text).contains(&unspaced(ERROR)) && unspaced(&text).contains(&unspaced(PROMPT))
        {
            break text;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "exited before Enter: {text}"
        );
        assert!(Instant::now() < deadline, "error/prompt missing: {text}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        unspaced(&text).find(&unspaced(ERROR)).unwrap()
            < unspaced(&text).find(&unspaced(PROMPT)).unwrap()
    );
    // Observing the prompt alone does not prove the launcher actually waits.
    std::thread::sleep(Duration::from_millis(350));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "did not hold for Enter"
    );
    unsafe {
        let events = [true, false].map(|down| INPUT_RECORD {
            EventType: KEY_EVENT as u16,
            Event: INPUT_RECORD_0 {
                KeyEvent: KEY_EVENT_RECORD {
                    bKeyDown: down.into(),
                    wRepeatCount: 1,
                    wVirtualKeyCode: 0x0d,
                    wVirtualScanCode: 0x1c,
                    uChar: KEY_EVENT_RECORD_0 { UnicodeChar: 13 },
                    dwControlKeyState: 0,
                },
            },
        });
        let mut written = 0;
        WriteConsoleInputW(
            GetStdHandle(STD_INPUT_HANDLE).unwrap(),
            &events,
            &mut written,
        )
        .unwrap();
        assert_eq!(written, events.len() as u32);
    }
    let status = wait_exit(&mut child.0, Duration::from_secs(2));
    assert_eq!(status.code(), Some(1));
    let mut evidence = vec![serde_json::json!({
        "mode": "console", "console_text": text,
        "alive_before_enter": true, "enter_records": 2, "exit_code": status.code()
    })];

    // Exercise each terminal gate separately, plus both redirected. Keep the
    // stdin pipe OPEN: accidental read_line must time out instead of seeing EOF.
    for (mode, redirect_in, redirect_err) in [
        ("stdin", true, false),
        ("stderr", false, true),
        ("both", true, true),
    ] {
        let stderr = root.join(format!("{mode}.stderr"));
        let mut command = launcher(&root);
        if redirect_in {
            command.stdin(Stdio::piped());
        }
        if redirect_err {
            command.stderr(fs::File::create(&stderr).unwrap());
        }
        let before = screen();
        let start = Instant::now();
        let mut child = OwnedChild(command.spawn().unwrap());
        let status = wait_exit(&mut child.0, Duration::from_secs(2));
        assert_eq!(status.code(), Some(1));
        let error = if redirect_err {
            fs::read_to_string(&stderr).unwrap()
        } else {
            screen().strip_prefix(&before).unwrap().to_owned()
        };
        assert!(
            unspaced(&error).contains(&unspaced(ERROR)),
            "missing grammar refusal ({mode}): {error}"
        );
        assert!(
            !unspaced(&error).contains(&unspaced(PROMPT)),
            "non-TTY prompt ({mode}): {error}"
        );
        evidence.push(serde_json::json!({
            "mode": mode, "stderr": error, "exit_code": status.code(),
            "elapsed_ms": start.elapsed().as_millis(), "stdin_pipe_held_open": redirect_in
        }));
    }
    assert!(!root.join("untouched-profile").exists());
    fs::write(
        root.join("evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}

#[test]
fn r13_g_launcher_error_visibility_in_hidden_console() {
    let sandbox = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox");
    let fixtures = sandbox.join("r13-g-launcher-error");
    fs::create_dir_all(&fixtures).unwrap();
    let fixture = tempfile::tempdir_in(&fixtures).unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let root = PathBuf::from(root.to_string_lossy().trim_start_matches(r"\\?\"));
    let exe = std::env::current_exe().unwrap();
    let app: Vec<_> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command: Vec<_> = format!(
        "\"{}\" --ignored --exact r13_g_launcher_error_host --nocapture --test-threads=1",
        exe.display()
    )
    .encode_utf16()
    .chain(Some(0))
    .collect();
    let mut env = vec![(
        "QB_R13_G_FIXTURE".to_owned(),
        root.to_string_lossy().into_owned(),
    )];
    if let Ok(system) = std::env::var("SystemRoot") {
        env.push(("SystemRoot".into(), system));
    }
    for key in [
        "QB_HOME",
        "HOME",
        "USERPROFILE",
        "CODEX_HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
    ] {
        env.push((key.into(), root.to_string_lossy().into_owned()));
    }
    env.sort_by_key(|(key, _)| key.to_ascii_lowercase());
    let environment: Vec<u16> = env
        .into_iter()
        .flat_map(|(key, value)| {
            format!("{key}={value}\0")
                .encode_utf16()
                .collect::<Vec<_>>()
        })
        .chain(Some(0))
        .collect();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: 0,
        ..Default::default()
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        assert!(
            !job.is_invalid(),
            "CreateJobObjectW: {}",
            std::io::Error::last_os_error()
        );
        let mut host = Host {
            process: PROCESS_INFORMATION::default(),
            job,
        };
        CreateProcessW(
            PCWSTR(app.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            Some(environment.as_ptr().cast()),
            None,
            &startup,
            &mut host.process,
        )
        .unwrap();
        // Assign before resume so even an immediate host failure cannot orphan
        // a real launcher process waiting on console input.
        assert_ne!(AssignProcessToJobObject(job, host.process.hProcess), 0);
        assert_ne!(ResumeThread(host.process.hThread), u32::MAX);
        assert_eq!(
            WaitForSingleObject(host.process.hProcess, 20000),
            WAIT_OBJECT_0,
            "hidden console host exceeded 20 seconds"
        );
        let mut code = 0;
        GetExitCodeProcess(host.process.hProcess, &mut code).unwrap();
        assert_eq!(code, 0, "hidden host failed; fixture: {}", root.display());
    }
    let evidence = fs::read(root.join("evidence.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&evidence).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 4);
    let reports = sandbox.join("reports");
    fs::create_dir_all(&reports).unwrap();
    let report = reports.join(format!("r13-g-launcher-error-{}.json", std::process::id()));
    fs::write(&report, evidence).unwrap();
    println!("launcher error evidence: {}", report.display());
}
