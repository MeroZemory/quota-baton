#![cfg(windows)]

use std::{fs, path::PathBuf, process::Command, time::Duration};
use windows::{
    Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{
            CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, PROCESS_INFORMATION,
            STARTF_USESHOWWINDOW, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
        },
    },
    core::{PCWSTR, PWSTR},
};

#[path = "../src/launcher_script.rs"]
mod launcher_script;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetConsoleTitleW(buffer: *mut u16, size: u32) -> u32;
    fn SetConsoleTitleW(title: *const u16) -> i32;
    fn GetConsoleCP() -> u32;
    fn GetConsoleOutputCP() -> u32;
    fn SetConsoleCP(cp: u32) -> i32;
    fn SetConsoleOutputCP(cp: u32) -> i32;
}
fn title() -> String {
    let mut b = vec![0; 32768];
    let n = unsafe { GetConsoleTitleW(b.as_mut_ptr(), b.len() as u32) };
    String::from_utf16_lossy(&b[..n as usize])
}
fn set_title(s: &str) {
    let s: Vec<_> = s.encode_utf16().chain(Some(0)).collect();
    assert_ne!(unsafe { SetConsoleTitleW(s.as_ptr()) }, 0);
}

// Helpers run only in the test's separate hidden console, never the user's tab.
#[test]
#[ignore = "invoked by r81_title_after_child_start_in_hidden_console"]
fn r81_console_probe() {
    let root = PathBuf::from(std::env::var_os("QB_R81_FIXTURE").unwrap());
    let mode = std::env::var("QB_R81_TITLE_MODE").unwrap();
    fs::write(root.join("initial-title"), title()).unwrap();
    fs::write(
        root.join("args.json"),
        serde_json::to_vec(&std::env::args().collect::<Vec<_>>()).unwrap(),
    )
    .unwrap();
    if mode == "own-script" {
        set_title(&format!(
            "{}\\npm\\codex.cmd - my-session",
            std::env::var("APPDATA").unwrap()
        ));
    } else if mode == "own" {
        set_title("client owns this title");
    } else if mode == "cmd" {
        set_title(&std::env::var("ComSpec").unwrap());
    } else if mode == "cmd-space" {
        set_title(&format!("{} ", std::env::var("ComSpec").unwrap()));
    } else if mode == "cmd-suffix" {
        set_title(&format!(
            "{} - node synthetic.js",
            std::env::var("ComSpec").unwrap()
        ));
    }
    fs::write(root.join("started"), "started").unwrap();
    fs::write(
        root.join("child-codepage.json"),
        serde_json::to_vec(&unsafe { [GetConsoleCP(), GetConsoleOutputCP()] }).unwrap(),
    )
    .unwrap();
    if mode == "own-late" {
        std::thread::sleep(Duration::from_millis(450));
        set_title("client owns this title");
        std::thread::sleep(Duration::from_millis(850));
    } else if mode == "cmd-late" {
        std::thread::sleep(Duration::from_millis(3400));
        set_title(&format!("{} ", std::env::var("ComSpec").unwrap()));
        std::thread::sleep(Duration::from_millis(1200));
    } else {
        std::thread::sleep(Duration::from_millis(650));
    }
    fs::write(root.join("title"), title()).unwrap();
    fs::write(
        root.join("home"),
        std::env::var_os("CODEX_HOME")
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    fs::write(
        root.join("label"),
        std::env::var("SYNTHETIC_LABEL").unwrap(),
    )
    .unwrap();
    std::process::exit(37);
}

#[test]
#[ignore = "invoked by r81_title_after_child_start_in_hidden_console"]
fn r81_console_host() {
    let root = PathBuf::from(std::env::var_os("QB_R81_FIXTURE").unwrap());
    unsafe {
        assert_ne!(SetConsoleCP(949), 0);
        assert_ne!(SetConsoleOutputCP(949), 0);
    }
    let launcher = root.join("codex-qb.cmd");
    fs::write(
        &launcher,
        launcher_script::codex_launcher_bytes(std::path::Path::new(env!(
            "CARGO_BIN_EXE_quota-baton"
        ))),
    )
    .unwrap();
    let mut child = Command::new(&launcher);
    for value in [
        "two words",
        "한글 ! ^ & (x)",
        "100% literal",
        "%QB_R81_LITERAL%",
        "ends\\",
        "quoted\"value",
    ] {
        child.arg("--skip").arg(value);
    }
    let status = child
        .current_dir(root.join("한글 project"))
        .env("USERPROFILE", root.join("profile"))
        .env(
            "APPDATA",
            root.join("AppData/Roaming")
                .to_string_lossy()
                .replace('/', "\\"),
        )
        .env("SYNTHETIC_LABEL", "retained-label")
        .env("QB_R81_LITERAL", "must-not-expand")
        // Deliberately invalid: launch-client must never open a core home.
        .env("QB_HOME", root.join("nonexistent-home"))
        .status()
        .unwrap();
    fs::write(root.join("status"), status.code().unwrap().to_string()).unwrap();
    fs::write(
        root.join("restored-codepage.json"),
        serde_json::to_vec(&unsafe { [GetConsoleCP(), GetConsoleOutputCP()] }).unwrap(),
    )
    .unwrap();
}

#[test]
fn r81_title_after_child_start_in_hidden_console() {
    let sandbox = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r81-tests");
    fs::create_dir_all(&sandbox).unwrap();
    for mode in [
        "actual",
        "cmd",
        "own",
        "own-late",
        "cmd-space",
        "cmd-suffix",
        "cmd-late",
        "actual-node",
        "own-script",
    ] {
        let node_dir = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .find(|p| p.is_absolute() && p.join("node.exe").is_file());
        if mode == "actual-node" && node_dir.is_none() {
            println!("actual-node unavailable: no node.exe on test PATH");
            continue;
        }
        let root = tempfile::tempdir_in(&sandbox).unwrap();
        let root = root.path().canonicalize().unwrap();
        let root = PathBuf::from(root.to_string_lossy().trim_start_matches(r"\\?\"));
        fs::create_dir(root.join("한글 project")).unwrap();
        fs::create_dir_all(root.join("AppData/Roaming/npm")).unwrap();
        let exe = std::env::current_exe().unwrap();
        fs::write(root.join("AppData/Roaming/npm/codex.cmd"), format!(
            "@echo off\r\necho shim>\"%QB_R81_FIXTURE%\\shim-ran\"\r\n\"{}\" --ignored --exact r81_console_probe --nocapture %*\r\nexit /b %errorlevel%\r\n", exe.display())).unwrap();
        if mode == "actual-node" {
            let script = root.join("AppData/Roaming/npm/node_modules/@openai/codex/bin/codex.js");
            fs::create_dir_all(script.parent().unwrap()).unwrap();
            fs::write(script, r#"const {spawnSync}=require('node:child_process');
const result=spawnSync(process.env.QB_R81_PROBE,['--ignored','--exact','r81_console_probe','--nocapture',...process.argv.slice(2)],{stdio:'inherit'});
process.exit(result.status ?? 1);
"#).unwrap();
        }
        let mut env: Vec<(String, String)> = ["SystemRoot", "ComSpec", "PATH", "TEMP", "TMP"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok().map(|v| (key.into(), v)))
            .collect();
        if mode == "actual-node" {
            let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
            let path = std::env::join_paths([node_dir.as_ref().unwrap(), &system]).unwrap();
            env.retain(|(key, _)| !key.eq_ignore_ascii_case("PATH"));
            env.push(("PATH".into(), path.to_string_lossy().into_owned()));
        }
        // This mode explicitly tests the resolved .exe, even if the machine's
        // Node directory also contains a .com wrapper. Other modes test fallback.
        env.push((
            "PATHEXT".into(),
            if mode == "actual-node" {
                ".EXE;.COM;.BAT;.CMD"
            } else {
                ".COM;.EXE;.BAT;.CMD"
            }
            .into(),
        ));
        env.push(("QB_R81_FIXTURE".into(), root.to_string_lossy().into_owned()));
        env.push(("QB_R81_TITLE_MODE".into(), mode.into()));
        env.push(("QB_R81_PROBE".into(), exe.to_string_lossy().into_owned()));
        env.sort_by_key(|(k, _)| k.to_ascii_lowercase());
        let environment: Vec<u16> = env
            .into_iter()
            .flat_map(|(k, v)| format!("{k}={v}\0").encode_utf16().collect::<Vec<_>>())
            .chain(Some(0))
            .collect();
        let app: Vec<_> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut command: Vec<_> = format!(
            "\"{}\" --ignored --exact r81_console_host --nocapture",
            exe.display()
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESHOWWINDOW,
            wShowWindow: 0,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(
                PCWSTR(app.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
                Some(environment.as_ptr().cast()),
                None,
                &startup,
                &mut process,
            )
            .unwrap();
            let wait = WaitForSingleObject(process.hProcess, 20000);
            if wait != WAIT_OBJECT_0 {
                let _ = TerminateProcess(process.hProcess, 1);
            }
            CloseHandle(process.hThread).unwrap();
            CloseHandle(process.hProcess).unwrap();
            assert_eq!(wait, WAIT_OBJECT_0, "hidden host timed out");
        }
        assert!(root.join("started").exists(), "child never started");
        assert_eq!(
            root.join("shim-ran").exists(),
            mode != "actual-node",
            "node resolution must bypass the npm cmd shim"
        );
        for (name, expected) in [
            ("child-codepage.json", [65001, 65001]),
            ("restored-codepage.json", [949, 949]),
        ] {
            let actual: [u32; 2] =
                serde_json::from_slice(&fs::read(root.join(name)).unwrap()).unwrap();
            assert_eq!(actual, expected, "{name}");
        }
        assert_eq!(fs::read_to_string(root.join("status")).unwrap(), "37");
        assert_eq!(
            fs::read_to_string(root.join("title")).unwrap(),
            if mode == "own-script" {
                format!(
                    "{}\\npm\\codex.cmd - my-session",
                    root.join("AppData/Roaming")
                        .to_string_lossy()
                        .replace('/', "\\")
                )
            } else if mode.starts_with("own") {
                "client owns this title".to_owned()
            } else {
                "codex-qb · 한글 project".to_owned()
            }
        );
        assert_eq!(
            fs::read_to_string(root.join("home")).unwrap(),
            root.join("profile/.codex-acct/qb")
                .to_string_lossy()
                .replace('/', "\\")
        );
        assert_eq!(
            fs::read_to_string(root.join("label")).unwrap(),
            "retained-label"
        );
        let args: Vec<String> =
            serde_json::from_slice(&fs::read(root.join("args.json")).unwrap()).unwrap();
        assert_eq!(
            &args[5..],
            [
                "--skip",
                "two words",
                "--skip",
                "한글 ! ^ & (x)",
                "--skip",
                "100% literal",
                "--skip",
                "%QB_R81_LITERAL%",
                "--skip",
                "ends\\",
                "--skip",
                "quoted\"value"
            ]
        );
        assert!(!root.join("nonexistent-home").exists());
        println!(
            "{mode}: generated launcher, post-start title, 12 forwarded arguments, exit 37, CODEX_HOME, label and 949/65001/restored949 verified"
        );
    }
}

use std::os::windows::ffi::OsStrExt;
