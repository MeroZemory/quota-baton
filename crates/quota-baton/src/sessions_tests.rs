use super::*;
use rusqlite::Connection;
use std::time::Duration;
const A: &str = "11111111-1111-1111-1111-111111111111";
const B: &str = "22222222-2222-2222-2222-222222222222";
const C: &str = "33333333-3333-3333-3333-333333333333";

#[test]
fn r9_cli_diagnostics_allow_only_fixed_messages_not_attached_content() {
    let fixed = "대화 기록이 분기되었습니다. sessions unimport 후 다시 가져오세요";
    assert_eq!(
        friendly_error(&anyhow::anyhow!(fixed)).as_deref(),
        Some(fixed)
    );
    assert!(friendly_error(&anyhow::anyhow!("{fixed} SYNTHETIC-PRIVATE")).is_none());
    assert!(friendly_error(&anyhow::anyhow!("SYNTHETIC-PRIVATE SQL error")).is_none());
    assert!(
        friendly_error(&anyhow::anyhow!(
            "같은 대화 기록 ID가 서로 다른 파일에 중복되어 있습니다: SYNTHETIC-PRIVATE"
        ))
        .is_none()
    );
    assert!(
        friendly_error(&anyhow::anyhow!(
            "압축 대화 메타데이터를 안전하게 읽을 수 없습니다: SYNTHETIC-PRIVATE"
        ))
        .is_none()
    );
    assert_eq!(goal_status("active"), "진행 중");
    assert_eq!(goal_status("SYNTHETIC-PRIVATE"), "알 수 없음");
}

#[test]
fn r9_resume_uuid_parses_global_resume_options_values_and_separator() {
    for args in [
        vec!["resume", A],
        vec!["resume", "--all", A],
        vec!["-m", "model", "--no-daemon", "resume", "--all", A],
        vec!["--config", "x=\"resume\"", "resume", "--", A],
        vec!["exec", "--json", "resume", A],
        vec!["--config", "--last", "resume", A],
        vec!["resume", "--", A, "--last"],
    ] {
        assert_eq!(
            resume_uuid(&args.iter().map(OsString::from).collect::<Vec<_>>())
                .unwrap()
                .as_deref(),
            Some(A)
        );
    }
    for args in [
        vec!["resume"],
        vec!["resume", "--last"],
        vec!["--config", A, "resume"],
        vec!["--help", "resume", A],
        vec!["resume", A, "--help"],
        vec!["resume", "--last", A],
        vec!["resume", A, "--last"],
        vec!["exec", "resume", A, "--last"],
        vec!["exec", "a prompt containing resume"],
    ] {
        assert!(
            resume_uuid(&args.iter().map(OsString::from).collect::<Vec<_>>())
                .unwrap()
                .is_none()
        );
    }
    assert!(
        resume_uuid(&["resume".into(), "--unknown".into(), A.into()])
            .unwrap()
            .is_none()
    );
    assert_eq!(
        resume_uuid(&["resume".into(), A.into(), "--native-new".into()])
            .unwrap()
            .as_deref(),
        Some(A)
    );
}

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
        let map_path = root.path().join("herdr/session-homes.json");
        Self {
            _root: root,
            source,
            store: Store {
                home,
                target,
                search_homes: Vec::new(),
                map_path,
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
        import_one(
            &self.store,
            &c,
            id,
            ImportOptions {
                apply,
                skip_existing: false,
                announce: false,
                keep_source: true,
                clean_stale: false,
            },
        )
    }
}

fn source_snapshot(home: &Path) -> [Vec<u8>; 3] {
    ["state_5.sqlite", "goals_1.sqlite", "session_index.jsonl"]
        .map(|name| fs::read(home.join(name)).unwrap())
}

fn assert_no_move_journal(f: &Fixture, id: &str) {
    assert!(!f.store.path().exists());
    assert!(
        !f.store
            .home
            .join("journal/session-moves")
            .join(format!("{id}.json"))
            .exists()
    );
    assert!(!f.store.target.join("state_5.sqlite").exists());
    assert!(!f.store.target.join("goals_1.sqlite").exists());
    assert!(!f.store.target.join("sessions").exists());
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
    let error = import_one_using(
        &f.store,
        &catalog,
        A,
        ImportOptions {
            apply: true,
            skip_existing: false,
            announce: false,
            keep_source: true,
            clean_stale: false,
        },
        |_, _| Err(std::io::Error::from_raw_os_error(17)),
    )
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
        import_one_using(
            &f.store,
            &catalog,
            B,
            ImportOptions {
                apply: true,
                skip_existing: false,
                announce: false,
                keep_source: true,
                clean_stale: false
            },
            |a, b| {
                let count = calls.get();
                calls.set(count + 1);
                if count == 0 {
                    fs::hard_link(a, b)
                } else {
                    Err(std::io::Error::from_raw_os_error(17))
                }
            }
        )
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

#[test]
fn r9_unimport_metadata_failure_is_not_reusable_and_can_finish_after_repair() {
    let f = Fixture::new();
    f.add(A, None, false, "active");
    f.import(A, true).unwrap();
    let goals = Connection::open(f.store.target.join("goals_1.sqlite")).unwrap();
    goals
        .execute(
            "UPDATE thread_goals SET objective='synthetic changed' WHERE thread_id=?1",
            [A],
        )
        .unwrap();
    assert!(unimport(&f.store, A, true, false).is_err());
    assert!(!f.store.load().unwrap().imports[A].complete);
    assert!(f.import(A, true).is_err());
    goals
        .execute(
            "UPDATE thread_goals SET objective='SYNTHETIC PRIVATE OBJECTIVE' WHERE thread_id=?1",
            [A],
        )
        .unwrap();
    drop(goals);
    unimport(&f.store, A, true, false).unwrap();
    assert!(f.store.load().unwrap().imports.is_empty());
}

#[test]
fn r9_compressed_sibling_does_not_block_owned_plain_unimport() {
    let f = Fixture::new();
    let source = f.add(A, None, false, "complete");
    f.import(A, true).unwrap();
    let compressed = source.with_extension("jsonl.zst");
    fs::write(&compressed, b"synthetic compressed sibling").unwrap();
    let record = f.store.load().unwrap().imports[A].clone();
    assert!(verify_links(&record, false).is_err());
    unimport(&f.store, A, true, false).unwrap();
    assert!(source.exists() && compressed.exists());
    assert!(!record.files[0].target.exists());
}

#[test]
fn r9_unimport_transfers_current_borrowed_ancestor_goal_without_deleting_it() {
    let f = Fixture::new();
    f.add(A, None, true, "active");
    f.add(B, Some(A), false, "complete");
    f.add(C, Some(A), false, "complete");
    f.import(B, true).unwrap();
    let goals = Connection::open(f.store.target.join("goals_1.sqlite")).unwrap();
    goals
        .execute(
            "UPDATE thread_goals SET updated_at_ms=123 WHERE thread_id=?1",
            [A],
        )
        .unwrap();
    drop(goals);
    f.import(C, true).unwrap();
    unimport(&f.store, B, true, false).unwrap();
    let goals = Connection::open(f.store.target.join("goals_1.sqlite")).unwrap();
    assert_eq!(
        goals
            .query_row(
                "SELECT updated_at_ms FROM thread_goals WHERE thread_id=?1",
                [A],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        123
    );
    drop(goals);
    unimport(&f.store, C, true, false).unwrap();
}

#[test]
fn r9_move_refuses_unfenceable_fork_ancestor_without_changing_source() {
    let f = Fixture::new();
    let parent = f.add(A, None, true, "active");
    let child = f.add(B, Some(A), false, "usage_limited");
    let parent_bytes = fs::read(&parent).unwrap();
    let child_bytes = fs::read(&child).unwrap();
    let before = source_snapshot(&f.source);
    let catalog = discovery::catalog(&f.source).unwrap();
    let error = import_one(
        &f.store,
        &catalog,
        B,
        ImportOptions {
            apply: true,
            skip_existing: false,
            announce: false,
            keep_source: false,
            clean_stale: false,
        },
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "조상 전용 대화의 모든 관리 재개 경로를 차단할 수 없어 이 대화의 이동을 거절합니다"
    );
    assert!(parent.exists() && child.exists());
    assert!(fs::read(parent).unwrap() == parent_bytes);
    assert!(fs::read(child).unwrap() == child_bytes);
    assert!(
        source_snapshot(&f.source) == before,
        "refusal changed thread/goal rows or the source index"
    );
    assert_eq!(db::threads(&f.source).unwrap().len(), 2);
    assert!(db::thread_path(&f.source, A).unwrap().is_some());
    assert!(db::thread_path(&f.source, B).unwrap().is_some());
    assert_no_move_journal(&f, B);
}

#[test]
fn r9_move_verification_failure_never_removes_source_name_or_rows() {
    let f = Fixture::new();
    let original = f.add(A, None, false, "active");
    let catalog = discovery::catalog(&f.source).unwrap();
    assert!(
        import_one_using(
            &f.store,
            &catalog,
            A,
            ImportOptions {
                apply: true,
                skip_existing: false,
                announce: false,
                keep_source: false,
                clean_stale: false
            },
            |source, target| {
                fs::hard_link(source, target)?;
                fs::remove_file(target)?;
                fs::write(target, b"synthetic wrong identity")
            }
        )
        .is_err()
    );
    assert!(original.exists());
    let record = f.store.load().unwrap().imports[A].clone();
    assert!(!record.move_started && !record.complete);
    let db = Connection::open(f.source.join("state_5.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM threads WHERE id=?1", [A], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn r9_stale_copy_requires_explicit_cleanup_and_unimport_restores_it() {
    let mut f = Fixture::new();
    let old = Fixture::new();
    let current = f.add(A, None, false, "active");
    let stale = old.add(A, None, false, "active");
    File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(600)))
        .unwrap();
    f.store.search_homes = vec![old.source.clone()];
    let catalog = discovery::catalog(&f.source).unwrap();
    let plan = stale_plan(&f.store, &catalog, A).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(plan[0].names[0].bytes > 0);
    import_one(
        &f.store,
        &catalog,
        A,
        ImportOptions {
            apply: false,
            skip_existing: false,
            announce: false,
            keep_source: true,
            clean_stale: true,
        },
    )
    .unwrap();
    assert!(current.exists() && stale.exists());
    import_one(
        &f.store,
        &catalog,
        A,
        ImportOptions {
            apply: true,
            skip_existing: false,
            announce: false,
            keep_source: true,
            clean_stale: true,
        },
    )
    .unwrap();
    assert!(current.exists() && !stale.exists());
    let record = f.store.load().unwrap().imports[A].clone();
    assert!(record.stale[0].names[0].backup.exists());
    unimport(&f.store, A, true, false).unwrap();
    assert!(current.exists() && stale.exists());
    let db = Connection::open(old.source.join("goals_1.sqlite")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT status FROM thread_goals WHERE thread_id=?1",
            [A],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "active"
    );
}

#[test]
fn r9_bulk_move_refuses_dependency_chain_even_in_leaf_first_order() {
    let f = Fixture::new();
    let parent = f.add(A, None, false, "active");
    let child = f.add(B, Some(A), false, "usage_limited");
    let leaf = f.add(C, Some(B), false, "complete");
    let contents = [&parent, &child, &leaf].map(|path| fs::read(path).unwrap());
    let before = source_snapshot(&f.source);
    let catalog = discovery::catalog(&f.source).unwrap();
    for id in [C, B, A] {
        let error = import_one(
            &f.store,
            &catalog,
            id,
            ImportOptions {
                apply: true,
                skip_existing: false,
                announce: false,
                keep_source: false,
                clean_stale: false,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            if id == A {
                "이 대화에서 갈라진 대화가 있습니다. 가족 이동을 지원하기 전에는 옮길 수 없습니다"
            } else {
                "조상 전용 대화의 모든 관리 재개 경로를 차단할 수 없어 이 대화의 이동을 거절합니다"
            }
        );
        assert_no_move_journal(&f, id);
    }
    assert!(parent.exists() && child.exists() && leaf.exists());
    for (path, bytes) in [&parent, &child, &leaf].iter().zip(contents) {
        assert!(fs::read(path).unwrap() == bytes);
    }
    assert!(source_snapshot(&f.source) == before);
    assert_eq!(db::threads(&f.source).unwrap().len(), 3);
    assert!(db::threads(&f.store.target).unwrap().is_empty());
}

#[test]
fn r9_move_interrupted_after_journal_rows_or_unlinks_restores_from_journal() {
    for boundary in 0..3 {
        let f = Fixture::new();
        let source = f.add(A, None, false, "active");
        f.import(A, true).unwrap();
        let mut journal = f.store.load().unwrap();
        let record = journal.imports.get_mut(A).unwrap();
        record.keep_source = false;
        record.complete = false;
        record.move_started = true;
        for link in &mut record.files {
            link.moved = true;
        }
        let snapshot = db::source_snapshot(&f.source, &[A.into()]).unwrap();
        record.source_database =
            crate::cutover::protect(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        f.store.save(&journal).unwrap();
        if boundary >= 1 {
            db::remove_exact(&snapshot).unwrap();
        }
        if boundary >= 2 {
            fs::remove_file(&source).unwrap();
        }
        unimport(&f.store, A, true, false).unwrap();
        assert!(source.exists(), "boundary {boundary}");
        assert_eq!(db::threads(&f.source).unwrap().len(), 1);
        let goals = Connection::open(f.source.join("goals_1.sqlite")).unwrap();
        assert_eq!(
            goals
                .query_row(
                    "SELECT status FROM thread_goals WHERE thread_id=?1",
                    [A],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "active"
        );
        assert!(db::threads(&f.store.target).unwrap().is_empty());
        assert!(f.store.load().unwrap().imports.is_empty());
    }
}

#[test]
fn r9_stale_cleanup_refuses_move_with_an_unmoved_fork_descendant() {
    let mut f = Fixture::new();
    let old = Fixture::new();
    let current = f.add(A, None, false, "active");
    let stale = old.add(A, None, false, "active");
    let child = old.add(B, Some(A), false, "active");
    File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(600)))
        .unwrap();
    f.store.search_homes = vec![old.source.clone()];
    let before = source_snapshot(&f.source);
    let old_before = source_snapshot(&old.source);
    let contents = [&current, &stale, &child].map(|path| fs::read(path).unwrap());
    let catalog = discovery::catalog(&f.source).unwrap();
    let error = import_one(
        &f.store,
        &catalog,
        A,
        ImportOptions {
            apply: true,
            skip_existing: false,
            announce: false,
            keep_source: false,
            clean_stale: true,
        },
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "이 대화에서 갈라진 대화가 있습니다. 가족 이동을 지원하기 전에는 옮길 수 없습니다"
    );
    assert!(current.exists() && stale.exists() && child.exists());
    for (path, bytes) in [&current, &stale, &child].iter().zip(contents) {
        assert!(fs::read(path).unwrap() == bytes);
    }
    assert!(source_snapshot(&f.source) == before);
    assert!(source_snapshot(&old.source) == old_before);
    assert_eq!(db::threads(&f.source).unwrap().len(), 1);
    assert_eq!(db::threads(&old.source).unwrap().len(), 2);
    assert_no_move_journal(&f, A);
}

#[test]
fn r9_move_refreshes_dependencies_and_refuses_late_child() {
    let f = Fixture::new();
    let parent = f.add(A, None, false, "active");
    let cached = discovery::catalog(&f.source).unwrap();
    let child = f.add(B, Some(A), false, "active");
    let before = source_snapshot(&f.source);
    let parent_bytes = fs::read(&parent).unwrap();
    let child_bytes = fs::read(&child).unwrap();
    let error = import_one(
        &f.store,
        &cached,
        A,
        ImportOptions {
            apply: true,
            skip_existing: false,
            announce: false,
            keep_source: false,
            clean_stale: false,
        },
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "이 대화에서 갈라진 대화가 있습니다. 가족 이동을 지원하기 전에는 옮길 수 없습니다"
    );
    assert!(parent.exists() && child.exists());
    assert!(fs::read(parent).unwrap() == parent_bytes);
    assert!(fs::read(child).unwrap() == child_bytes);
    assert!(source_snapshot(&f.source) == before);
    assert!(db::thread_path(&f.source, A).unwrap().is_some());
    assert!(db::thread_path(&f.source, B).unwrap().is_some());
    assert!(db::thread_path(&f.store.target, A).unwrap().is_none());
    assert!(db::thread_path(&f.store.target, B).unwrap().is_none());
    assert!(discovery::catalog(&f.source).unwrap().chain(B).is_ok());
    assert_no_move_journal(&f, A);
}

#[test]
fn r9_pending_root_is_never_shadowed_by_completed_child_provenance() {
    let f = Fixture::new();
    f.add(C, None, false, "active");
    f.add(A, Some(C), false, "active");
    f.import(A, true).unwrap();
    f.import(C, true).unwrap();
    let mut journal = f.store.load().unwrap();
    assert_eq!(resume_entry(&journal, C).unwrap().unwrap().root, C);
    journal.imports.get_mut(C).unwrap().complete = false;
    journal.imports.get_mut(C).unwrap().move_started = true;
    f.store.save(&journal).unwrap();
    assert!(resume_entry(&f.store.load().unwrap(), C).is_err());
    assert!(resume_entry(&f.store.load().unwrap(), A).is_ok());
}

#[test]
fn r9_paginated_stale_rows_deleted_before_rename_recover_from_journal() {
    let mut f = Fixture::new();
    let old = Fixture::new();
    f.add(A, None, false, "active");
    let first = old.add(A, None, false, "active");
    let second = first
        .parent()
        .unwrap()
        .join(format!("rollout-2026-10-21T00-00-00-{B}.jsonl"));
    let mut meta: serde_json::Value = serde_json::from_slice(&fs::read(&first).unwrap()).unwrap();
    meta["payload"]["history_base"] =
        serde_json::json!({"thread_id":A,"end_ordinal_exclusive":5,"end_byte_offset":100});
    fs::write(&second, format!("{meta}\n")).unwrap();
    let current = discovery::catalog(&f.source)
        .unwrap()
        .active(A)
        .unwrap()
        .path
        .clone();
    let current_second = current.parent().unwrap().join(second.file_name().unwrap());
    fs::write(&current_second, format!("{meta}\n")).unwrap();
    File::options()
        .write(true)
        .open(&current_second)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)))
        .unwrap();
    let current_db = Connection::open(f.source.join("state_5.sqlite")).unwrap();
    current_db
        .execute(
            "UPDATE threads SET rollout_path=?1 WHERE id=?2",
            rusqlite::params![current_second.to_str().unwrap(), A],
        )
        .unwrap();
    drop(current_db);

    for file in [&first, &second] {
        File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(600)),
            )
            .unwrap();
    }
    let db = Connection::open(old.source.join("state_5.sqlite")).unwrap();
    db.execute(
        "UPDATE threads SET rollout_path=?1 WHERE id=?2",
        rusqlite::params![second.to_str().unwrap(), A],
    )
    .unwrap();
    drop(db);
    f.store.search_homes = vec![old.source.clone()];
    let catalog = discovery::catalog(&f.source).unwrap();
    let stale = stale_plan(&f.store, &catalog, A).unwrap();
    assert_eq!(stale[0].names.len(), 2);
    f.import(A, true).unwrap();
    let mut journal = f.store.load().unwrap();
    let entry = journal.imports.get_mut(A).unwrap();
    entry.complete = false;
    entry.stale = stale;
    let snapshot: db::DbPlan =
        serde_json::from_slice(&crate::cutover::unprotect(&entry.stale[0].database).unwrap())
            .unwrap();
    f.store.save(&journal).unwrap();
    db::remove_exact(&snapshot).unwrap();
    assert!(discovery::catalog(&old.source).unwrap().chain(A).is_err());
    unimport(&f.store, A, true, false).unwrap();
    assert!(first.exists() && second.exists());
    assert_eq!(
        discovery::catalog(&old.source)
            .unwrap()
            .chain(A)
            .unwrap()
            .len(),
        2
    );
    assert!(f.store.load().unwrap().imports.is_empty());
}

#[test]
fn r9_inventory_refuses_different_content_or_goal_without_timestamp_selection() {
    for goal_conflict in [false, true] {
        let mut f = Fixture::new();
        let other = Fixture::new();
        let original = f.add(A, None, false, "active");
        let peer = other.add(
            A,
            None,
            false,
            if goal_conflict { "complete" } else { "active" },
        );
        if !goal_conflict {
            OpenOptions::new()
                .append(true)
                .open(&peer)
                .unwrap()
                .write_all(b"{\"type\":\"synthetic_divergence\"}\n")
                .unwrap();
        }
        File::options()
            .write(true)
            .open(&peer)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(600)),
            )
            .unwrap();
        f.store.search_homes = vec![other.source.clone()];
        let catalog = discovery::catalog(&f.source).unwrap();
        let error = import_one(
            &f.store,
            &catalog,
            A,
            ImportOptions {
                apply: true,
                skip_existing: false,
                announce: false,
                keep_source: false,
                clean_stale: true,
            },
        )
        .unwrap_err();
        assert!(error.downcast_ref::<InventoryConflict>().is_some());
        let message = friendly_error(&error).unwrap();
        assert!(
            message.contains(&f.source.display().to_string())
                && message.contains(&other.source.display().to_string())
        );
        assert!(!message.contains("SYNTHETIC PRIVATE"));
        assert!(original.exists() && peer.exists());
        assert!(f.store.load().unwrap().imports.is_empty());
    }
}

#[test]
fn r9_move_consolidates_inventory_copies_and_returns_only_current_owner() {
    let mut f = Fixture::new();
    let other = Fixture::new();
    let original = f.add(A, None, false, "active");
    let peer = other.add(A, None, false, "active");
    f.store.search_homes = vec![other.source.clone()];
    let catalog = discovery::catalog(&f.source).unwrap();
    import_one(
        &f.store,
        &catalog,
        A,
        ImportOptions {
            apply: true,
            skip_existing: false,
            announce: false,
            keep_source: false,
            clean_stale: false,
        },
    )
    .unwrap();
    assert!(!original.exists() && !peer.exists());
    assert!(db::threads(&f.source).unwrap().is_empty());
    assert!(db::threads(&other.source).unwrap().is_empty());
    assert_eq!(db::threads(&f.store.target).unwrap().len(), 1);
    let current_path = db::thread_path(&f.store.target, A).unwrap().unwrap();
    let current_identity = guard::identity(&current_path).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&current_path)
        .unwrap()
        .write_all(b"{\"type\":\"synthetic_current_state\"}\n")
        .unwrap();
    File::options()
        .write(true)
        .open(&current_path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)))
        .unwrap();
    let current = fs::read(&current_path).unwrap();
    {
        let goals = Connection::open(f.store.target.join("goals_1.sqlite")).unwrap();
        assert_eq!(
            goals
                .execute(
                    "UPDATE thread_goals SET status='complete',updated_at_ms=7 WHERE thread_id=?1",
                    [A]
                )
                .unwrap(),
            1
        );
    }
    // Staged unimport/return is a reverse CURRENT move. It does not resurrect
    // every pre-consolidation owner or restore its old goal snapshot.
    for _ in 0..2 {
        staged::return_current(&f.store, A, None, true, true).unwrap();
        assert!(original.exists() && !peer.exists());
        assert!(fs::read(&original).unwrap() == current);
        assert_eq!(guard::identity(&original).unwrap(), current_identity);
        assert_eq!(db::threads(&f.source).unwrap().len(), 1);
        assert!(db::threads(&other.source).unwrap().is_empty());
        assert!(db::threads(&f.store.target).unwrap().is_empty());
        let goals = Connection::open(f.source.join("goals_1.sqlite")).unwrap();
        let state = goals
            .query_row(
                "SELECT status,updated_at_ms FROM thread_goals WHERE thread_id=?1",
                [A],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap();
        assert_eq!(state, ("complete".into(), 7));
    }
}

#[test]
fn r9_return_index_appends_current_name_over_old_home_hint() {
    let f = Fixture::new();
    f.add(A, None, false, "active");
    let old = format!(
        "{}\n",
        serde_json::json!({"id":A,"thread_name":"old-home-name","updated_at":"2026-10-20T00:00:00Z"})
    );
    fs::write(f.store.target.join("session_index.jsonl"), old.as_bytes()).unwrap();
    let current = serde_json::json!({"id":A,"thread_name":"current-owner-name","updated_at":"2026-10-21T00:00:00Z"});
    // The exact current JSON also occurred before the stale latest hint.
    fs::write(
        f.store.target.join("session_index.jsonl"),
        format!("{current}\n{old}"),
    )
    .unwrap();
    fs::write(f.source.join("session_index.jsonl"), format!("{current}\n")).unwrap();
    let plan = db::plan_return(&f.source, &f.store.target, &[A.into()]).unwrap();
    let append = index_plan(&f.store.target, &plan).unwrap();
    assert_eq!(append.len(), 1);
    update_index(&f.store.target, &append, false).unwrap();
    let bytes = fs::read_to_string(f.store.target.join("session_index.jsonl")).unwrap();
    assert!(bytes.starts_with(&format!("{current}\n{old}")));
    assert_eq!(bytes.lines().count(), 3);
    let newest: serde_json::Value = serde_json::from_str(bytes.lines().last().unwrap()).unwrap();
    assert_eq!(newest, current);
    assert!(index_plan(&f.store.target, &plan).unwrap().is_empty());
    update_index(&f.store.target, &append, false).unwrap();
    assert_eq!(
        fs::read_to_string(f.store.target.join("session_index.jsonl")).unwrap(),
        bytes
    );
}

#[test]
fn r9_import_refuses_destination_without_row_at_other_or_archived_path() {
    for archived in [false, true] {
        let f = Fixture::new();
        let source = f.add(A, None, false, "active");
        let extra = f
            .store
            .target
            .join(if archived {
                "archived_sessions"
            } else {
                "sessions/other"
            })
            .join(source.file_name().unwrap());
        fs::create_dir_all(extra.parent().unwrap()).unwrap();
        let mut body = fs::read(&source).unwrap();
        body.extend_from_slice(b"{\"type\":\"synthetic_divergence\"}\n");
        fs::write(&extra, &body).unwrap();
        for apply in [false, true] {
            let catalog = discovery::catalog(&f.source).unwrap();
            let error = import_one(
                &f.store,
                &catalog,
                A,
                ImportOptions {
                    apply,
                    skip_existing: false,
                    announce: false,
                    keep_source: false,
                    clean_stale: false,
                },
            )
            .unwrap_err();
            assert!(error.downcast_ref::<InventoryConflict>().is_some());
            assert!(source.exists());
            assert_eq!(fs::read(&extra).unwrap(), body);
            assert!(!f.store.path().exists());
            assert!(!f.store.target.join("state_5.sqlite").exists());
            assert!(!f.store.target.join("sessions/2026/10/21").exists());
        }
    }
}

#[test]
fn bulk_excluded_item_stays_at_source_even_if_its_transient_refusal_disappears() {
    let f = Fixture::new();
    let a = f.add(A, None, false, "active");
    let b = f.add(B, None, false, "active");
    File::options()
        .write(true)
        .open(&a)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now()))
        .unwrap();
    let catalog = discovery::catalog(&f.source).unwrap();
    let (included, refused) = preview_imports(
        &f.store,
        &catalog,
        &[A.into(), B.into()],
        true,
        false,
        false,
        false,
    )
    .unwrap();
    assert_eq!(included, vec![B.to_owned()]);
    assert_eq!(refused, [1, 0, 0]);
    File::options()
        .write(true)
        .open(&a)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)))
        .unwrap();
    let options = |apply| ImportOptions {
        apply,
        skip_existing: false,
        announce: false,
        keep_source: false,
        clean_stale: false,
    };
    assert!(import_one(&f.store, &catalog, A, options(false)).is_ok());
    for id in included {
        import_one(&f.store, &catalog, &id, options(true)).unwrap();
    }
    assert!(a.exists());
    assert!(!b.exists());
    assert!(
        discovery::catalog(&f.store.target)
            .unwrap()
            .threads
            .contains_key(B)
    );
    assert!(
        !discovery::catalog(&f.store.target)
            .unwrap()
            .threads
            .contains_key(A)
    );
}

#[test]
fn share_related_unreadable_compressed_inventory_refuses_before_target_mutation() {
    for location in ["source", "target", "peer"] {
        let mut f = Fixture::new();
        let original = f.add(A, None, false, "active");
        let home = match location {
            "source" => f.source.clone(),
            "target" => f.store.target.clone(),
            _ => {
                let peer = f._root.path().join("peer");
                fs::create_dir_all(&peer).unwrap();
                f.store.search_homes.push(peer.clone());
                peer
            }
        };
        let corrupt = home
            .join("archived_sessions")
            .join(format!("rollout-2026-10-21T00-00-00-{A}.jsonl.zst"));
        fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        fs::write(&corrupt, b"SYNTHETIC corrupt compressed header").unwrap();
        for apply in [false, true] {
            let result = f.import(A, apply);
            assert!(
                result.is_err(),
                "{location} preflight allowed a related unreadable rollout"
            );
            assert!(original.exists());
            assert!(!f.store.path().exists());
            assert!(!f.store.target.join("state_5.sqlite").exists());
            assert!(!f.store.target.join("goals_1.sqlite").exists());
            assert!(!f.store.target.join("session_index.jsonl").exists());
            assert!(!f.store.target.join("sessions").exists());
            assert_eq!(
                fs::read(&corrupt).unwrap(),
                b"SYNTHETIC corrupt compressed header"
            );
        }
    }
}

#[test]
fn share_unrelated_unreadable_compressed_inventory_is_allowed() {
    let f = Fixture::new();
    let original = f.add(A, None, false, "active");
    let corrupt = f
        .store
        .target
        .join("archived_sessions")
        .join(format!("rollout-2026-10-21T00-00-00-{C}.jsonl.zst"));
    fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
    fs::write(&corrupt, b"SYNTHETIC unrelated corrupt compressed header").unwrap();
    assert!(f.import(A, false).unwrap());
    assert!(f.import(A, true).unwrap());
    assert!(original.exists());
    assert_eq!(
        fs::read(&corrupt).unwrap(),
        b"SYNTHETIC unrelated corrupt compressed header"
    );
    assert!(f.store.load().unwrap().imports[A].complete);
}

#[test]
fn share_selected_compressed_rollout_keeps_original_bytes_and_file_identity() {
    let f = Fixture::new();
    let original = f.add(A, None, false, "active");
    let compressed = original.with_extension("jsonl.zst");
    let body = zstd::stream::encode_all(fs::read(&original).unwrap().as_slice(), 1).unwrap();
    fs::write(&compressed, &body).unwrap();
    fs::remove_file(original).unwrap();
    File::options()
        .write(true)
        .open(&compressed)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)))
        .unwrap();
    Connection::open(f.source.join("state_5.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET rollout_path=?1 WHERE id=?2",
            rusqlite::params![compressed.to_str().unwrap(), A],
        )
        .unwrap();
    let identity = guard::identity(&compressed).unwrap();
    assert!(f.import(A, false).unwrap());
    assert!(f.import(A, true).unwrap());
    let journal = f.store.load().unwrap();
    let shared = &journal.imports[A].files[0].target;
    assert_eq!(fs::read(&compressed).unwrap(), body);
    assert_eq!(fs::read(shared).unwrap(), body);
    assert_eq!(guard::identity(shared).unwrap(), identity);
    assert_eq!(guard::hardlink_count(shared).unwrap(), 2);
    assert!(journal.imports[A].complete);
    unimport(&f.store, A, false, false).unwrap();
    unimport(&f.store, A, true, false).unwrap();
    assert!(!shared.exists());
    assert_eq!(fs::read(&compressed).unwrap(), body);
    assert_eq!(guard::identity(&compressed).unwrap(), identity);
    assert_eq!(guard::hardlink_count(&compressed).unwrap(), 1);
}

#[path = "sessions_share_safety_tests.rs"]
mod share_safety;
