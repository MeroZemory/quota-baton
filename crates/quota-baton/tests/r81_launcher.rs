#![cfg(windows)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
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

struct LaunchCore {
    _runtime: tokio::runtime::Runtime,
    issued: Arc<AtomicUsize>,
    catalogs: Arc<AtomicUsize>,
    base_url: String,
}
impl LaunchCore {
    fn new(home: &Path, codex_home: &Path) -> Self {
        use axum::{
            Json, Router,
            http::HeaderMap,
            routing::{get, post},
        };
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!matches!(port, 10100 | 10101));
        let admin = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let client = URL_SAFE_NO_PAD.encode([2u8; 32]);
        let handle = URL_SAFE_NO_PAD.encode([3u8; 32]);
        fs::write(home.join("admin.token"), &admin).unwrap();
        fs::write(home.join("client.key"), &client).unwrap();
        fs::write(
            home.join("config.toml"),
            format!("[server]\ndata_port={port}\nadmin_port={port}\n"),
        )
        .unwrap();
        fs::write(
            codex_home.join("config.toml"),
            "model = \"synthetic-model\"\nmodel_provider = \"openai\"\n",
        )
        .unwrap();
        let issued = Arc::new(AtomicUsize::new(0));
        let catalogs = Arc::new(AtomicUsize::new(0));
        let issue_count = issued.clone();
        let catalog_count = catalogs.clone();
        let base = format!("http://127.0.0.1:{port}/l/{handle}/v1");
        let base_url = base.clone();
        let app = Router::new()
            .route("/admin/v1/launches", post(move |headers: HeaderMap, Json(request): Json<serde_json::Value>| {
                let (admin, base, count) = (admin.clone(), base.clone(), issue_count.clone());
                async move {
                    assert_eq!(headers.get("authorization").unwrap(), format!("Bearer {admin}").as_str());
                    assert_eq!(request["model"], "synthetic-model");
                    assert_eq!(request["model_is_explicit"], false);
                    assert_eq!(request["move_for_model"], false);
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"base_url":base,"catalog_url":format!("{base}/models"),
                        "account":"synthetic-launch-account","model":request["model"]}))
                }
            }))
            .route(&format!("/l/{handle}/v1/models"), get(move |headers: HeaderMap| {
                let (client, count) = (client.clone(), catalog_count.clone());
                async move {
                    assert_eq!(headers.get("authorization").unwrap(), format!("Bearer {client}").as_str());
                    assert_eq!(headers.get("accept-encoding").unwrap(), "identity");
                    count.fetch_add(1, Ordering::SeqCst);
                    ([ ("x-quota-baton-account", "synthetic-launch-account") ],
                        Json(serde_json::json!({"models":[{"slug":"synthetic-model"}]})))
                }
            }));
        runtime.spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _runtime: runtime,
            issued,
            catalogs,
            base_url,
        }
    }
    fn counts(&self) -> (usize, usize) {
        (
            self.issued.load(Ordering::SeqCst),
            self.catalogs.load(Ordering::SeqCst),
        )
    }
}

const EXISTING_ID: &str = "11111111-1111-1111-1111-111111111111";

fn native_args(mode: &str) -> Vec<&'static str> {
    if mode == "fresh-empty" {
        return vec![];
    }
    let mut args = if mode == "existing-resume" {
        vec!["resume", EXISTING_ID]
    } else {
        vec!["fix X"]
    };
    args.extend([
        "--enable",
        "new_feature",
        "-i",
        "one.png",
        "-i",
        "two.png",
        "--",
        // Preserve these transport/quoting operands as literal prompt values;
        // managed R11 launches reject actual profile/OSS/routing overrides.
        "--profile",
        "work",
        "--oss",
        "-c",
        "openai_base_url='http://synthetic.invalid/v1'",
        "-c",
        "model_catalog_url='http://synthetic.invalid/catalog'",
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
        "quoted\"value",
    ]);
    args
}

// Helpers run only in the test's separate hidden console, never the user's tab.
#[test]
#[ignore = "invoked by r81_title_after_child_start_in_hidden_console"]
fn r81_console_probe() {
    let root = PathBuf::from(std::env::var_os("QB_R81_FIXTURE").unwrap());
    let mode = std::env::var("QB_R81_TITLE_MODE").unwrap();
    fs::write(root.join("initial-title"), title()).unwrap();
    let mut args = std::env::args().collect::<Vec<_>>();
    // Only libtest's transport separator is removed; all native args are recorded.
    assert_eq!(args[5], "--");
    args.remove(5);
    fs::write(root.join("args.json"), serde_json::to_vec(&args).unwrap()).unwrap();
    let native = &args[5..];
    assert_eq!(native[0], "-c");
    assert_eq!(native[2], "-c");
    assert_eq!(&native[4..6], ["--model", "synthetic-model"]);
    let snapshot = native[3].parse::<toml_edit::DocumentMut>().unwrap();
    let snapshot = PathBuf::from(snapshot["model_catalog_json"].as_str().unwrap());
    assert!(snapshot.is_file(), "catalog must live through the child");
    let catalog: serde_json::Value = serde_json::from_slice(&fs::read(snapshot).unwrap()).unwrap();
    assert_eq!(
        catalog,
        serde_json::json!({"models":[{"slug":"synthetic-model"}]})
    );
    fs::write(root.join("catalog-checked"), "checked").unwrap();
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
    child.args(native_args(&std::env::var("QB_R81_TITLE_MODE").unwrap()));
    let status = child
        .current_dir(root.join("한글 project"))
        .env("USERPROFILE", root.join("profile"))
        .env("HOME", root.join("profile"))
        .env("CODEX_HOME", root.join("profile/.codex-acct/qb"))
        .env(
            "APPDATA",
            root.join("AppData/Roaming")
                .to_string_lossy()
                .replace('/', "\\"),
        )
        .env("SYNTHETIC_LABEL", "retained-label")
        .env("QB_R81_LITERAL", "must-not-expand")
        // Admission and R11 issuance use the parent's authenticated fixture core.
        .env("QB_HOME", root.join("qb"))
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
        "fresh-empty",
        "existing-resume",
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
        fs::create_dir_all(root.join("profile/.codex-acct/qb")).unwrap();
        fs::create_dir_all(root.join("profile/AppData/Local")).unwrap();
        fs::create_dir_all(root.join("temp")).unwrap();
        fs::create_dir_all(root.join("qb/journal")).unwrap();
        fs::write(root.join("qb/journal/wiring.json"), serde_json::to_vec(
            &serde_json::json!({"schema":3,"mode":"dedicated","install_completed":true,"home":root.join("profile/.codex-acct/qb")})
        ).unwrap()).unwrap();
        let core = LaunchCore::new(&root.join("qb"), &root.join("profile/.codex-acct/qb"));
        if mode == "existing-resume" {
            let home = root.join("profile/.codex-acct/qb");
            let rollout = home
                .join("sessions")
                .join(format!("rollout-{EXISTING_ID}.jsonl"));
            fs::create_dir_all(rollout.parent().unwrap()).unwrap();
            fs::write(&rollout, format!("{}\n", serde_json::json!({
                "type":"session_meta", "payload":{"id":EXISTING_ID,"model_provider":"openai"}
            }))).unwrap();
            let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
            db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,model_provider TEXT NOT NULL,archived INTEGER DEFAULT 0);").unwrap();
            db.execute(
                "INSERT INTO threads VALUES(?1,?2,'openai',0)",
                rusqlite::params![EXISTING_ID, rollout.to_str().unwrap()],
            )
            .unwrap();
        }
        let exe = std::env::current_exe().unwrap();
        fs::write(root.join("AppData/Roaming/npm/codex.cmd"), format!(
            "@echo off\r\nif \"%~1\"==\"--version\" (echo probe>\"%QB_R81_FIXTURE%\\version-probed\"& echo codex-cli 0.999.0& exit /b 0)\r\necho shim>\"%QB_R81_FIXTURE%\\shim-ran\"\r\n\"{}\" --ignored --exact r81_console_probe --nocapture -- %*\r\nexit /b %errorlevel%\r\n", exe.display())).unwrap();
        if mode == "actual-node" {
            let script = root.join("AppData/Roaming/npm/node_modules/@openai/codex/bin/codex.js");
            fs::create_dir_all(script.parent().unwrap()).unwrap();
            fs::write(script, r#"const {spawnSync}=require('node:child_process');
const args=process.argv.slice(2);
if(args.length===1 && args[0]==='--version'){require('node:fs').writeFileSync(process.env.QB_R81_FIXTURE+'/version-probed','probe');process.stdout.write('codex-cli 0.999.0\n');process.exit(0);}
const result=spawnSync(process.env.QB_R81_PROBE,['--ignored','--exact','r81_console_probe','--nocapture','--',...args],{stdio:'inherit'});
process.exit(result.status ?? 1);
"#).unwrap();
        }
        let mut env: Vec<(String, String)> = ["SystemRoot", "ComSpec", "PATH"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok().map(|v| (key.into(), v)))
            .collect();
        for key in ["TEMP", "TMP"] {
            env.push((key.into(), root.join("temp").to_string_lossy().into_owned()));
        }
        env.push((
            "LOCALAPPDATA".into(),
            root.join("profile/AppData/Local")
                .to_string_lossy()
                .into_owned(),
        ));
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
        assert!(
            root.join("started").exists(),
            "child never started: {mode}; status {:?}",
            fs::read_to_string(root.join("status"))
        );
        assert_eq!(
            core.counts(),
            (1, 1),
            "one authenticated issue and account-guarded catalog"
        );
        assert!(root.join("catalog-checked").exists());
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
        let native = &args[5..];
        assert_eq!(native[0], "-c");
        assert_eq!(native[2], "-c");
        assert_eq!(&native[4..6], ["--model", "synthetic-model"]);
        let routing = native[1].parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(routing.len(), 1);
        assert_eq!(
            routing["openai_base_url"].as_str(),
            Some(core.base_url.as_str())
        );
        let snapshot = native[3].parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(snapshot.len(), 1);
        let snapshot = PathBuf::from(snapshot["model_catalog_json"].as_str().unwrap());
        let catalog_directory = root
            .join("qb/state/launch-catalogs")
            .canonicalize()
            .unwrap();
        assert_eq!(snapshot.parent(), Some(catalog_directory.as_path()));
        assert!(
            !snapshot.exists(),
            "catalog must be released after the child exits"
        );
        assert_eq!(&native[6..], native_args(mode));
        let expect_version_probe =
            mode == "fresh-empty" && cfg!(all(feature = "testkit", debug_assertions));
        assert_eq!(
            root.join("version-probed").exists(),
            expect_version_probe,
            "{mode}: native capability version probe expectation"
        );
        assert!(!root.join("nonexistent-home").exists());
        println!(
            "{mode}: generated launcher, post-start title, exact forwarded arguments, version probe={expect_version_probe}, exit 37, CODEX_HOME, label and 949/65001/restored949 verified"
        );
    }
}

use std::os::windows::ffi::OsStrExt;
