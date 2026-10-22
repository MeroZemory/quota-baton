//! Metadata-only rollout discovery. Never retain or print conversation records.
//! Codex protocol::HistoryPosition.thread_id is a physical rollout id, whereas
//! SessionMeta.id stays stable across revert (protocol.rs:3132-3150).
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, SystemTime},
};

pub(crate) const ROLLOUT_PATH_ERROR: &str =
    "대화 기록 경로와 압축 파일을 확인할 수 없습니다. doctor로 확인하세요";

/// Codex keeps the canonical .jsonl name in SQLite after compression. Resolve
/// only its compressed sibling; never select between two existing names.
pub(crate) fn resolve_rollout_path(home: &Path, path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        home.join(path)
    };
    crate::sessions_guard::safe_path(home, home)?;
    // The database may use a different spelling of the same NTFS home.
    // Walk every component without reparses, then check canonical containment.
    crate::sessions_guard::safe_path(path.parent().context(ROLLOUT_PATH_ERROR)?, &path)?;
    let selected = if path.extension().is_some_and(|ext| ext == "jsonl") {
        let compressed = path.with_extension("jsonl.zst");
        crate::sessions_guard::safe_path(
            compressed.parent().context(ROLLOUT_PATH_ERROR)?,
            &compressed,
        )?;
        match (path.try_exists()?, compressed.try_exists()?) {
            (true, false) => path,
            (false, true) => compressed,
            _ => bail!(ROLLOUT_PATH_ERROR),
        }
    } else {
        path
    };
    let selected = fs::canonicalize(selected).context(ROLLOUT_PATH_ERROR)?;
    ensure!(
        selected.starts_with(fs::canonicalize(home)?),
        "{ROLLOUT_PATH_ERROR}"
    );
    Ok(selected)
}

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
    /// Nonprimary readable names: hardlink aliases or conflicting physical copies.
    pub aliases: Vec<Rollout>,
    pub conflicts: BTreeSet<String>,
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

#[derive(Debug)]
pub(crate) struct RelatedUnreadable {
    id: String,
    reason: &'static str,
}
impl std::fmt::Display for RelatedUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.id)
    }
}
impl std::error::Error for RelatedUnreadable {}

#[derive(Debug)]
pub(crate) struct PhysicalConflict {
    id: String,
}
impl std::fmt::Display for PhysicalConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "같은 대화 기록 ID가 서로 다른 파일에 중복되어 있습니다: {}",
            self.id
        )
    }
}
impl std::error::Error for PhysicalConflict {}

const HEADER_LIMIT: u64 = 4 * 1024 * 1024;
const COMPRESSED_LIMIT: u64 = 8 * 1024 * 1024;
const CACHE_HOME_LIMIT: usize = 64;
const CATALOG_NODE_LIMIT: usize = 100_000;

// Only successful, allowlisted header metadata is memoized. Each hit hashes the
// exact compressed input prefix observed by its original decoder, from a newly
// validated open file. Size/mtime/file-id equality alone never authorizes a hit.
// Names, aliases, graph closure, unreadable files and absence are always fresh.
const HEADER_CACHE_ENTRIES: usize = 8192;
const HEADER_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct CompressedHeader {
    input_bytes: u64,
    digest: [u8; 32],
    rollout: Rollout,
    cost: usize,
}

#[derive(Default)]
struct HeaderCache {
    entries: BTreeMap<PathBuf, CompressedHeader>,
    bytes: usize,
}

static COMPRESSED_HEADERS: std::sync::Mutex<HeaderCache> = std::sync::Mutex::new(HeaderCache {
    entries: BTreeMap::new(),
    bytes: 0,
});

fn with_headers<T>(operation: impl FnOnce(&mut HeaderCache) -> T) -> T {
    let mut cache = COMPRESSED_HEADERS.lock().unwrap_or_else(|poison| {
        let mut cache = poison.into_inner();
        *cache = HeaderCache::default();
        cache
    });
    operation(&mut cache)
}

struct HeaderInput<R> {
    inner: R,
    digest: Sha256,
    bytes: u64,
}

impl<R: Read> Read for HeaderInput<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        // Bound raw compressed read-ahead independently of decoded output.
        let capacity = output.len().min(8192);
        let count = self.inner.read(&mut output[..capacity])?;
        self.digest.update(&output[..count]);
        self.bytes += count as u64;
        Ok(count)
    }
}

fn cached_compressed(
    path: &Path,
    opened: &crate::sessions_guard::HeaderOpen,
) -> Result<Option<Rollout>> {
    let Some(entry) = with_headers(|cache| cache.entries.get(path).cloned()) else {
        return Ok(None);
    };
    let metadata = opened.file().metadata()?;
    let mut input = HeaderInput {
        inner: opened.file().take(entry.input_bytes),
        digest: Sha256::new(),
        bytes: 0,
    };
    let mut scratch = [0u8; 8192];
    while input.read(&mut scratch)? != 0 {}
    if input.bytes != entry.input_bytes || <[u8; 32]>::from(input.digest.finalize()) != entry.digest
    {
        opened.file().seek(SeekFrom::Start(0))?;
        return Ok(None);
    }
    let mut rollout = entry.rollout;
    rollout.bytes = metadata.len();
    if rollout.metadata.contains_key("updated_at") {
        rollout.metadata.insert(
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
    Ok(Some(rollout))
}

fn cache_compressed(path: &Path, rollout: &Rollout, input_bytes: u64, digest: [u8; 32]) {
    // Sizing is optional. Native non-Unicode ancestor names are valid paths;
    // serde's inability to encode them must not hide a successfully read header.
    let Ok(encoded) = serde_json::to_vec(rollout) else {
        return;
    };
    let cost = encoded.len() + 128;
    if cost > HEADER_CACHE_BYTES {
        return;
    }
    with_headers(|cache| {
        if let Some(old) = cache.entries.remove(path) {
            cache.bytes -= old.cost;
        }
        if cache.entries.len() >= HEADER_CACHE_ENTRIES || cache.bytes + cost > HEADER_CACHE_BYTES {
            // Bounded eviction only costs a fresh decode; it never hides a file.
            *cache = HeaderCache::default();
        }
        cache.bytes += cost;
        cache.entries.insert(
            path.to_owned(),
            CompressedHeader {
                input_bytes,
                digest,
                rollout: rollout.clone(),
                cost,
            },
        );
    });
}

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

fn read_meta(
    directory: &crate::sessions_guard::HeaderDirectory,
    path: &Path,
    relative: PathBuf,
    physical: String,
) -> Result<Rollout> {
    let opened = crate::sessions_guard::open_header(directory, path)?;
    let compressed = path.extension().is_some_and(|s| s == "zst");
    if compressed && let Some(mut rollout) = cached_compressed(path, &opened)? {
        rollout.relative = relative;
        rollout.physical = physical;
        return Ok(rollout);
    }
    let mut witness = None;
    let (line, metadata) = if compressed {
        let metadata = opened.file().metadata()?;
        let mut input = HeaderInput {
            inner: opened.file().take(COMPRESSED_LIMIT),
            digest: Sha256::new(),
            bytes: 0,
        };
        let mut decoder = zstd::stream::read::Decoder::new(&mut input)?.single_frame();
        decoder.window_log_max(23)?;
        let mut line = String::new();
        // No decoded read-ahead into conversation records. Input/window remain
        // bounded independently; only the first frame can provide metadata.
        let count =
            BufReader::with_capacity(1, decoder.take(HEADER_LIMIT + 1)).read_line(&mut line)?;
        ensure!(
            count as u64 <= HEADER_LIMIT && line.ends_with('\n'),
            "대화 메타데이터를 안전하게 읽을 수 없습니다"
        );
        witness = Some((input.bytes, <[u8; 32]>::from(input.digest.finalize())));
        (line, metadata)
    } else {
        let (line, metadata) = crate::sessions_plain_header::read(&opened, HEADER_LIMIT as usize)?;
        let line = String::from_utf8(line)
            .map_err(|_| anyhow::anyhow!("대화 메타데이터 형식이 올바르지 않습니다"))?;
        (line, metadata)
    };
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
    let rollout = Rollout {
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
    };
    if let Some((bytes, digest)) = witness {
        cache_compressed(path, &rollout, bytes, digest);
    }
    Ok(rollout)
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

struct HeaderJob {
    directory: crate::sessions_guard::HeaderDirectory,
    path: PathBuf,
    relative: PathBuf,
    physical: String,
    compressed: bool,
}

fn read_jobs(jobs: &[HeaderJob]) -> Result<Vec<Result<Rollout>>> {
    let read = |job: &HeaderJob| {
        read_meta(
            &job.directory,
            &job.path,
            job.relative.clone(),
            job.physical.clone(),
        )
    };
    if jobs.len() < 32 {
        return Ok(jobs.iter().map(read).collect());
    }
    // Four bounded readers; each owns its decoder/mapping. Join in enumeration
    // order so conflict/error classification is unchanged. No graph is shared.
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in jobs.chunks(jobs.len().div_ceil(4)) {
            handles.push(
                std::thread::Builder::new()
                    .name("qb-session-header".into())
                    .spawn_scoped(scope, move || chunk.iter().map(read).collect::<Vec<_>>())?,
            );
        }
        let mut results = Vec::with_capacity(jobs.len());
        for handle in handles {
            results.extend(
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("대화 메타데이터 작업을 완료하지 못했습니다"))?,
            );
        }
        Ok(results)
    })
}

fn scan_catalog(home: PathBuf) -> Result<Catalog> {
    let mut result = Catalog {
        home: home.clone(),
        rollouts: BTreeMap::new(),
        aliases: Vec::new(),
        conflicts: BTreeSet::new(),
        compressed: Vec::new(),
        unreadable: Vec::new(),
        threads: BTreeMap::new(),
    };
    let mut jobs = Vec::new();
    for top in ["sessions", "archived_sessions"] {
        let base = home.join(top);
        if !base.exists() {
            continue;
        }
        let mut pending = vec![(base.clone(), 0)];
        while let Some((dir, depth)) = pending.pop() {
            ensure!(depth <= 8, "대화 폴더 깊이가 너무 큽니다");
            ordinary(&dir)?;
            let directory = crate::sessions_guard::header_directory(&home, &dir)?;
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
                ensure!(jobs.len() < CATALOG_NODE_LIMIT, "대화 파일이 너무 많습니다");
                let relative = path.strip_prefix(&base)?.to_owned();
                jobs.push(HeaderJob {
                    directory: directory.clone(),
                    path,
                    relative,
                    physical,
                    compressed,
                });
            }
        }
    }
    for (job, parsed) in jobs.iter().zip(read_jobs(&jobs)?) {
        let physical = job.physical.clone();
        let item = match parsed {
            Ok(item) => item,
            Err(_) if job.compressed => {
                result.unreadable.push(UnreadableRollout {
                    path: job.path.clone(),
                    physical: Some(physical),
                    reason: "압축 대화 메타데이터를 안전하게 읽을 수 없습니다",
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let choices = result.threads.entry(item.thread.clone()).or_default();
        if !choices.contains(&physical) {
            choices.push(physical.clone());
        }
        if let Some(existing) = result.rollouts.get(&physical) {
            if crate::sessions_guard::identity(&existing.path)?
                != crate::sessions_guard::identity(&item.path)?
                || existing.thread != item.thread
            {
                result.conflicts.insert(physical);
            }
            result.aliases.push(item);
            continue;
        }
        result.rollouts.insert(physical, item);
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

    /// Refuse conflicting or unreadable names whose filename UUID is in the known
    /// graph. Unknown IDs do not imply a relationship. No rows are read here.
    pub(crate) fn ensure_readable_for(&self, ids: &BTreeSet<String>) -> Result<()> {
        let related = self.related_ids(ids);
        if let Some(id) = self.conflicts.intersection(&related).next() {
            return Err(PhysicalConflict { id: id.clone() }.into());
        }
        for item in &self.unreadable {
            if let Some(id) = &item.physical
                && related.contains(id)
            {
                return Err(RelatedUnreadable {
                    id: id.clone(),
                    reason: item.reason,
                }
                .into());
            }
        }
        Ok(())
    }

    /// Every readable name, including hardlink aliases and conflicting copies.
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
        self.ensure_readable_for(&BTreeSet::from([id.to_owned()]))?;
        if let Some(path) = crate::sessions_db::thread_path(&self.home, id)? {
            let path = resolve_rollout_path(&self.home, &path)?;
            ensure!(
                path.starts_with(&self.home),
                "대화 경로가 원본 홈 밖에 있습니다"
            );
            let physical = physical_id(&path).context("대화 기록 경로 형식이 올바르지 않습니다")?;
            // A failed header cannot connect its physical UUID to the logical
            // thread. The authoritative row supplies that relationship.
            self.ensure_readable_for(&BTreeSet::from([id.to_owned(), physical.clone()]))?;
            return self
                .all()
                .find(|r| r.physical == physical && r.path == path && r.thread == id)
                .context("대화 상태와 기록 경로가 다릅니다");
        }
        let choices = self
            .threads
            .get(id)
            .context("원본에서 대화를 찾지 못했습니다")?;
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
        crate::sessions_db::require_provider(item.provider.as_deref().unwrap_or("(없음)"))?;
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
        let found = catalog(home.path()).unwrap();
        assert_eq!(found.all().count(), 2);
        assert_eq!(found.conflicts, BTreeSet::from([A.into()]));
        assert!(found.active(A).err().unwrap().is::<PhysicalConflict>());
        assert!(found.chain(A).err().unwrap().is::<PhysicalConflict>());
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
