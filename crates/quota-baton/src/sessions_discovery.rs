//! Metadata-only rollout discovery. Never retain or print conversation records.
//! Codex protocol::HistoryPosition.thread_id is a physical rollout id, whereas
//! SessionMeta.id stays stable across revert (protocol.rs:3132-3150).
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, SystemTime},
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Rollout {
    pub path: PathBuf,
    pub relative: PathBuf,
    pub thread: String,
    pub physical: String,
    pub provider: Option<String>,
    pub history_base: Option<String>,
    pub forked_from: Option<String>,
    pub bytes: u64,
    pub metadata: BTreeMap<String, crate::sessions_db::DbValue>,
}

#[derive(Clone)]
pub(crate) struct Catalog {
    pub home: PathBuf,
    pub rollouts: BTreeMap<String, Rollout>,
    pub aliases: Vec<Rollout>,
    /// All compressed names, including successfully decoded headers (counts only).
    pub compressed: Vec<PathBuf>,
    pub unreadable: Vec<UnreadableRollout>,
    pub threads: BTreeMap<String, Vec<String>>,
}

/// No decoder errors or record contents are retained.
#[derive(Clone)]
pub(crate) struct UnreadableRollout {
    pub path: PathBuf,
    pub physical: Option<String>,
    pub reason: &'static str,
}

const HEADER_LIMIT: u64 = 4 * 1024 * 1024;
const COMPRESSED_LIMIT: u64 = 8 * 1024 * 1024;
const CACHE_HOME_LIMIT: usize = 64;
const CATALOG_NODE_LIMIT: usize = 100_000;

#[derive(Clone, Default)]
struct MetadataCache {
    homes: BTreeMap<PathBuf, Rc<Catalog>>,
    nodes: usize,
}

thread_local! {
    static READ_ONLY_CACHE: RefCell<Option<MetadataCache>> = const { RefCell::new(None) };
}

/// Lexically scoped, thread-bound metadata snapshots. Drop nested guards in
/// reverse creation order. Never hold this across a mutation or its preview.
#[must_use = "retain the guard for the read-only invocation"]
pub(crate) struct ReadOnlyCacheGuard {
    previous: Option<MetadataCache>,
    // Rc makes the guard !Send/!Sync, matching the thread-local cache.
    _thread: std::marker::PhantomData<Rc<()>>,
}

/// Opt in only for a read-only invocation (e.g. import --apply=false/list).
/// Nested scopes inherit snapshots, then roll back their additions on drop,
/// including unwinding. The outermost drop returns to uncached discovery.
/// Only successful catalogs are cached; rows and filesystem guards stay live.
pub(crate) fn read_only_cache() -> ReadOnlyCacheGuard {
    READ_ONLY_CACHE.with(|slot| {
        let mut state = slot.borrow_mut();
        let previous = state.take();
        *state = Some(previous.clone().unwrap_or_default());
        ReadOnlyCacheGuard {
            previous,
            _thread: std::marker::PhantomData,
        }
    })
}

impl Drop for ReadOnlyCacheGuard {
    fn drop(&mut self) {
        READ_ONLY_CACHE.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn ordinary(path: &Path) -> Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            m.file_attributes() & 0x400 == 0,
            "연결된 디렉터리나 파일은 가져올 수 없습니다"
        );
    }
    ensure!(
        !m.file_type().is_symlink(),
        "연결된 경로는 가져올 수 없습니다"
    );
    Ok(m)
}

fn physical_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let name = name
        .strip_suffix(".zst")
        .unwrap_or(name)
        .strip_suffix(".jsonl")?;
    if !name.starts_with("rollout-") || name.len() < 36 {
        return None;
    }
    let id = name.get(name.len() - 36..)?;
    valid_id(id).then(|| id.to_ascii_lowercase())
}

fn read_meta(path: &Path, relative: PathBuf, physical: String) -> Result<Rollout> {
    let metadata = ordinary(path)?;
    ensure!(metadata.is_file(), "대화 기록이 일반 파일이 아닙니다");
    let file = fs::File::open(path)?;
    let reader: Box<dyn Read> = if path.extension().is_some_and(|s| s == "zst") {
        let mut decoder =
            zstd::stream::read::Decoder::new(file.take(COMPRESSED_LIMIT))?.single_frame();
        decoder.window_log_max(23)?;
        Box::new(decoder)
    } else {
        Box::new(file)
    };
    let mut line = String::new();
    // Only the first record can be session_meta. Never scan assistant/user text.
    // Capacity one prevents decoded read-ahead into the next record. Decoder
    // input and window are bounded independently of the decoded header limit.
    let count = BufReader::with_capacity(1, reader.take(HEADER_LIMIT + 1)).read_line(&mut line)?;
    ensure!(
        count as u64 <= HEADER_LIMIT && line.ends_with('\n'),
        "대화 메타데이터를 안전하게 읽을 수 없습니다"
    );
    let value: Value = serde_json::from_str(&line)
        .map_err(|_| anyhow::anyhow!("대화 메타데이터 형식이 올바르지 않습니다"))?;
    ensure!(
        value.get("type").and_then(Value::as_str) == Some("session_meta"),
        "대화 메타데이터가 없습니다"
    );
    let meta = value.get("payload").context("대화 메타데이터가 없습니다")?;
    let thread = meta
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| valid_id(s))
        .context("대화 ID가 올바르지 않습니다")?
        .to_ascii_lowercase();
    let reference = |v: Option<&Value>| -> Result<Option<String>> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if valid_id(s) => Ok(Some(s.to_ascii_lowercase())),
            _ => bail!("대화 연결 ID가 올바르지 않습니다"),
        }
    };
    let mut fields = BTreeMap::new();
    for name in ["cwd", "source", "sandbox_policy", "approval_mode"] {
        if let Some(value) = meta.get(name).and_then(Value::as_str) {
            fields.insert(
                name.into(),
                crate::sessions_db::DbValue::Text(value.as_bytes().to_vec()),
            );
        }
    }
    if let Some(created) = meta
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    {
        fields.insert(
            "created_at".into(),
            crate::sessions_db::DbValue::Integer(created.timestamp()),
        );
        fields.insert(
            "updated_at".into(),
            crate::sessions_db::DbValue::Integer(
                metadata
                    .modified()?
                    .duration_since(SystemTime::UNIX_EPOCH)?
                    .as_secs()
                    .try_into()?,
            ),
        );
    }
    Ok(Rollout {
        path: path.to_owned(),
        relative,
        thread,
        physical,
        provider: meta
            .get("model_provider")
            .and_then(Value::as_str)
            .map(str::to_owned),
        history_base: reference(meta.get("history_base").and_then(|v| v.get("thread_id")))?,
        forked_from: reference(meta.get("forked_from_id"))?,
        bytes: metadata.len(),
        metadata: fields,
    })
}

pub(crate) fn catalog(home: &Path) -> Result<Catalog> {
    ensure!(ordinary(home)?.is_dir(), "원본 홈이 폴더가 아닙니다");
    let home = fs::canonicalize(home)?;
    if let Some(snapshot) = READ_ONLY_CACHE.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|cache| cache.homes.get(&home).cloned())
    }) {
        return Ok((*snapshot).clone());
    }
    let result = scan_catalog(home)?;
    READ_ONLY_CACHE.with(|slot| -> Result<()> {
        let mut state = slot.borrow_mut();
        if let Some(cache) = state.as_mut() {
            // Count every retained name/diagnostic, conservatively counting a
            // compressed name again when it also has a header or failure.
            let nodes = result.rollouts.len()
                + result.aliases.len()
                + result.compressed.len()
                + result.unreadable.len();
            ensure!(
                cache.homes.len() < CACHE_HOME_LIMIT,
                "읽기 전용 대화 캐시의 홈이 너무 많습니다"
            );
            ensure!(
                nodes <= CATALOG_NODE_LIMIT.saturating_sub(cache.nodes),
                "읽기 전용 대화 캐시의 파일이 너무 많습니다"
            );
            cache.nodes += nodes;
            cache
                .homes
                .insert(result.home.clone(), Rc::new(result.clone()));
        }
        Ok(())
    })?;
    Ok(result)
}

fn scan_catalog(home: PathBuf) -> Result<Catalog> {
    let mut result = Catalog {
        home: home.clone(),
        rollouts: BTreeMap::new(),
        aliases: Vec::new(),
        compressed: Vec::new(),
        unreadable: Vec::new(),
        threads: BTreeMap::new(),
    };
    for top in ["sessions", "archived_sessions"] {
        let base = home.join(top);
        if !base.exists() {
            continue;
        }
        let mut pending = vec![(base.clone(), 0)];
        while let Some((dir, depth)) = pending.pop() {
            ensure!(depth <= 8, "대화 폴더 깊이가 너무 큽니다");
            ordinary(&dir)?;
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let kind = ordinary(&path)?;
                if kind.is_dir() {
                    pending.push((path, depth + 1));
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let compressed = path.extension().is_some_and(|s| s == "zst");
                if compressed {
                    ensure!(
                        result.compressed.len() < CATALOG_NODE_LIMIT,
                        "대화 파일이 너무 많습니다"
                    );
                    result.compressed.push(path.clone());
                }
                let Some(physical) = physical_id(&path) else {
                    if compressed {
                        result.unreadable.push(UnreadableRollout {
                            path,
                            physical: None,
                            reason: "압축 대화 파일 이름에서 ID를 확인할 수 없습니다",
                        });
                    }
                    continue;
                };
                ensure!(
                    result.rollouts.len() + result.aliases.len() < CATALOG_NODE_LIMIT,
                    "대화 파일이 너무 많습니다"
                );
                let relative = path.strip_prefix(&base)?.to_owned();
                let item = match read_meta(&path, relative, physical.clone()) {
                    Ok(item) => item,
                    Err(_) if compressed => {
                        result.unreadable.push(UnreadableRollout {
                            path,
                            physical: Some(physical),
                            reason: "압축 대화 메타데이터를 안전하게 읽을 수 없습니다",
                        });
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if let Some(existing) = result.rollouts.get(&physical) {
                    ensure!(
                        crate::sessions_guard::identity(&existing.path)?
                            == crate::sessions_guard::identity(&item.path)?
                            && existing.thread == item.thread,
                        "같은 대화 기록 ID가 서로 다른 파일에 중복되어 있습니다: {}",
                        physical
                    );
                    result.aliases.push(item);
                    continue;
                }
                result
                    .threads
                    .entry(item.thread.clone())
                    .or_default()
                    .push(physical.clone());
                result.rollouts.insert(physical, item);
            }
        }
    }
    Ok(result)
}

impl Catalog {
    /// Expand known IDs through logical, physical, fork and history-base edges.
    /// Callers seed this with moving IDs plus IDs established by rows/references;
    /// for cross-home graphs, union and repeat across catalogs until stable.
    pub(crate) fn related_ids(&self, ids: &BTreeSet<String>) -> BTreeSet<String> {
        let mut edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for item in self.all() {
            for id in std::iter::once(item.physical.as_str())
                .chain(item.forked_from.as_deref())
                .chain(item.history_base.as_deref())
            {
                edges.entry(&item.thread).or_default().insert(id);
                edges.entry(id).or_default().insert(&item.thread);
            }
        }
        let mut found: BTreeSet<String> = ids.iter().map(|id| id.to_ascii_lowercase()).collect();
        let mut pending: Vec<_> = found.iter().cloned().collect();
        while let Some(id) = pending.pop() {
            if let Some(neighbors) = edges.get(id.as_str()) {
                for neighbor in neighbors {
                    if found.insert((*neighbor).to_owned()) {
                        pending.push((*neighbor).to_owned());
                    }
                }
            }
        }
        found
    }

    /// Refuse only unreadable names whose exact filename UUID is in the known
    /// graph. Unknown IDs do not imply a relationship. No rows are read here.
    pub(crate) fn ensure_readable_for(&self, ids: &BTreeSet<String>) -> Result<()> {
        let related = self.related_ids(ids);
        for item in &self.unreadable {
            if let Some(id) = &item.physical
                && related.contains(id)
            {
                bail!("{}: {}", item.reason, id);
            }
        }
        Ok(())
    }

    /// Every discoverable name, including hardlink aliases of a physical segment.
    pub(crate) fn all(&self) -> impl Iterator<Item = &Rollout> {
        self.rollouts.values().chain(self.aliases.iter())
    }

    /// Header-only logical descendants, including children with no SQLite rows.
    /// A history_base UUID identifies a physical segment, not a logical thread.
    pub(crate) fn descendants(&self, id: &str) -> BTreeSet<String> {
        let mut children: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for item in self.all() {
            let physical_parent = item
                .history_base
                .as_ref()
                .and_then(|base| self.rollouts.get(base))
                .map(|parent| parent.thread.as_str());
            for parent in item
                .forked_from
                .as_deref()
                .into_iter()
                .chain(physical_parent)
            {
                if parent != item.thread {
                    children.entry(parent).or_default().insert(&item.thread);
                }
            }
        }
        let root = id.to_ascii_lowercase();
        let mut seen = BTreeSet::from([root.clone()]);
        let mut pending = vec![root.clone()];
        while let Some(parent) = pending.pop() {
            if let Some(items) = children.get(parent.as_str()) {
                for child in items {
                    if seen.insert((*child).to_owned()) {
                        pending.push((*child).to_owned());
                    }
                }
            }
        }
        seen.remove(&root);
        seen
    }

    pub(crate) fn active(&self, id: &str) -> Result<&Rollout> {
        let choices = self
            .threads
            .get(id)
            .context("원본에서 대화를 찾지 못했습니다")?;
        if let Some(path) = crate::sessions_db::thread_path(&self.home, id)? {
            let path = fs::canonicalize(path).context("대화 기록 경로를 확인할 수 없습니다")?;
            ensure!(
                path.starts_with(&self.home),
                "대화 경로가 원본 홈 밖에 있습니다"
            );
            let physical = physical_id(&path).context("대화 기록 경로 형식이 올바르지 않습니다")?;
            return self
                .all()
                .find(|r| r.physical == physical && r.path == path && r.thread == id)
                .context("대화 상태와 기록 경로가 다릅니다");
        }
        ensure!(
            choices.len() == 1,
            "여러 대화 기록 중 활성 파일을 확인할 수 없습니다"
        );
        self.rollouts
            .get(&choices[0])
            .context("대화 기록이 없습니다")
    }

    pub fn chain(&self, id: &str) -> Result<Vec<Rollout>> {
        ensure!(valid_id(id), "대화 ID는 UUID여야 합니다");
        self.ensure_readable_for(&BTreeSet::from([id.to_owned()]))?;
        let mut done = BTreeSet::new();
        let mut visiting = BTreeSet::new();
        let mut result = Vec::new();
        self.visit(
            self.active(&id.to_ascii_lowercase())?,
            &mut done,
            &mut visiting,
            &mut result,
        )?;
        Ok(result)
    }

    fn visit(
        &self,
        item: &Rollout,
        done: &mut BTreeSet<String>,
        visiting: &mut BTreeSet<String>,
        result: &mut Vec<Rollout>,
    ) -> Result<()> {
        if done.contains(&item.physical) {
            return Ok(());
        }
        ensure!(
            visiting.insert(item.physical.clone()),
            "대화 연결이 순환합니다"
        );
        ensure!(visiting.len() <= 256, "대화 연결이 너무 깊습니다");
        ensure!(
            item.provider.as_deref() == Some("openai"),
            "openai 제공자의 대화만 가져올 수 있습니다"
        );
        if let Some(base) = &item.history_base {
            self.visit(
                self.rollouts
                    .get(base)
                    .context("이전 대화 기록 조각이 없습니다")?,
                done,
                visiting,
                result,
            )?;
        }
        if let Some(parent) = &item.forked_from
            && parent != &item.thread
        {
            self.visit(self.active(parent)?, done, visiting, result)?;
        }
        visiting.remove(&item.physical);
        done.insert(item.physical.clone());
        result.push(item.clone());
        Ok(())
    }
}

#[cfg(test)]
#[path = "sessions_compressed_tests.rs"]
mod compressed_tests;

pub(crate) fn refuse_recent(items: &[Rollout], now: SystemTime) -> Result<()> {
    for item in items {
        let modified = fs::metadata(&item.path)?.modified()?;
        ensure!(
            now.duration_since(modified).unwrap_or_default() >= Duration::from_secs(120),
            "최근에 사용한 대화입니다. 먼저 대화를 종료하고 120초 후 다시 시도하세요"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "11111111-1111-1111-1111-111111111111";
    const B: &str = "22222222-2222-2222-2222-222222222222";
    const C: &str = "33333333-3333-3333-3333-333333333333";
    const D: &str = "44444444-4444-4444-4444-444444444444";
    fn home() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r9-tests");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(fs::canonicalize(root).unwrap()).unwrap()
    }
    fn write(
        home: &Path,
        thread: &str,
        physical: &str,
        base: Option<&str>,
        parent: Option<&str>,
        archived: bool,
    ) -> PathBuf {
        let path = home
            .join(if archived {
                "archived_sessions"
            } else {
                "sessions/2026/10/21"
            })
            .join(format!("rollout-2026-10-21T00-00-00-{physical}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut value = serde_json::json!({"type":"session_meta","payload":{"id":thread,"model_provider":"openai"}});
        if let Some(base) = base {
            value["payload"]["history_base"] = serde_json::json!({"thread_id":base,"end_ordinal_exclusive":10,"end_byte_offset":100});
        }
        if let Some(parent) = parent {
            value["payload"]["forked_from_id"] = parent.into();
        }
        fs::write(&path,format!("{value}\n{{\"type\":\"event_msg\",\"payload\":\"SYNTHETIC-CONVERSATION-NOT-RETAINED\"}}\n")).unwrap();
        path
    }
    #[test]
    fn three_level_fork_has_archived_ancestor_and_physical_segments() {
        let home = home();
        write(home.path(), A, A, None, None, true);
        write(home.path(), B, B, Some(A), Some(A), false);
        write(home.path(), C, C, Some(B), Some(B), false);
        let catalog = catalog(home.path()).unwrap();
        let chain = catalog.chain(C).unwrap();
        assert_eq!(
            chain.iter().map(|r| r.thread.as_str()).collect::<Vec<_>>(),
            [A, B, C]
        );
        assert!(
            chain[0]
                .path
                .to_string_lossy()
                .contains("archived_sessions")
        );
        assert!(
            !serde_json::to_string(&chain)
                .unwrap()
                .contains("SYNTHETIC-CONVERSATION")
        );
    }
    #[test]
    fn missing_parent_cycle_unknown_provider_and_bad_id_are_refused() {
        let home = home();
        let path = write(home.path(), A, A, Some(B), None, false);
        assert!(catalog(home.path()).unwrap().chain(A).is_err());
        write(home.path(), B, B, Some(A), None, false);
        assert!(catalog(home.path()).unwrap().chain(A).is_err());
        fs::write(path,format!("{}\n",serde_json::json!({"type":"session_meta","payload":{"id":A,"model_provider":"other"}}))).unwrap();
        let catalog = catalog(home.path()).unwrap();
        assert!(catalog.chain(A).is_err());
        assert!(catalog.chain(D).is_err());
        assert!(catalog.chain("../auth.json").is_err());
    }
    #[test]
    fn recent_or_future_write_is_refused_old_record_is_allowed() {
        let home = home();
        let path = write(home.path(), A, A, None, None, false);
        let chain = catalog(home.path()).unwrap().chain(A).unwrap();
        assert!(refuse_recent(&chain, SystemTime::now()).is_err());
        let old = SystemTime::now() - Duration::from_secs(121);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        assert!(refuse_recent(&chain, SystemTime::now()).is_ok());
        assert!(refuse_recent(&chain, old - Duration::from_secs(1)).is_err());
    }
    #[test]
    fn malformed_filename_unicode_never_panics_and_ambiguous_segments_refuse() {
        let home = home();
        write(home.path(), A, A, None, None, false);
        write(home.path(), A, B, Some(A), None, false);
        assert!(catalog(home.path()).unwrap().chain(A).is_err());
        assert!(
            physical_id(Path::new(
                "rollout-가가가가가가가가가가가가가가가가가가가.jsonl"
            ))
            .is_none()
        );
        for id in [A, B, C] {
            assert!(valid_id(id));
        }
    }
    #[cfg(windows)]
    #[test]
    fn hardlink_aliases_are_retained_and_database_can_select_an_alias() {
        let home = home();
        let original = write(home.path(), A, A, None, None, false);
        let alias = home
            .path()
            .join("archived_sessions")
            .join(original.file_name().unwrap());
        fs::create_dir_all(alias.parent().unwrap()).unwrap();
        fs::hard_link(&original, &alias).unwrap();
        let found = catalog(home.path()).unwrap();
        assert_eq!(found.rollouts.len(), 1);
        assert_eq!(found.aliases.len(), 1);
        assert_eq!(found.all().count(), 2);
        assert_eq!(found.threads[A], [A]);
        assert_eq!(found.chain(A).unwrap().len(), 1);
        let selected = &found.aliases[0].path;
        let db = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL);")
            .unwrap();
        db.execute(
            "INSERT INTO threads VALUES(?1, ?2)",
            rusqlite::params![A, selected.to_str().unwrap()],
        )
        .unwrap();
        assert_eq!(
            &catalog(home.path()).unwrap().active(A).unwrap().path,
            selected
        );
    }

    #[cfg(windows)]
    #[test]
    fn equal_bytes_with_different_file_identity_are_not_aliases() {
        let home = home();
        write(home.path(), A, A, None, None, false);
        write(home.path(), A, A, None, None, true);
        assert!(catalog(home.path()).is_err());
    }

    #[test]
    fn descendants_follow_logical_forks_and_physical_bases_transitively_without_rows() {
        let home = home();
        write(home.path(), A, D, None, None, true);
        write(home.path(), B, B, Some(D), None, false);
        write(home.path(), C, C, None, Some(B), false);
        let found = catalog(home.path()).unwrap();
        assert_eq!(found.descendants(A), BTreeSet::from([B.into(), C.into()]));
        assert_eq!(found.descendants(B), BTreeSet::from([C.into()]));
        assert!(found.descendants(C).is_empty());
        assert!(!home.path().join("state_5.sqlite").exists());
        write(home.path(), A, D, None, Some(C), true);
        assert_eq!(
            catalog(home.path()).unwrap().descendants(A),
            BTreeSet::from([B.into(), C.into()])
        );
    }

    #[test]
    fn read_only_snapshot_is_stable_canonical_and_fresh_after_drop() {
        let home = home();
        let plain = write(home.path(), A, A, None, None, false);
        let compressed = plain.with_extension("jsonl.zst");
        fs::write(
            &compressed,
            zstd::stream::encode_all(fs::read(&plain).unwrap().as_slice(), 1).unwrap(),
        )
        .unwrap();
        fs::remove_file(plain).unwrap();
        let guard = read_only_cache();
        let mut first = catalog(home.path()).unwrap();
        assert_eq!(first.rollouts.len(), 1);
        // Returned catalogs are independent of the stored snapshot.
        first.rollouts.clear();
        fs::write(&compressed, b"synthetic corrupt compressed header").unwrap();
        write(home.path(), B, B, None, None, false);
        let again = catalog(&home.path().join(".")).unwrap();
        assert_eq!(again.rollouts.len(), 1);
        assert!(again.unreadable.is_empty());
        READ_ONLY_CACHE.with(|slot| assert_eq!(slot.borrow().as_ref().unwrap().homes.len(), 1));
        drop(guard);
        let fresh = catalog(home.path()).unwrap();
        assert!(fresh.threads.contains_key(B));
        assert!(!fresh.threads.contains_key(A));
        assert_eq!(fresh.unreadable.len(), 1);
        READ_ONLY_CACHE.with(|slot| assert!(slot.borrow().is_none()));
        // Default-off calls observe every subsequent filesystem change.
        write(home.path(), C, C, None, None, false);
        assert!(catalog(home.path()).unwrap().threads.contains_key(C));
    }

    #[test]
    fn nested_read_only_scope_inherits_then_rolls_back_added_snapshots() {
        let first = home();
        let second = home();
        write(first.path(), A, A, None, None, false);
        let outer = read_only_cache();
        catalog(first.path()).unwrap();
        write(first.path(), B, B, None, None, false);
        {
            let _inner = read_only_cache();
            assert!(!catalog(first.path()).unwrap().threads.contains_key(B));
            assert!(catalog(second.path()).unwrap().rollouts.is_empty());
            write(second.path(), C, C, None, None, false);
            assert!(catalog(second.path()).unwrap().rollouts.is_empty());
        }
        assert!(!catalog(first.path()).unwrap().threads.contains_key(B));
        assert!(catalog(second.path()).unwrap().threads.contains_key(C));
        drop(outer);
        assert!(catalog(first.path()).unwrap().threads.contains_key(B));
    }

    #[test]
    fn read_only_scope_unwinding_restores_outer_then_disables_cache() {
        let first = home();
        let second = home();
        let outer = read_only_cache();
        catalog(first.path()).unwrap();
        let unwind = std::panic::catch_unwind(|| {
            let _inner = read_only_cache();
            catalog(second.path()).unwrap();
            write(first.path(), A, A, None, None, false);
            write(second.path(), B, B, None, None, false);
            panic!("synthetic nested scope rollback");
        });
        assert!(unwind.is_err());
        assert!(catalog(first.path()).unwrap().rollouts.is_empty());
        assert!(catalog(second.path()).unwrap().threads.contains_key(B));
        drop(outer);
        let unwind = std::panic::catch_unwind(|| {
            let _guard = read_only_cache();
            catalog(first.path()).unwrap();
            write(first.path(), C, C, None, None, false);
            panic!("synthetic outer scope rollback");
        });
        assert!(unwind.is_err());
        READ_ONLY_CACHE.with(|slot| assert!(slot.borrow().is_none()));
        assert!(catalog(first.path()).unwrap().threads.contains_key(C));
    }

    #[test]
    fn failed_scan_is_not_cached_and_capacity_refusal_does_not_poison_scope() {
        let home = home();
        let path = write(home.path(), A, A, None, None, false);
        fs::write(&path, b"invalid first record\n").unwrap();
        let _guard = read_only_cache();
        assert!(catalog(home.path()).is_err());
        READ_ONLY_CACHE.with(|slot| assert!(slot.borrow().as_ref().unwrap().homes.is_empty()));
        write(home.path(), A, A, None, None, false);
        READ_ONLY_CACHE.with(|slot| slot.borrow_mut().as_mut().unwrap().nodes = CATALOG_NODE_LIMIT);
        assert!(catalog(home.path()).is_err());
        READ_ONLY_CACHE.with(|slot| {
            let mut state = slot.borrow_mut();
            let cache = state.as_mut().unwrap();
            assert!(cache.homes.is_empty());
            cache.nodes = 0;
        });
        assert!(catalog(home.path()).unwrap().threads.contains_key(A));
    }
}
