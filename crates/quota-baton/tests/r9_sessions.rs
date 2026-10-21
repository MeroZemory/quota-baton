//! Product CLI acceptance; only project-local synthetic homes and client stub.
#![cfg(windows)]
use rusqlite::{Connection, OptionalExtension};
use std::{
    fs,
    io::Write,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant, SystemTime},
};
const A: &str = "11111111-1111-1111-1111-111111111111";
const B: &str = "22222222-2222-2222-2222-222222222222";
const C: &str = "33333333-3333-3333-3333-333333333333";
const D: &str = "44444444-4444-4444-4444-444444444444";
const PRIVATE_BODY: &str = "SYNTHETIC_PRIVATE_SESSION_BODY";

/// Run alone with `cargo test -p quota-baton --test r9_sessions
/// r93_5300_rollout_preview_timing -- --ignored --nocapture`.
/// Typed-confirmation move/return phase timings use the report's ConPTY bench.
#[test]
#[ignore = "explicit machine timing benchmark; 5300 project-local synthetic rollouts"]
fn r93_5300_rollout_preview_timing() {
    let fixture = Fixture::new();
    let selected = fixture.add(A, false, "openai");
    let directory = fixture.source.join("archived_sessions/2026/10/21");
    fs::create_dir_all(&directory).unwrap();
    for slot in 1..5300 {
        let id = format!("aaaaaaaa-0000-0000-0000-{slot:012x}");
        let header = serde_json::json!({"type":"session_meta","payload":{
            "id":id,"model_provider":"openai","base_instructions":{"text":"x".repeat(16384)}
        }});
        let bytes = format!("{header}\n{{\"private\":\"{PRIVATE_BODY}\"}}\n");
        let compressed = slot <= 3950;
        let path = directory.join(format!(
            "rollout-test-{id}.jsonl{}",
            if compressed { ".zst" } else { "" }
        ));
        let bytes = if compressed {
            zstd::stream::encode_all(bytes.as_bytes(), 1).unwrap()
        } else {
            bytes.into_bytes()
        };
        fs::write(path, bytes).unwrap();
    }
    let start = Instant::now();
    let output = fixture.import(A, false);
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed <= Duration::from_secs(10),
        "preview took {elapsed:?}"
    );
    assert!(selected.exists());
    assert!(!fixture.target.join("state_5.sqlite").exists());
    assert!(!fixture.qb.join("journal/sessions-import.json").exists());
    println!("5300 rollouts / 3950 zstd: preview {elapsed:?}; read-only");
}
struct Fixture {
    _root: tempfile::TempDir,
    user: PathBuf,
    source: PathBuf,
    target: PathBuf,
    qb: PathBuf,
    stub: PathBuf,
    sentinel: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r9-cli-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        let user = root.path().join("user");
        let source = user.join(".codex");
        let target = user.join(".codex-acct/qb");
        let qb = root.path().join("qb");
        for dir in [
            &source,
            &target,
            &qb.join("journal"),
            &user.join("AppData/Roaming/npm"),
            &user.join("AppData/Local"),
            &root.path().join("temp"),
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(qb.join("journal/wiring.json"),serde_json::to_vec(&serde_json::json!({"schema":3,"mode":"dedicated","install_completed":true,"home":target})).unwrap()).unwrap();
        let sentinel = root.path().join("launched.txt");
        let stub = user.join("AppData/Roaming/npm/codex.cmd");
        fs::write(
            &stub,
            format!(
                "@echo off\r\nif \"%~1\"==\"--version\" (echo codex-cli 0.163.0& exit /b 0)\r\necho synthetic-launch>\"{}\"\r\nexit /b 0\r\n",
                sentinel.display()
            ),
        )
        .unwrap();
        let db = Connection::open(source.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,model_provider TEXT NOT NULL,archived INTEGER DEFAULT 0);").unwrap();
        Self {
            _root: root,
            user,
            source,
            target,
            qb,
            stub,
            sentinel,
        }
    }
    fn add(&self, id: &str, recent: bool, provider: &str) -> PathBuf {
        self.add_with_parent(id, recent, provider, None)
    }
    fn add_with_parent(
        &self,
        id: &str,
        recent: bool,
        provider: &str,
        parent: Option<&str>,
    ) -> PathBuf {
        let path = self
            .source
            .join("sessions")
            .join(format!("rollout-2026-10-21T00-00-00-{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut meta = serde_json::json!({"type":"session_meta","payload":{"id":id,"model_provider":provider}});
        if let Some(parent) = parent {
            meta["payload"]["forked_from_id"] = parent.into();
            meta["payload"]["history_base"] = serde_json::json!({"thread_id":parent,"end_ordinal_exclusive":5,"end_byte_offset":100});
        }
        fs::write(&path, format!("{meta}\n{}\n", serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":PRIVATE_BODY}}))).unwrap();
        if !recent {
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)),
                )
                .unwrap();
        }
        let db = Connection::open(self.source.join("state_5.sqlite")).unwrap();
        db.execute(
            "INSERT INTO threads VALUES(?1,?2,?3,0)",
            rusqlite::params![id, path.to_str().unwrap(), provider],
        )
        .unwrap();
        path
    }
    fn cli(&self, args: &[&std::ffi::OsStr]) -> Output {
        let stdout = self._root.path().join("stdout.txt");
        let stderr = self._root.path().join("stderr.txt");
        let mut child = Command::new(env!("CARGO_BIN_EXE_quota-baton"))
            .args(args)
            // Many fixtures run together; each short-lived CLI needs only two
            // reactor workers, rather than one per machine CPU per process.
            .env("TOKIO_WORKER_THREADS", "2")
            .env("QB_HOME", &self.qb)
            .env("USERPROFILE", &self.user)
            .env("APPDATA", self.user.join("AppData/Roaming"))
            .env("LOCALAPPDATA", self.user.join("AppData/Local"))
            .env("TEMP", self._root.path().join("temp"))
            .env("TMP", self._root.path().join("temp"))
            .env("HOME", &self.user)
            .env("CODEX_HOME", &self.target)
            .current_dir(self._root.path())
            .creation_flags(0x08000000)
            .stdin(std::process::Stdio::null())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("synthetic CLI exceeded15s");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        };
        assert!(!String::from_utf8_lossy(&output.stdout).contains(PRIVATE_BODY));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(PRIVATE_BODY));
        output
    }
    fn import(&self, id: &str, apply: bool) -> Output {
        self.import_options(id, apply, &[])
    }
    fn import_options(&self, id: &str, apply: bool, options: &[&str]) -> Output {
        let mut args = vec![
            std::ffi::OsStr::new("sessions"),
            "import".as_ref(),
            "--from".as_ref(),
            self.source.as_os_str(),
            id.as_ref(),
        ];
        if apply {
            args.push("--apply".as_ref());
            args.push("--yes".as_ref());
        }
        args.extend(options.iter().map(std::ffi::OsStr::new));
        self.cli(&args)
    }
    fn resume(&self, id: &str) -> Output {
        self.launch(&["resume", id])
    }
    fn launch(&self, client_args: &[&str]) -> Output {
        let mut args = vec![
            "launch-client".as_ref(),
            "--client".as_ref(),
            "codex".as_ref(),
            "--command".as_ref(),
            self.stub.as_os_str(),
            "--".as_ref(),
        ];
        args.extend(client_args.iter().map(std::ffi::OsStr::new));
        self.cli(&args)
    }
    fn unimport(&self, id: &str) -> Output {
        self.cli(&[
            "sessions".as_ref(),
            "unimport".as_ref(),
            id.as_ref(),
            "--apply".as_ref(),
            "--yes".as_ref(),
        ])
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn r92_released_wire_journals_allow_plain_version_resume_and_import() {
    for shape in [
        include_str!("fixtures/wiring/r3.json"),
        include_str!("fixtures/wiring/r3_1.json"),
        include_str!("fixtures/wiring/r3_2.json"),
    ] {
        let f = Fixture::new();
        let mut journal: serde_json::Value = serde_json::from_str(shape).unwrap();
        journal["home"] = serde_json::json!(f.target);
        let path = f.qb.join("journal/wiring.json");
        let before = serde_json::to_vec_pretty(&journal).unwrap();
        fs::write(&path, &before).unwrap();
        assert_success(&f.cli(&["launch-client".as_ref(), "--check".as_ref()]));
        assert!(!f.sentinel.exists());
        assert!(!f.target.join(".quota-baton-admission.lock").exists());
        assert_success(&f.launch(&["--version"]));
        assert_success(&f.launch(&[]));
        f.add(A, false, "openai");
        assert_success(&f.import(A, false));
        assert_success(&f.import(A, true));
        assert_success(&f.resume(A));
        assert_eq!(
            fs::read(path).unwrap(),
            before,
            "reads must not migrate wiring"
        );
    }
}

#[test]
fn r92_read_only_launch_check_rejects_partial_wire_without_a_child_or_lock() {
    for mutation in [
        "not_done",
        "removed",
        "upgrade",
        "restoring",
        "terminal_false",
    ] {
        let f = Fixture::new();
        let mut journal: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/wiring/r3_2.json")).unwrap();
        journal["home"] = serde_json::json!(f.target);
        match mutation {
            "not_done" => journal["entries"][0]["done"] = false.into(),
            "removed" => journal["entries"][0]["removed"] = true.into(),
            "upgrade" => journal["entries"][1]["upgrade"] = serde_json::json!({"hash":"synthetic"}),
            "restoring" => {
                journal["entries"][1]["original"] = serde_json::json!({"restoring":true})
            }
            "terminal_false" => journal["install_completed"] = false.into(),
            _ => unreachable!(),
        }
        let path = f.qb.join("journal/wiring.json");
        let before = serde_json::to_vec_pretty(&journal).unwrap();
        fs::write(&path, &before).unwrap();
        assert_refused(
            &f,
            &f.cli(&["launch-client".as_ref(), "--check".as_ref()]),
            "전용 Codex 홈 연결",
        );
        assert!(!f.target.join(".quota-baton-admission.lock").exists());
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

fn assert_refused(fixture: &Fixture, output: &Output, reason: &str) {
    assert!(!output.status.success());
    assert!(!fixture.sentinel.exists());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(reason),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_confirmation_refused(fixture: &Fixture, output: &Output) {
    assert!(!output.status.success());
    assert!(!fixture.sentinel.exists());
    let diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(diagnostic.contains("터미널"), "{diagnostic}");
    assert!(
        diagnostic.contains("quota-baton sessions import"),
        "{diagnostic}"
    );
}

fn thread_path(home: &Path, id: &str) -> Option<PathBuf> {
    Connection::open(home.join("state_5.sqlite"))
        .unwrap()
        .query_row(
            "SELECT rollout_path FROM threads WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap()
        .map(PathBuf::from)
}

fn goal_status(home: &Path, id: &str) -> Option<String> {
    Connection::open(home.join("goals_1.sqlite"))
        .unwrap()
        .query_row(
            "SELECT status FROM thread_goals WHERE thread_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
}

fn assert_counts(output: &Output, counts: [u64; 5]) {
    assert_success(output);
    let text = String::from_utf8_lossy(&output.stdout);
    let row = text
        .lines()
        .find(|line| line.starts_with("개수 |"))
        .unwrap_or_else(|| panic!("missing bulk counts: {text}"));
    let actual: Vec<u64> = row
        .split('|')
        .skip(1)
        .map(|value| value.trim().parse().unwrap())
        .collect();
    assert_eq!(actual, counts, "{text}");
    assert!(text.contains("이미 가져옴 | 실행 중 건너뜀 | 분기 건너뜀 | 실패"));
}
#[test]
fn r9_cli_dry_default_bulk_skips_recent_and_existing_and_never_prints_body() {
    let f = Fixture::new();
    let source = f.add(A, false, "openai");
    f.add(B, true, "openai");
    let out = f.import(A, false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!f.qb.join("journal/sessions-import.json").exists());
    assert!(
        !f.qb
            .join(format!("journal/session-moves/{A}.json"))
            .exists()
    );
    assert!(source.exists());
    assert!(!f.target.join("sessions").exists());
    let out = f.cli(&[
        "sessions".as_ref(),
        "import".as_ref(),
        "--from".as_ref(),
        f.source.as_os_str(),
        "--all".as_ref(),
        "--keep-source".as_ref(),
        "--apply".as_ref(),
        "--yes".as_ref(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_counts(&out, [1, 0, 1, 0, 0]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("결과 | 공유 |"));
    assert!(source.exists());
    let target = f.target.join("sessions").join(source.file_name().unwrap());
    assert!(target.exists());
    let out = f.cli(&[
        "sessions".as_ref(),
        "import".as_ref(),
        "--from".as_ref(),
        f.source.as_os_str(),
        "--all".as_ref(),
        "--keep-source".as_ref(),
        "--apply".as_ref(),
        "--yes".as_ref(),
    ]);
    assert_counts(&out, [0, 1, 1, 0, 0]);
}
#[test]
fn r9_no_tty_resume_needs_manual_import_then_reuses_and_unimports() {
    let f = Fixture::new();
    let source = f.add(A, false, "openai");
    let original = fs::read(&source).unwrap();
    {
        let goals = Connection::open(f.source.join("goals_1.sqlite")).unwrap();
        goals.execute_batch("CREATE TABLE thread_goals(thread_id TEXT PRIMARY KEY,objective TEXT NOT NULL,status TEXT NOT NULL,updated_at_ms INTEGER NOT NULL);").unwrap();
        goals
            .execute(
                "INSERT INTO thread_goals VALUES (?1,'synthetic objective','active',0)",
                [A],
            )
            .unwrap();
    }
    let original_state = fs::read(f.source.join("state_5.sqlite")).unwrap();
    let original_goals = fs::read(f.source.join("goals_1.sqlite")).unwrap();
    let refused = f.resume(A);
    assert_confirmation_refused(&f, &refused);
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(
        fs::read(f.source.join("state_5.sqlite")).unwrap(),
        original_state
    );
    assert_eq!(
        fs::read(f.source.join("goals_1.sqlite")).unwrap(),
        original_goals
    );
    assert!(!f.qb.join("journal/sessions-import.json").exists());
    assert!(
        !f.qb
            .join(format!("journal/session-moves/{A}.json"))
            .exists()
    );
    assert!(!f.target.join("state_5.sqlite").exists());
    assert!(!f.target.join("goals_1.sqlite").exists());
    assert!(!f.target.join("sessions").exists());

    let imported = f.import(A, true);
    assert_success(&imported);
    assert!(!String::from_utf8_lossy(&imported.stdout).contains("입력하세요"));
    assert!(!f.sentinel.exists());
    let target = f.target.join("sessions").join(source.file_name().unwrap());
    assert!(!source.exists());
    assert_eq!(fs::read(&target).unwrap(), original);
    assert!(thread_path(&f.source, A).is_none());
    assert_eq!(thread_path(&f.target, A), Some(target.clone()));
    assert_eq!(goal_status(&f.source, A), None);
    assert_eq!(goal_status(&f.target, A).as_deref(), Some("paused"));
    let reused = f.resume(A);
    assert_success(&reused);
    assert!(f.sentinel.exists());
    assert!(!source.exists());
    assert!(!String::from_utf8_lossy(&reused.stdout).contains("기존 대화를 qb로"));
    assert!(!String::from_utf8_lossy(&reused.stdout).contains("입력하세요"));
    assert!(thread_path(&f.source, A).is_none());
    assert_eq!(thread_path(&f.target, A), Some(target.clone()));
    fs::remove_file(&f.sentinel).unwrap();
    assert_success(&f.unimport(A));
    assert!(!f.sentinel.exists());
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(thread_path(&f.source, A), Some(source));
    // Reverse the current Q state; do not reactivate an old import snapshot.
    assert_eq!(goal_status(&f.source, A).as_deref(), Some("paused"));
    assert!(!target.exists());
    assert!(thread_path(&f.target, A).is_none());
    assert_eq!(goal_status(&f.target, A), None);
}

#[test]
fn r9_no_tty_import_apply_without_yes_preserves_source_and_has_no_journal() {
    let f = Fixture::new();
    let source = f.add(A, false, "openai");
    let original = fs::read(&source).unwrap();
    let original_state = fs::read(f.source.join("state_5.sqlite")).unwrap();
    let out = f.cli(&[
        "sessions".as_ref(),
        "import".as_ref(),
        "--from".as_ref(),
        f.source.as_os_str(),
        A.as_ref(),
        "--apply".as_ref(),
    ]);
    assert_confirmation_refused(&f, &out);
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(
        fs::read(f.source.join("state_5.sqlite")).unwrap(),
        original_state
    );
    assert!(!f.qb.join("journal/sessions-import.json").exists());
    assert!(
        !f.qb
            .join(format!("journal/session-moves/{A}.json"))
            .exists()
    );
    assert!(!f.target.join("sessions").exists());
    assert!(!f.target.join("state_5.sqlite").exists());
    assert!(!f.target.join("goals_1.sqlite").exists());
    assert!(!f.target.join("session_index.jsonl").exists());
}

#[test]
fn r9_resume_uuid_with_trailing_help_does_not_import() {
    assert_resume_without_import(&["resume", A, "--help"]);
}

#[test]
fn r9_resume_last_with_uuid_prompt_does_not_import() {
    assert_resume_without_import(&["resume", "--last", A]);
}

#[test]
fn r9_resume_uuid_prompt_with_trailing_last_does_not_import() {
    assert_resume_without_import(&["resume", A, "--last"]);
}

fn assert_resume_without_import(args: &[&str]) {
    let f = Fixture::new();
    let source = f.add(A, false, "openai");
    let original = fs::read(&source).unwrap();
    let original_state = fs::read(f.source.join("state_5.sqlite")).unwrap();
    let target = f.target.join("sessions").join(source.file_name().unwrap());

    let out = f.launch(args);
    assert_success(&out);
    assert!(f.sentinel.exists(), "stub did not launch for {args:?}");
    assert!(source.exists(), "source removed for {args:?}");
    assert!(fs::read(&source).unwrap() == original);
    assert_eq!(thread_path(&f.source, A), Some(source));
    assert!(fs::read(f.source.join("state_5.sqlite")).unwrap() == original_state);
    assert!(!f.qb.join("journal/sessions-import.json").exists());
    assert!(
        !f.qb
            .join(format!("journal/session-moves/{A}.json"))
            .exists()
    );
    assert!(!target.exists());
    // Do not use thread_path on an absent target DB: its test helper opens
    // SQLite with CREATE, which would itself mutate this no-import fixture.
    assert!(!f.target.join("state_5.sqlite").exists());
    assert!(!f.target.join("goals_1.sqlite").exists());
    assert!(!f.target.join("session_index.jsonl").exists());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("기존 대화를 qb로"));
}

#[test]
fn r9_last_as_model_value_does_not_suppress_resume_confirmation() {
    let f = Fixture::new();
    let source = f.add(A, false, "openai");
    let original = fs::read(&source).unwrap();
    let target = f.target.join("sessions").join(source.file_name().unwrap());

    // Use an admitted value-taking option. Arbitrary --config overrides are
    // deliberately rejected by panel 2 before the resume hook is reached.
    let refused = f.launch(&["--model", "--last", "resume", A]);
    assert_confirmation_refused(&f, &refused);
    assert_eq!(fs::read(&source).unwrap(), original);
    assert!(!f.qb.join("journal/sessions-import.json").exists());
    assert!(
        !f.qb
            .join(format!("journal/session-moves/{A}.json"))
            .exists()
    );
    assert!(!target.exists());
    assert_success(&f.import_options(A, true, &["--keep-source"]));
    let reused = f.launch(&["--model", "--last", "resume", A]);
    assert_success(&reused);
    assert!(f.sentinel.exists());
    assert!(f.qb.join("journal/sessions-import.json").exists());
    assert!(target.exists());
    assert!(fs::read(&target).unwrap() == original);
    assert_eq!(thread_path(&f.target, A), Some(target));
}

#[test]
fn r9_yes_keep_source_shares_without_a_prompt() {
    let f = Fixture::new();
    let source = f.add(A, false, "openai");
    let original = fs::read(&source).unwrap();
    let original_state = fs::read(f.source.join("state_5.sqlite")).unwrap();
    let out = f.import_options(A, true, &["--keep-source"]);
    assert_success(&out);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("결과 | 공유 |"));
    assert!(!text.contains("입력하세요"));
    assert!(!f.sentinel.exists());
    let target = f.target.join("sessions").join(source.file_name().unwrap());
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(
        fs::read(f.source.join("state_5.sqlite")).unwrap(),
        original_state
    );
    assert_eq!(thread_path(&f.source, A), Some(source));
    assert_eq!(thread_path(&f.target, A), Some(target));
    assert!(f.qb.join("journal/sessions-import.json").exists());
}

#[test]
fn r9_keep_source_every_resume_grammar_refuses_source_writer_before_launch() {
    let f = Fixture::new();
    let source = f.add(A, false, "openai");
    assert_success(&f.import_options(A, true, &["--keep-source"]));
    assert!(source.exists());
    assert!(!f.sentinel.exists());
    let writer = fs::OpenOptions::new().append(true).open(&source).unwrap();
    let grammars: &[&[&str]] = &[
        &["resume", A],
        &["resume", "--all", A],
        &["-m", "synthetic-model", "resume", "--all", A],
        &["resume", "--", A],
    ];
    for args in grammars {
        assert_refused(&f, &f.launch(args), "다른 곳에서 실행 중");
    }
    drop(writer);
    for args in grammars {
        let out = f.launch(args);
        assert_success(&out);
        assert!(f.sentinel.exists());
        assert!(!String::from_utf8_lossy(&out.stdout).contains("기존 대화를 qb로"));
        fs::remove_file(&f.sentinel).unwrap();
    }
}
#[test]
fn r9_readable_compressed_only_rowless_target_allows_managed_picker_launches() {
    let grammars: &[&[&str]] = &[&[], &["resume"], &["resume", "--last"]];
    for args in grammars {
        let f = Fixture::new();
        // Prove wiring, version admission, and this grammar reach the client
        // before introducing the compressed-only target inventory.
        assert_success(&f.launch(args));
        assert!(
            f.sentinel.exists(),
            "client control did not launch: {args:?}"
        );
        fs::remove_file(&f.sentinel).unwrap();

        let compressed = f
            .target
            .join("sessions")
            .join(format!("rollout-2026-10-21T00-00-00-{B}.jsonl.zst"));
        fs::create_dir_all(compressed.parent().unwrap()).unwrap();
        let meta = serde_json::json!({
            "type":"session_meta",
            "payload":{
                "id":B,"model_provider":"openai","forked_from_id":A,
                "history_base":{
                    "thread_id":A,"end_ordinal_exclusive":1,"end_byte_offset":1
                }
            }
        });
        let body = format!("{meta}\n");
        let encoded = zstd::stream::encode_all(body.as_bytes(), 1).unwrap();
        assert_eq!(
            zstd::stream::decode_all(encoded.as_slice()).unwrap(),
            body.as_bytes()
        );
        fs::write(&compressed, &encoded).unwrap();
        assert!(!f.target.join("state_5.sqlite").exists());
        let source_state = fs::read(f.source.join("state_5.sqlite")).unwrap();

        assert_success(&f.launch(args));
        assert!(f.sentinel.exists(), "client did not launch: {args:?}");
        assert_eq!(fs::read(&compressed).unwrap(), encoded);
        assert_eq!(
            fs::read(f.source.join("state_5.sqlite")).unwrap(),
            source_state
        );
        assert!(!f.target.join("state_5.sqlite").exists());
        assert!(!f.target.join("goals_1.sqlite").exists());
        assert!(!f.target.join("session_index.jsonl").exists());
        assert!(!f.qb.join("journal/sessions-import.json").exists());
        assert!(!f.qb.join("journal/session-moves").exists());
    }
}

#[test]
fn r9_auto_resume_diverged_or_unreadable_selected_records_never_launch() {
    let f = Fixture::new();
    let source = f.add(A, false, "openai");
    assert_success(&f.import_options(A, true, &["--keep-source"]));
    let target = f.target.join("sessions").join(source.file_name().unwrap());
    let original = fs::read(&source).unwrap();
    let source_state = fs::read(f.source.join("state_5.sqlite")).unwrap();
    let target_state = fs::read(f.target.join("state_5.sqlite")).unwrap();
    let journal = fs::read(f.qb.join("journal/sessions-import.json")).unwrap();
    let compressed = source.with_extension("jsonl.zst");
    fs::write(&compressed, b"synthetic compressed marker").unwrap();
    let out = f.resume(A);
    assert_refused(&f, &out, "압축");
    assert_eq!(
        fs::read(&compressed).unwrap(),
        b"synthetic compressed marker"
    );
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(fs::read(&target).unwrap(), original);
    assert_eq!(
        fs::read(f.source.join("state_5.sqlite")).unwrap(),
        source_state
    );
    assert_eq!(
        fs::read(f.target.join("state_5.sqlite")).unwrap(),
        target_state
    );
    assert_eq!(
        fs::read(f.qb.join("journal/sessions-import.json")).unwrap(),
        journal
    );
    fs::remove_file(compressed).unwrap();
    // Break the shared link before introducing true target divergence.
    fs::remove_file(&target).unwrap();
    fs::write(&target, b"unrelated synthetic").unwrap();
    let out = f.resume(A);
    assert_refused(
        &f,
        &out,
        "대화 기록이 분기되었습니다. sessions unimport 후 다시 가져오세요",
    );
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(fs::read(&target).unwrap(), b"unrelated synthetic");
    assert_eq!(
        fs::read(f.source.join("state_5.sqlite")).unwrap(),
        source_state
    );
    assert_eq!(
        fs::read(f.target.join("state_5.sqlite")).unwrap(),
        target_state
    );
    assert_eq!(
        fs::read(f.qb.join("journal/sessions-import.json")).unwrap(),
        journal
    );
}

#[test]
fn r9_ancestor_resume_uses_child_journal_provenance_outside_known_homes() {
    let mut f = Fixture::new();
    let manual_home = f._root.path().join("manual-source");
    fs::rename(&f.source, &manual_home).unwrap();
    f.source = manual_home;
    let ancestor = f.add(A, false, "openai");
    let child = f.add_with_parent(B, false, "openai", Some(A));
    assert_success(&f.import_options(B, true, &["--keep-source"]));
    assert!(ancestor.exists() && child.exists());
    assert!(thread_path(&f.target, A).is_some());
    // A was installed only as B's ancestor, never as a separate import. The
    // journaled manual home is outside automatic discovery's search roots.
    assert_success(&f.resume(A));
    assert!(f.sentinel.exists());
    fs::remove_file(&f.sentinel).unwrap();
    let writer = fs::OpenOptions::new().append(true).open(&ancestor).unwrap();
    let refusal = f.resume(A);
    assert_refused(&f, &refusal, "다른 곳에서 실행 중");
    let diagnostic = String::from_utf8_lossy(&refusal.stderr);
    assert!(diagnostic.contains("app-server-daemon/daemon.pid"));
    assert!(diagnostic.contains("해당 PID"));
    assert!(diagnostic.contains("다른 대화를 제공하지 않는"));
    drop(writer);

    // Preserve the old linked file as a prefix; only source's active physical
    // filename changes. Identity checks on the saved names alone cannot catch it.
    let replacement = f
        .source
        .join("sessions")
        .join(format!("rollout-2026-10-21T00-00-00-{C}.jsonl"));
    fs::write(&replacement, fs::read(&ancestor).unwrap()).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)))
        .unwrap();
    {
        let db = Connection::open(f.source.join("state_5.sqlite")).unwrap();
        db.execute(
            "UPDATE threads SET rollout_path=?1 WHERE id=?2",
            rusqlite::params![replacement.to_str().unwrap(), A],
        )
        .unwrap();
    }
    assert_refused(&f, &f.resume(A), "분기");
    assert_eq!(
        fs::read(&ancestor).unwrap(),
        fs::read(&replacement).unwrap()
    );
}

#[test]
fn r9_bulk_move_reports_moved_live_diverged_and_failed_separately() {
    let f = Fixture::new();
    let moved = f.add(A, false, "openai");
    let live = f.add(B, false, "openai");
    let diverged = f.add(C, false, "openai");
    let unsupported = f.add(D, false, "other");
    let conflicting = f
        .target
        .join("sessions")
        .join(diverged.file_name().unwrap());
    fs::create_dir_all(conflicting.parent().unwrap()).unwrap();
    // Same valid metadata and bytes, but a separate file rather than a hardlink.
    fs::write(&conflicting, fs::read(&diverged).unwrap()).unwrap();
    let writer = fs::OpenOptions::new().append(true).open(&live).unwrap();
    let out = f.cli(&[
        "sessions".as_ref(),
        "import".as_ref(),
        "--from".as_ref(),
        f.source.as_os_str(),
        "--all".as_ref(),
        "--apply".as_ref(),
        "--yes".as_ref(),
    ]);
    assert_counts(&out, [1, 0, 1, 1, 1]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("결과 | 이동 |"));
    assert!(text.contains(&format!("{B}: 실행 중/최근 사용으로 건너뜀")));
    assert!(text.contains(&format!("{C}: 기록 분기로 건너뜀")));
    assert!(text.contains(&format!("{D}: 실패")));
    assert!(!moved.exists());
    assert!(
        f.target
            .join("sessions")
            .join(moved.file_name().unwrap())
            .exists()
    );
    assert!(thread_path(&f.source, A).is_none());
    for id in [B, C, D] {
        assert!(thread_path(&f.source, id).is_some());
        assert!(thread_path(&f.target, id).is_none());
    }
    assert!(live.exists() && diverged.exists() && unsupported.exists());
    assert_eq!(
        fs::read(&conflicting).unwrap(),
        fs::read(&diverged).unwrap()
    );
    assert!(!f.sentinel.exists());
    drop(writer);
}

#[test]
fn r9_explicit_move_succeeds_and_conflicts_with_keep_source_without_journal() {
    let f = Fixture::new();
    let source = f.add(A, false, "openai");
    let original = fs::read(&source).unwrap();
    let target = f.target.join("sessions").join(source.file_name().unwrap());
    let conflict = f.import_options(A, true, &["--move", "--keep-source"]);
    assert_eq!(conflict.status.code(), Some(2));
    let diagnostic = String::from_utf8_lossy(&conflict.stderr);
    assert!(diagnostic.contains("--move") && diagnostic.contains("--keep-source"));
    assert!(!f.qb.join("journal/sessions-import.json").exists());
    assert!(
        !f.qb
            .join(format!("journal/session-moves/{A}.json"))
            .exists()
    );
    assert!(!f.target.join("state_5.sqlite").exists());
    assert!(!target.exists());
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(thread_path(&f.source, A), Some(source.clone()));
    assert!(!f.sentinel.exists());

    let moved = f.import_options(A, true, &["--move"]);
    assert_success(&moved);
    assert!(String::from_utf8_lossy(&moved.stdout).contains("결과 | 이동 |"));
    assert!(!source.exists());
    assert_eq!(fs::read(&target).unwrap(), original);
    assert!(thread_path(&f.source, A).is_none());
    assert_eq!(thread_path(&f.target, A), Some(target));
    assert!(
        f.qb.join(format!("journal/session-moves/{A}.json"))
            .exists()
    );
    assert!(!f.sentinel.exists());
}

#[test]
fn r9_cli_live_unknown_provider_and_missing_wiring_refuse() {
    let f = Fixture::new();
    let source = f.add(A, false, "other");
    assert!(!f.import(A, true).status.success());
    assert!(!f.qb.join("journal/sessions-import.json").exists());
    assert!(
        !f.qb
            .join(format!("journal/session-moves/{A}.json"))
            .exists()
    );
    fs::OpenOptions::new()
        .append(true)
        .open(source)
        .unwrap()
        .write_all(b"{}\n")
        .unwrap();
    fs::remove_file(f.qb.join("journal/wiring.json")).unwrap();
    assert!(!f.import(A, true).status.success());
}

#[test]
fn r91_list_marks_duplicate_physical_id_and_keeps_unrelated_sessions_visible() {
    let f = Fixture::new();
    let a = f.add(A, false, "openai");
    let b = f.add(B, false, "openai");
    let duplicate = f
        .source
        .join("archived_sessions")
        .join(a.file_name().unwrap());
    fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
    fs::copy(&a, &duplicate).unwrap();
    let before_a = fs::read(&a).unwrap();
    let before_b = fs::read(&b).unwrap();
    let result = f.cli(&[
        "sessions".as_ref(),
        "import".as_ref(),
        "--from".as_ref(),
        f.source.as_os_str(),
        "--list".as_ref(),
    ]);
    assert_success(&result);
    let text = String::from_utf8_lossy(&result.stdout);
    assert!(text.contains(&format!("{A} · 물리 ID 중복")));
    assert!(text.lines().any(|line| line == B));
    assert_eq!(fs::read(&a).unwrap(), before_a);
    assert_eq!(fs::read(&b).unwrap(), before_b);
    assert!(!f.target.join("sessions").exists());
    assert!(!f.qb.join("journal/sessions-import.json").exists());
}
