#![cfg(windows)]

use super::*;
use rusqlite::{Connection, params};
use std::{cell::Cell, collections::BTreeMap, fs::OpenOptions, io::Write, rc::Rc, time::Duration};

const ROOT: &str = "11111111-1111-1111-1111-111111111111";
const CHILD: &str = "22222222-2222-2222-2222-222222222222";
const GRANDCHILD: &str = "33333333-3333-3333-3333-333333333333";
const PHYSICAL: &str = "55555555-5555-5555-5555-555555555555";
const SECOND: &str = "66666666-6666-6666-6666-666666666666";
const CHILD_PHYSICAL: &str = "77777777-7777-7777-7777-777777777777";
const GRANDCHILD_PHYSICAL: &str = "88888888-8888-8888-8888-888888888888";
const APPEND: &[u8] = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"marker\":\"SYNTHETIC native append\"}}\n";

type TestHook = Box<dyn FnMut(&str) -> Result<()>>;

/// Hooks belong to this test thread and are restored even if an assertion panics.
struct HookGuard(Option<TestHook>);
impl HookGuard {
    fn install(hook: impl FnMut(&str) -> Result<()> + 'static) -> Self {
        Self(TEST_HOOK.with(|slot| slot.replace(Some(Box::new(hook)))))
    }
}
impl Drop for HookGuard {
    fn drop(&mut self) {
        TEST_HOOK.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

#[derive(Debug)]
struct Interrupted;
impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("synthetic move interruption")
    }
}
impl std::error::Error for Interrupted {}

struct Fixture {
    root: tempfile::TempDir,
    source: PathBuf,
    original: PathBuf,
    store: Store,
}
impl Fixture {
    fn new() -> Result<Self> {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/sessions-move-tests");
        fs::create_dir_all(&base)?;
        let root = tempfile::tempdir_in(fs::canonicalize(base)?)?;
        let source = root.path().join("source");
        let target = root.path().join("target");
        let home = root.path().join("qb");
        for dir in [&source, &target, &home] {
            fs::create_dir_all(dir)?;
        }
        fs::create_dir_all(home.join("journal"))?;
        let source = fs::canonicalize(source)?;
        let target = fs::canonicalize(target)?;
        let home = fs::canonicalize(home)?;
        let original = rollout(&source, ROOT, PHYSICAL, None, None)?;
        schema(&source)?;
        seed_rows(&source, &original)?;
        append_index(&source, "SYNTHETIC source title")?;
        let map_path = root.path().join("herdr/session-homes.json");
        Ok(Self {
            root,
            source,
            original,
            store: Store {
                home,
                target,
                search_homes: Vec::new(),
                map_path,
            },
        })
    }

    fn plan(&self) -> Result<Move> {
        make_plan(
            &self.store,
            &self.source,
            &self.store.target,
            ROOT,
            false,
            &self.source,
        )
    }

    fn hook_store(&self) -> Store {
        Store {
            home: self.store.home.clone(),
            target: self.store.target.clone(),
            search_homes: self.store.search_homes.clone(),
            map_path: self.store.map_path.clone(),
        }
    }
}

fn schema(home: &Path) -> Result<()> {
    Connection::open(home.join("state_5.sqlite"))?.execute_batch(
        "CREATE TABLE threads(
            id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL,
            model_provider TEXT NOT NULL, title TEXT NOT NULL,
            archived INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
            tokens_used INTEGER NOT NULL, created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL, recency_at INTEGER NOT NULL,
            recency_at_ms INTEGER NOT NULL);
         CREATE INDEX idx_thread_provider ON threads(model_provider);",
    )?;
    Connection::open(home.join("goals_1.sqlite"))?.execute_batch(
        "CREATE TABLE thread_goals(
            thread_id TEXT PRIMARY KEY, objective TEXT NOT NULL,
            status TEXT NOT NULL, created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL, tokens_used INTEGER NOT NULL,
            token_budget INTEGER NOT NULL);",
    )?;
    Ok(())
}

fn seed_rows(home: &Path, name: &Path) -> Result<()> {
    Connection::open(home.join("state_5.sqlite"))?.execute(
        "INSERT INTO threads VALUES(
            ?1,?2,'openai','SYNTHETIC source title',0,1000,2000,11,
            1000000,2000000,2000,2000000)",
        params![ROOT, name.to_str().context("synthetic path")?],
    )?;
    Connection::open(home.join("goals_1.sqlite"))?.execute(
        "INSERT INTO thread_goals VALUES(
            ?1,'SYNTHETIC source objective','active',10,20,7,100)",
        [ROOT],
    )?;
    Ok(())
}

fn age(name: &Path) -> Result<()> {
    File::options().write(true).open(name)?.set_times(
        fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)),
    )?;
    Ok(())
}

fn rollout(
    home: &Path,
    logical: &str,
    physical: &str,
    parent: Option<&str>,
    history: Option<&str>,
) -> Result<PathBuf> {
    let name = home
        .join("sessions/2026/10/21")
        .join(format!("rollout-2026-10-21T00-00-00-{physical}.jsonl"));
    fs::create_dir_all(name.parent().context("rollout parent")?)?;
    let mut meta = serde_json::json!({
        "type":"session_meta",
        "payload":{"id":logical,"model_provider":"openai"}
    });
    if let Some(parent) = parent {
        meta["payload"]["forked_from_id"] = parent.into();
    }
    if let Some(history) = history {
        meta["payload"]["history_base"] = serde_json::json!({
            "thread_id":history,"end_ordinal_exclusive":1,"end_byte_offset":1
        });
    }
    fs::write(
        &name,
        format!(
            "{meta}\n{{\"type\":\"event_msg\",\"payload\":{{\"marker\":\"SYNTHETIC root body\"}}}}\n"
        ),
    )?;
    age(&name)?;
    Ok(name)
}

fn append_index(home: &Path, title: &str) -> Result<()> {
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join("session_index.jsonl"))?,
        "{}",
        serde_json::json!({
            "id":ROOT,"thread_name":title,"updated_at":"2026-10-20T00:00:00Z",
            "synthetic_extra":"preserved"
        })
    )?;
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
struct Rows {
    thread: Option<BTreeMap<String, db::DbValue>>,
    goal: Option<BTreeMap<String, db::DbValue>>,
}
fn rows(home: &Path) -> Result<Rows> {
    let row = db::source_snapshot(home, &[ROOT.into()])?
        .threads
        .into_iter()
        .next()
        .context("root row snapshot")?;
    Ok(Rows {
        thread: row.thread.map(|r| r.values),
        goal: row.goal.map(|r| r.values),
    })
}
fn no_rows(home: &Path) -> Result<()> {
    let current = rows(home)?;
    assert!(current.thread.is_none() && current.goal.is_none());
    Ok(())
}
fn text(value: &str) -> db::DbValue {
    db::DbValue::Text(value.as_bytes().to_vec())
}

fn public_names(home: &Path, id: &str) -> Result<Vec<PathBuf>> {
    Ok(discovery::catalog(home)?
        .all()
        .filter(|r| r.thread == id)
        .map(|r| r.path.clone())
        .collect())
}

#[derive(Clone)]
struct FileWitness {
    names: Vec<PathBuf>,
    stage: PathBuf,
    target: Option<PathBuf>,
    identity: guard::FileIdentity,
    body: Vec<u8>,
}
#[derive(Clone)]
struct Witness {
    files: Vec<FileWitness>,
    target: PathBuf,
}
impl Witness {
    fn new(m: &Move) -> Result<Self> {
        Ok(Self {
            files: m
                .files
                .iter()
                .map(|f| {
                    Ok(FileWitness {
                        names: f.sources.clone(),
                        stage: f.stage.clone(),
                        target: f.target.clone(),
                        identity: f.identity,
                        body: fs::read(&f.sources[0])?,
                    })
                })
                .collect::<Result<_>>()?,
            target: m.target.clone(),
        })
    }

    fn check(&self, durable: &Move, label: &str) -> Result<()> {
        for f in &self.files {
            let live = f
                .names
                .iter()
                .chain(std::iter::once(&f.stage))
                .chain(f.target.iter())
                .filter(|p| p.exists())
                .collect::<Vec<_>>();
            assert!(!live.is_empty(), "last file name lost at {label}");
            for name in &live {
                assert_eq!(guard::identity(name)?, f.identity, "identity at {label}");
                assert_eq!(fs::read(name)?, f.body, "body at {label}");
                let meta: serde_json::Value =
                    serde_json::from_slice(f.body.split(|b| *b == b'\n').next().unwrap())?;
                assert_eq!(meta["payload"]["id"], ROOT);
            }
            assert_eq!(guard::hardlink_count(live[0])? as usize, live.len());
            if f.target.as_ref().is_some_and(|p| p.exists()) {
                assert!(f.names.iter().all(|p| !p.exists()));
            }
        }
        if durable.phase < Phase::Decided || durable.phase == Phase::Aborted || label == "decided" {
            assert!(public_names(&self.target, ROOT)?.is_empty());
            no_rows(&self.target)?;
        }
        Ok(())
    }
}

fn interrupt_plan(f: &Fixture, m: Move, label: &'static str, nth: usize) -> Result<Witness> {
    let witness = Witness::new(&m)?;
    let observed = witness.clone();
    let store = f.hook_store();
    let seen = Rc::new(Cell::new(0));
    let count = seen.clone();
    let hook = HookGuard::install(move |current| {
        observed.check(&load(&store, ROOT)?.context("saved move")?, current)?;
        if current == label {
            count.set(count.get() + 1);
            if count.get() == nth {
                return Err(Interrupted.into());
            }
        }
        Ok(())
    });
    let error = run_plan(&f.store, m).expect_err("named interruption must stop execution");
    drop(hook);
    assert!(
        error.is::<Interrupted>(),
        "move failed before injected checkpoint {label}/{nth} (hits={}): {error:#}",
        seen.get()
    );
    assert_eq!(seen.get(), nth, "unreached interruption {label}/{nth}");
    Ok(witness)
}
fn interrupt(f: &Fixture, label: &'static str, nth: usize) -> Result<Witness> {
    interrupt_plan(f, f.plan()?, label, nth)
}

#[derive(Debug, PartialEq, Eq)]
enum Node {
    Directory,
    File(guard::FileIdentity, Vec<u8>),
}
fn tree(root: &Path, include_evidence: bool) -> Result<BTreeMap<PathBuf, Node>> {
    let mut result = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if !include_evidence && entry.file_name() == "session-moves-evidence" {
                continue;
            }
            let name = entry.path();
            let relative = name.strip_prefix(root)?.to_path_buf();
            if entry.file_type()?.is_dir() {
                result.insert(relative, Node::Directory);
                pending.push(name);
            } else {
                result.insert(
                    relative,
                    Node::File(guard::identity(&name)?, fs::read(name)?),
                );
            }
        }
    }
    Ok(result)
}
fn recover_twice(f: &Fixture) -> Result<()> {
    recover(&f.store, ROOT)?;
    let after = tree(f.root.path(), true)?;
    recover(&f.store, ROOT)?;
    assert_eq!(
        tree(f.root.path(), true)?,
        after,
        "closed recovery must be a no-op"
    );
    Ok(())
}

fn assert_source(f: &Fixture, witness: &Witness, original: &Rows) -> Result<()> {
    let m = load(&f.store, ROOT)?.context("saved move")?;
    assert!(m.phase == Phase::Aborted && !open(&m));
    assert!(!pending(&f.store)?);
    assert_eq!(owner(&f.store, ROOT)?, Some(f.source.clone()));
    for file in &witness.files {
        assert!(!file.stage.exists());
        assert!(file.target.as_ref().is_none_or(|p| !p.exists()));
        for name in &file.names {
            assert_eq!(guard::identity(name)?, file.identity);
            assert_eq!(fs::read(name)?, file.body);
        }
    }
    assert!(
        rows(&f.source)? == *original,
        "original rows were not restored exactly"
    );
    no_rows(&f.store.target)?;
    assert!(public_names(&f.store.target, ROOT)?.is_empty());
    assert!(!f.store.map_path.exists());
    Ok(())
}

fn assert_target(f: &Fixture, witness: &Witness) -> Result<()> {
    let m = load(&f.store, ROOT)?.context("saved move")?;
    assert!(m.phase == Phase::Complete && !open(&m));
    assert!(!pending(&f.store)?);
    assert_eq!(owner(&f.store, ROOT)?, Some(f.store.target.clone()));
    for file in &witness.files {
        assert!(file.names.iter().all(|p| !p.exists()));
        if let Some(name) = &file.target {
            assert!(!file.stage.exists());
            assert_eq!(guard::identity(name)?, file.identity);
            assert_eq!(fs::read(name)?, file.body);
        }
    }
    no_rows(&f.source)?;
    let current = rows(&f.store.target)?;
    let goal = current.goal.context("published goal")?;
    assert!(goal.get("status") == Some(&text("paused")));
    assert!(goal.get("objective") == Some(&text("SYNTHETIC source objective")));
    assert!(goal.get("tokens_used") == Some(&db::DbValue::Integer(7)));
    assert!(goal.get("token_budget") == Some(&db::DbValue::Integer(100)));
    let selected = db::thread_path(&f.store.target, ROOT)?.context("published row")?;
    assert!(
        witness
            .files
            .iter()
            .any(|file| file.target.as_ref() == Some(&selected))
    );
    let map: serde_json::Value = serde_json::from_slice(&fs::read(&f.store.map_path)?)?;
    assert_eq!(
        Path::new(map["sessions"][ROOT].as_str().unwrap()),
        f.store.target
    );
    Ok(())
}

#[test]
fn every_forward_checkpoint_preserves_a_file_and_recovers_to_one_public_home() -> Result<()> {
    for (label, nth, decided) in [
        ("prepared", 1, false),
        ("staged", 1, false),
        ("staged", 2, false),
        ("source_name_removed", 1, false),
        ("source_names_removed", 1, false),
        ("source_rows_removed", 1, false),
        ("decided", 1, true),
        ("published", 1, true),
        ("target_rows", 1, true),
        ("map_committed", 1, true),
        ("committed", 1, true),
    ] {
        let f = Fixture::new()?;
        let original = rows(&f.source)?;
        let witness = interrupt(&f, label, nth)?;
        let recovery_witness = witness.clone();
        let store = f.hook_store();
        let hook = HookGuard::install(move |current| {
            recovery_witness.check(&load(&store, ROOT)?.context("saved recovery")?, current)
        });
        recover_twice(&f)?;
        drop(hook);
        if decided {
            assert_target(&f, &witness)?;
        } else {
            assert_source(&f, &witness, &original)?;
        }
    }
    Ok(())
}

#[test]
fn every_paginated_name_stays_recoverable_during_staging_unlink_and_publication() -> Result<()> {
    for (label, nth, decided) in [
        ("staged", 1, false),
        ("staged", 2, false),
        ("staged", 3, false),
        ("source_name_removed", 1, false),
        ("source_name_removed", 2, false),
        ("published", 1, true),
        ("published", 2, true),
    ] {
        let f = Fixture::new()?;
        rollout(&f.source, ROOT, SECOND, None, Some(PHYSICAL))?;
        let original = rows(&f.source)?;
        let witness = interrupt(&f, label, nth)?;
        assert_eq!(witness.files.len(), 2);
        recover_twice(&f)?;
        if decided {
            assert_target(&f, &witness)?;
            assert_eq!(public_names(&f.store.target, ROOT)?.len(), 2);
        } else {
            assert_source(&f, &witness, &original)?;
        }
    }
    Ok(())
}

#[test]
fn interrupted_rollback_keeps_restored_names_and_can_be_repeated() -> Result<()> {
    for label in ["rollback_name_restored", "rollback_complete"] {
        let f = Fixture::new()?;
        let original = rows(&f.source)?;
        let witness = interrupt(&f, "source_rows_removed", 1)?;
        let observed = witness.clone();
        let store = f.hook_store();
        let reached = Rc::new(Cell::new(false));
        let hit = reached.clone();
        let hook = HookGuard::install(move |current| {
            observed.check(&load(&store, ROOT)?.context("rollback record")?, current)?;
            if current == label {
                hit.set(true);
                return Err(Interrupted.into());
            }
            Ok(())
        });
        let error = recover(&f.store, ROOT).expect_err("injected rollback interruption");
        drop(hook);
        assert!(reached.get() && error.is::<Interrupted>());
        assert!(f.original.exists());
        recover_twice(&f)?;
        assert_source(&f, &witness, &original)?;
    }
    Ok(())
}

#[test]
fn planning_and_read_only_queries_create_no_journal_stage_or_locks() -> Result<()> {
    let f = Fixture::new()?;
    let before = tree(f.root.path(), true)?;
    let m = f.plan()?;
    assert!(m.files.iter().all(|file| !file.stage.exists()));
    assert!(load(&f.store, ROOT)?.is_none());
    assert!(!recorded(&f.store, ROOT)?);
    assert!(!pending(&f.store)?);
    assert!(owner(&f.store, ROOT)?.is_none());
    assert!(doctor_records(&f.store)?.is_empty());
    assert!(import(
        &f.store,
        &discovery::catalog(&f.source)?,
        ROOT,
        ImportOptions {
            apply: false,
            skip_existing: false,
            announce: false,
            keep_source: false,
            clean_stale: false,
        },
    )?);
    assert_eq!(tree(f.root.path(), true)?, before);
    assert!(!path(&f.store, ROOT).exists());

    run_plan(&f.store, m)?;
    let published = tree(f.root.path(), true)?;
    return_current(&f.store, ROOT, None, false, false)?;
    return_current(&f.store, ROOT, Some(&f.source), false, false)?;
    assert_eq!(tree(f.root.path(), true)?, published);
    Ok(())
}

#[test]
fn return_preview_with_an_open_journal_never_runs_recovery_or_changes_current_state() -> Result<()>
{
    for (label, nth, decided) in [
        ("prepared", 1, false),
        ("staged", 2, false),
        ("source_name_removed", 1, false),
        ("source_rows_removed", 1, false),
        ("decided", 1, true),
        ("published", 1, true),
        ("target_rows", 1, true),
        ("map_committed", 1, true),
    ] {
        let f = Fixture::new()?;
        let original = rows(&f.source)?;
        let witness = interrupt(&f, label, nth)?;
        let journal = load(&f.store, ROOT)?.context("open preview journal")?;
        assert!(open(&journal));
        let before = tree(f.root.path(), true)?;
        let source_rows = rows(&f.source)?;
        let target_rows = rows(&f.store.target)?;
        for explicit_destination in [false, true] {
            for yes in [false, true] {
                assert!(
                    return_current(
                        &f.store,
                        ROOT,
                        explicit_destination.then_some(f.source.as_path()),
                        yes,
                        false,
                    )
                    .is_err()
                );
                let after = load(&f.store, ROOT)?.context("preserved preview journal")?;
                assert!(open(&after) && after.phase == journal.phase);
                assert!(pending(&f.store)?);
                assert!(rows(&f.source)? == source_rows);
                assert!(rows(&f.store.target)? == target_rows);
                assert_eq!(tree(f.root.path(), true)?, before);
            }
        }
        recover_twice(&f)?;
        if decided {
            assert_target(&f, &witness)?;
        } else {
            assert_source(&f, &witness, &original)?;
        }
    }
    Ok(())
}

#[test]
fn header_descendants_are_transitive_and_refused_even_without_sqlite() -> Result<()> {
    for physical_edges in [false, true] {
        for sqlite_present in [false, true] {
            let f = Fixture::new()?;
            rollout(
                &f.source,
                CHILD,
                CHILD_PHYSICAL,
                (!physical_edges).then_some(ROOT),
                physical_edges.then_some(PHYSICAL),
            )?;
            rollout(
                &f.source,
                GRANDCHILD,
                GRANDCHILD_PHYSICAL,
                (!physical_edges).then_some(CHILD),
                physical_edges.then_some(CHILD_PHYSICAL),
            )?;
            if !sqlite_present {
                fs::remove_file(f.source.join("state_5.sqlite"))?;
                fs::remove_file(f.source.join("goals_1.sqlite"))?;
            }
            let descendants = discovery::catalog(&f.source)?.descendants(ROOT);
            assert!(descendants.contains(CHILD) && descendants.contains(GRANDCHILD));
            let before = tree(f.root.path(), true)?;
            assert!(f.plan().is_err());
            assert_eq!(tree(f.root.path(), true)?, before);
            assert!(!path(&f.store, ROOT).exists());
            assert_eq!(public_names(&f.source, ROOT)?.len(), 1);
            assert!(public_names(&f.store.target, ROOT)?.is_empty());
        }
    }
    Ok(())
}

fn compressed_test_home(f: &mut Fixture, location: &str) -> Result<PathBuf> {
    f.store.search_homes.push(f.source.clone());
    match location {
        "source" => Ok(f.source.clone()),
        "target" => Ok(f.store.target.clone()),
        "inventory" => {
            let peer = f.root.path().join("peer");
            fs::create_dir_all(&peer)?;
            let peer = fs::canonicalize(peer)?;
            f.store.search_homes.push(peer.clone());
            Ok(peer)
        }
        _ => bail!("unknown synthetic compressed home"),
    }
}

fn compressed_child(home: &Path, top: &str, history_only: bool) -> Result<PathBuf> {
    let name = home.join(top).join("2026/10/21").join(format!(
        "rollout-2026-10-21T00-00-00-{CHILD_PHYSICAL}.jsonl.zst"
    ));
    fs::create_dir_all(name.parent().context("compressed child parent")?)?;
    let mut meta = serde_json::json!({
        "type":"session_meta","payload":{"id":CHILD,"model_provider":"openai"}
    });
    if history_only {
        meta["payload"]["history_base"] = serde_json::json!({
            "thread_id":PHYSICAL,"end_ordinal_exclusive":1,"end_byte_offset":1
        });
    } else {
        meta["payload"]["forked_from_id"] = ROOT.into();
    }
    let body = format!("{meta}\n").into_bytes();
    fs::write(&name, zstd::stream::encode_all(body.as_slice(), 1)?)?;
    assert_eq!(zstd::stream::decode_all(File::open(&name)?)?, body);
    let child = db::source_snapshot(home, &[CHILD.into()])?;
    assert!(
        child
            .threads
            .iter()
            .all(|row| row.thread.is_none() && row.goal.is_none())
    );
    Ok(name)
}

fn assert_compressed_doctor(f: &Fixture, name: &Path) -> Result<()> {
    let before = tree(f.root.path(), true)?;
    let diagnostics = super::super::doctor_impl::check(&f.store)?;
    assert!(
        !diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "압축_대화_관계_미확인"
                && diagnostic.id.is_none()
                && diagnostic.paths.iter().any(|path| path == name)
        }),
        "doctor must not classify readable compressed metadata as unknown"
    );
    assert_eq!(tree(f.root.path(), true)?, before);
    Ok(())
}

#[test]
fn compressed_rowless_descendants_refuse_planning_without_unknown_metadata_diagnostics()
-> Result<()> {
    for location in ["source", "target", "inventory"] {
        for top in ["sessions", "archived_sessions"] {
            for history_only in [false, true] {
                let mut f = Fixture::new()?;
                let home = compressed_test_home(&mut f, location)?;
                let child = compressed_child(&home, top, history_only)?;
                assert!(discovery::catalog(&home)?.compressed.contains(&child));
                let before = tree(f.root.path(), true)?;
                assert!(f.plan().is_err());
                assert_eq!(tree(f.root.path(), true)?, before);
                assert!(!path(&f.store, ROOT).exists());
                assert!(!f.store.target.join(".quota-baton/staging").exists());
                assert!(f.original.exists());
                no_rows(&f.store.target)?;
                assert_compressed_doctor(&f, &child)?;
            }
        }
    }
    Ok(())
}

#[test]
fn admission_check_refuses_a_rowless_compressed_child_in_the_wired_q_home() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(
        f.store.home.join("journal/wiring.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema":3,"mode":"dedicated","install_completed":true,
            "home":f.store.target
        }))?,
    )?;
    super::super::admission_check(&f.store.home, None)?;
    compressed_child(&f.store.target, "sessions", false)?;
    no_rows(&f.store.target)?;
    assert!(!path(&f.store, ROOT).exists());
    let before = tree(f.root.path(), true)?;
    assert!(super::super::admission_check(&f.store.home, None).is_ok());
    assert_eq!(tree(f.root.path(), true)?, before);
    Ok(())
}

#[test]
fn late_compressed_children_allow_source_rollback_but_block_postdecision_mutation() -> Result<()> {
    for location in ["source", "target", "inventory"] {
        for (label, nth, decided) in [
            ("staged", 2, false),
            ("source_rows_removed", 1, false),
            ("decided", 1, true),
            ("published", 1, true),
            ("target_rows", 1, true),
            ("map_committed", 1, true),
        ] {
            let mut f = Fixture::new()?;
            let home = compressed_test_home(&mut f, location)?;
            let original_rows = rows(&f.source)?;
            let witness = interrupt(&f, label, nth)?;
            let child = compressed_child(&home, "archived_sessions", true)?;
            let compressed = fs::read(&child)?;
            let identity = guard::identity(&child)?;
            if decided {
                let m = load(&f.store, ROOT)?.context("late compressed journal")?;
                verify_files(&m)?;
                assert_conflict_preserved(&f).with_context(|| {
                    format!("late compressed conflict (location={location}, checkpoint={label})")
                })?;
                no_rows(&f.source)?;
                assert!(
                    witness
                        .files
                        .iter()
                        .all(|file| file.names.iter().all(|name| !name.exists()))
                );
            } else {
                recover_twice(&f)?;
                assert_source(&f, &witness, &original_rows)?;
            }
            assert_eq!(fs::read(&child)?, compressed);
            assert_eq!(guard::identity(&child)?, identity);
            assert_compressed_doctor(&f, &child)?;
        }
    }
    Ok(())
}

#[test]
fn one_file_with_different_physical_uuid_aliases_and_later_active_row_refuses_without_writes()
-> Result<()> {
    let f = Fixture::new()?;
    let later = f
        .original
        .parent()
        .context("source rollout parent")?
        .join(format!("rollout-2026-10-21T00-00-00-{SECOND}.jsonl"));
    fs::hard_link(&f.original, &later)?;
    Connection::open(f.source.join("state_5.sqlite"))?.execute(
        "UPDATE threads SET rollout_path=?1 WHERE id=?2",
        params![later.to_str().context("later active path")?, ROOT],
    )?;
    assert_eq!(guard::identity(&f.original)?, guard::identity(&later)?);
    assert_eq!(guard::hardlink_count(&later)?, 2);
    let catalog = discovery::catalog(&f.source)?;
    assert_eq!(catalog.active(ROOT)?.path, later);
    let incoming = super::super::migration_files(&catalog, ROOT)?;
    assert_eq!(incoming.len(), 2);
    assert!(incoming.iter().any(|file| file.physical == PHYSICAL));
    assert!(incoming.iter().any(|file| file.physical == SECOND));
    let before = tree(f.root.path(), true)?;
    assert!(f.plan().is_err());
    assert_eq!(tree(f.root.path(), true)?, before);
    assert!(!path(&f.store, ROOT).exists());
    assert!(!f.store.target.join(".quota-baton/staging").exists());
    assert!(public_names(&f.store.target, ROOT)?.is_empty());
    no_rows(&f.store.target)?;
    Ok(())
}

#[test]
fn an_ancestor_alias_in_q_refuses_without_touching_the_existing_family() -> Result<()> {
    let f = Fixture::new()?;
    let alias = f
        .store
        .target
        .join("archived_sessions")
        .join(f.original.file_name().unwrap());
    fs::create_dir_all(alias.parent().unwrap())?;
    fs::hard_link(&f.original, &alias)?;
    let child = rollout(&f.store.target, CHILD, CHILD_PHYSICAL, None, Some(PHYSICAL))?;
    no_rows(&f.store.target)?;
    let before = tree(f.root.path(), true)?;
    let error = f.plan().err().context("ancestor alias was accepted")?;
    assert!(
        error
            .to_string()
            .contains("대상 qb 홈에 이 대화의 조상 별칭")
    );
    assert_eq!(tree(f.root.path(), true)?, before);
    assert_eq!(guard::identity(&alias)?, guard::identity(&f.original)?);
    assert!(child.exists() && !path(&f.store, ROOT).exists());
    Ok(())
}

#[test]
fn partial_or_divergent_paginated_inventory_in_another_home_is_refused_without_writes() -> Result<()>
{
    for physicals in [
        vec![PHYSICAL],
        vec![SECOND],
        vec![PHYSICAL, CHILD_PHYSICAL],
        vec![PHYSICAL, SECOND, CHILD_PHYSICAL],
    ] {
        let mut f = Fixture::new()?;
        let second = rollout(&f.source, ROOT, SECOND, None, Some(PHYSICAL))?;
        let peer = f.root.path().join("peer");
        fs::create_dir_all(peer.join("sessions"))?;
        let peer = fs::canonicalize(peer)?;
        for physical in physicals {
            let source = match physical {
                PHYSICAL => &f.original,
                SECOND => &second,
                _ => {
                    rollout(&peer, ROOT, physical, None, None)?;
                    continue;
                }
            };
            let copy = peer.join("sessions").join(source.file_name().unwrap());
            fs::copy(source, &copy)?;
            age(&copy)?;
        }
        // These are header-only copies; no database disagreement or unknown
        // hardlink can accidentally make this inventory test pass.
        no_rows(&peer)?;
        f.store.search_homes.push(peer.clone());
        let before = tree(f.root.path(), true)?;
        let error = f
            .plan()
            .err()
            .context("incomplete or divergent inventory was accepted")?;
        let conflict = error
            .downcast_ref::<super::super::InventoryConflict>()
            .context("physical inventory refusal must be typed")?;
        assert!(conflict.homes.contains(&f.source) && conflict.homes.contains(&peer));
        assert_eq!(tree(f.root.path(), true)?, before);
        assert_eq!(public_names(&f.source, ROOT)?.len(), 2);
        assert!(!public_names(&peer, ROOT)?.is_empty());
        assert!(public_names(&f.store.target, ROOT)?.is_empty());
        no_rows(&f.store.target)?;
        assert!(!path(&f.store, ROOT).exists());
    }
    Ok(())
}

#[test]
fn exact_paginated_hardlink_inventory_in_another_home_moves_every_public_name() -> Result<()> {
    let mut f = Fixture::new()?;
    let second = rollout(&f.source, ROOT, SECOND, None, Some(PHYSICAL))?;
    let peer = f.root.path().join("peer");
    fs::create_dir_all(peer.join("sessions"))?;
    let peer = fs::canonicalize(peer)?;
    let mut peer_names = Vec::new();
    for source in [&f.original, &second] {
        let alias = peer.join("sessions").join(source.file_name().unwrap());
        fs::hard_link(source, &alias)?;
        peer_names.push(alias);
    }
    f.store.search_homes.push(peer.clone());
    let m = f.plan()?;
    let witness = Witness::new(&m)?;
    assert_eq!(witness.files.len(), 2);
    run_plan(&f.store, m)?;
    assert_target(&f, &witness)?;
    assert!(peer_names.iter().all(|name| !name.exists()));
    assert!(public_names(&peer, ROOT)?.is_empty());
    assert!(public_names(&f.source, ROOT)?.is_empty());
    assert_eq!(public_names(&f.store.target, ROOT)?.len(), 2);
    for file in &witness.files {
        assert_eq!(guard::hardlink_count(file.target.as_ref().unwrap())?, 1);
    }
    no_rows(&peer)?;
    recover_twice(&f)?;
    Ok(())
}

#[test]
fn an_uninventoried_hardlink_refuses_and_preserves_every_name() -> Result<()> {
    let f = Fixture::new()?;
    let unknown = f.root.path().join("unmanaged-name.jsonl");
    fs::hard_link(&f.original, &unknown)?;
    assert_eq!(guard::hardlink_count(&f.original)?, 2);
    let before = tree(f.root.path(), true)?;
    assert!(f.plan().is_err());
    assert_eq!(tree(f.root.path(), true)?, before);
    assert_eq!(guard::identity(&unknown)?, guard::identity(&f.original)?);
    assert!(!path(&f.store, ROOT).exists());
    Ok(())
}

#[test]
fn all_catalogued_aliases_of_one_file_are_removed_not_just_one_uuid_entry() -> Result<()> {
    let mut f = Fixture::new()?;
    let peer = f.root.path().join("peer");
    fs::create_dir_all(&peer)?;
    let peer = fs::canonicalize(peer)?;
    let aliases = [
        f.source
            .join("sessions/alias")
            .join(f.original.file_name().unwrap()),
        f.source
            .join("archived_sessions")
            .join(f.original.file_name().unwrap()),
        peer.join("sessions").join(f.original.file_name().unwrap()),
    ];
    for alias in &aliases {
        fs::create_dir_all(alias.parent().unwrap())?;
        fs::hard_link(&f.original, alias)?;
    }
    schema(&peer)?;
    seed_rows(&peer, &aliases[2])?;
    f.store.search_homes.push(peer.clone());
    let m = f.plan()?;
    assert_eq!(m.files.len(), 1);
    assert_eq!(m.files[0].sources.len(), 4);
    let witness = Witness::new(&m)?;
    run_plan(&f.store, m)?;
    assert_target(&f, &witness)?;
    assert!(aliases.iter().all(|name| !name.exists()));
    assert!(public_names(&f.source, ROOT)?.is_empty());
    assert!(public_names(&peer, ROOT)?.is_empty());
    assert_eq!(public_names(&f.store.target, ROOT)?.len(), 1);
    no_rows(&peer)?;
    assert_eq!(
        guard::hardlink_count(witness.files[0].target.as_ref().unwrap())?,
        1
    );
    recover_twice(&f)?;
    Ok(())
}

#[test]
fn a_missing_source_goal_is_not_synthesized_or_restored_on_return() -> Result<()> {
    let f = Fixture::new()?;
    Connection::open(f.source.join("goals_1.sqlite"))?
        .execute("DELETE FROM thread_goals WHERE thread_id=?1", [ROOT])?;
    run_plan(&f.store, f.plan()?)?;
    assert!(rows(&f.store.target)?.goal.is_none());
    no_rows(&f.source)?;
    return_current(&f.store, ROOT, None, true, true)?;
    assert!(rows(&f.source)?.thread.is_some());
    assert!(rows(&f.source)?.goal.is_none());
    no_rows(&f.store.target)?;
    assert_eq!(public_names(&f.source, ROOT)?.len(), 1);
    assert!(public_names(&f.store.target, ROOT)?.is_empty());
    recover_twice(&f)?;
    Ok(())
}

#[test]
fn interrupted_rollback_preserves_an_originally_absent_source_thread_row() -> Result<()> {
    for goal_present in [false, true] {
        for label in ["staged", "source_names_removed", "source_rows_removed"] {
            let f = Fixture::new()?;
            // A rollout-only thread can coexist with an otherwise empty state
            // catalogue and an independently stored goal. Keep the empty source
            // schema usable for destination synthesis without inventing a row.
            fs::remove_file(f.source.join("state_5.sqlite"))?;
            Connection::open(f.source.join("state_5.sqlite"))?.execute_batch(
                "CREATE TABLE threads(
                    id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL,
                    model_provider TEXT NOT NULL, archived INTEGER DEFAULT 0,
                    title TEXT NOT NULL DEFAULT '');",
            )?;
            if !goal_present {
                Connection::open(f.source.join("goals_1.sqlite"))?
                    .execute("DELETE FROM thread_goals WHERE thread_id=?1", [ROOT])?;
            }
            let original = rows(&f.source)?;
            assert!(original.thread.is_none());
            assert_eq!(original.goal.is_some(), goal_present);
            let witness = interrupt(&f, label, if label == "staged" { 2 } else { 1 })?;
            recover_twice(&f)?;
            assert_source(&f, &witness, &original)?;
            assert!(
                rows(&f.source)?.thread.is_none(),
                "rollback synthesized a source row"
            );
            assert!(public_names(&f.store.target, ROOT)?.is_empty());
            no_rows(&f.store.target)?;
        }
    }
    Ok(())
}

#[test]
fn a_late_header_only_fork_after_unlink_restores_source_rows_and_names() -> Result<()> {
    for label in ["source_name_removed", "source_rows_removed"] {
        for history_only in [false, true] {
            let f = Fixture::new()?;
            let original = rows(&f.source)?;
            let m = f.plan()?;
            let witness = Witness::new(&m)?;
            let observed = witness.clone();
            let store = f.hook_store();
            let source = f.source.clone();
            let forked = Rc::new(Cell::new(false));
            let injected = forked.clone();
            let hook = HookGuard::install(move |current| {
                observed.check(&load(&store, ROOT)?.context("late fork record")?, current)?;
                if current == label && !injected.replace(true) {
                    assert!(public_names(&source, ROOT)?.is_empty());
                    rollout(
                        &source,
                        CHILD,
                        CHILD_PHYSICAL,
                        (!history_only).then_some(ROOT),
                        history_only.then_some(PHYSICAL),
                    )?;
                    rollout(&source, GRANDCHILD, GRANDCHILD_PHYSICAL, Some(CHILD), None)?;
                }
                Ok(())
            });
            assert!(run_plan(&f.store, m).is_err());
            drop(hook);
            assert!(forked.get());
            assert_source(&f, &witness, &original)?;
            assert_eq!(public_names(&f.source, CHILD)?.len(), 1);
            assert_eq!(public_names(&f.source, GRANDCHILD)?.len(), 1);
            let descendants = discovery::catalog(&f.source)?.descendants(ROOT);
            assert!(descendants.contains(CHILD) && descendants.contains(GRANDCHILD));
            recover_twice(&f)?;
        }
    }
    Ok(())
}

fn append_bytes(name: &Path, bytes: &[u8]) -> Result<()> {
    OpenOptions::new()
        .append(true)
        .open(name)?
        .write_all(bytes)?;
    Ok(())
}

/// Decode only this fixture's DPAPI envelope, and check the actual observations,
/// not merely that a file with an evidence-like name was created.
fn assert_evidence(f: &Fixture, before: &Move) -> Result<()> {
    let dir = f.store.home.join("journal/session-moves-evidence");
    let mut entries = fs::read_dir(dir)
        .context("conflict recovery did not publish encrypted recovery evidence")?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    let bytes = fs::read(entries.last().context("missing recovery evidence")?)?;
    let envelope: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert_eq!(envelope["schema"], 1);
    assert!(envelope.get("observed").is_none() && envelope.get("rows").is_none());
    assert!(!String::from_utf8_lossy(&bytes).contains("SYNTHETIC"));
    let observed: serde_json::Value = serde_json::from_slice(&crate::cutover::unprotect(
        envelope["payload"]
            .as_str()
            .context("encrypted evidence payload")?,
    )?)?;
    assert_eq!(observed["journal"], serde_json::to_value(before)?);
    let files = observed["observed"].as_array().context("observed names")?;
    let names = before.files.iter().flat_map(|file| {
        file.sources
            .iter()
            .chain(std::iter::once(&file.stage))
            .chain(file.target.iter())
    });
    let mut count = 0;
    for name in names {
        count += 1;
        let entry = files
            .iter()
            .find(|entry| entry["path"] == serde_json::json!(name))
            .context("name missing from evidence")?;
        assert_eq!(entry["exists"], name.exists());
        if name.exists() {
            assert_eq!(
                entry["identity"],
                serde_json::to_value(guard::identity(name)?)?
            );
            assert_eq!(entry["bytes"], fs::metadata(name)?.len());
        }
    }
    assert_eq!(files.len(), count);
    let captured: Vec<db::DbPlan> = serde_json::from_value(observed["rows"].clone())?;
    assert_eq!(captured.len(), before.homes.len());
    for home in &before.homes {
        let captured = captured
            .iter()
            .find(|plan| &plan.target_home == home)
            .context("home rows missing from evidence")?;
        let current = db::source_snapshot(home, &[ROOT.into()])?;
        assert!(
            captured.threads == current.threads,
            "evidence must retain current rows"
        );
    }
    Ok(())
}

fn assert_conflict_preserved(f: &Fixture) -> Result<()> {
    let m = load(&f.store, ROOT)?.context("conflict journal")?;
    assert!(open(&m));
    let journal = fs::read(path(&f.store, ROOT))?;
    let before = tree(f.root.path(), false)?;
    for _ in 0..2 {
        let error = recover(&f.store, ROOT).expect_err("conflict must not converge");
        let current = load(&f.store, ROOT)?.context("open conflict journal")?;
        assert!(open(&current) && current.phase == m.phase);
        assert!(pending(&f.store)?);
        assert_eq!(fs::read(path(&f.store, ROOT))?, journal);
        assert_eq!(
            tree(f.root.path(), false)?,
            before,
            "conflict changed live evidence"
        );
        assert_evidence(f, &m)
            .with_context(|| format!("evidence missing after recovery error: {error:#}"))?;
    }
    Ok(())
}

#[test]
fn missing_acknowledged_target_thread_or_goal_is_a_nondestructive_conflict() -> Result<()> {
    for goal in [false, true] {
        let f = Fixture::new()?;
        let witness = interrupt(&f, "target_rows", 1)?;
        if goal {
            Connection::open(f.store.target.join("goals_1.sqlite"))?
                .execute("DELETE FROM thread_goals WHERE thread_id=?1", [ROOT])?;
        } else {
            Connection::open(f.store.target.join("state_5.sqlite"))?
                .execute("DELETE FROM threads WHERE id=?1", [ROOT])?;
        }
        assert_conflict_preserved(&f)?;
        no_rows(&f.source)?;
        assert!(witness.files[0].target.as_ref().unwrap().exists());
        assert!(!f.original.exists());
    }
    Ok(())
}

#[test]
fn unexpected_target_rows_before_decision_block_rollback_without_restoring_source() -> Result<()> {
    for goal in [false, true] {
        let f = Fixture::new()?;
        let witness = interrupt(&f, "source_rows_removed", 1)?;
        schema(&f.store.target)?;
        seed_rows(&f.store.target, witness.files[0].target.as_ref().unwrap())?;
        if goal {
            Connection::open(f.store.target.join("state_5.sqlite"))?
                .execute("DELETE FROM threads WHERE id=?1", [ROOT])?;
        } else {
            Connection::open(f.store.target.join("goals_1.sqlite"))?
                .execute("DELETE FROM thread_goals WHERE thread_id=?1", [ROOT])?;
        }
        assert_conflict_preserved(&f)?;
        no_rows(&f.source)?;
        assert!(!f.original.exists());
        assert!(witness.files[0].stage.exists());
        assert!(public_names(&f.store.target, ROOT)?.is_empty());
    }
    Ok(())
}

fn assert_native_source_append_with_destination_conflict(goal_only: bool) -> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir_all(f.store.map_path.parent().context("synthetic map parent")?)?;
    let mut map = serde_json::json!({"sessions":{},"synthetic_extra":"preserve"});
    map["sessions"][ROOT] = serde_json::to_value(&f.source)?;
    map["sessions"][CHILD] = serde_json::to_value(&f.store.target)?;
    fs::write(&f.store.map_path, serde_json::to_vec_pretty(&map)?)?;
    let map_bytes = fs::read(&f.store.map_path)?;
    let map_identity = guard::identity(&f.store.map_path)?;

    let original_rows = rows(&f.source)?;
    let witness = interrupt(&f, "staged", 2)?;
    let m = load(&f.store, ROOT)?.context("staged source append journal")?;
    assert!(m.phase == Phase::Staged && open(&m));
    let file = &witness.files[0];
    assert!(file.stage.exists() && f.original.exists());
    append_bytes(&f.original, APPEND)?;
    Connection::open(f.source.join("state_5.sqlite"))?.execute(
        "UPDATE threads SET updated_at=updated_at+1 WHERE id=?1",
        [ROOT],
    )?;
    let old_update = match original_rows
        .thread
        .as_ref()
        .and_then(|row| row.get("updated_at"))
    {
        Some(db::DbValue::Integer(value)) => *value,
        _ => bail!("synthetic source update time must be an integer"),
    };
    let mut current_source = original_rows;
    current_source
        .thread
        .as_mut()
        .context("source thread")?
        .insert("updated_at".into(), db::DbValue::Integer(old_update + 1));
    assert!(
        rows(&f.source)? == current_source,
        "source stable fields or goal changed"
    );
    let body = fs::read(&f.original)?;
    assert!(body.starts_with(&file.body) && body.ends_with(APPEND));
    assert_eq!(fs::read(&file.stage)?, body);
    assert_eq!(guard::hardlink_count(&f.original)?, 2);

    schema(&f.store.target)?;
    seed_rows(
        &f.store.target,
        file.target.as_ref().context("intended target")?,
    )?;
    if goal_only {
        Connection::open(f.store.target.join("state_5.sqlite"))?
            .execute("DELETE FROM threads WHERE id=?1", [ROOT])?;
    } else {
        Connection::open(f.store.target.join("goals_1.sqlite"))?
            .execute("DELETE FROM thread_goals WHERE thread_id=?1", [ROOT])?;
    }
    let destination_rows = rows(&f.store.target)?;
    assert_eq!(destination_rows.thread.is_some(), !goal_only);
    assert_eq!(destination_rows.goal.is_some(), goal_only);
    assert!(public_names(&f.store.target, ROOT)?.is_empty());

    // The source append is otherwise eligible for the narrow native-resume
    // exception. Unexpected destination ownership must still prevent cleanup.
    assert_conflict_preserved(&f)?;
    assert!(rows(&f.source)? == current_source);
    assert!(rows(&f.store.target)? == destination_rows);
    for name in [&f.original, &file.stage] {
        assert_eq!(fs::read(name)?, body);
        assert_eq!(guard::identity(name)?, file.identity);
    }
    assert_eq!(guard::hardlink_count(&f.original)?, 2);
    assert!(public_names(&f.store.target, ROOT)?.is_empty());
    assert_eq!(fs::read(&f.store.map_path)?, map_bytes);
    assert_eq!(guard::identity(&f.store.map_path)?, map_identity);
    Ok(())
}

#[test]
fn native_source_append_with_unexpected_target_thread_preserves_open_conflict() -> Result<()> {
    assert_native_source_append_with_destination_conflict(false)
}

#[test]
fn native_source_append_with_unexpected_target_goal_only_preserves_open_conflict() -> Result<()> {
    assert_native_source_append_with_destination_conflict(true)
}

#[test]
fn unrecorded_append_without_row_advance_never_claims_recovery_success() -> Result<()> {
    for (label, nth) in [
        ("prepared", 1),
        ("staged", 1),
        ("staged", 2),
        ("source_name_removed", 1),
        ("source_names_removed", 1),
        ("source_rows_removed", 1),
        ("decided", 1),
        ("published", 1),
        ("target_rows", 1),
        ("map_committed", 1),
    ] {
        let f = Fixture::new()?;
        let witness = interrupt(&f, label, nth)?;
        let file = &witness.files[0];
        let name = if file.stage.exists() {
            &file.stage
        } else if f.original.exists() {
            &f.original
        } else {
            file.target.as_ref().unwrap()
        };
        append_bytes(name, APPEND)?;
        let appended = fs::read(name)?;
        assert!(appended.starts_with(&file.body) && appended.ends_with(APPEND));
        if load(&f.store, ROOT)?.context("interrupted move")?.phase < Phase::Decided
            && file.names.iter().any(|n| n.exists())
        {
            recover(&f.store, ROOT)?;
            let recovered = load(&f.store, ROOT)?.context("closed source-kept journal")?;
            assert!(recovered.phase == Phase::AbortedSourceKept && !open(&recovered));
            assert_eq!(fs::read(&f.original)?, appended);
        } else {
            assert_conflict_preserved(&f)?;
            assert_eq!(fs::read(name)?, appended);
        }
    }
    Ok(())
}

#[test]
fn native_source_append_and_only_allowed_integer_changes_abort_without_old_row_replay() -> Result<()>
{
    for (label, nth, field) in [
        ("prepared", 1, "updated_at"),
        ("staged", 1, "updated_at"),
        ("staged", 2, "created_at"),
        ("staged", 2, "updated_at"),
        ("staged", 2, "tokens_used"),
        ("staged", 2, "created_at_ms"),
        ("staged", 2, "updated_at_ms"),
        ("staged", 2, "recency_at"),
        ("staged", 2, "recency_at_ms"),
    ] {
        let f = Fixture::new()?;
        let alias = f
            .source
            .join("archived_sessions")
            .join(f.original.file_name().unwrap());
        fs::create_dir_all(alias.parent().unwrap())?;
        fs::hard_link(&f.original, &alias)?;
        let old_rows = rows(&f.source)?;
        let witness = interrupt(&f, label, nth)?;
        append_bytes(&f.original, APPEND)?;
        Connection::open(f.source.join("state_5.sqlite"))?.execute(
            &format!("UPDATE threads SET {field}={field}+1 WHERE id=?1"),
            [ROOT],
        )?;
        let current = rows(&f.source)?;
        assert!(current != old_rows && current.goal == old_rows.goal);
        let appended = fs::read(&f.original)?;
        recover_twice(&f)?;
        let m = load(&f.store, ROOT)?.context("source append abort")?;
        assert!(m.phase == Phase::AbortedSourceKept && !open(&m));
        assert!(!pending(&f.store)?);
        assert_eq!(owner(&f.store, ROOT)?, Some(f.source.clone()));
        assert!(
            rows(&f.source)? == current,
            "current operational row was replayed away"
        );
        for name in [&f.original, &alias] {
            assert_eq!(fs::read(name)?, appended);
            assert_eq!(guard::identity(name)?, witness.files[0].identity);
        }
        assert_eq!(guard::hardlink_count(&f.original)?, 2);
        assert!(witness.files.iter().all(|file| !file.stage.exists()));
        assert!(public_names(&f.store.target, ROOT)?.is_empty());
        no_rows(&f.store.target)?;
        assert!(!f.store.map_path.exists());
    }
    Ok(())
}

#[test]
fn source_append_keeps_current_rows_but_refuses_prefix_identity_or_untracked_links() -> Result<()> {
    for change in [
        "title",
        "goal",
        "goal_clear",
        "integer_type",
        "prefix",
        "tail",
        "link",
        "identity",
    ] {
        let f = Fixture::new()?;
        let witness = interrupt(&f, "staged", 2)?;
        append_bytes(
            &f.original,
            if change == "tail" {
                b"{\"incomplete\":"
            } else {
                APPEND
            },
        )?;
        let state = Connection::open(f.source.join("state_5.sqlite"))?;
        state.execute(
            "UPDATE threads SET updated_at=updated_at+1 WHERE id=?1",
            [ROOT],
        )?;
        match change {
            "title" => {
                state.execute(
                    "UPDATE threads SET title='SYNTHETIC changed title' WHERE id=?1",
                    [ROOT],
                )?;
            }
            "goal" => {
                Connection::open(f.source.join("goals_1.sqlite"))?.execute(
                    "UPDATE thread_goals SET objective='SYNTHETIC changed goal' WHERE thread_id=?1",
                    [ROOT],
                )?;
            }
            "goal_clear" => {
                Connection::open(f.source.join("goals_1.sqlite"))?
                    .execute("DELETE FROM thread_goals WHERE thread_id=?1", [ROOT])?;
            }
            "integer_type" => {
                state.execute(
                    "UPDATE threads SET tokens_used='SYNTHETIC noninteger' WHERE id=?1",
                    [ROOT],
                )?;
            }
            "prefix" => {
                let changed = String::from_utf8(fs::read(&f.original)?)?
                    .replace("SYNTHETIC root body", "SYNTHETIC changed body");
                fs::write(&f.original, changed)?;
            }
            "link" => {
                fs::hard_link(&f.original, f.root.path().join("unrecorded-alias"))?;
            }
            "identity" => {
                let bytes = fs::read(&f.original)?;
                fs::remove_file(&f.original)?;
                fs::write(&f.original, bytes)?;
                assert_ne!(guard::identity(&f.original)?, witness.files[0].identity);
            }
            "tail" => {}
            _ => unreachable!(),
        }
        drop(state);
        let current_rows = rows(&f.source)?;
        let current_body = fs::read(&f.original)?;
        if matches!(change, "prefix" | "link" | "identity") {
            assert_conflict_preserved(&f)?;
            assert!(witness.files[0].stage.exists());
        } else {
            recover_twice(&f)?;
            let m = load(&f.store, ROOT)?.context("source kept journal")?;
            assert!(m.phase == Phase::AbortedSourceKept && !open(&m));
            assert!(!witness.files[0].stage.exists());
            assert!(
                f.store
                    .home
                    .join("journal/session-moves-evidence")
                    .join(format!("{ROOT}-aborted-source-kept.json"))
                    .exists()
            );
        }
        assert!(rows(&f.source)? == current_rows);
        assert_eq!(fs::read(&f.original)?, current_body);
        assert!(f.original.exists());
        assert!(public_names(&f.store.target, ROOT)?.is_empty());
    }
    Ok(())
}

fn advance_target(f: &Fixture, m: &Move) -> Result<()> {
    // At the first published hook the rollout exists before metadata replay.
    // Materialize exactly the projected rows, then simulate a native resume.
    if rows(&f.store.target)?.thread.is_none() {
        db::apply(&m.database)?;
    }
    Connection::open(f.store.target.join("state_5.sqlite"))?.execute(
        "UPDATE threads SET updated_at=updated_at+1,updated_at_ms=updated_at_ms+1000,
            tokens_used=tokens_used+5 WHERE id=?1",
        [ROOT],
    )?;
    Ok(())
}

#[test]
fn fully_published_unrecorded_target_append_is_a_preserved_conflict() -> Result<()> {
    for label in ["published", "target_rows", "map_committed"] {
        let f = Fixture::new()?;
        let witness = interrupt(&f, label, 1)?;
        let old = load(&f.store, ROOT)?.context("pre-adoption journal")?;
        if label == "published" {
            assert!(old.phase == Phase::Decided);
        }
        let target = witness.files[0].target.as_ref().unwrap();
        advance_target(&f, &old)?;
        let current = rows(&f.store.target)?;
        append_bytes(target, APPEND)?;
        let body = fs::read(target)?;
        assert!(body.starts_with(&witness.files[0].body) && body.ends_with(APPEND));
        assert_conflict_preserved(&f)?;
        let journal = load(&f.store, ROOT)?.context("postdecision conflict journal")?;
        assert!(open(&journal) && journal.phase == old.phase);
        assert!(rows(&f.store.target)? == current);
        assert_eq!(fs::read(target)?, body);
        assert_eq!(guard::identity(target)?, witness.files[0].identity);
        assert_eq!(journal.files[0].bytes, old.files[0].bytes);
        no_rows(&f.source)?;
        assert!(public_names(&f.source, ROOT)?.is_empty());
        assert_evidence(&f, &old)?;
    }
    Ok(())
}

#[test]
fn eligible_target_append_with_a_late_compressed_child_preserves_the_original_open_journal()
-> Result<()> {
    for location in ["source", "target", "inventory"] {
        let mut f = Fixture::new()?;
        let home = compressed_test_home(&mut f, location)?;
        let witness = interrupt(&f, "published", 1)?;
        let original = load(&f.store, ROOT)?.context("pre-adoption journal")?;
        assert!(original.phase == Phase::Decided);
        let file = &witness.files[0];
        let target = file.target.as_ref().context("published target")?;
        advance_target(&f, &original)?;
        let current = rows(&f.store.target)?;
        append_bytes(target, APPEND)?;
        let body = fs::read(target)?;
        assert!(body.starts_with(&file.body) && body.ends_with(APPEND));
        assert_eq!(guard::identity(target)?, file.identity);
        assert_eq!(guard::hardlink_count(target)?, 1);
        assert!(file.names.iter().all(|name| !name.exists()));
        no_rows(&f.source)?;
        let child = compressed_child(&home, "archived_sessions", true)?;
        let compressed = fs::read(&child)?;

        // Adoption must refuse before rewriting the old file/row baseline,
        // even though the published append is otherwise native-compatible.
        assert_conflict_preserved(&f)?;
        let preserved = load(&f.store, ROOT)?.context("preserved pre-adoption journal")?;
        assert_eq!(preserved.files[0].bytes, original.files[0].bytes);
        assert_eq!(preserved.files[0].sha256, original.files[0].sha256);
        assert!(preserved.database == original.database);
        assert!(rows(&f.store.target)? == current);
        assert_eq!(fs::read(target)?, body);
        assert_eq!(fs::read(&child)?, compressed);
        mapping::verify(&original.map)?;
        no_rows(&f.source)?;
    }
    Ok(())
}

#[test]
fn native_target_append_after_state_only_commit_preserves_missing_goal_as_open_conflict()
-> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir_all(f.store.map_path.parent().context("synthetic map parent")?)?;
    let mut map = serde_json::json!({"sessions":{},"synthetic_extra":"preserve"});
    map["sessions"][ROOT] = serde_json::to_value(&f.source)?;
    map["sessions"][CHILD] = serde_json::to_value(&f.store.target)?;
    fs::write(&f.store.map_path, serde_json::to_vec_pretty(&map)?)?;

    let witness = interrupt(&f, "published", 1)?;
    let map_bytes = fs::read(&f.store.map_path)?;
    let map_identity = guard::identity(&f.store.map_path)?;

    let m = load(&f.store, ROOT)?.context("published state-only journal")?;
    assert!(m.phase == Phase::Decided && open(&m));
    let file = &witness.files[0];
    let target = file.target.as_ref().context("published target")?;
    db::apply(&m.database)?;
    let projected = rows(&f.store.target)?;
    assert!(projected.thread.is_some() && projected.goal.is_some());
    Connection::open(f.store.target.join("goals_1.sqlite"))?
        .execute("DELETE FROM thread_goals WHERE thread_id=?1", [ROOT])?;
    Connection::open(f.store.target.join("state_5.sqlite"))?.execute(
        "UPDATE threads SET updated_at=updated_at+1 WHERE id=?1",
        [ROOT],
    )?;
    append_bytes(target, APPEND)?;
    let old_update = match projected
        .thread
        .as_ref()
        .and_then(|row| row.get("updated_at"))
    {
        Some(db::DbValue::Integer(value)) => *value,
        _ => bail!("synthetic target update time must be an integer"),
    };
    let mut current = projected;
    current.goal = None;
    current
        .thread
        .as_mut()
        .context("target thread")?
        .insert("updated_at".into(), db::DbValue::Integer(old_update + 1));
    assert!(rows(&f.store.target)? == current);
    let body = fs::read(target)?;
    assert!(body.starts_with(&file.body) && body.ends_with(APPEND));
    no_rows(&f.source)?;

    // A native append between the independent state/goal commits is an explicit
    // availability limit: preserve current data rather than infer a missing goal.
    assert_conflict_preserved(&f)?;
    assert!(rows(&f.store.target)? == current);
    assert!(rows(&f.store.target)?.goal.is_none());
    assert_eq!(fs::read(target)?, body);
    assert_eq!(guard::identity(target)?, file.identity);
    assert_eq!(guard::hardlink_count(target)?, 1);
    assert!(file.names.iter().all(|name| !name.exists()));
    assert!(!file.stage.exists());
    no_rows(&f.source)?;
    assert_eq!(fs::read(&f.store.map_path)?, map_bytes);
    assert_eq!(guard::identity(&f.store.map_path)?, map_identity);
    Ok(())
}

#[test]
fn partial_publication_cannot_adopt_an_append_even_with_advanced_target_rows() -> Result<()> {
    let f = Fixture::new()?;
    rollout(&f.source, ROOT, SECOND, None, Some(PHYSICAL))?;
    let witness = interrupt(&f, "published", 1)?;
    let old = load(&f.store, ROOT)?.context("partial publication journal")?;
    let published = witness
        .files
        .iter()
        .filter_map(|file| file.target.as_ref())
        .filter(|name| name.exists())
        .collect::<Vec<_>>();
    assert_eq!(published.len(), 1);
    assert!(witness.files.iter().any(|file| file.stage.exists()));
    advance_target(&f, &old)?;
    append_bytes(published[0], APPEND)?;
    assert_conflict_preserved(&f)?;
    assert_eq!(public_names(&f.store.target, ROOT)?.len(), 1);
    no_rows(&f.source)?;
    assert!(public_names(&f.source, ROOT)?.is_empty());
    Ok(())
}

#[test]
fn target_append_requires_unchanged_goal_stable_fields_prefix_identity_and_known_links()
-> Result<()> {
    for change in [
        "no_advance",
        "title",
        "goal",
        "prefix",
        "tail",
        "link",
        "identity",
    ] {
        let f = Fixture::new()?;
        let witness = interrupt(&f, "published", 1)?;
        let m = load(&f.store, ROOT)?.context("published journal")?;
        let target = witness.files[0].target.as_ref().unwrap();
        db::apply(&m.database)?;
        if change != "no_advance" {
            advance_target(&f, &m)?;
        }
        append_bytes(
            target,
            if change == "tail" {
                b"{\"incomplete\":"
            } else {
                APPEND
            },
        )?;
        match change {
            "title" => {
                Connection::open(f.store.target.join("state_5.sqlite"))?.execute(
                    "UPDATE threads SET title='SYNTHETIC changed title' WHERE id=?1",
                    [ROOT],
                )?;
            }
            "goal" => {
                Connection::open(f.store.target.join("goals_1.sqlite"))?.execute(
                    "UPDATE thread_goals SET tokens_used=tokens_used+1 WHERE thread_id=?1",
                    [ROOT],
                )?;
            }
            "prefix" => {
                let changed = String::from_utf8(fs::read(target)?)?
                    .replace("SYNTHETIC root body", "SYNTHETIC changed body");
                fs::write(target, changed)?;
            }
            "link" => {
                fs::hard_link(target, f.root.path().join("unrecorded-target-alias"))?;
            }
            "identity" => {
                let bytes = fs::read(target)?;
                fs::remove_file(target)?;
                fs::write(target, bytes)?;
                assert_ne!(guard::identity(target)?, witness.files[0].identity);
            }
            "no_advance" | "tail" => {}
            _ => unreachable!(),
        }
        assert_conflict_preserved(&f)?;
        no_rows(&f.source)?;
        assert!(public_names(&f.source, ROOT)?.is_empty());
        assert!(target.exists());
    }
    Ok(())
}

#[test]
fn target_append_cannot_adopt_if_a_source_name_or_source_rows_reappear() -> Result<()> {
    for source_rows in [false, true] {
        let f = Fixture::new()?;
        let witness = interrupt(&f, "published", 1)?;
        let m = load(&f.store, ROOT)?.context("published journal")?;
        let target = witness.files[0].target.as_ref().unwrap();
        advance_target(&f, &m)?;
        append_bytes(target, APPEND)?;
        if source_rows {
            seed_rows(&f.source, &f.original)
                .context("reintroducing synthetic source SQLite rows")?;
        } else {
            assert!(
                f.original
                    .parent()
                    .context("source rollout parent")?
                    .is_dir(),
                "source rollout parent disappeared before fixture setup"
            );
            fs::hard_link(target, &f.original)
                .context("reintroducing synthetic source filename")?;
        }
        assert_conflict_preserved(&f)
            .with_context(|| format!("source reappearance conflict (rows={source_rows})"))?;
        assert_eq!(fs::read(target)?.last(), Some(&b'\n'));
    }
    Ok(())
}

#[test]
fn unchanged_source_hardlink_reappearance_after_decision_is_a_nondestructive_conflict() -> Result<()>
{
    for label in ["decided", "published", "target_rows", "map_committed"] {
        let f = Fixture::new()?;
        let witness = interrupt(&f, label, 1)?;
        let m = load(&f.store, ROOT)?.context("decided journal")?;
        let file = &witness.files[0];
        assert!(!f.original.exists());
        no_rows(&f.source)?;
        let surviving = if file.stage.exists() {
            &file.stage
        } else {
            file.target.as_ref().unwrap()
        };
        fs::create_dir_all(f.original.parent().context("source rollout parent")?)?;
        fs::hard_link(surviving, &f.original)?;
        assert_eq!(fs::read(&f.original)?, file.body);
        assert_eq!(guard::identity(&f.original)?, file.identity);
        assert_eq!(guard::hardlink_count(&f.original)?, 2);
        no_rows(&f.source)?;
        // This specifically exercises recovery after file verification succeeds,
        // rather than the separate append-adoption/conflict path.
        verify_files(&m)?;
        assert_conflict_preserved(&f)?;
        assert!(f.original.exists());
        assert_eq!(fs::read(&f.original)?, file.body);
        no_rows(&f.source)?;
    }
    Ok(())
}

#[test]
fn source_abort_preserves_decreasing_integers_while_target_append_conflicts() -> Result<()> {
    for target in [false, true] {
        for field in ["updated_at", "tokens_used"] {
            let f = Fixture::new()?;
            let witness = interrupt(
                &f,
                if target { "published" } else { "staged" },
                if target { 1 } else { 2 },
            )?;
            let m = load(&f.store, ROOT)?.context("append journal")?;
            let home = if target { &f.store.target } else { &f.source };
            let name = if target {
                witness.files[0].target.as_ref().unwrap()
            } else {
                &f.original
            };
            if target {
                db::apply(&m.database)?;
            }
            let original = rows(home)?;
            let old = match original.thread.as_ref().and_then(|row| row.get(field)) {
                Some(db::DbValue::Integer(value)) => *value,
                _ => bail!("synthetic operational field must start as an integer"),
            };
            Connection::open(home.join("state_5.sqlite"))?.execute(
                &format!("UPDATE threads SET {field}={field}-1 WHERE id=?1"),
                [ROOT],
            )?;
            append_bytes(name, APPEND)?;
            let mut expected = original.clone();
            expected
                .thread
                .as_mut()
                .context("synthetic thread row")?
                .insert(field.into(), db::DbValue::Integer(old - 1));
            assert!(
                rows(home)? == expected,
                "stable fields or goal changed during setup"
            );
            assert!(fs::read(name)?.starts_with(&witness.files[0].body));
            if target {
                assert_conflict_preserved(&f)
                    .with_context(|| format!("decreasing {field} target append"))?;
            } else {
                recover_twice(&f)?;
                assert!(
                    load(&f.store, ROOT)?.context("source abort")?.phase
                        == Phase::AbortedSourceKept
                );
            }
            assert!(rows(home)? == expected, "conflict replaced current rows");
        }
    }
    Ok(())
}

fn change_current_q_state(f: &Fixture) -> Result<Rows> {
    Connection::open(f.store.target.join("state_5.sqlite"))?.execute(
        "UPDATE threads SET title='SYNTHETIC current title',archived=1,
            updated_at=updated_at+10,tokens_used=91 WHERE id=?1",
        [ROOT],
    )?;
    Connection::open(f.store.target.join("goals_1.sqlite"))?.execute(
        "UPDATE thread_goals SET objective='SYNTHETIC current objective',status='complete',
            created_at_ms=123,updated_at_ms=987654,tokens_used=81,token_budget=555
            WHERE thread_id=?1",
        [ROOT],
    )?;
    append_index(&f.store.target, "SYNTHETIC current title")?;
    let name = db::thread_path(&f.store.target, ROOT)?.context("current Q path")?;
    append_bytes(&name, APPEND)?;
    age(&name)?;
    rows(&f.store.target)
}

#[test]
fn return_and_undo_move_current_goal_thread_and_index_instead_of_original_snapshot() -> Result<()> {
    // None is the same origin-selection path used by the staged Unimport arm.
    for explicit_destination in [false, true] {
        let f = Fixture::new()?;
        let original = rows(&f.source)?;
        run_plan(&f.store, f.plan()?)?;
        let current = change_current_q_state(&f)?;
        assert!(current.goal != original.goal);
        let q_name = db::thread_path(&f.store.target, ROOT)?.context("Q path")?;
        let identity = guard::identity(&q_name)?;
        let body = fs::read(&q_name)?;
        let store = f.hook_store();
        let hook = HookGuard::install(move |label| {
            if label == "prepared" {
                let m = load(&store, ROOT)?.context("prepared return")?;
                assert!(m.files.iter().all(|file| {
                    file.stage
                        .starts_with(store.target.join(".quota-baton/staging"))
                }));
                assert!(public_names(&m.target, ROOT)?.is_empty());
            }
            Ok(())
        });
        return_current(
            &f.store,
            ROOT,
            explicit_destination.then_some(f.source.as_path()),
            true,
            true,
        )?;
        drop(hook);
        let after = rows(&f.source)?;
        assert!(
            after.goal == current.goal,
            "return restored an obsolete goal snapshot"
        );
        let mut expected = current.thread.context("current Q thread")?;
        let returned = db::thread_path(&f.source, ROOT)?.context("returned path")?;
        expected.insert(
            "rollout_path".into(),
            text(returned.to_str().context("returned path text")?),
        );
        assert!(after.thread == Some(expected));
        assert_eq!(guard::identity(&returned)?, identity);
        assert_eq!(fs::read(&returned)?, body);
        no_rows(&f.store.target)?;
        assert!(public_names(&f.store.target, ROOT)?.is_empty());
        assert_eq!(public_names(&f.source, ROOT)?.len(), 1);
        assert_eq!(owner(&f.store, ROOT)?, Some(f.source.clone()));
        let index = fs::read_to_string(f.source.join("session_index.jsonl"))?;
        let last = index
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<serde_json::Result<Vec<_>>>()?
            .into_iter()
            .rfind(|entry| entry["id"] == ROOT)
            .context("current index entry")?;
        assert_eq!(last["thread_name"], "SYNTHETIC current title");
        assert_eq!(last["synthetic_extra"], "preserved");
        let map: serde_json::Value = serde_json::from_slice(&fs::read(&f.store.map_path)?)?;
        assert_eq!(Path::new(map["sessions"][ROOT].as_str().unwrap()), f.source);
        recover_twice(&f)?;
        let closed = tree(f.root.path(), true)?;
        return_current(&f.store, ROOT, None, true, true)?;
        assert_eq!(tree(f.root.path(), true)?, closed);
    }
    Ok(())
}

#[test]
fn return_and_undo_refuse_header_only_children_without_changing_current_q_state() -> Result<()> {
    for explicit_destination in [false, true] {
        let f = Fixture::new()?;
        run_plan(&f.store, f.plan()?)?;
        let current = change_current_q_state(&f)?;
        rollout(&f.store.target, CHILD, CHILD_PHYSICAL, None, Some(PHYSICAL))?;
        rollout(
            &f.store.target,
            GRANDCHILD,
            GRANDCHILD_PHYSICAL,
            Some(CHILD),
            None,
        )?;
        let before = tree(f.root.path(), true)?;
        assert!(
            return_current(
                &f.store,
                ROOT,
                explicit_destination.then_some(f.source.as_path()),
                true,
                true,
            )
            .is_err()
        );
        assert_eq!(tree(f.root.path(), true)?, before);
        assert!(rows(&f.store.target)? == current);
        no_rows(&f.source)?;
        assert_eq!(public_names(&f.store.target, ROOT)?.len(), 1);
        assert!(public_names(&f.source, ROOT)?.is_empty());
        assert_eq!(public_names(&f.store.target, CHILD)?.len(), 1);
        assert_eq!(public_names(&f.store.target, GRANDCHILD)?.len(), 1);
    }
    Ok(())
}

#[test]
fn return_and_redo_keep_prior_encrypted_journals_and_private_duplicate_evidence_reachable()
-> Result<()> {
    let mut f = Fixture::new()?;
    let peer = f.root.path().join("peer");
    fs::create_dir_all(&peer)?;
    let peer = fs::canonicalize(peer)?;
    let copy = peer.join("sessions").join(f.original.file_name().unwrap());
    fs::create_dir_all(copy.parent().unwrap())?;
    fs::copy(&f.original, &copy)?;
    age(&copy)?;
    assert_ne!(guard::identity(&copy)?, guard::identity(&f.original)?);
    schema(&peer)?;
    seed_rows(&peer, &copy)?;
    f.store.search_homes.push(peer.clone());
    let first = f.plan()?;
    let private = first
        .files
        .iter()
        .find(|file| file.target.is_none())
        .context("private duplicate segment")?;
    let retained = private.stage.clone();
    let identity = private.identity;
    let body = fs::read(&copy)?;
    run_plan(&f.store, first)?;
    assert!(!copy.exists());
    no_rows(&peer)?;
    let initial_journal = fs::read(path(&f.store, ROOT))?;
    return_current(&f.store, ROOT, None, true, true)?;
    let returned_journal = fs::read(path(&f.store, ROOT))?;
    run_plan(&f.store, f.plan()?)?;
    let latest = load(&f.store, ROOT)?.context("redo journal")?;
    assert!(latest.phase == Phase::Complete);
    assert_eq!(fs::read(&retained)?, body);
    assert_eq!(guard::identity(&retained)?, identity);
    assert!(latest.retained_stages.contains(&retained));
    assert_eq!(latest.prior_journals.len(), 2);
    assert_eq!(fs::read(&latest.prior_journals[0])?, initial_journal);
    assert_eq!(fs::read(&latest.prior_journals[1])?, returned_journal);
    for name in &latest.prior_journals {
        let envelope: serde_json::Value = serde_json::from_slice(&fs::read(name)?)?;
        assert!(envelope.get("files").is_none());
        let old: Move = serde_json::from_slice(&crate::cutover::unprotect(
            envelope["payload"]
                .as_str()
                .context("prior encrypted journal")?,
        )?)?;
        assert!(old.phase == Phase::Complete && old.id == ROOT);
    }
    let records = doctor_records(&f.store)?;
    let record = records
        .iter()
        .find(|record| record.id == ROOT)
        .context("doctor record")?;
    assert!(record.stage_paths.contains(&retained));
    assert!(
        record
            .physical_ids
            .iter()
            .any(|physical| physical == PHYSICAL)
    );
    assert_eq!(public_names(&f.store.target, ROOT)?.len(), 1);
    assert!(public_names(&f.source, ROOT)?.is_empty());
    assert!(public_names(&peer, ROOT)?.is_empty());
    recover_twice(&f)?;
    Ok(())
}

#[test]
fn selected_compressed_rollout_moves_and_returns_without_decoding_its_body() -> Result<()> {
    let mut f = Fixture::new()?;
    let compressed = f.original.with_extension("jsonl.zst");
    let bytes = zstd::stream::encode_all(fs::read(&f.original)?.as_slice(), 1)?;
    fs::write(&compressed, &bytes)?;
    fs::remove_file(&f.original)?;
    f.original = compressed;
    age(&f.original)?;
    Connection::open(f.source.join("state_5.sqlite"))?.execute(
        "UPDATE threads SET rollout_path=?1 WHERE id=?2",
        params![f.original.to_str().context("compressed path")?, ROOT],
    )?;
    let identity = guard::identity(&f.original)?;
    let plan = f.plan()?;
    let target = plan.files[0]
        .target
        .clone()
        .context("compressed destination")?;
    run_plan(&f.store, plan)?;
    assert!(!f.original.exists());
    assert_eq!(fs::read(&target)?, bytes);
    assert_eq!(guard::identity(&target)?, identity);
    assert_eq!(guard::hardlink_count(&target)?, 1);
    return_current(&f.store, ROOT, None, true, true)?;
    assert!(!target.exists());
    assert_eq!(fs::read(&f.original)?, bytes);
    assert_eq!(guard::identity(&f.original)?, identity);
    assert_eq!(guard::hardlink_count(&f.original)?, 1);
    Ok(())
}

#[test]
fn explicit_map_resolution_closes_decided_move_preserving_other_uuid_changes() -> Result<()> {
    let f = Fixture::new()?;
    let witness = interrupt(&f, "decided", 1)?;
    let mut current: serde_json::Value = serde_json::from_slice(&fs::read(&f.store.map_path)?)?;
    current["sessions"][ROOT] = "SYNTHETIC conflicting home".into();
    current["sessions"][CHILD] = "SYNTHETIC concurrently updated other home".into();
    current["future"] = serde_json::json!({"preserve": true});
    fs::write(&f.store.map_path, serde_json::to_vec(&current)?)?;
    assert!(recover(&f.store, ROOT).is_err());
    assert!(open(&load(&f.store, ROOT)?.context("open journal")?));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&f.store.map_path)?)?,
        current
    );
    recover_resolve(&f.store, ROOT)?;
    let m = load(&f.store, ROOT)?.context("resolved journal")?;
    assert!(m.phase == Phase::Complete && !open(&m));
    current["sessions"][ROOT] = serde_json::to_value(&f.store.target)?;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&f.store.map_path)?)?,
        current
    );
    assert!(!f.original.exists());
    let target = witness.files[0].target.as_ref().context("target")?;
    assert_eq!(fs::read(target)?, witness.files[0].body);
    assert_eq!(guard::identity(target)?, witness.files[0].identity);
    no_rows(&f.source)?;
    assert!(rows(&f.store.target)?.goal.is_some());
    recover_twice(&f)?;
    Ok(())
}

#[test]
fn source_append_after_partial_alias_removal_keeps_cleared_goal_and_short_evidence() -> Result<()> {
    let f = Fixture::new()?;
    let alias = f
        .source
        .join("archived_sessions")
        .join(f.original.file_name().unwrap());
    fs::create_dir_all(alias.parent().context("alias parent")?)?;
    fs::hard_link(&f.original, &alias)?;
    let witness = interrupt(&f, "source_name_removed", 1)?;
    let public = witness.files[0]
        .names
        .iter()
        .find(|p| p.exists())
        .context("remaining source alias")?;
    append_bytes(public, APPEND)?;
    Connection::open(f.source.join("goals_1.sqlite"))?
        .execute("DELETE FROM thread_goals WHERE thread_id=?1", [ROOT])?;
    let current = rows(&f.source)?;
    recover_twice(&f)?;
    assert!(rows(&f.source)? == current);
    assert!(rows(&f.source)?.goal.is_none());
    assert!(f.original.exists() && alias.exists());
    assert!(!witness.files[0].stage.exists());
    assert_eq!(guard::hardlink_count(&f.original)?, 2);
    let evidence = f
        .store
        .home
        .join("journal/session-moves-evidence")
        .join(format!("{ROOT}-aborted-source-kept.json"));
    let envelope: serde_json::Value = serde_json::from_slice(&fs::read(evidence)?)?;
    let decoded = crate::cutover::unprotect(envelope["payload"].as_str().context("payload")?)?;
    let text = String::from_utf8(decoded)?;
    assert!(!text.contains("SYNTHETIC") && !text.contains("objective") && !text.contains("rows"));
    let short: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(short["outcome"], "aborted-source-kept");
    assert!(
        short["files"][0]["current_bytes"].as_u64().unwrap()
            > short["files"][0]["planned_bytes"].as_u64().unwrap()
    );
    assert!(short["files"][0]["identity"].is_object());
    assert!(short["files"][0]["modified_unix_ms"].is_number());
    Ok(())
}

#[test]
fn manual_share_refuses_open_move_ids_and_borrowed_ancestors_before_publication() -> Result<()> {
    for borrowed_ancestor in [false, true] {
        let f = Fixture::new()?;
        let witness = interrupt(&f, "staged", 2)?;
        let selected = if borrowed_ancestor {
            rollout(&f.source, CHILD, CHILD_PHYSICAL, Some(ROOT), Some(PHYSICAL))?;
            CHILD
        } else {
            ROOT
        };
        let catalog = discovery::catalog(&f.source)?;
        for apply in [false, true] {
            let error = super::super::import_one(
                &f.store,
                &catalog,
                selected,
                ImportOptions {
                    apply,
                    skip_existing: false,
                    announce: false,
                    keep_source: true,
                    clean_stale: false,
                },
            )
            .expect_err("open move must fence manual sharing and its ancestors");
            assert!(
                error
                    .to_string()
                    .contains("이 대화는 이동 복구가 필요합니다")
            );
            assert!(!f.store.path().exists());
            assert!(public_names(&f.store.target, ROOT)?.is_empty());
            assert!(public_names(&f.store.target, CHILD)?.is_empty());
            no_rows(&f.store.target)?;
            assert!(open(&load(&f.store, ROOT)?.context("preserved move")?));
        }
        if borrowed_ancestor {
            let child = discovery::catalog(&f.source)?.active(CHILD)?.path.clone();
            fs::remove_file(child)?;
        }
        recover_twice(&f)?;
        assert!(f.original.exists());
        assert!(!witness.files[0].stage.exists());
    }
    Ok(())
}

/// Opt-in evidence aid. The default and full suites never read a real home.
#[cfg(feature = "testkit")]
#[test]
#[ignore = "explicit R9.1 authorization and read-only source/output paths required"]
fn r91_authorized_readonly_inventory() -> Result<()> {
    let source = PathBuf::from(
        std::env::var_os("QB_R91_READONLY_HOME").context("explicit source required")?,
    );
    ensure!(
        source == Path::new("C:/Users/SampleUser/.codex"),
        "authorized session inventory only"
    );
    let output = PathBuf::from(
        std::env::var_os("QB_R91_READONLY_REPORT").context("explicit report required")?,
    );
    ensure!(
        output.is_absolute() && output.components().any(|p| p.as_os_str() == ".sandbox"),
        "project evidence only"
    );
    let mut f = Fixture::new()?;
    f.source = fs::canonicalize(&source)?;
    f.store.search_homes = super::super::known_homes(source.parent().context("source parent")?);
    let _cache = discovery::read_only_cache();
    let catalog = discovery::catalog(&source)?;
    let mut movable = 0usize;
    let mut refused = BTreeMap::<&str, usize>::new();
    for id in catalog.threads.keys() {
        match make_plan(&f.store, &source, &f.store.target, id, false, &source) {
            Ok(_) => movable += 1,
            Err(error) => {
                let message = error.to_string();
                let reason = if error.is::<discovery::PhysicalConflict>() {
                    "physical_id_conflict"
                } else if error.is::<super::super::InventoryConflict>() {
                    "inventory_divergence"
                } else if error.is::<guard::LiveWriter>() {
                    "live_writer"
                } else if message.contains("갈라진 대화") {
                    "descendants"
                } else if message.contains("조상 전용") {
                    "ancestor_dependency"
                } else if message.contains("120초") {
                    "recent_use"
                } else if message.contains("openai 제공자") {
                    "different_provider"
                } else if message.contains("압축") {
                    "related_compressed_unreadable"
                } else if message.contains("버전")
                    || message.contains("마이그레이션")
                    || message.contains("스키마")
                {
                    "metadata_schema"
                } else {
                    "other_preflight_refusal"
                };
                *refused.entry(reason).or_default() += 1;
            }
        }
    }
    let mut unreadable = BTreeMap::<&str, usize>::new();
    for item in &catalog.unreadable {
        *unreadable.entry(item.reason).or_default() += 1;
    }
    fs::write(
        output,
        serde_json::to_vec_pretty(&serde_json::json!({
            "read_only":true,"credential_or_config_access":false,"content_retained":false,
            "logical_ids":catalog.threads.len(),"compressed_names":catalog.compressed.len(),
            "movable":movable,"refused_with_reason":refused,"unreadable_names_by_reason":unreadable,
            "scope":"same production make_plan; synthetic destination/map/journal; no mutation engine invoked"
        }))?,
    )?;
    Ok(())
}
