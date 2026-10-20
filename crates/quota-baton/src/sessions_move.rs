//! D21 panel 2: one public home, private hardlink staging, durable admission.
//! No rollback after the publication decision. Conflicts retain all evidence.
use super::{ImportOptions, Store, checkpoint, db, discovery, guard, index_plan, update_index};
use crate::{
    sessions_admission as admission, sessions_confirm as confirm, sessions_map as mapping,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
type MoveHook = Box<dyn FnMut(&str) -> Result<()>>;
#[cfg(test)]
thread_local! { static TEST_HOOK: std::cell::RefCell<Option<MoveHook>> = std::cell::RefCell::new(None); }

const CONFLICT: &str = "이동 기록과 현재 상태가 다릅니다. 증거를 보존했습니다. doctor로 확인하세요";
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Prepared,
    Staged,
    Unlinking,
    NamesGone,
    RowsGone,
    Decided,
    Published,
    Rows,
    Mapped,
    Complete,
    Aborted,
}
#[derive(Serialize, Deserialize)]
struct Segment {
    sources: Vec<PathBuf>,
    physical: String,
    stage: PathBuf,
    target: Option<PathBuf>,
    identity: guard::FileIdentity,
    bytes: u64,
    sha256: String,
}
#[derive(Serialize, Deserialize)]
struct Move {
    schema: u8,
    id: String,
    source: PathBuf,
    target: PathBuf,
    origin: PathBuf,
    homes: Vec<PathBuf>,
    files: Vec<Segment>,
    source_db: Vec<db::DbPlan>,
    database: db::DbPlan,
    index: Vec<String>,
    map: mapping::MapPlan,
    phase: Phase,
    #[serde(default)]
    retained_stages: Vec<PathBuf>,
    #[serde(default)]
    prior_journals: Vec<PathBuf>,
}
pub(super) struct DoctorRecord {
    pub id: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub owner: PathBuf,
    pub stage_paths: Vec<PathBuf>,
    pub open: bool,
    pub physical_ids: Vec<String>,
}
fn path(store: &Store, id: &str) -> PathBuf {
    store
        .home
        .join("journal/session-moves")
        .join(format!("{id}.json"))
}
fn open(m: &Move) -> bool {
    !matches!(m.phase, Phase::Complete | Phase::Aborted)
}
fn save(store: &Store, m: &Move) -> Result<()> {
    let p = path(store, &m.id);
    let _parent = guard::prepare_parent(&store.home, p.parent().context(CONFLICT)?)?;
    let payload = crate::cutover::protect(&serde_json::to_vec(m)?)?;
    let native =
        fs::canonicalize(p.parent().context(CONFLICT)?)?.join(p.file_name().context(CONFLICT)?);
    crate::cutover::atomic_bytes(
        &native,
        &serde_json::to_vec(&serde_json::json!({"schema":2,"open":open(m),"payload":payload}))?,
    )
}
fn load(store: &Store, id: &str) -> Result<Option<Move>> {
    ensure!(discovery::valid_id(id), "대화 ID는 UUID여야 합니다");
    let p = path(store, id);
    guard::safe_path(&store.home, &p)?;
    if !p.try_exists()? {
        return Ok(None);
    }
    ensure!(fs::metadata(&p)?.len() < 64 * 1024 * 1024, "{CONFLICT}");
    let envelope: serde_json::Value = serde_json::from_slice(&fs::read(&p)?)?;
    ensure!(envelope["schema"] == 2, "{CONFLICT}");
    let m: Move = serde_json::from_slice(&crate::cutover::unprotect(
        envelope["payload"].as_str().context(CONFLICT)?,
    )?)?;
    ensure!(
        m.schema == 2 && m.id == id && envelope["open"].as_bool() == Some(open(&m)),
        "{CONFLICT}"
    );
    for home in [&m.source, &m.target, &m.origin] {
        ensure!(home.is_absolute(), "{CONFLICT}");
        guard::safe_path(home, home)?;
    }
    ensure!(
        m.source != m.target
            && !m.files.is_empty()
            && m.homes.contains(&m.source)
            && m.homes.contains(&m.target),
        "{CONFLICT}"
    );
    let stage_root = store.target.join(".quota-baton/staging").join(id);
    for file in &m.files {
        ensure!(
            file.stage.starts_with(&stage_root) && !file.sources.is_empty(),
            "{CONFLICT}"
        );
        guard::safe_path(&store.target, &file.stage)?;
        for name in &file.sources {
            let home = m
                .homes
                .iter()
                .find(|h| {
                    name.starts_with(h.join("sessions"))
                        || name.starts_with(h.join("archived_sessions"))
                })
                .context(CONFLICT)?;
            guard::safe_path(home, name)?;
        }
        if let Some(target) = &file.target {
            ensure!(target.starts_with(m.target.join("sessions")), "{CONFLICT}");
            guard::safe_path(&m.target, target)?;
        }
    }
    Ok(Some(m))
}
pub(super) fn doctor_records(store: &Store) -> Result<Vec<DoctorRecord>> {
    let root = store.home.join("journal/session-moves");
    guard::safe_path(&store.home, &root)?;
    let mut records = Vec::new();
    if !root.exists() {
        return Ok(records);
    }
    for entry in fs::read_dir(root)? {
        let p = entry?.path();
        let id = p.file_stem().and_then(|v| v.to_str()).context(CONFLICT)?;
        let m = load(store, id)?.context(CONFLICT)?;
        let is_open = open(&m);
        records.push(DoctorRecord {
            id: m.id,
            source: m.source.clone(),
            target: m.target.clone(),
            owner: if m.phase == Phase::Complete {
                m.target
            } else {
                m.source
            },
            physical_ids: m.files.iter().map(|f| f.physical.clone()).collect(),
            stage_paths: m
                .files
                .into_iter()
                .map(|f| f.stage)
                .chain(m.retained_stages)
                .collect(),
            open: is_open,
        });
    }
    Ok(records)
}
pub(super) fn pending(store: &Store) -> Result<bool> {
    Ok(doctor_records(store)?.iter().any(|r| r.open))
}
fn hash(p: &Path) -> Result<String> {
    let mut f = File::open(p)?;
    let mut h = Sha256::new();
    let mut b = [0; 65536];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
fn same(f: &Segment, p: &Path) -> Result<()> {
    ensure!(
        guard::identity(p)? == f.identity
            && fs::metadata(p)?.len() == f.bytes
            && hash(p)? == f.sha256,
        "{CONFLICT}"
    );
    Ok(())
}
fn surviving(f: &Segment) -> Result<&Path> {
    std::iter::once(&f.stage)
        .chain(f.sources.iter())
        .chain(f.target.iter())
        .find(|p| p.exists())
        .map(PathBuf::as_path)
        .context(CONFLICT)
}
fn known_links(f: &Segment) -> usize {
    std::iter::once(&f.stage)
        .chain(f.sources.iter())
        .chain(f.target.iter())
        .filter(|p| p.exists())
        .count()
}
fn verify_files(m: &Move) -> Result<()> {
    // Complete preflight before any recovery mutation; never delete a conflict.
    for f in &m.files {
        for p in std::iter::once(&f.stage)
            .chain(f.sources.iter())
            .chain(f.target.iter())
            .filter(|p| p.exists())
        {
            same(f, p)?;
        }
        let p = surviving(f)?;
        ensure!(
            guard::hardlink_count(p)? as usize == known_links(f),
            "발견하지 못한 하드링크가 있습니다. doctor로 확인하세요"
        );
    }
    Ok(())
}
fn descendants(home: &Path, id: &str) -> Result<()> {
    let c = discovery::catalog(home)?;
    ensure!(
        c.compressed.is_empty(),
        "압축된 대화의 후손 관계를 확인할 수 없어 이동을 거절합니다. doctor로 확인하세요"
    );
    ensure!(
        c.descendants(id).is_empty(),
        "이 대화에서 갈라진 대화가 있습니다. 가족 이동을 지원하기 전에는 옮길 수 없습니다"
    );
    Ok(())
}
fn rows_compatible(expected: &db::DbPlan, partial: bool) -> Result<()> {
    let actual = db::source_snapshot(
        &expected.target_home,
        &expected
            .threads
            .iter()
            .map(|t| t.id.clone())
            .collect::<Vec<_>>(),
    )?;
    for (a, b) in actual.threads.iter().zip(&expected.threads) {
        for (current, saved) in [(&a.thread, &b.thread), (&a.goal, &b.goal)] {
            ensure!(
                (partial && current.is_none())
                    || current.as_ref().map(|r| &r.values) == saved.as_ref().map(|r| &r.values),
                "{CONFLICT}"
            );
        }
    }
    Ok(())
}
fn found_descendants(m: &Move, home: &Path) -> Result<bool> {
    let c = discovery::catalog(home)?;
    ensure!(
        c.compressed.is_empty(),
        "압축된 대화의 후손 관계를 확인할 수 없어 이동을 거절합니다. doctor로 확인하세요"
    );
    let mut seen = BTreeSet::from([m.id.clone()]);
    loop {
        let before = seen.len();
        for f in c.all() {
            let parent = f.forked_from.as_ref().is_some_and(|p| seen.contains(p));
            let history = f.history_base.as_ref().is_some_and(|p| {
                m.files.iter().any(|s| &s.physical == p)
                    || c.rollouts.get(p).is_some_and(|r| seen.contains(&r.thread))
            });
            if parent || history {
                seen.insert(f.thread.clone());
            }
        }
        if seen.len() == before {
            break;
        }
    }
    Ok(seen.len() > 1)
}
fn source_rows_present(m: &Move) -> Result<()> {
    for p in &m.source_db {
        rows_compatible(p, false)?;
    }
    Ok(())
}
fn target_rows_compatible(m: &Move) -> Result<()> {
    rows_compatible(&m.database, true)
}
fn source_rows_absent(m: &Move) -> Result<()> {
    ensure!(
        m.files
            .iter()
            .all(|f| f.sources.iter().all(|p| !p.exists())),
        "{CONFLICT}"
    );
    for home in &m.homes {
        if home == &m.target {
            continue;
        }
        let p = db::source_snapshot(home, std::slice::from_ref(&m.id))?;
        ensure!(
            p.threads
                .iter()
                .all(|t| t.thread.is_none() && t.goal.is_none()),
            "{CONFLICT}"
        );
    }
    Ok(())
}
fn make_plan(
    store: &Store,
    source: &Path,
    target: &Path,
    id: &str,
    returning: bool,
    origin: &Path,
) -> Result<Move> {
    ensure!(discovery::valid_id(id), "대화 ID는 UUID여야 합니다");
    let source = fs::canonicalize(source)?;
    let target = fs::canonicalize(target)?;
    ensure!(source != target, "원본과 대상 홈이 같습니다");
    let catalog = discovery::catalog(&source)?;
    let files = super::migration_files(&catalog, id)?;
    ensure!(
        files.iter().all(|f| f.thread == id),
        "조상 전용 대화의 모든 관리 재개 경로를 차단할 수 없어 이 대화의 이동을 거절합니다"
    );
    ensure!(
        files
            .iter()
            .all(|f| f.provider.as_deref().is_none_or(|p| p == "openai")),
        "openai 제공자의 대화만 가져올 수 있습니다"
    );
    discovery::refuse_recent(&files, SystemTime::now())?;
    descendants(&source, id)?;
    let dest = discovery::catalog(&target)?;
    ensure!(
        dest.compressed.is_empty(),
        "압축된 대화의 후손 관계를 확인할 수 없어 이동을 거절합니다. doctor로 확인하세요"
    );
    if dest.threads.contains_key(id) {
        return Err(super::InventoryConflict {
            homes: vec![source.clone(), target.clone()],
        }
        .into());
    }
    let mut homes = vec![source.clone(), target.clone()];
    for home in &store.search_homes {
        let h = fs::canonicalize(home)?;
        if !homes.contains(&h) {
            homes.push(h);
        }
    }
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let mut segments: Vec<Segment> = Vec::new();
    let mut snapshots = Vec::new();
    for home in &homes {
        if home == &target {
            continue;
        }
        let other = discovery::catalog(home)?;
        ensure!(
            other.compressed.is_empty(),
            "압축된 대화의 후손 관계를 확인할 수 없어 이동을 거절합니다. doctor로 확인하세요"
        );
        if !other.threads.contains_key(id) {
            let plan = db::source_snapshot(home, &[id.into()])?;
            ensure!(
                plan.threads
                    .iter()
                    .all(|t| t.thread.is_none() && t.goal.is_none()),
                "{CONFLICT}"
            );
            continue;
        }
        descendants(home, id)?;
        let conflict = || super::InventoryConflict {
            homes: vec![source.clone(), home.clone()],
        };
        let expected = files.iter().map(|f| &f.physical).collect::<BTreeSet<_>>();
        let found = other
            .all()
            .filter(|f| f.thread == id)
            .map(|f| &f.physical)
            .collect::<BTreeSet<_>>();
        if found != expected {
            return Err(conflict().into());
        }
        let copy_snapshot = db::source_snapshot(home, &[id.into()])?;
        if copy_snapshot
            .threads
            .iter()
            .any(|t| t.thread.is_some() || t.goal.is_some())
            && !db::compare_goal_state(&[source.clone(), home.clone()], id)?
        {
            return Err(conflict().into());
        }
        for item in other.all().filter(|f| f.thread == id) {
            let peer = files
                .iter()
                .find(|f| f.physical == item.physical)
                .ok_or_else(conflict)?;
            if !super::same_contents(&peer.path, &item.path)? {
                return Err(conflict().into());
            }
            let identity = guard::identity(&item.path)?;
            if let Some(existing) = segments.iter_mut().find(|s| s.identity == identity) {
                ensure!(
                    existing.physical == item.physical,
                    "하나의 파일에 서로 다른 물리 대화 ID가 있어 이동을 거절합니다"
                );
                if !existing.sources.contains(&item.path) {
                    existing.sources.push(item.path.clone());
                }
                continue;
            }
            let primary = home == &source;
            let slot = segments.len();
            segments.push(Segment {
                sources: vec![item.path.clone()],
                physical: item.physical.clone(),
                stage: store
                    .target
                    .join(".quota-baton/staging")
                    .join(id)
                    .join(&nonce)
                    .join(format!("{slot}.jsonl")),
                target: primary.then(|| target.join("sessions").join(&peer.relative)),
                identity,
                bytes: item.bytes,
                sha256: hash(&item.path)?,
            });
        }
        snapshots.push(db::source_snapshot(home, &[id.into()])?);
    }
    // The primary segment is discovered first (source is first in homes).
    ensure!(!segments.is_empty(), "원본에서 대화를 찾지 못했습니다");
    for f in &segments {
        ensure!(
            guard::same_volume(&f.sources[0], &store.target)?
                && guard::same_volume(&f.sources[0], &target)?,
            "서로 다른 볼륨에는 하드링크로 대화를 옮길 수 없습니다"
        );
        ensure!(
            guard::hardlink_count(&f.sources[0])? as usize == f.sources.len(),
            "발견하지 못한 하드링크가 있습니다. doctor로 확인하세요"
        );
    }
    let mut database = if returning {
        db::plan_return(&source, &target, &[id.into()])?
    } else {
        db::plan(&source, &target, &[id.into()])?
    };
    if database.threads.iter().any(|t| t.thread.is_none()) {
        let item = catalog.active(id)?;
        let mut fields = item.metadata.clone();
        fields
            .entry("title".into())
            .or_insert_with(|| db::DbValue::Text(Vec::new()));
        db::add_synthetic_thread(
            &mut database,
            &db::ThreadInfo {
                id: id.into(),
                rollout_path: item.path.clone(),
                model_provider: "openai".into(),
            },
            fields,
        )?;
    }
    let index = index_plan(&target, &database)?;
    let map = mapping::plan(
        &store.map_path,
        id,
        &target,
        &store.home.join("journal/session-map-backups"),
    )?;
    Ok(Move {
        schema: 2,
        id: id.into(),
        source,
        target,
        origin: origin.to_owned(),
        homes,
        files: segments,
        source_db: snapshots,
        database,
        index,
        map,
        phase: Phase::Prepared,
        retained_stages: Vec::new(),
        prior_journals: Vec::new(),
    })
}
fn step(store: &Store, m: &Move, label: &str) -> Result<()> {
    #[cfg(test)]
    TEST_HOOK.with(|hook| -> Result<()> {
        if let Some(f) = hook.borrow_mut().as_mut() {
            f(label)?;
        }
        Ok(())
    })?;
    checkpoint(store, &m.id, &format!("stage:{label}"))
}
fn rollback(store: &Store, m: &mut Move) -> Result<()> {
    ensure!(
        m.phase < Phase::Decided,
        "게시 결정 뒤에는 앞으로만 복구합니다"
    );
    verify_files(m)?;
    ensure!(
        m.files
            .iter()
            .all(|f| f.target.as_ref().is_none_or(|p| !p.exists())),
        "{CONFLICT}"
    );
    for p in &m.source_db {
        rows_compatible(p, true)?;
    }
    let target = db::source_snapshot(&m.target, std::slice::from_ref(&m.id))?;
    ensure!(
        target
            .threads
            .iter()
            .all(|t| t.thread.is_none() && t.goal.is_none()),
        "{CONFLICT}"
    );
    // prepare() in db::apply checks every existing row before inserting any.
    for f in &m.files {
        for name in &f.sources {
            if !name.exists() {
                let origin = surviving(f)?;
                let root = m
                    .homes
                    .iter()
                    .find(|h| name.starts_with(h))
                    .context(CONFLICT)?;
                let _parent = guard::prepare_parent(root, name.parent().context(CONFLICT)?)?;
                fs::hard_link(origin, name)?;
                same(f, name)?;
                step(store, m, "rollback_name_restored")?;
            }
        }
    }
    for db in &m.source_db {
        db::apply(db)?;
        // A rollout-only source legitimately had no thread row. Restore its
        // exact optional rows, rather than requiring destination metadata.
        rows_compatible(db, false)?;
    }
    for f in &m.files {
        if f.stage.exists() {
            guard::unlink_counted(&store.target, &f.stage, f.identity, known_links(f) as u32)?;
        }
    }
    m.phase = Phase::Aborted;
    save(store, m)?;
    step(store, m, "rollback_complete")?;
    Ok(())
}
fn execute(store: &Store, m: &mut Move) -> Result<()> {
    verify_files(m)?;
    if m.phase >= Phase::Decided {
        source_rows_absent(m)?;
        for home in &m.homes {
            ensure!(!found_descendants(m, home)?, "{CONFLICT}");
        }
    }
    target_rows_compatible(m)?;
    if m.phase < Phase::NamesGone {
        source_rows_present(m)?;
    } else if m.phase < Phase::Decided {
        for snap in &m.source_db {
            rows_compatible(snap, true)?;
        }
    }

    if m.phase == Phase::Prepared {
        for f in &m.files {
            let _parent =
                guard::prepare_parent(&store.target, f.stage.parent().context(CONFLICT)?)?;
            fs::hard_link(&f.sources[0], &f.stage)?;
            same(f, &f.stage)?;
            step(store, m, "staged")?;
        }
        m.phase = Phase::Staged;
        save(store, m)?;
        step(store, m, "staged")?;
    }
    if m.phase <= Phase::Unlinking {
        for home in &m.homes {
            if home != &m.target {
                descendants(home, &m.id)?;
            }
        }
        m.phase = Phase::Unlinking;
        save(store, m)?;
        for f in &m.files {
            for name in &f.sources {
                if name.exists() {
                    verify_files(m)?;
                    let root = m
                        .homes
                        .iter()
                        .find(|h| name.starts_with(h))
                        .context(CONFLICT)?;
                    guard::unlink_validated(
                        root,
                        name,
                        f.identity,
                        known_links(f) as u32,
                        |handle| {
                            ensure!(handle.metadata()?.len() == f.bytes, "{CONFLICT}");
                            let mut view = handle;
                            let mut hash = Sha256::new();
                            let mut chunk = [0u8; 65536];
                            loop {
                                let n = view.read(&mut chunk)?;
                                if n == 0 {
                                    break;
                                }
                                hash.update(&chunk[..n]);
                            }
                            ensure!(format!("{:x}", hash.finalize()) == f.sha256, "{CONFLICT}");
                            Ok(())
                        },
                    )?;
                    step(store, m, "source_name_removed")?;
                }
            }
        }
        m.phase = Phase::NamesGone;
        save(store, m)?;
        step(store, m, "source_names_removed")?;
    }
    if m.phase == Phase::NamesGone {
        for home in &m.homes {
            if home != &m.target && found_descendants(m, home)? {
                rollback(store, m)?;
                bail!("이동 중 갈라진 대화가 나타나 원본으로 되돌렸습니다");
            }
        }
        for snap in &m.source_db {
            db::remove_exact(snap)?;
        }
        m.phase = Phase::RowsGone;
        save(store, m)?;
        step(store, m, "source_rows_removed")?;
    }
    if m.phase == Phase::RowsGone {
        verify_files(m)?;
        source_rows_absent(m)?;
        for home in &m.homes {
            if home != &m.target && found_descendants(m, home)? {
                rollback(store, m)?;
                bail!("이동 중 갈라진 대화가 나타나 원본으로 되돌렸습니다");
            }
        }
        m.phase = Phase::Decided;
        save(store, m)?;
        step(store, m, "decided")?;
    }
    if m.phase == Phase::Decided {
        source_rows_absent(m)?;
        for f in &m.files {
            if let Some(target) = &f.target
                && !target.exists()
            {
                let parent = target.parent().context(CONFLICT)?;
                let _parent = guard::prepare_parent(&m.target, parent)?;
                guard::rename_verified(&store.target, &f.stage, &m.target, target, f.identity)?;
                step(store, m, "published")?;
            }
        }
        m.phase = Phase::Published;
        save(store, m)?;
    }
    if m.phase == Phase::Published {
        db::apply(&m.database)?;
        db::verify(&m.database)?;
        m.phase = Phase::Rows;
        save(store, m)?;
        step(store, m, "target_rows")?;
    }
    if m.phase == Phase::Rows {
        verify_files(m)?;
        source_rows_absent(m)?;
        db::verify(&m.database)?;
        update_index(&m.target, &m.index, false)?;
        mapping::apply(&m.map)?;
        mapping::verify(&m.map)?;
        m.phase = Phase::Mapped;
        save(store, m)?;
        step(store, m, "map_committed")?;
    }
    if m.phase == Phase::Mapped {
        verify_files(m)?;
        source_rows_absent(m)?;
        db::verify(&m.database)?;
        mapping::verify(&m.map)?;
        m.phase = Phase::Complete;
        save(store, m)?;
        step(store, m, "committed")?;
    }
    Ok(())
}
fn run_plan(store: &Store, mut m: Move) -> Result<()> {
    let _admission = admission::exclusive(&m.homes)?;
    let _store = store.lock()?;
    ensure!(
        !pending(store)?,
        "미완료 이동이 있습니다. sessions recover로 먼저 복구하세요"
    );
    let names = m
        .files
        .iter()
        .flat_map(|f| f.sources.iter().cloned())
        .collect::<Vec<_>>();
    let _guards = guard::reserve_commit_exclusive(&m.homes, std::slice::from_ref(&m.id), &names)?;
    verify_files(&m)?;
    for home in &m.homes {
        if home != &m.target {
            descendants(home, &m.id)?;
        }
    }
    if let Some(previous) = load(store, &m.id)? {
        ensure!(
            !open(&previous),
            "미완료 이동이 있습니다. sessions recover로 먼저 복구하세요"
        );
        let archive = store.home.join("journal/session-move-history");
        let _parent = guard::prepare_parent(&store.home, &archive)?;
        let archive = fs::canonicalize(archive)?.join(format!(
            "{}-{}.json",
            m.id,
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        crate::cutover::atomic_bytes(&archive, &fs::read(path(store, &m.id))?)?;
        m.prior_journals = previous.prior_journals;
        m.prior_journals.push(archive);
        m.retained_stages = previous.retained_stages;
        m.retained_stages.extend(
            previous
                .files
                .into_iter()
                .map(|f| f.stage)
                .filter(|p| p.exists()),
        );
    }
    save(store, &m)?;
    step(store, &m, "prepared")?;
    execute(store, &mut m)
}
pub(super) fn import(
    store: &Store,
    catalog: &discovery::Catalog,
    id: &str,
    o: ImportOptions,
) -> Result<bool> {
    if let Some(m) = load(store, id)? {
        ensure!(
            !open(&m),
            "미완료 이동이 있습니다. sessions recover로 먼저 복구하세요"
        );
        if m.phase == Phase::Complete && m.target == store.target {
            ensure!(db::thread_path(&store.target, id)?.is_some(), "{CONFLICT}");
            return Ok(!o.skip_existing);
        }
    }
    let m = make_plan(
        store,
        &catalog.home,
        &store.target,
        id,
        false,
        &catalog.home,
    )?;
    if o.announce {
        super::preview(id, &super::migration_files(catalog, id)?, &store.target)?;
    }
    if o.apply {
        #[cfg(not(test))]
        admission::verify_installed_version()?;
        run_plan(store, m)?;
    }
    Ok(true)
}
fn preserve_evidence(store: &Store, m: &Move) -> Result<()> {
    let observations=m.files.iter().flat_map(|f| std::iter::once(&f.stage).chain(f.sources.iter()).chain(f.target.iter())).map(|p| {
        serde_json::json!({"path":p,"exists":p.exists(),"identity":guard::identity(p).ok(),"bytes":fs::metadata(p).ok().map(|v|v.len())})
    }).collect::<Vec<_>>();
    let rows = m
        .homes
        .iter()
        .filter_map(|h| db::source_snapshot(h, std::slice::from_ref(&m.id)).ok())
        .collect::<Vec<_>>();
    let payload = crate::cutover::protect(&serde_json::to_vec(
        &serde_json::json!({"journal":m,"observed":observations,"rows":rows}),
    )?)?;
    let dir = store.home.join("journal/session-moves-evidence");
    let _parent = guard::prepare_parent(&store.home, &dir)?;
    let p = fs::canonicalize(&dir)?.join(format!(
        "{}-{}.json",
        m.id,
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    crate::cutover::atomic_bytes(
        &p,
        &serde_json::to_vec(&serde_json::json!({"schema":1,"payload":payload}))?,
    )
}
fn abort_legitimate_source_append(store: &Store, m: &mut Move) -> Result<bool> {
    // Before the first source-name removal the sole public owner may legitimately
    // resume after a crash. Only verified append-only bytes accompanied by a
    // native operational row advance qualify. Never replay the old row snapshot.
    if m.phase > Phase::Staged
        || m.files.iter().any(|f| {
            f.target.as_ref().is_some_and(|p| p.exists()) || f.sources.iter().any(|p| !p.exists())
        })
    {
        return Ok(false);
    }
    // An append does not authorize clearing conflicting destination ownership.
    // Check before deleting staging or closing the durable admission barrier.
    let destination = db::source_snapshot(&m.target, std::slice::from_ref(&m.id))?;
    ensure!(
        destination
            .threads
            .iter()
            .all(|t| t.thread.is_none() && t.goal.is_none()),
        "{CONFLICT}"
    );
    let mut advanced = false;
    for snap in &m.source_db {
        let now = db::source_snapshot(&snap.target_home, std::slice::from_ref(&m.id))?;
        for (a, b) in now.threads.iter().zip(&snap.threads) {
            if a.goal.as_ref().map(|r| &r.values) != b.goal.as_ref().map(|r| &r.values) {
                return Ok(false);
            }
            match (&a.thread, &b.thread) {
                (Some(current), Some(old)) => {
                    if current.values.keys().ne(old.values.keys()) {
                        return Ok(false);
                    }
                    for (key, value) in &current.values {
                        if old.values.get(key) == Some(value) {
                            continue;
                        }
                        if !matches!(
                            key.as_str(),
                            "created_at"
                                | "updated_at"
                                | "tokens_used"
                                | "created_at_ms"
                                | "updated_at_ms"
                                | "recency_at"
                                | "recency_at_ms"
                        ) || !matches!(
                            (value, old.values.get(key)),
                            (db::DbValue::Integer(now), Some(db::DbValue::Integer(before))) if now > before
                        ) {
                            return Ok(false);
                        }
                        advanced = true;
                    }
                }
                (None, None) => {}
                _ => return Ok(false),
            }
        }
    }
    if !advanced {
        return Ok(false);
    }
    for f in &m.files {
        let p = &f.sources[0];
        if guard::identity(p)? != f.identity
            || fs::metadata(p)?.len() < f.bytes
            || guard::hardlink_count(p)? as usize != known_links(f)
        {
            return Ok(false);
        }
        let mut stream = File::open(p)?;
        let mut h = Sha256::new();
        let mut left = f.bytes;
        let mut buffer = [0u8; 65536];
        while left > 0 {
            let wanted = (left as usize).min(buffer.len());
            let count = stream.read(&mut buffer[..wanted])?;
            if count == 0 {
                return Ok(false);
            }
            h.update(&buffer[..count]);
            left -= count as u64;
        }
        if format!("{:x}", h.finalize()) != f.sha256 {
            return Ok(false);
        }
        let reader = std::io::BufReader::new(stream);
        use std::io::BufRead;
        for line in reader.lines() {
            if serde_json::from_str::<serde_json::Value>(&line?).is_err() {
                return Ok(false);
            }
        }
        for name in &f.sources {
            if guard::identity(name)? != f.identity {
                return Ok(false);
            }
        }
    }
    for f in &m.files {
        if f.stage.exists() {
            guard::unlink_counted(&store.target, &f.stage, f.identity, known_links(f) as u32)?;
        }
    }
    m.phase = Phase::Aborted;
    save(store, m)?;
    Ok(true)
}
fn adopt_legitimate_target_append(store: &Store, m: &mut Move) -> Result<bool> {
    if m.phase < Phase::Decided
        || m.files.iter().any(|f| {
            f.sources.iter().any(|p| p.exists()) || f.target.as_ref().is_some_and(|p| !p.exists())
        })
    {
        return Ok(false);
    }
    source_rows_absent(m)?;
    for home in &m.homes {
        ensure!(!found_descendants(m, home)?, "{CONFLICT}");
    }
    let current = db::source_snapshot(&m.target, std::slice::from_ref(&m.id))?;
    let mut advanced = false;
    for (now, old) in current.threads.iter().zip(&m.database.threads) {
        if now.goal.as_ref().map(|r| &r.values) != old.goal.as_ref().map(|r| &r.values) {
            return Ok(false);
        }
        let (Some(now), Some(old)) = (&now.thread, &old.thread) else {
            return Ok(false);
        };
        if now.values.keys().ne(old.values.keys()) {
            return Ok(false);
        }
        for (key, value) in &now.values {
            if old.values.get(key) == Some(value) {
                continue;
            }
            if !matches!(
                key.as_str(),
                "created_at"
                    | "updated_at"
                    | "tokens_used"
                    | "created_at_ms"
                    | "updated_at_ms"
                    | "recency_at"
                    | "recency_at_ms"
            ) || !matches!(
                (value, old.values.get(key)),
                (db::DbValue::Integer(now), Some(db::DbValue::Integer(before))) if now > before
            ) {
                return Ok(false);
            }
            advanced = true;
        }
    }
    if !advanced {
        return Ok(false);
    }
    for f in &m.files {
        let path = surviving(f)?;
        if guard::identity(path)? != f.identity
            || guard::hardlink_count(path)? as usize != known_links(f)
            || fs::metadata(path)?.len() < f.bytes
        {
            return Ok(false);
        }
        if f.target.is_none() {
            same(f, path)?;
            continue;
        }
        let mut file = File::open(path)?;
        let mut hash = Sha256::new();
        let mut remaining = f.bytes;
        let mut buffer = [0u8; 65536];
        while remaining > 0 {
            let wanted = (remaining as usize).min(buffer.len());
            let n = file.read(&mut buffer[..wanted])?;
            if n == 0 {
                return Ok(false);
            }
            hash.update(&buffer[..n]);
            remaining -= n as u64;
        }
        if format!("{:x}", hash.finalize()) != f.sha256 {
            return Ok(false);
        }
        use std::io::BufRead;
        for line in std::io::BufReader::new(file).lines() {
            if serde_json::from_str::<serde_json::Value>(&line?).is_err() {
                return Ok(false);
            }
        }
    }
    preserve_evidence(store, m)?;
    for f in &mut m.files {
        if let Some(path) = &f.target {
            f.bytes = fs::metadata(path)?.len();
            f.sha256 = hash(path)?;
        }
    }
    m.database = current;
    save(store, m)?;
    Ok(true)
}
pub(super) fn recover(store: &Store, id: &str) -> Result<()> {
    let mut m = load(store, id)?.context("복구할 이동 기록이 없습니다")?;
    if !open(&m) {
        return Ok(());
    }
    let _admission = admission::exclusive(&m.homes)?;
    let _store = store.lock()?;
    let names = m
        .files
        .iter()
        .flat_map(|f| {
            f.sources
                .iter()
                .chain(std::iter::once(&f.stage))
                .chain(f.target.iter())
        })
        .filter(|p| p.exists())
        .cloned()
        .collect::<Vec<_>>();
    let _guards = guard::reserve_commit_exclusive(&m.homes, std::slice::from_ref(&m.id), &names)?;
    let result = (|| {
        if verify_files(&m).is_err() {
            if abort_legitimate_source_append(store, &mut m)? {
                return Ok(());
            }
            ensure!(adopt_legitimate_target_append(store, &mut m)?, "{CONFLICT}");
        }
        if m.phase < Phase::Decided {
            rollback(store, &mut m)
        } else {
            execute(store, &mut m)
        }
    })();
    if result.is_err() {
        preserve_evidence(store, &m)?;
    }
    result
}
pub(super) fn return_current(
    store: &Store,
    id: &str,
    to: Option<&Path>,
    yes: bool,
    apply: bool,
) -> Result<()> {
    let previous = load(store, id)?;
    if previous.as_ref().is_some_and(open) {
        ensure!(
            apply,
            "미완료 이동이 있습니다. sessions recover로 먼저 복구하세요"
        );
        return recover(store, id);
    }
    let legacy = store.load()?;
    let origin = to
        .map(PathBuf::from)
        .or_else(|| previous.as_ref().map(|m| m.origin.clone()))
        .or_else(|| legacy.imports.get(id).map(|m| m.source_home.clone()))
        .context("돌려보낼 홈을 --to로 지정하세요")?;
    if let Some(m) = &previous
        && m.phase == Phase::Complete
        && m.target != store.target
    {
        ensure!(m.target == fs::canonicalize(&origin)?, "{CONFLICT}");
        return Ok(());
    }
    let m = make_plan(store, &store.target, &origin, id, true, &origin)?;
    crate::print_stdout(format_args!(
        "대화 {id}: 현재 qb 상태를 {}로 옮깁니다",
        origin.display()
    ))?;
    if !apply {
        return Ok(());
    }
    confirm::ask(
        yes,
        confirm::Mode::Move,
        false,
        "quota-baton sessions return <UUID> --to <HOME>",
    )?;
    #[cfg(not(test))]
    admission::verify_installed_version()?;
    run_plan(store, m)
}
pub(super) fn recorded(store: &Store, id: &str) -> Result<bool> {
    Ok(load(store, id)?.is_some())
}
pub(super) fn owner(store: &Store, id: &str) -> Result<Option<PathBuf>> {
    Ok(load(store, id)?.map(|m| {
        if m.phase == Phase::Complete {
            m.target
        } else {
            m.source
        }
    }))
}

#[cfg(test)]
#[path = "sessions_move_tests.rs"]
mod tests;
