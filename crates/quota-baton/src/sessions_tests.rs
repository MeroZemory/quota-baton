use super::*;
use rusqlite::Connection;
use std::time::Duration;
const A: &str = "11111111-1111-1111-1111-111111111111";
const B: &str = "22222222-2222-2222-2222-222222222222";
const C: &str = "33333333-3333-3333-3333-333333333333";

struct Fixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    store: Store,
}
impl Fixture {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r9-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        let home = root.path().join("qb");
        for path in [&source, &target, &home.join("journal")] {
            fs::create_dir_all(path).unwrap();
        }
        let source = fs::canonicalize(source).unwrap();
        let target = fs::canonicalize(target).unwrap();
        let db = Connection::open(source.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,model_provider TEXT NOT NULL,archived INTEGER DEFAULT 0); CREATE INDEX idx_thread_provider ON threads(model_provider);").unwrap();
        let goals = Connection::open(source.join("goals_1.sqlite")).unwrap();
        goals.execute_batch("CREATE TABLE thread_goals(thread_id TEXT PRIMARY KEY,objective TEXT NOT NULL,status TEXT NOT NULL,updated_at_ms INTEGER NOT NULL);").unwrap();
        Self {
            _root: root,
            source,
            store: Store {
                home,
                target,
                search_homes: Vec::new(),
            },
        }
    }
    fn add(&self, id: &str, parent: Option<&str>, archived: bool, status: &str) -> PathBuf {
        let path = self
            .source
            .join(if archived {
                "archived_sessions"
            } else {
                "sessions/2026/10/21"
            })
            .join(format!("rollout-2026-10-21T00-00-00-{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut meta = serde_json::json!({"type":"session_meta","payload":{"id":id,"model_provider":"openai"}});
        if let Some(parent) = parent {
            meta["payload"]["forked_from_id"] = parent.into();
            meta["payload"]["history_base"] = serde_json::json!({"thread_id":parent,"end_ordinal_exclusive":5,"end_byte_offset":100});
        }
        fs::write(&path, format!("{meta}\n")).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)),
            )
            .unwrap();
        let db = Connection::open(self.source.join("state_5.sqlite")).unwrap();
        db.execute("INSERT INTO threads(id,rollout_path,model_provider,archived) VALUES (?1,?2,'openai',?3)",rusqlite::params![id,path.to_str().unwrap(),i64::from(archived)]).unwrap();
        let db = Connection::open(self.source.join("goals_1.sqlite")).unwrap();
        db.execute(
            "INSERT INTO thread_goals VALUES (?1,'SYNTHETIC PRIVATE OBJECTIVE',?2,0)",
            rusqlite::params![id, status],
        )
        .unwrap();
        let mut index = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.source.join("session_index.jsonl"))
            .unwrap();
        use std::io::Write;
        writeln!(index,"{}",serde_json::json!({"id":id,"thread_name":"SYNTHETIC PRIVATE TITLE","updated_at":"2026-10-20T00:00:00Z"})).unwrap();
        path
    }
    fn import(&self, id: &str, apply: bool) -> Result<bool> {
        let c = discovery::catalog(&self.source)?;
        import_one(&self.store, &c, id, apply, false, false)
    }
}

#[test]
fn r9_three_level_dry_run_apply_idempotent_and_unimport() {
    let f = Fixture::new();
    let original = f.add(A, None, true, "active");
    f.add(B, Some(A), false, "usage_limited");
    f.add(C, Some(B), false, "complete");
    let source_before = fs::read(&original).unwrap();
    f.import(C, false).unwrap();
    assert!(!f.store.path().exists());
    assert!(!f.store.target.join("sessions").exists());
    assert!(!f.store.target.join("state_5.sqlite").exists());
    f.import(C, true).unwrap();
    let journal = f.store.load().unwrap();
    let entry = &journal.imports[C];
    assert_eq!(entry.files.len(), 3);
    assert!(entry.complete);
    for link in &entry.files {
        assert_eq!(
            guard::identity(&link.source).unwrap(),
            guard::identity(&link.target).unwrap()
        );
    }
    assert_eq!(fs::read(&original).unwrap(), source_before);
    let journal_bytes = fs::read_to_string(f.store.path()).unwrap();
    assert!(!journal_bytes.contains("SYNTHETIC PRIVATE"));
    let goals = Connection::open(f.store.target.join("goals_1.sqlite")).unwrap();
    for (id, status) in [(A, "paused"), (B, "paused"), (C, "complete")] {
        assert_eq!(
            goals
                .query_row(
                    "SELECT status FROM thread_goals WHERE thread_id=?1",
                    [id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            status
        );
    }
    drop(goals);
    let index = fs::read(f.store.target.join("session_index.jsonl")).unwrap();
    f.import(C, true).unwrap();
    assert_eq!(
        fs::read(f.store.target.join("session_index.jsonl")).unwrap(),
        index
    );
    unimport(&f.store, C, false, false).unwrap();
    assert!(entry.files.iter().all(|l| l.target.exists()));
    unimport(&f.store, C, true, false).unwrap();
    assert!(
        entry
            .files
            .iter()
            .all(|l| !l.target.exists() && l.source.exists())
    );
    assert!(f.store.load().unwrap().imports.is_empty());
}

#[test]
fn r9_grown_file_and_divergence_refuse_then_force_unimport_only_links() {
    let f = Fixture::new();
    let source = f.add(A, None, false, "complete");
    f.import(A, true).unwrap();
    let entry = f.store.load().unwrap().imports[A].clone();
    use std::io::Write;
    OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"{}\n")
        .unwrap();
    assert!(unimport(&f.store, A, true, false).is_err());
    assert!(entry.files[0].target.exists());
    assert!(verify_links(&entry, false).is_ok());
    unimport(&f.store, A, true, true).unwrap();
    assert!(source.exists());
    assert!(!entry.files[0].target.exists());
    // Re-import with old mtime, then replace one target name while source is intact.
    File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)))
        .unwrap();
    f.import(A, true).unwrap();
    let entry = f.store.load().unwrap().imports[A].clone();
    fs::remove_file(&entry.files[0].target).unwrap();
    fs::write(&entry.files[0].target, b"unrelated").unwrap();
    assert!(verify_links(&entry, false).is_err());
    assert!(unimport(&f.store, A, true, true).is_err());
    assert_eq!(fs::read(&entry.files[0].target).unwrap(), b"unrelated");
}

#[test]
fn r9_sibling_import_keeps_shared_parent_until_last_owner_unimports() {
    let f = Fixture::new();
    f.add(A, None, true, "active");
    f.add(B, Some(A), false, "complete");
    f.add(C, Some(A), false, "complete");
    f.import(B, true).unwrap();
    f.import(C, true).unwrap();
    let parent = f.store.load().unwrap().imports[B].files[0].target.clone();
    unimport(&f.store, B, true, false).unwrap();
    assert!(parent.exists());
    let goals = Connection::open(f.store.target.join("goals_1.sqlite")).unwrap();
    assert_eq!(
        goals
            .query_row(
                "SELECT count(*) FROM thread_goals WHERE thread_id=?1",
                [A],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        1
    );
    drop(goals);
    unimport(&f.store, C, true, false).unwrap();
    assert!(!parent.exists());
}

#[test]
fn r9_no_wiring_unknown_id_and_recent_write_refuse_without_journal() {
    let f = Fixture::new();
    assert!(Store::open(&f.store.home).is_err());
    assert!(f.import(A, true).is_err());
    let source = f.add(A, None, false, "active");
    File::options()
        .write(true)
        .open(source)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now()))
        .unwrap();
    assert!(f.import(A, true).is_err());
    assert!(!f.store.path().exists());
}

#[test]
fn r9_cross_volume_refuses_without_copy_and_pending_journal_rolls_back() {
    let f = Fixture::new();
    let source = f.add(A, None, false, "active");
    let catalog = discovery::catalog(&f.source).unwrap();
    let error = import_one_using(&f.store, &catalog, A, true, false, false, |_, _| {
        Err(std::io::Error::from_raw_os_error(17))
    })
    .unwrap_err();
    assert!(error.to_string().contains("복사하지 않았습니다"));
    let journal = f.store.load().unwrap();
    let record = &journal.imports[A];
    assert!(!record.complete);
    assert!(record.files.iter().all(|f| !f.target.exists()));
    assert!(source.exists());
    unimport(&f.store, A, true, false).unwrap();
    assert!(f.store.load().unwrap().imports.is_empty());
}

#[test]
fn r9_interrupted_after_first_link_can_unimport_and_retry() {
    let f = Fixture::new();
    f.add(A, None, true, "active");
    f.add(B, Some(A), false, "complete");
    let catalog = discovery::catalog(&f.source).unwrap();
    let calls = std::cell::Cell::new(0);
    assert!(
        import_one_using(&f.store, &catalog, B, true, false, false, |a, b| {
            let count = calls.get();
            calls.set(count + 1);
            if count == 0 {
                fs::hard_link(a, b)
            } else {
                Err(std::io::Error::from_raw_os_error(17))
            }
        })
        .is_err()
    );
    let record = f.store.load().unwrap().imports[B].clone();
    assert!(record.files[0].target.exists());
    assert!(!record.files[1].target.exists());
    unimport(&f.store, B, true, false).unwrap();
    assert!(
        record
            .files
            .iter()
            .all(|l| l.source.exists() && !l.target.exists())
    );
    f.import(B, true).unwrap();
    assert!(f.store.load().unwrap().imports[B].complete);
}
