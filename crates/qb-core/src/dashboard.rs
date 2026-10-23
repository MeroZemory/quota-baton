//! Dashboard reducers and typed views, with bounded memory and disk-safe history.
pub mod history;
pub mod labels;
mod resolver;
pub mod runtime;

use crate::accounts::Provider;
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use labels::{LabelView, MemoryLabel, OriginalShortId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const GROUP_CAP: usize = 20_000;
pub const LIST_CAP: usize = 2_000;
const REQUEST_CAP: usize = 8_192;
const SOCKET_CAP: usize = 4_096;
const TIMELINE_CAP: usize = 256;
const SUBAGENT_CAP: usize = 256;
pub const METADATA_BYTES: usize = 2 * 1024 * 1024;
pub const RECENT_TOTAL: usize = 4096;
pub const TIMELINE_TOTAL: usize = 8192;
pub const SUBAGENT_TOTAL: usize = 8192;
const DETAIL_LIMITS: [usize; 4] = [METADATA_BYTES, RECENT_TOTAL, TIMELINE_TOTAL, SUBAGENT_TOTAL];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    Identifier,
    Range,
    Capacity,
    Timestamp,
    Number,
    Schema,
}

/// These wrappers admit only local account IDs and ledger hashes, never labels.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountId(String);
impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for AccountId {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() == 9
            && (s.starts_with("cl-") || s.starts_with("cx-"))
            && s[3..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(s))
        } else {
            Err("invalid account id")
        }
    }
}
impl From<AccountId> for String {
    fn from(id: AccountId) -> Self {
        id.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);
impl SessionId {
    pub fn from_key(key: &str) -> Self {
        let digest = Sha256::digest(group_root(key).as_bytes());
        Self(digest[..6].iter().map(|b| format!("{b:02x}")).collect())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for SessionId {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() == 12
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(s))
        } else {
            Err("invalid session hash")
        }
    }
}
impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.0
    }
}

pub fn group_root(key: &str) -> &str {
    if key.starts_with("claude:") {
        key.split_once(":agent:").map_or(key, |(root, _)| root)
    } else {
        key
    }
}

fn safe_product(value: &str) -> Option<String> {
    if value.is_empty()
        || value.len() > 100
        || value.contains("sk-")
        || value.contains("eyJ")
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        None
    } else {
        Some(value.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    CodexCli,
    CodexVscode,
    CodexExec,
    CodexDesktop,
    ClaudeCli,
    ClaudeVscode,
    ClaudeSdk,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientView {
    pub kind: ClientKind,
    pub product: Option<String>,
    pub version: Option<String>,
}
impl Default for ClientView {
    fn default() -> Self {
        Self {
            kind: ClientKind::Unknown,
            product: None,
            version: None,
        }
    }
}
impl ClientView {
    pub fn from_headers(originator: Option<&str>, ua: Option<&str>, app: Option<&str>) -> Self {
        let ua = ua.filter(|v| v.len() <= 1024).unwrap_or("");
        let token = ua
            .split_whitespace()
            .next()
            .filter(|v| v.bytes().filter(|b| *b == b'/').count() <= 1)
            .unwrap_or("");
        let (product, version) = token
            .split_once('/')
            .map_or((token, None), |(a, b)| (a, Some(b)));
        let origin = originator.filter(|v| v.len() <= 100).unwrap_or("");
        let kind = match origin {
            "codex_cli_rs" | "codex-tui" => ClientKind::CodexCli,
            "codex_vscode" => ClientKind::CodexVscode,
            "codex_exec" => ClientKind::CodexExec,
            "Codex Desktop" => ClientKind::CodexDesktop,
            other if other.starts_with("codex_desktop") => ClientKind::CodexDesktop,
            _ if product == "claude-cli"
                && (app == Some("vscode") || ua.contains("claude-vscode")) =>
            {
                ClientKind::ClaudeVscode
            }
            _ if product == "claude-cli" && ua.contains("sdk-") => ClientKind::ClaudeSdk,
            _ if product == "claude-cli" && ua.contains("external, cli") => ClientKind::ClaudeCli,
            _ => ClientKind::Unknown,
        };
        Self {
            kind,
            product: safe_product(if origin.is_empty() { product } else { origin })
                .or_else(|| safe_product(product)),
            version: version.and_then(safe_product),
        }
    }
}

/// Input from the binding table, AFTER releasing its lock. No raw key/path here.
#[derive(Clone, PartialEq, Eq)]
pub struct BindingGroup {
    pub id: SessionId,
    pub provider: Provider,
    pub account: AccountId,
    pub first_seen_at: Option<DateTime<Utc>>,
    pub bound_since: Option<DateTime<Utc>>,
    pub last_used_at: DateTime<Utc>,
    pub active: bool,
    pub marked_for_move: bool,
    pub client_kind: Option<ClientKind>,
    pub restored: bool,
}

/// Memory evidence must never be passed to a persistence/ledger command.
#[derive(Default)]
pub struct PrivateEvidence {
    label: Option<MemoryLabel>,
    original: Option<OriginalShortId>,
}
impl PrivateEvidence {
    pub fn new(label: Option<MemoryLabel>, original: Option<OriginalShortId>) -> Self {
        Self { label, original }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Http,
    Websocket,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    Ok,
    RateHoldOk,
    UpstreamError,
    ClientDrop,
    RelayedRate429,
    NetworkError,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveReason {
    New,
    LaunchModel,
    ModelChoice,
    Forced,
    FiveHourFarMove,
    ColdRebalance,
    ResetHoldTimeout,
    Overload,
}
impl MoveReason {
    fn is_initial_placement(self) -> bool {
        matches!(self, Self::New | Self::LaunchModel)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheResult {
    Hit,
    Miss,
}

#[derive(Clone, Serialize)]
pub struct MoveView {
    pub at: DateTime<Utc>,
    pub from: Option<AccountId>,
    pub to: AccountId,
    pub reason: MoveReason,
    pub next_turn_cache: Option<CacheResult>,
}
#[derive(Clone, Serialize)]
pub struct TimelineView {
    pub at: DateTime<Utc>,
    pub kind: &'static str,
    pub account: AccountId,
    pub from: Option<AccountId>,
    pub reason: MoveReason,
    pub next_turn_cache: Option<CacheResult>,
}
#[derive(Clone, Serialize)]
pub struct FailoverView {
    pub from: AccountId,
    pub to: AccountId,
}
#[derive(Clone, Serialize)]
pub struct RecentRequest {
    pub ts: DateTime<Utc>,
    pub outcome: RequestOutcome,
    pub status: u16,
    pub ttfb_ms: u64,
    pub dur_ms: u64,
    pub rate_wait_ms: u64,
    pub rate_retries: u32,
    pub failover: Option<FailoverView>,
}
pub struct RequestStart {
    pub id: u64,
    pub at: DateTime<Utc>,
    pub transport: Transport,
}
pub struct RequestEnd {
    pub at: DateTime<Utc>,
    pub outcome: RequestOutcome,
    pub status: u16,
    pub ttfb_ms: u64,
    pub dur_ms: u64,
    pub rate_wait_ms: u64,
    pub rate_retries: u32,
    pub failover: Option<FailoverView>,
}
struct LiveRequest {
    session: SessionId,
    start: RequestStart,
    day: NaiveDate,
    waiting: Option<DateTime<Utc>>,
}

struct DayCounts {
    date: NaiveDate,
    requests: u64,
    failures: u64,
    waits: u64,
    wait_ms: u64,
    complete: bool,
}
struct SessionStats {
    binding: BindingGroup,
    private: PrivateEvidence,
    client: ClientView,
    model: Option<String>,
    day: DayCounts,
    subagents: BTreeSet<SessionId>,
    recent: VecDeque<RecentRequest>,
    timeline: VecDeque<TimelineView>,
    first_account: Option<AccountId>,
    truncated: bool,
    live: BTreeSet<u64>,
    sockets: BTreeSet<u64>,
    lookup: labels::LookupBudget,
    retained: [usize; 4],
    stamps: [u64; 4],
    retention_protected: bool,
    inflight_known: bool,
    metadata_truncated: bool,
    recent_known: bool,
    timeline_known: bool,
    subagents_known: bool,
}
impl SessionStats {
    fn retained_usage(&self) -> [usize; 4] {
        let field = |v: &Option<String>| v.as_ref().map_or(0, |s| s.capacity().saturating_add(64));
        [
            self.private
                .label
                .as_ref()
                .map_or(0, MemoryLabel::retained_bytes)
                + self
                    .private
                    .original
                    .as_ref()
                    .map_or(0, OriginalShortId::retained_bytes)
                + field(&self.client.product)
                + field(&self.client.version)
                + field(&self.model),
            self.recent.len(),
            self.timeline.len(),
            self.subagents.len(),
        ]
    }
    fn roll_day(&mut self, day: NaiveDate) {
        if day > self.day.date {
            self.day = DayCounts {
                date: day,
                requests: 0,
                failures: 0,
                waits: 0,
                wait_ms: 0,
                complete: true,
            };
            self.subagents.clear();
            self.timeline.clear();
            self.timeline.shrink_to_fit();
            self.first_account = None;
            self.timeline_known = true;
            self.subagents_known = true;
        }
    }
}

#[derive(Serialize)]
pub struct InflightView {
    pub requests: usize,
    pub oldest_started_at: Option<DateTime<Utc>>,
    pub ws_open: usize,
    pub ws_response_in_flight: bool,
}
#[derive(Serialize)]
pub struct PacingWait {
    pub waiting: usize,
    pub since: DateTime<Utc>,
}
#[derive(Serialize)]
pub struct RateWaitToday {
    pub count: u64,
    pub total_ms: u64,
}
/// Native binding-row authority, distinct from the short dashboard identity.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SessionRouting {
    pub action_id: String,
    pub on_5h_limit: crate::routing::On5hLimit,
    pub mode: crate::routing::Mode,
}
/// Actual live admitted quota waits; rate pacing is a separate observation.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct QuotaWaitView {
    pub waiting: u32,
    pub reset_at: DateTime<Utc>,
}
#[derive(Serialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub provider: Provider,
    pub client: ClientView,
    pub label: Option<LabelView>,
    pub account: AccountId,
    pub first_seen_at: Option<DateTime<Utc>>,
    pub bound_since: Option<DateTime<Utc>>,
    pub last_used_at: DateTime<Utc>,
    pub active: bool,
    pub inflight: InflightView,
    pub inflight_known: bool,
    pub pacing_wait: Option<PacingWait>,
    pub routing: Option<SessionRouting>,
    pub quota_wait: Option<QuotaWaitView>,
    pub quota_wait_known: bool,
    pub model: Option<String>,
    pub subagents_today: usize,
    pub requests_today: u64,
    pub failures_today: u64,
    pub rate_wait_today: RateWaitToday,
    pub moves_today: Vec<MoveView>,
    pub first_account_today: Option<AccountId>,
    pub marked_for_move: bool,
    pub counters_complete: bool,
    pub truncated: bool,
    pub subagents_known: bool,
    pub moves_known: bool,
    pub metadata_truncated: bool,
}
#[derive(Serialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub session: SessionSummary,
    pub original_short_id: Option<String>,
    pub recent_requests: Vec<RecentRequest>,
    pub timeline: Vec<TimelineView>,
    pub recent_requests_known: bool,
    pub timeline_known: bool,
}
#[derive(Serialize)]
pub struct SessionsView {
    pub move_cost: Option<crate::observability::reread_accounting::Snapshot>,
    pub now: DateTime<Utc>,
    pub active_window_secs: u64,
    pub sessions: Vec<SessionSummary>,
    pub truncated: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRange {
    Active,
    Hour,
    Today,
    All,
}
impl SessionRange {
    pub fn parse(value: Option<&str>) -> Result<Self, Invalid> {
        match value.unwrap_or("hour") {
            "active" => Ok(Self::Active),
            "hour" => Ok(Self::Hour),
            "today" => Ok(Self::Today),
            "all" => Ok(Self::All),
            _ => Err(Invalid::Range),
        }
    }
}
#[derive(Serialize)]
pub struct SessionChanges {
    pub changed: Vec<SessionId>,
    pub removed: Vec<SessionId>,
    pub resync: bool,
}

pub struct Dashboard {
    sessions: BTreeMap<SessionId, SessionStats>,
    requests: BTreeMap<u64, LiveRequest>,
    sockets: BTreeMap<u64, SessionId>,
    changed: BTreeSet<SessionId>,
    removed: BTreeSet<SessionId>,
    next_emit_ms: Option<u64>,
    resync: bool,
    offset: FixedOffset,
    active_window_secs: u64,
    detail_usage: [usize; 4],
    detail_lru: [BTreeSet<(bool, u64, SessionId)>; 4],
    retention_tick: u64,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct RetentionUsage {
    pub metadata_bytes: usize,
    pub recent_requests: usize,
    pub timeline_entries: usize,
    pub subagent_hashes: usize,
}
impl Dashboard {
    pub fn new(offset: FixedOffset, active_window_secs: u64) -> Self {
        Self {
            sessions: BTreeMap::new(),
            requests: BTreeMap::new(),
            sockets: BTreeMap::new(),
            changed: BTreeSet::new(),
            removed: BTreeSet::new(),
            next_emit_ms: None,
            resync: false,
            offset,
            active_window_secs,
            detail_usage: [0; 4],
            detail_lru: std::array::from_fn(|_| BTreeSet::new()),
            retention_tick: 0,
        }
    }
    fn date(&self, at: DateTime<Utc>) -> NaiveDate {
        at.with_timezone(&self.offset).date_naive()
    }
    fn dirty(&mut self, id: &SessionId) {
        self.retain_details(id);
        self.notify_changed(id);
    }
    fn notify_changed(&mut self, id: &SessionId) {
        if self.resync {
            return;
        }
        self.removed.remove(id);
        self.changed.insert(id.clone());
        self.bound_changes();
    }
    // Four shared budgets, not per-group budgets multiplied by GROUP_CAP.
    // Quiet groups precede active/live groups; if all groups are busy, only
    // display detail is evicted. Live handles, pins and counters never are.
    fn retain_details(&mut self, id: &SessionId) {
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        self.retention_tick = self.retention_tick.saturating_add(1);
        let usage = s.retained_usage();
        let protected = s.binding.active || !s.live.is_empty() || !s.sockets.is_empty();
        for (kind, new) in usage.into_iter().enumerate() {
            if s.retained[kind] > 0 {
                self.detail_lru[kind].remove(&(s.retention_protected, s.stamps[kind], id.clone()));
            }
            self.detail_usage[kind] = self.detail_usage[kind]
                .saturating_sub(s.retained[kind])
                .saturating_add(new);
            s.retained[kind] = new;
            s.stamps[kind] = self.retention_tick;
            if new > 0 {
                self.detail_lru[kind].insert((protected, s.stamps[kind], id.clone()));
            }
        }
        s.retention_protected = protected;
        for (kind, limit) in DETAIL_LIMITS.into_iter().enumerate() {
            while self.detail_usage[kind] > limit {
                let Some((protected, stamp, victim)) = self.detail_lru[kind].pop_first() else {
                    break;
                };
                let Some(s) = self.sessions.get_mut(&victim) else {
                    continue;
                };
                let before = s.retained[kind];
                match kind {
                    0 => {
                        s.private = PrivateEvidence::default();
                        s.client.product = None;
                        s.client.version = None;
                        s.model = None;
                        s.metadata_truncated = true;
                    }
                    1 => {
                        s.recent.pop_front();
                        s.recent.shrink_to_fit();
                        s.recent_known = false;
                    }
                    2 => {
                        s.timeline.pop_front();
                        s.timeline.shrink_to_fit();
                        s.timeline_known = false;
                    }
                    3 => {
                        s.subagents.pop_first();
                        s.subagents_known = false;
                    }
                    _ => unreachable!(),
                }
                s.truncated = true;
                let after = s.retained_usage()[kind];
                s.retained[kind] = after;
                self.detail_usage[kind] = self.detail_usage[kind]
                    .saturating_sub(before)
                    .saturating_add(after);
                if after > 0 {
                    self.detail_lru[kind].insert((protected, stamp, victim.clone()));
                }
                self.notify_changed(&victim);
            }
        }
    }
    fn remove_session(&mut self, id: &SessionId) {
        if let Some(s) = self.sessions.remove(id) {
            for kind in 0..4 {
                self.detail_usage[kind] = self.detail_usage[kind].saturating_sub(s.retained[kind]);
                if s.retained[kind] > 0 {
                    self.detail_lru[kind].remove(&(
                        s.retention_protected,
                        s.stamps[kind],
                        id.clone(),
                    ));
                }
            }
        }
    }
    pub fn retention_usage(&self) -> RetentionUsage {
        RetentionUsage {
            metadata_bytes: self.detail_usage[0],
            recent_requests: self.detail_usage[1],
            timeline_entries: self.detail_usage[2],
            subagent_hashes: self.detail_usage[3],
        }
    }
    fn bound_changes(&mut self) {
        if self.changed.len() + self.removed.len() > GROUP_CAP * 2 {
            self.changed.clear();
            self.removed.clear();
            self.resync = true;
        }
    }
    pub fn sync_bindings(
        &mut self,
        mut bindings: Vec<BindingGroup>,
        now: DateTime<Utc>,
    ) -> Result<(), Invalid> {
        if bindings.len() > GROUP_CAP {
            return Err(Invalid::Capacity);
        }
        let mut keep = BTreeSet::new();
        if bindings.iter().any(|b| !keep.insert(b.id.clone())) {
            return Err(Invalid::Identifier);
        }
        let protected_extra = self
            .sessions
            .iter()
            .filter(|(id, s)| !keep.contains(*id) && (!s.live.is_empty() || !s.sockets.is_empty()))
            .count();
        if keep.len() + protected_extra > GROUP_CAP {
            return Err(Invalid::Capacity);
        }
        for b in bindings.drain(..) {
            self.upsert_binding(b, now)?;
        }
        let expired = self
            .sessions
            .iter()
            .filter(|(id, s)| !keep.contains(*id) && s.live.is_empty() && s.sockets.is_empty())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            self.remove_session(&id);
            self.changed.remove(&id);
            self.removed.insert(id);
            self.bound_changes();
        }
        Ok(())
    }
    pub fn upsert_binding(&mut self, b: BindingGroup, now: DateTime<Utc>) -> Result<(), Invalid> {
        if !self.sessions.contains_key(&b.id) && self.sessions.len() >= GROUP_CAP {
            return Err(Invalid::Capacity);
        }
        let day = self.date(now);
        let id = b.id.clone();
        let changed = if let Some(s) = self.sessions.get_mut(&id) {
            let changed = s.binding != b || day > s.day.date;
            s.roll_day(day);
            s.binding = b;
            changed
        } else {
            let complete = !b.restored;
            let client = ClientView {
                kind: b.client_kind.unwrap_or(ClientKind::Unknown),
                ..ClientView::default()
            };
            self.sessions.insert(
                id.clone(),
                SessionStats {
                    binding: b,
                    private: PrivateEvidence::default(),
                    client,
                    model: None,
                    day: DayCounts {
                        date: day,
                        requests: 0,
                        failures: 0,
                        waits: 0,
                        wait_ms: 0,
                        complete,
                    },
                    subagents: BTreeSet::new(),
                    recent: VecDeque::new(),
                    timeline: VecDeque::new(),
                    first_account: None,
                    truncated: false,
                    live: BTreeSet::new(),
                    sockets: BTreeSet::new(),
                    lookup: labels::LookupBudget::default(),
                    retained: [0; 4],
                    stamps: [0; 4],
                    retention_protected: false,
                    inflight_known: true,
                    metadata_truncated: false,
                    recent_known: complete,
                    timeline_known: complete,
                    subagents_known: complete,
                },
            );
            true
        };
        if changed {
            self.dirty(&id);
        }
        Ok(())
    }
    pub fn evidence(
        &mut self,
        id: &SessionId,
        evidence: PrivateEvidence,
        client: Option<ClientView>,
    ) {
        if let Some(s) = self.sessions.get_mut(id) {
            let label_changed =
                MemoryLabel::replace_if_better(&mut s.private.label, evidence.label);
            let client_changed = client.is_some();
            if evidence.original.is_some() {
                s.private.original = evidence.original;
            }
            if let Some(client) = client {
                s.client = ClientView {
                    kind: client.kind,
                    product: client.product.as_deref().and_then(safe_product),
                    version: client.version.as_deref().and_then(safe_product),
                };
            }
            if label_changed || s.private.original.is_some() || client_changed {
                self.dirty(id);
            }
        }
    }
    pub fn claude_lookup_admit(&mut self, id: &SessionId, now_ms: u64) -> bool {
        self.sessions.get_mut(id).is_some_and(|s| {
            s.binding.provider == Provider::Claude
                && s.private.label.is_none()
                && s.lookup.admit(now_ms)
        })
    }
    pub fn display_model(&mut self, id: &SessionId, value: Option<&str>) {
        let Some(next) = value.and_then(safe_product) else {
            return;
        };
        if let Some(s) = self.sessions.get_mut(id)
            && s.model.as_ref() != Some(&next)
        {
            s.model = Some(next);
            self.dirty(id);
        }
    }
    pub fn subagent(&mut self, id: &SessionId, agent_key: &str, now: DateTime<Utc>) {
        let day = self.date(now);
        if let Some(s) = self.sessions.get_mut(id) {
            s.roll_day(day);
            // Hash the distinct agent identifier itself, not its group root.
            let digest = Sha256::digest(agent_key.as_bytes());
            let child = SessionId(digest[..6].iter().map(|b| format!("{b:02x}")).collect());
            if !s.subagents.contains(&child) {
                if s.subagents.len() < SUBAGENT_CAP {
                    s.subagents.insert(child);
                } else {
                    s.truncated = true;
                    s.subagents_known = false;
                }
            }
            self.dirty(id);
        }
    }
    pub fn start(&mut self, id: &SessionId, request: RequestStart) -> Result<(), Invalid> {
        if self.requests.len() >= REQUEST_CAP {
            if let Some(s) = self.sessions.get_mut(id) {
                s.inflight_known = false;
                s.truncated = true;
                s.day.complete = false;
                self.dirty(id);
            }
            return Err(Invalid::Capacity);
        }
        if self.requests.contains_key(&request.id) {
            return Err(Invalid::Identifier);
        }
        let day = self.date(request.at);
        let s = self.sessions.get_mut(id).ok_or(Invalid::Identifier)?;
        s.roll_day(day);
        if s.day.date == day {
            s.day.requests = s.day.requests.saturating_add(1);
        }
        s.live.insert(request.id);
        self.requests.insert(
            request.id,
            LiveRequest {
                session: id.clone(),
                start: request,
                day,
                waiting: None,
            },
        );
        self.dirty(id);
        Ok(())
    }
    pub fn pacing(&mut self, request: u64, since: Option<DateTime<Utc>>) {
        let id = if let Some(r) = self.requests.get_mut(&request) {
            r.waiting = since;
            Some(r.session.clone())
        } else {
            None
        };
        if let Some(id) = id {
            self.dirty(&id);
        }
    }
    pub fn finish(&mut self, request: u64, end: RequestEnd) -> bool {
        let Some(r) = self.requests.remove(&request) else {
            return false;
        };
        let today = self.date(end.at);
        if let Some(s) = self.sessions.get_mut(&r.session) {
            s.live.remove(&request);
            s.roll_day(today);
            if s.day.date == r.day {
                if !matches!(end.outcome, RequestOutcome::Ok | RequestOutcome::RateHoldOk) {
                    s.day.failures = s.day.failures.saturating_add(1);
                }
                if end.rate_wait_ms > 0 || end.rate_retries > 0 {
                    s.day.waits = s.day.waits.saturating_add(1);
                    s.day.wait_ms = s.day.wait_ms.saturating_add(end.rate_wait_ms);
                }
            }
            s.recent.push_back(RecentRequest {
                ts: r.start.at,
                outcome: end.outcome,
                status: end.status,
                ttfb_ms: end.ttfb_ms,
                dur_ms: end.dur_ms,
                rate_wait_ms: end.rate_wait_ms,
                rate_retries: end.rate_retries,
                failover: end.failover,
            });
            while s.recent.len() > 10 {
                s.recent.pop_front();
            }
            if s.recent.len() == 10 {
                s.recent_known = true;
            }
        }
        self.dirty(&r.session);
        true
    }
    pub fn socket_open(&mut self, socket: u64, id: &SessionId) -> Result<(), Invalid> {
        if self.sockets.len() >= SOCKET_CAP {
            if let Some(s) = self.sessions.get_mut(id) {
                s.inflight_known = false;
                s.truncated = true;
                self.dirty(id);
            }
            return Err(Invalid::Capacity);
        }
        if !self.sessions.contains_key(id) || self.sockets.contains_key(&socket) {
            return Err(Invalid::Identifier);
        }
        self.sockets.insert(socket, id.clone());
        self.sessions
            .get_mut(id)
            .expect("validated session")
            .sockets
            .insert(socket);
        self.dirty(id);
        Ok(())
    }
    pub fn socket_close(&mut self, socket: u64) {
        if let Some(id) = self.sockets.remove(&socket) {
            if let Some(s) = self.sessions.get_mut(&id) {
                s.sockets.remove(&socket);
            }
            self.dirty(&id);
        }
    }
    pub fn placed(&mut self, id: &SessionId, mv: MoveView) {
        let day = self.date(mv.at);
        if let Some(s) = self.sessions.get_mut(id) {
            s.roll_day(day);
            if day != s.day.date {
                return;
            }
            s.first_account
                .get_or_insert_with(|| mv.from.clone().unwrap_or_else(|| mv.to.clone()));
            s.timeline.push_back(TimelineView {
                at: mv.at,
                kind: if mv.reason.is_initial_placement() {
                    "placed"
                } else {
                    "move"
                },
                account: mv.to,
                from: mv.from,
                reason: mv.reason,
                next_turn_cache: mv.next_turn_cache,
            });
            while s.timeline.len() > TIMELINE_CAP {
                s.timeline.pop_front();
                s.truncated = true;
                s.timeline_known = false;
            }
            self.dirty(id);
        }
    }
    /// Only the existing after_move completion marker may supply this fact.
    pub fn move_cache_result(
        &mut self,
        id: &SessionId,
        moved_at: DateTime<Utc>,
        result: CacheResult,
    ) {
        if let Some(s) = self.sessions.get_mut(id)
            && let Some(mv) = s.timeline.iter_mut().find(|m| {
                m.at == moved_at && !m.reason.is_initial_placement() && m.next_turn_cache.is_none()
            })
        {
            mv.next_turn_cache = Some(result);
            self.dirty(id);
        }
    }
    fn summary(&self, id: &SessionId, s: &SessionStats, now: DateTime<Utc>) -> SessionSummary {
        let current_day = self.date(now) == s.day.date;
        let live = s
            .live
            .iter()
            .filter_map(|request| self.requests.get(request))
            .collect::<Vec<_>>();
        let waits = live.iter().filter_map(|r| r.waiting).collect::<Vec<_>>();
        SessionSummary {
            id: id.clone(),
            provider: s.binding.provider,
            client: s.client.clone(),
            label: s.private.label.as_ref().map(MemoryLabel::view),
            account: s.binding.account.clone(),
            first_seen_at: s.binding.first_seen_at,
            bound_since: s.binding.bound_since,
            last_used_at: s.binding.last_used_at,
            active: s.binding.active,
            inflight_known: s.inflight_known,
            inflight: InflightView {
                requests: live.len(),
                oldest_started_at: live.iter().map(|r| r.start.at).min(),
                ws_open: s.sockets.len(),
                ws_response_in_flight: live
                    .iter()
                    .any(|r| r.start.transport == Transport::Websocket),
            },
            pacing_wait: waits.iter().min().map(|since| PacingWait {
                waiting: waits.len(),
                since: *since,
            }),
            // Runtime enriches only from current native binding/owner state.
            routing: None,
            quota_wait: None,
            quota_wait_known: false,
            model: s.model.clone(),
            subagents_today: if current_day { s.subagents.len() } else { 0 },
            requests_today: if current_day { s.day.requests } else { 0 },
            failures_today: if current_day { s.day.failures } else { 0 },
            rate_wait_today: RateWaitToday {
                count: if current_day { s.day.waits } else { 0 },
                total_ms: if current_day { s.day.wait_ms } else { 0 },
            },
            moves_today: if current_day {
                s.timeline
                    .iter()
                    .filter(|m| !m.reason.is_initial_placement())
                    .map(|m| MoveView {
                        at: m.at,
                        from: m.from.clone(),
                        to: m.account.clone(),
                        reason: m.reason,
                        next_turn_cache: m.next_turn_cache,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            first_account_today: current_day.then(|| s.first_account.clone()).flatten(),
            marked_for_move: s.binding.marked_for_move,
            counters_complete: !current_day || s.day.complete,
            truncated: s.truncated,
            subagents_known: !current_day || s.subagents_known,
            moves_known: !current_day || s.timeline_known,
            metadata_truncated: s.metadata_truncated,
        }
    }
    pub fn sessions(&self, range: SessionRange, now: DateTime<Utc>) -> SessionsView {
        let today = self.date(now);
        let mut sessions = self
            .sessions
            .iter()
            .map(|(id, s)| self.summary(id, s, now))
            .filter(|s| match range {
                SessionRange::Active => s.active,
                SessionRange::Hour => {
                    s.inflight.requests > 0 || s.last_used_at >= now - chrono::Duration::hours(1)
                }
                SessionRange::Today => self.date(s.last_used_at) == today,
                SessionRange::All => true,
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| {
            (b.inflight.requests > 0)
                .cmp(&(a.inflight.requests > 0))
                .then_with(|| b.last_used_at.cmp(&a.last_used_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        let truncated = sessions.len() > LIST_CAP;
        sessions.truncate(LIST_CAP);
        SessionsView {
            move_cost: None,
            now,
            active_window_secs: self.active_window_secs,
            sessions,
            truncated,
        }
    }
    pub fn detail(&self, id: &SessionId, now: DateTime<Utc>) -> Option<SessionDetail> {
        let s = self.sessions.get(id)?;
        Some(SessionDetail {
            session: self.summary(id, s, now),
            original_short_id: s
                .private
                .original
                .as_ref()
                .map(OriginalShortId::detail_value),
            recent_requests: s.recent.iter().rev().cloned().collect(),
            timeline: if self.date(now) == s.day.date {
                s.timeline.iter().cloned().collect()
            } else {
                Vec::new()
            },
            recent_requests_known: s.recent_known,
            timeline_known: self.date(now) != s.day.date || s.timeline_known,
        })
    }
    /// Caller supplies monotonic elapsed milliseconds; a wall clock jump cannot stall SSE.
    pub fn take_changes(&mut self, now_ms: u64) -> Option<SessionChanges> {
        if (!self.resync && self.changed.is_empty() && self.removed.is_empty())
            || self.next_emit_ms.is_some_and(|next| now_ms < next)
        {
            return None;
        }
        self.next_emit_ms = Some(now_ms.saturating_add(1000));
        Some(SessionChanges {
            changed: std::mem::take(&mut self.changed).into_iter().collect(),
            removed: std::mem::take(&mut self.removed).into_iter().collect(),
            resync: std::mem::take(&mut self.resync),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> DateTime<Utc> {
        "2026-10-19T14:59:50Z".parse().unwrap()
    }
    fn account() -> AccountId {
        "cx-123abc".to_owned().try_into().unwrap()
    }
    fn binding(key: &str) -> BindingGroup {
        BindingGroup {
            id: SessionId::from_key(key),
            provider: Provider::Codex,
            account: account(),
            first_seen_at: None,
            bound_since: None,
            last_used_at: now(),
            active: true,
            marked_for_move: false,
            client_kind: None,
            restored: true,
        }
    }
    fn model() -> Dashboard {
        Dashboard::new(FixedOffset::east_opt(9 * 3600).unwrap(), 300)
    }
    fn end(at: DateTime<Utc>) -> RequestEnd {
        RequestEnd {
            at,
            outcome: RequestOutcome::Ok,
            status: 200,
            ttfb_ms: 5,
            dur_ms: 100,
            rate_wait_ms: 10,
            rate_retries: 1,
            failover: None,
        }
    }
    #[test]
    fn root_hash_matches_ledger_and_agent_grouping() {
        let root = "claude:private-root";
        let expected: String = Sha256::digest(root.as_bytes())[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            SessionId::from_key("claude:private-root:agent:child").as_str(),
            expected
        );
        assert_ne!(
            SessionId::from_key("codex:x:agent:a"),
            SessionId::from_key("codex:x")
        );
        assert!(serde_json::from_str::<SessionId>("\"RAW-SESSION\"").is_err());
    }
    #[test]
    fn private_label_and_sid_prefix_only_project_into_intended_memory_views() {
        let mut d = model();
        let b = binding("codex:12345678-private-tail");
        let id = b.id.clone();
        d.sync_bindings(vec![b], now()).unwrap();
        d.evidence(
            &id,
            PrivateEvidence::new(
                MemoryLabel::new(
                    "C:\\private-path\\folder-sentinel",
                    labels::LabelSource::Launcher,
                ),
                OriginalShortId::from_root_key("codex:12345678-private-tail"),
            ),
            None,
        );
        let list = serde_json::to_string(&d.sessions(SessionRange::All, now())).unwrap();
        assert!(list.contains("folder-sentinel"));
        assert!(!list.contains("12345678"));
        assert!(!list.contains("private-tail"));
        assert!(!list.contains("private-path"));
        let detail = serde_json::to_value(d.detail(&id, now()).unwrap()).unwrap();
        assert_eq!(detail["original_short_id"], "12345678");
        assert_eq!(detail["counters_complete"], false);
        assert!(detail["first_seen_at"].is_null());
    }
    #[test]
    fn one_request_counts_once_and_drop_completion_is_idempotent() {
        let mut d = model();
        let b = binding("codex:root");
        let id = b.id.clone();
        d.sync_bindings(vec![b], now()).unwrap();
        d.start(
            &id,
            RequestStart {
                id: 1,
                at: now(),
                transport: Transport::Http,
            },
        )
        .unwrap();
        assert_eq!(
            d.start(
                &id,
                RequestStart {
                    id: 1,
                    at: now(),
                    transport: Transport::Http
                }
            ),
            Err(Invalid::Identifier)
        );
        d.pacing(1, Some(now()));
        assert_eq!(
            d.detail(&id, now())
                .unwrap()
                .session
                .pacing_wait
                .unwrap()
                .waiting,
            1
        );
        assert!(d.finish(1, end(now())));
        assert!(!d.finish(1, end(now())));
        let s = d.detail(&id, now()).unwrap();
        assert_eq!(s.session.requests_today, 1);
        assert_eq!(s.session.rate_wait_today.total_ms, 10);
        assert_eq!(s.recent_requests.len(), 1);
    }
    #[test]
    fn local_midnight_does_not_charge_yesterday_completion_to_today() {
        let mut d = model();
        let b = binding("codex:root");
        let id = b.id.clone();
        d.sync_bindings(vec![b], now()).unwrap();
        d.start(
            &id,
            RequestStart {
                id: 1,
                at: now(),
                transport: Transport::Http,
            },
        )
        .unwrap();
        let tomorrow = now() + chrono::Duration::seconds(20);
        let mut failure = end(tomorrow);
        failure.outcome = RequestOutcome::ClientDrop;
        d.finish(1, failure);
        let s = d.detail(&id, tomorrow).unwrap();
        assert_eq!(s.session.requests_today, 0);
        assert_eq!(s.session.failures_today, 0);
        assert!(s.session.counters_complete);
    }
    #[test]
    fn ws_connection_is_not_request_and_authoritative_activity_is_preserved() {
        let mut d = model();
        let mut b = binding("codex:root");
        b.active = false;
        let id = b.id.clone();
        d.sync_bindings(vec![b], now()).unwrap();
        d.socket_open(1, &id).unwrap();
        let s = d.detail(&id, now()).unwrap().session;
        assert_eq!(s.inflight.ws_open, 1);
        assert_eq!(s.requests_today, 0);
        assert!(!s.inflight.ws_response_in_flight);
        assert!(!s.active);
        d.start(
            &id,
            RequestStart {
                id: 2,
                at: now(),
                transport: Transport::Websocket,
            },
        )
        .unwrap();
        assert!(
            d.detail(&id, now())
                .unwrap()
                .session
                .inflight
                .ws_response_in_flight
        );
        d.finish(2, end(now()));
        d.socket_close(1);
        assert_eq!(d.detail(&id, now()).unwrap().session.inflight.ws_open, 0);
    }
    #[test]
    fn ranges_caps_sorting_debounce_and_removal() {
        let mut d = model();
        let mut rows = Vec::new();
        for n in 0..2001 {
            let mut b = binding(&format!("codex:{n}"));
            b.active = n == 0;
            b.last_used_at = now() - chrono::Duration::hours(2);
            rows.push(b);
        }
        let id = rows[0].id.clone();
        d.sync_bindings(rows, now()).unwrap();
        d.start(
            &id,
            RequestStart {
                id: 1,
                at: now(),
                transport: Transport::Http,
            },
        )
        .unwrap();
        assert_eq!(d.sessions(SessionRange::Active, now()).sessions.len(), 1);
        assert_eq!(d.sessions(SessionRange::Hour, now()).sessions.len(), 1);
        let all = d.sessions(SessionRange::All, now());
        assert!(all.truncated);
        assert_eq!(all.sessions.len(), 2000);
        assert_eq!(all.sessions[0].id, id);
        assert!(d.take_changes(0).is_some());
        d.pacing(1, Some(now()));
        assert!(d.take_changes(999).is_none());
        assert!(d.take_changes(1000).is_some());
        d.finish(1, end(now()));
        d.sync_bindings(vec![], now()).unwrap();
        assert!(d.detail(&id, now()).is_none());
        assert!(SessionRange::parse(Some("invalid")).is_err());
    }
    #[test]
    fn recent_requests_and_subagents_are_bounded_and_unknown_model_is_preserved() {
        let mut d = model();
        let b = binding("codex:root");
        let id = b.id.clone();
        d.sync_bindings(vec![b], now()).unwrap();
        for n in 0..300 {
            d.start(
                &id,
                RequestStart {
                    id: n,
                    at: now(),
                    transport: Transport::Http,
                },
            )
            .unwrap();
            d.finish(n, end(now()));
            d.subagent(&id, &format!("agent-{n}"), now());
        }
        d.display_model(&id, Some("gpt-5.5"));
        d.display_model(&id, None);
        d.display_model(&id, Some("secret/path"));
        let s = d.detail(&id, now()).unwrap();
        assert_eq!(s.recent_requests.len(), 10);
        assert_eq!(s.session.subagents_today, 256);
        assert!(s.session.truncated);
        assert_eq!(s.session.model.as_deref(), Some("gpt-5.5"));
    }
    #[test]
    fn r101_known_codex_originators_and_unknown_are_classified_exactly() {
        for (origin, kind) in [
            ("codex-tui", ClientKind::CodexCli),
            ("codex_cli_rs", ClientKind::CodexCli),
            ("codex_vscode", ClientKind::CodexVscode),
            ("codex_exec", ClientKind::CodexExec),
            ("Codex Desktop", ClientKind::CodexDesktop),
            ("codex_desktop_macos", ClientKind::CodexDesktop),
            ("codex-tui-unrecognized", ClientKind::Unknown),
        ] {
            let client = ClientView::from_headers(Some(origin), Some("codex/0.1"), None);
            assert_eq!(client.kind, kind, "{origin}");
            assert_eq!(client.version.as_deref(), Some("0.1"));
        }
    }
    #[test]
    fn client_classification_keeps_only_safe_product_version() {
        assert_eq!(
            ClientView::from_headers(None, Some("claude-cli/2.1 (external, claude-vscode)"), None)
                .kind,
            ClientKind::ClaudeVscode
        );
        assert_eq!(
            ClientView::from_headers(Some("codex_cli_rs"), Some("codex/0.1"), None).kind,
            ClientKind::CodexCli
        );
        assert!(
            ClientView::from_headers(None, Some("secret/path/to/folder"), None)
                .product
                .is_none()
        );
    }
    #[test]
    fn placements_and_moves_keep_todays_bounded_timeline_and_one_cache_result() {
        let mut d = model();
        let b = binding("codex:root");
        let id = b.id.clone();
        d.sync_bindings(vec![b], now()).unwrap();
        d.placed(
            &id,
            MoveView {
                at: now(),
                from: None,
                to: account(),
                reason: MoveReason::New,
                next_turn_cache: None,
            },
        );
        let moved = now() + chrono::Duration::seconds(1);
        d.placed(
            &id,
            MoveView {
                at: moved,
                from: Some(account()),
                to: "cx-111111".to_owned().try_into().unwrap(),
                reason: MoveReason::Overload,
                next_turn_cache: None,
            },
        );
        d.move_cache_result(&id, moved, CacheResult::Hit);
        d.move_cache_result(&id, moved, CacheResult::Miss);
        let detail = d.detail(&id, moved).unwrap();
        assert_eq!(detail.timeline.len(), 2);
        assert_eq!(detail.session.moves_today.len(), 1);
        assert_eq!(
            detail.session.moves_today[0].next_turn_cache,
            Some(CacheResult::Hit)
        );
        assert_eq!(detail.session.first_account_today, Some(account()));
        for _ in 0..300 {
            d.placed(
                &id,
                MoveView {
                    at: moved,
                    from: Some(account()),
                    to: account(),
                    reason: MoveReason::Forced,
                    next_turn_cache: None,
                },
            );
        }
        assert_eq!(d.detail(&id, moved).unwrap().timeline.len(), 256);
        assert!(d.detail(&id, moved).unwrap().session.truncated);
        assert!(
            d.detail(&id, now() + chrono::Duration::days(1))
                .unwrap()
                .timeline
                .is_empty()
        );
    }

    #[test]
    fn model_launch_initial_placement_never_becomes_a_move_or_cache_result() {
        for initial in [MoveReason::New, MoveReason::LaunchModel] {
            let mut d = model();
            let b = binding("codex:model-choice");
            let id = b.id.clone();
            d.sync_bindings(vec![b], now()).unwrap();
            d.placed(
                &id,
                MoveView {
                    at: now(),
                    from: None,
                    to: account(),
                    reason: initial,
                    next_turn_cache: None,
                },
            );
            d.move_cache_result(&id, now(), CacheResult::Hit);
            let detail = d.detail(&id, now()).unwrap();
            assert_eq!(detail.timeline[0].kind, "placed");
            assert_eq!(detail.timeline[0].next_turn_cache, None);
            assert!(detail.session.moves_today.is_empty());
            assert_eq!(detail.session.first_account_today, Some(account()));

            let moved = now() + chrono::Duration::seconds(1);
            d.placed(
                &id,
                MoveView {
                    at: moved,
                    from: Some(account()),
                    to: "cx-111111".to_owned().try_into().unwrap(),
                    reason: MoveReason::ModelChoice,
                    next_turn_cache: None,
                },
            );
            d.move_cache_result(&id, moved, CacheResult::Miss);
            let detail = d.detail(&id, moved).unwrap();
            assert_eq!(detail.timeline.len(), 2);
            assert_eq!(detail.timeline[0].next_turn_cache, None);
            assert_eq!(detail.timeline[1].kind, "move");
            assert_eq!(detail.session.moves_today.len(), 1);
            assert_eq!(
                detail.session.moves_today[0].reason,
                MoveReason::ModelChoice
            );
            assert_eq!(
                detail.session.moves_today[0].next_turn_cache,
                Some(CacheResult::Miss)
            );
        }
    }

    fn assert_global_retention(d: &Dashboard) {
        let mut sum = [0usize; 4];
        for (id, s) in &d.sessions {
            assert_eq!(s.retained, s.retained_usage());
            for (kind, total) in sum.iter_mut().enumerate() {
                *total += s.retained[kind];
                if s.retained[kind] > 0 {
                    assert!(d.detail_lru[kind].contains(&(
                        s.retention_protected,
                        s.stamps[kind],
                        id.clone()
                    )));
                }
            }
        }
        assert_eq!(sum, d.detail_usage);
        for (kind, limit) in DETAIL_LIMITS.into_iter().enumerate() {
            assert!(sum[kind] <= limit);
            assert_eq!(
                d.detail_lru[kind].len(),
                d.sessions.values().filter(|s| s.retained[kind] > 0).count()
            );
        }
        // Eviction must free allocations, not merely make empty retained buffers.
        assert!(
            d.sessions
                .values()
                .map(|s| s.recent.capacity())
                .sum::<usize>()
                <= 2 * RECENT_TOTAL
        );
        assert!(
            d.sessions
                .values()
                .map(|s| s.timeline.capacity())
                .sum::<usize>()
                <= 2 * TIMELINE_TOTAL
        );
    }

    #[test]
    fn saturated_many_group_details_obey_global_caps_without_losing_live_state_or_counts() {
        let mut d = model();
        let rows: Vec<_> = (0..512)
            .map(|n| {
                let mut b = binding(&format!("codex:stress-{n}"));
                b.active = n == 0;
                b.restored = false;
                b
            })
            .collect();
        let ids: Vec<_> = rows.iter().map(|b| b.id.clone()).collect();
        d.sync_bindings(rows, now()).unwrap();
        let held = 1_000_000;
        d.start(
            &ids[0],
            RequestStart {
                id: held,
                at: now(),
                transport: Transport::Websocket,
            },
        )
        .unwrap();
        d.socket_open(held, &ids[0]).unwrap();
        d.pacing(held, Some(now()));
        for (group, id) in ids.iter().enumerate() {
            for n in 0..10 {
                let request = (group * 10 + n) as u64;
                d.start(
                    id,
                    RequestStart {
                        id: request,
                        at: now(),
                        transport: Transport::Http,
                    },
                )
                .unwrap();
                assert!(d.finish(request, end(now())));
            }
            for n in 0..TIMELINE_CAP {
                d.placed(
                    id,
                    MoveView {
                        at: now(),
                        from: Some(account()),
                        to: account(),
                        reason: MoveReason::Forced,
                        next_turn_cache: None,
                    },
                );
                d.subagent(id, &format!("agent-{group}-{n}"), now());
            }
            assert_global_retention(&d);
        }
        assert_eq!(d.retention_usage().recent_requests, RECENT_TOTAL);
        assert_eq!(d.retention_usage().timeline_entries, TIMELINE_TOTAL);
        assert_eq!(d.retention_usage().subagent_hashes, SUBAGENT_TOTAL);
        let live = d.detail(&ids[0], now()).unwrap();
        assert_eq!(live.session.inflight.requests, 1);
        assert_eq!(live.session.inflight.ws_open, 1);
        assert!(live.session.inflight.ws_response_in_flight);
        assert_eq!(live.session.pacing_wait.unwrap().waiting, 1);
        assert_eq!(live.session.requests_today, 11);
        assert_eq!(live.recent_requests.len(), 10);
        assert_eq!(live.timeline.len(), TIMELINE_CAP);
        assert_eq!(live.session.subagents_today, SUBAGENT_CAP);
        assert!(live.recent_requests_known && live.timeline_known && live.session.subagents_known);
        let mut evicted = 0;
        for id in &ids[1..] {
            let detail = d.detail(id, now()).unwrap();
            assert_eq!(detail.session.requests_today, 10);
            assert!(detail.session.counters_complete);
            assert_eq!(detail.session.rate_wait_today.count, 10);
            if detail.timeline.len() < TIMELINE_CAP {
                evicted += 1;
                assert!(!detail.timeline_known);
                assert!(!detail.session.moves_known);
                assert!(detail.session.truncated);
            }
            if detail.session.subagents_today < SUBAGENT_CAP {
                assert!(!detail.session.subagents_known);
            }
            if detail.recent_requests.len() < 10 {
                assert!(!detail.recent_requests_known);
            }
        }
        assert!(evicted > 0);
        assert!(d.finish(held, end(now())));
        d.socket_close(held);
        d.sync_bindings(vec![], now()).unwrap();
        assert_global_retention(&d);
        assert_eq!(d.detail_usage, [0; 4]);
        assert!(d.detail_lru.iter().all(BTreeSet::is_empty));
    }

    #[test]
    fn shared_metadata_byte_budget_prefers_quiet_groups_and_marks_unknown_fields() {
        let mut d = model();
        let rows: Vec<_> = (0..4000)
            .map(|n| {
                let mut b = binding(&format!("codex:metadata-{n}"));
                b.active = n == 0;
                b.restored = false;
                b
            })
            .collect();
        let ids: Vec<_> = rows.iter().map(|b| b.id.clone()).collect();
        d.sync_bindings(rows, now()).unwrap();
        for id in &ids {
            d.evidence(
                id,
                PrivateEvidence::new(
                    MemoryLabel::new(&"😀".repeat(64), labels::LabelSource::Launcher),
                    OriginalShortId::from_root_key("codex:abcdefgh-original"),
                ),
                Some(ClientView {
                    kind: ClientKind::CodexCli,
                    product: Some("p".repeat(100)),
                    version: Some("v".repeat(100)),
                }),
            );
            d.display_model(id, Some(&"m".repeat(100)));
        }
        assert_global_retention(&d);
        assert!(d.retention_usage().metadata_bytes <= METADATA_BYTES);
        let protected = d.detail(&ids[0], now()).unwrap();
        assert!(protected.session.label.is_some());
        assert!(protected.original_short_id.is_some());
        let victims: Vec<_> = ids
            .iter()
            .filter_map(|id| {
                let detail = d.detail(id, now()).unwrap();
                detail.session.metadata_truncated.then_some(detail)
            })
            .collect();
        assert!(!victims.is_empty());
        for detail in victims {
            assert!(detail.session.truncated);
            assert!(detail.session.label.is_none());
            assert!(detail.original_short_id.is_none());
            assert!(detail.session.model.is_none());
            assert!(detail.session.client.product.is_none());
            assert_eq!(detail.session.client.kind, ClientKind::CodexCli);
            assert_eq!(detail.session.requests_today, 0);
            assert!(detail.session.counters_complete);
        }
        d.sync_bindings(vec![], now()).unwrap();
        assert_global_retention(&d);
        assert_eq!(d.detail_usage, [0; 4]);
    }

    #[test]
    fn all_busy_groups_still_obey_detail_caps_and_retain_every_live_handle() {
        let mut d = model();
        let rows: Vec<_> = (0..96)
            .map(|n| {
                let mut b = binding(&format!("codex:busy-{n}"));
                b.restored = false;
                b
            })
            .collect();
        let ids: Vec<_> = rows.iter().map(|b| b.id.clone()).collect();
        d.sync_bindings(rows, now()).unwrap();
        for (n, id) in ids.iter().enumerate() {
            d.start(
                id,
                RequestStart {
                    id: n as u64,
                    at: now(),
                    transport: Transport::Http,
                },
            )
            .unwrap();
            d.pacing(n as u64, Some(now()));
            for entry in 0..128 {
                d.placed(
                    id,
                    MoveView {
                        at: now(),
                        from: Some(account()),
                        to: account(),
                        reason: MoveReason::Overload,
                        next_turn_cache: None,
                    },
                );
                d.subagent(id, &format!("child-{n}-{entry}"), now());
            }
        }
        assert_global_retention(&d);
        assert!(
            ids.iter()
                .any(|id| !d.detail(id, now()).unwrap().timeline_known)
        );
        for id in &ids {
            let detail = d.detail(id, now()).unwrap();
            assert_eq!(detail.session.inflight.requests, 1);
            assert_eq!(detail.session.pacing_wait.unwrap().waiting, 1);
            assert_eq!(detail.session.requests_today, 1);
            assert!(detail.session.counters_complete);
        }
        for n in 0..ids.len() {
            assert!(d.finish(n as u64, end(now())));
        }
        assert!(d.requests.is_empty());
        assert_global_retention(&d);
    }
}
