//! Read-only D21 panel 2 audit (docs/decisions.md): names and header IDs only.
//! Never open rollout bodies, restore snapshots, create locks, or repair data.
use super::{Store, db, discovery, guard};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Diagnostic {
    pub(super) code: &'static str,
    pub(super) id: Option<String>,
    pub(super) paths: Vec<PathBuf>,
}

fn report(out: &mut Vec<Diagnostic>, code: &'static str, id: Option<&str>, paths: Vec<PathBuf>) {
    out.push(Diagnostic {
        code,
        id: id.filter(|id| discovery::valid_id(id)).map(str::to_owned),
        paths,
    });
}

struct Home {
    catalog: discovery::Catalog,
    threads: BTreeMap<String, PathBuf>,
    goals: BTreeSet<String>,
}

struct Operation {
    id: String,
    physical_ids: Vec<String>,
    source: PathBuf,
    owners: BTreeSet<PathBuf>,
    open: bool,
    shared: bool,
}

/// Include goal-only rows, without selecting objectives, counters or budgets.
fn goal_ids(home: &Path) -> Result<BTreeSet<String>> {
    let path = home.join("goals_1.sqlite");
    guard::safe_path(home, &path)?;
    if !path.try_exists()? {
        return Ok(BTreeSet::new());
    }
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    connection.execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED;")?;
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='thread_goals')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(BTreeSet::new());
    }
    let mut statement = connection.prepare("SELECT thread_id FROM thread_goals")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    ensure!(
        ids.iter().all(|id| discovery::valid_id(id)),
        "대화 상태 ID를 확인할 수 없습니다"
    );
    Ok(ids.into_iter().map(|id| id.to_ascii_lowercase()).collect())
}

fn canonical(path: &Path) -> Result<PathBuf> {
    guard::safe_path(path, path)?;
    Ok(fs::canonicalize(path)?)
}

/// Read every staging entry, including names which are not rollout filenames.
/// A reparse point is evidence to report, never a directory to follow.
fn staging(store: &Store, recorded: &BTreeSet<PathBuf>, out: &mut Vec<Diagnostic>) -> Result<()> {
    let root = store.target.join(".quota-baton/staging");
    guard::safe_path(&store.target, &root)?;
    if !root.try_exists()? {
        return Ok(());
    }
    let mut pending = vec![(root, 0)];
    let mut count = 0;
    while let Some((dir, depth)) = pending.pop() {
        ensure!(depth <= 16, "단계 폴더를 확인할 수 없습니다");
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            count += 1;
            ensure!(count <= 100_000, "단계 파일이 너무 많습니다");
            if guard::safe_path(&store.target, &path).is_err() {
                report(out, "안전하지_않은_단계", None, vec![path]);
                continue;
            }
            if path.is_dir() {
                pending.push((path, depth + 1));
            } else if !recorded.contains(&fs::canonicalize(&path)?) {
                report(out, "고아_단계", None, vec![path]);
            }
        }
    }
    Ok(())
}

fn descendants(children: &BTreeMap<String, BTreeSet<String>>, id: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::from([id.to_owned()]);
    let mut pending = vec![id.to_owned()];
    while let Some(parent) = pending.pop() {
        if let Some(items) = children.get(&parent) {
            for child in items {
                if found.insert(child.clone()) {
                    pending.push(child.clone());
                }
            }
        }
    }
    found.remove(id);
    found
}

/// Evidence analysis: operation owners are authoritative, and a Q prefix
/// without its own row is intentional only when a Q root still references it.
fn analyze(q: &Path, homes: &[Home], operations: &[Operation]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut physical: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // Journals retain physical root IDs even when neither public home has a
    // root header (a crash after source unlinking, before target publication).
    for operation in operations {
        for id in &operation.physical_ids {
            physical
                .entry(id.clone())
                .or_default()
                .insert(operation.id.clone());
        }
    }
    for home in homes {
        for item in home.catalog.all() {
            physical
                .entry(item.physical.clone())
                .or_default()
                .insert(item.thread.clone());
        }
    }
    let mut children: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut q_parents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for home in homes {
        for item in home.catalog.all() {
            let parents = item.history_base.as_ref().and_then(|id| physical.get(id));
            for parent in item.forked_from.iter().chain(parents.into_iter().flatten()) {
                if parent != &item.thread {
                    children
                        .entry(parent.clone())
                        .or_default()
                        .insert(item.thread.clone());
                    if home.catalog.home == q {
                        q_parents
                            .entry(item.thread.clone())
                            .or_default()
                            .insert(parent.clone());
                    }
                }
            }
        }
    }
    for (id, logical) in &physical {
        if logical.len() > 1 {
            report(
                &mut out,
                "대화_식별_충돌",
                Some(id),
                homes
                    .iter()
                    .flat_map(|home| home.catalog.all())
                    .filter(|r| &r.physical == id)
                    .map(|r| r.path.clone())
                    .collect(),
            );
        }
    }
    let mut expected: BTreeMap<&str, BTreeSet<PathBuf>> = BTreeMap::new();
    for operation in operations {
        let owners = expected.entry(&operation.id).or_default();
        if !owners.is_empty() && *owners != operation.owners {
            report(
                &mut out,
                "소유_기록_충돌",
                Some(&operation.id),
                owners.union(&operation.owners).cloned().collect(),
            );
        }
        owners.extend(operation.owners.iter().cloned());
        if operation.open {
            report(
                &mut out,
                "미완료_이동",
                Some(&operation.id),
                vec![operation.source.clone()],
            );
        }
        if !operation.shared && (operation.open || !operation.owners.contains(&operation.source)) {
            let family = descendants(&children, &operation.id);
            for home in homes
                .iter()
                .filter(|home| home.catalog.home == operation.source)
            {
                for item in home
                    .catalog
                    .all()
                    .filter(|item| family.contains(&item.thread))
                {
                    report(
                        &mut out,
                        "누락_후손",
                        Some(&item.thread),
                        vec![item.path.clone()],
                    );
                }
            }
        }
    }
    let mut actual: BTreeMap<&str, BTreeSet<PathBuf>> = BTreeMap::new();
    for home in homes {
        for id in home.threads.keys().chain(home.goals.iter()) {
            actual
                .entry(id)
                .or_default()
                .insert(home.catalog.home.clone());
            if expected
                .get(id.as_str())
                .is_some_and(|owners| !owners.contains(&home.catalog.home))
            {
                report(
                    &mut out,
                    "잘못된_소유_행",
                    Some(id),
                    vec![home.catalog.home.clone()],
                );
            }
        }
        for (id, path) in &home.threads {
            // Canonicalize the stored path: native Codex often stores ordinary
            // C:\ paths while discovery uses Windows verbatim canonical paths.
            let selected = guard::safe_path(&home.catalog.home, path)
                .and_then(|()| Ok(fs::canonicalize(path)?));
            if !selected.is_ok_and(|path| {
                home.catalog
                    .all()
                    .any(|r| r.thread == *id && r.path == path)
            }) {
                report(
                    &mut out,
                    "대화_경로_불일치",
                    Some(id),
                    vec![home.catalog.home.clone(), path.clone()],
                );
            }
        }
        if home.catalog.home == q {
            let mut referenced: BTreeSet<String> = home.threads.keys().cloned().collect();
            let mut pending: Vec<_> = referenced.iter().cloned().collect();
            while let Some(child) = pending.pop() {
                if let Some(parents) = q_parents.get(&child) {
                    for parent in parents {
                        if referenced.insert(parent.clone()) {
                            pending.push(parent.clone());
                        }
                    }
                }
            }
            for item in home.catalog.all() {
                if !home.threads.contains_key(&item.thread)
                    && !referenced.contains(&item.thread)
                    && !operations.iter().any(|op| op.open && op.id == item.thread)
                {
                    report(
                        &mut out,
                        "미참조_조상",
                        Some(&item.thread),
                        vec![item.path.clone()],
                    );
                }
            }
        }
    }
    for (id, owners) in &actual {
        if owners.len() > 1
            && !expected
                .get(id)
                .is_some_and(|allowed| owners.is_subset(allowed))
        {
            report(
                &mut out,
                "중복_소유_행",
                Some(id),
                owners.iter().cloned().collect(),
            );
        }
    }
    for operation in operations
        .iter()
        .filter(|op| !op.open && !op.shared && !op.owners.contains(&op.source))
    {
        for owner in &operation.owners {
            if !homes
                .iter()
                .any(|home| home.catalog.home == *owner && home.threads.contains_key(&operation.id))
            {
                report(
                    &mut out,
                    "소유_행_없음",
                    Some(&operation.id),
                    vec![owner.clone()],
                );
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

pub(super) fn check(store: &Store) -> Result<Vec<Diagnostic>> {
    let records = super::staged::doctor_records(store)?;
    check_records(store, records)
}

fn check_records(
    store: &Store,
    records: Vec<super::staged::DoctorRecord>,
) -> Result<Vec<Diagnostic>> {
    let legacy = store.load()?;
    let mut paths = store.search_homes.iter().cloned().collect::<BTreeSet<_>>();
    paths.insert(store.target.clone());
    let mut operations = Vec::new();
    let mut stages = BTreeSet::new();
    let mut staged_ids = BTreeSet::new();
    for record in records {
        ensure!(
            discovery::valid_id(&record.id)
                && record.physical_ids.iter().all(|id| discovery::valid_id(id)),
            "이동 기록 ID를 확인할 수 없습니다"
        );
        paths.extend([
            record.source.clone(),
            record.target.clone(),
            record.owner.clone(),
        ]);
        staged_ids.insert(record.id.to_ascii_lowercase());
        for path in record.stage_paths {
            // Return destinations are account homes; staging always stays in Q.
            guard::safe_path(&store.target, &path)?;
            if path.try_exists()? {
                stages.insert(fs::canonicalize(path)?);
            }
        }
        operations.push(Operation {
            id: record.id.to_ascii_lowercase(),
            physical_ids: record
                .physical_ids
                .into_iter()
                .map(|id| id.to_ascii_lowercase())
                .collect(),
            source: canonical(&record.source)?,
            owners: BTreeSet::from([canonical(&record.owner)?]),
            open: record.open,
            shared: false,
        });
    }
    for import in legacy.imports.values() {
        paths.extend([import.source_home.clone(), import.target_home.clone()]);
        paths.extend(import.stale.iter().map(|copy| copy.home.clone()));
        if staged_ids.contains(&import.root) {
            continue;
        }
        let source = canonical(&import.source_home)?;
        let target = canonical(&import.target_home)?;
        let open = !import.complete || (!import.keep_source && !import.move_done);
        let owners = if import.keep_source {
            BTreeSet::from([source.clone(), target])
        } else if import.move_done {
            BTreeSet::from([target])
        } else {
            BTreeSet::from([source.clone()])
        };
        operations.push(Operation {
            id: import.root.clone(),
            physical_ids: import
                .files
                .iter()
                .filter(|file| file.thread == import.root)
                .filter_map(|file| {
                    let stem = file.source.file_stem()?.to_str()?;
                    let id = stem.get(stem.len().checked_sub(36)?..)?;
                    discovery::valid_id(id).then(|| id.to_ascii_lowercase())
                })
                .collect(),
            source: source.clone(),
            owners: owners.clone(),
            open,
            shared: import.keep_source,
        });
        // Older share journals included ancestor metadata as well as the root.
        if import.keep_source {
            for id in import
                .threads
                .iter()
                .filter(|id| *id != &import.root && !staged_ids.contains(*id))
            {
                operations.push(Operation {
                    id: id.clone(),
                    physical_ids: Vec::new(),
                    source: source.clone(),
                    owners: owners.clone(),
                    open,
                    shared: true,
                });
            }
        }
    }
    let mut homes = Vec::new();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for path in paths {
        if !path.try_exists()? {
            continue;
        }
        let inspected = (|| -> Result<Home> {
            let home = canonical(&path)?;
            let catalog = discovery::catalog(&home)?;
            let threads = db::threads(&home)?
                .into_iter()
                .map(|row| (row.id.to_ascii_lowercase(), row.rollout_path))
                .collect();
            let goals = goal_ids(&home)?;
            Ok(Home {
                catalog,
                threads,
                goals,
            })
        })();
        match inspected {
            Ok(home) if seen.insert(home.catalog.home.clone()) => {
                for physical in &home.catalog.conflicts {
                    report(
                        &mut out,
                        "물리_ID_중복",
                        Some(physical),
                        home.catalog
                            .all()
                            .filter(|r| &r.physical == physical)
                            .map(|r| r.path.clone())
                            .collect(),
                    );
                }
                for unreadable in &home.catalog.unreadable {
                    report(
                        &mut out,
                        "압축_대화_읽기_실패",
                        unreadable.physical.as_deref(),
                        vec![unreadable.path.clone()],
                    );
                }
                homes.push(home);
            }
            Ok(_) => (),
            Err(_) => report(&mut out, "홈_검사_실패", None, vec![path]),
        }
    }
    out.extend(analyze(&canonical(&store.target)?, &homes, &operations));
    staging(store, &stages, &mut out)?;
    out.sort();
    out.dedup();
    Ok(out)
}

pub(super) fn run(store: &Store) -> Result<()> {
    // Never expose arbitrary SQLite/journal parser errors through this command.
    let diagnostics = check(store).map_err(|_| anyhow::anyhow!("대화_점검_읽기_실패"))?;
    for item in &diagnostics {
        println!("{}", item.code);
        if let Some(id) = &item.id {
            println!("{id}");
        }
        for path in &item.paths {
            println!("{}", path.display());
        }
    }
    ensure!(
        diagnostics.is_empty(),
        "대화 점검에서 문제가 발견되었습니다"
    );
    println!("대화_점검_정상");
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    const A: &str = "11111111-1111-1111-1111-111111111111";
    const B: &str = "22222222-2222-2222-2222-222222222222";
    const C: &str = "33333333-3333-3333-3333-333333333333";
    const D: &str = "44444444-4444-4444-4444-444444444444";

    fn fixture() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r9-doctor-tests");
        fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap()
    }

    #[test]
    fn readable_compressed_is_clean_and_only_failed_headers_are_reported() {
        let root = fixture();
        let q = root.path().join("q");
        let plain = write(&q, A, A, None, None);
        let compressed = plain.with_extension("jsonl.zst");
        let bytes = zstd::stream::encode_all(fs::read(&plain).unwrap().as_slice(), 1).unwrap();
        fs::write(&compressed, bytes).unwrap();
        fs::remove_file(plain).unwrap();
        let connection = rusqlite::Connection::open(q.join("state_5.sqlite")).unwrap();
        connection.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, model_provider TEXT NOT NULL);").unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES(?1, ?2, 'openai')",
                rusqlite::params![A, compressed.to_str().unwrap()],
            )
            .unwrap();
        drop(connection);
        let store = Store {
            home: root.path().join("qb"),
            target: canonical(&q).unwrap(),
            search_homes: Vec::new(),
            map_path: root.path().join("map.json"),
        };
        assert!(check_records(&store, Vec::new()).unwrap().is_empty());
        let bad = q
            .join("sessions")
            .join(format!("rollout-test-{B}.jsonl.zst"));
        fs::write(&bad, b"SYNTHETIC-PRIVATE-INVALID-ZSTD").unwrap();
        let result = check_records(&store, Vec::new()).unwrap();
        let failures: Vec<_> = result
            .iter()
            .filter(|d| d.code == "압축_대화_읽기_실패")
            .collect();
        assert_eq!(failures.len(), 1, "{result:?}");
        assert_eq!(failures[0].id.as_deref(), Some(B));
        assert_eq!(failures[0].paths, [canonical(&bad).unwrap()]);
        assert!(
            !result
                .iter()
                .any(|d| d.paths.contains(&canonical(&compressed).unwrap()))
        );
    }

    #[test]
    fn conflicting_physical_copies_are_reported_without_poisoning_the_home() {
        let root = fixture();
        let q = root.path().join("q");
        let original = write(&q, A, A, None, None);
        let copy = q
            .join("archived_sessions")
            .join(original.file_name().unwrap());
        fs::create_dir_all(copy.parent().unwrap()).unwrap();
        fs::copy(&original, &copy).unwrap();
        write(&q, B, B, None, None);
        let store = Store {
            home: root.path().join("qb"),
            target: canonical(&q).unwrap(),
            search_homes: Vec::new(),
            map_path: root.path().join("map.json"),
        };
        let diagnostics = check_records(&store, Vec::new()).unwrap();
        let conflicts: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == "물리_ID_중복")
            .collect();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].id.as_deref(), Some(A));
        assert_eq!(conflicts[0].paths.len(), 2);
        assert!(conflicts[0].paths.contains(&canonical(&original).unwrap()));
        assert!(conflicts[0].paths.contains(&canonical(&copy).unwrap()));
        assert!(!diagnostics.iter().any(|d| d.code == "홈_검사_실패"));
        assert!(discovery::catalog(&q).unwrap().chain(B).is_ok());
    }

    fn write(
        home: &Path,
        id: &str,
        physical: &str,
        base: Option<&str>,
        parent: Option<&str>,
    ) -> PathBuf {
        let path = home
            .join("sessions")
            .join(format!("rollout-test-{physical}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let meta = serde_json::json!({"type":"session_meta","payload":{
            "id":id,"model_provider":"openai","forked_from_id":parent,
            "history_base":base.map(|id| serde_json::json!({"thread_id":id}))
        }});
        fs::write(&path, format!("{meta}\n")).unwrap();
        path
    }

    fn home(path: &Path, rows: &[(&str, PathBuf)]) -> Home {
        fs::create_dir_all(path).unwrap();
        Home {
            catalog: discovery::catalog(path).unwrap(),
            threads: rows
                .iter()
                .map(|(id, path)| ((*id).into(), path.clone()))
                .collect(),
            goals: BTreeSet::new(),
        }
    }

    fn operation(source: &Path, target: &Path) -> Operation {
        Operation {
            id: A.into(),
            physical_ids: Vec::new(),
            source: canonical(source).unwrap(),
            owners: BTreeSet::from([canonical(target).unwrap()]),
            open: false,
            shared: false,
        }
    }

    #[test]
    fn intentional_ancestor_aliases_are_clean_but_unreferenced_names_are_reported() {
        let root = fixture();
        let q = root.path().join("q");
        let ancestor = write(&q, A, A, None, None);
        let alias = q
            .join("archived_sessions")
            .join(ancestor.file_name().unwrap());
        fs::create_dir_all(alias.parent().unwrap()).unwrap();
        fs::hard_link(ancestor, alias).unwrap();
        let child = write(&q, B, B, Some(A), Some(A));
        let observed = home(&q, &[(B, child)]);
        assert!(analyze(&canonical(&q).unwrap(), &[observed], &[]).is_empty());
        let unused = write(&q, C, C, None, None);
        let observed = home(
            &q,
            &[(
                B,
                q.join("sessions").join(format!("rollout-test-{B}.jsonl")),
            )],
        );
        let result = analyze(&canonical(&q).unwrap(), &[observed], &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code, "미참조_조상");
        assert_eq!(result[0].id.as_deref(), Some(C));
        assert_eq!(result[0].paths, [unused]);
    }

    #[test]
    fn wrong_owner_goal_only_rows_and_header_only_descendants_are_detected() {
        let root = fixture();
        let source = root.path().join("source");
        let q = root.path().join("q");
        // The physical parent D is now only in Q. Source has no parent row or name.
        let parent = write(&q, A, D, None, None);
        write(&source, B, B, Some(D), None);
        write(&source, C, C, None, Some(B));
        let mut old = home(&source, &[]);
        old.goals.insert(A.into());
        let new = home(&q, &[(A, parent)]);
        let result = analyze(
            &canonical(&q).unwrap(),
            &[old, new],
            &[operation(&source, &q)],
        );
        assert!(
            result
                .iter()
                .any(|d| d.code == "잘못된_소유_행" && d.id.as_deref() == Some(A))
        );
        let descendants: BTreeSet<_> = result
            .iter()
            .filter(|d| d.code == "누락_후손")
            .map(|d| d.id.clone().unwrap())
            .collect();
        assert_eq!(descendants, BTreeSet::from([B.into(), C.into()]));
    }

    #[test]
    fn journal_physical_anchor_finds_children_when_all_root_names_are_gone() {
        let root = fixture();
        let source = root.path().join("source");
        let q = root.path().join("q");
        fs::create_dir_all(&q).unwrap();
        write(&source, B, B, Some(D), None);
        write(&source, C, C, None, Some(B));
        let mut op = operation(&source, &q);
        op.physical_ids = vec![D.into()];
        op.open = true;
        op.owners = BTreeSet::from([canonical(&source).unwrap()]);
        let result = analyze(
            &canonical(&q).unwrap(),
            &[home(&source, &[]), home(&q, &[])],
            &[op],
        );
        let children: BTreeSet<_> = result
            .iter()
            .filter(|d| d.code == "누락_후손")
            .map(|d| d.id.clone().unwrap())
            .collect();
        assert_eq!(children, BTreeSet::from([B.into(), C.into()]));
        assert!(result.iter().any(|d| d.code == "미완료_이동"));
    }

    #[test]
    fn explicit_legacy_share_allows_both_row_owners_and_children() {
        let root = fixture();
        let source = root.path().join("source");
        let q = root.path().join("q");
        let original = write(&source, A, A, None, None);
        write(&source, B, B, None, Some(A));
        let target = write(&q, A, A, None, None);
        let mut op = operation(&source, &q);
        op.shared = true;
        op.owners.insert(canonical(&source).unwrap());
        let result = analyze(
            &canonical(&q).unwrap(),
            &[home(&source, &[(A, original)]), home(&q, &[(A, target)])],
            &[op],
        );
        assert!(result.is_empty(), "{result:?}");
    }

    #[test]
    fn aborted_move_with_original_rowless_source_does_not_report_missed_children() {
        let root = fixture();
        let source = root.path().join("source");
        let q = root.path().join("q");
        fs::create_dir_all(&q).unwrap();
        write(&source, A, A, None, None);
        write(&source, B, B, None, Some(A));
        let mut aborted = operation(&source, &q);
        aborted.owners = BTreeSet::from([canonical(&source).unwrap()]);
        assert!(
            analyze(
                &canonical(&q).unwrap(),
                &[home(&source, &[]), home(&q, &[])],
                &[aborted]
            )
            .is_empty()
        );
    }

    #[test]
    fn empty_check_creates_no_journals_locks_or_databases() {
        let root = fixture();
        let q = root.path().join("q");
        let source = root.path().join("source");
        fs::create_dir_all(&q).unwrap();
        fs::create_dir_all(&source).unwrap();
        let store = Store {
            home: root.path().join("qb"),
            target: canonical(&q).unwrap(),
            search_homes: vec![source.clone()],
            map_path: root.path().join("map.json"),
        };
        assert!(check(&store).unwrap().is_empty());
        assert!(!store.home.exists());
        assert_eq!(fs::read_dir(q).unwrap().count(), 0);
        assert_eq!(fs::read_dir(source).unwrap().count(), 0);
        assert!(!store.map_path.exists());
    }

    #[test]
    fn orphan_staging_is_read_only_and_journaled_names_are_recognized() {
        let root = fixture();
        let q = root.path().join("q");
        let stage = q.join(".quota-baton/staging/operation");
        fs::create_dir_all(&stage).unwrap();
        let known = stage.join("known");
        let orphan = stage.join("orphan");
        fs::write(&known, b"synthetic stage").unwrap();
        fs::write(&orphan, b"synthetic stage").unwrap();
        let store = Store {
            home: root.path().join("qb"),
            target: canonical(&q).unwrap(),
            search_homes: vec![],
            map_path: root.path().join("map.json"),
        };
        let mut result = Vec::new();
        staging(
            &store,
            &BTreeSet::from([canonical(&known).unwrap()]),
            &mut result,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code, "고아_단계");
        assert_eq!(result[0].paths.as_slice(), std::slice::from_ref(&orphan));
        assert_eq!(fs::read(known).unwrap(), b"synthetic stage");
        assert_eq!(fs::read(orphan).unwrap(), b"synthetic stage");
        assert!(!store.home.exists());
    }

    fn check_return_with_q_staging(open: bool, keep_stage: bool) {
        let root = fixture();
        let q = root.path().join("q");
        let account = root.path().join("account");
        fs::create_dir_all(&q).unwrap();
        fs::create_dir_all(&account).unwrap();
        let owner = if open { &q } else { &account };
        let rollout = write(owner, A, D, None, None);
        let stage = q
            .join(".quota-baton/staging")
            .join(A)
            .join("return/0.jsonl");
        fs::create_dir_all(stage.parent().unwrap()).unwrap();
        if keep_stage {
            fs::hard_link(&rollout, &stage).unwrap();
        }
        let state = owner.join("state_5.sqlite");
        let connection = rusqlite::Connection::open(&state).unwrap();
        connection.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, model_provider TEXT NOT NULL);").unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES(?1, ?2, 'openai')",
                rusqlite::params![A, rollout.to_str().unwrap()],
            )
            .unwrap();
        drop(connection);
        let state_before = fs::read(&state).unwrap();
        let rollout_before = fs::read(&rollout).unwrap();
        let store = Store {
            home: root.path().join("qb"),
            target: canonical(&q).unwrap(),
            search_homes: vec![account.clone()],
            map_path: root.path().join("map.json"),
        };
        let record = super::super::staged::DoctorRecord {
            id: A.into(),
            source: canonical(&q).unwrap(),
            target: canonical(&account).unwrap(),
            owner: canonical(owner).unwrap(),
            stage_paths: vec![stage.clone()],
            open,
            physical_ids: vec![D.into()],
        };
        // Exercises the production record validation and staging scan together;
        // validating against record.target would reject the Q path on return.
        let result = check_records(&store, vec![record]).unwrap();
        if open {
            assert_eq!(result.len(), 1, "{result:?}");
            assert_eq!(result[0].code, "미완료_이동");
            assert_eq!(result[0].id.as_deref(), Some(A));
        } else {
            assert!(result.is_empty(), "{result:?}");
        }
        assert_eq!(fs::read(&state).unwrap(), state_before);
        assert_eq!(fs::read(&rollout).unwrap(), rollout_before);
        assert_eq!(stage.exists(), keep_stage);
        assert!(!store.home.exists());
    }

    #[test]
    fn completed_return_accepts_q_stages_with_account_destination() {
        check_return_with_q_staging(false, true);
        check_return_with_q_staging(false, false);
    }

    #[test]
    fn interrupted_return_recognizes_q_stage_and_reports_pending_only() {
        check_return_with_q_staging(true, true);
    }

    #[test]
    fn goals_inventory_reads_only_ids_and_never_creates_an_absent_database() {
        let root = fixture();
        assert!(goal_ids(root.path()).unwrap().is_empty());
        let path = root.path().join("goals_1.sqlite");
        assert!(!path.exists());
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE thread_goals(thread_id TEXT PRIMARY KEY, objective TEXT);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO thread_goals VALUES(?1, ?2)",
                [A, "SYNTHETIC-PRIVATE-OBJECTIVE"],
            )
            .unwrap();
        drop(connection);
        let before = fs::read(&path).unwrap();
        assert_eq!(goal_ids(root.path()).unwrap(), BTreeSet::from([A.into()]));
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
