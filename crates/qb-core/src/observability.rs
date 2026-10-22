use crate::accounts::Provider;
pub mod reread_accounting;
use axum::http::HeaderMap;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
    #[serde(default)]
    pub rate_wait_ms: u64,
    #[serde(default)]
    pub rate_retries: u32,
    #[serde(default)]
    pub relayed_rate_429s: u32,
    pub outcome: String,
    #[serde(default)]
    pub retained_copy: bool,
    #[serde(default)]
    pub upstream_error_code: Option<String>,
    #[serde(default)]
    pub attempt_details: Vec<AttemptRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_usage: Option<CacheUsage>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub after_move: bool,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct CacheUsage {
    pub input_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

/// Only numerical usage fields cross the response observer into the ledger.
pub fn cache_usage(event: &serde_json::Value) -> Option<CacheUsage> {
    let usage = event
        .get("usage")
        .or_else(|| event.get("response")?.get("usage"))
        .or_else(|| event.get("message")?.get("usage"))?;
    let result = CacheUsage {
        input_tokens: usage["input_tokens"].as_u64(),
        cached_tokens: usage["cache_read_input_tokens"]
            .as_u64()
            .or_else(|| usage["input_tokens_details"]["cached_tokens"].as_u64()),
        cache_creation_input_tokens: usage["cache_creation_input_tokens"].as_u64(),
    };
    (result.input_tokens.is_some()
        || result.cached_tokens.is_some()
        || result.cache_creation_input_tokens.is_some())
    .then_some(result)
}

/// Merge cumulative numeric fields from ONE turn, replacing present fields, never adding.
/// Claude message_start and message_delta may expose disjoint usage fields.
pub fn merge_cache_usage(target: &mut Option<CacheUsage>, incoming: CacheUsage) {
    let current = target.get_or_insert_with(CacheUsage::default);
    current.input_tokens = incoming.input_tokens.or(current.input_tokens);
    current.cached_tokens = incoming.cached_tokens.or(current.cached_tokens);
    current.cache_creation_input_tokens = incoming
        .cache_creation_input_tokens
        .or(current.cache_creation_input_tokens);
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub account: String,
    pub failover_reason: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct QuotaState {
    #[serde(default = "schema")]
    schema: u32,
    #[serde(default)]
    accounts: BTreeMap<String, Snapshot>,
}
fn reset_moved(new: &serde_json::Value, old: &serde_json::Value) -> bool {
    new.as_str()
        .zip(old.as_str())
        .and_then(|(new, old)| {
            Some((
                DateTime::parse_from_rfc3339(new).ok()?,
                DateTime::parse_from_rfc3339(old).ok()?,
            ))
        })
        .is_some_and(|(new, old)| (new - old).num_seconds().abs() > 300)
}
fn schema() -> u32 {
    1
}

#[derive(Clone, Serialize, Deserialize)]
struct Window {
    name: String,
    used_pct: Option<f64>,
    reset_at: Option<DateTime<Utc>>,
    #[serde(default)]
    observed_at: Option<DateTime<Utc>>,
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
    #[serde(default)]
    serving_from_credits: Option<bool>,
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
            && self.serving_from_credits.is_none()
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
        // A new non-credit active limit ends an old paid-serving claim.
        if self.active_limit.as_deref().is_some_and(|l| l != "credits")
            && self.serving_from_credits.is_none()
        {
            self.serving_from_credits = Some(false);
        }
        self.serving_from_credits = self.serving_from_credits.or(old.serving_from_credits);
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
    PollQuota(String, Box<Snapshot>),
    Ledger(serde_json::Value),
    #[cfg(all(feature = "shape-capture", debug_assertions))]
    Capture(serde_json::Value),
    Flush(mpsc::Sender<Result<(), String>>),
    FreezeQuota(mpsc::Sender<Result<(), String>>),
    Stop,
}

pub struct Observability {
    dashboard: Arc<crate::dashboard::runtime::Runtime>,
    accounting: std::sync::Mutex<reread_accounting::Registry>,
    move_warnings: std::sync::Mutex<VecDeque<(Instant, serde_json::Value)>>,
    reread_notice_tokens: AtomicU64,
    reread_alerted: std::sync::atomic::AtomicBool,
    // One recent aggregate only; hidden trays recover transient broadcast loss.
    recent_burst: std::sync::Mutex<Option<(std::time::Instant, DateTime<Utc>, u32)>>,
    events: std::sync::Mutex<Option<tokio::sync::broadcast::Sender<serde_json::Value>>>,
    snapshots: std::sync::Mutex<BTreeMap<String, serde_json::Value>>,
    shadow: std::sync::Mutex<crate::routing::forecast::Shadow>,
    tx: mpsc::SyncSender<Command>,
    worker: Option<thread::JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
    #[cfg(all(feature = "shape-capture", debug_assertions))]
    capture: Option<crate::capture::Capture>,
}

impl Observability {
    /// Record exactly once, at committed real move, before another request can start.
    pub fn committed_move(
        &self,
        provider: Provider,
        canonical_key: &str,
        to_account: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<reread_accounting::MoveId, reread_accounting::Error> {
        let id = self
            .accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .moved(provider, canonical_key, to_account, model, at)?;
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(canonical_key.as_bytes());
        let session: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        self.enqueue(Command::Ledger(
            serde_json::json!({"ts":at, "event":"binding_move_cost",
            "move_id":id, "session":session, "provider":provider, "reread_tokens":null,
            "measurement":"pending"}),
        ));
        Ok(id)
    }
    /// Alias adoption preserves baseline, outstanding tickets and move attribution.
    /// Conflicting existing canonical identities are rejected rather than silently merged.
    pub fn accounting_alias(
        &self,
        provider: Provider,
        existing_key: &str,
        alias_key: &str,
        at: DateTime<Utc>,
    ) -> Result<(), reread_accounting::Error> {
        self.accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .alias(provider, existing_key, alias_key, at)
    }
    pub fn accounting_begin(
        &self,
        provider: Provider,
        canonical_key: &str,
        account: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<reread_accounting::RequestTicket, reread_accounting::Error> {
        self.accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin(provider, canonical_key, account, model, at)
    }
    pub fn accounting_end(
        &self,
        ticket: reread_accounting::RequestTicket,
        success: bool,
        usage: Option<&CacheUsage>,
        at: DateTime<Utc>,
    ) -> Result<reread_accounting::Completion, reread_accounting::Error> {
        let result = self
            .accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish(ticket, success, usage, at)?;
        if let reread_accounting::Completion::Move { id, reread_tokens } = result {
            self.enqueue(Command::Ledger(
                serde_json::json!({"ts":at, "event":"binding_move_cost",
                "move_id":id, "reread_tokens":reread_tokens,
                "measurement":if reread_tokens.is_some() { "known" } else { "unknown" }}),
            ));
        }
        let snapshot = self.accounting_snapshot(at);
        let above = snapshot.hour.tokens >= self.reread_notice_tokens.load(Ordering::Acquire);
        let was_above = self.reread_alerted.swap(above, Ordering::AcqRel);
        if above && !was_above {
            self.move_warning(serde_json::json!({"kind":"move_reread", "observed_at":at,
                "reread_tokens":snapshot.hour.tokens,
                "known_count":snapshot.hour.known_count, "unknown_count":snapshot.hour.unknown_count}));
        }
        Ok(result)
    }
    pub fn accounting_snapshot(&self, at: DateTime<Utc>) -> reread_accounting::Snapshot {
        self.accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot(at)
    }
    pub fn record_move_guard(&self, event: &crate::bindings::MoveGuardEvent) {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(event.key.as_bytes());
        let session: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        self.enqueue(Command::Ledger(
            serde_json::json!({"ts":Utc::now(), "event":"binding_move_refused",
            "session":session, "from":event.from, "to":event.to, "reason":event.reason,
            "proposed_reason":event.proposed_reason,
            "until":event.until, "newly_tripped":event.newly_tripped, "forced":event.forced}),
        ));
        if event.newly_tripped {
            self.move_warning(serde_json::json!({
                "kind":"move_breaker", "observed_at":Utc::now(),
                "move_list":event.move_list, "until":event.until,
                "session":session, "from":event.from, "to":event.to,
                "reason":event.reason, "forced":event.forced}));
        }
    }
    pub fn configure_move_notice(&self, threshold: u64) {
        self.reread_notice_tokens
            .store(threshold.clamp(1, 1_000_000_000), Ordering::Release);
    }
    fn move_warning(&self, value: serde_json::Value) {
        let mut warnings = self.move_warnings.lock().unwrap_or_else(|e| e.into_inner());
        warnings.retain(|(at, _)| at.elapsed() < StdDuration::from_secs(600));
        if warnings.len() >= 32 {
            warnings.pop_front();
        }
        warnings.push_back((Instant::now(), value.clone()));
        drop(warnings);
        self.enqueue(Command::Ledger(
            serde_json::json!({"ts":Utc::now(), "event":"core_warning", "data":value}),
        ));
        self.progress(serde_json::json!({"event":"core_warning", "data":value}));
    }

    pub fn configure_forecast(&self, enabled: bool) {
        self.shadow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enable(enabled);
    }
    pub fn forecast_load(&self, id: &str, active: usize) {
        self.shadow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .regime(id, Some(active), None);
    }
    pub fn forecast_view(&self, id: &str, active: usize, inflight: usize) -> serde_json::Value {
        let snapshot = self.quota_snapshot(id).unwrap_or_default();
        self.shadow.lock().unwrap_or_else(|e| e.into_inner()).view(
            id,
            &snapshot,
            active,
            inflight,
            Utc::now(),
        )
    }
    pub fn dashboard(&self) -> &Arc<crate::dashboard::runtime::Runtime> {
        &self.dashboard
    }

    pub fn record_takeover(&self, phase: &str) {
        self.enqueue(Command::Ledger(serde_json::json!({
            "ts": Utc::now().to_rfc3339(), "event": "takeover",
            "phase": phase, "pid": std::process::id()
        })));
    }
    pub(crate) fn set_events(&self, sender: tokio::sync::broadcast::Sender<serde_json::Value>) {
        self.dashboard.set_events(sender.clone());
        *self.events.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
    }
    fn progress(&self, event: serde_json::Value) {
        if let Some(sender) = self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = sender.send(event);
        }
    }
    pub fn record_login(
        &self,
        provider: Provider,
        login_id: &str,
        status: &str,
        account_id: Option<&str>,
    ) {
        if status == "completed" || status == "success" {
            self.dashboard.account_event(
                crate::dashboard::history::ActivityKind::Login,
                provider,
                account_id,
                None,
                None,
            );
        }
        self.enqueue(Command::Ledger(serde_json::json!({"ts":Utc::now().to_rfc3339(),"event":"login","provider":provider,"login_id":login_id,"outcome":status,"account":account_id})));
        self.progress(serde_json::json!({"event":"login_progress","data":{"login_id":login_id,"status":status,"account_id":account_id}}));
    }
    pub fn record_refresh(
        &self,
        account: &str,
        trigger: &str,
        outcome: &str,
        expires_at: Option<DateTime<Utc>>,
    ) {
        self.dashboard.account_event(
            crate::dashboard::history::ActivityKind::TokenRefresh,
            provider_for(account),
            Some(account),
            Some(refresh_activity_outcome(outcome)),
            expires_at,
        );
        self.enqueue(Command::Ledger(serde_json::json!({"ts":Utc::now().to_rfc3339(),"event":"token_refresh","account":account,"trigger":trigger,"outcome":outcome,"expires_at":expires_at})));
    }
    pub(crate) fn recent_warnings(&self) -> Vec<serde_json::Value> {
        let mut recent = self.recent_burst.lock().unwrap_or_else(|e| e.into_inner());
        if recent
            .as_ref()
            .is_some_and(|(at, _, _)| at.elapsed() >= std::time::Duration::from_secs(600))
        {
            *recent = None;
        }
        let mut values: Vec<_> = recent.as_ref().map(|(_, ts, count)| {
            serde_json::json!({"kind":"burst", "burst_new_sessions":count, "observed_at":ts})
        }).into_iter().collect();
        let mut warnings = self.move_warnings.lock().unwrap_or_else(|e| e.into_inner());
        warnings.retain(|(at, _)| at.elapsed() < StdDuration::from_secs(600));
        values.extend(warnings.iter().map(|(_, value)| value.clone()));
        values
    }
    pub(crate) fn remember_burst(&self, observed_count: u32) {
        *self.recent_burst.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((std::time::Instant::now(), Utc::now(), observed_count));
    }
    pub(crate) fn burst_warning(&self, observed_count: u32) {
        self.dashboard
            .activity(crate::dashboard::history::ActivityInput {
                ts: Utc::now(),
                kind: crate::dashboard::history::ActivityKind::Burst,
                provider: Provider::Codex,
                account: None,
                account_to: None,
                notable: true,
                data: crate::dashboard::history::ActivityData {
                    count: observed_count,
                    ..Default::default()
                },
            });
        self.remember_burst(observed_count);
        self.enqueue(Command::Ledger(serde_json::json!({
            "ts": Utc::now().to_rfc3339(), "event": "core_warning",
            "kind": "burst", "burst_new_sessions": observed_count,
        })));
        self.progress(serde_json::json!({
            "event": "core_warning",
            "data": {"kind": "burst", "burst_new_sessions": observed_count},
        }));
    }
    pub fn core_warning(&self, account: &str, code: &str) {
        self.enqueue(Command::Ledger(serde_json::json!({"ts":Utc::now().to_rfc3339(),"event":"core_warning","account":account,"code":code})));
        self.progress(
            serde_json::json!({"event":"core_warning","data":{"account":account,"code":code}}),
        );
    }
    pub fn record_guard(&self, phase: &str) {
        self.enqueue(Command::Ledger(serde_json::json!({
            "ts": Utc::now().to_rfc3339(),
            "event": phase,
        })));
        self.progress(serde_json::json!({"event": phase, "data": {}}));
    }
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
        let mut state = match fs::read(&path) {
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
        for snapshot in state.accounts.values_mut() {
            let mut credit = serde_json::to_value(&snapshot.credits).unwrap_or_default();
            crate::credits::normalize(&mut credit);
            snapshot.credits = serde_json::from_value(credit).unwrap_or_default();
            snapshot.signals.merge(Signals::default());
        }
        let dashboard = crate::dashboard::runtime::Runtime::new();
        restore_dashboard(home, &dashboard);
        let worker_dashboard = dashboard.clone();
        // 요청 경로에서는 큐가 가득 차도 디스크나 작성자를 기다리지 않는다.
        let (tx, rx) = mpsc::sync_channel(256);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = dropped.clone();
        let home = home.to_owned();
        let snapshots = state
            .accounts
            .iter()
            .filter_map(|(id, s)| serde_json::to_value(s).ok().map(|v| (id.clone(), v)))
            .collect();
        let worker = thread::Builder::new()
            .name("qb-observability".into())
            .spawn(move || run(home, state, rx, worker_dropped, worker_dashboard))?;
        Ok(Self {
            dashboard,
            accounting: std::sync::Mutex::new(reread_accounting::Registry::new(Utc::now())),
            move_warnings: std::sync::Mutex::new(VecDeque::new()),
            reread_notice_tokens: AtomicU64::new(100_000),
            reread_alerted: std::sync::atomic::AtomicBool::new(false),
            recent_burst: std::sync::Mutex::new(None),
            events: std::sync::Mutex::new(None),
            shadow: std::sync::Mutex::new(crate::routing::forecast::Shadow::default()),
            snapshots: std::sync::Mutex::new(snapshots),
            tx,
            worker: Some(worker),
            dropped,
            #[cfg(all(feature = "shape-capture", debug_assertions))]
            capture,
        })
    }

    pub fn observe_headers(&self, account_id: &str, provider: Provider, headers: &HeaderMap) {
        let _ = self.observe_headers_snapshot(account_id, provider, headers);
    }

    pub(crate) fn observe_headers_snapshot(
        &self,
        account_id: &str,
        provider: Provider,
        headers: &HeaderMap,
    ) -> Option<serde_json::Value> {
        self.observe_headers_reported(account_id, provider, headers)
            .map(|(merged, _)| merged)
    }

    pub(crate) fn observe_headers_reported(
        &self,
        account_id: &str,
        provider: Provider,
        headers: &HeaderMap,
    ) -> Option<(serde_json::Value, serde_json::Value)> {
        let snapshot = snapshot(provider, headers, Utc::now())?;
        let reported = serde_json::to_value(&snapshot).ok()?;
        let merged = self.remember(account_id, &snapshot, true)?;
        Some((merged, reported))
    }

    pub(crate) fn observe_poll(
        &self,
        id: &str,
        value: serde_json::Value,
        replace_windows: bool,
    ) -> Option<serde_json::Value> {
        let next: Snapshot = serde_json::from_value(value).ok()?;
        // Complete poll responses replace the window set. Partial responses
        // retain known windows just like partial response headers.
        self.remember(id, &next, !replace_windows)
    }

    pub(crate) fn capture_poll(&self, provider: Provider, body: &[u8]) {
        #[cfg(all(feature = "shape-capture", debug_assertions))]
        if let Some(capture) = &self.capture {
            let headers = HeaderMap::new();
            if let Some(mut value) = capture.shape(&headers, b"{}") {
                value["shape"]["poll"] = crate::quota::poll_shape(provider, body);
                value["shape"]["provider"] = serde_json::json!(provider);
                self.enqueue(Command::Capture(value));
            }
        }
        #[cfg(not(all(feature = "shape-capture", debug_assertions)))]
        let _ = (provider, body);
    }

    pub fn observe_codex_event(&self, account_id: &str, event: &serde_json::Value) {
        let _ = self.observe_codex_event_snapshot(account_id, event);
    }

    pub(crate) fn observe_codex_event_snapshot(
        &self,
        account_id: &str,
        event: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        self.observe_codex_event_reported(account_id, event)
            .map(|(merged, _)| merged)
    }

    pub(crate) fn observe_codex_event_reported(
        &self,
        account_id: &str,
        event: &serde_json::Value,
    ) -> Option<(serde_json::Value, serde_json::Value)> {
        if event.get("type").and_then(serde_json::Value::as_str) != Some("codex.rate_limits") {
            return None;
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
        for key in ["limit_reached", "plan_type", "active_limit"] {
            if let Some(value) = event.get(key).or_else(|| limits.get(key)) {
                add(format!("x-codex-{}", key.replace('_', "-")), value);
            }
        }
        if let Some(value) = event
            .get("serving_from_credits")
            .or_else(|| limits.get("serving_from_credits"))
            .or_else(|| event.get("credits").and_then(|c| c.get("used")))
        {
            add("x-codex-credits-serving".into(), value);
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
            let reported = serde_json::to_value(&snapshot).ok()?;
            let merged = self.remember(account_id, &snapshot, true)?;
            return Some((merged, reported));
        }
        None
    }

    pub fn record_placement(&self, event: &crate::bindings::MoveEvent) {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(event.key.as_bytes());
        let session: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        self.enqueue(Command::Ledger(serde_json::json!({
            "ts":Utc::now().to_rfc3339(), "event":"placement", "session":session,
            "account":event.to, "evidence":event.evidence, "capped":event.capped,
            "placed_since_obs":event.placed_since_obs, "charge":event.placement_charge_pct
        })));
    }
    pub fn record_unknown_cost_skips(&self, count: usize) {
        if count > 0 {
            self.enqueue(Command::Ledger(serde_json::json!({
                "ts":Utc::now().to_rfc3339(), "event":"victim_skipped_unknown_cost",
                "count":count
            })));
        }
    }
    pub fn record_placement_poll(&self, account: &str) {
        self.enqueue(Command::Ledger(serde_json::json!({
            "ts":Utc::now().to_rfc3339(), "event":"poll", "account":account,
            "reason":"placement_evidence"
        })));
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
            self.enqueue(Command::Capture(serde_json::json!({"destination":destination,"shape":{"response_header_names":headers.keys().map(|n|n.as_str()).filter(|name|crate::dashboard::labels::capture_header_allowed(name)).collect::<Vec<_>>(),"response_header_values":values}})));
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
        record.model = record.model.as_deref().and_then(safe_identifier);
        if let Some(id) = &record.account {
            self.shadow
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .regime(id, None, record.model.as_deref());
        }
        record.originator = record.originator.as_deref().and_then(safe_identifier);
        if let Ok(value) = serde_json::to_value(record) {
            self.enqueue(Command::Ledger(value));
        }
    }

    pub fn record_move(&self, key: &str, from: Option<&str>, to: &str, reason: &str, bytes: u64) {
        self.record_group_move(key, from, to, reason, bytes, 1, false);
    }

    pub fn record_overload(
        &self,
        account: &str,
        active: usize,
        threshold: usize,
        groups_hit: usize,
        marked: usize,
    ) {
        self.dashboard
            .activity(crate::dashboard::history::ActivityInput {
                ts: Utc::now(),
                kind: crate::dashboard::history::ActivityKind::OverloadDetected,
                provider: provider_for(account),
                account: account.to_owned().try_into().ok(),
                account_to: None,
                notable: true,
                data: crate::dashboard::history::ActivityData {
                    active: Some(active.min(u32::MAX as usize) as u32),
                    threshold: Some(threshold.min(u32::MAX as usize) as u32),
                    groups_hit: Some(groups_hit.min(u32::MAX as usize) as u32),
                    ..Default::default()
                },
            });
        self.enqueue(Command::Ledger(serde_json::json!({
            "ts": Utc::now().to_rfc3339(),
            "event": "overload_detected",
            "account": account,
            "active": active,
            "threshold": threshold,
            "groups_hit": groups_hit,
            "marked": marked,
        })));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_group_move(
        &self,
        key: &str,
        from: Option<&str>,
        to: &str,
        reason: &str,
        bytes: u64,
        keys: usize,
        debug_pin: bool,
    ) {
        self.dashboard.placement(key, from, to, reason);
        use sha2::{Digest, Sha256};
        let reason = safe_identifier(reason).unwrap_or_else(|| "unknown".into());
        let digest = Sha256::digest(key.as_bytes());
        let session: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        self.enqueue(Command::Ledger(serde_json::json!({
            "ts": Utc::now().to_rfc3339(), "event": "binding_move",
            "session": session, "from": from, "to": to, "reason": reason,
            "bytes_in": bytes, "keys": keys, "debug_pin": debug_pin
        })));
    }

    fn remember(
        &self,
        id: &str,
        next: &Snapshot,
        preserve_missing_windows: bool,
    ) -> Option<serde_json::Value> {
        use crate::dashboard::history::*;
        if let Some(balance) = next.credits.as_ref().and_then(|c| c.balance) {
            self.dashboard
                .observe_credit_forecast(id, balance, next.observed_at);
        }
        if let Ok(account) = crate::dashboard::AccountId::try_from(id.to_owned()) {
            for window in &next.windows {
                if let Some(used_pct) = window.used_pct {
                    self.dashboard.observe_usage_forecast(
                        id,
                        &window.name,
                        used_pct,
                        window.reset_at,
                        window.observed_at.unwrap_or(next.observed_at),
                    );
                } else if let Some(reset) = window.reset_at {
                    self.dashboard.observe_forecast_reset(
                        id,
                        &window.name,
                        reset,
                        window.observed_at.unwrap_or(next.observed_at),
                    );
                }
                if let Ok(name) = window.name.clone().try_into() {
                    self.dashboard.usage(UsageObservation {
                        account: account.clone(),
                        observed_at: window.observed_at.unwrap_or(next.observed_at),
                        window: Some(WindowObservation {
                            name,
                            scope: WindowScope::Account,
                            used_pct: window.used_pct,
                            reset_at: window.reset_at,
                        }),
                        credits: None,
                    });
                }
            }
        }

        if let Ok(mut snapshots) = self.snapshots.lock()
            && let Ok(mut value) = serde_json::to_value(next)
        {
            // 부분 헤더 응답은 기존 창 정보를 지우지 않는다.
            if let Some(old) = snapshots.get(id) {
                if let (Some(new), Some(previous)) =
                    (value["windows"].as_array_mut(), old["windows"].as_array())
                {
                    for window in previous {
                        if let Some(updated) = new.iter_mut().find(|w| w["name"] == window["name"])
                        {
                            let reset_changed = !updated["reset_at"].is_null()
                                && !window["reset_at"].is_null()
                                && reset_moved(&updated["reset_at"], &window["reset_at"]);
                            for key in ["used_pct", "reset_at", "observed_at", "status"] {
                                if updated[key].is_null() && !(key == "status" && reset_changed) {
                                    updated[key] = window[key].clone();
                                }
                            }
                        } else if preserve_missing_windows {
                            new.push(window.clone());
                        }
                    }
                }
                let fresh_reset = value["windows"].as_array().is_some_and(|windows| {
                    windows.iter().any(|window| {
                        window["used_pct"].as_f64().is_some_and(|used| used < 100.0)
                            && old["windows"].as_array().is_some_and(|old| {
                                old.iter().any(|prior| {
                                    prior["name"] == window["name"]
                                        && reset_moved(&window["reset_at"], &prior["reset_at"])
                                })
                            })
                    })
                });
                if let (Ok(mut next_signals), Ok(mut prior_signals)) = (
                    serde_json::from_value::<Signals>(value["signals"].clone()),
                    serde_json::from_value::<Signals>(old["signals"].clone()),
                ) {
                    if fresh_reset {
                        prior_signals.unified_status = None;
                        prior_signals.limit_reached = None;
                        prior_signals.serving_from_credits = None;
                        prior_signals.active_limit = None;
                    }
                    next_signals.merge(prior_signals);
                    value["signals"] = serde_json::to_value(next_signals).unwrap_or_default();
                }
                if value["plan"].is_null() {
                    value["plan"] = old["plan"].clone();
                }
                crate::credits::merge(&mut value["credits"], &old["credits"]);
                let subscription_still_full = value["windows"].as_array().is_some_and(|windows| {
                    windows.iter().any(|w| {
                        w["name"].as_str().is_some_and(|n| !n.contains('_'))
                            && w["used_pct"].as_f64().is_some_and(|p| p >= 99.0)
                    })
                });
                if (!fresh_reset || subscription_still_full)
                    && !crate::credits::serving_ended(&value)
                    && old["paid_serving_observed_at"].as_str().is_some()
                {
                    value["paid_serving_observed_at"] = old["paid_serving_observed_at"].clone();
                }
            }
            crate::credits::normalize(&mut value["credits"]);
            let mut signals: Signals = serde_json::from_value(value["signals"].clone()).ok()?;
            signals.merge(Signals::default());
            value["signals"] = serde_json::to_value(signals).ok()?;
            let persisted: Snapshot = serde_json::from_value(value.clone()).ok()?;
            snapshots.insert(id.into(), value.clone());
            self.shadow
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .observe(id, &value, Utc::now());
            // Persist the exact in-memory result in observation order. The writer
            // must never apply a second, different partial-update reducer.
            self.enqueue(Command::PollQuota(id.into(), Box::new(persisted)));
            return Some(value);
        }
        None
    }

    pub fn quota_snapshot(&self, id: &str) -> Option<serde_json::Value> {
        self.snapshots.lock().ok()?.get(id).cloned()
    }

    pub(crate) fn paid_serving_proof(
        &self,
        id: &str,
        observed_at: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let mut snapshots = self.snapshots.lock().ok()?;
        let snapshot = snapshots.get_mut(id)?;
        if &snapshot["observed_at"] != observed_at {
            return None;
        }
        snapshot["paid_serving_observed_at"] = if !crate::credits::serving_ended(snapshot) {
            serde_json::json!(Utc::now())
        } else {
            serde_json::Value::Null
        };
        Some(snapshot.clone())
    }

    pub(crate) fn record_account(&self, account: &crate::accounts::Account) {
        self.dashboard.account_changed(account);
        // Only canonical subscription names enter the account ledger.
        let plan = account.plan.as_deref().filter(|plan| {
            matches!(
                *plan,
                "pro"
                    | "max_5x"
                    | "max_20x"
                    | "team"
                    | "enterprise"
                    | "free"
                    | "plus"
                    | "business"
                    | "edu"
                    | "go"
                    | "max"
            )
        });
        self.enqueue(Command::Ledger(serde_json::json!({
            "ts": Utc::now().to_rfc3339(), "event": "account_changed",
            "account": account.id, "provider": account.provider,
            "generation": account.generation, "plan": plan,
            "health": account.health, "user_state": account.user_state,
        })));
    }

    pub(crate) fn record_account_health(
        &self,
        account: &crate::accounts::Account,
        from: &crate::accounts::Health,
    ) {
        self.dashboard.account_health(account, from);
        self.record_health_ledger(&account.id, from, &account.health);
    }

    pub fn record_health(
        &self,
        id: &str,
        from: &crate::accounts::Health,
        to: &crate::accounts::Health,
    ) {
        self.dashboard.health(id, from, to);
        self.record_health_ledger(id, from, to);
    }

    fn record_health_ledger(
        &self,
        id: &str,
        from: &crate::accounts::Health,
        to: &crate::accounts::Health,
    ) {
        let value = serde_json::to_value(to).unwrap_or_default();
        self.enqueue(Command::Ledger(serde_json::json!({"ts":Utc::now().to_rfc3339(),"event":"health_changed","account":id,"from":from,"to":to,"reason":value["reason"],"until":value["until"]})));
    }

    pub fn record_reload(
        &self,
        account_id: &str,
        trigger: &str,
        outcome: &str,
        expires_at: Option<DateTime<Utc>>,
    ) {
        self.dashboard.account_event(
            crate::dashboard::history::ActivityKind::LinkedReload,
            provider_for(account_id),
            Some(account_id),
            Some(reload_activity_outcome(outcome)),
            expires_at,
        );
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
    pub fn freeze_quota(&self) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::FreezeQuota(tx))
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
                    .filter(|v| *v >= 0.0)
                    .map(|v| v * 100.0);
                let reset_at = timestamp(h, &format!("{prefix}-reset"));
                let status = status(h, &format!("{prefix}-status"));
                if used_pct.is_some() || reset_at.is_some() || status.is_some() {
                    windows.push(Window {
                        name: name.into(),
                        used_pct,
                        reset_at,
                        observed_at: used_pct.map(|_| now),
                        status,
                    });
                }
            }
        }
        Provider::Codex => {
            signals.limit_reached = header(h, "x-codex-limit-reached").and_then(|v| v.parse().ok());
            signals.active_limit = header(h, "x-codex-active-limit")
                .and_then(|v| match v {
                    "primary" | "secondary" | "5h" | "7d" | "7d_oi" | "credits" => Some(v),
                    "five_hour" => Some("5h"),
                    "seven_day" => Some("7d"),
                    "seven_day_opus" => Some("7d_oi"),
                    _ => None,
                })
                .map(str::to_owned);
            signals.serving_from_credits = header(h, "x-codex-credits-serving")
                .and_then(|v| v.parse().ok())
                .or_else(|| (header(h, "x-codex-active-limit") == Some("credits")).then_some(true));
            for slot in ["primary", "secondary"] {
                let prefix = format!("x-codex-{slot}");
                let used_pct = number(h, &format!("{prefix}-used-percent")).filter(|v| *v >= 0.0);
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
                if used_pct.is_some() || reset_at.is_some_and(|reset| reset > now) {
                    windows.push(Window {
                        name,
                        used_pct,
                        reset_at,
                        observed_at: used_pct.map(|_| now),
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
    dashboard: Option<Arc<crate::dashboard::runtime::Runtime>>,
    activity_plan: crate::dashboard::history::WriteBehind,
    usage_plan: crate::dashboard::history::WriteBehind,
    dashboard_clock: Instant,

    home: PathBuf,
    state: QuotaState,
    quota_dirty: bool,
    quota_frozen: bool,
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
            dashboard: None,
            activity_plan: Default::default(),
            usage_plan: Default::default(),
            dashboard_clock: now,
            home,
            state,
            quota_dirty: false,
            quota_frozen: false,
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
            Command::PollQuota(id, next) => {
                // observe_poll already merged this snapshot and cleared stale
                // limit signals; a second merge must not resurrect them.
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
            Command::Flush(_) | Command::FreezeQuota(_) | Command::Stop => unreachable!(),
        };
        if result.is_err() {
            tracing::warn!("Observability write failed; later events will retry");
            self.error = true;
        }
    }
    fn dashboard_persist(&mut self, force: bool) -> Result<(), String> {
        let Some(runtime) = &self.dashboard else {
            return Ok(());
        };
        let now = u64::try_from(self.dashboard_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (activity_revision, usage_revision) = runtime.revisions();
        let activity = self.activity_plan.offer(activity_revision, now, true);
        let usage = self.usage_plan.offer(usage_revision, now, force);
        if activity.is_none() && usage.is_none() {
            return Ok(());
        }
        // One clone per offered kind, no snapshot-bearing commands in the queue.
        let archive = runtime.archive(activity.is_some(), usage.is_some());
        let mut success = true;
        if let (Some(revision), Some(snapshot)) = (activity, archive.activity) {
            let written = snapshot.compact_jsonl().ok().is_some_and(|bytes| {
                atomic_write(&self.home.join("state/activity.jsonl"), &bytes).is_ok()
            });
            self.activity_plan.complete(revision, written, now);
            success &= written;
        }
        if let (Some(revision), Some(snapshot)) = (usage, archive.usage) {
            let written = snapshot.snapshot_json().ok().is_some_and(|bytes| {
                atomic_write(&self.home.join("state/usage-history.json"), &bytes).is_ok()
            });
            self.usage_plan.complete(revision, written, now);
            success &= written;
        }
        if success {
            Ok(())
        } else {
            Err("dashboard archive write failed".into())
        }
    }
    fn persist(&mut self, now: Instant) {
        if self.quota_frozen {
            self.quota_dirty = false;
        }
        if now.duration_since(self.last_prune) >= WARN_INTERVAL {
            prune(&self.home.join("ledger"));
            self.last_prune = now;
        }
        if now.duration_since(self.last_sync) < SYNC_INTERVAL {
            return;
        }
        self.last_sync = now;
        if !self.quota_frozen && self.dashboard_persist(false).is_err() {
            tracing::warn!("Dashboard persistence failed; next batch will retry");
        }
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
        self.dashboard_persist(true)?;
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
fn run(
    home: PathBuf,
    state: QuotaState,
    rx: mpsc::Receiver<Command>,
    dropped: Arc<AtomicU64>,
    dashboard: Arc<crate::dashboard::runtime::Runtime>,
) {
    let mut writer = Writer::new(home, state);
    writer.dashboard = Some(dashboard.clone());
    let mut warnings = DropWarnings {
        last: Instant::now() - WARN_INTERVAL,
    };
    loop {
        match rx.recv_timeout(SYNC_INTERVAL.saturating_sub(writer.last_sync.elapsed())) {
            Ok(Command::Flush(reply)) => {
                let _ = reply.send(writer.flush());
            }
            Ok(Command::FreezeQuota(reply)) => {
                dashboard.freeze();
                let result = writer.flush();
                if result.is_ok() {
                    writer.quota_frozen = true;
                }
                let _ = reply.send(result);
            }
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                dashboard.freeze();
                if writer.flush().is_err() {
                    tracing::warn!("Observability shutdown flush failed");
                }
                break;
            }
            Ok(command) => writer.event(command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        dashboard.tick();
        writer.persist(Instant::now());
        warnings.report(&dropped, Instant::now());
    }
}
#[cfg(test)]
mod r7_archive_tests {
    use super::*;
    use crate::dashboard::history::*;
    #[test]
    fn r7_credential_activity_maps_checked_successes_without_hiding_real_failures() {
        use crate::dashboard::RequestOutcome;
        let temp = tempfile::tempdir().unwrap();
        let obs = Observability::new(temp.path(), None).unwrap();
        let account = "cx-123abc";
        for outcome in ["updated", "unchanged", "success"] {
            obs.record_reload(account, "selection", outcome, None);
        }
        obs.record_refresh(account, "manual", "success", None);
        let events = obs.dashboard().activity_page(None, 200, 7).unwrap().events;
        assert_eq!(events.len(), 4);
        assert!(
            events
                .iter()
                .all(|event| event.data.outcome == Some(RequestOutcome::Ok)
                    && !event.notable
                    && !event.data.recovered)
        );
        for outcome in ["not_sent", "401", "http_401", "link_stale", "link_invalid"] {
            obs.record_reload(account, "selection", outcome, None);
            obs.record_refresh(account, "manual", outcome, None);
        }
        let events = obs.dashboard().activity_page(None, 200, 7).unwrap().events;
        let errors: Vec<_> = events.iter().filter(|event| event.seq > 4).collect();
        assert_eq!(errors.len(), 10);
        assert!(errors.iter().all(|event| event.data.outcome
            == Some(RequestOutcome::UpstreamError)
            && event.notable));
        // Domain-specific success names must not accidentally mark an unsent
        // refresh as successful; unchanged is only a successful linked check.
        obs.record_refresh(account, "manual", "unchanged", None);
        obs.record_reload(account, "selection", "unchanged", None);
        let events = obs.dashboard().activity_page(None, 200, 7).unwrap().events;
        let latest = events.iter().max_by_key(|event| event.seq).unwrap();
        assert_eq!(latest.kind, ActivityKind::LinkedReload);
        assert_eq!(latest.data.outcome, Some(RequestOutcome::Ok));
        assert!(latest.data.recovered);
        assert!(!latest.notable);
        let refresh = events
            .iter()
            .filter(|event| event.kind == ActivityKind::TokenRefresh)
            .max_by_key(|event| event.seq)
            .unwrap();
        assert_eq!(refresh.data.outcome, Some(RequestOutcome::UpstreamError));
        assert!(refresh.notable);
    }
    #[test]
    fn r7_archive_failed_atomic_write_retries_latest_revision_and_freeze_blocks_late_changes() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        fs::create_dir(home.join("state")).unwrap();
        let runtime = crate::dashboard::runtime::Runtime::new();
        runtime.account_event(
            ActivityKind::TokenRefresh,
            Provider::Codex,
            Some("cx-123abc"),
            Some(crate::dashboard::RequestOutcome::UpstreamError),
            None,
        );
        let mut writer = Writer::new(home.to_owned(), QuotaState::default());
        writer.dashboard = Some(runtime.clone());
        // Destination is a synthetic directory: atomic replacement must fail, leaving dirty state.
        fs::create_dir(home.join("state/activity.jsonl")).unwrap();
        assert!(writer.flush().is_err());
        assert!(writer.activity_plan.dirty(runtime.revisions().0));
        runtime.account_event(
            ActivityKind::TokenRefresh,
            Provider::Codex,
            Some("cx-123abc"),
            Some(crate::dashboard::RequestOutcome::Ok),
            None,
        );
        runtime.usage(UsageObservation {
            account: "cx-123abc".to_owned().try_into().unwrap(),
            observed_at: Utc::now(),
            window: Some(WindowObservation {
                name: "43200m".to_owned().try_into().unwrap(),
                scope: WindowScope::Account,
                used_pct: Some(42.0),
                reset_at: None,
            }),
            credits: None,
        });
        fs::remove_dir(home.join("state/activity.jsonl")).unwrap();
        runtime.freeze();
        let revisions = runtime.revisions();
        runtime.account_event(
            ActivityKind::Login,
            Provider::Codex,
            Some("cx-123abc"),
            None,
            None,
        );
        assert_eq!(runtime.revisions(), revisions);
        writer.flush().unwrap();
        assert!(!writer.activity_plan.dirty(revisions.0));
        assert!(!writer.usage_plan.dirty(revisions.1));
        let restored = crate::dashboard::runtime::Runtime::new();
        restore_dashboard(home, &restored);
        let events = restored.activity_page(None, 200, 7).unwrap().events;
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|event| event.data.recovered));
        assert_eq!(restored.usage_page(48).unwrap().accounts.len(), 1);
        assert!(fs::read_dir(home.join("state")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }
}

// Credential event results are not request outcomes. In particular, a valid
// linked token with no newer source is an ordinary successful check.
fn refresh_activity_outcome(outcome: &str) -> crate::dashboard::RequestOutcome {
    match outcome {
        "success" | "ok" => crate::dashboard::RequestOutcome::Ok,
        _ => crate::dashboard::RequestOutcome::UpstreamError,
    }
}
fn reload_activity_outcome(outcome: &str) -> crate::dashboard::RequestOutcome {
    match outcome {
        "updated" | "unchanged" | "success" | "ok" => crate::dashboard::RequestOutcome::Ok,
        _ => crate::dashboard::RequestOutcome::UpstreamError,
    }
}

fn provider_for(id: &str) -> Provider {
    if id.starts_with("cl-") {
        Provider::Claude
    } else {
        Provider::Codex
    }
}
fn restore_dashboard(home: &Path, dashboard: &crate::dashboard::runtime::Runtime) {
    use crate::dashboard::history::{ActivityLog, INPUT_BYTES, UsageHistory};
    fn bounded(path: &Path) -> Option<Vec<u8>> {
        use std::io::Read;
        let file = std::fs::File::open(path).ok()?;
        if file.metadata().ok()?.len() > INPUT_BYTES as u64 {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(INPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        (bytes.len() <= INPUT_BYTES).then_some(bytes)
    }
    let now = Utc::now();
    let activity = bounded(&home.join("state/activity.jsonl"))
        .and_then(|bytes| ActivityLog::restore_jsonl(&bytes, now).ok())
        .unwrap_or_default();
    let usage = bounded(&home.join("state/usage-history.json"))
        .and_then(|bytes| UsageHistory::restore_json(&bytes, now).ok())
        .unwrap_or_default();
    dashboard.restore(activity, usage);
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

    #[test]
    fn r13_forecast_observation_uses_fresh_values_and_stays_cold_after_restore() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        let now = Utc::now();
        let start = now - chrono::Duration::minutes(20);
        let reset = now + chrono::Duration::hours(2);
        for (minute, used) in [(0, 10.), (5, 12.), (10, 14.)] {
            obs.observe_poll("cx-123abc", json!({"windows":[{"name":"5h","used_pct":used,"reset_at":reset}],"source":"poll","observed_at":start + chrono::Duration::minutes(minute),"signals":{}}), false).unwrap();
        }
        // A partial poll must not replay the cached primary value at a new time.
        obs.observe_poll("cx-123abc", json!({"windows":[{"name":"7d","used_pct":20.,"reset_at":now+chrono::Duration::days(2)}],"source":"poll","observed_at":now,"signals":{}}), false).unwrap();
        let forecast = obs.dashboard().usage_forecast("cx-123abc", now, None);
        let window = forecast.windows.iter().find(|w| w.name == "5h").unwrap();
        assert_eq!(window.observed_at, start + chrono::Duration::minutes(10));
        assert_eq!(window.recent_pct_per_hour, Some(12.));
        obs.flush().unwrap();
        let restored = Observability::new(dir.path(), None).unwrap();
        assert!(
            restored
                .dashboard()
                .usage_forecast("cx-123abc", now, None)
                .windows
                .is_empty()
        );
    }

    #[test]
    fn binding_moves_hash_identity_and_cache_usage_only_keeps_numbers() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        obs.record_move("claude:private-session", Some("old"), "new", "forced", 123);
        obs.record_move(
            "claude:private-session",
            Some("new"),
            "next",
            "launch_model",
            123,
        );
        let usage = cache_usage(&json!({"response":{"usage":{"input_tokens":100,
            "input_tokens_details":{"cached_tokens":80},"private":"conversation-secret"}}}))
        .unwrap();
        assert_eq!(usage.cached_tokens, Some(80));
        let mut request = record();
        request.cache_usage = Some(usage);
        obs.record_request(request);
        obs.flush().unwrap();
        let text = fs::read_dir(dir.path().join("ledger"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<String>();
        assert!(!text.contains("private-session"));
        assert!(!text.contains("conversation-secret"));
        assert!(text.contains("binding_move") && text.contains("cached_tokens"));
        assert!(text.contains("launch_model"));
    }

    #[test]
    fn input_only_and_split_claude_usage_are_retained_without_summing() {
        let input_only = cache_usage(&json!({"usage":{"input_tokens":100}})).unwrap();
        assert_eq!(input_only.input_tokens, Some(100));
        assert_eq!(input_only.cached_tokens, None);
        assert_eq!(
            reread_accounting::uncached(Provider::Codex, input_only),
            None
        );
        let mut merged = cache_usage(&json!({"message":{"usage":{
            "input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}}));
        let delta = cache_usage(&json!({"usage":{"input_tokens":12}})).unwrap();
        merge_cache_usage(&mut merged, delta);
        merge_cache_usage(&mut merged, delta); // Repeated cumulative update is idempotent.
        let usage = merged.unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cached_tokens,
                usage.cache_creation_input_tokens
            ),
            (Some(12), Some(100), Some(20))
        );
        assert_eq!(
            reread_accounting::uncached(Provider::Claude, usage),
            Some(32)
        );
        assert!(
            cache_usage(&json!({"usage":{"input_tokens":-1,"cached_tokens":"private"}})).is_none()
        );
    }

    #[test]
    fn move_guard_refusals_hash_key_and_only_new_trip_broadcasts() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        let (events, mut receiver) = tokio::sync::broadcast::channel(16);
        obs.set_events(events);
        let mut event = crate::bindings::MoveGuardEvent {
            proposed_reason: "forced".into(),
            move_list: Vec::new(),
            key: "codex:private-move-guard-session".into(),
            from: "a".into(),
            to: "b".into(),
            reason: "global_move_budget".into(),
            until: Utc::now() + Duration::minutes(10),
            newly_tripped: false,
            forced: true,
        };
        obs.record_move_guard(&event);
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        event.newly_tripped = true;
        obs.record_move_guard(&event);
        let warning = receiver.try_recv().unwrap();
        assert_eq!(warning["event"], "core_warning");
        assert_eq!(warning["data"]["kind"], "move_breaker");
        assert_eq!(warning["data"]["forced"], true);
        assert!(!warning.to_string().contains(&event.key));
        obs.flush().unwrap();
        let text = fs::read_dir(dir.path().join("ledger"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<String>();
        assert_eq!(text.matches("binding_move_refused").count(), 2);
        assert!(!text.contains(&event.key));
    }

    #[test]
    fn accounting_hooks_measure_once_and_ledger_association_has_no_raw_key() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        let at = Utc::now();
        let usage = CacheUsage {
            input_tokens: Some(10),
            cached_tokens: Some(0),
            cache_creation_input_tokens: None,
        };
        let baseline = obs
            .accounting_begin(Provider::Codex, "private-canonical-key", "a", Some("m"), at)
            .unwrap();
        obs.accounting_end(baseline, true, Some(&usage), at)
            .unwrap();
        obs.accounting_alias(
            Provider::Codex,
            "private-canonical-key",
            "private-alias-key",
            at,
        )
        .unwrap();
        let id = obs
            .committed_move(Provider::Codex, "private-alias-key", "b", Some("m"), at)
            .unwrap();
        let ticket = obs
            .accounting_begin(Provider::Codex, "private-canonical-key", "b", Some("m"), at)
            .unwrap();
        assert_eq!(
            obs.accounting_end(
                ticket,
                true,
                Some(&CacheUsage {
                    input_tokens: Some(100),
                    ..usage
                }),
                at
            )
            .unwrap(),
            reread_accounting::Completion::Move {
                id,
                reread_tokens: Some(90)
            }
        );
        let snapshot = obs.accounting_snapshot(at);
        assert_eq!(
            (
                snapshot.hour.tokens,
                snapshot.hour.known_count,
                snapshot.hour.unknown_count
            ),
            (90, 1, 0)
        );
        obs.flush().unwrap();
        let text = fs::read_dir(dir.path().join("ledger"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<String>();
        assert!(!text.contains("private-canonical-key") && !text.contains("private-alias-key"));
        assert_eq!(text.matches("binding_move_cost").count(), 2);
        assert!(text.contains("\"reread_tokens\":90"));
    }

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
            rate_wait_ms: 0,
            rate_retries: 0,
            relayed_rate_429s: 0,
            outcome: "ok".into(),
            retained_copy: true,
            upstream_error_code: None,
            attempt_details: Vec::new(),
            cache_usage: None,
            after_move: false,
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
    fn complete_poll_replaces_stale_windows() {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        let first = serde_json::json!({
            "windows": [
                {"name":"5h","used_pct":10.0,"reset_at":null,"observed_at":null,"status":null},
                {"name":"7d","used_pct":20.0,"reset_at":null,"observed_at":null,"status":null}
            ],
            "plan":null,"credits":null,"source":"poll",
            "observed_at":"2030-01-01T00:00:00Z","signals":{}
        });
        let second = serde_json::json!({
            "windows": [
                {"name":"5h","used_pct":11.0,"reset_at":null,"observed_at":null,"status":null}
            ],
            "plan":null,"credits":null,"source":"poll",
            "observed_at":"2030-01-01T00:01:00Z","signals":{}
        });
        obs.observe_poll("cx-synthetic", first, true).unwrap();
        let snapshot = obs.observe_poll("cx-synthetic", second, true).unwrap();
        let windows = snapshot["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0]["name"], "5h");
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
        Command::PollQuota(
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
    fn retained_burst_is_bounded_fresh_and_expires_without_persistence() {
        let (tx, rx) = mpsc::sync_channel(16);
        let obs = Observability {
            dashboard: crate::dashboard::runtime::Runtime::new(),
            accounting: std::sync::Mutex::new(reread_accounting::Registry::new(Utc::now())),
            move_warnings: std::sync::Mutex::new(VecDeque::new()),
            reread_notice_tokens: AtomicU64::new(100_000),
            reread_alerted: std::sync::atomic::AtomicBool::new(false),
            recent_burst: std::sync::Mutex::new(None),
            events: std::sync::Mutex::new(None),
            shadow: std::sync::Mutex::new(crate::routing::forecast::Shadow::default()),
            snapshots: std::sync::Mutex::new(BTreeMap::new()),
            tx,
            worker: None,
            dropped: Arc::new(AtomicU64::new(0)),
            #[cfg(all(feature = "shape-capture", debug_assertions))]
            capture: None,
        };
        assert!(obs.recent_warnings().is_empty());
        obs.burst_warning(31);
        obs.remember_burst(32);
        assert!(
            matches!(rx.try_recv(), Ok(Command::Ledger(value)) if value["burst_new_sessions"] == 31)
        );
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let warnings = obs.recent_warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["burst_new_sessions"], 32);
        assert_eq!(warnings[0].as_object().unwrap().len(), 3);
        obs.recent_burst.lock().unwrap().as_mut().unwrap().0 -= StdDuration::from_secs(600);
        assert!(obs.recent_warnings().is_empty());
        assert!(obs.recent_burst.lock().unwrap().is_none());
        obs.remember_burst(33);
        assert_eq!(obs.recent_warnings()[0]["burst_new_sessions"], 33);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn bounded_queue_drops_without_waiting_and_warns_once_per_minute() {
        // 작성자가 멈춘 큐로 느린 디스크를 결정적으로 재현한다.
        let (tx, rx) = mpsc::sync_channel(2);
        let obs = Observability {
            dashboard: crate::dashboard::runtime::Runtime::new(),
            accounting: std::sync::Mutex::new(reread_accounting::Registry::new(Utc::now())),
            move_warnings: std::sync::Mutex::new(VecDeque::new()),
            reread_notice_tokens: AtomicU64::new(100_000),
            reread_alerted: std::sync::atomic::AtomicBool::new(false),
            recent_burst: std::sync::Mutex::new(None),
            events: std::sync::Mutex::new(None),
            shadow: std::sync::Mutex::new(crate::routing::forecast::Shadow::default()),
            snapshots: std::sync::Mutex::new(BTreeMap::new()),
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
            dashboard: crate::dashboard::runtime::Runtime::new(),
            accounting: std::sync::Mutex::new(reread_accounting::Registry::new(Utc::now())),
            move_warnings: std::sync::Mutex::new(VecDeque::new()),
            reread_notice_tokens: AtomicU64::new(100_000),
            reread_alerted: std::sync::atomic::AtomicBool::new(false),
            recent_burst: std::sync::Mutex::new(None),
            events: std::sync::Mutex::new(None),
            shadow: std::sync::Mutex::new(crate::routing::forecast::Shadow::default()),
            snapshots: std::sync::Mutex::new(BTreeMap::new()),
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
    fn codex_realistic_windows_keep_known_future_resets_without_inventing_usage() {
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
        let reset_only = snapshot(Provider::Codex, &h, now).unwrap();
        assert_eq!(reset_only.windows.len(), 1);
        assert_eq!(reset_only.windows[0].name, "7d");
        assert_eq!(reset_only.windows[0].used_pct, None);
        assert_eq!(reset_only.windows[0].observed_at, None);
        assert_eq!(
            reset_only.windows[0].reset_at,
            DateTime::from_timestamp(2000000000, 0)
        );
        h.remove("x-codex-secondary-reset-at");
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
