use crate::accounts::Provider;
use axum::http::HeaderMap;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration as StdDuration, Instant},
};

const LEDGER_MAX_BYTES: u64 = 500_000_000;
const SYNC_INTERVAL: StdDuration = StdDuration::from_secs(1);
const WARN_INTERVAL: StdDuration = StdDuration::from_secs(60);

pub fn safe_identifier(text: &str) -> Option<String> {
    if text.is_empty()
        || text.len() > 100
        || text.contains("sk-")
        || text.contains("eyJ")
        || !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return None;
    }
    Some(text.to_owned())
}

pub fn safe_error_code(event: &serde_json::Value) -> Option<String> {
    let root = event.get("response").unwrap_or(event);
    let error = root.get("error").unwrap_or(root);
    error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .or_else(|| error.get("type").and_then(serde_json::Value::as_str))
        .and_then(safe_identifier)
}

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
    #[serde(default)]
    pub upstream_error_code: Option<String>,
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
    dropped: Arc<AtomicU64>,
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
        prune(&home.join("ledger"));
        // 손상된 캐시는 격리하고 빈 상태로 시작한다. 원문 오류는 로그에 남기지 않는다.
        let path = home.join("state/quota.json");
        let empty = || QuotaState {
            schema: 1,
            ..Default::default()
        };
        let state = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<QuotaState>(&bytes) {
                Ok(state) if state.schema == 1 => state,
                _ => {
                    let backup = path.with_file_name(format!(
                        "quota.json.corrupt-{}-{:08x}",
                        Utc::now().timestamp_millis(),
                        rand::random::<u32>()
                    ));
                    if fs::rename(&path, backup).is_err() {
                        tracing::warn!("Corrupt quota state could not be renamed; starting empty");
                    } else {
                        tracing::warn!("Corrupt quota state renamed; starting empty");
                    }
                    empty()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => empty(),
            Err(_) => {
                tracing::warn!("Quota state could not be read; starting empty");
                empty()
            }
        };
        // 요청 경로에서는 큐가 가득 차도 디스크나 작성자를 기다리지 않는다.
        let (tx, rx) = mpsc::sync_channel(256);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = dropped.clone();
        let home = home.to_owned();
        let worker = thread::Builder::new()
            .name("qb-observability".into())
            .spawn(move || run(home, state, rx, worker_dropped))?;
        Ok(Self {
            tx,
            worker: Some(worker),
            dropped,
            #[cfg(all(feature = "shape-capture", debug_assertions))]
            capture,
        })
    }

    pub fn observe_headers(&self, account_id: &str, provider: Provider, headers: &HeaderMap) {
        if let Some(snapshot) = snapshot(provider, headers, Utc::now()) {
            self.enqueue(Command::Quota(account_id.into(), Box::new(snapshot)));
        }
    }

    pub fn observe_codex_event(&self, account_id: &str, event: &serde_json::Value) {
        if event.get("type").and_then(serde_json::Value::as_str) != Some("codex.rate_limits") {
            return;
        }
        let limits = event.get("rate_limits").unwrap_or(event);
        let mut headers = HeaderMap::new();
        let mut add = |name: String, value: &serde_json::Value| {
            let text = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            if let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_str(&text),
            ) {
                headers.insert(name, value);
            }
        };
        for slot in ["primary", "secondary"] {
            if let Some(window) = limits.get(slot) {
                for key in [
                    "used_percent",
                    "window_minutes",
                    "reset_after_seconds",
                    "reset_at",
                ] {
                    if let Some(value) = window.get(key) {
                        add(format!("x-codex-{slot}-{}", key.replace('_', "-")), value);
                    }
                }
            }
        }
        for key in ["limit_reached", "plan_type"] {
            if let Some(value) = event.get(key).or_else(|| limits.get(key)) {
                add(format!("x-codex-{}", key.replace('_', "-")), value);
            }
        }
        if let Some(credits) = event.get("credits").or_else(|| limits.get("credits")) {
            for key in ["has_credits", "unlimited", "balance"] {
                if let Some(value) = credits.get(key) {
                    add(format!("x-codex-credits-{}", key.replace('_', "-")), value);
                }
            }
        }
        if let Some(mut snapshot) = snapshot(Provider::Codex, &headers, Utc::now()) {
            snapshot.source = "codex.rate_limits".into();
            self.enqueue(Command::Quota(account_id.into(), Box::new(snapshot)));
        }
    }

    pub fn record_ws_connection(
        &self,
        connection: &str,
        account: &str,
        event: &str,
        code: Option<u16>,
        reason: &str,
    ) {
        self.enqueue(Command::Ledger(serde_json::json!({"ts":Utc::now().to_rfc3339(),"event":event,"transport":"ws","connection":connection,"account":account,"close_code":code,"reason":reason})));
    }

    pub fn capture_ws_response(&self, ticket: Option<&Path>, headers: &HeaderMap) {
        #[cfg(all(feature = "shape-capture", debug_assertions))]
        if let Some(destination) = ticket {
            let mut values = BTreeMap::new();
            for name in headers.keys().filter(|name| {
                name.as_str().starts_with("x-codex-")
                    || name.as_str().starts_with("openai-")
                    || name.as_str() == "x-request-id"
            }) {
                let entries: Vec<_> = headers
                    .get_all(name)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .filter(|v| v.len() <= 4096)
                    .collect();
                values.insert(name.as_str(), entries);
            }
            self.enqueue(Command::Capture(serde_json::json!({"destination":destination,"shape":{"response_header_names":headers.keys().map(|n|n.as_str()).collect::<Vec<_>>(),"response_header_values":values}})));
        }
        #[cfg(not(all(feature = "shape-capture", debug_assertions)))]
        let _ = (ticket, headers);
    }

    pub fn capture_ws_types(&self, ticket: Option<&Path>, counts: &BTreeMap<String, u64>) {
        #[cfg(all(feature = "shape-capture", debug_assertions))]
        if let Some(destination) = ticket {
            self.enqueue(Command::Capture(
                serde_json::json!({"destination":destination,"shape":{"message_types":counts}}),
            ));
        }
        #[cfg(not(all(feature = "shape-capture", debug_assertions)))]
        let _ = (ticket, counts);
    }

    pub fn record_request(&self, mut record: RequestRecord) {
        record.upstream_error_code = record
            .upstream_error_code
            .as_deref()
            .and_then(safe_identifier);
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
            self.enqueue(Command::Ledger(value));
        }
    }

    pub fn record_reload(
        &self,
        account_id: &str,
        trigger: &str,
        outcome: &str,
        expires_at: Option<DateTime<Utc>>,
    ) {
        self.enqueue(Command::Ledger(serde_json::json!({"ts": Utc::now().to_rfc3339(), "event": "linked_reload", "account": account_id, "trigger": trigger, "outcome": outcome, "expires_at": expires_at})));
    }

    pub fn capture(&self, headers: &HeaderMap, body: &[u8]) {
        let _ = self.capture_request(headers, body);
    }

    pub fn capture_request(&self, headers: &HeaderMap, body: &[u8]) -> Option<PathBuf> {
        #[cfg(all(feature = "shape-capture", debug_assertions))]
        if let Some(capture) = &self.capture
            && let Some(value) = capture.shape(headers, body)
        {
            let destination = value["destination"].as_str().map(PathBuf::from);
            self.enqueue(Command::Capture(value));
            return destination;
        }
        #[cfg(not(all(feature = "shape-capture", debug_assertions)))]
        let _ = (headers, body);
        None
    }

    pub fn capture_response(&self, ticket: Option<&Path>, headers: &HeaderMap) {
        #[cfg(all(feature = "shape-capture", debug_assertions))]
        if let Some(destination) = ticket {
            self.enqueue(Command::Capture(serde_json::json!({
                "destination": destination,
                "shape": crate::capture::response_shape(headers)
            })));
        }
        #[cfg(not(all(feature = "shape-capture", debug_assertions)))]
        let _ = (ticket, headers);
    }

    fn enqueue(&self, command: Command) {
        if self.tx.try_send(command).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
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
        let Some(worker) = self.worker.take() else {
            return;
        };
        let tx = self.tx.clone();
        let cleanup = move || {
            let _ = tx.send(Command::Stop);
            let _ = worker.join();
        };
        // 마지막 Arc가 요청 처리 중 해제되어도 런타임 스레드는 종료 배출을 기다리지 않는다.
        if tokio::runtime::Handle::try_current().is_ok() {
            if thread::Builder::new()
                .name("qb-observability-stop".into())
                .spawn(cleanup)
                .is_err()
            {
                // 송신자가 모두 해제되면 작성자는 남은 큐를 배출하고 종료한다.
                tracing::warn!(
                    "Observability cleanup thread unavailable; writer will drain on disconnect"
                );
            }
        } else {
            cleanup();
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
                    Some(n) => format!("{n}m"),
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
                if used_pct.is_some() {
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

// 누락 수는 작성자가 집계하며 최대 분당 한 번만 경고한다.
struct DropWarnings {
    last: Instant,
}
impl DropWarnings {
    fn report(&mut self, dropped: &AtomicU64, now: Instant) -> u64 {
        if now.duration_since(self.last) < WARN_INTERVAL {
            return 0;
        }
        let count = dropped.swap(0, Ordering::Relaxed);
        if count != 0 {
            tracing::warn!(dropped = count, "Observability queue dropped events");
            self.last = now;
        }
        count
    }
}

struct Writer {
    home: PathBuf,
    state: QuotaState,
    quota_dirty: bool,
    dirty_files: BTreeSet<PathBuf>,
    last_sync: Instant,
    last_prune: Instant,
    error: bool,
    #[cfg(test)]
    sync_batches: usize,
}
impl Writer {
    fn new(home: PathBuf, state: QuotaState) -> Self {
        let now = Instant::now();
        Self {
            home,
            state,
            quota_dirty: false,
            dirty_files: BTreeSet::new(),
            last_sync: now,
            last_prune: now,
            error: false,
            #[cfg(test)]
            sync_batches: 0,
        }
    }
    fn append(&mut self, path: PathBuf, value: &serde_json::Value) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        // 부분 쓰기가 실패해도 해당 파일은 다음 배치에서 동기화한다.
        self.dirty_files.insert(path);
        file.write_all(&bytes)
    }
    fn event(&mut self, command: Command) {
        let result = match command {
            Command::Quota(id, mut next) => {
                if let Some(old) = self.state.accounts.remove(&id) {
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
                self.state.accounts.insert(id, *next);
                self.quota_dirty = true;
                Ok(())
            }
            Command::Ledger(value) => self.append(
                self.home
                    .join("ledger")
                    .join(format!("{}.jsonl", Utc::now().format("%Y-%m-%d"))),
                &value,
            ),
            #[cfg(all(feature = "shape-capture", debug_assertions))]
            Command::Capture(value) => match value["destination"].as_str().map(PathBuf::from) {
                Some(path) => self.append(path, &value["shape"]),
                None => Ok(()),
            },
            Command::Flush(_) | Command::Stop => unreachable!(),
        };
        if result.is_err() {
            tracing::warn!("Observability write failed; later events will retry");
            self.error = true;
        }
    }
    fn persist(&mut self, now: Instant) {
        if now.duration_since(self.last_prune) >= WARN_INTERVAL {
            prune(&self.home.join("ledger"));
            self.last_prune = now;
        }
        if now.duration_since(self.last_sync) < SYNC_INTERVAL {
            return;
        }
        self.last_sync = now;
        if !self.quota_dirty && self.dirty_files.is_empty() {
            return;
        }
        #[cfg(test)]
        {
            self.sync_batches += 1;
        }
        if self.quota_dirty {
            let result = serde_json::to_vec(&self.state)
                .map_err(std::io::Error::other)
                .and_then(|bytes| atomic_write(&self.home.join("state/quota.json"), &bytes));
            if result.is_ok() {
                self.quota_dirty = false;
            } else {
                tracing::warn!("Quota state write failed; next batch will retry");
            }
        }
        self.dirty_files.retain(|path| {
            let result = OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|file| file.sync_data());
            if result.is_err() {
                tracing::warn!("Observability sync failed; next batch will retry");
                true
            } else {
                false
            }
        });
        // 실제 동기화가 오래 걸려도 완료 시점부터 다음 배치까지 간격을 지킨다.
        self.last_sync = now.max(Instant::now());
    }
    fn flush(&mut self) -> Result<(), String> {
        // 명시적 flush도 이전 배치로부터 1초를 지키고, 수락한 이벤트의 저장을 완료한다.
        if self.quota_dirty || !self.dirty_files.is_empty() {
            thread::sleep(SYNC_INTERVAL.saturating_sub(self.last_sync.elapsed()));
            self.persist(Instant::now());
        }
        if std::mem::take(&mut self.error) || self.quota_dirty || !self.dirty_files.is_empty() {
            Err("observability persistence failed".into())
        } else {
            Ok(())
        }
    }
}
fn run(home: PathBuf, state: QuotaState, rx: mpsc::Receiver<Command>, dropped: Arc<AtomicU64>) {
    let mut writer = Writer::new(home, state);
    let mut warnings = DropWarnings {
        last: Instant::now() - WARN_INTERVAL,
    };
    loop {
        match rx.recv_timeout(SYNC_INTERVAL.saturating_sub(writer.last_sync.elapsed())) {
            Ok(Command::Flush(reply)) => {
                let _ = reply.send(writer.flush());
            }
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if writer.flush().is_err() {
                    tracing::warn!("Observability shutdown flush failed");
                }
                break;
            }
            Ok(command) => writer.event(command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        writer.persist(Instant::now());
        warnings.report(&dropped, Instant::now());
    }
}
fn prune(dir: &Path) {
    if retain(dir, Utc::now().date_naive(), LEDGER_MAX_BYTES).is_err() {
        tracing::warn!("Ledger retention failed; next prune will retry");
    }
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
                Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW},
                core::PCWSTR,
            };
            let src: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
            let dst: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(src.as_ptr()),
                    PCWSTR(dst.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING,
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
            upstream_error_code: None,
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

    #[derive(Clone, Default)]
    struct TestLog(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for TestLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn warnings(f: impl FnOnce()) -> String {
        let log = TestLog::default();
        let sink = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = log.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }
    fn quota_command(percent: &str) -> Command {
        let mut h = HeaderMap::new();
        h.insert("x-codex-primary-used-percent", percent.parse().unwrap());
        Command::Quota(
            "cx-synthetic".into(),
            Box::new(snapshot(Provider::Codex, &h, Utc::now()).unwrap()),
        )
    }
    fn writer(dir: &Path) -> Writer {
        fs::create_dir_all(dir.join("state")).unwrap();
        fs::create_dir_all(dir.join("ledger")).unwrap();
        Writer::new(
            dir.to_owned(),
            QuotaState {
                schema: 1,
                ..Default::default()
            },
        )
    }

    #[test]
    fn bounded_queue_drops_without_waiting_and_warns_once_per_minute() {
        // 작성자가 멈춘 큐로 느린 디스크를 결정적으로 재현한다.
        let (tx, rx) = mpsc::sync_channel(2);
        let obs = Observability {
            tx,
            worker: None,
            dropped: Arc::new(AtomicU64::new(0)),
            #[cfg(all(feature = "shape-capture", debug_assertions))]
            capture: None,
        };
        let start = Instant::now();
        obs.record_request(record());
        obs.record_request(record());
        for _ in 0..600 {
            obs.record_request(record());
        }
        obs.record_reload("cx-synthetic", "expiry", "unchanged", None);
        let mut h = HeaderMap::new();
        h.insert("x-codex-limit-reached", "true".parse().unwrap());
        obs.observe_headers("cx-synthetic", Provider::Codex, &h);
        assert!(start.elapsed() < StdDuration::from_secs(1));
        assert_eq!(obs.dropped.load(Ordering::Relaxed), 602);
        let mut reporter = DropWarnings {
            last: start - WARN_INTERVAL,
        };
        let log = warnings(|| {
            assert_eq!(reporter.report(&obs.dropped, start), 602);
            obs.record_request(record());
            assert_eq!(
                reporter.report(
                    &obs.dropped,
                    start + WARN_INTERVAL - StdDuration::from_nanos(1)
                ),
                0
            );
            assert_eq!(obs.dropped.load(Ordering::Relaxed), 1);
            assert_eq!(reporter.report(&obs.dropped, start + WARN_INTERVAL), 1);
        });
        assert_eq!(log.matches("Observability queue dropped events").count(), 2);
        assert!(log.contains("dropped=602"));
        assert!(log.contains("dropped=1"));
        assert_eq!(rx.try_iter().count(), 2);
        drop(rx);
    }

    #[test]
    fn ledger_and_quota_sync_in_one_second_batches() {
        let dir = directory();
        let mut writer = writer(dir.path());
        let start = writer.last_sync;
        for i in 0..100 {
            writer.event(Command::Ledger(json!({"req": i})));
            writer.event(quota_command("10"));
            writer.persist(start + StdDuration::from_millis(i));
        }
        assert_eq!(writer.sync_batches, 0);
        assert!(!dir.path().join("state/quota.json").exists());
        writer.persist(start + SYNC_INTERVAL);
        assert_eq!(writer.sync_batches, 1);
        assert!(!writer.quota_dirty);
        assert!(writer.dirty_files.is_empty());
        writer.event(quota_command("20"));
        writer.event(Command::Ledger(json!({"req": 100})));
        writer.persist(start + SYNC_INTERVAL + StdDuration::from_millis(999));
        assert_eq!(writer.sync_batches, 1);
        let state: Value =
            serde_json::from_slice(&fs::read(dir.path().join("state/quota.json")).unwrap())
                .unwrap();
        assert_eq!(
            state["accounts"]["cx-synthetic"]["windows"][0]["used_pct"],
            10.0
        );
        writer.persist(start + SYNC_INTERVAL * 2);
        assert_eq!(writer.sync_batches, 2);
        let state: Value =
            serde_json::from_slice(&fs::read(dir.path().join("state/quota.json")).unwrap())
                .unwrap();
        assert_eq!(
            state["accounts"]["cx-synthetic"]["windows"][0]["used_pct"],
            20.0
        );
        let ledger = dir
            .path()
            .join("ledger")
            .join(format!("{}.jsonl", Utc::now().format("%Y-%m-%d")));
        assert_eq!(fs::read_to_string(&ledger).unwrap().lines().count(), 101);
    }

    #[test]
    fn flush_drains_accepted_events_and_respects_sync_interval() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        for _ in 0..100 {
            obs.record_request(record());
        }
        obs.observe_headers("cx-synthetic", Provider::Codex, &{
            let mut h = HeaderMap::new();
            h.insert("x-codex-limit-reached", "true".parse().unwrap());
            h
        });
        assert_eq!(obs.dropped.load(Ordering::Relaxed), 0);
        obs.flush().unwrap();
        let ledger = dir
            .path()
            .join("ledger")
            .join(format!("{}.jsonl", Utc::now().format("%Y-%m-%d")));
        assert_eq!(fs::read_to_string(&ledger).unwrap().lines().count(), 100);
        assert!(dir.path().join("state/quota.json").exists());
        obs.record_request(record());
        let start = Instant::now();
        obs.flush().unwrap();
        assert!(start.elapsed() >= StdDuration::from_millis(900));
        assert_eq!(fs::read_to_string(&ledger).unwrap().lines().count(), 101);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_drop_never_waits_for_full_queue_and_still_stops_writer() {
        let (tx, rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let worker = thread::spawn(move || {
            // 회귀 시에도 테스트가 영원히 정지하지 않도록 대기 시간을 제한한다.
            let _ = resume_rx.recv_timeout(StdDuration::from_secs(2));
            let mut drained = 0;
            for command in rx {
                match command {
                    Command::Stop => break,
                    Command::Ledger(_) => drained += 1,
                    _ => panic!("unexpected command"),
                }
            }
            let _ = done_tx.send(drained);
        });
        let obs = Observability {
            tx,
            worker: Some(worker),
            dropped: Arc::new(AtomicU64::new(0)),
            #[cfg(all(feature = "shape-capture", debug_assertions))]
            capture: None,
        };
        obs.record_request(record());
        let start = Instant::now();
        drop(obs);
        assert!(start.elapsed() < StdDuration::from_millis(250));
        tokio::time::sleep(StdDuration::from_millis(10)).await;
        resume_tx.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(StdDuration::from_secs(3), done_rx)
                .await
                .unwrap()
                .unwrap(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn transient_sync_and_quota_errors_retry_in_next_batch_without_sticky_failure() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = directory();
        let mut writer = writer(dir.path());
        let start = writer.last_sync;
        writer.event(Command::Ledger(json!({"req": "synthetic"})));
        writer.event(quota_command("42"));
        let ledger = writer.dirty_files.iter().next().unwrap().clone();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&ledger)
            .unwrap();
        fs::create_dir(dir.path().join("state/quota.json")).unwrap();
        let log = warnings(|| writer.persist(start + SYNC_INTERVAL));
        assert!(log.contains("Quota state write failed; next batch will retry"));
        assert!(log.contains("Observability sync failed; next batch will retry"));
        assert!(writer.quota_dirty);
        assert!(writer.dirty_files.contains(&ledger));
        drop(lock);
        fs::remove_dir(dir.path().join("state/quota.json")).unwrap();
        writer.persist(start + SYNC_INTERVAL * 2);
        assert!(!writer.quota_dirty);
        assert!(writer.dirty_files.is_empty());
        writer.flush().unwrap();
        let state: Value =
            serde_json::from_slice(&fs::read(dir.path().join("state/quota.json")).unwrap())
                .unwrap();
        assert_eq!(
            state["accounts"]["cx-synthetic"]["windows"][0]["used_pct"],
            42.0
        );
        assert_eq!(fs::read_to_string(ledger).unwrap().lines().count(), 1);
    }

    #[test]
    fn transient_ledger_and_quota_errors_recover_after_flush() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        let ledger = dir
            .path()
            .join("ledger")
            .join(format!("{}.jsonl", Utc::now().format("%Y-%m-%d")));
        fs::create_dir(&ledger).unwrap();
        obs.record_request(record());
        assert_eq!(
            obs.flush().unwrap_err().to_string(),
            "observability persistence failed"
        );
        fs::remove_dir(&ledger).unwrap();
        obs.record_request(record());
        obs.flush().unwrap();
        assert_eq!(fs::read_to_string(&ledger).unwrap().lines().count(), 1);
        fs::create_dir(dir.path().join("state/quota.json")).unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-codex-limit-reached", "true".parse().unwrap());
        obs.observe_headers("cx-synthetic", Provider::Codex, &h);
        assert_eq!(
            obs.flush().unwrap_err().to_string(),
            "observability persistence failed"
        );
        fs::remove_dir(dir.path().join("state/quota.json")).unwrap();
        // 새 quota 이벤트가 없어도 이전의 최신 상태를 다시 저장한다.
        obs.flush().unwrap();
        let value: Value =
            serde_json::from_slice(&fs::read(dir.path().join("state/quota.json")).unwrap())
                .unwrap();
        assert_eq!(
            value["accounts"]["cx-synthetic"]["signals"]["limit_reached"],
            true
        );
        obs.flush().unwrap();
    }

    #[test]
    fn corrupt_and_unsupported_quota_are_renamed_warned_and_start_empty() {
        for contents in ["fake-access-marker", r#"{"schema":2,"accounts":{}}"#] {
            let dir = directory();
            fs::create_dir(dir.path().join("state")).unwrap();
            fs::write(dir.path().join("state/quota.json"), contents).unwrap();
            let log = warnings(|| {
                let obs = Observability::new(dir.path(), None).unwrap();
                assert!(!dir.path().join("state/quota.json").exists());
                let files: Vec<_> = fs::read_dir(dir.path().join("state"))
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .collect();
                assert_eq!(files.len(), 1);
                assert!(
                    files[0]
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with("quota.json.corrupt-")
                );
                assert_eq!(fs::read_to_string(&files[0]).unwrap(), contents);
                let mut h = HeaderMap::new();
                h.insert("x-codex-limit-reached", "true".parse().unwrap());
                obs.observe_headers("cx-new", Provider::Codex, &h);
                obs.flush().unwrap();
                let value: Value =
                    serde_json::from_slice(&fs::read(dir.path().join("state/quota.json")).unwrap())
                        .unwrap();
                assert_eq!(value["schema"], 1);
                assert_eq!(value["accounts"].as_object().unwrap().len(), 1);
            });
            assert!(log.contains("Corrupt quota state renamed; starting empty"));
            assert!(!log.contains("fake-access-marker"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn locked_expired_ledger_warns_starts_and_retries_next_prune() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = directory();
        fs::create_dir(dir.path().join("ledger")).unwrap();
        let expired = dir.path().join("ledger/2000-01-01.jsonl");
        fs::write(&expired, "synthetic").unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&expired)
            .unwrap();
        let log = warnings(|| {
            let obs = Observability::new(dir.path(), None).unwrap();
            obs.record_request(record());
            obs.flush().unwrap();
        });
        assert!(log.contains("Ledger retention failed; next prune will retry"));
        assert!(expired.exists());
        let mut writer = writer(dir.path());
        let start = writer.last_prune;
        let log = warnings(|| writer.persist(start + WARN_INTERVAL));
        assert!(log.contains("Ledger retention failed"));
        drop(lock);
        writer.persist(start + WARN_INTERVAL + StdDuration::from_secs(59));
        assert!(expired.exists());
        writer.persist(start + WARN_INTERVAL * 2);
        assert!(!expired.exists());
        assert!(!writer.error);
    }

    #[test]
    fn codex_realistic_windows_require_usage_and_never_invent_resets() {
        let now: DateTime<Utc> = "2026-10-13T00:00:00Z".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-codex-primary-used-percent", "12".parse().unwrap());
        h.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        h.insert(
            "x-codex-primary-reset-after-seconds",
            "120".parse().unwrap(),
        );
        h.insert("x-codex-secondary-window-minutes", "10080".parse().unwrap());
        h.insert(
            "x-codex-secondary-reset-after-seconds",
            "0".parse().unwrap(),
        );
        let primary = snapshot(Provider::Codex, &h, now).unwrap();
        assert_eq!(primary.windows.len(), 1);
        assert_eq!(primary.windows[0].name, "5h");
        assert_eq!(
            primary.windows[0].reset_at,
            Some(now + Duration::seconds(120))
        );
        h.insert("x-codex-secondary-used-percent", "7".parse().unwrap());
        h.insert("x-codex-secondary-reset-at", "2000000000".parse().unwrap());
        let both = snapshot(Provider::Codex, &h, now).unwrap();
        assert_eq!(both.windows[1].name, "7d");
        assert_eq!(
            both.windows[1].reset_at,
            DateTime::from_timestamp(2000000000, 0)
        );
        h.remove("x-codex-primary-reset-after-seconds");
        h.insert("x-codex-primary-window-minutes", "60".parse().unwrap());
        let absent = snapshot(Provider::Codex, &h, now).unwrap();
        assert_eq!(absent.windows[0].name, "60m");
        assert_eq!(absent.windows[0].reset_at, None);
        h.remove("x-codex-primary-used-percent");
        h.remove("x-codex-secondary-used-percent");
        assert!(snapshot(Provider::Codex, &h, now).is_none());
    }

    #[cfg(feature = "shape-capture")]
    #[test]
    fn response_shape_capture_allowlists_limits_and_keeps_last_request_response() {
        let dir = directory();
        let destination = dir.path().join("capture");
        fs::create_dir(&destination).unwrap();
        let obs = Observability::new(
            dir.path(),
            Some(CaptureOptions {
                dir: destination.clone(),
                limit: 1,
            }),
        )
        .unwrap();
        let ticket = obs.capture_request(&HeaderMap::new(), b"{}").unwrap();
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("x-codex-primary-used-percent", "20"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("retry-after", "60"),
            ("x-request-id", "synthetic-id"),
            ("set-cookie", "fake-cookie-secret"),
            ("authorization", "Bearer fake-access-secret"),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        obs.capture_response(Some(&ticket), &headers);
        assert!(obs.capture_request(&HeaderMap::new(), b"{}").is_none());
        obs.capture_response(None, &headers);
        obs.flush().unwrap();
        let text = fs::read_to_string(ticket).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1]["response_header_values"]["x-codex-primary-used-percent"],
            serde_json::json!(["20"])
        );
        assert_eq!(
            lines[1]["response_header_names"].as_array().unwrap().len(),
            4
        );
        assert!(!text.contains("fake-cookie-secret"));
        assert!(!text.contains("fake-access-secret"));
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
