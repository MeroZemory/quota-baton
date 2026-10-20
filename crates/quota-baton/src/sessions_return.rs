//! D21/Addenda 3-4: return current Q state, never an old import snapshot.
//! Caller supplies the global conflict inventory; this module rechecks its
//! recorded names, selected rows and root provenance under commit reservations.
//! Pending return journals must also be honored by launch/import entrypoints.
//! Recovery rolls forward using these exact intents, refusing changed data.

use super::{Store, db, discovery, guard, index_plan, source_has_dependents, update_index};
use crate::{sessions_confirm as confirm, sessions_map as mapping};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Prepared,
    Linked,
    Rows,
    Verified,
    Mapped,
    Retiring,
    Retired,
    Committed,
}

#[derive(Deserialize, Serialize)]
struct Name {
    source: PathBuf,
    target: PathBuf,
    identity: guard::FileIdentity,
    bytes: u64,
    modified_ns: u64,
    thread: String,
    retain_source: bool,
}

// No Debug: metadata and map plans contain private values. The whole record,
// including both complete DbPlans, is DPAPI protected before its first write.
#[derive(Deserialize, Serialize)]
struct Return {
    schema: u8,
    id: String,
    source: PathBuf,
    target: PathBuf,
    homes: Vec<PathBuf>,
    probe_names: Vec<PathBuf>,
    ids: Vec<String>,
    names: Vec<Name>,
    database: db::DbPlan,
    source_database: db::DbPlan,
    index: Vec<String>,
    map: mapping::MapPlan,
    import_hash: Option<String>,
    phase: Phase,
}

fn journal_path(store: &Store, id: &str) -> PathBuf {
    store
        .home
        .join("journal")
        .join(format!("sessions-return-{id}.json"))
}

fn save(store: &Store, record: &Return) -> Result<()> {
    let path = journal_path(store, &record.id);
    guard::safe_path(&store.home, &path)?;
    let payload = crate::cutover::protect(&serde_json::to_vec(record)?)?;
    ensure!(
        payload.len() < 64 * 1024 * 1024 - 1024,
        "돌려보내기 기록이 너무 큽니다"
    );
    crate::cutover::atomic_bytes(
        &path,
        &serde_json::to_vec(&serde_json::json!({"schema":1,"payload":payload}))?,
    )
}

fn load(store: &Store, id: &str) -> Result<Option<Return>> {
    let path = journal_path(store, id);
    guard::safe_path(&store.home, &path)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    ensure!(
        fs::metadata(&path)?.len() <= 64 * 1024 * 1024,
        "돌려보내기 기록이 너무 큽니다"
    );
    let envelope: serde_json::Value = serde_json::from_slice(&fs::read(path)?)
        .map_err(|_| anyhow::anyhow!("돌려보내기 기록을 읽을 수 없습니다"))?;
    ensure!(
        envelope["schema"] == 1,
        "지원하지 않는 돌려보내기 기록입니다"
    );
    let bytes = crate::cutover::unprotect(
        envelope["payload"]
            .as_str()
            .context("돌려보내기 보호 기록이 없습니다")?,
    )?;
    let record: Return = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("돌려보내기 기록을 읽을 수 없습니다"))?;
    validate(store, &record, id)?;
    Ok(Some(record))
}

/// Entry points call this before import or resume, even when the ordinary
/// import journal still marks the root complete. This check is read-only.
pub(super) fn ensure_not_pending(store: &Store, id: &str) -> Result<()> {
    ensure!(discovery::valid_id(id), "대화 ID는 UUID여야 합니다");
    ensure!(
        load(store, &id.to_ascii_lowercase())?
            .is_none_or(|record| record.phase == Phase::Committed),
        "중단된 돌려보내기가 있습니다. sessions return으로 먼저 완료하세요"
    );
    Ok(())
}

fn validate(store: &Store, record: &Return, id: &str) -> Result<()> {
    ensure!(
        record.schema == 1
            && record.id == id
            && record.source == store.target
            && record.target != record.source
            && record.target.is_absolute(),
        "돌려보내기 기록의 범위가 다릅니다"
    );
    ensure!(
        record.database.source_home == record.source
            && record.database.target_home == record.target
            && record.database.preserve_current_state
            && !record.database.is_source_snapshot
            && record.source_database.source_home == record.source
            && record.source_database.target_home == record.source
            && record.source_database.is_source_snapshot,
        "돌려보내기 메타데이터 기록의 범위가 다릅니다"
    );
    for plan in [&record.database, &record.source_database] {
        ensure!(
            plan.threads.len() == 1 && plan.threads[0].id == id,
            "돌려보내기는 요청한 대화 항목만 처리합니다"
        );
    }
    ensure!(
        record.ids.iter().any(|item| item == id) && !record.names.is_empty(),
        "돌려보내기 기록이 불완전합니다"
    );
    for name in &record.names {
        guard::safe_path(&record.source, &name.source)?;
        guard::safe_path(&record.target.join("sessions"), &name.target)?;
        ensure!(
            name.thread == id || name.retain_source,
            "조상 기록은 원본에 남겨야 합니다"
        );
    }
    for path in &record.probe_names {
        let home = record
            .homes
            .iter()
            .filter(|home| path.starts_with(home))
            .max_by_key(|home| home.components().count())
            .context("검사할 기록이 홈 밖에 있습니다")?;
        guard::safe_path(home, path)?;
    }
    Ok(())
}

fn fingerprint(entry: &super::Import) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(entry)?)))
}

fn modified(path: &Path) -> Result<u64> {
    fs::metadata(path)?
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos()
        .try_into()
        .context("기록 수정 시간을 확인할 수 없습니다")
}

fn check_name(name: &Name, path: &Path) -> Result<()> {
    ensure!(
        !guard::compressed_sibling(path)
            && guard::identity(path)? == name.identity
            && fs::metadata(path)?.len() == name.bytes
            && modified(path)? == name.modified_ns,
        "돌려보내기 중 기록이 변경되었습니다. 변경된 현재 상태는 덮어쓰지 않았습니다"
    );
    Ok(())
}

fn check_names(record: &Return) -> Result<()> {
    for name in &record.names {
        ensure!(
            !guard::compressed_sibling(&name.source) && !guard::compressed_sibling(&name.target),
            "압축된 기록과 함께 돌려보낼 수 없습니다"
        );
        if name.source.try_exists()? {
            check_name(name, &name.source)?;
        } else {
            ensure!(
                record.phase >= Phase::Retiring && !name.retain_source && name.thread == record.id,
                "돌려보낼 원본 기록이 없습니다"
            );
        }
        if name.target.try_exists()? {
            check_name(name, &name.target)?;
        } else {
            ensure!(
                record.phase == Phase::Prepared,
                "검증한 대상 기록이 없어졌습니다"
            );
        }
    }
    Ok(())
}

fn probes(record: &Return) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut names = record
        .names
        .iter()
        .flat_map(|name| [&name.source, &name.target])
        .filter(|path| path.exists())
        .cloned()
        .collect::<Vec<_>>();
    names.extend(
        record
            .probe_names
            .iter()
            .filter(|path| path.exists())
            .cloned(),
    );
    names.sort();
    names.dedup();
    (record.homes.clone(), names)
}

fn check_record_destination_inventory(record: &Return) -> Result<()> {
    // Q supplies the intended physical segments before retirement. During
    // recovery its rows/names may be gone, so use only the verified intended
    // target names, never arbitrary same-ID destination alternatives.
    let source_names = record.phase < Phase::Retiring;
    let home = if source_names {
        &record.source
    } else {
        &record.target
    };
    let catalog = discovery::catalog(home)?;
    let files = record
        .names
        .iter()
        .map(|name| {
            let path = if source_names {
                &name.source
            } else {
                &name.target
            };
            catalog
                .rollouts
                .values()
                .find(|file| &file.path == path)
                .cloned()
                .context("돌려보내기 기록 조각을 확인할 수 없습니다")
        })
        .collect::<Result<Vec<_>>>()?;
    super::check_destination_inventory(&record.source, &record.target, &files)
}

fn plan(store: &Store, id: &str, to: Option<&Path>, map_path: &Path) -> Result<Return> {
    let journal = store.load()?;
    let own = journal.imports.get(id);
    ensure!(
        own.is_none_or(|entry| entry.complete),
        "중단된 가져오기를 먼저 복구하세요"
    );
    let target = to
        .or_else(|| own.map(|entry| entry.source_home.as_path()))
        .context("원본 홈 기록이 없습니다. --to로 대상 홈을 지정하세요")?;
    guard::safe_path(target, target)?;
    ensure!(target.is_dir(), "돌려보낼 대상 홈이 없습니다");
    let target = fs::canonicalize(target)?;
    ensure!(target != store.target, "원본과 대상 홈이 같습니다");
    let catalog = discovery::catalog(&store.target)?;
    let mut files = catalog.chain(id)?;
    // Include inactive physical segments of this logical root too. They must
    // not leave a stale resumable root in Q after its row has moved elsewhere.
    for file in catalog.rollouts.values().filter(|file| file.thread == id) {
        if !files.iter().any(|existing| existing.path == file.path) {
            files.push(file.clone());
        }
    }
    discovery::refuse_recent(&files, SystemTime::now())?;
    ensure!(
        files.iter().all(|file| file
            .provider
            .as_deref()
            .is_none_or(|provider| provider == "openai")),
        "openai 공급자의 대화만 돌려보낼 수 있습니다"
    );
    super::check_destination_inventory(&store.target, &target, &files)?;
    let ids = files
        .iter()
        .map(|file| file.thread.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let retain_root = source_has_dependents(&catalog, id)?;
    let mut names = Vec::new();
    for file in &files {
        let destination = target.join("sessions").join(&file.relative);
        guard::safe_path(&target, &destination)?;
        let name = Name {
            source: file.path.clone(),
            target: destination,
            identity: guard::identity(&file.path)?,
            bytes: file.bytes,
            modified_ns: modified(&file.path)?,
            thread: file.thread.clone(),
            retain_source: file.thread != id || retain_root,
        };
        if name.target.try_exists()? {
            check_name(&name, &name.target)?;
        }
        names.push(name);
    }
    let database = db::plan_return(&store.target, &target, &[id.into()])?;
    ensure!(
        database.threads[0].thread.is_some(),
        "Q에 현재 대화 메타데이터가 없습니다"
    );
    let source_database = db::source_snapshot(&store.target, &[id.into()])?;
    let index = index_plan(&target, &database)?;
    let map = mapping::plan(
        map_path,
        id,
        &target,
        &store.home.join("journal/sessions-return-map"),
    )?;
    let mut homes = vec![store.target.clone(), target.clone()];
    let mut probe_names = Vec::new();
    // Probe every known matching home without asking a partially retired
    // catalog to choose an active segment. Recovery uses only recorded paths.
    for home in &store.search_homes {
        guard::safe_path(home, home)?;
        let home = fs::canonicalize(home)?;
        if !homes.contains(&home) {
            let other = discovery::catalog(&home)?;
            let extra = db::source_snapshot(&home, &[id.into()])?;
            ensure!(
                extra.threads[0].thread.is_none() && extra.threads[0].goal.is_none(),
                "다른 홈의 대화 사본을 먼저 전체 검증해야 합니다"
            );
            for file in other.rollouts.values().filter(|file| file.thread == id) {
                let identity = guard::identity(&file.path)?;
                ensure!(
                    names
                        .iter()
                        .any(|name| name.thread == id && name.identity == identity),
                    "다른 홈의 대화 사본을 먼저 전체 검증해야 합니다"
                );
            }
            let found = other
                .rollouts
                .values()
                .filter(|file| ids.contains(&file.thread))
                .map(|file| file.path.clone())
                .collect::<Vec<_>>();
            if !found.is_empty() {
                homes.push(home);
                probe_names.extend(found);
            }
        }
    }
    // Existing names at the destination can include pagination alternatives;
    // probe them all, without requiring an active destination row during recovery.
    let destination = discovery::catalog(&target)?;
    probe_names.extend(
        destination
            .rollouts
            .values()
            .filter(|file| ids.contains(&file.thread))
            .map(|file| file.path.clone()),
    );
    homes.sort();
    homes.dedup();
    Ok(Return {
        schema: 1,
        id: id.into(),
        source: store.target.clone(),
        target,
        homes,
        probe_names,
        ids,
        names,
        database,
        source_database,
        index,
        map,
        import_hash: own.map(fingerprint).transpose()?,
        phase: Phase::Prepared,
    })
}

fn preview(record: &Return) -> Result<()> {
    crate::print_stdout(format_args!(
        "대화 {}: {} → {}",
        record.id,
        record.source.display(),
        record.target.display()
    ))?;
    let bytes: u64 = record.names.iter().map(|name| name.bytes).sum();
    crate::print_stdout(format_args!(
        "현재 기록 {}개 · {bytes}바이트 · 복사 없이 연결합니다",
        record.names.len()
    ))?;
    for name in &record.names {
        crate::print_stdout(format_args!(
            "{} → {}",
            name.source.display(),
            name.target.display()
        ))?;
        if name.retain_source {
            crate::print_stdout(format_args!(
                "다른 대화에 필요한 원본 이름 유지: {}",
                name.source.display()
            ))?;
        }
    }
    for (id, _, status) in db::summaries(&record.database) {
        crate::print_stdout(format_args!(
            "현재 목표 {id}: {} (상태 유지)",
            super::goal_status(&status)
        ))?;
    }
    crate::print_stdout(format_args!(
        "검증 후 Q의 대화 항목을 제거합니다. 과거 스냅샷은 복원하지 않습니다."
    ))?;
    Ok(())
}

fn step(
    store: &Store,
    record: &mut Return,
    phase: Phase,
    hook: &mut dyn FnMut(&str) -> Result<()>,
    point: &str,
) -> Result<()> {
    record.phase = phase;
    save(store, record)?;
    hook(point)
}

fn execute(
    store: &Store,
    record: &mut Return,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    check_names(record)?;
    check_record_destination_inventory(record)?;
    if record.phase < Phase::Retiring {
        db::verify(&record.source_database)?;
    }
    if record.phase >= Phase::Verified {
        db::verify(&record.database)?;
    }
    if record.phase >= Phase::Mapped {
        mapping::verify(&record.map)?;
    }
    if record.phase < Phase::Linked {
        for name in &record.names {
            if !name.target.try_exists()? {
                fs::create_dir_all(name.target.parent().context("대상 폴더가 없습니다")?)?;
                guard::safe_path(&record.target, &name.target)?;
                check_name(name, &name.source)?;
                fs::hard_link(&name.source, &name.target)
                    .map_err(|_| anyhow::anyhow!("하드링크를 만들 수 없습니다. 같은 드라이브의 홈을 사용하세요. 기록은 복사하지 않았습니다"))?;
            }
            check_name(name, &name.target)?;
        }
        step(store, record, Phase::Linked, hook, "linked")?;
    }
    if record.phase < Phase::Rows {
        db::apply(&record.database)?;
        update_index(&record.target, &record.index, false)?;
        step(store, record, Phase::Rows, hook, "rows")?;
    }
    if record.phase < Phase::Verified {
        check_names(record)?;
        db::verify(&record.database)?;
        step(store, record, Phase::Verified, hook, "verified")?;
    }
    if record.phase < Phase::Mapped {
        check_names(record)?;
        db::verify(&record.database)?;
        mapping::apply(&record.map)?;
        mapping::verify(&record.map)?;
        step(store, record, Phase::Mapped, hook, "mapped")?;
    }
    if record.phase < Phase::Retiring {
        db::verify(&record.source_database)?;
        db::verify(&record.database)?;
        check_names(record)?;
        check_record_destination_inventory(record)?;
        if source_has_dependents(&discovery::catalog(&record.source)?, &record.id)? {
            for name in &mut record.names {
                if name.thread == record.id {
                    name.retain_source = true;
                }
            }
        }
        step(store, record, Phase::Retiring, hook, "retiring")?;
    }
    if record.phase == Phase::Retiring {
        db::verify(&record.database)?;
        check_names(record)?;
        db::remove_exact(&record.source_database)?;
        hook("source-rows-removed")?;
        for name in record
            .names
            .iter()
            .filter(|name| name.thread == record.id && !name.retain_source)
        {
            if name.source.try_exists()? {
                check_name(name, &name.source)?;
                check_name(name, &name.target)?;
                guard::remove_verified(&record.source, &name.source, name.identity)?;
                hook("source-name-removed")?;
            }
        }
        step(store, record, Phase::Retired, hook, "retired")?;
    }
    if record.phase == Phase::Retired {
        db::verify(&record.database)?;
        mapping::verify(&record.map)?;
        check_names(record)?;
        let source = db::source_snapshot(&record.source, std::slice::from_ref(&record.id))?;
        ensure!(
            source.threads[0].thread.is_none() && source.threads[0].goal.is_none(),
            "Q에 다른 현재 대화 항목이 생겼습니다"
        );
        let mut journal = store.load()?;
        if let Some(own) = journal.imports.get(&record.id) {
            ensure!(
                Some(fingerprint(own)?) == record.import_hash,
                "원본 가져오기 기록이 변경되었습니다"
            );
            journal.imports.remove(&record.id);
            store.save(&journal)?;
        }
        hook("import-removed")?;
        step(store, record, Phase::Committed, hook, "committed")?;
    }
    Ok(())
}

pub(super) fn run(store: &Store, id: &str, to: Option<&Path>, yes: bool) -> Result<()> {
    run_using(store, id, to, yes, &store.map_path, &mut |step| {
        super::checkpoint(store, id, &format!("return:{step}"))
    })
}

fn run_using(
    store: &Store,
    id: &str,
    to: Option<&Path>,
    yes: bool,
    map_path: &Path,
    hook: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    ensure!(discovery::valid_id(id), "대화 ID는 UUID여야 합니다");
    let id = id.to_ascii_lowercase();
    let existing = load(store, &id)?;
    let new_operation = existing
        .as_ref()
        .is_none_or(|record| record.phase == Phase::Committed);
    let mut record = match existing {
        Some(record) if record.phase != Phase::Committed => {
            if let Some(to) = to {
                ensure!(
                    fs::canonicalize(to)? == record.target,
                    "진행 중인 돌려보내기의 대상 홈이 다릅니다"
                );
            }
            record
        }
        Some(record) if db::thread_path(&store.target, &id)?.is_none() => {
            if let Some(to) = to {
                ensure!(
                    fs::canonicalize(to)? == record.target,
                    "완료한 돌려보내기의 대상 홈이 다릅니다"
                );
            }
            return Ok(crate::print_stdout(format_args!(
                "{id}: 이미 돌려보낸 대화입니다"
            ))?);
        }
        _ => plan(store, &id, to, map_path)?,
    };
    validate(store, &record, &id)?;
    let (homes, names) = probes(&record);
    let read_guards = guard::probe_all(&homes, &record.ids, &names)?;
    check_names(&record)?;
    check_record_destination_inventory(&record)?;
    preview(&record)?;
    let manual = format!(
        "quota-baton sessions return {id} --to \"{}\"",
        record.target.display()
    );
    confirm::ask(yes, confirm::Mode::Move, false, &manual)?;
    drop(read_guards);
    let _lock = store.lock()?;
    // Coordinator supplies this creating reservation API. It holds the Codex
    // coordination/thread locks in all homes, including missing target locks.
    let _guards = guard::reserve_commit(&homes, &record.ids, &names)?;
    let current = store.load()?;
    ensure!(
        current.imports.get(&id).map(fingerprint).transpose()? == record.import_hash
            || (record.phase >= Phase::Retired && !current.imports.contains_key(&id)),
        "원본 가져오기 기록이 변경되었습니다"
    );
    // Another return may have progressed while confirmation was pending. Never
    // replace its phase/snapshots with our earlier plan.
    if let Some(saved) = load(store, &id)? {
        ensure!(
            (new_operation && saved.phase == Phase::Committed)
                || serde_json::to_vec(&saved)? == serde_json::to_vec(&record)?,
            "다른 돌려보내기 작업이 진행되었습니다. 같은 명령을 다시 실행하세요"
        );
    }
    check_names(&record)?;
    check_record_destination_inventory(&record)?;
    if record.phase < Phase::Retiring {
        db::verify(&record.source_database)?;
    }
    save(store, &record)?;
    hook("prepared")?;
    execute(store, &mut record, hook)?;
    crate::print_stdout(format_args!(
        "대화 {id}를 {}로 돌려보냈습니다",
        record.target.display()
    ))?;
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::time::Duration;

    const ID: &str = "11111111-1111-1111-1111-111111111111";
    const OTHER: &str = "22222222-2222-2222-2222-222222222222";
    const SEGMENT: &str = "33333333-3333-3333-3333-333333333333";
    const OBJECTIVE: &str = "CURRENT_SYNTHETIC_GOAL_OBJECTIVE";

    struct Fixture {
        _root: tempfile::TempDir,
        store: Store,
        account: PathBuf,
        roots: Vec<PathBuf>,
        original_index: Vec<u8>,
        index_identity: guard::FileIdentity,
    }

    impl Fixture {
        fn new(dependent: bool, paginated: bool) -> Result<Self> {
            let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r9-return-tests");
            fs::create_dir_all(&base)?;
            let root = tempfile::tempdir_in(base.canonicalize()?)?;
            let control = root.path().join("control");
            let qb = root.path().join("Q");
            let account = root.path().join("account");
            for path in [control.join("journal"), qb.clone(), account.clone()] {
                fs::create_dir_all(path)?;
            }
            let qb = qb.canonicalize()?;
            let account = account.canonicalize()?;
            let map_path = root.path().join("appdata/herdr/session-homes.json");
            fs::create_dir_all(map_path.parent().context("map parent")?)?;
            fs::write(
                &map_path,
                serde_json::to_vec(
                    &serde_json::json!({"sessions":{(ID):&qb,(OTHER):&qb},"extra":"keep"}),
                )?,
            )?;
            let store = Store {
                home: control,
                target: qb.clone(),
                search_homes: Vec::new(),
                map_path,
            };
            let mut roots = vec![write_rollout(&qb, ID, ID, None, None)?];
            if paginated {
                roots.push(write_rollout(&qb, ID, SEGMENT, Some(ID), None)?);
            }
            let other = write_rollout(&qb, OTHER, OTHER, None, dependent.then_some(ID))?;
            let state = Connection::open(qb.join("state_5.sqlite"))?;
            state.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,model_provider TEXT NOT NULL,archived INTEGER NOT NULL DEFAULT 0)")?;
            for (id, path) in [(ID, roots.last().context("root segment")?), (OTHER, &other)] {
                state.execute(
                    "INSERT INTO threads VALUES(?1,?2,'openai',0)",
                    (id, path.to_str().context("path")?),
                )?;
            }
            let goals = Connection::open(qb.join("goals_1.sqlite"))?;
            goals.execute_batch("CREATE TABLE thread_goals(thread_id TEXT PRIMARY KEY,objective TEXT NOT NULL,status TEXT NOT NULL,tokens_used INTEGER NOT NULL,updated_at_ms INTEGER NOT NULL)")?;
            goals.execute(
                "INSERT INTO thread_goals VALUES(?1,?2,'active',42,44)",
                (ID, OBJECTIVE),
            )?;
            fs::write(
                qb.join("session_index.jsonl"),
                format!("{{\"id\":\"{ID}\",\"thread_name\":\"current synthetic title\"}}\n"),
            )?;
            let original_index =
                format!("{{\"id\":\"{OTHER}\",\"thread_name\":\"keep\"}}\n").into_bytes();
            fs::write(account.join("session_index.jsonl"), &original_index)?;
            let index_identity = guard::identity(&account.join("session_index.jsonl"))?;
            let mut journal = super::super::Journal {
                schema: 1,
                ..Default::default()
            };
            for id in [ID, OTHER] {
                let paths = if id == ID {
                    roots.clone()
                } else {
                    vec![other.clone()]
                };
                let links = paths
                    .iter()
                    .map(|path| {
                        Ok(super::super::Link {
                            source: account
                                .join("sessions")
                                .join(path.file_name().context("file name")?),
                            target: path.clone(),
                            identity: guard::identity(path)?,
                            length: fs::metadata(path)?.len(),
                            owned: true,
                            thread: id.into(),
                            moved: true,
                            retained_ancestor: false,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                journal.imports.insert(
                    id.into(),
                    super::super::Import {
                        source_home: account.clone(),
                        target_home: qb.clone(),
                        files: links,
                        threads: vec![id.into()],
                        // Return must never interpret either historical snapshot.
                        database: "old database deliberately not used".into(),
                        source_database: "old source deliberately not used".into(),
                        index: Vec::new(),
                        complete: true,
                        root: id.into(),
                        keep_source: false,
                        move_started: true,
                        move_done: true,
                        stale: Vec::new(),
                        session_map: None,
                    },
                );
            }
            store.save(&journal)?;
            Ok(Self {
                _root: root,
                store,
                account,
                roots,
                original_index,
                index_identity,
            })
        }

        fn run(&self, hook: &mut dyn FnMut(&str) -> Result<()>) -> Result<()> {
            run_using(&self.store, ID, None, true, &self.store.map_path, hook)
        }

        fn check_complete(&self, retained: bool) -> Result<()> {
            let record = load(&self.store, ID)?.context("return journal")?;
            assert!(record.phase == Phase::Committed);
            assert!(db::thread_path(&self.store.target, ID)?.is_none());
            assert!(db::thread_path(&self.store.target, OTHER)?.is_some());
            let current = db::source_snapshot(&self.account, &[ID.into()])?;
            let goal = current.threads[0].goal.as_ref().context("returned goal")?;
            assert!(goal.values["objective"] == db::DbValue::Text(OBJECTIVE.as_bytes().to_vec()));
            assert!(goal.values["status"] == db::DbValue::Text(b"active".to_vec()));
            assert!(goal.values["tokens_used"] == db::DbValue::Integer(42));
            assert!(goal.values["updated_at_ms"] == db::DbValue::Integer(44));
            for name in &record.names {
                assert!(guard::identity(&name.target)? == name.identity);
                if name.thread == ID {
                    assert_eq!(name.source.exists(), retained);
                }
            }
            let journal = self.store.load()?;
            assert!(!journal.imports.contains_key(ID));
            assert!(journal.imports.contains_key(OTHER));
            mapping::verify(&record.map)?;
            let index = fs::read(self.account.join("session_index.jsonl"))?;
            assert!(index.starts_with(&self.original_index));
            assert!(
                guard::identity(&self.account.join("session_index.jsonl"))? == self.index_identity
            );
            assert_eq!(
                String::from_utf8(index)?
                    .lines()
                    .filter(|line| line.contains(ID))
                    .count(),
                1
            );
            let protected = fs::read(journal_path(&self.store, ID))?;
            assert!(!String::from_utf8_lossy(&protected).contains(OBJECTIVE));
            for home in [&self.store.target, &self.account] {
                for name in ["state_5.sqlite", "goals_1.sqlite"] {
                    let db = Connection::open(home.join(name))?;
                    let integrity: String =
                        db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                    assert_eq!(integrity, "ok");
                }
            }
            Ok(())
        }
    }

    fn write_rollout(
        home: &Path,
        id: &str,
        physical: &str,
        base: Option<&str>,
        parent: Option<&str>,
    ) -> Result<PathBuf> {
        let path = home
            .join("sessions/2026/10/21")
            .join(format!("rollout-2026-10-21T00-00-00-{physical}.jsonl"));
        fs::create_dir_all(path.parent().context("rollout parent")?)?;
        let mut meta = serde_json::json!({"type":"session_meta","payload":{"id":id,"model_provider":"openai"}});
        if let Some(base) = base {
            meta["payload"]["history_base"] =
                serde_json::json!({"thread_id":base,"end_ordinal_exclusive":1,"end_byte_offset":0});
        }
        if let Some(parent) = parent {
            meta["payload"]["forked_from_id"] = parent.into();
        }
        fs::write(
            &path,
            format!(
                "{meta}\n{{\"type\":\"event_msg\",\"payload\":\"SYNTHETIC_BODY_NEVER_READ\"}}\n"
            ),
        )?;
        fs::OpenOptions::new().write(true).open(&path)?.set_times(
            fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(300)),
        )?;
        Ok(path)
    }

    #[test]
    fn return_replays_every_durable_phase_and_partial_paginated_source_retirement() -> Result<()> {
        for point in [
            "prepared",
            "linked",
            "rows",
            "verified",
            "mapped",
            "retiring",
            "source-rows-removed",
            "source-name-removed",
            "retired",
            "import-removed",
            "committed",
        ] {
            let f = Fixture::new(false, true)?;
            let mut interrupted = false;
            let result = f.run(&mut |phase| {
                if phase == point && !interrupted {
                    interrupted = true;
                    anyhow::bail!("synthetic interruption");
                }
                Ok(())
            });
            assert!(interrupted && result.is_err(), "missing checkpoint {point}");
            if point != "committed" {
                assert!(ensure_not_pending(&f.store, ID).is_err());
            }
            f.run(&mut |_| Ok(()))?;
            ensure_not_pending(&f.store, ID)?;
            f.check_complete(false)?;
        }
        Ok(())
    }

    #[test]
    fn return_retires_only_root_rows_and_keeps_q_names_needed_by_children() -> Result<()> {
        let f = Fixture::new(true, false)?;
        f.run(&mut |_| Ok(()))?;
        f.check_complete(true)?;
        assert!(f.roots.iter().all(|path| path.exists()));
        assert!(discovery::catalog(&f.store.target)?.chain(OTHER).is_ok());
        assert!(db::thread_path(&f.account, OTHER)?.is_none());
        Ok(())
    }

    #[test]
    fn interrupted_return_refuses_changed_destination_goal_without_replaying_old_data() -> Result<()>
    {
        let f = Fixture::new(false, false)?;
        assert!(
            f.run(&mut |phase| {
                if phase == "rows" {
                    anyhow::bail!("synthetic interruption");
                }
                Ok(())
            })
            .is_err()
        );
        let goals = Connection::open(f.account.join("goals_1.sqlite"))?;
        goals.execute(
            "UPDATE thread_goals SET objective='CHANGED_AFTER_INTERRUPTION'",
            [],
        )?;
        assert!(f.run(&mut |_| Ok(())).is_err());
        let goal = db::source_snapshot(&f.account, &[ID.into()])?
            .threads
            .remove(0)
            .goal
            .context("current goal")?;
        assert!(
            goal.values["objective"] == db::DbValue::Text(b"CHANGED_AFTER_INTERRUPTION".to_vec())
        );
        assert!(db::thread_path(&f.store.target, ID)?.is_some());
        assert!(f.roots[0].exists());
        assert!(f.store.load()?.imports.contains_key(ID));
        Ok(())
    }

    #[test]
    fn interrupted_return_refuses_changed_source_goal_without_restoring_snapshot() -> Result<()> {
        let f = Fixture::new(false, false)?;
        assert!(
            f.run(&mut |phase| {
                if phase == "linked" {
                    anyhow::bail!("synthetic interruption");
                }
                Ok(())
            })
            .is_err()
        );
        let goals = Connection::open(f.store.target.join("goals_1.sqlite"))?;
        goals.execute("UPDATE thread_goals SET tokens_used=99", [])?;
        assert!(f.run(&mut |_| Ok(())).is_err());
        assert!(!f.account.join("state_5.sqlite").exists());
        let current = db::source_snapshot(&f.store.target, &[ID.into()])?;
        assert!(
            current.threads[0]
                .goal
                .as_ref()
                .context("current goal")?
                .values["tokens_used"]
                == db::DbValue::Integer(99)
        );
        assert!(f.roots[0].exists());
        Ok(())
    }

    #[test]
    fn return_planning_creates_no_journal_locks_or_target_rows() -> Result<()> {
        let f = Fixture::new(false, false)?;
        let original = fs::read(f.store.path())?;
        ensure_not_pending(&f.store, ID)?;
        let record = plan(&f.store, ID, None, &f.store.map_path)?;
        let (homes, names) = probes(&record);
        let _read_only = guard::probe_all(&homes, &record.ids, &names)?;
        assert!(fs::read(f.store.path())? == original);
        assert!(!journal_path(&f.store, ID).exists());
        assert!(!f.store.target.join("thread-writer-locks").exists());
        assert!(!f.account.join("thread-writer-locks").exists());
        assert!(!f.account.join("state_5.sqlite").exists());
        assert!(f.roots[0].exists());
        Ok(())
    }

    #[test]
    fn return_refuses_unhandled_additional_root_rows_or_distinct_file_copies() -> Result<()> {
        for with_row in [false, true] {
            let mut f = Fixture::new(false, false)?;
            let third = f._root.path().join("third-account");
            fs::create_dir_all(&third)?;
            let third = third.canonicalize()?;
            let file = write_rollout(&third, ID, ID, None, None)?;
            if with_row {
                let state = Connection::open(third.join("state_5.sqlite"))?;
                state.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,model_provider TEXT NOT NULL,archived INTEGER NOT NULL DEFAULT 0)")?;
                state.execute(
                    "INSERT INTO threads VALUES(?1,?2,'openai',0)",
                    (ID, file.to_str().context("path")?),
                )?;
            }
            f.store.search_homes.push(third);
            assert!(plan(&f.store, ID, None, &f.store.map_path).is_err());
            assert!(!journal_path(&f.store, ID).exists());
            assert!(!f.account.join("state_5.sqlite").exists());
            assert!(db::thread_path(&f.store.target, ID)?.is_some());
            assert!(f.roots[0].exists());
        }
        Ok(())
    }

    #[test]
    fn return_refuses_archived_or_alternate_destination_versions_without_rows_before_mutation()
    -> Result<()> {
        use std::io::Write;
        for paginated in [false, true] {
            for archived in [false, true] {
                let f = Fixture::new(false, paginated)?;
                // In the paginated case ID is the prefix, not the active head
                // SEGMENT. Comparing only the destination's head misses it.
                let incoming = write_rollout(&f.account, ID, ID, None, None)?;
                let directory = f.account.join(if archived {
                    "archived_sessions/2024/01/01"
                } else {
                    "sessions/2024/01/01"
                });
                fs::create_dir_all(&directory)?;
                let conflict = directory.join(format!("rollout-2024-01-01T00-00-00-{ID}.jsonl"));
                fs::rename(incoming, &conflict)?;
                fs::OpenOptions::new()
                    .append(true)
                    .open(&conflict)?
                    .write_all(
                        b"{\"type\":\"event_msg\",\"payload\":\"DIFFERENT_SYNTHETIC_CONTENT\"}\n",
                    )?;
                let original_conflict = fs::read(&conflict)?;
                let original_import = fs::read(f.store.path())?;
                let original_map = fs::read(&f.store.map_path)?;
                let original_state = fs::read(f.store.target.join("state_5.sqlite"))?;
                let original_goals = fs::read(f.store.target.join("goals_1.sqlite"))?;
                assert!(!f.account.join("state_5.sqlite").exists());

                let error = f
                    .run(&mut |_| Ok(()))
                    .err()
                    .context("conflicting destination accepted")?;
                assert!(
                    error
                        .downcast_ref::<super::super::InventoryConflict>()
                        .is_some()
                );
                assert!(fs::read(f.store.path())? == original_import);
                assert!(fs::read(&f.store.map_path)? == original_map);
                assert!(fs::read(f.store.target.join("state_5.sqlite"))? == original_state);
                assert!(fs::read(f.store.target.join("goals_1.sqlite"))? == original_goals);
                assert!(fs::read(&conflict)? == original_conflict);
                assert!(fs::read(f.account.join("session_index.jsonl"))? == f.original_index);
                assert!(!journal_path(&f.store, ID).exists());
                assert!(!f.account.join("state_5.sqlite").exists());
                assert!(!f.account.join("goals_1.sqlite").exists());
                assert!(!f.store.home.join("journal/sessions-import.lock").exists());
                assert!(!f.store.target.join("thread-writer-locks").exists());
                assert!(!f.account.join("thread-writer-locks").exists());
                for path in &f.roots {
                    let relative = path.strip_prefix(f.store.target.join("sessions"))?;
                    assert!(path.exists());
                    assert!(!f.account.join("sessions").join(relative).exists());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn return_inventory_accepts_exact_intended_hardlinks_for_each_paginated_segment() -> Result<()>
    {
        let f = Fixture::new(false, true)?;
        for source in &f.roots {
            let relative = source.strip_prefix(f.store.target.join("sessions"))?;
            let target = f.account.join("sessions").join(relative);
            fs::create_dir_all(target.parent().context("segment parent")?)?;
            fs::hard_link(source, target)?;
        }
        assert!(!f.account.join("state_5.sqlite").exists());
        f.run(&mut |_| Ok(()))?;
        f.check_complete(false)?;
        Ok(())
    }

    #[test]
    fn return_inventory_refuses_equal_content_hardlink_at_an_extra_archived_name() -> Result<()> {
        let f = Fixture::new(false, false)?;
        let directory = f.account.join("archived_sessions/2024/01/01");
        fs::create_dir_all(&directory)?;
        let alias = directory.join(format!("rollout-2024-01-01T00-00-00-{ID}.jsonl"));
        fs::hard_link(&f.roots[0], &alias)?;
        let identity = guard::identity(&alias)?;
        let error = f
            .run(&mut |_| Ok(()))
            .err()
            .context("extra destination alias accepted")?;
        assert!(
            error
                .downcast_ref::<super::super::InventoryConflict>()
                .is_some()
        );
        assert!(guard::identity(&alias)? == identity);
        assert!(f.roots[0].exists());
        assert!(!f.account.join("state_5.sqlite").exists());
        assert!(!journal_path(&f.store, ID).exists());
        Ok(())
    }
}
