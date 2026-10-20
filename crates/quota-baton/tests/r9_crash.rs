//! Actual CLI process termination/recovery in project-local synthetic homes.
//! These tests prove the recovered state has no data loss. They do NOT prove
//! G3's single-writable-location guarantee during the interrupted commit window.
#![cfg(all(windows, feature = "testkit", debug_assertions))]

use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    io::{self, Read},
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
const BODY: &str = "SYNTHETIC_R9_CRASH_BODY";
const OLD_OBJECTIVE: &str = "SYNTHETIC_R9_OLD_OBJECTIVE";
const CURRENT_OBJECTIVE: &str = "SYNTHETIC_R9_CURRENT_OBJECTIVE";
const OTHER_OBJECTIVE: &str = "SYNTHETIC_R9_OTHER_OBJECTIVE";
const COMMAND_DEADLINE: Duration = Duration::from_secs(20);
const REAP_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Eq, PartialEq)]
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

fn import_journal(qb: &Path) -> Value {
    use windows::Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        },
    };
    // Decode only this fixture's protected journal. Keep product journal privacy
    // intact; no plaintext inspection command or real account data is involved.
    let envelope: Value =
        serde_json::from_slice(&fs::read(qb.join("journal/sessions-import.json")).unwrap())
            .unwrap();
    assert_eq!(envelope["schema"], 1);
    let mut input = STANDARD
        .decode(envelope["payload"].as_str().unwrap())
        .unwrap();
    let data = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(input.len()).unwrap(),
        pbData: input.as_mut_ptr(),
    };
    let mut entropy = b"quota-baton/v1/wiring-journal".to_vec();
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(entropy.len()).unwrap(),
        pbData: entropy.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input and entropy remain alive for the synchronous DPAPI call;
    // output receives a Windows allocation that is copied and LocalFree'd below.
    unsafe {
        CryptUnprotectData(
            &data,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .unwrap();
    assert!(output.cbData > 0 && !output.pbData.is_null());
    // SAFETY: successful DPAPI output covers cbData initialized bytes.
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    // SAFETY: this is the allocation returned by DPAPI, no longer borrowed.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
    }
    serde_json::from_slice(&bytes).unwrap()
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
        for private in [BODY, OLD_OBJECTIVE, CURRENT_OBJECTIVE, OTHER_OBJECTIVE] {
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

    fn success(mut self) {
        let status = match self.wait_until(Instant::now() + COMMAND_DEADLINE) {
            Some(status) => status,
            None => {
                self.kill_and_reap();
                panic!("CLI exceeded command deadline: {}", self.diagnostics());
            }
        };
        assert!(
            status.success(),
            "CLI failed ({status}): {}",
            self.diagnostics()
        );
        let _ = self.diagnostics();
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
        ] {
            fs::create_dir_all(dir).unwrap();
        }
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
            "--move".into(),
            "--apply".into(),
            "--yes".into(),
        ]
    }

    fn rollback_args(&self) -> Vec<OsString> {
        vec![
            "sessions".into(),
            "unimport".into(),
            ROOT.into(),
            "--apply".into(),
            "--force".into(),
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
        command
            .args(args)
            .env("QB_HOME", &self.qb)
            .env("USERPROFILE", &self.user)
            .env("HOME", &self.user)
            .env("APPDATA", &self.appdata)
            .env("CODEX_HOME", &self.target)
            .env_remove("QB_R9_CHECKPOINT")
            .env_remove("QB_R9_CHECKPOINT_NOTIFY")
            .current_dir(self.root.path())
            .creation_flags(0x08000000)
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
        while_parked();
        process.kill_and_reap();
        let _ = process.diagnostics();
    }

    fn q_name(&self, source: &Path) -> PathBuf {
        self.target.join(source.strip_prefix(&self.source).unwrap())
    }

    fn import_and_change_current_goal(&self) {
        self.run(&self.import_args());
        assert!(self.segments.iter().all(|path| !path.exists()));
        assert!(thread_row(&self.source, ROOT).is_none());
        assert_eq!(goal(&self.source, ROOT), None);
        let db = Connection::open(self.target.join("goals_1.sqlite")).unwrap();
        db.busy_timeout(Duration::from_secs(2)).unwrap();
        let current = current_goal();
        assert_eq!(db.execute("UPDATE thread_goals SET objective=?1,status=?2,updated_at_ms=?3 WHERE thread_id=?4", params![current.objective, current.status, current.updated_at_ms, ROOT]).unwrap(), 1);
        drop(db);
        assert_eq!(goal(&self.target, ROOT), Some(current_goal()));
        let map: Value = serde_json::from_slice(&fs::read(&self.map_path).unwrap()).unwrap();
        assert_eq!(
            Path::new(map["sessions"][ROOT].as_str().unwrap()),
            self.target.as_path()
        );
    }

    fn assert_recovered(&self, expected_goal: Goal, other_archived: i64) {
        for (source, original) in self.segments.iter().zip(&self.contents) {
            assert_eq!(
                fs::read(source).unwrap(),
                *original,
                "lost rollout segment after recovery"
            );
            assert!(!self.q_name(source).exists());
        }
        assert_eq!(
            thread_row(&self.source, ROOT),
            Some((
                self.segments[1].to_str().unwrap().into(),
                "openai".into(),
                0
            ))
        );
        assert_eq!(goal(&self.source, ROOT), Some(expected_goal));
        assert_eq!(thread_row(&self.target, ROOT), None);
        assert_eq!(goal(&self.target, ROOT), None);
        assert_eq!(
            thread_row(&self.source, OTHER),
            Some((
                self.other.to_str().unwrap().into(),
                "openai".into(),
                other_archived
            ))
        );
        assert_eq!(
            goal(&self.source, OTHER),
            Some(Goal {
                objective: OTHER_OBJECTIVE.into(),
                status: "paused".into(),
                updated_at_ms: 23
            })
        );
        assert_eq!(fs::read(&self.other).unwrap(), self.other_contents);
        assert!(
            fs::read(self.source.join("session_index.jsonl"))
                .unwrap()
                .starts_with(&self.original_index),
            "append-only source index was rewritten"
        );
        let map: Value = serde_json::from_slice(&fs::read(&self.map_path).unwrap()).unwrap();
        assert_eq!(map, self.original_map);
        let journal = import_journal(&self.qb);
        assert!(!journal["imports"].as_object().unwrap().contains_key(ROOT));
        assert_integrity(self.root.path());
    }
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

fn recover_import_after_kill(label: &str) {
    let fixture = Fixture::new();
    fixture.kill_at(&fixture.import_args(), label, || {});
    if label == "source_name_removed" {
        assert_eq!(
            fixture.segments.iter().filter(|path| path.exists()).count(),
            1,
            "must exercise partial two-segment retirement"
        );
    }
    fixture.run(&fixture.rollback_args());
    fixture.assert_recovered(original_goal(), 0);
}

fn recover_return_after_kill(label: &str) {
    let fixture = Fixture::new();
    fixture.import_and_change_current_goal();
    fixture.kill_at(&fixture.return_args(), label, || {});
    if label == "return:source-name-removed" {
        assert_eq!(
            fixture
                .segments
                .iter()
                .filter(|path| fixture.q_name(path).exists())
                .count(),
            1,
            "must exercise partial Q retirement"
        );
    }
    fixture.run(&fixture.return_args());
    fixture.assert_recovered(current_goal(), 0);
    fixture.run(&fixture.return_args()); // Completed-return replay is a no-op.
    fixture.assert_recovered(current_goal(), 0);
}

macro_rules! import_recovery {
    ($name:ident, $label:literal) => {
        #[test]
        fn $name() {
            recover_import_after_kill($label);
        }
    };
}

macro_rules! return_recovery {
    ($name:ident, $label:literal) => {
        #[test]
        fn $name() {
            recover_return_after_kill($label);
        }
    };
}

import_recovery!(import_recovery_after_prepared_kill, "prepared");
import_recovery!(import_recovery_after_linked_kill, "linked");
import_recovery!(import_recovery_after_target_rows_kill, "target_rows");
import_recovery!(import_recovery_after_index_appended_kill, "index_appended");
import_recovery!(import_recovery_after_verified_kill, "verified");
import_recovery!(import_recovery_after_retiring_kill, "retiring");
import_recovery!(
    import_recovery_after_source_rows_removed_kill,
    "source_rows_removed"
);
import_recovery!(
    import_recovery_after_first_source_name_removed_kill,
    "source_name_removed"
);
import_recovery!(import_recovery_after_source_retired_kill, "source_retired");
import_recovery!(
    import_recovery_after_aliases_retired_kill,
    "aliases_retired"
);
import_recovery!(import_recovery_after_map_committed_kill, "map_committed");
import_recovery!(import_recovery_after_committed_kill, "committed");

return_recovery!(return_recovery_after_prepared_kill, "return:prepared");
return_recovery!(return_recovery_after_linked_kill, "return:linked");
return_recovery!(return_recovery_after_rows_kill, "return:rows");
return_recovery!(return_recovery_after_verified_kill, "return:verified");
return_recovery!(return_recovery_after_mapped_kill, "return:mapped");
return_recovery!(return_recovery_after_retiring_kill, "return:retiring");
return_recovery!(
    return_recovery_after_source_rows_removed_kill,
    "return:source-rows-removed"
);
return_recovery!(
    return_recovery_after_first_source_name_removed_kill,
    "return:source-name-removed"
);
return_recovery!(return_recovery_after_retired_kill, "return:retired");
return_recovery!(
    return_recovery_after_import_removed_kill,
    "return:import-removed"
);
return_recovery!(return_recovery_after_committed_kill, "return:committed");

#[test]
fn rollback_recovery_preserves_unrelated_sqlite_commit_made_during_import() {
    let fixture = Fixture::new();
    fixture.kill_at(&fixture.import_args(), "verified", || {
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
    fixture.run(&fixture.rollback_args());
    fixture.assert_recovered(original_goal(), 1);
}
