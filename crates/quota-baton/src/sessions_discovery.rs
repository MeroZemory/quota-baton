//! Metadata-only rollout discovery. Never retain or print conversation records.
//! Codex protocol::HistoryPosition.thread_id is a physical rollout id, whereas
//! SessionMeta.id stays stable across revert (protocol.rs:3132-3150).
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
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

pub(crate) struct Catalog {
    pub home: PathBuf,
    pub rollouts: BTreeMap<String, Rollout>,
    pub threads: BTreeMap<String, Vec<String>>,
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
    let name = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
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
    let mut line = String::new();
    // Only the first record can be session_meta. Never scan assistant/user text.
    let count = BufReader::new(file.take(4 * 1024 * 1024 + 1)).read_line(&mut line)?;
    ensure!(
        count <= 4 * 1024 * 1024 && line.ends_with('\n'),
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
    let mut result = Catalog {
        home: home.clone(),
        rollouts: BTreeMap::new(),
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
                if path.extension().is_some_and(|s| s == "zst") {
                    continue;
                }
                let Some(physical) = physical_id(&path) else {
                    continue;
                };
                ensure!(result.rollouts.len() < 100_000, "대화 파일이 너무 많습니다");
                let relative = path.strip_prefix(&base)?.to_owned();
                let item = read_meta(&path, relative, physical.clone())?;
                ensure!(
                    !result.rollouts.contains_key(&physical),
                    "같은 대화 기록 ID가 중복되어 있습니다"
                );
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
                .rollouts
                .get(&physical)
                .filter(|r| r.path == path && r.thread == id)
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
}
