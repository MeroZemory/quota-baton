//! Actual CLI process termination/recovery in project-local synthetic homes.
//! D21 panel 2: continuous public-home exclusion, hardlink reachability,
//! durable managed admission, directional recovery, and conflict preservation.
#![cfg(all(windows, feature = "testkit", debug_assertions))]

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant, SystemTime},
};

const ROOT: &str = "11111111-1111-1111-1111-111111111111";
const SEGMENT_A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const SEGMENT_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const OTHER: &str = "22222222-2222-2222-2222-222222222222";
const CHILD: &str = "33333333-3333-3333-3333-333333333333";
const GRANDCHILD: &str = "44444444-4444-4444-4444-444444444444";
const BODY: &str = "SYNTHETIC_R9_CRASH_BODY";
const OLD_OBJECTIVE: &str = "SYNTHETIC_R9_OLD_OBJECTIVE";
const CURRENT_OBJECTIVE: &str = "SYNTHETIC_R9_CURRENT_OBJECTIVE";
const OTHER_OBJECTIVE: &str = "SYNTHETIC_R9_OTHER_OBJECTIVE";
const CURRENT_MARKER: &str = "SYNTHETIC_R9_CURRENT_ROLLOUT";
const COMMAND_DEADLINE: Duration = Duration::from_secs(20);
const REAP_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
struct Goal {
    objective: String,
    status: String,
    updated_at_ms: i64,
}

fn original_goal() -> Goal {
    Goal {
        objective: OLD_OBJECTIVE.into(),
        status: "active".into(),
        updated_at_ms: 17,
    }
}

fn current_goal() -> Goal {
    Goal {
        objective: CURRENT_OBJECTIVE.into(),
        status: "complete".into(),
        updated_at_ms: 999,
    }
}

fn database(path: &Path) -> Connection {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.busy_timeout(Duration::from_secs(2)).unwrap();
    db
}

fn has_table(db: &Connection, name: &str) -> bool {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )
    .unwrap()
}

fn thread_row(home: &Path, id: &str) -> Option<(String, String, i64)> {
    let path = home.join("state_5.sqlite");
    if !path.exists() {
        return None;
    }
    let db = database(&path);
    if !has_table(&db, "threads") {
        return None;
    }
    db.query_row(
        "SELECT rollout_path,model_provider,archived FROM threads WHERE id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
    .unwrap()
}

fn goal(home: &Path, id: &str) -> Option<Goal> {
    let path = home.join("goals_1.sqlite");
    if !path.exists() {
        return None;
    }
    let db = database(&path);
    if !has_table(&db, "thread_goals") {
        return None;
    }
    db.query_row(
        "SELECT objective,status,updated_at_ms FROM thread_goals WHERE thread_id=?1",
        [id],
        |row| {
            Ok(Goal {
                objective: row.get(0)?,
                status: row.get(1)?,
                updated_at_ms: row.get(2)?,
            })
        },
    )
    .optional()
    .unwrap()
}

fn move_journal(qb: &Path) -> Value {
    let bytes = fs::read(
        qb.join("journal/session-moves")
            .join(format!("{ROOT}.json")),
    )
    .unwrap();
    // Inspect only envelope metadata, never decrypt private journal snapshots.
    let envelope: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope["schema"], 2);
    assert!(envelope["open"].is_boolean());
    assert!(
        envelope["payload"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    let text = String::from_utf8(bytes).unwrap();
    for private in [
        BODY,
        OLD_OBJECTIVE,
        CURRENT_OBJECTIVE,
        OTHER_OBJECTIVE,
        CURRENT_MARKER,
    ] {
        assert!(
            !text.contains(private),
            "journal exposed private fixture contents"
        );
    }
    envelope
}

fn physical_id(path: &Path) -> (u32, u64) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
    };
    let file = fs::File::open(path).unwrap();
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns the live handle and information is writable output.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }.unwrap();
    (
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    )
}

fn root_names(directory: &Path) -> Vec<PathBuf> {
    let mut pending = vec![directory.to_path_buf()];
    let mut names = Vec::new();
    while let Some(dir) = pending.pop() {
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            assert!(!kind.is_symlink(), "unexpected fixture symlink");
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl") {
                let file = fs::File::open(entry.path()).unwrap();
                let mut first = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut first)
                    .unwrap();
                if serde_json::from_str::<Value>(&first)
                    .ok()
                    .is_some_and(|v| v["type"] == "session_meta" && v["payload"]["id"] == ROOT)
                {
                    names.push(entry.path());
                }
            }
        }
    }
    names.sort();
    names
}

fn public_names(home: &Path) -> Vec<PathBuf> {
    ["sessions", "archived_sessions"]
        .iter()
        .flat_map(|top| root_names(&home.join(top)))
        .collect()
}

struct Process {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
    reaped: bool,
}

impl Process {
    fn diagnostics(&self) -> String {
        fn text(path: &Path) -> String {
            let Ok(file) = fs::File::open(path) else {
                return "<no capture>".into();
            };
            let mut bytes = Vec::new();
            file.take(64 * 1024).read_to_end(&mut bytes).unwrap();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        let out = text(&self.stdout);
        let err = text(&self.stderr);
        for private in [
            BODY,
            OLD_OBJECTIVE,
            CURRENT_OBJECTIVE,
            OTHER_OBJECTIVE,
            CURRENT_MARKER,
        ] {
            assert!(
                !out.contains(private) && !err.contains(private),
                "CLI printed synthetic private content"
            );
        }
        format!("stdout: {out}\nstderr: {err}")
    }

    fn wait_until(&mut self, deadline: Instant) -> Option<ExitStatus> {
        loop {
            if self.child.try_wait().unwrap().is_some() {
                // try_wait observed termination; wait now only reaps/copies its
                // cached exit status and cannot wait on a still-running child.
                let status = self.child.wait().unwrap();
                self.reaped = true;
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill_and_reap(&mut self) {
        assert!(!self.reaped);
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "checkpoint child exited before kill: {}",
            self.diagnostics()
        );
        // Child::kill targets this owned process handle, never a discovered PID.
        self.child.kill().unwrap();
        let status = self
            .wait_until(Instant::now() + REAP_DEADLINE)
            .unwrap_or_else(|| {
                panic!(
                    "owned child did not terminate after kill: {}",
                    self.diagnostics()
                )
            });
        assert!(!status.success(), "terminated child unexpectedly succeeded");
    }

    fn finish(mut self, success: bool) -> String {
        let status = match self.wait_until(Instant::now() + COMMAND_DEADLINE) {
            Some(status) => status,
            None => {
                self.kill_and_reap();
                panic!("CLI exceeded command deadline: {}", self.diagnostics());
            }
        };
        assert!(
            status.success() == success,
            "unexpected CLI status ({status}): {}",
            self.diagnostics()
        );
        self.diagnostics()
    }

    fn success(self) {
        let _ = self.finish(true);
    }

    fn failure(self) -> String {
        self.finish(false)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        // Panic/error cleanup is bounded too. Only reap after observed exit.
        let deadline = Instant::now() + REAP_DEADLINE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    let _ = self.child.wait();
                    break;
                }
                Err(_) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => break,
            }
        }
    }
}

struct Fixture {
    root: tempfile::TempDir,
    user: PathBuf,
    appdata: PathBuf,
    source: PathBuf,
    target: PathBuf,
    qb: PathBuf,
    map_path: PathBuf,
    original_map: Value,
    segments: [PathBuf; 2],
    contents: [Vec<u8>; 2],
    identities: [(u32, u64); 2],
    other: PathBuf,
    other_contents: Vec<u8>,
    original_index: Vec<u8>,
    next_log: AtomicUsize,
}

impl Fixture {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r9-crash-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        assert!(
            root.path()
                .components()
                .any(|part| part.as_os_str() == ".sandbox")
        );
        let user = root.path().join("user");
        let appdata = user.join("AppData/Roaming");
        let source = user.join(".codex");
        let target = user.join(".codex-acct/qb");
        let qb = root.path().join("qb");
        let map_path = appdata.join("herdr/session-homes.json");
        for dir in [
            &source,
            &target,
            &qb.join("journal"),
            map_path.parent().unwrap(),
            &root.path().join("logs"),
            &appdata.join("npm"),
            &user.join("AppData/Local"),
            &root.path().join("temp"),
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(
            appdata.join("npm/codex.cmd"),
            b"@echo off\r\necho codex-cli 0.163.0\r\n",
        )
        .unwrap();
        fs::write(
            root.path().join("client.cmd"),
            b"@echo off\r\nif \"%~1\"==\"--version\" (echo codex-cli 0.163.0& exit /b 0)\r\necho synthetic-launch>\"%QB_PANEL2_SENTINEL%\"\r\nexit /b 0\r\n",
        )
        .unwrap();
        fs::write(
            qb.join("journal/wiring.json"),
            serde_json::to_vec(
                &json!({"schema":3,"mode":"dedicated","install_completed":true,"home":target}),
            )
            .unwrap(),
        )
        .unwrap();
        let a = write_rollout(&source, ROOT, SEGMENT_A, None);
        let b = write_rollout(&source, ROOT, SEGMENT_B, Some(SEGMENT_A));
        let other = write_rollout(&source, OTHER, OTHER, None);
        {
            let db = Connection::open(source.join("state_5.sqlite")).unwrap();
            db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,model_provider TEXT NOT NULL,archived INTEGER DEFAULT 0); CREATE INDEX idx_thread_provider ON threads(model_provider);").unwrap();
            for (id, path) in [(ROOT, &b), (OTHER, &other)] {
                db.execute(
                    "INSERT INTO threads VALUES(?1,?2,'openai',0)",
                    params![id, path.to_str().unwrap()],
                )
                .unwrap();
            }
        }
        {
            let db = Connection::open(source.join("goals_1.sqlite")).unwrap();
            db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE thread_goals(thread_id TEXT PRIMARY KEY,objective TEXT NOT NULL,status TEXT NOT NULL,updated_at_ms INTEGER NOT NULL);").unwrap();
            for (id, objective, status, updated) in [
                (ROOT, OLD_OBJECTIVE, "active", 17),
                (OTHER, OTHER_OBJECTIVE, "paused", 23),
            ] {
                db.execute(
                    "INSERT INTO thread_goals VALUES(?1,?2,?3,?4)",
                    params![id, objective, status, updated],
                )
                .unwrap();
            }
        }
        let original_index = format!("{}\n{}\n", json!({"id":ROOT,"thread_name":"synthetic root","updated_at":"2026-10-20T00:00:00Z"}), json!({"id":OTHER,"thread_name":"synthetic unrelated","updated_at":"2026-10-20T00:00:00Z"})).into_bytes();
        fs::write(source.join("session_index.jsonl"), &original_index).unwrap();
        let original_map = json!({"sessions": {ROOT: source, OTHER: source}, "future": {"preserve": [true, "synthetic"]}});
        fs::write(&map_path, serde_json::to_vec(&original_map).unwrap()).unwrap();
        let contents = [fs::read(&a).unwrap(), fs::read(&b).unwrap()];
        let identities = [physical_id(&a), physical_id(&b)];
        let other_contents = fs::read(&other).unwrap();
        Self {
            root,
            user,
            appdata,
            source,
            target,
            qb,
            map_path,
            original_map,
            segments: [a, b],
            contents,
            identities,
            other,
            other_contents,
            original_index,
            next_log: AtomicUsize::new(0),
        }
    }

    fn import_args(&self) -> Vec<OsString> {
        vec![
            "sessions".into(),
            "import".into(),
            "--from".into(),
            self.source.as_os_str().to_owned(),
            ROOT.into(),
            "--apply".into(),
            "--yes".into(),
        ]
    }

    fn unimport_args(&self) -> Vec<OsString> {
        vec![
            "sessions".into(),
            "unimport".into(),
            ROOT.into(),
            "--apply".into(),
            "--yes".into(),
        ]
    }

    fn return_args(&self) -> Vec<OsString> {
        vec![
            "sessions".into(),
            "return".into(),
            ROOT.into(),
            "--to".into(),
            self.source.as_os_str().to_owned(),
            "--yes".into(),
        ]
    }

    fn spawn(&self, args: &[OsString], checkpoint: Option<(&str, SocketAddr)>) -> Process {
        let serial = self.next_log.fetch_add(1, Ordering::Relaxed);
        let stdout = self.root.path().join(format!("logs/{serial}.stdout.log"));
        let stderr = self.root.path().join(format!("logs/{serial}.stderr.log"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
        command.env_clear();
        for key in [
            "SystemRoot",
            "WINDIR",
            "PATH",
            "PATHEXT",
            "COMSPEC",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .args(args)
            .env("QB_HOME", &self.qb)
            .env("USERPROFILE", &self.user)
            .env("HOME", &self.user)
            .env("APPDATA", &self.appdata)
            .env("LOCALAPPDATA", self.user.join("AppData/Local"))
            .env("TEMP", self.root.path().join("temp"))
            .env("TMP", self.root.path().join("temp"))
            .env("NO_PROXY", "*")
            .env("HTTP_PROXY", "")
            .env("HTTPS_PROXY", "")
            .env("ALL_PROXY", "")
            .env("QB_PANEL2_SENTINEL", self.root.path().join("launched.txt"))
            .env("CODEX_HOME", &self.target)
            .env_remove("QB_R9_CHECKPOINT")
            .env_remove("QB_R9_CHECKPOINT_NOTIFY")
            .current_dir(self.root.path())
            .creation_flags(0x08000000 | 0x00004000)
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap());
        if let Some((label, address)) = checkpoint {
            command
                .env("QB_R9_CHECKPOINT", label)
                .env("QB_R9_CHECKPOINT_NOTIFY", address.to_string());
        }
        Process {
            child: command.spawn().unwrap(),
            stdout,
            stderr,
            reaped: false,
        }
    }

    fn run(&self, args: &[OsString]) {
        self.spawn(args, None).success();
    }

    fn kill_at(&self, args: &[OsString], label: &str, while_parked: impl FnOnce()) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        assert!(!matches!(address.port(), 10100 | 10101));
        listener.set_nonblocking(true).unwrap();
        let mut process = self.spawn(args, Some((label, address)));
        let deadline = Instant::now() + COMMAND_DEADLINE;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    assert!(peer.ip().is_loopback());
                    break stream;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("checkpoint accept failed: {error}"),
            }
            assert!(
                process.child.try_wait().unwrap().is_none(),
                "CLI exited before {label}: {}",
                process.diagnostics()
            );
            if Instant::now() >= deadline {
                process.kill_and_reap();
                panic!("CLI did not reach {label}: {}", process.diagnostics());
            }
            thread::sleep(Duration::from_millis(10));
        };
        stream.set_nonblocking(true).unwrap();
        let read_deadline = Instant::now() + Duration::from_secs(2);
        let mut notification = Vec::new();
        let mut buffer = [0_u8; 128];
        loop {
            assert!(
                Instant::now() < read_deadline,
                "checkpoint {label} notification exceeded read deadline"
            );
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    notification.extend_from_slice(&buffer[..count]);
                    assert!(
                        notification.len() <= 256,
                        "oversized checkpoint notification"
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("checkpoint notification read failed: {error}"),
            }
        }
        assert_eq!(
            notification.as_slice(),
            label.as_bytes(),
            "wrong checkpoint notification"
        );
        self.assert_namespace();
        while_parked();
        process.kill_and_reap();
        self.assert_namespace();
        let _ = process.diagnostics();
    }

    fn q_name(&self, source: &Path) -> PathBuf {
        self.target.join(source.strip_prefix(&self.source).unwrap())
    }

    fn recover_args(&self) -> Vec<OsString> {
        vec!["sessions".into(), "recover".into(), ROOT.into()]
    }

    fn launch_args(&self, args: &[&str]) -> Vec<OsString> {
        let mut result = vec![
            "launch-client".into(),
            "--client".into(),
            "codex".into(),
            "--command".into(),
            self.root.path().join("client.cmd").into_os_string(),
            "--".into(),
        ];
        result.extend(args.iter().map(|arg| OsString::from(*arg)));
        result
    }

    fn assert_namespace(&self) {
        let homes = [&self.source, &self.target]
            .iter()
            .filter(|home| !public_names(home).is_empty())
            .count();
        assert!(
            homes <= 1,
            "root is discoverable in both homes, including after process death"
        );
        let all = root_names(self.root.path());
        for identity in self.identities {
            assert!(
                all.iter().any(|path| physical_id(path) == identity),
                "lost the last hardlink of a physical rollout, including private staging"
            );
        }
    }

    fn assert_boundary_state(&self, label: &str, reverse: bool) {
        self.assert_namespace();
        let (from, to) = if reverse {
            (&self.target, &self.source)
        } else {
            (&self.source, &self.target)
        };
        let source_count = public_names(from).len();
        let target_count = public_names(to).len();
        match label {
            "stage:prepared" | "stage:staged" => {
                assert!(source_count >= 2);
                assert_eq!(target_count, 0);
            }
            "stage:source_name_removed" => {
                assert_eq!(source_count, 1, "exercise partial multi-page retirement");
                assert_eq!(target_count, 0);
            }
            "stage:source_names_removed" | "stage:source_rows_removed" | "stage:decided" => {
                assert_eq!(source_count, 0);
                assert_eq!(target_count, 0);
            }
            "stage:published" => {
                assert_eq!(source_count, 0);
                assert_eq!(
                    target_count, 1,
                    "exercise partial multi-physical publication"
                );
            }
            "stage:target_rows" | "stage:map_committed" | "stage:committed" => {
                assert_eq!(source_count, 0);
                assert_eq!(target_count, 2);
            }
            _ => panic!("unknown acceptance boundary"),
        }
    }

    fn assert_managed_admission(&self) {
        if move_journal(&self.qb)["open"] == false {
            return;
        }
        let before = self.protected_state();
        for args in [
            vec!["resume", ROOT],
            vec!["exec", "resume", ROOT],
            vec!["resume", SEGMENT_A],
            vec!["resume", SEGMENT_B],
        ] {
            let sentinel = self.root.path().join("launched.txt");
            assert!(!sentinel.exists());
            let diagnostic = self.spawn(&self.launch_args(&args), None).failure();
            assert!(
                diagnostic.contains("복구") || diagnostic.contains("recover"),
                "wrong refusal"
            );
            assert!(!sentinel.exists());
            assert!(
                self.protected_state() == before,
                "ID refusal changed session state"
            );
        }
        for args in [
            vec![],
            vec!["resume", "--last"],
            vec!["resume"],
            vec!["app-server"],
            vec!["fresh synthetic prompt"],
        ] {
            self.run(&self.launch_args(&args));
            let sentinel = self.root.path().join("launched.txt");
            assert!(
                sentinel.exists(),
                "fresh/unbound launch was blocked by another ID's journal"
            );
            fs::remove_file(sentinel).unwrap();
            assert!(
                self.protected_state() == before,
                "fresh recorder changed session state"
            );
        }
    }

    fn assert_launcher_control(&self) {
        self.run(&self.launch_args(&["--help"]));
        let sentinel = self.root.path().join("launched.txt");
        assert!(sentinel.exists(), "client marker control did not launch");
        fs::remove_file(sentinel).unwrap();
    }

    fn protected_state(&self) -> ProtectedState {
        let names = root_names(self.root.path())
            .into_iter()
            .map(|path| {
                let identity = physical_id(&path);
                let bytes = fs::read(&path).unwrap();
                (path, identity, bytes)
            })
            .collect();
        ProtectedState {
            rows: [
                thread_row(&self.source, ROOT),
                thread_row(&self.target, ROOT),
                thread_row(&self.source, OTHER),
                thread_row(&self.target, OTHER),
            ],
            goals: [
                goal(&self.source, ROOT),
                goal(&self.target, ROOT),
                goal(&self.source, OTHER),
                goal(&self.target, OTHER),
            ],
            names,
            map: fs::read(&self.map_path).unwrap(),
            indices: [
                fs::read(self.source.join("session_index.jsonl")).ok(),
                fs::read(self.target.join("session_index.jsonl")).ok(),
            ],
            other: fs::read(&self.other).unwrap(),
        }
    }

    fn import_and_change_current_goal(&self) -> [Vec<u8>; 2] {
        self.run(&self.import_args());
        assert!(self.segments.iter().all(|path| !path.exists()));
        assert!(thread_row(&self.source, ROOT).is_none());
        assert!(goal(&self.source, ROOT).is_none());
        let db = Connection::open(self.target.join("goals_1.sqlite")).unwrap();
        db.busy_timeout(Duration::from_secs(2)).unwrap();
        let current = current_goal();
        assert_eq!(db.execute("UPDATE thread_goals SET objective=?1,status=?2,updated_at_ms=?3 WHERE thread_id=?4", params![current.objective, current.status, current.updated_at_ms, ROOT]).unwrap(), 1);
        drop(db);
        let path = self.q_name(&self.segments[1]);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"event_msg","payload":{"type":"user_message","message":CURRENT_MARKER}})
        )
        .unwrap();
        file.set_times(
            fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)),
        )
        .unwrap();
        drop(file);
        assert!(goal(&self.target, ROOT) == Some(current_goal()));
        self.assert_namespace();
        [
            fs::read(self.q_name(&self.segments[0])).unwrap(),
            fs::read(path).unwrap(),
        ]
    }

    fn assert_owner(
        &self,
        target_owns: bool,
        expected_goal: Goal,
        other_archived: i64,
        contents: &[Vec<u8>; 2],
    ) {
        self.assert_namespace();
        let (owner, empty) = if target_owns {
            (&self.target, &self.source)
        } else {
            (&self.source, &self.target)
        };
        let names = public_names(owner);
        assert_eq!(names.len(), 2);
        assert!(public_names(empty).is_empty());
        for (index, identity) in self.identities.iter().enumerate() {
            let name = names
                .iter()
                .find(|path| physical_id(path) == *identity)
                .expect("physical rollout missing from owning home");
            assert!(
                fs::read(name).unwrap() == contents[index],
                "recovery lost or rewound rollout contents"
            );
        }
        let (path, provider, archived) = thread_row(owner, ROOT).expect("owning row absent");
        assert_eq!(physical_id(Path::new(&path)), self.identities[1]);
        assert_eq!(provider, "openai");
        assert_eq!(archived, 0);
        assert!(goal(owner, ROOT) == Some(expected_goal));
        assert!(thread_row(empty, ROOT).is_none());
        assert!(goal(empty, ROOT).is_none());
        let other = thread_row(&self.source, OTHER).unwrap();
        assert_eq!(physical_id(Path::new(&other.0)), physical_id(&self.other));
        assert_eq!(other.1, "openai");
        assert_eq!(other.2, other_archived);
        assert!(
            goal(&self.source, OTHER)
                == Some(Goal {
                    objective: OTHER_OBJECTIVE.into(),
                    status: "paused".into(),
                    updated_at_ms: 23
                })
        );
        assert!(fs::read(&self.other).unwrap() == self.other_contents);
        assert!(
            fs::read(self.source.join("session_index.jsonl"))
                .unwrap()
                .starts_with(&self.original_index),
            "append-only source index was rewritten"
        );
        let mut expected_map = self.original_map.clone();
        let map: Value = serde_json::from_slice(&fs::read(&self.map_path).unwrap()).unwrap();
        assert_eq!(
            fs::canonicalize(Path::new(map["sessions"][ROOT].as_str().unwrap())).unwrap(),
            fs::canonicalize(owner).unwrap()
        );
        expected_map["sessions"][ROOT] = map["sessions"][ROOT].clone();
        assert!(map == expected_map, "unrelated map fields were changed");
        assert_eq!(move_journal(&self.qb)["open"], false);
        assert_integrity(self.root.path());
    }
}

#[derive(PartialEq, Eq)]
struct ProtectedState {
    rows: [Option<(String, String, i64)>; 4],
    goals: [Option<Goal>; 4],
    names: Vec<(PathBuf, (u32, u64), Vec<u8>)>,
    map: Vec<u8>,
    indices: [Option<Vec<u8>>; 2],
    other: Vec<u8>,
}

fn write_rollout(home: &Path, id: &str, physical: &str, base: Option<&str>) -> PathBuf {
    let path = home
        .join("sessions/2026/10/21")
        .join(format!("rollout-2026-10-21T00-00-00-{physical}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut meta = json!({"type":"session_meta","payload":{"id":id,"model_provider":"openai"}});
    if let Some(base) = base {
        meta["payload"]["history_base"] =
            json!({"thread_id":base,"end_ordinal_exclusive":1,"end_byte_offset":0});
    }
    fs::write(
        &path,
        format!(
            "{meta}\n{}\n",
            json!({"type":"event_msg","payload":{"type":"user_message","message":BODY}})
        ),
    )
    .unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)))
        .unwrap();
    path
}

fn write_descendant(home: &Path, id: &str, parent: &str) -> PathBuf {
    let path = write_rollout(home, id, id, None);
    let physical_parent = if parent == ROOT { SEGMENT_B } else { parent };
    let header = json!({"type":"session_meta","payload":{"id":id,"model_provider":"openai",
        "forked_from_id":parent,"history_base":{"thread_id":physical_parent,"end_ordinal_exclusive":1,"end_byte_offset":0}}});
    fs::write(
        &path,
        format!(
            "{header}\n{}\n",
            json!({"type":"event_msg","payload":{"type":"user_message","message":BODY}})
        ),
    )
    .unwrap();
    age(&path);
    path
}

fn assert_integrity(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    let mut checked = 0;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "sqlite" || ext == "db")
            {
                let db = database(&entry.path());
                let mut statement = db.prepare("PRAGMA integrity_check").unwrap();
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                assert_eq!(
                    rows,
                    vec!["ok".to_owned()],
                    "SQLite integrity failure: {}",
                    entry.path().display()
                );
                checked += 1;
            }
        }
    }
    assert!(checked >= 2, "no source databases checked");
}

fn post_decision(label: &str) -> bool {
    matches!(
        label,
        "stage:decided"
            | "stage:published"
            | "stage:target_rows"
            | "stage:map_committed"
            | "stage:committed"
    )
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}

fn imported_goal(fixture: &Fixture, planned_between: (i64, i64)) -> Goal {
    let timestamp = goal(&fixture.target, ROOT)
        .expect("imported goal is absent")
        .updated_at_ms;
    // Import pauses the goal and stamps it when the original plan is made.
    // Recovery must replay that timestamp, not retain 17 or generate a new one.
    assert!((planned_between.0..=planned_between.1).contains(&timestamp));
    assert!(timestamp > original_goal().updated_at_ms);
    Goal {
        status: "paused".into(),
        updated_at_ms: timestamp,
        ..original_goal()
    }
}

fn recover_import_after_kill(label: &str) {
    let fixture = Fixture::new();
    fixture.assert_launcher_control();
    let planned_after = unix_millis();
    fixture.kill_at(&fixture.import_args(), label, || {
        fixture.assert_boundary_state(label, false)
    });
    let planned_before = unix_millis();
    fixture.assert_managed_admission();
    let mut expected = None;
    for _ in 0..2 {
        fixture.run(&fixture.recover_args());
        let expected_goal = expected.get_or_insert_with(|| {
            if post_decision(label) {
                imported_goal(&fixture, (planned_after, planned_before))
            } else {
                original_goal()
            }
        });
        fixture.assert_owner(
            post_decision(label),
            expected_goal.clone(),
            0,
            &fixture.contents,
        );
    }
}

fn recover_reverse_after_kill(label: &str, undo: bool) {
    let fixture = Fixture::new();
    let contents = fixture.import_and_change_current_goal();
    let args = if undo {
        fixture.unimport_args()
    } else {
        fixture.return_args()
    };
    fixture.kill_at(&args, label, || fixture.assert_boundary_state(label, true));
    fixture.assert_managed_admission();
    for _ in 0..2 {
        fixture.run(&fixture.recover_args());
        fixture.assert_owner(!post_decision(label), current_goal(), 0, &contents);
    }
}

macro_rules! crash_case {
    ($name:ident, $kind:ident, $label:literal) => {
        #[test]
        fn $name() { crash_case!(@run $kind, $label); }
    };
    (@run import, $label:literal) => { recover_import_after_kill($label) };
    (@run return_current, $label:literal) => { recover_reverse_after_kill($label, false) };
    (@run undo_current, $label:literal) => { recover_reverse_after_kill($label, true) };
}

// Every mutation boundary of move, current-state return, and current-state undo.
// Multiple pages in one home count as ONE discoverable home, not dual ownership.
crash_case!(
    import_recovery_after_prepared_kill,
    import,
    "stage:prepared"
);
crash_case!(import_recovery_after_staged_kill, import, "stage:staged");
crash_case!(
    import_recovery_after_source_name_removed_kill,
    import,
    "stage:source_name_removed"
);
crash_case!(
    import_recovery_after_source_names_removed_kill,
    import,
    "stage:source_names_removed"
);
crash_case!(
    import_recovery_after_source_rows_removed_kill,
    import,
    "stage:source_rows_removed"
);
crash_case!(import_recovery_after_decided_kill, import, "stage:decided");
crash_case!(
    import_recovery_after_published_kill,
    import,
    "stage:published"
);
crash_case!(
    import_recovery_after_target_rows_kill,
    import,
    "stage:target_rows"
);
crash_case!(
    import_recovery_after_map_committed_kill,
    import,
    "stage:map_committed"
);
crash_case!(
    import_recovery_after_committed_kill,
    import,
    "stage:committed"
);
crash_case!(
    return_current_recovery_after_prepared_kill,
    return_current,
    "stage:prepared"
);
crash_case!(
    return_current_recovery_after_staged_kill,
    return_current,
    "stage:staged"
);
crash_case!(
    return_current_recovery_after_source_name_removed_kill,
    return_current,
    "stage:source_name_removed"
);
crash_case!(
    return_current_recovery_after_source_names_removed_kill,
    return_current,
    "stage:source_names_removed"
);
crash_case!(
    return_current_recovery_after_source_rows_removed_kill,
    return_current,
    "stage:source_rows_removed"
);
crash_case!(
    return_current_recovery_after_decided_kill,
    return_current,
    "stage:decided"
);
crash_case!(
    return_current_recovery_after_published_kill,
    return_current,
    "stage:published"
);
crash_case!(
    return_current_recovery_after_target_rows_kill,
    return_current,
    "stage:target_rows"
);
crash_case!(
    return_current_recovery_after_map_committed_kill,
    return_current,
    "stage:map_committed"
);
crash_case!(
    return_current_recovery_after_committed_kill,
    return_current,
    "stage:committed"
);
crash_case!(
    undo_current_recovery_after_prepared_kill,
    undo_current,
    "stage:prepared"
);
crash_case!(
    undo_current_recovery_after_staged_kill,
    undo_current,
    "stage:staged"
);
crash_case!(
    undo_current_recovery_after_source_name_removed_kill,
    undo_current,
    "stage:source_name_removed"
);
crash_case!(
    undo_current_recovery_after_source_names_removed_kill,
    undo_current,
    "stage:source_names_removed"
);
crash_case!(
    undo_current_recovery_after_source_rows_removed_kill,
    undo_current,
    "stage:source_rows_removed"
);
crash_case!(
    undo_current_recovery_after_decided_kill,
    undo_current,
    "stage:decided"
);
crash_case!(
    undo_current_recovery_after_published_kill,
    undo_current,
    "stage:published"
);
crash_case!(
    undo_current_recovery_after_target_rows_kill,
    undo_current,
    "stage:target_rows"
);
crash_case!(
    undo_current_recovery_after_map_committed_kill,
    undo_current,
    "stage:map_committed"
);
crash_case!(
    undo_current_recovery_after_committed_kill,
    undo_current,
    "stage:committed"
);

fn recovery_is_interruptible(first: &str, during: &str) {
    let fixture = Fixture::new();
    let planned_after = unix_millis();
    fixture.kill_at(&fixture.import_args(), first, || {});
    let planned_before = unix_millis();
    fixture.kill_at(&fixture.recover_args(), during, || {});
    fixture.assert_managed_admission();
    let mut expected = None;
    for _ in 0..2 {
        fixture.run(&fixture.recover_args());
        let expected_goal = expected.get_or_insert_with(|| {
            if post_decision(first) {
                imported_goal(&fixture, (planned_after, planned_before))
            } else {
                original_goal()
            }
        });
        fixture.assert_owner(
            post_decision(first),
            expected_goal.clone(),
            0,
            &fixture.contents,
        );
    }
}

#[test]
fn rollback_recovery_survives_kill_after_first_restored_name() {
    recovery_is_interruptible("stage:source_rows_removed", "stage:rollback_name_restored");
}
#[test]
fn rollback_recovery_survives_kill_after_rollback_complete() {
    recovery_is_interruptible("stage:source_rows_removed", "stage:rollback_complete");
}
#[test]
fn forward_recovery_survives_kill_after_first_publication() {
    recovery_is_interruptible("stage:decided", "stage:published");
}
#[test]
fn forward_recovery_survives_kill_after_target_rows() {
    recovery_is_interruptible("stage:decided", "stage:target_rows");
}
#[test]
fn forward_recovery_survives_kill_after_map_commit() {
    recovery_is_interruptible("stage:decided", "stage:map_committed");
}
#[test]
fn forward_recovery_survives_kill_after_commit() {
    recovery_is_interruptible("stage:decided", "stage:committed");
}

#[test]
fn undo_rollback_recovery_can_be_killed_and_preserves_current_state() {
    let fixture = Fixture::new();
    let contents = fixture.import_and_change_current_goal();
    fixture.kill_at(&fixture.unimport_args(), "stage:source_rows_removed", || {});
    fixture.kill_at(
        &fixture.recover_args(),
        "stage:rollback_name_restored",
        || {},
    );
    fixture.assert_managed_admission();
    for _ in 0..2 {
        fixture.run(&fixture.recover_args());
        fixture.assert_owner(true, current_goal(), 0, &contents);
    }
}

#[test]
fn undo_forward_recovery_can_be_killed_and_preserves_current_state() {
    let fixture = Fixture::new();
    let contents = fixture.import_and_change_current_goal();
    fixture.kill_at(&fixture.unimport_args(), "stage:decided", || {});
    fixture.kill_at(&fixture.recover_args(), "stage:published", || {});
    for _ in 0..2 {
        fixture.run(&fixture.recover_args());
        fixture.assert_owner(false, current_goal(), 0, &contents);
    }
}

#[test]
fn completed_return_command_replay_preserves_current_bytes_and_goal() {
    let fixture = Fixture::new();
    let contents = fixture.import_and_change_current_goal();
    for _ in 0..2 {
        fixture.run(&fixture.return_args());
        fixture.assert_owner(false, current_goal(), 0, &contents);
    }
}

#[test]
fn completed_unimport_command_replay_preserves_current_bytes_and_goal() {
    let fixture = Fixture::new();
    let contents = fixture.import_and_change_current_goal();
    for _ in 0..2 {
        fixture.run(&fixture.unimport_args());
        fixture.assert_owner(false, current_goal(), 0, &contents);
    }
}

#[test]
fn rollback_recovery_preserves_unrelated_sqlite_commit_made_during_import() {
    let fixture = Fixture::new();
    fixture.kill_at(&fixture.import_args(), "stage:staged", || {
        let mut db = Connection::open(fixture.source.join("state_5.sqlite")).unwrap();
        db.busy_timeout(Duration::from_secs(2)).unwrap();
        let transaction = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        assert_eq!(
            transaction
                .execute("UPDATE threads SET archived=1 WHERE id=?1", [OTHER])
                .unwrap(),
            1
        );
        transaction.commit().unwrap();
    });
    for _ in 0..2 {
        fixture.run(&fixture.recover_args());
        fixture.assert_owner(false, original_goal(), 1, &fixture.contents);
    }
}

#[test]
fn all_public_aliases_retire_before_any_target_publication() {
    let fixture = Fixture::new();
    let alias = fixture
        .source
        .join("archived_sessions/alias")
        .join(fixture.segments[0].file_name().unwrap());
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    fs::hard_link(&fixture.segments[0], &alias).unwrap();
    assert_eq!(public_names(&fixture.source).len(), 3);
    fixture.kill_at(&fixture.import_args(), "stage:source_names_removed", || {
        assert!(public_names(&fixture.source).is_empty());
        assert!(public_names(&fixture.target).is_empty());
        assert!(!alias.exists());
    });
    for _ in 0..2 {
        fixture.run(&fixture.recover_args());
    }
    assert_eq!(public_names(&fixture.source).len(), 3);
    assert!(public_names(&fixture.target).is_empty());
    assert_eq!(physical_id(&alias), fixture.identities[0]);
    assert!(thread_row(&fixture.source, ROOT).is_some());
    assert!(goal(&fixture.source, ROOT) == Some(original_goal()));
    assert_eq!(move_journal(&fixture.qb)["open"], false);
    assert_integrity(fixture.root.path());
}

fn assert_refusal_unchanged(fixture: &Fixture, args: &[OsString]) {
    let before = fixture.protected_state();
    let diagnostic = fixture.spawn(args, None).failure();
    assert!(
        diagnostic
            .chars()
            .any(|c| ('\u{ac00}'..='\u{d7af}').contains(&c)),
        "refusal needs a Korean explanation"
    );
    assert!(
        fixture.protected_state() == before,
        "refused move changed protected evidence"
    );
    assert_integrity(fixture.root.path());
}

#[test]
fn counterexample_1_header_only_child_and_grandchild_protect_root_by_refusal() {
    let fixture = Fixture::new();
    let child = write_descendant(&fixture.source, CHILD, ROOT);
    let grandchild = write_descendant(&fixture.source, GRANDCHILD, CHILD);
    assert!(thread_row(&fixture.source, CHILD).is_none());
    assert!(thread_row(&fixture.source, GRANDCHILD).is_none());
    let child_bytes = fs::read(&child).unwrap();
    let grandchild_bytes = fs::read(&grandchild).unwrap();
    assert_refusal_unchanged(&fixture, &fixture.import_args());
    fixture.assert_namespace();
    assert!(fs::read(child).unwrap() == child_bytes);
    assert!(fs::read(grandchild).unwrap() == grandchild_bytes);
    assert!(
        !fixture
            .qb
            .join("journal/session-moves")
            .join(format!("{ROOT}.json"))
            .exists()
    );
}

#[test]
fn preexisting_q_ancestor_alias_is_refused_without_exception() {
    let fixture = Fixture::new();
    let alias = fixture.q_name(&fixture.segments[0]);
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    fs::hard_link(&fixture.segments[0], alias).unwrap();
    assert!(thread_row(&fixture.target, ROOT).is_none());
    assert_eq!(public_names(&fixture.source).len(), 2);
    assert_eq!(public_names(&fixture.target).len(), 1);
    assert_refusal_unchanged(&fixture, &fixture.import_args());
}

#[test]
fn unfenceable_fork_ancestor_causes_refusal_before_mutation() {
    let fixture = Fixture::new();
    let ancestor = write_rollout(&fixture.source, CHILD, CHILD, None);
    let mut records: Vec<Value> = fixture.contents[1]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    records[0]["payload"]["forked_from_id"] = CHILD.into();
    records[0]["payload"]["history_base"] =
        json!({"thread_id":CHILD,"end_ordinal_exclusive":1,"end_byte_offset":0});
    let bytes = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    fs::write(&fixture.segments[1], bytes).unwrap();
    age(&fixture.segments[1]);
    assert!(ancestor.exists());
    assert_refusal_unchanged(&fixture, &fixture.import_args());
}

#[test]
fn unknown_hardlink_is_refused_and_not_deleted() {
    let fixture = Fixture::new();
    let unknown = fixture.root.path().join("unknown-name.jsonl");
    fs::hard_link(&fixture.segments[0], &unknown).unwrap();
    assert_refusal_unchanged(&fixture, &fixture.import_args());
    assert_eq!(physical_id(&unknown), fixture.identities[0]);
}

#[test]
fn open_native_style_handle_forces_refusal_and_safe_predecision_recovery() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new();
    let handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&fixture.segments[1])
        .unwrap();
    let _ = fixture.spawn(&fixture.import_args(), None).failure();
    assert!(fixture.segments[1].exists(), "held name was unlinked");
    assert!(fs::read(&fixture.segments[1]).unwrap() == fixture.contents[1]);
    assert!(
        public_names(&fixture.target).is_empty(),
        "published despite a held source handle"
    );
    fixture.assert_namespace();
    drop(handle);
    let path = fixture
        .qb
        .join("journal/session-moves")
        .join(format!("{ROOT}.json"));
    if path.exists() {
        for _ in 0..2 {
            fixture.run(&fixture.recover_args());
        }
        fixture.assert_owner(false, original_goal(), 0, &fixture.contents);
    } else {
        assert!(fixture.segments.iter().all(|path| path.exists()));
        assert!(thread_row(&fixture.source, ROOT).is_some());
        assert!(goal(&fixture.source, ROOT) == Some(original_goal()));
        assert_integrity(fixture.root.path());
    }
}

#[test]
fn late_header_child_is_preserved_while_predecision_recovery_restores_root() {
    let fixture = Fixture::new();
    fixture.kill_at(&fixture.import_args(), "stage:source_names_removed", || {
        write_descendant(&fixture.source, CHILD, ROOT);
    });
    for _ in 0..2 {
        fixture.run(&fixture.recover_args());
    }
    fixture.assert_owner(false, original_goal(), 0, &fixture.contents);
    assert!(thread_row(&fixture.source, CHILD).is_none());
    assert_refusal_unchanged(&fixture, &fixture.import_args());
}

fn append_conflict(label: &str) {
    let fixture = Fixture::new();
    fixture.kill_at(&fixture.import_args(), label, || {});
    let name = root_names(fixture.root.path())
        .into_iter()
        .find(|path| physical_id(path) == fixture.identities[1])
        .unwrap();
    let mut file = fs::OpenOptions::new().append(true).open(&name).unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"event_msg","payload":{"type":"user_message","message":CURRENT_MARKER}})
    )
    .unwrap();
    drop(file);
    if label == "stage:staged" {
        let current = fixture.protected_state();
        fixture.run(&fixture.recover_args());
        assert_eq!(move_journal(&fixture.qb)["open"], false);
        let after = fixture.protected_state();
        assert_eq!(after.rows, current.rows);
        assert_eq!(after.goals, current.goals);
        assert_eq!(after.map, current.map);
        assert_eq!(after.indices, current.indices);
        assert_eq!(after.other, current.other);
        fixture.assert_namespace();
        fixture.assert_launcher_control();
        fixture.run(&fixture.recover_args());
    } else {
        for _ in 0..2 {
            assert_refusal_unchanged(&fixture, &fixture.recover_args());
            fixture.assert_namespace();
            assert_eq!(move_journal(&fixture.qb)["open"], true);
        }
    }
}
#[test]
fn unrecorded_append_before_decision_aborts_source_kept() {
    append_conflict("stage:staged");
}

#[test]
fn unrecorded_append_after_decision_is_preserved_with_refusal() {
    append_conflict("stage:published");
}

#[test]
fn conflicting_target_row_is_preserved_and_recovery_stays_open() {
    let fixture = Fixture::new();
    fixture.kill_at(&fixture.import_args(), "stage:target_rows", || {});
    let db = Connection::open(fixture.target.join("state_5.sqlite")).unwrap();
    assert_eq!(
        db.execute("UPDATE threads SET archived=1 WHERE id=?1", [ROOT])
            .unwrap(),
        1
    );
    drop(db);
    for _ in 0..2 {
        assert_refusal_unchanged(&fixture, &fixture.recover_args());
    }
    assert_eq!(thread_row(&fixture.target, ROOT).unwrap().2, 1);
    assert_eq!(move_journal(&fixture.qb)["open"], true);
}

#[test]
fn conflicting_source_row_is_preserved_without_destructive_rollback() {
    let fixture = Fixture::new();
    fixture.kill_at(&fixture.import_args(), "stage:source_names_removed", || {});
    let db = Connection::open(fixture.source.join("state_5.sqlite")).unwrap();
    assert_eq!(
        db.execute("UPDATE threads SET archived=1 WHERE id=?1", [ROOT])
            .unwrap(),
        1
    );
    drop(db);
    for _ in 0..2 {
        let before = fixture.protected_state();
        let _ = fixture.spawn(&fixture.recover_args(), None).failure();
        let after = fixture.protected_state();
        assert!(before.rows == after.rows && before.goals == after.goals);
        assert!(
            before.map == after.map
                && before.indices == after.indices
                && before.other == after.other
        );
        // Restoring an additional source hardlink is non-destructive. Every
        // existing evidence name, identity and byte must survive unchanged.
        assert!(before.names.iter().all(|name| after.names.contains(name)));
        assert_eq!(move_journal(&fixture.qb)["open"], true);
        assert_integrity(fixture.root.path());
    }
    fixture.assert_namespace();
}

#[test]
fn unsupported_native_version_never_opens_a_move() {
    let fixture = Fixture::new();
    fs::write(
        fixture.appdata.join("npm/codex.cmd"),
        b"@echo off\r\necho codex-cli 99.0.0\r\n",
    )
    .unwrap();
    assert_refusal_unchanged(&fixture, &fixture.import_args());
    assert!(
        !fixture
            .qb
            .join("journal/session-moves")
            .join(format!("{ROOT}.json"))
            .exists()
    );
}

fn age(path: &Path) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)))
        .unwrap();
}
