//! Disk-safe reducers. Types deliberately have no label, raw-ID/prefix, path or JSON payload field.
use super::{AccountId, Invalid, MoveReason, RequestOutcome, SessionId};
use crate::accounts::Provider;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

pub const EVENT_CAP: usize = 2_000;
const MOVED_CAP: usize = 128;
pub const INPUT_BYTES: usize = 8 * 1024 * 1024;
const ROW_BYTES: usize = 32 * 1024;
const ACCOUNT_CAP: usize = 128;
const WINDOW_CAP: usize = 16;
const POINT_CAP: usize = 10_000;
const TOTAL_POINTS: usize = 50_000;
const SPAN_CAP: usize = 128;

// Logical retention is also an allocation bound, including scheduled empty windows.
fn compact_buffer<T>(buffer: &mut VecDeque<T>) {
    if buffer.is_empty() {
        *buffer = VecDeque::new();
    } else if buffer.capacity() > 64usize.max(buffer.len().saturating_mul(2)) {
        buffer.shrink_to_fit();
    }
}

fn cutoff(now: DateTime<Utc>, hours: i64) -> DateTime<Utc> {
    now.checked_sub_signed(Duration::hours(hours))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WindowName(String);
impl WindowName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for WindowName {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if matches!(s.as_str(), "primary" | "secondary") {
            return Ok(Self(s));
        }
        if s.len() >= 2 && s.len() <= 10 && matches!(s.as_bytes().last(), Some(b'm' | b'h' | b'd'))
        {
            let n = &s[..s.len() - 1];
            if n.bytes().all(|b| b.is_ascii_digit()) && n.parse::<u32>().is_ok_and(|n| n > 0) {
                return Ok(Self(s));
            }
        }
        Err("invalid account window")
    }
}
impl From<WindowName> for String {
    fn from(n: WindowName) -> Self {
        n.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    QuotaReached,
    QuotaReset,
    CreditsStarted,
    CreditsStopped,
    NeedsRelogin,
    LinkStale,
    Payment,
    Permission,
    StoreError,
    Move,
    OverloadDetected,
    RateHold,
    Burst,
    TokenRefresh,
    LinkedReload,
    Login,
    AccountChanged,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionCode {
    Quota,
    NeedsRelogin,
    Paused,
    LinkStale,
    Payment,
    Permission,
    Auth,
    StoreError,
    Burst,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingAction {
    Pause,
    Resume,
    Disable,
    Enable,
    DrainOn,
    DrainOff,
    CreditsOn,
    CreditsOff,
}

/// Explicit structured fields; no arbitrary serde_json::Value or human prose.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<MoveReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attention: Option<AttentionCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RequestOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<SettingAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits_used: Option<f64>,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub max_wait_ms: u64,
    #[serde(default)]
    pub relayed: u32,
    #[serde(default)]
    pub moved_groups: u32,
    #[serde(default)]
    pub sessions_truncated: bool,
    #[serde(default)]
    pub recovered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups_hit: Option<u32>,
}
impl ActivityData {
    fn valid(&self) -> bool {
        self.sessions.len() <= MOVED_CAP
            && self.credits_used.is_none_or(|n| n.is_finite() && n >= 0.0)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskActivityEvent {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub kind: ActivityKind,
    pub provider: Provider,
    pub account: Option<AccountId>,
    pub account_to: Option<AccountId>,
    pub notable: bool,
    pub data: ActivityData,
}
pub struct ActivityInput {
    pub ts: DateTime<Utc>,
    pub kind: ActivityKind,
    pub provider: Provider,
    pub account: Option<AccountId>,
    pub account_to: Option<AccountId>,
    pub notable: bool,
    pub data: ActivityData,
}
pub struct RateHoldObservation {
    pub ts: DateTime<Utc>,
    pub provider: Provider,
    pub account: AccountId,
    pub wait_ms: u64,
    pub retries: u32,
    pub relayed: u32,
}
#[derive(Serialize)]
pub struct ActivityPage {
    pub events: Vec<DiskActivityEvent>,
    pub next_seq: u64,
    pub truncated: bool,
}
// Archive metadata shares the event archive's atomic replacement, but is never
// an activity event and has no timestamp/retention or public cursor row.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityCheckpoint {
    schema: u8,
    high_water_seq: u64,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum ActivityArchiveRow {
    Checkpoint(ActivityCheckpoint),
    Event(Box<DiskActivityEvent>),
}
#[derive(Clone)]
pub struct ActivityLog {
    events: VecDeque<DiskActivityEvent>,
    next_seq: u64,
    revision: u64,
    truncated: bool,
}
impl Default for ActivityLog {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
            next_seq: 1,
            revision: 0,
            truncated: false,
        }
    }
}
impl ActivityLog {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    fn changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let before = self.events.len();
        self.events.retain(|e| e.ts >= cutoff(now, 7 * 24));
        while self.events.len() > EVENT_CAP {
            self.events.pop_front();
            self.truncated = true;
        }
        compact_buffer(&mut self.events);
        if before != self.events.len() {
            self.changed();
        }
    }
    /// Returns a full safe upsert for existing SSE/Observability write-behind.
    pub fn push(
        &mut self,
        mut input: ActivityInput,
        now: DateTime<Utc>,
    ) -> Result<Option<DiskActivityEvent>, Invalid> {
        if input.ts > now || input.ts < cutoff(now, 7 * 24) {
            return Err(Invalid::Timestamp);
        }
        if !input.data.valid() {
            return Err(Invalid::Capacity);
        }
        self.prune(now);
        if matches!(
            input.kind,
            ActivityKind::TokenRefresh | ActivityKind::LinkedReload
        ) && input.data.outcome == Some(RequestOutcome::Ok)
        {
            input.data.recovered = self
                .events
                .iter()
                .rev()
                .find(|e| {
                    e.kind == input.kind
                        && e.provider == input.provider
                        && e.account == input.account
                })
                .is_some_and(|e| {
                    e.data
                        .outcome
                        .is_some_and(|outcome| outcome != RequestOutcome::Ok)
                });
        }
        if input.kind == ActivityKind::Move && input.data.reason == Some(MoveReason::New) {
            return Ok(None);
        }
        input.notable = match input.kind {
            ActivityKind::Move => input.data.reason != Some(MoveReason::ColdRebalance),
            ActivityKind::TokenRefresh | ActivityKind::LinkedReload => {
                !matches!(input.data.outcome, Some(RequestOutcome::Ok))
            }
            ActivityKind::Login => false,
            ActivityKind::AccountChanged => input.data.action.is_some(),
            _ => input.notable,
        };
        if input.kind == ActivityKind::Move
            && input.data.reason == Some(MoveReason::Forced)
            && let Some(session) = input.data.session.as_ref()
        {
            let candidate = self.events.iter_mut().rev().find(|e| {
                e.kind == ActivityKind::QuotaReached
                    && e.account == input.account
                    && e.provider == input.provider
                    && input.account.is_some()
                    && input.ts >= e.ts
                    && (input.ts - e.ts) <= Duration::seconds(5)
            });
            if let Some(event) = candidate {
                if !event.data.sessions.contains(session) {
                    event.data.moved_groups = event.data.moved_groups.saturating_add(1);
                    if event.data.sessions.len() < MOVED_CAP {
                        event.data.sessions.push(session.clone());
                    } else {
                        event.data.sessions_truncated = true;
                    }
                }
                if event.account_to.is_none() && event.data.moved_groups == 1 {
                    event.account_to = input.account_to;
                } else if event.account_to != input.account_to {
                    event.account_to = None;
                }
                let upsert = event.clone();
                self.changed();
                return Ok(Some(upsert));
            }
        }
        let seq = self.next_seq;
        self.next_seq = seq.checked_add(1).ok_or(Invalid::Capacity)?;
        let event = DiskActivityEvent {
            seq,
            ts: input.ts,
            kind: input.kind,
            provider: input.provider,
            account: input.account,
            account_to: input.account_to,
            notable: input.notable,
            data: input.data,
        };
        self.events.push_back(event.clone());
        self.changed();
        self.prune(now);
        Ok(Some(event))
    }
    pub fn rate_hold(
        &mut self,
        observation: RateHoldObservation,
        now: DateTime<Utc>,
    ) -> Result<Option<DiskActivityEvent>, Invalid> {
        let o = observation;
        if o.retries == 0 && o.relayed == 0 {
            return Ok(None);
        }
        if o.ts > now || o.ts < cutoff(now, 7 * 24) {
            return Err(Invalid::Timestamp);
        }
        self.prune(now);
        let bucket = o.ts.timestamp().div_euclid(600);
        if let Some(event) = self.events.iter_mut().find(|e| {
            e.kind == ActivityKind::RateHold
                && e.account.as_ref() == Some(&o.account)
                && e.provider == o.provider
                && e.ts.timestamp().div_euclid(600) == bucket
        }) {
            event.data.count = event.data.count.saturating_add(1);
            event.data.max_wait_ms = event.data.max_wait_ms.max(o.wait_ms);
            event.data.relayed = event.data.relayed.saturating_add(o.relayed);
            let upsert = event.clone();
            self.changed();
            return Ok(Some(upsert));
        }
        self.push(
            ActivityInput {
                ts: o.ts,
                kind: ActivityKind::RateHold,
                provider: o.provider,
                account: Some(o.account),
                account_to: None,
                notable: true,
                data: ActivityData {
                    count: 1,
                    max_wait_ms: o.wait_ms,
                    relayed: o.relayed,
                    ..ActivityData::default()
                },
            },
            now,
        )
    }
    pub fn query(
        &self,
        since_seq: Option<u64>,
        limit: usize,
        days: u8,
        now: DateTime<Utc>,
    ) -> Result<ActivityPage, Invalid> {
        if limit == 0 || limit > EVENT_CAP || !(1..=7).contains(&days) {
            return Err(Invalid::Range);
        }
        let eligible = self
            .events
            .iter()
            .filter(|e| e.ts >= cutoff(now, i64::from(days) * 24));
        let mut events = eligible
            .filter(|e| since_seq.is_none_or(|seq| e.seq > seq))
            .cloned()
            .collect::<Vec<_>>();
        if since_seq.is_none() {
            events.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.seq.cmp(&a.seq)));
        }
        let truncated = self.truncated || events.len() > limit;
        events.truncate(limit);
        let next_seq = events
            .iter()
            .map(|e| e.seq)
            .max()
            .unwrap_or(since_seq.unwrap_or(0));
        Ok(ActivityPage {
            events,
            next_seq,
            truncated,
        })
    }
    /// Retain the issued sequence high-water mark even after every event expires.
    pub fn compact_jsonl(&self) -> Result<Vec<u8>, Invalid> {
        let mut bytes = Vec::new();
        serde_json::to_writer(
            &mut bytes,
            &ActivityCheckpoint {
                schema: 1,
                high_water_seq: self.next_seq - 1,
            },
        )
        .map_err(|_| Invalid::Schema)?;
        bytes.push(b'\n');
        for event in &self.events {
            serde_json::to_writer(&mut bytes, event).map_err(|_| Invalid::Schema)?;
            bytes.push(b'\n');
        }
        if bytes.len() > INPUT_BYTES {
            return Err(Invalid::Capacity);
        }
        Ok(bytes)
    }
    pub fn restore_jsonl(bytes: &[u8], now: DateTime<Utc>) -> Result<Self, Invalid> {
        if bytes.len() > INPUT_BYTES {
            return Err(Invalid::Capacity);
        }
        let mut latest: BTreeMap<u64, DiskActivityEvent> = BTreeMap::new();
        let mut max_seq = 0;
        let mut truncated = false;
        let lines = bytes.split_inclusive(|b| *b == b'\n').collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if line.len() > ROW_BYTES {
                return Err(Invalid::Capacity);
            }
            let event = match serde_json::from_slice::<ActivityArchiveRow>(line) {
                Ok(ActivityArchiveRow::Checkpoint(checkpoint)) => {
                    if checkpoint.schema != 1 {
                        return Err(Invalid::Schema);
                    }
                    max_seq = max_seq.max(checkpoint.high_water_seq);
                    continue;
                }
                Ok(ActivityArchiveRow::Event(event)) => *event,
                Err(_) if index + 1 == lines.len() && !line.ends_with(b"\n") => break,
                Err(_) => return Err(Invalid::Schema),
            };
            if event.seq == 0 || !event.data.valid() || event.ts > now {
                return Err(Invalid::Schema);
            }
            max_seq = max_seq.max(event.seq);
            if let Some(old) = latest.get(&event.seq)
                && (old.ts != event.ts
                    || old.kind != event.kind
                    || old.provider != event.provider
                    || old.account != event.account)
            {
                return Err(Invalid::Schema);
            }
            if event.ts >= cutoff(now, 7 * 24) {
                latest.insert(event.seq, event);
            }
            while latest.len() > EVENT_CAP {
                latest.pop_first();
                truncated = true;
            }
        }
        let next_seq = max_seq.checked_add(1).ok_or(Invalid::Capacity)?;
        Ok(Self {
            events: latest.into_values().collect(),
            next_seq,
            revision: 0,
            truncated,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WindowScope {
    Account,
    Model,
}
pub struct WindowObservation {
    pub name: WindowName,
    pub scope: WindowScope,
    pub used_pct: Option<f64>,
    pub reset_at: Option<DateTime<Utc>>,
}
pub struct CreditsObservation {
    pub serving: bool,
    pub balance: Option<f64>,
}
pub struct UsageObservation {
    pub account: AccountId,
    pub observed_at: DateTime<Utc>,
    pub window: Option<WindowObservation>,
    pub credits: Option<CreditsObservation>,
}

/// Null reset-boundary points mean unobserved, not invented 0% usage.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Point {
    at: DateTime<Utc>,
    used_pct: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreditSpan {
    from: DateTime<Utc>,
    to: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct WindowSeries {
    points: VecDeque<Point>,
    resets: VecDeque<DateTime<Utc>>,
    scheduled_reset: Option<DateTime<Utc>>,
    last_observed: Option<DateTime<Utc>>,
    last_pct: Option<f64>,
    truncated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AccountSeries {
    windows: BTreeMap<WindowName, WindowSeries>,
    spans: VecDeque<CreditSpan>,
    last_credit: Option<DateTime<Utc>>,
    credit_start_balance: Option<f64>,
    truncated: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageDisk {
    schema: u8,
    accounts: BTreeMap<AccountId, AccountSeries>,
}
#[derive(Serialize)]
pub struct UsageWindowView {
    pub name: WindowName,
    pub points: Vec<(DateTime<Utc>, Option<f64>)>,
    pub resets: Vec<DateTime<Utc>>,
    pub truncated: bool,
}
#[derive(Serialize)]
pub struct UsageAccountView {
    pub id: AccountId,
    pub windows: Vec<UsageWindowView>,
    pub credits_spans: Vec<(DateTime<Utc>, Option<DateTime<Utc>>)>,
    pub truncated: bool,
}
#[derive(Serialize)]
pub struct UsageView {
    pub accounts: Vec<UsageAccountView>,
    pub truncated: bool,
}
#[derive(Clone, Default)]
pub struct UsageHistory {
    accounts: BTreeMap<AccountId, AccountSeries>,
    revision: u64,
    truncated: bool,
}

/// Writer scheduling only. The existing Observability thread owns file I/O.
/// Completion of revision N must not mark a newer coalesced revision N+1 clean.
#[derive(Default)]
pub struct WriteBehind {
    written: u64,
    in_flight: Option<u64>,
    next_due_ms: u64,
}
impl WriteBehind {
    pub fn offer(&mut self, revision: u64, now_ms: u64, force: bool) -> Option<u64> {
        if revision <= self.written
            || self.in_flight.is_some()
            || (!force && now_ms < self.next_due_ms)
        {
            return None;
        }
        self.in_flight = Some(revision);
        Some(revision)
    }
    pub fn complete(&mut self, revision: u64, success: bool, now_ms: u64) {
        if self.in_flight != Some(revision) {
            return;
        }
        self.in_flight = None;
        if success {
            self.written = self.written.max(revision);
            self.next_due_ms = now_ms.saturating_add(60_000);
        } else {
            self.next_due_ms = now_ms.saturating_add(1_000);
        }
    }
    pub fn dirty(&self, revision: u64) -> bool {
        revision > self.written
    }
    pub fn activity_compaction_due(append_rows: usize, append_bytes: usize) -> bool {
        append_rows >= EVENT_CAP * 2 || append_bytes >= INPUT_BYTES
    }
}
impl UsageHistory {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    fn changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
    fn validate(observation: &UsageObservation, now: DateTime<Utc>) -> Result<(), Invalid> {
        if observation.observed_at > now || observation.observed_at < cutoff(now, 48) {
            return Err(Invalid::Timestamp);
        }
        if observation
            .window
            .as_ref()
            .and_then(|w| w.used_pct)
            .is_some_and(|n| !n.is_finite() || !(0.0..=100.0).contains(&n))
            || observation
                .credits
                .as_ref()
                .and_then(|c| c.balance)
                .is_some_and(|n| !n.is_finite() || n < 0.0)
        {
            return Err(Invalid::Number);
        }
        Ok(())
    }
    /// Returns a known credits-spend amount only on a serving -> stopped edge.
    pub fn observe(
        &mut self,
        o: UsageObservation,
        now: DateTime<Utc>,
    ) -> Result<Option<f64>, Invalid> {
        Self::validate(&o, now)?;
        self.advance(now);
        if !self.accounts.contains_key(&o.account) && self.accounts.len() >= ACCOUNT_CAP {
            self.truncated = true;
            return Err(Invalid::Capacity);
        }
        let a = self.accounts.entry(o.account).or_default();
        let mut spent = None;
        if let Some(w) = o.window
            && w.scope == WindowScope::Account
        {
            if !a.windows.contains_key(&w.name) && a.windows.len() >= WINDOW_CAP {
                a.truncated = true;
                return Err(Invalid::Capacity);
            }
            let series = a.windows.entry(w.name).or_default();
            if series.last_observed.is_none_or(|at| o.observed_at >= at) {
                if let Some(pct) = w.used_pct {
                    let preceding = series.points.iter().rev().find(|p| p.at <= o.observed_at);
                    if series.last_pct != Some(pct)
                        || preceding.is_none_or(|p| p.used_pct.is_none())
                    {
                        let index = series.points.partition_point(|p| p.at < o.observed_at);
                        let point = Point {
                            at: o.observed_at,
                            used_pct: Some(pct),
                        };
                        if series
                            .points
                            .get(index)
                            .is_some_and(|p| p.at == o.observed_at)
                        {
                            series.points[index] = point;
                        } else {
                            series.points.insert(index, point);
                        }
                    }
                    series.last_pct = Some(pct);
                    series.last_observed = Some(o.observed_at);
                }
                if let Some(reset) = w.reset_at
                    && series.resets.back().is_none_or(|last| reset > *last)
                {
                    series.scheduled_reset = Some(reset);
                }
            }
        }
        if let Some(credit) = o.credits
            && a.last_credit.is_none_or(|at| o.observed_at >= at)
        {
            let open = a.spans.back().is_some_and(|s| s.to.is_none());
            if credit.serving && !open {
                a.spans.push_back(CreditSpan {
                    from: o.observed_at,
                    to: None,
                });
                a.credit_start_balance = credit.balance;
            } else if !credit.serving && open {
                a.spans.back_mut().expect("open span").to = Some(o.observed_at);
                spent = a
                    .credit_start_balance
                    .zip(credit.balance)
                    .map(|(from, to)| (from - to).max(0.0).floor());
                a.credit_start_balance = None;
            }
            a.last_credit = Some(o.observed_at);
        }
        self.changed();
        self.advance(now);
        Ok(spent)
    }
    pub fn advance(&mut self, now: DateTime<Utc>) {
        let since = cutoff(now, 48);
        let mut dirty = false;
        for a in self.accounts.values_mut() {
            for w in a.windows.values_mut() {
                if let Some(reset) = w.scheduled_reset
                    && reset <= now
                {
                    w.scheduled_reset = None;
                    if reset >= since && w.resets.back().is_none_or(|r| reset > *r) {
                        w.resets.push_back(reset);
                        // Insert chronologically; observation may arrive after reset time.
                        let index = w.points.partition_point(|p| p.at < reset);
                        if w.points.get(index).is_none_or(|p| p.at != reset) {
                            w.points.insert(
                                index,
                                Point {
                                    at: reset,
                                    used_pct: None,
                                },
                            );
                        }
                    }
                    dirty = true;
                }
                while w.points.front().is_some_and(|p| p.at < since) {
                    w.points.pop_front();
                    dirty = true;
                }
                while w.resets.front().is_some_and(|at| *at < since) {
                    w.resets.pop_front();
                    dirty = true;
                }
                while w.points.len() > POINT_CAP {
                    w.points.pop_front();
                    w.truncated = true;
                    dirty = true;
                }
                while w.resets.len() > POINT_CAP {
                    w.resets.pop_front();
                    w.truncated = true;
                    dirty = true;
                }
            }
            while a
                .spans
                .front()
                .is_some_and(|s| s.to.is_some_and(|to| to < since))
            {
                a.spans.pop_front();
                dirty = true;
            }
            if let Some(span) = a.spans.front_mut()
                && span.from < since
            {
                span.from = since;
                dirty = true;
            }
            while a.spans.len() > SPAN_CAP {
                a.spans.pop_front();
                a.truncated = true;
                dirty = true;
            }
            a.windows.retain(|_, w| {
                !w.points.is_empty() || !w.resets.is_empty() || w.scheduled_reset.is_some()
            });
        }
        self.accounts
            .retain(|_, a| !a.windows.is_empty() || !a.spans.is_empty());
        loop {
            let count = self
                .accounts
                .values()
                .flat_map(|a| a.windows.values())
                .map(|w| w.points.len() + w.resets.len())
                .sum::<usize>();
            if count <= TOTAL_POINTS {
                break;
            }
            let oldest = self
                .accounts
                .iter()
                .flat_map(|(id, a)| {
                    a.windows.iter().flat_map(move |(name, w)| {
                        [
                            w.points
                                .front()
                                .map(|p| (p.at, id.clone(), name.clone(), false)),
                            w.resets
                                .front()
                                .map(|at| (*at, id.clone(), name.clone(), true)),
                        ]
                        .into_iter()
                        .flatten()
                    })
                })
                .min();
            let Some((_, id, name, reset)) = oldest else {
                break;
            };
            let w = self
                .accounts
                .get_mut(&id)
                .unwrap()
                .windows
                .get_mut(&name)
                .unwrap();
            if reset {
                w.resets.pop_front();
            } else {
                w.points.pop_front();
            }
            w.truncated = true;
            self.truncated = true;
            dirty = true;
        }
        for account in self.accounts.values_mut() {
            compact_buffer(&mut account.spans);
            for window in account.windows.values_mut() {
                compact_buffer(&mut window.points);
                compact_buffer(&mut window.resets);
            }
        }
        if dirty {
            self.changed();
        }
    }
    pub fn query(&self, hours: u8, now: DateTime<Utc>) -> Result<UsageView, Invalid> {
        if hours == 0 || hours > 48 {
            return Err(Invalid::Range);
        }
        let since = cutoff(now, i64::from(hours));
        let accounts = self
            .accounts
            .iter()
            .map(|(id, a)| UsageAccountView {
                id: id.clone(),
                windows: a
                    .windows
                    .iter()
                    .map(|(name, w)| UsageWindowView {
                        name: name.clone(),
                        points: w
                            .points
                            .iter()
                            .filter(|p| p.at >= since && p.at <= now)
                            .map(|p| (p.at, p.used_pct))
                            .collect(),
                        resets: w
                            .resets
                            .iter()
                            .filter(|at| **at >= since && **at <= now)
                            .copied()
                            .collect(),
                        truncated: w.truncated,
                    })
                    .collect(),
                credits_spans: a
                    .spans
                    .iter()
                    .filter(|s| s.to.is_none_or(|to| to >= since))
                    .map(|s| (s.from.max(since), s.to))
                    .collect(),
                truncated: a.truncated,
            })
            .collect();
        Ok(UsageView {
            accounts,
            truncated: self.truncated,
        })
    }
    pub fn snapshot_json(&self) -> Result<Vec<u8>, Invalid> {
        let bytes = serde_json::to_vec(&UsageDisk {
            schema: 1,
            accounts: self.accounts.clone(),
        })
        .map_err(|_| Invalid::Schema)?;
        if bytes.len() > INPUT_BYTES {
            return Err(Invalid::Capacity);
        }
        Ok(bytes)
    }
    pub fn restore_json(bytes: &[u8], now: DateTime<Utc>) -> Result<Self, Invalid> {
        if bytes.len() > INPUT_BYTES {
            return Err(Invalid::Capacity);
        }
        let disk: UsageDisk = serde_json::from_slice(bytes).map_err(|_| Invalid::Schema)?;
        if disk.schema != 1 || disk.accounts.len() > ACCOUNT_CAP {
            return Err(Invalid::Schema);
        }
        for a in disk.accounts.values() {
            if a.windows.len() > WINDOW_CAP || a.spans.len() > SPAN_CAP {
                return Err(Invalid::Capacity);
            }
            if a.credit_start_balance
                .is_some_and(|n| !n.is_finite() || n < 0.0)
                || a.last_credit.is_some_and(|at| at > now)
            {
                return Err(Invalid::Schema);
            }
            for w in a.windows.values() {
                if w.points.len() > POINT_CAP || w.resets.len() > POINT_CAP {
                    return Err(Invalid::Capacity);
                }
                if w.last_pct
                    .is_some_and(|n| !n.is_finite() || !(0.0..=100.0).contains(&n))
                    || w.last_observed.is_some_and(|at| at > now)
                {
                    return Err(Invalid::Schema);
                }
                let mut last = None;
                for p in &w.points {
                    if p.at > now
                        || last.is_some_and(|at| p.at <= at)
                        || p.used_pct
                            .is_some_and(|n| !n.is_finite() || !(0.0..=100.0).contains(&n))
                    {
                        return Err(Invalid::Schema);
                    }
                    last = Some(p.at);
                }
                if w.resets
                    .iter()
                    .zip(w.resets.iter().skip(1))
                    .any(|(a, b)| b <= a)
                    || w.resets.iter().any(|at| *at > now)
                {
                    return Err(Invalid::Schema);
                }
            }
            let mut end = None;
            for (i, s) in a.spans.iter().enumerate() {
                if s.from > now
                    || s.to.is_some_and(|to| to < s.from || to > now)
                    || end.is_some_and(|to| s.from < to)
                    || (s.to.is_none() && i + 1 < a.spans.len())
                {
                    return Err(Invalid::Schema);
                }
                end = s.to;
            }
        }
        let truncated = disk
            .accounts
            .values()
            .any(|a| a.truncated || a.windows.values().any(|w| w.truncated));
        let mut history = Self {
            accounts: disk.accounts,
            revision: 0,
            truncated,
        };
        history.advance(now);
        Ok(history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> DateTime<Utc> {
        "2026-10-19T06:12:00Z".parse().unwrap()
    }
    fn account() -> AccountId {
        "cx-123abc".to_owned().try_into().unwrap()
    }
    fn input(kind: ActivityKind, ts: DateTime<Utc>) -> ActivityInput {
        ActivityInput {
            ts,
            kind,
            provider: Provider::Codex,
            account: Some(account()),
            account_to: None,
            notable: true,
            data: ActivityData::default(),
        }
    }
    fn observation(at: DateTime<Utc>, pct: f64) -> UsageObservation {
        UsageObservation {
            account: account(),
            observed_at: at,
            window: Some(WindowObservation {
                name: "5h".to_owned().try_into().unwrap(),
                scope: WindowScope::Account,
                used_pct: Some(pct),
                reset_at: None,
            }),
            credits: None,
        }
    }
    #[test]
    fn rate_buckets_update_same_seq_and_account_without_clock_tolerance() {
        let mut log = ActivityLog::default();
        let at = now();
        let make = |ts, wait_ms, relayed| RateHoldObservation {
            ts,
            provider: Provider::Codex,
            account: account(),
            wait_ms,
            retries: 1,
            relayed,
        };
        let a = log.rate_hold(make(at, 100, 0), at).unwrap().unwrap();
        let b = log
            .rate_hold(
                make(at + Duration::seconds(1), 300, 1),
                at + Duration::seconds(1),
            )
            .unwrap()
            .unwrap();
        assert_eq!(a.seq, b.seq);
        assert_eq!(b.data.count, 2);
        assert_eq!(b.data.max_wait_ms, 300);
        assert_eq!(b.data.relayed, 1);
        let next = at + Duration::minutes(10);
        let c = log.rate_hold(make(next, 1, 0), next).unwrap().unwrap();
        assert_ne!(c.seq, a.seq);
        let bytes = log.compact_jsonl().unwrap();
        let restored = ActivityLog::restore_jsonl(&bytes, next).unwrap();
        assert_eq!(restored.events.len(), 2);
    }
    #[test]
    fn quota_forced_move_folds_five_seconds_but_never_first_placement() {
        let mut log = ActivityLog::default();
        let at = now();
        let quota = log
            .push(input(ActivityKind::QuotaReached, at), at)
            .unwrap()
            .unwrap();
        let mut mv = input(ActivityKind::Move, at + Duration::seconds(5));
        mv.data.reason = Some(MoveReason::Forced);
        mv.data.session = Some(SessionId::from_key("codex:root"));
        mv.account_to = Some("cx-111111".to_owned().try_into().unwrap());
        let folded = log.push(mv, at + Duration::seconds(5)).unwrap().unwrap();
        assert_eq!(folded.seq, quota.seq);
        assert_eq!(folded.data.moved_groups, 1);
        assert_eq!(log.events.len(), 1);
        let mut new = input(ActivityKind::Move, at);
        new.data.reason = Some(MoveReason::New);
        assert!(log.push(new, at).unwrap().is_none());
        let mut late = input(ActivityKind::Move, at + Duration::seconds(6));
        late.data.reason = Some(MoveReason::Forced);
        late.data.session = Some(SessionId::from_key("codex:late"));
        assert_ne!(
            log.push(late, at + Duration::seconds(6))
                .unwrap()
                .unwrap()
                .seq,
            quota.seq
        );
    }
    #[test]
    fn activity_is_bounded_and_cursor_pages_do_not_skip_unreturned_events() {
        let mut log = ActivityLog::default();
        for _ in 0..2001 {
            log.push(input(ActivityKind::Login, now()), now()).unwrap();
        }
        assert_eq!(log.events.len(), 2000);
        assert!(log.truncated);
        let page = log.query(Some(0), 3, 7, now()).unwrap();
        assert_eq!(page.next_seq, 4);
        assert_eq!(page.events.len(), 3);
        let next = log.query(Some(page.next_seq), 3, 7, now()).unwrap();
        assert_eq!(next.events[0].seq, 5);
        log.prune(now() + Duration::days(8));
        assert!(log.events.is_empty());
        assert!(log.query(None, 2001, 7, now()).is_err());
        assert!(log.query(None, 10, 0, now()).is_err());
    }
    #[test]
    fn expired_empty_compaction_preserves_cursor_across_repeated_restarts() {
        let mut log = ActivityLog::default();
        for _ in 0..4 {
            log.push(input(ActivityKind::Login, now()), now()).unwrap();
        }
        let cursor = log.query(None, 100, 7, now()).unwrap().next_seq;
        assert_eq!(cursor, 4);
        let late = now() + Duration::days(8);
        log.prune(late);
        assert!(log.events.is_empty());
        let compacted = log.compact_jsonl().unwrap();
        assert!(!compacted.is_empty());
        let mut restored = ActivityLog::restore_jsonl(&compacted, late).unwrap();
        assert!(
            restored
                .query(None, 100, 7, late)
                .unwrap()
                .events
                .is_empty()
        );
        let new = restored
            .push(input(ActivityKind::Login, late), late)
            .unwrap()
            .unwrap();
        assert_eq!(new.seq, cursor + 1);
        let page = restored.query(Some(cursor), 100, 7, late).unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].seq, new.seq);
        assert_eq!(page.next_seq, new.seq);

        let later = late + Duration::days(8);
        restored.prune(later);
        let mut reopened =
            ActivityLog::restore_jsonl(&restored.compact_jsonl().unwrap(), later).unwrap();
        assert!(reopened.events.is_empty());
        reopened
            .push(input(ActivityKind::Login, later), later)
            .unwrap();
        let page = reopened.query(Some(new.seq), 100, 7, later).unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].seq, new.seq + 1);
    }

    #[test]
    fn checkpoint_restore_keeps_legacy_rows_and_rejects_invalid_metadata() {
        let mut log = ActivityLog::default();
        let event = log
            .push(input(ActivityKind::Login, now()), now())
            .unwrap()
            .unwrap();
        // Pre-checkpoint event-only archives remain readable and migratable.
        let mut legacy = serde_json::to_vec(&event).unwrap();
        legacy.push(b'\n');
        let restored = ActivityLog::restore_jsonl(&legacy, now()).unwrap();
        let migrated =
            ActivityLog::restore_jsonl(&restored.compact_jsonl().unwrap(), now()).unwrap();
        assert_eq!(migrated.events.len(), 1);
        assert_eq!(migrated.next_seq, 2);

        let mut checkpoint_then_event = b"{\"schema\":1,\"high_water_seq\":40}\n".to_vec();
        checkpoint_then_event.extend_from_slice(&legacy);
        let restored = ActivityLog::restore_jsonl(&checkpoint_then_event, now()).unwrap();
        assert_eq!(restored.next_seq, 41);
        assert_eq!(restored.events.len(), 1);
        for invalid in [
            "{\"schema\":2,\"high_water_seq\":40}\n",
            "{\"schema\":1,\"high_water_seq\":40,\"label\":\"private\"}\n",
            "{\"schema\":1,\"high_water_seq\":18446744073709551615}\n",
        ] {
            assert!(ActivityLog::restore_jsonl(invalid.as_bytes(), now()).is_err());
        }
    }

    #[test]
    fn append_upsert_restore_partial_tail_and_structural_privacy() {
        let mut log = ActivityLog::default();
        let mut e = log
            .push(input(ActivityKind::RateHold, now()), now())
            .unwrap()
            .unwrap();
        let mut bytes = serde_json::to_vec(&e).unwrap();
        bytes.push(b'\n');
        e.data.count = 9;
        serde_json::to_writer(&mut bytes, &e).unwrap();
        bytes.extend_from_slice(b"\n{partial");
        let restored = ActivityLog::restore_jsonl(&bytes, now()).unwrap();
        assert_eq!(restored.events[0].data.count, 9);
        let mut forbidden = serde_json::to_value(&e).unwrap();
        forbidden["data"]["label"] = serde_json::json!("private-folder");
        let mut bad = serde_json::to_vec(&forbidden).unwrap();
        bad.push(b'\n');
        assert!(ActivityLog::restore_jsonl(&bad, now()).is_err());
        forbidden["data"].as_object_mut().unwrap().remove("label");
        forbidden["original_short_id"] = serde_json::json!("12345678");
        assert!(serde_json::from_value::<DiskActivityEvent>(forbidden).is_err());
    }
    #[test]
    fn usage_deduplicates_unchanged_observation_excludes_model_and_keeps_dynamic_windows() {
        let mut h = UsageHistory::default();
        h.observe(observation(now(), 10.0), now()).unwrap();
        h.observe(
            observation(now() + Duration::seconds(1), 10.0),
            now() + Duration::seconds(1),
        )
        .unwrap();
        assert_eq!(
            h.query(24, now() + Duration::seconds(1)).unwrap().accounts[0].windows[0]
                .points
                .len(),
            1
        );
        let mut model = observation(now(), 20.0);
        model.window.as_mut().unwrap().scope = WindowScope::Model;
        h.observe(model, now()).unwrap();
        assert_eq!(h.accounts[&account()].windows.len(), 1);
        let mut free = observation(now(), 30.0);
        free.window.as_mut().unwrap().name = "43200m".to_owned().try_into().unwrap();
        h.observe(free, now()).unwrap();
        assert_eq!(h.accounts[&account()].windows.len(), 2);
        assert!(WindowName::try_from(String::from("private-folder")).is_err());
    }
    #[test]
    fn reset_boundary_is_once_and_unknown_instead_of_fabricated_zero() {
        let mut h = UsageHistory::default();
        let reset = now() + Duration::seconds(10);
        let mut o = observation(now(), 100.0);
        o.window.as_mut().unwrap().reset_at = Some(reset);
        h.observe(o, now()).unwrap();
        h.advance(reset);
        h.advance(reset + Duration::seconds(1));
        let w = &h.query(24, reset + Duration::seconds(1)).unwrap().accounts[0].windows[0];
        assert_eq!(w.resets, vec![reset]);
        assert_eq!(w.points.last().unwrap(), &(reset, None));
        h.observe(
            observation(reset + Duration::seconds(2), 3.0),
            reset + Duration::seconds(2),
        )
        .unwrap();
        assert_eq!(
            h.query(24, reset + Duration::seconds(2)).unwrap().accounts[0].windows[0]
                .points
                .last()
                .unwrap()
                .1,
            Some(3.0)
        );
    }
    #[test]
    fn credits_spans_use_serving_evidence_floor_and_unknown_balance() {
        let mut h = UsageHistory::default();
        let credit = |at, serving, balance| UsageObservation {
            account: account(),
            observed_at: at,
            window: None,
            credits: Some(CreditsObservation { serving, balance }),
        };
        h.observe(credit(now(), true, Some(10.0)), now()).unwrap();
        assert_eq!(
            h.observe(
                credit(now() + Duration::seconds(1), false, Some(12.0)),
                now() + Duration::seconds(1)
            )
            .unwrap(),
            Some(0.0)
        );
        h.observe(
            credit(now() + Duration::seconds(2), true, None),
            now() + Duration::seconds(2),
        )
        .unwrap();
        assert_eq!(
            h.observe(
                credit(now() + Duration::seconds(3), false, Some(1.0)),
                now() + Duration::seconds(3)
            )
            .unwrap(),
            None
        );
        assert_eq!(
            h.query(24, now() + Duration::seconds(3)).unwrap().accounts[0]
                .credits_spans
                .len(),
            2
        );
    }
    #[test]
    fn usage_rejects_nonfinite_stale_future_and_roundtrips_with_retention() {
        let mut h = UsageHistory::default();
        for pct in [f64::NAN, f64::INFINITY, -1.0, 101.0] {
            assert_eq!(
                h.observe(observation(now(), pct), now()),
                Err(Invalid::Number)
            );
        }
        assert_eq!(
            h.observe(observation(now() + Duration::seconds(1), 1.0), now()),
            Err(Invalid::Timestamp)
        );
        h.observe(observation(now(), 42.0), now()).unwrap();
        let bytes = h.snapshot_json().unwrap();
        let restored = UsageHistory::restore_json(&bytes, now()).unwrap();
        assert_eq!(
            restored.query(48, now()).unwrap().accounts[0].windows[0].points[0].1,
            Some(42.0)
        );
        h.advance(now() + Duration::hours(49));
        assert!(
            h.query(48, now() + Duration::hours(49))
                .unwrap()
                .accounts
                .is_empty()
        );
        assert!(h.query(0, now()).is_err());
        assert!(h.query(49, now()).is_err());
    }
    #[test]
    fn point_cap_is_explicit_not_silent_sampling() {
        let mut h = UsageHistory::default();
        for n in 0..10005 {
            let at = now() + Duration::milliseconds(n);
            h.observe(observation(at, (n % 100) as f64), at).unwrap();
        }
        let page = h.query(48, now() + Duration::seconds(20)).unwrap();
        assert_eq!(page.accounts[0].windows[0].points.len(), 10000);
        assert!(page.accounts[0].windows[0].truncated);
    }
    #[test]
    fn out_of_order_observation_before_passed_reset_stays_sorted_and_roundtrips() {
        let mut h = UsageHistory::default();
        let reset = now() + Duration::seconds(10);
        let mut initial = observation(now(), 10.0);
        initial.window.as_mut().unwrap().reset_at = Some(reset);
        h.observe(initial, now()).unwrap();
        let late = now() + Duration::seconds(20);
        h.observe(observation(now() + Duration::seconds(5), 20.0), late)
            .unwrap();
        let bytes = h.snapshot_json().unwrap();
        let restored = UsageHistory::restore_json(&bytes, late).unwrap();
        let points = &restored.query(48, late).unwrap().accounts[0].windows[0].points;
        assert_eq!(points[1].1, Some(20.0));
        assert_eq!(points[2], (reset, None));
    }
    #[test]
    fn same_percentage_after_full_retention_expiry_starts_a_new_series() {
        let mut h = UsageHistory::default();
        h.observe(observation(now(), 10.0), now()).unwrap();
        let later = now() + Duration::hours(49);
        h.observe(observation(later, 10.0), later).unwrap();
        assert_eq!(
            h.query(48, later).unwrap().accounts[0].windows[0].points,
            vec![(later, Some(10.0))]
        );
    }
    #[test]
    fn routine_refresh_and_first_placement_are_not_notable() {
        let mut log = ActivityLog::default();
        let mut refresh = input(ActivityKind::TokenRefresh, now());
        refresh.data.outcome = Some(RequestOutcome::Ok);
        assert!(!log.push(refresh, now()).unwrap().unwrap().notable);
        let mut failure = input(ActivityKind::TokenRefresh, now());
        failure.data.outcome = Some(RequestOutcome::UpstreamError);
        assert!(log.push(failure, now()).unwrap().unwrap().notable);
        let mut cold = input(ActivityKind::Move, now());
        cold.data.reason = Some(MoveReason::ColdRebalance);
        assert!(!log.push(cold, now()).unwrap().unwrap().notable);
    }
    #[test]
    fn writebehind_retries_and_old_ack_cannot_drop_newer_dirty_state() {
        let mut writer = WriteBehind::default();
        assert_eq!(writer.offer(1, 0, false), Some(1));
        assert!(writer.offer(2, 0, true).is_none());
        writer.complete(1, true, 0);
        assert!(writer.dirty(2));
        assert!(writer.offer(2, 59_999, false).is_none());
        assert_eq!(writer.offer(2, 60_000, false), Some(2));
        writer.complete(1, true, 60_000);
        assert!(writer.dirty(2));
        writer.complete(2, false, 60_000);
        assert!(writer.offer(2, 60_999, false).is_none());
        assert_eq!(writer.offer(2, 61_000, false), Some(2));
        writer.complete(2, true, 61_000);
        assert!(!writer.dirty(2));
        assert_eq!(writer.offer(3, 61_001, true), Some(3));
        assert!(WriteBehind::activity_compaction_due(4000, 0));
    }
    #[test]
    fn scheduled_window_churn_bounds_allocated_capacity_not_only_logical_points() {
        let mut history = UsageHistory::default();
        let at = now();
        for account in 0..4 {
            let id: AccountId = format!("cx-{account:06x}").try_into().unwrap();
            let mut series = AccountSeries::default();
            for window in 1..=16 {
                let mut points = VecDeque::with_capacity(POINT_CAP);
                for n in 0..900 {
                    points.push_back(Point {
                        at: at - chrono::Duration::seconds(900 - n),
                        used_pct: Some((n % 100) as f64),
                    });
                }
                series.windows.insert(
                    format!("{window}h").try_into().unwrap(),
                    WindowSeries {
                        points,
                        resets: VecDeque::with_capacity(POINT_CAP),
                        scheduled_reset: Some(at + Duration::hours(72)),
                        ..Default::default()
                    },
                );
            }
            history.accounts.insert(id, series);
        }
        history.advance(at);
        let windows: Vec<_> = history
            .accounts
            .values()
            .flat_map(|a| a.windows.values())
            .collect();
        let logical: usize = windows
            .iter()
            .map(|w| w.points.len() + w.resets.len())
            .sum();
        let capacity: usize = windows
            .iter()
            .map(|w| w.points.capacity() + w.resets.capacity())
            .sum();
        assert!(logical <= TOTAL_POINTS);
        assert!(capacity <= 2 * TOTAL_POINTS + 64 * windows.len() * 2);
        assert!(
            windows
                .iter()
                .all(|w| !w.resets.is_empty() || w.resets.capacity() == 0)
        );
        assert!(history.truncated);
        drop(windows);
        history.advance(at + Duration::hours(49));
        let windows: Vec<_> = history
            .accounts
            .values()
            .flat_map(|a| a.windows.values())
            .collect();
        assert_eq!(windows.len(), 64); // Future schedule may remain; old allocation must not.
        assert!(
            windows
                .iter()
                .all(|w| w.points.capacity() == 0 && w.resets.capacity() == 0)
        );
    }
}
