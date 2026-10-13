use crate::accounts::Provider;
use axum::http::HeaderMap;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

const LEDGER_MAX_BYTES: u64 = 500_000_000;

#[derive(Clone, Debug)]
pub struct CaptureOptions {
    pub dir: PathBuf,
    pub limit: usize,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
            limit: 50,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub ts: String,
    pub req: String,
    pub family: String,
    pub route: String,
    pub transport: String,
    pub account: Option<String>,
    pub session: Option<String>,
    pub model: Option<String>,
    pub originator: Option<String>,
    pub ua: Option<String>,
    pub status: u16,
    pub upstream_status: Option<u16>,
    pub ttfb_ms: u64,
    pub dur_ms: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub attempts: u32,
    pub outcome: String,
    #[serde(default)]
    pub retained_copy: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct QuotaState {
    #[serde(default = "schema")]
    schema: u32,
    #[serde(default)]
    accounts: BTreeMap<String, Snapshot>,
}
fn schema() -> u32 {
    1
}

#[derive(Clone, Serialize, Deserialize)]
struct Window {
    name: String,
    used_pct: Option<f64>,
    reset_at: Option<DateTime<Utc>>,
    status: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    windows: Vec<Window>,
    plan: Option<String>,
    credits: Option<Credits>,
    source: String,
    observed_at: DateTime<Utc>,
    #[serde(default)]
    signals: Signals,
}
#[derive(Default, Serialize, Deserialize)]
struct Signals {
    unified_status: Option<String>,
    unified_reset_at: Option<DateTime<Utc>>,
    representative_claim: Option<String>,
    overage_status: Option<String>,
    retry_after_seconds: Option<f64>,
    limit_reached: Option<bool>,
    active_limit: Option<String>,
}
impl Signals {
    fn is_empty(&self) -> bool {
        self.unified_status.is_none()
            && self.unified_reset_at.is_none()
            && self.representative_claim.is_none()
            && self.overage_status.is_none()
            && self.retry_after_seconds.is_none()
            && self.limit_reached.is_none()
            && self.active_limit.is_none()
    }
    fn merge(&mut self, old: Self) {
        self.unified_status = self.unified_status.take().or(old.unified_status);
        self.unified_reset_at = self.unified_reset_at.or(old.unified_reset_at);
        self.representative_claim = self
            .representative_claim
            .take()
            .or(old.representative_claim);
        self.overage_status = self.overage_status.take().or(old.overage_status);
        self.retry_after_seconds = self.retry_after_seconds.or(old.retry_after_seconds);
        self.limit_reached = self.limit_reached.or(old.limit_reached);
        self.active_limit = self.active_limit.take().or(old.active_limit);
    }
}
#[derive(Serialize, Deserialize)]
struct Credits {
    has_credits: Option<bool>,
    unlimited: Option<bool>,
    balance: Option<f64>,
}

enum Command {
    Quota(String, Box<Snapshot>),
    Ledger(serde_json::Value),
    #[cfg(all(feature = "shape-capture", debug_assertions))]
    Capture(serde_json::Value),
    Flush(mpsc::Sender<Result<(), String>>),
    Stop,
}

pub struct Observability {
    tx: mpsc::SyncSender<Command>,
    worker: Option<thread::JoinHandle<()>>,
    #[cfg(all(feature = "shape-capture", debug_assertions))]
    capture: Option<crate::capture::Capture>,
}

impl Observability {
    pub fn new(home: &Path, capture: Option<CaptureOptions>) -> anyhow::Result<Self> {
        #[cfg(not(all(feature = "shape-capture", debug_assertions)))]
        anyhow::ensure!(
            capture.is_none(),
            "shape capture is unavailable in this build"
        );
        #[cfg(all(feature = "shape-capture", debug_assertions))]
        let capture = capture.map(crate::capture::Capture::new).transpose()?;
        fs::create_dir_all(home.join("state"))?;
        fs::create_dir_all(home.join("ledger"))?;
        retain(
            &home.join("ledger"),
            Utc::now().date_naive(),
            LEDGER_MAX_BYTES,
        )
        .map_err(|_| anyhow::anyhow!("ledger retention failed"))?;
        // 리스너를 열기 전에 기존 스냅샷을 검증하고 읽는다.
        let state = match fs::read(home.join("state/quota.json")) {
            Ok(bytes) => serde_json::from_slice::<QuotaState>(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid quota state"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => QuotaState {
                schema: 1,
                ..Default::default()
            },
            Err(_) => anyhow::bail!("quota state read failed"),
        };
        anyhow::ensure!(state.schema == 1, "unsupported quota state schema");
        // 디스크가 느릴 때 큐를 제한하고 대기하여 원장 항목을 버리지 않는다.
        let (tx, rx) = mpsc::sync_channel(256);
        let home = home.to_owned();
        let worker = thread::Builder::new()
            .name("qb-observability".into())
            .spawn(move || run(home, state, rx))?;
        Ok(Self {
            tx,
            worker: Some(worker),
            #[cfg(all(feature = "shape-capture", debug_assertions))]
            capture,
        })
    }

    pub fn observe_headers(&self, account_id: &str, provider: Provider, headers: &HeaderMap) {
        if let Some(snapshot) = snapshot(provider, headers, Utc::now()) {
            let _ = self
                .tx
                .send(Command::Quota(account_id.into(), Box::new(snapshot)));
        }
    }

    pub fn record_request(&self, mut record: RequestRecord) {
        // 쿼리와 원본 세션 식별자는 메타데이터 파일에도 남기지 않는다.
        record.route = record
            .route
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_owned();
        record.session = record
            .session
            .filter(|v| v.len() == 12 && v.bytes().all(|c| c.is_ascii_hexdigit()));
        record.ua = record.ua.as_deref().and_then(product);
        if let Ok(value) = serde_json::to_value(record) {
            let _ = self.tx.send(Command::Ledger(value));
        }
    }

    pub fn record_reload(
        &self,
        account_id: &str,
        trigger: &str,
        outcome: &str,
        expires_at: Option<DateTime<Utc>>,
    ) {
        let _ = self.tx.send(Command::Ledger(serde_json::json!({"ts": Utc::now().to_rfc3339(), "event": "linked_reload", "account": account_id, "trigger": trigger, "outcome": outcome, "expires_at": expires_at})));
    }

    pub fn capture(&self, headers: &HeaderMap, body: &[u8]) {
        #[cfg(all(feature = "shape-capture", debug_assertions))]
        if let Some(capture) = &self.capture
            && let Some(value) = capture.shape(headers, body)
        {
            let _ = self.tx.send(Command::Capture(value));
        }
        #[cfg(not(all(feature = "shape-capture", debug_assertions)))]
        let _ = (headers, body);
    }

    pub fn flush(&self) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Flush(tx))
            .map_err(|_| anyhow::anyhow!("observability worker unavailable"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("observability worker unavailable"))?
            .map_err(anyhow::Error::msg)
    }
}
impl Drop for Observability {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn ua_product(headers: &HeaderMap) -> Option<String> {
    header(headers, "user-agent").and_then(product)
}
fn product(value: &str) -> Option<String> {
    let value = value.split_whitespace().next()?;
    let (name, version) = value.split_once('/')?;
    let valid = |v: &str| {
        !v.is_empty()
            && v.len() <= 128
            && v.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-+".contains(&c))
    };
    (valid(name) && valid(version)).then(|| value.to_owned())
}
fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)?.to_str().ok()
}
fn number(h: &HeaderMap, name: &str) -> Option<f64> {
    header(h, name)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}
fn timestamp(h: &HeaderMap, name: &str) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(header(h, name)?.parse().ok()?, 0)
}
fn status(h: &HeaderMap, name: &str) -> Option<String> {
    header(h, name)
        .filter(|s| {
            matches!(
                *s,
                "allowed" | "allowed_warning" | "rejected" | "true" | "false"
            )
        })
        .map(str::to_owned)
}
fn snapshot(provider: Provider, h: &HeaderMap, now: DateTime<Utc>) -> Option<Snapshot> {
    let mut windows = Vec::new();
    let mut plan = None;
    let mut credits = None;
    let mut signals = Signals::default();
    match provider {
        Provider::Claude => {
            signals.unified_status = status(h, "anthropic-ratelimit-unified-status");
            signals.unified_reset_at = timestamp(h, "anthropic-ratelimit-unified-reset");
            signals.representative_claim =
                header(h, "anthropic-ratelimit-unified-representative-claim")
                    .filter(|v| {
                        matches!(
                            *v,
                            "5h" | "7d" | "7d_oi" | "five_hour" | "seven_day" | "seven_day_opus"
                        )
                    })
                    .map(str::to_owned);
            signals.overage_status = status(h, "anthropic-ratelimit-unified-overage-status");
            signals.retry_after_seconds = number(h, "retry-after").filter(|v| *v >= 0.0);
            for name in ["5h", "7d", "7d_oi"] {
                let prefix = format!("anthropic-ratelimit-unified-{name}");
                let used_pct = number(h, &format!("{prefix}-utilization"))
                    .filter(|v| (0.0..=1.0).contains(v))
                    .map(|v| v * 100.0);
                let reset_at = timestamp(h, &format!("{prefix}-reset"));
                let status = status(h, &format!("{prefix}-status"));
                if used_pct.is_some() || reset_at.is_some() || status.is_some() {
                    windows.push(Window {
                        name: name.into(),
                        used_pct,
                        reset_at,
                        status,
                    });
                }
            }
        }
        Provider::Codex => {
            signals.limit_reached = header(h, "x-codex-limit-reached").and_then(|v| v.parse().ok());
            signals.active_limit = header(h, "x-codex-active-limit")
                .filter(|v| matches!(*v, "primary" | "secondary" | "5h" | "7d"))
                .map(str::to_owned);
            for slot in ["primary", "secondary"] {
                let prefix = format!("x-codex-{slot}");
                let used_pct = number(h, &format!("{prefix}-used-percent"))
                    .filter(|v| (0.0..=100.0).contains(v));
                let minutes = header(h, &format!("{prefix}-window-minutes"))
                    .and_then(|v| v.parse::<u32>().ok());
                let name = match minutes {
                    Some(300) => "5h".into(),
                    Some(10080) => "7d".into(),
                    Some(n) if n > 0 => format!("{n}m"),
                    _ => slot.into(),
                };
                let reset_at = timestamp(h, &format!("{prefix}-reset-at")).or_else(|| {
                    header(h, &format!("{prefix}-reset-after-seconds"))
                        .and_then(|v| v.parse::<i64>().ok())
                        .filter(|v| *v >= 0)
                        .and_then(Duration::try_seconds)
                        .and_then(|d| now.checked_add_signed(d))
                });
                let status = status(h, "x-codex-limit-reached");
                if used_pct.is_some() || reset_at.is_some() {
                    windows.push(Window {
                        name,
                        used_pct,
                        reset_at,
                        status,
                    });
                }
            }
            plan = header(h, "x-codex-plan-type")
                .filter(|v| {
                    matches!(
                        *v,
                        "free" | "plus" | "pro" | "team" | "business" | "enterprise" | "edu" | "go"
                    )
                })
                .map(str::to_owned);
            let bool_header = |name| header(h, name).and_then(|v| v.parse::<bool>().ok());
            let c = Credits {
                has_credits: bool_header("x-codex-credits-has-credits"),
                unlimited: bool_header("x-codex-credits-unlimited"),
                balance: number(h, "x-codex-credits-balance"),
            };
            if c.has_credits.is_some() || c.unlimited.is_some() || c.balance.is_some() {
                credits = Some(c);
            }
        }
    }
    if windows.is_empty() && plan.is_none() && credits.is_none() && signals.is_empty() {
        return None;
    }
    Some(Snapshot {
        windows,
        plan,
        credits,
        source: "headers".into(),
        observed_at: now,
        signals,
    })
}

fn run(home: PathBuf, mut state: QuotaState, rx: mpsc::Receiver<Command>) {
    let mut error = None;
    for command in rx {
        let result = match command {
            Command::Quota(id, mut next) => {
                if let Some(old) = state.accounts.remove(&id) {
                    for mut window in old.windows {
                        if let Some(new) = next.windows.iter_mut().find(|w| w.name == window.name) {
                            new.used_pct = new.used_pct.or(window.used_pct);
                            new.reset_at = new.reset_at.or(window.reset_at);
                            new.status = new.status.take().or(window.status.take());
                        } else {
                            next.windows.push(window);
                        }
                    }
                    next.plan = next.plan.or(old.plan);
                    if let (Some(new), Some(old)) = (&mut next.credits, &old.credits) {
                        new.has_credits = new.has_credits.or(old.has_credits);
                        new.unlimited = new.unlimited.or(old.unlimited);
                        new.balance = new.balance.or(old.balance);
                    }
                    next.credits = next.credits.or(old.credits);
                    next.signals.merge(old.signals);
                }
                state.accounts.insert(id, *next);
                serde_json::to_vec(&state)
                    .map_err(std::io::Error::other)
                    .and_then(|bytes| atomic_write(&home.join("state/quota.json"), &bytes))
            }
            Command::Ledger(value) => append(
                &home
                    .join("ledger")
                    .join(format!("{}.jsonl", Utc::now().format("%Y-%m-%d"))),
                &value,
            )
            .and_then(|_| {
                retain(
                    &home.join("ledger"),
                    Utc::now().date_naive(),
                    LEDGER_MAX_BYTES,
                )
            }),
            #[cfg(all(feature = "shape-capture", debug_assertions))]
            Command::Capture(value) => {
                let path = value["destination"].as_str().map(PathBuf::from);
                match path {
                    Some(path) => append(&path, &value["shape"]),
                    None => Ok(()),
                }
            }
            Command::Flush(reply) => {
                let _ = reply.send(error.clone().map_or(Ok(()), Err));
                continue;
            }
            Command::Stop => break,
        };
        if result.is_err() {
            error = Some("observability persistence failed".to_owned());
        }
    }
}
fn append(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_data()
}
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("{:032x}.tmp", rand::random::<u128>()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::{
                Win32::Storage::FileSystem::{
                    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
                },
                core::PCWSTR,
            };
            let src: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
            let dst: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(src.as_ptr()),
                    PCWSTR(dst.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            }
            .map_err(std::io::Error::other)
        }
        #[cfg(not(windows))]
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
fn retain(dir: &Path, today: NaiveDate, cap: u64) -> std::io::Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("jsonl")
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        let Some(date) = path
            .file_stem()
            .and_then(|v| v.to_str())
            .and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok())
        else {
            continue;
        };
        if today.signed_duration_since(date).num_days() >= 30 {
            fs::remove_file(path)?;
        } else {
            files.push((date, path, entry.metadata()?.len()));
        }
    }
    files.sort_by_key(|v| v.0);
    let mut total: u64 = files.iter().map(|v| v.2).sum();
    for (_, path, size) in files {
        if total <= cap {
            break;
        }
        fs::remove_file(path)?;
        total -= size;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn directory() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/observability-tests");
        fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(base).unwrap()
    }

    fn record() -> RequestRecord {
        RequestRecord {
            ts: Utc::now().to_rfc3339(),
            req: "request-1".into(),
            family: "codex".into(),
            route: "POST /v1/responses?key=fake-access-marker".into(),
            transport: "http".into(),
            account: Some("cx-synthetic".into()),
            session: Some("fake-session-marker".into()),
            model: Some("synthetic-model".into()),
            originator: Some("sample_cli".into()),
            ua: Some("sample_cli/1.2 (fake-access-marker)".into()),
            status: 200,
            upstream_status: Some(200),
            ttfb_ms: 1,
            dur_ms: 2,
            bytes_in: 3,
            bytes_out: 4,
            attempts: 1,
            outcome: "ok".into(),
            retained_copy: true,
        }
    }

    #[test]
    fn passive_snapshots_restore_merge_and_ignore_unrelated_headers() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.25".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-reset",
            "2000000000".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-status",
            "allowed".parse().unwrap(),
        );
        h.insert(
            "authorization",
            "Bearer fake-access-marker".parse().unwrap(),
        );
        obs.observe_headers("cl-synthetic", Provider::Claude, &h);
        h.clear();
        h.insert("x-codex-primary-used-percent", "35.5".parse().unwrap());
        h.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        h.insert("x-codex-primary-reset-after-seconds", "60".parse().unwrap());
        h.insert("x-codex-secondary-window-minutes", "10080".parse().unwrap());
        h.insert("x-codex-secondary-used-percent", "99".parse().unwrap());
        h.insert("x-codex-plan-type", "plus".parse().unwrap());
        h.insert("x-codex-credits-balance", "12.5".parse().unwrap());
        obs.observe_headers("cx-synthetic", Provider::Codex, &h);
        obs.flush().unwrap();
        drop(obs);
        let obs = Observability::new(dir.path(), None).unwrap();
        h.clear();
        h.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        h.insert("x-codex-primary-used-percent", "36".parse().unwrap());
        obs.observe_headers("cx-synthetic", Provider::Codex, &h);
        obs.observe_headers("unrelated", Provider::Codex, &HeaderMap::new());
        obs.flush().unwrap();
        let text = fs::read_to_string(dir.path().join("state/quota.json")).unwrap();
        let state: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            state["accounts"]["cl-synthetic"]["windows"][0]["used_pct"],
            25.0
        );
        let cx = &state["accounts"]["cx-synthetic"];
        assert_eq!(cx["windows"][0]["name"], "5h");
        assert_eq!(cx["windows"][0]["used_pct"], 36.0);
        assert!(cx["windows"][0]["reset_at"].is_string());
        assert_eq!(cx["windows"][1]["name"], "7d");
        assert_eq!(cx["plan"], "plus");
        assert_eq!(cx["credits"]["balance"], 12.5);
        assert!(state["accounts"].get("unrelated").is_none());
        assert!(!text.contains("fake-access-marker"));
        assert!(!text.contains("health"));
    }

    #[test]
    fn ledger_fields_reload_secrecy_and_drop_drain() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        obs.record_request(record());
        let expiry = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
        obs.record_reload("cx-synthetic", "expiry", "unchanged", Some(expiry));
        obs.record_reload("cx-synthetic", "upstream_401", "link_stale", None);
        drop(obs);
        let path = fs::read_dir(dir.path().join("ledger"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = fs::read_to_string(path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|v| serde_json::from_str(v).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        for key in [
            "ts",
            "req",
            "family",
            "route",
            "transport",
            "account",
            "session",
            "model",
            "originator",
            "ua",
            "status",
            "upstream_status",
            "ttfb_ms",
            "dur_ms",
            "bytes_in",
            "bytes_out",
            "attempts",
            "outcome",
            "retained_copy",
        ] {
            assert!(lines[0].get(key).is_some(), "{key}");
        }
        assert_eq!(lines[0]["route"], "POST /v1/responses");
        assert_eq!(lines[0]["ua"], "sample_cli/1.2");
        assert_eq!(lines[0]["session"], Value::Null);
        assert_eq!(lines[1]["event"], "linked_reload");
        assert_eq!(lines[1]["expires_at"], json!(expiry));
        assert_eq!(lines[2]["trigger"], "upstream_401");
        assert_eq!(lines[2]["outcome"], "link_stale");
        assert_eq!(lines[2]["expires_at"], Value::Null);
        for marker in [
            "fake-access-marker",
            "fake-session-marker",
            "authorization",
            "access_token",
            "refresh_token",
        ] {
            assert!(!text.contains(marker), "{marker}");
        }
    }

    #[test]
    fn retention_expires_by_age_or_size_preserving_other_files() {
        let dir = directory();
        let today = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
        for (date, bytes) in [
            ("2026-09-13", 4),
            ("2026-10-11", 6),
            ("2026-10-12", 6),
            ("2026-10-13", 6),
        ] {
            fs::write(dir.path().join(format!("{date}.jsonl")), vec![b'a'; bytes]).unwrap();
        }
        fs::write(dir.path().join("unrelated.jsonl"), "keep").unwrap();
        retain(dir.path(), today, 12).unwrap();
        assert!(!dir.path().join("2026-09-13.jsonl").exists());
        assert!(!dir.path().join("2026-10-11.jsonl").exists());
        assert!(dir.path().join("2026-10-12.jsonl").exists());
        assert!(dir.path().join("2026-10-13.jsonl").exists());
        assert!(dir.path().join("unrelated.jsonl").exists());
        retain(dir.path(), today, 5).unwrap();
        assert!(!dir.path().join("2026-10-13.jsonl").exists());
        assert!(dir.path().join("unrelated.jsonl").exists());
    }

    #[test]
    fn global_signals_are_passive_typed_and_secret_values_rejected() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        let mut h = HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "2000000000"),
            ("anthropic-ratelimit-unified-representative-claim", "7d"),
            ("anthropic-ratelimit-unified-overage-status", "allowed"),
            ("retry-after", "12.5"),
        ] {
            h.insert(name, value.parse().unwrap());
        }
        obs.observe_headers("cl-global", Provider::Claude, &h);
        h.clear();
        h.insert("x-codex-active-limit", "secondary".parse().unwrap());
        obs.observe_headers("cx-active-only", Provider::Codex, &h);
        h.clear();
        h.insert("x-codex-limit-reached", "true".parse().unwrap());
        obs.observe_headers("cx-limit-only", Provider::Codex, &h);
        h.insert("x-codex-plan-type", "fake-access-marker".parse().unwrap());
        h.insert(
            "x-codex-active-limit",
            "fake-access-marker".parse().unwrap(),
        );
        obs.observe_headers("cx-limit-only", Provider::Codex, &h);
        h.clear();
        h.insert(
            "anthropic-ratelimit-unified-status",
            "fake-access-marker".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-representative-claim",
            "fake-access-marker".parse().unwrap(),
        );
        obs.observe_headers("invalid", Provider::Claude, &h);
        obs.flush().unwrap();
        let text = fs::read_to_string(dir.path().join("state/quota.json")).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let signals = &value["accounts"]["cl-global"]["signals"];
        assert_eq!(signals["unified_status"], "rejected");
        assert_eq!(
            signals["unified_reset_at"],
            json!(DateTime::from_timestamp(2_000_000_000, 0).unwrap())
        );
        assert_eq!(signals["representative_claim"], "7d");
        assert_eq!(signals["overage_status"], "allowed");
        assert_eq!(signals["retry_after_seconds"], 12.5);
        assert_eq!(
            value["accounts"]["cx-active-only"]["signals"]["active_limit"],
            "secondary"
        );
        assert_eq!(
            value["accounts"]["cx-limit-only"]["signals"]["limit_reached"],
            true
        );
        assert!(value["accounts"].get("invalid").is_none());
        assert!(!text.contains("fake-access-marker"));
        assert!(!text.contains("health"));
    }

    #[test]
    fn bounded_queue_keeps_every_request_and_flush_reports_safe_errors() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        for i in 0..600 {
            let mut record = record();
            record.req = format!("request-{i}");
            obs.record_request(record);
        }
        obs.flush().unwrap();
        let path = fs::read_dir(dir.path().join("ledger"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 600);
        let ids: std::collections::BTreeSet<_> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["req"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(ids.len(), 600);
        fs::create_dir(dir.path().join("state/quota.json")).unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-codex-limit-reached", "true".parse().unwrap());
        obs.observe_headers("cx-synthetic", Provider::Codex, &h);
        assert_eq!(
            obs.flush().unwrap_err().to_string(),
            "observability persistence failed"
        );
    }

    #[test]
    fn invalid_state_refuses_start_and_errors_are_sanitized() {
        let dir = directory();
        fs::create_dir(dir.path().join("state")).unwrap();
        fs::write(dir.path().join("state/quota.json"), "fake-access-marker").unwrap();
        let error = Observability::new(dir.path(), None)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(error, "invalid quota state");
        fs::write(
            dir.path().join("state/quota.json"),
            serde_json::to_vec(&json!({"schema": 2, "accounts": {}})).unwrap(),
        )
        .unwrap();
        assert!(Observability::new(dir.path(), None).is_err());
    }

    #[test]
    fn ua_is_only_product_and_version() {
        let mut h = HeaderMap::new();
        h.insert(
            "user-agent",
            "client/2.3 (private-installation) extra/4".parse().unwrap(),
        );
        assert_eq!(ua_product(&h).as_deref(), Some("client/2.3"));
        h.insert("user-agent", "Bearer fake-access-marker".parse().unwrap());
        assert_eq!(ua_product(&h), None);
    }

    #[cfg(not(feature = "shape-capture"))]
    #[test]
    fn default_build_refuses_capture_and_call_is_noop() {
        let dir = directory();
        assert!(
            Observability::new(
                dir.path(),
                Some(CaptureOptions {
                    dir: dir.path().into(),
                    limit: 50
                })
            )
            .is_err()
        );
        let obs = Observability::new(dir.path(), None).unwrap();
        obs.capture(&HeaderMap::new(), b"fake-access-marker");
        obs.flush().unwrap();
        assert!(!dir.path().join("captures").exists());
    }

    #[cfg(feature = "shape-capture")]
    #[test]
    fn capture_plain_and_zstd_keys_only_limit_and_secrecy() {
        let dir = directory();
        let obs = Observability::new(
            dir.path(),
            Some(CaptureOptions {
                dir: dir.path().into(),
                limit: 2,
            }),
        )
        .unwrap();
        let mut h = HeaderMap::new();
        for (name, value) in [
            ("authorization", "Bearer fake-access-marker"),
            ("x-api-key", "fake-key-marker"),
            ("session_id", "fake-session-marker"),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("originator", "sample_cli"),
            ("user-agent", "sample_cli/1.2 (fake-install-marker)"),
        ] {
            h.insert(name, value.parse().unwrap());
        }
        let body = br#"{"model":"fake-body-marker","messages":[{"role":"user","content":{"text":"fake-body-marker","fourth":{"hidden_key":"fake-body-marker"}}}],"session_id":"fake-session-marker"}"#;
        obs.capture(&h, body);
        let compressed = zstd::stream::encode_all(body.as_slice(), 1).unwrap();
        h.insert("content-encoding", "zstd".parse().unwrap());
        obs.capture(&h, &compressed);
        obs.capture(&h, &compressed);
        obs.flush().unwrap();
        let path = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .unwrap();
        let text = fs::read_to_string(path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["body_keys"], lines[1]["body_keys"]);
        assert!(
            lines[0]["body_keys"]
                .as_array()
                .unwrap()
                .contains(&json!("text"))
        );
        assert!(!text.contains("hidden_key"));
        assert_eq!(
            lines[0]["header_values"]["user-agent"],
            json!(["sample_cli/1.2"])
        );
        for marker in [
            "fake-access-marker",
            "fake-key-marker",
            "fake-session-marker",
            "fake-install-marker",
            "fake-body-marker",
        ] {
            assert!(!text.contains(marker), "{marker}");
        }
        assert_eq!(body[0], b'{');
    }
}
