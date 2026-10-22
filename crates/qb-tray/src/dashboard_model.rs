//! Pure dashboard DTOs/projection. Clock, data and preferences are explicit inputs.
use crate::{state::*, v2};
use chrono::{DateTime, Datelike, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionsResponse {
    pub now: Option<DateTime<Utc>>,
    pub active_window_secs: Option<u64>,
    pub sessions: Vec<Session>,
    pub truncated: bool,
    pub move_cost: Option<MoveCost>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MoveCostAggregate {
    pub tokens: u64,
    pub known_count: u64,
    pub unknown_count: u64,
    pub observed_since: Option<DateTime<Utc>>,
    pub recording_complete: bool,
    pub tokens_saturated: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MoveCost {
    pub hour: MoveCostAggregate,
    pub rolling24h: MoveCostAggregate,
    pub clock_adjusted: bool,
}
impl MoveCost {
    pub fn lines(&self) -> [String; 2] {
        self.lines_with_offset(None)
    }
    pub fn lines_at(&self, offset: FixedOffset) -> [String; 2] {
        self.lines_with_offset(Some(offset))
    }
    fn lines_with_offset(&self, offset: Option<FixedOffset>) -> [String; 2] {
        let value = |a: &MoveCostAggregate| {
            if a.known_count == 0 && a.unknown_count > 0 {
                return crate::i18n::static_text("미측정").to_owned();
            }
            if a.known_count == 0 {
                return crate::i18n::static_text("관측된 이동 없음").to_owned();
            }
            crate::i18n::format(
                "{0}{1} 토큰",
                &[
                    (if a.unknown_count > 0 || !a.recording_complete || a.tokens_saturated {
                        crate::i18n::static_text("최소 ")
                    } else {
                        ""
                    })
                    .to_string(),
                    (a.tokens).to_string(),
                ],
            )
        };
        let a = &self.rolling24h;
        [
            crate::i18n::format(
                "계정 이동으로 다시 읽은 토큰 · 1시간 {0} · 24시간 {1}",
                &[(value(&self.hour)).to_string(), (value(a)).to_string()],
            ),
            crate::i18n::format(
                "측정 {0}회 · 미측정 {1}회 · {2}{3}",
                &[
                    (a.known_count).to_string(),
                    (a.unknown_count).to_string(),
                    (a.observed_since
                        .map(|t| {
                            crate::i18n::format(
                                "{0}부터 관측",
                                &[match offset {
                                    Some(offset) => crate::i18n::date_time(
                                        &t.with_timezone(&offset),
                                        "%m/%d %H:%M",
                                    ),
                                    None => crate::i18n::date_time(
                                        &t.with_timezone(&chrono::Local),
                                        "%m/%d %H:%M",
                                    ),
                                }],
                            )
                        })
                        .unwrap_or_else(|| {
                            crate::i18n::static_text("관측 시작 시각 확인 전").into()
                        }))
                    .to_string(),
                    (if !a.recording_complete {
                        crate::i18n::static_text(" · 일부 기록 누락")
                    } else if self.clock_adjusted {
                        crate::i18n::static_text(" · 시각 보정 포함")
                    } else {
                        ""
                    })
                    .to_string(),
                ],
            ),
        ]
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Client {
    pub kind: String,
    pub product: Option<String>,
    pub version: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Label {
    pub text: String,
    pub source: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Inflight {
    pub requests: Option<u64>,
    pub oldest_started_at: Option<DateTime<Utc>>,
    pub ws_open: Option<u64>,
    pub ws_response_in_flight: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PacingWait {
    pub waiting: Option<u64>,
    pub since: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RateWait {
    pub count: Option<u64>,
    pub total_ms: Option<u64>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Move {
    pub at: Option<DateTime<Utc>>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub reason: String,
    pub next_turn_cache: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Session {
    pub id: String,
    pub provider: String,
    pub client: Client,
    pub label: Option<Label>,
    pub account: String,
    pub first_seen_at: Option<DateTime<Utc>>,
    pub bound_since: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub active: bool,
    pub inflight: Inflight,
    pub inflight_known: bool,
    pub pacing_wait: Option<PacingWait>,
    pub model: Option<String>,
    pub subagents_today: Option<u64>,
    pub requests_today: Option<u64>,
    pub failures_today: Option<u64>,
    pub rate_wait_today: RateWait,
    pub moves_today: Vec<Move>,
    pub first_account_today: Option<String>,
    pub marked_for_move: bool,
    pub outside_git: Option<bool>,
    pub counters_complete: bool,
    pub truncated: bool,
    pub subagents_known: bool,
    pub moves_known: bool,
    pub metadata_truncated: bool,
}
impl Default for Session {
    fn default() -> Self {
        Self {
            id: Default::default(),
            provider: Default::default(),
            client: Default::default(),
            label: None,
            account: Default::default(),
            first_seen_at: None,
            bound_since: None,
            last_used_at: None,
            active: false,
            inflight: Default::default(),
            // Legacy/mock snapshots predate this loss-of-evidence flag.
            inflight_known: true,
            pacing_wait: None,
            model: None,
            subagents_today: None,
            requests_today: None,
            failures_today: None,
            rate_wait_today: Default::default(),
            moves_today: vec![],
            first_account_today: None,
            marked_for_move: false,
            outside_git: None,
            counters_complete: false,
            truncated: false,
            subagents_known: false,
            moves_known: false,
            metadata_truncated: false,
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub session: Session,
    /// Detail-only, pre-truncated by core. Never place on Session/list rows.
    pub original_short_id: Option<String>,
    pub recent_requests: Vec<RequestSummary>,
    pub timeline: Vec<Placement>,
    pub recent_requests_known: bool,
    pub timeline_known: bool,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Placement {
    pub at: Option<DateTime<Utc>>,
    pub kind: String,
    pub account: String,
    pub from: Option<String>,
    pub reason: Option<String>,
    pub next_turn_cache: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Failover {
    pub from: String,
    pub to: String,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct RequestSummary {
    pub ts: Option<DateTime<Utc>>,
    pub outcome: String,
    pub status: Option<u16>,
    pub ttfb_ms: Option<u64>,
    pub dur_ms: Option<u64>,
    pub rate_wait_ms: Option<u64>,
    pub rate_retries: Option<u64>,
    pub failover: Option<Failover>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ActivityEvent {
    pub seq: u64,
    pub ts: Option<DateTime<Utc>>,
    pub kind: String,
    pub provider: String,
    pub account: Option<String>,
    pub account_to: Option<String>,
    pub notable: bool,
    pub data: ActivityData,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ActivityData {
    pub window: Option<String>,
    pub reset_at: Option<DateTime<Utc>>,
    pub moved_groups: Option<u64>,
    pub count: Option<u64>,
    pub max_wait_ms: Option<u64>,
    pub relayed: Option<u64>,
    pub reason: Option<String>,
    pub session: Option<String>,
    pub sessions: Vec<String>,
    pub attention: Option<String>,
    pub outcome: Option<String>,
    pub credits_used: Option<serde_json::Number>,
    pub groups_hit: Option<u64>,
    pub sessions_truncated: bool,
    // Presentation-only enrichment from current in-memory sessions/accounts.
    // These are explicitly NOT accepted from activity wire/disk JSON.
    #[serde(skip)]
    pub label: Option<String>,
    #[serde(skip)]
    pub labels: Vec<String>,
    #[serde(rename = "active")]
    pub active_sessions: Option<u64>,
    pub threshold: Option<u64>,
    #[serde(skip)]
    pub next_turn_cache: Option<String>,
    #[serde(skip)]
    pub no_eligible: bool,
    #[serde(skip)]
    pub actionable: bool,
    #[serde(skip)]
    pub allow_credits: bool,
    #[serde(skip)]
    pub disabled_accounts: Option<u64>,
    #[serde(skip)]
    pub ok: Option<bool>,
    #[serde(rename = "recovered")]
    pub after_failure: bool,
    #[serde(skip)]
    pub needs_relogin: bool,
    pub action: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub balance_start: Option<serde_json::Number>,
    #[serde(skip)]
    pub balance_stop: Option<serde_json::Number>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct UsageHistory {
    pub accounts: Vec<AccountHistory>,
    pub truncated: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AccountHistory {
    pub id: String,
    pub windows: Vec<HistoryWindow>,
    pub credits_spans: Vec<(DateTime<Utc>, Option<DateTime<Utc>>)>,
    pub truncated: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HistoryWindow {
    pub name: String,
    pub points: Vec<(DateTime<Utc>, Option<f64>)>,
    pub resets: Vec<DateTime<Utc>>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ActivityResponse {
    pub events: Vec<ActivityEvent>,
    pub next_seq: u64,
    pub truncated: bool,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SessionChanges {
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub resync: bool,
}

#[derive(Clone, Debug)]
pub struct Clock {
    pub now: DateTime<FixedOffset>,
    pub day_start: DateTime<Utc>,
    pub day_end: DateTime<Utc>,
}
impl Clock {
    pub fn utc(&self) -> DateTime<Utc> {
        self.now.with_timezone(&Utc)
    }
    pub fn stamp(&self, at: Option<DateTime<Utc>>) -> String {
        at.map(|t| {
            t.with_timezone(self.now.offset())
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(unknown)
    }
    pub fn time(&self, at: Option<DateTime<Utc>>) -> String {
        let Some(at) = at else {
            return unknown();
        };
        let local = at.with_timezone(self.now.offset());
        if crate::i18n::language() == crate::i18n::UiLanguage::En {
            return crate::i18n::date_time(&local, "%H:%M");
        }
        let delta = local
            .date_naive()
            .signed_duration_since(self.now.date_naive())
            .num_days();
        match delta {
            0 => local.format("%H:%M").to_string(),
            -1 => crate::i18n::format("어제 {0}", &[(local.format("%H:%M")).to_string()]),
            1 => crate::i18n::format("내일 {0}", &[(local.format("%H:%M")).to_string()]),
            _ => format!(
                "{}/{} {}",
                local.month(),
                local.day(),
                local.format("%H:%M")
            ),
        }
    }
    pub fn age(&self, at: Option<DateTime<Utc>>) -> String {
        let Some(at) = at else {
            return unknown();
        };
        let secs = self.utc().signed_duration_since(at).num_seconds().max(0);
        match secs {
            0..=59 => crate::i18n::static_text("방금").into(),
            60..=3599 => crate::i18n::format("{0}분 전", &[(secs / 60).to_string()]),
            3600..=86399 => crate::i18n::format("{0}시간 전", &[(secs / 3600).to_string()]),
            _ => self.time(Some(at)),
        }
    }
    pub fn day_label(&self, at: DateTime<Utc>) -> String {
        let local = at.with_timezone(self.now.offset());
        let day = local.date_naive();
        let prefix = match self.now.date_naive().signed_duration_since(day).num_days() {
            0 => crate::i18n::static_text("오늘 · "),
            -1 => "",
            1 => crate::i18n::static_text("어제 · "),
            _ => "",
        };
        let weekday = [
            crate::i18n::static_text("월"),
            crate::i18n::static_text("화"),
            crate::i18n::static_text("수"),
            crate::i18n::static_text("목"),
            crate::i18n::static_text("금"),
            crate::i18n::static_text("토"),
            crate::i18n::static_text("일"),
        ][local.weekday().num_days_from_monday() as usize];
        if crate::i18n::language() == crate::i18n::UiLanguage::En {
            format!("{prefix}{}({weekday})", local.format("%b %-d"))
        } else {
            format!("{prefix}{}/{}({weekday})", local.month(), local.day())
        }
    }
}
#[derive(Clone, Debug)]
pub struct Connection {
    pub core_ok: bool,
    pub stream_ok: bool,
    pub retry_seconds: u32,
}
impl Default for Connection {
    fn default() -> Self {
        Self {
            core_ok: true,
            stream_ok: true,
            retry_seconds: 0,
        }
    }
}
#[derive(Clone, Debug)]
pub struct DashboardSnapshot {
    /// Last successful dashboard data publication; independent of the UI clock.
    pub data_at: Option<DateTime<Utc>>,
    pub accounts: v2::View,
    pub sessions: SessionsResponse,
    pub activity: Vec<ActivityEvent>,
    pub activity_truncated: bool,
    pub usage: UsageHistory,
    pub connection: Connection,
    pub revision: u64,
}
impl DashboardSnapshot {
    pub fn new(accounts: v2::View) -> Self {
        Self {
            data_at: None,
            accounts,
            sessions: Default::default(),
            activity: vec![],
            activity_truncated: false,
            usage: Default::default(),
            connection: Default::default(),
            revision: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id: String,
    pub account: String,
    pub account_provider: String,
    pub account_chip: Option<v2::Chip>,
    pub label: String,
    pub client: String,
    pub short_id: String,
    pub model: String,
    pub start: String,
    pub activity: String,
    pub requests: String,
    pub memo: String,
    pub label_source: String,
    pub dot: String,
    pub inflight: bool,
    pub inflight_known: bool,
    pub active: bool,
    pub last_used: Option<DateTime<Utc>>,
    pub first_seen: Option<DateTime<Utc>>,
    pub request_count: Option<u64>,
    pub selected: bool,
}
#[derive(Clone, Debug)]
pub struct AccountGroup {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub plan: String,
    pub chip: String,
    pub tone: v2::Tone,
    pub active: u64,
    pub threshold: Option<u64>,
    pub bound: u64,
    pub remaining: String,
    pub reset: String,
    pub remaining_pct: Option<u8>,
    pub windows: Vec<AccountUsageWindow>,
    pub forecast_lines: Vec<String>,
    pub forecast_tooltip: String,
    pub forecast_warning: bool,
    pub relogin: bool,
    /// Shared tray reason; nonempty only for re-login, including linked accounts.
    pub relogin_reason: String,
    pub paused: bool,
    pub disabled: bool,
    pub collapsed: bool,
    pub no_sessions: String,
    pub rows: Vec<SessionRow>,
}
#[derive(Clone, Debug)]
pub struct AccountUsageWindow {
    pub label: String,
    pub remaining: String,
    pub reset: String,
    pub remaining_pct: u8,
}
#[derive(Clone, Debug)]
pub struct EventRow {
    pub kind: String,
    pub seq: u64,
    pub day: String,
    pub time: String,
    pub tag: String,
    pub tone: v2::Tone,
    pub title: String,
    pub body: String,
    pub account: Option<String>,
    pub account_label: Option<String>,
    pub session: Option<String>,
    pub session_label: Option<String>,
    pub relogin: bool,
    pub notable: bool,
    pub category: EventFilter,
    pub at: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug)]
pub struct DetailView {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub raw_prefix: Option<String>,
    pub status: String,
    /// `status` is chip text; assignment/serving text is separate.
    pub status_tone: v2::Tone,
    pub account_status: String,
    pub facts: Vec<(String, String)>,
    pub timeline: Vec<String>,
    pub requests: Vec<String>,
    pub expired: bool,
}
#[derive(Clone, Debug)]
pub struct Series {
    pub name: String,
    pub points: Vec<(DateTime<Utc>, Option<f64>)>,
    pub resets: Vec<DateTime<Utc>>,
    pub future_resets: Vec<DateTime<Utc>>,
    pub remaining: Option<u8>,
}
#[derive(Clone, Debug)]
pub struct UsageTile {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub plan: String,
    pub chip: String,
    pub tone: v2::Tone,
    pub windows: Vec<AccountUsageWindow>,
    pub forecast_lines: Vec<String>,
    pub forecast_tooltip: String,
    pub forecast_warning: bool,
    /// Display-only future lines; never included in observed history or hit tables.
    pub projections: Vec<UsageForecastSeries>,
    /// Latest valid account/window timestamp or known history point, never a null/future point.
    pub last_observed: Option<DateTime<Utc>>,
    pub paused: bool,
    pub series: Vec<Series>,
    pub credits_spans: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    pub unconfirmed: bool,
    pub truncated: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub struct UsageForecastSeries {
    pub name: String,
    /// Remaining percent, projected using the core's selected rate and used_pct.
    pub points: [(DateTime<Utc>, f64); 2],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmptyState {
    None,
    NoAccounts,
    NoSessions(String),
    NoMatch(String),
}
#[derive(Clone, Debug)]
pub struct DashboardView {
    pub revision: u64,
    pub clock: Clock,
    pub data_at: Option<DateTime<Utc>>,
    pub connection: Connection,
    pub summaries: Vec<String>,
    pub provider_summaries: Vec<v2::Summary>,
    pub provider_usage: Vec<crate::provider_usage::Projection>,
    pub todo: usize,
    pub session_count: usize,
    pub active_count: usize,
    pub inflight_count: usize,
    pub inflight_count_known: bool,
    pub counters_unknown: bool,
    pub move_cost_lines: Option<[String; 2]>,
    pub provider_counts: [usize; 3],
    pub groups: Vec<AccountGroup>,
    pub flat: Vec<SessionRow>,
    pub events: Vec<EventRow>,
    pub recent: Vec<EventRow>,
    pub event_counts: Vec<(EventFilter, usize)>,
    pub detail: Option<DetailView>,
    pub usage: Vec<UsageTile>,
    pub empty: EmptyState,
    pub last_session: Option<String>,
    pub truncated: bool,
    pub event_empty: String,
    pub activity_truncated: bool,
}
impl DashboardView {
    pub fn inflight_text(&self) -> String {
        if self.inflight_count_known {
            self.inflight_count.to_string()
        } else if self.inflight_count > 0 {
            format!("≥{}", self.inflight_count)
        } else {
            unknown()
        }
    }
}
pub fn unknown() -> String {
    "—".into()
}
pub fn clean(value: &str) -> String {
    value.chars().filter(|c| !c.is_control() && !matches!(c,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{2028}'..='\u{202e}'|'\u{2066}'..='\u{2069}')).take(256).collect()
}
pub fn folder(value: &str) -> String {
    let value = clean(
        value
            .trim_end_matches(['\\', '/'])
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(""),
    )
    .chars()
    .take(64)
    .collect::<String>();
    if value.is_empty() || matches!(value.trim(), "." | "..") || value.contains(':') {
        String::new()
    } else {
        value.trim().into()
    }
}
pub fn hash(value: &str) -> Option<String> {
    (value.len() == 12
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| value.to_string())
}
fn count(value: Option<u64>) -> String {
    value
        .map(|v| v2::group_number(&v.to_string()))
        .unwrap_or_else(unknown)
}
pub fn duration(ms: Option<u64>) -> String {
    if crate::i18n::language() == crate::i18n::UiLanguage::En {
        return ms
            .map(|v| {
                if v < 1000 {
                    format!("{v}ms")
                } else {
                    crate::i18n::duration(v / 1000)
                }
            })
            .unwrap_or_else(unknown);
    }
    ms.map(|v| format!("{}:{:02}", v / 60000, v / 1000 % 60))
        .unwrap_or_else(unknown)
}
fn client(s: &Session) -> String {
    match s.client.kind.as_str() {
        "unknown"
            if matches!(
                s.client.product.as_deref(),
                Some("codex-tui" | "codex_cli_rs")
            ) =>
        {
            "Codex CLI".into()
        }
        "unknown" if s.client.product.as_deref() == Some("codex_vscode") => "Codex VS Code".into(),
        "unknown" if s.client.product.as_deref() == Some("codex_exec") => "Codex exec".into(),
        "unknown"
            if matches!(
                s.client.product.as_deref(),
                Some("Codex Desktop" | "codex_desktop")
            ) =>
        {
            crate::i18n::static_text("Codex 앱").into()
        }
        "codex_cli" | "codex_cli_rs" => "Codex CLI".into(),
        "codex_vscode" => "Codex VS Code".into(),
        "codex_exec" => "Codex exec".into(),
        "codex_desktop" => crate::i18n::static_text("Codex 앱").into(),
        "claude_cli" => "Claude Code CLI".into(),
        "claude_vscode" => "Claude Code VS Code".into(),
        "claude_sdk" => "Claude Agent SDK".into(),
        _ => {
            let p = folder(s.client.product.as_deref().unwrap_or(""));
            if p.is_empty() {
                v2::provider_label(&s.provider).into()
            } else {
                p
            }
        }
    }
}
fn label(s: &Session) -> String {
    s.label
        .as_ref()
        .map(|l| folder(&l.text))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            crate::i18n::format(
                "{0} 세션 {1}",
                &[
                    (client(s)).to_string(),
                    (hash(&s.id)
                        .unwrap_or_default()
                        .chars()
                        .take(6)
                        .collect::<String>())
                    .to_string(),
                ],
            )
        })
}
fn source(s: &Session) -> String {
    match s.label.as_ref().map(|l| l.source.as_str()) {
        Some("launcher") => crate::i18n::static_text("codex-qb/claude-qb가 알려 준 작업 폴더 이름"),
        Some("codex_workspace") => {
            crate::i18n::static_text("Codex가 보낸 작업 폴더(git 저장소) 이름")
        }
        Some("claude_project") => crate::i18n::static_text("Claude Code 대화 기록 폴더 이름"),
        _ => crate::i18n::static_text("이름을 알 수 없어 클라이언트와 세션 번호로 표시합니다"),
    }
    .into()
}
fn flying(s: &Session) -> bool {
    s.inflight.requests.unwrap_or(0) > 0 || s.inflight.ws_response_in_flight
}
fn in_range(s: &Session, range: Range, clock: &Clock) -> bool {
    match range {
        Range::Active => s.active,
        Range::Hour => {
            flying(s)
                || s.last_used_at.is_some_and(|v| {
                    v <= clock.utc() && v >= clock.utc() - chrono::Duration::hours(1)
                })
        }
        Range::Today => s
            .last_used_at
            .is_some_and(|v| v >= clock.day_start && v < clock.day_end),
        Range::All => true,
    }
}
fn account_rows(view: &v2::View) -> Vec<&v2::AccountRow> {
    view.sections
        .iter()
        .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
        .collect()
}
fn account_name(view: &v2::View, id: &str) -> String {
    account_rows(view)
        .iter()
        .find(|r| r.raw.id == id)
        .map(|r| folder(&r.name))
        .unwrap_or_else(|| crate::i18n::static_text("알 수 없는 계정").into())
}
fn account_windows(a: &v2::AccountRow, clock: &Clock) -> Vec<AccountUsageWindow> {
    a.windows
        .iter()
        .filter(|w| w.account_scoped)
        .filter_map(|w| {
            w.remaining.map(|pct| AccountUsageWindow {
                label: if crate::i18n::language() == crate::i18n::UiLanguage::En {
                    w.raw.name.clone()
                } else {
                    w.label.clone()
                },
                remaining: if crate::i18n::language() == crate::i18n::UiLanguage::En {
                    format!("{pct}% left")
                } else {
                    crate::i18n::format("{0}% 남음", &[pct.to_string()])
                },
                reset: if crate::i18n::language() == crate::i18n::UiLanguage::En {
                    w.reset
                        .map(|at| {
                            let at = at.with_timezone(clock.now.offset());
                            format!(
                                "resets {}",
                                at.format(if at.date_naive() == clock.now.date_naive() {
                                    "%H:%M"
                                } else {
                                    "%b %-d %H:%M"
                                })
                            )
                        })
                        .unwrap_or_default()
                } else {
                    w.reset_text.clone().unwrap_or_default()
                },
                remaining_pct: pct,
            })
        })
        .collect()
}
fn observed_at(a: &v2::AccountRow, clock: &Clock) -> Option<DateTime<Utc>> {
    a.raw
        .observed_at
        .iter()
        .chain(
            a.windows
                .iter()
                .filter(|w| w.account_scoped)
                .filter_map(|w| w.raw.observed_at.as_ref()),
        )
        .filter_map(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .filter(|t| *t <= clock.utc())
        .max()
}

fn folding_barrier(e: &ActivityEvent) -> bool {
    e.kind == "quota_reset" || e.kind == "move" || e.data.moved_groups.unwrap_or(0) > 0
}

// Walk newest first. Barriers clear every window for the affected accounts.
fn folded_events(source: &[&ActivityEvent], accounts: &v2::View, clock: &Clock) -> Vec<EventRow> {
    let mut source = source.to_vec();
    source.sort_by(|a, b| b.ts.cmp(&a.ts).then(b.seq.cmp(&a.seq)));
    // An undated move or reset cannot be proven outside a candidate pair.
    let uncertain_barriers: Vec<_> = source
        .iter()
        .filter(|e| folding_barrier(e) && e.ts.is_none())
        .flat_map(|e| {
            [e.account.as_ref(), e.account_to.as_ref()]
                .into_iter()
                .flatten()
                .map(move |a| (e.provider.clone(), Some(a.clone())))
        })
        .collect();
    // Legacy rows may omit the window. Use only a unique supplied event window
    // for this exact provider/account/reset, never current account data or duration.
    let mut known_windows: BTreeMap<_, BTreeSet<String>> = BTreeMap::new();
    for e in &source {
        if e.kind == "quota_reached"
            && let (Some(account), Some(reset), Some(window)) = (
                e.account.as_ref(),
                e.data.reset_at,
                e.data
                    .window
                    .as_deref()
                    .map(str::trim)
                    .filter(|w| !w.is_empty()),
            )
        {
            known_windows
                .entry((e.provider.clone(), account.clone(), reset))
                .or_default()
                .insert(window.to_owned());
        }
    }
    let mut rows: Vec<EventRow> = vec![];
    let mut seen = BTreeMap::new();
    let mut repeats: BTreeMap<usize, Vec<DateTime<Utc>>> = BTreeMap::new();
    for e in source {
        if folding_barrier(e) {
            seen.retain(|(p, a, _, _), _| {
                !(p == &e.provider
                    && (Some(a) == e.account.as_ref() || Some(a) == e.account_to.as_ref()))
            });
        }
        let foldable = e.kind == "quota_reached"
            && !e.data.no_eligible
            && e.data.moved_groups.unwrap_or(0) == 0
            && !e.provider.trim().is_empty()
            && e.account.as_deref().is_some_and(|a| !a.trim().is_empty())
            && e.data.reset_at.is_some()
            && e.ts.is_some()
            && !uncertain_barriers.contains(&(e.provider.clone(), e.account.clone()));
        if foldable {
            let key = (
                e.provider.clone(),
                e.account.clone().unwrap(),
                e.data
                    .window
                    .as_deref()
                    .map(str::trim)
                    .filter(|w| !w.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        let windows = known_windows.get(&(
                            e.provider.clone(),
                            e.account.clone().unwrap(),
                            e.data.reset_at.unwrap(),
                        ))?;
                        (windows.len() == 1).then(|| windows.iter().next().unwrap().clone())
                    }),
                e.data.reset_at.unwrap(),
            );
            if let Some(&index) = seen.get(&key) {
                repeats.entry(index).or_default().push(e.ts.unwrap());
                // Preserve supplied window text from a folded row without
                // replacing the newest event's identity, time or body.
                if e.data
                    .window
                    .as_deref()
                    .is_some_and(|window| !window.trim().is_empty())
                    && let Some(row) = event_row(e, accounts, clock)
                {
                    rows[index].title = row.title;
                }
                continue;
            }
            seen.insert(key, rows.len());
        }
        if e.ts.is_some()
            && let Some(row) = event_row(e, accounts, clock)
        {
            rows.push(row);
        }
    }
    for (index, times) in repeats {
        let clocks = times
            .iter()
            .take(3)
            .map(|t| clock.time(Some(*t)))
            .collect::<Vec<_>>()
            .join(", ");
        rows[index].body.push_str(&crate::i18n::format(
            "\n같은 알림 {0}번 더 · {1}",
            &[(times.len()).to_string(), clocks.to_string()],
        ));
        if times.len() > 3 {
            rows[index].body.push_str(&crate::i18n::format(
                " 외 {0}번",
                &[(times.len() - 3).to_string()],
            ));
        }
    }
    rows
}

fn session_row(
    s: &Session,
    accounts: &v2::View,
    clock: &Clock,
    selected: Option<&str>,
) -> SessionRow {
    let account = account_rows(accounts)
        .into_iter()
        .find(|a| a.raw.id == s.account);
    let mut notes = vec![];
    if let Some(p) = &s.pacing_wait
        && p.waiting.unwrap_or(0) > 0
    {
        let elapsed = p
            .since
            .map(|t| {
                clock
                    .utc()
                    .signed_duration_since(t)
                    .num_seconds()
                    .max(0)
                    .to_string()
            })
            .unwrap_or_else(unknown);
        notes.push(crate::i18n::format(
            "속도 조절 대기 {0}초",
            &[elapsed.to_string()],
        ));
    }
    if s.subagents_known && s.subagents_today.unwrap_or(0) > 0 {
        notes.push(crate::i18n::format(
            "하위 에이전트 {0}",
            &[(count(s.subagents_today)).to_string()],
        ));
    }
    if let Some(m) = s
        .moves_today
        .iter()
        .filter(|m| {
            m.at.is_some_and(|t| t >= clock.day_start && t < clock.day_end)
        })
        .max_by_key(|m| m.at)
    {
        notes.push(crate::i18n::format(
            "{0} 옮겨짐",
            &[(clock.time(m.at)).to_string()],
        ));
    }
    if s.provider == "codex" && s.outside_git == Some(true) {
        notes.push(crate::i18n::static_text("git 저장소 밖에서 실행").into());
    }
    if account_rows(accounts)
        .iter()
        .any(|a| a.raw.id == s.account && a.raw.credits_in_use == Some(true))
    {
        notes.push(crate::i18n::static_text("크레딧으로 처리 중").into());
    }
    if !s.moves_known {
        notes.push(crate::i18n::static_text("계정 이력 일부 확인 전").into());
    }
    let inflight = flying(s);
    let dot = if !s.inflight_known {
        crate::i18n::static_text("— · 요청 상태 확인 전").into()
    } else if inflight {
        crate::i18n::format(
            "요청 중 · {0}건",
            &[(count(s.inflight.requests)).to_string()],
        )
    } else if s.active {
        crate::i18n::static_text("5분 안에 활동").into()
    } else {
        crate::i18n::static_text("쉬는 중 · 5분 넘게 요청 없음").into()
    };
    let activity = if !s.inflight_known {
        crate::i18n::format(
            "요청 상태 확인 전 · 마지막 활동 {0}",
            &[(clock.age(s.last_used_at)).to_string()],
        )
    } else if inflight {
        crate::i18n::format(
            "요청 중 {0}",
            &[(duration(s.inflight.oldest_started_at.map(|t| {
                clock
                    .utc()
                    .signed_duration_since(t)
                    .num_milliseconds()
                    .max(0) as u64
            })))
            .to_string()],
        )
    } else {
        clock.age(s.last_used_at)
    };
    SessionRow {
        id: s.id.clone(),
        account: account_name(accounts, &s.account),
        account_provider: v2::provider_label(&s.provider).into(),
        account_chip: account
            .filter(|a| a.state != v2::DisplayState::InUse)
            .and_then(|a| a.chip.clone()),
        label: label(s),
        client: client(s),
        short_id: s.id.chars().take(6).collect(),
        model: s
            .model
            .as_deref()
            .map(clean)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(unknown),
        start: s
            .first_seen_at
            .map(|v| clock.time(Some(v)))
            .unwrap_or_else(|| crate::i18n::static_text("이전 실행부터").into()),
        activity,
        requests: count(s.requests_today.filter(|_| s.counters_complete)),
        memo: notes.join(" · "),
        label_source: source(s),
        dot,
        inflight,
        inflight_known: s.inflight_known,
        active: s.active,
        last_used: s.last_used_at,
        first_seen: s.first_seen_at,
        request_count: s.requests_today.filter(|_| s.counters_complete),
        selected: selected == Some(&s.id),
    }
}
fn compare_rows(
    a: &SessionRow,
    b: &SessionRow,
    key: SortKey,
    descending: bool,
) -> std::cmp::Ordering {
    let cmp = match key {
        SortKey::Status => (a.inflight, a.active).cmp(&(b.inflight, b.active)),
        SortKey::Session => a.label.to_lowercase().cmp(&b.label.to_lowercase()),
        SortKey::Client => a
            .client
            .to_lowercase()
            .cmp(&b.client.to_lowercase())
            .then(a.short_id.cmp(&b.short_id)),
        SortKey::Account => a.account.cmp(&b.account),
        SortKey::Model => a.model.cmp(&b.model),
        SortKey::Start => a.first_seen.cmp(&b.first_seen),
        SortKey::LastActivity => a.last_used.cmp(&b.last_used),
        SortKey::Requests => a.request_count.cmp(&b.request_count),
        SortKey::Memo => a.memo.cmp(&b.memo),
    };
    let cmp = if descending { cmp.reverse() } else { cmp };
    if key == SortKey::LastActivity {
        b.inflight.cmp(&a.inflight).then(cmp).then(a.id.cmp(&b.id))
    } else {
        cmp.then(a.id.cmp(&b.id))
    }
}

pub fn project(
    snapshot: &DashboardSnapshot,
    state: &mut DashboardState,
    clock: &Clock,
) -> DashboardView {
    let all: Vec<_> = snapshot
        .sessions
        .sessions
        .iter()
        .filter(|s| hash(&s.id).is_some())
        .collect();
    let ranged: Vec<_> = all
        .iter()
        .copied()
        .filter(|s| in_range(s, state.range, clock))
        .collect();
    let provider_counts = [
        ranged.len(),
        ranged.iter().filter(|s| s.provider == "claude").count(),
        ranged.iter().filter(|s| s.provider == "codex").count(),
    ];
    let sessions: Vec<_> = ranged
        .iter()
        .copied()
        .filter(|s| {
            state.provider.matches(&s.provider)
                && state
                    .account_filter
                    .as_deref()
                    .is_none_or(|a| a == s.account)
        })
        .collect();
    let needle = clean(&state.search).to_lowercase();
    let matching: Vec<_> = sessions
        .iter()
        .copied()
        .filter(|s| {
            if needle.is_empty() {
                return true;
            }
            let row = session_row(s, &snapshot.accounts, clock, state.selected.as_deref());
            [
                row.label,
                row.short_id,
                s.id.clone(),
                row.account,
                row.client,
                row.model,
            ]
            .join(" ")
            .to_lowercase()
            .contains(&needle)
        })
        .collect();
    let mut flat: Vec<_> = matching
        .iter()
        .map(|s| session_row(s, &snapshot.accounts, clock, state.selected.as_deref()))
        .collect();
    flat.sort_by(|a, b| compare_rows(a, b, state.sort, state.descending));
    state.table_freeze.apply(&mut flat, |r| r.id.clone());
    let mut groups = vec![];
    for a in account_rows(&snapshot.accounts).into_iter().filter(|a| {
        state.provider.matches(&a.raw.provider)
            && state
                .account_filter
                .as_deref()
                .is_none_or(|id| id == a.raw.id)
    }) {
        let holds = a.raw.bound_sessions > 0 || all.iter().any(|s| s.account == a.raw.id);
        if a.raw.user_state == "disabled" && !holds {
            continue;
        }
        let mut rows: Vec<_> = matching
            .iter()
            .filter(|s| s.account == a.raw.id)
            .map(|s| session_row(s, &snapshot.accounts, clock, state.selected.as_deref()))
            .collect();
        rows.sort_by(|a, b| compare_rows(a, b, SortKey::LastActivity, true));
        state
            .group_orders
            .entry(a.raw.id.clone())
            .or_default()
            .frozen = state.table_freeze.frozen;
        state
            .group_orders
            .entry(a.raw.id.clone())
            .or_default()
            .apply(&mut rows, |r| r.id.clone());
        if !needle.is_empty() && rows.is_empty() {
            continue;
        }
        let remaining = a.usage.as_ref().and_then(|u| u.binding.as_ref());
        let windows = account_windows(a, clock);
        let blocked = a.raw.drain
            || a.raw.user_state != "active"
            || matches!(a.group, v2::Group::NeedsAction | v2::Group::Waiting)
            || a.raw.eligible_for_new_session == Some(false);
        let away = all
            .iter()
            .flat_map(|s| &s.moves_today)
            .filter(|m| {
                m.from.as_deref() == Some(&a.raw.id)
                    && m.at.is_some_and(|v| {
                        v >= clock.day_start && v < clock.day_end && v <= clock.utc()
                    })
            })
            .max_by_key(|m| m.at);
        let reason = if blocked {
            crate::i18n::static_text("새 세션을 배정하지 않습니다").into()
        } else if let Some(m) = away {
            crate::i18n::format(
                "{0}에 세션을 {1}로 옮겼습니다",
                &[
                    (clock.time(m.at)).to_string(),
                    (account_name(&snapshot.accounts, m.to.as_deref().unwrap_or(""))).to_string(),
                ],
            )
        } else {
            crate::i18n::static_text("새 세션 배정 가능").into()
        };
        groups.push(AccountGroup {
            id: a.raw.id.clone(),
            provider: a.raw.provider.clone(),
            name: folder(&a.name),
            plan: a.plan.clone().unwrap_or_else(unknown),
            chip: a.chip.as_ref().map(|c| c.text.clone()).unwrap_or_default(),
            tone: a.chip.as_ref().map_or(v2::Tone::Off, |c| c.tone),
            active: a.raw.active_sessions,
            threshold: a.raw.extra.get("spread_threshold").and_then(|v| v.as_u64()),
            bound: a.raw.bound_sessions,
            remaining: remaining
                .and_then(|w| {
                    w.remaining.map(|pct| {
                        crate::i18n::format(
                            "{0} {1}% 남음",
                            &[(w.label).to_string(), pct.to_string()],
                        )
                    })
                })
                .unwrap_or_else(|| crate::i18n::static_text("사용량 확인 전").into()),
            reset: remaining
                .and_then(|w| w.reset)
                .map(|v| crate::i18n::format("{0} 초기화", &[(clock.time(Some(v))).to_string()]))
                .unwrap_or_default(),
            remaining_pct: remaining.and_then(|w| w.remaining),
            windows,
            forecast_lines: a.forecast_lines.clone(),
            forecast_tooltip: a.forecast_tooltip.clone(),
            forecast_warning: a.forecast_warning,
            relogin: a.raw.health.kind == "needs_relogin" && a.menu.show_relogin,
            relogin_reason: if a.state == v2::DisplayState::Relogin {
                a.reason
                    .clone()
                    .unwrap_or_else(|| crate::i18n::static_text("다시 로그인 필요").into())
            } else {
                String::new()
            },
            paused: a.raw.user_state == "paused",
            disabled: a.raw.user_state == "disabled",
            collapsed: state.collapsed.contains(&a.raw.id),
            no_sessions: if reason.is_empty() {
                crate::i18n::static_text("연결된 세션 없음").into()
            } else {
                crate::i18n::format("연결된 세션 없음 · {0}", &[reason.to_string()])
            },
            rows,
        });
    }
    state.group_headers.frozen = state.table_freeze.frozen;
    state.group_headers.apply(&mut groups, |g| g.id.clone());
    groups.sort_by_key(|g| if g.provider == "claude" { 0 } else { 1 });
    let cutoff =
        clock.day_start - chrono::Duration::days(i64::from(state.event_days.saturating_sub(1)));
    let enriched: Vec<_> = snapshot
        .activity
        .iter()
        .filter(|e| {
            e.ts.is_some_and(|t| t <= clock.utc()) || (folding_barrier(e) && e.ts.is_none())
        })
        .map(|e| enriched_event(e, snapshot, &all))
        .collect();
    let mut events = folded_events(
        &enriched
            .iter()
            .filter(|e| e.ts.is_some_and(|t| t >= cutoff) || (folding_barrier(e) && e.ts.is_none()))
            .collect::<Vec<_>>(),
        &snapshot.accounts,
        clock,
    );
    let event_counts = EventFilter::ALL
        .iter()
        .map(|f| (*f, events.iter().filter(|e| f.matches(e)).count()))
        .collect();
    // Non-notable moves and resets must still break repeats in the feed.
    let mut recent = folded_events(
        &enriched
            .iter()
            .filter(|e| e.notable || folding_barrier(e))
            .collect::<Vec<_>>(),
        &snapshot.accounts,
        clock,
    );
    recent.retain(|e| e.notable);
    state
        .recent_freeze
        .apply(&mut recent, |r| r.seq.to_string());
    recent.truncate(8);
    let event_needle = clean(&state.event_search).to_lowercase();
    events.retain(|e| {
        state.event_filter.matches(e)
            && (event_needle.is_empty()
                || format!(
                    "{} {} {} {}",
                    e.title,
                    e.body,
                    e.session_label.as_deref().unwrap_or(""),
                    e.session.as_deref().unwrap_or("")
                )
                .to_lowercase()
                .contains(&event_needle))
    });
    let empty = if snapshot.accounts.empty {
        EmptyState::NoAccounts
    } else if flat.is_empty() && !needle.is_empty() {
        EmptyState::NoMatch(clean(&state.search))
    } else if flat.is_empty() {
        EmptyState::NoSessions(state.range.empty_title().into())
    } else {
        EmptyState::None
    };
    let detail = state
        .detail
        .as_ref()
        .filter(|d| state.selected.as_deref() == Some(&d.session.id))
        .map(|d| detail_view(d, &snapshot.accounts, clock, state.detail_expired))
        .or_else(|| {
            state
                .selected
                .as_deref()
                .and_then(hash)
                .map(|id| DetailView {
                    id: id.clone(),
                    title: crate::i18n::format("세션 {0}", &[id[..6].to_string()]),
                    subtitle: crate::i18n::format("세션 {0}", &[id.to_string()]),
                    raw_prefix: None,
                    status: crate::i18n::static_text("세션 정보를 확인하고 있습니다.").into(),
                    status_tone: v2::Tone::Off,
                    account_status: String::new(),
                    facts: vec![],
                    timeline: vec![],
                    requests: vec![],
                    expired: state.detail_expired,
                })
        });
    let last_session = all.iter().max_by_key(|s| s.last_used_at).map(|s| {
        crate::i18n::format(
            "마지막 세션: {0} ({1} · {2}) · {3} · {4}",
            &[
                (label(s)).to_string(),
                (client(s)).to_string(),
                (s.id.chars().take(6).collect::<String>()).to_string(),
                (account_name(&snapshot.accounts, &s.account)).to_string(),
                (clock.age(s.last_used_at)).to_string(),
            ],
        )
    });
    DashboardView {
        revision: snapshot.revision,
        clock: clock.clone(),
        data_at: snapshot.data_at,
        connection: snapshot.connection.clone(),
        summaries: snapshot
            .accounts
            .summaries
            .iter()
            .map(|s| s.accessible_name.clone())
            .collect(),
        provider_summaries: snapshot.accounts.summaries.clone(),
        provider_usage: crate::provider_usage::project_snapshot(snapshot, clock),
        todo: snapshot.accounts.summaries.iter().map(|s| s.todo).sum(),
        session_count: sessions.len(),
        active_count: sessions.iter().filter(|s| s.active).count(),
        inflight_count: sessions.iter().filter(|s| flying(s)).count(),
        inflight_count_known: !snapshot.sessions.truncated
            && sessions.iter().all(|s| s.inflight_known),
        counters_unknown: sessions.iter().any(|s| !s.counters_complete),
        move_cost_lines: snapshot
            .sessions
            .move_cost
            .as_ref()
            .map(|cost| cost.lines_at(*clock.now.offset())),
        provider_counts,
        groups,
        flat,
        events,
        recent,
        event_counts,
        detail,
        usage: usage_tiles(snapshot, clock),
        empty,
        last_session,
        truncated: snapshot.sessions.truncated,
        event_empty: crate::i18n::static_text("이 기간에는 기록할 만한 일이 없었습니다.").into(),
        activity_truncated: snapshot.activity_truncated,
    }
}

pub fn upsert_activity(events: &mut Vec<ActivityEvent>, incoming: ActivityEvent) {
    if let Some(old) = events.iter_mut().find(|e| e.seq == incoming.seq) {
        *old = incoming;
    } else {
        events.push(incoming);
    }
    events.sort_by_key(|e| e.seq);
    if events.len() > 2000 {
        events.drain(..events.len() - 2000);
    }
}
pub fn reason(word: &str) -> &str {
    match word {
        "quota" => crate::i18n::static_text("한도 도달"),
        "needs_relogin" => crate::i18n::static_text("다시 로그인 필요"),
        "paused" => crate::i18n::static_text("일시정지"),
        "link_stale" => crate::i18n::static_text("CPA 갱신 멈춤"),
        "payment" => crate::i18n::static_text("결제 거절"),
        "permission" => crate::i18n::static_text("접근 거부"),
        "auth" => crate::i18n::static_text("인증 확인 대기"),
        "store_error" => crate::i18n::static_text("저장 오류"),
        "overload" => crate::i18n::static_text("몰린 세션 나눔"),
        "forced" => crate::i18n::static_text("쓸 수 없는 계정에서 전환"),
        "cold_rebalance" => crate::i18n::static_text("오래 쉬어 다시 배치"),
        "reset_hold_timeout" => crate::i18n::static_text("초기화를 기다렸지만 풀리지 않음"),
        "new" => crate::i18n::static_text("처음 배정"),
        "launch_model" => crate::i18n::static_text("시작 모델에 맞춰 처음 배정"),
        "model_choice" => crate::i18n::static_text("선택한 모델에 맞춰 계정 전환"),
        _ => crate::i18n::static_text("사용 불가"),
    }
}
fn cache_sentence(value: Option<&str>) -> &'static str {
    match value {
        Some("miss") => crate::i18n::static_text("다음 요청은 캐시 없이 문맥을 다시 보냈습니다"),
        Some("hit") => crate::i18n::static_text("다음 요청도 캐시를 썼습니다"),
        _ => "",
    }
}
fn enriched_event(
    e: &ActivityEvent,
    snapshot: &DashboardSnapshot,
    sessions: &[&Session],
) -> ActivityEvent {
    let mut event = e.clone();
    let d = &mut event.data;
    d.label = d
        .session
        .as_deref()
        .and_then(|id| sessions.iter().find(|s| s.id == id))
        .map(|s| label(s));
    d.labels = d
        .sessions
        .iter()
        .filter_map(|id| hash(id))
        .map(|id| {
            sessions
                .iter()
                .find(|s| s.id == id)
                .map(|s| label(s))
                .unwrap_or_else(|| id[..6].into())
        })
        .collect();
    d.no_eligible |= e.account.is_none() && e.kind == "quota_reached";
    d.disabled_accounts = Some(
        account_rows(&snapshot.accounts)
            .iter()
            .filter(|a| a.raw.provider == e.provider && a.raw.user_state == "disabled")
            .count() as u64,
    );
    d.allow_credits = account_rows(&snapshot.accounts)
        .iter()
        .any(|a| a.raw.provider == e.provider && v2::credits_eligible_for_resume(&a.raw));
    d.actionable = d.allow_credits || d.disabled_accounts.unwrap_or(0) > 0;
    d.ok = d
        .outcome
        .as_deref()
        .map(|s| matches!(s, "ok" | "rate_hold_ok"));
    if d.after_failure {
        d.ok = Some(true);
    }
    d.needs_relogin = d.attention.as_deref() == Some("needs_relogin");
    if let Some(s) = d
        .session
        .as_deref()
        .and_then(|id| sessions.iter().find(|s| s.id == id))
    {
        d.next_turn_cache = s
            .moves_today
            .iter()
            .find(|m| m.at == e.ts)
            .and_then(|m| m.next_turn_cache.clone());
    }
    event
}
fn event_row(e: &ActivityEvent, accounts: &v2::View, clock: &Clock) -> Option<EventRow> {
    let d = &e.data;
    let p = v2::provider_label(&e.provider);
    let a = account_name(accounts, e.account.as_deref().unwrap_or(""));
    let b = account_name(accounts, e.account_to.as_deref().unwrap_or(""));
    let win = d
        .window
        .as_deref()
        .filter(|w| !w.trim().is_empty())
        .map(v2::window_label)
        .unwrap_or_default();
    let reset = clock.time(d.reset_at);
    let name = d
        .label
        .as_deref()
        .map(folder)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            d.session
                .as_deref()
                .and_then(hash)
                .map(|s| s[..6].to_string())
                .unwrap_or_else(unknown)
        });
    let mut tag = crate::i18n::static_text("정보");
    let mut tone = v2::Tone::Off;
    let mut category = EventFilter::All;
    let (title, body) = match e.kind.as_str() {
        "quota_reached" => {
            category = EventFilter::Moves;
            if d.no_eligible {
                if d.actionable || d.allow_credits || d.disabled_accounts.unwrap_or(0) > 0 {
                    tag = crate::i18n::static_text("할 일");
                    tone = v2::Tone::Bad;
                    category = EventFilter::Tasks;
                }
                let body = if d.allow_credits {
                    crate::i18n::static_text("유료 크레딧 사용 허용을 확인하세요.").into()
                } else if d.disabled_accounts.unwrap_or(0) > 0 {
                    crate::i18n::format(
                        "꺼진 계정 {0}개를 켜면 이어서 처리할 수 있습니다.",
                        &[(count(d.disabled_accounts)).to_string()],
                    )
                } else {
                    crate::i18n::format(
                        "{0} 초기화. 그때까지 새 요청은 오류로 끝납니다.",
                        &[reset.to_string()],
                    )
                };
                (
                    crate::i18n::format("{0} 계정 모두 한도 도달", &[p.to_string()]),
                    body,
                )
            } else {
                tag = crate::i18n::static_text("한도 도달");
                tone = v2::Tone::Limit;
                (
                    crate::i18n::format(
                        "{0} {1} · {2}한도 도달",
                        &[
                            p.to_string(),
                            a.to_string(),
                            (if win.is_empty() {
                                String::new()
                            } else {
                                format!("{win} ")
                            })
                            .to_string(),
                        ],
                    ),
                    if d.moved_groups.unwrap_or(0) > 0 {
                        crate::i18n::format(
                            "연결된 세션 {0}개를 {1}로 옮겨 이어서 처리합니다. {2}는 {3}에 초기화됩니다.",
                            &[
                                (count(d.moved_groups)).to_string(),
                                b.to_string(),
                                a.to_string(),
                                reset.to_string(),
                            ],
                        )
                    } else {
                        crate::i18n::format(
                            "새 세션은 다른 계정에 배정합니다. {0}에 초기화됩니다.",
                            &[reset.to_string()],
                        )
                    },
                )
            }
        }
        "credits_started" => {
            tag = crate::i18n::static_text("크레딧");
            tone = v2::Tone::Warn;
            category = EventFilter::Credits;
            (
                crate::i18n::format(
                    "{0} {1} · 크레딧으로 처리 시작",
                    &[p.to_string(), a.to_string()],
                ),
                crate::i18n::format(
                    "{0} 한도에 도달해 유료 크레딧으로 계속 처리합니다.",
                    &[win.to_string()],
                ),
            )
        }
        "credits_stopped" => {
            tag = crate::i18n::static_text("초기화");
            category = EventFilter::Credits;
            let mut text =
                crate::i18n::static_text("크레딧 대신 구독 한도로 다시 처리합니다.").to_string();
            if let Some(spent) = d
                .credits_used
                .as_ref()
                .and_then(|n| n.as_f64())
                .filter(|n| n.is_finite() && *n >= 0.)
            {
                text.push_str(&crate::i18n::format(
                    " 크레딧으로 처리한 동안 {0} 크레딧을 썼습니다.",
                    &[(v2::group_number(&format!("{:.0}", spent.floor()))).to_string()],
                ));
            } else if let (Some(start), Some(stop)) = (&d.balance_start, &d.balance_stop)
                && let Some(spent) = decimal_spent(start, stop)
            {
                text.push_str(&crate::i18n::format(
                    " 크레딧으로 처리한 동안 {0} 크레딧을 썼습니다.",
                    &[spent.to_string()],
                ));
            }
            (
                crate::i18n::format(
                    "{0} {1} · {2} 한도 초기화",
                    &[p.to_string(), a.to_string(), win.to_string()],
                ),
                text,
            )
        }
        "quota_reset" => {
            tag = crate::i18n::static_text("초기화");
            (
                crate::i18n::format(
                    "{0} {1} · {2} 한도 초기화",
                    &[p.to_string(), a.to_string(), win.to_string()],
                ),
                crate::i18n::static_text("다시 새 세션을 받을 수 있습니다.").into(),
            )
        }
        "needs_relogin" | "link_stale" | "payment" | "permission" | "store_error" => {
            tag = crate::i18n::static_text("할 일");
            tone = v2::Tone::Bad;
            category = EventFilter::Tasks;
            let until = account_rows(accounts)
                .into_iter()
                .find(|row| Some(&row.raw.id) == e.account.as_ref())
                .and_then(|row| row.raw.health.until.as_deref())
                .and_then(v2::timestamp)
                .map(|at| clock.time(Some(at)))
                .unwrap_or_else(|| crate::i18n::static_text("상태를 다시 확인할 때").into());
            let body = match e.kind.as_str() {
                "needs_relogin" => crate::i18n::static_text("트레이에서 이 계정의 \"다시 로그인\"을 누르세요. 처리할 다른 계정 상태도 확인하세요.").into(),
                "link_stale" => {
                    crate::i18n::static_text("CPA가 이 계정의 토큰을 갱신하지 않습니다. CPA 실행을 확인하거나 계정 인수를 고려하세요.").into()
                }
                "payment" => crate::i18n::format("{0}까지 이 계정을 쉽니다.", &[until.to_string()]),
                "permission" => crate::i18n::format("이 계정은 {0}까지 이 모델을 쉽니다.", &[until.to_string()]),
                _ => crate::i18n::static_text("토큰을 저장하지 못해 다시 시도하고 있습니다. 로그를 확인하세요.").into(),
            };
            let title = match e.kind.as_str() {
                "store_error" => crate::i18n::static_text("토큰 저장 실패"),
                "permission" => crate::i18n::static_text("조직 접근 거부"),
                _ => reason(&e.kind),
            };
            (format!("{p} {a} · {title}"), body)
        }
        "move" => {
            if matches!(d.reason.as_deref(), Some("new" | "launch_model")) {
                return None;
            }
            tag = crate::i18n::static_text("계정 전환");
            tone = v2::Tone::Use;
            category = EventFilter::Moves;
            match d.reason.as_deref() {
                Some("model_choice") => (
                    crate::i18n::format(
                        "{0} · {1} 세션을 {2}로 옮김",
                        &[p.to_string(), name.to_string(), b.to_string()],
                    ),
                    crate::i18n::format(
                        "선택한 모델을 지원하는 {0}로 계정을 전환했습니다.",
                        &[b.to_string()],
                    ),
                ),
                Some("overload") => (
                    crate::i18n::format(
                        "{0} · 몰린 세션 {1}개를 나눔",
                        &[
                            p.to_string(),
                            (count(d.moved_groups.or(d.count))).to_string(),
                        ],
                    ),
                    crate::i18n::format(
                        "{0}에 활성 세션이 {1}개(기준 {2}개)로 몰리고 속도 제한이 반복되어, {3}를 {4}로 옮겼습니다. {5}",
                        &[
                            a.to_string(),
                            (count(d.active_sessions)).to_string(),
                            (count(d.threshold)).to_string(),
                            (if d.labels.is_empty() {
                                name.clone()
                            } else {
                                d.labels
                                    .iter()
                                    .map(|s| folder(s))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .to_string(),
                            b.to_string(),
                            (cache_sentence(d.next_turn_cache.as_deref())).to_string(),
                        ],
                    ),
                ),
                Some("cold_rebalance") => (
                    crate::i18n::format(
                        "{0} · 오래 쉬던 {1} 세션을 {2}로 다시 배치",
                        &[p.to_string(), name.to_string(), b.to_string()],
                    ),
                    crate::i18n::static_text("캐시가 이미 식어 있어 옮겨도 손해가 없습니다.")
                        .into(),
                ),
                Some("reset_hold_timeout") => (
                    crate::i18n::format(
                        "{0} · {1} 세션을 {2}로 옮김",
                        &[p.to_string(), name.to_string(), b.to_string()],
                    ),
                    crate::i18n::format(
                        "{0}의 한도 초기화를 기다렸지만 제때 풀리지 않아 옮겼습니다.",
                        &[a.to_string()],
                    ),
                ),
                _ => (
                    crate::i18n::format(
                        "{0} · {1} 세션을 {2}로 옮김",
                        &[p.to_string(), name.to_string(), b.to_string()],
                    ),
                    crate::i18n::format(
                        "{0}를 쓸 수 없게 되어({1}) 다음 요청부터 {2}에서 처리합니다.",
                        &[
                            a.to_string(),
                            (reason(d.attention.as_deref().or(d.reason.as_deref()).unwrap_or("")))
                                .to_string(),
                            b.to_string(),
                        ],
                    ),
                ),
            }
        }
        "overload_detected" => {
            tag = crate::i18n::static_text("분산 안 함");
            category = EventFilter::Moves;
            (crate::i18n::format("{0} {1} · 세션이 고르게 퍼져 있어 옮기지 않음", &[p.to_string(), a.to_string()]),crate::i18n::static_text("속도 제한이 반복됐지만 여유 있는 다른 계정이 없어 세션을 그대로 두고 요청 속도만 낮췄습니다.").into())
        }
        "rate_hold" => {
            tag = crate::i18n::static_text("속도 조절");
            category = EventFilter::Pacing;
            let max = d.max_wait_ms.map(|v| v.div_ceil(1000));
            let body = if d.relayed.unwrap_or(0) > 0 {
                crate::i18n::format(
                    "{0}부터 요청 {1}건이 속도 제한에 걸렸고, 같은 계정에서 최대 {2}초 기다렸습니다. {3}건은 처리했고 {4}건은 속도 제한 오류를 클라이언트에 그대로 전달했습니다.",
                    &[
                        (clock.time(e.ts)).to_string(),
                        (count(d.count)).to_string(),
                        (count(max)).to_string(),
                        (count(d.count.zip(d.relayed).map(|(n, r)| n.saturating_sub(r))))
                            .to_string(),
                        (count(d.relayed)).to_string(),
                    ],
                )
            } else {
                crate::i18n::format(
                    "{0}부터 요청 {1}건이 속도 제한에 걸렸고, 같은 계정에서 최대 {2}초 기다린 뒤 모두 처리했습니다. 세션은 옮기지 않았습니다.",
                    &[
                        (clock.time(e.ts)).to_string(),
                        (count(d.count)).to_string(),
                        (count(max)).to_string(),
                    ],
                )
            };
            (
                crate::i18n::format(
                    "{0} {1} · 속도 제한을 기다려서 처리",
                    &[p.to_string(), a.to_string()],
                ),
                body,
            )
        }
        "burst" => {
            tag = crate::i18n::static_text("할 일");
            tone = v2::Tone::Bad;
            category = EventFilter::Tasks;
            (
                crate::i18n::static_text("새 대화 급증").into(),
                crate::i18n::static_text("클라이언트의 반복 실행을 확인하세요. 10분 동안 새 대화가 설정 한도를 넘었습니다.")
                    .into(),
            )
        }
        "token_refresh" | "linked_reload" => {
            tag = if d.needs_relogin {
                crate::i18n::static_text("할 일")
            } else {
                crate::i18n::static_text("토큰")
            };
            tone = if d.needs_relogin {
                v2::Tone::Bad
            } else {
                v2::Tone::Off
            };
            category = if d.needs_relogin {
                EventFilter::Tasks
            } else {
                EventFilter::Tokens
            };
            if d.ok == Some(false) {
                (
                    crate::i18n::format(
                        "{0} {1} · 토큰 갱신 실패",
                        &[p.to_string(), a.to_string()],
                    ),
                    crate::i18n::static_text("다음 갱신 시각에 다시 시도합니다.").into(),
                )
            } else if d.after_failure {
                (
                    crate::i18n::format(
                        "{0} {1} · 토큰 갱신을 다시 시도해 성공",
                        &[p.to_string(), a.to_string()],
                    ),
                    crate::i18n::format(
                        "새 토큰은 {0}까지 유효합니다.",
                        &[(clock.time(d.expires_at)).to_string()],
                    ),
                )
            } else {
                (
                    crate::i18n::format(
                        "{0} {1} · 토큰 갱신 완료",
                        &[p.to_string(), a.to_string()],
                    ),
                    crate::i18n::format(
                        "새 토큰은 {0}까지 유효합니다.",
                        &[(clock.time(d.expires_at)).to_string()],
                    ),
                )
            }
        }
        "login" => {
            tag = crate::i18n::static_text("토큰");
            category = EventFilter::Tokens;
            (
                crate::i18n::format("{0} {1} · 로그인 완료", &[p.to_string(), a.to_string()]),
                crate::i18n::static_text("계정 로그인을 마쳤습니다.").into(),
            )
        }
        "account_changed" | "pause" | "resume" | "drain" | "allow_credits" | "enable"
        | "disable" => {
            tag = crate::i18n::static_text("설정");
            let action = d.action.as_deref().unwrap_or(&e.kind);
            let (title, body) = match action {
                "pause" => (
                    crate::i18n::static_text("일시정지"),
                    crate::i18n::static_text("새 세션을 배정하지 않습니다."),
                ),
                "resume" => (
                    crate::i18n::static_text("재개"),
                    crate::i18n::static_text("다시 새 세션을 받을 수 있습니다."),
                ),
                "drain" | "drain_on" => (
                    crate::i18n::static_text("새 대화 받지 않음"),
                    crate::i18n::static_text("기존 세션은 유지하고 새 세션을 배정하지 않습니다."),
                ),
                "drain_off" => (
                    crate::i18n::static_text("새 대화 받기"),
                    crate::i18n::static_text("다시 새 세션을 받을 수 있습니다."),
                ),
                "credits_on" => (
                    crate::i18n::static_text("유료 크레딧 사용 허용 켜짐"),
                    crate::i18n::static_text(
                        "한도에 도달하면 유료 크레딧으로 계속 처리할 수 있습니다.",
                    ),
                ),
                "allow_credits" if d.allow_credits => (
                    crate::i18n::static_text("유료 크레딧 사용 허용 켜짐"),
                    crate::i18n::static_text(
                        "한도에 도달하면 유료 크레딧으로 계속 처리할 수 있습니다.",
                    ),
                ),
                "allow_credits" | "credits_off" => (
                    crate::i18n::static_text("유료 크레딧 사용 허용 꺼짐"),
                    crate::i18n::static_text(
                        "이제 한도에 도달하면 크레딧을 쓰지 않고 다른 계정으로 넘깁니다.",
                    ),
                ),
                "disable" => (
                    crate::i18n::static_text("계정 꺼짐"),
                    crate::i18n::static_text("이 계정에 요청을 배정하지 않습니다."),
                ),
                "enable" => (
                    crate::i18n::static_text("계정 켜짐"),
                    crate::i18n::static_text("계정 상태를 확인해 요청을 배정합니다."),
                ),
                _ => (
                    crate::i18n::static_text("계정 설정 변경"),
                    crate::i18n::static_text("계정 설정을 반영했습니다."),
                ),
            };
            (format!("{p} {a} · {title}"), body.into())
        }
        _ => return None,
    };
    let relogin = e.kind == "needs_relogin"
        && account_rows(accounts).iter().any(|a| {
            Some(&a.raw.id) == e.account.as_ref()
                && a.raw.health.kind == "needs_relogin"
                && a.menu.show_relogin
        });
    Some(EventRow {
        kind: e.kind.clone(),
        seq: e.seq,
        day: e.ts.map(|t| clock.day_label(t)).unwrap_or_else(unknown),
        time: clock.time(e.ts),
        tag: tag.into(),
        tone,
        title: title
            .split('·')
            .map(|part| part.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        body,
        account: e.account.clone(),
        account_label: e.account.as_ref().map(|_| a),
        session: d.session.as_deref().and_then(hash),
        session_label: d.label.as_deref().map(folder),
        relogin,
        notable: e.notable,
        category,
        at: e.ts,
    })
}

// Credit deltas use decimal text, not binary floating point. Floor after subtracting.
fn decimal_spent(start: &serde_json::Number, stop: &serde_json::Number) -> Option<String> {
    fn parts(n: &serde_json::Number) -> Option<(i128, u32)> {
        let s = n.to_string();
        if s.contains(['e', 'E']) {
            return None;
        }
        let (a, b) = s.split_once('.').unwrap_or((&s, ""));
        let scale = u32::try_from(b.len()).ok()?;
        if scale > 18 {
            return None;
        }
        Some((format!("{a}{b}").parse().ok()?, scale))
    }
    let (a, sa) = parts(start)?;
    let (b, sb) = parts(stop)?;
    let scale = sa.max(sb);
    let factor = 10i128.checked_pow(scale)?;
    let delta = a
        .checked_mul(10i128.checked_pow(scale - sa)?)?
        .checked_sub(b.checked_mul(10i128.checked_pow(scale - sb)?)?)?;
    Some(v2::group_number(&(delta.max(0) / factor).to_string()))
}
pub fn outcome(r: &RequestSummary, accounts: &v2::View) -> String {
    if let Some(f) = &r.failover
        && matches!(r.outcome.as_str(), "ok" | "rate_hold_ok")
    {
        return crate::i18n::format(
            "성공 · {0}에서 {1}로 넘겨 처리",
            &[
                (account_name(accounts, &f.from)).to_string(),
                (account_name(accounts, &f.to)).to_string(),
            ],
        );
    }
    match r.outcome.as_str() {
        "ok" => crate::i18n::static_text("성공").into(),
        "rate_hold_ok" => crate::i18n::format(
            "성공 · 속도 제한 {0}번 기다림",
            &[(count(r.rate_retries)).to_string()],
        ),
        "waiting" => crate::i18n::format(
            "진행 중 · {0}초 기다린 뒤 보냄",
            &[(count(r.rate_wait_ms.map(|v| v / 1000))).to_string()],
        ),
        "inflight" => crate::i18n::static_text("진행 중").into(),
        "relayed_rate_429" | "relayed_rate429" => {
            crate::i18n::static_text("실패 · 속도 제한 오류를 그대로 전달").into()
        }
        "network_error" => crate::i18n::static_text("실패 · 상류에 연결하지 못함").into(),
        "upstream_error" => crate::i18n::format(
            "실패 · 상류 오류 {0}",
            &[(r.status.map(|n| n.to_string()).unwrap_or_else(unknown)).to_string()],
        ),
        "client_drop" => crate::i18n::static_text("클라이언트가 중단").into(),
        _ => unknown(),
    }
}
fn detail_view(d: &SessionDetail, accounts: &v2::View, clock: &Clock, expired: bool) -> DetailView {
    let s = &d.session;
    let row = session_row(s, accounts, clock, Some(&s.id));
    let account = account_rows(accounts)
        .into_iter()
        .find(|a| a.raw.id == s.account);
    let account_text = account
        .map(|a| format!("{} · {}", folder(&a.name), a.plan.as_deref().unwrap_or("—")))
        .unwrap_or_else(unknown);
    let remaining = account
        .map(|a| {
            let windows = account_windows(a, clock);
            let values = windows
                .iter()
                .map(|w| format!("{} {}%", w.label, w.remaining_pct))
                .collect::<Vec<_>>()
                .join(" · ");
            if a.state == v2::DisplayState::Relogin {
                if values.is_empty() {
                    crate::i18n::static_text("확인 불가 · 사용량 확인 전").into()
                } else {
                    let observed = observed_at(a, clock)
                        .map(|t| {
                            crate::i18n::format("{0} 기준", &[(clock.time(Some(t))).to_string()])
                        })
                        .unwrap_or_else(|| crate::i18n::static_text("기준 시각 확인 전").into());
                    crate::i18n::format(
                        "확인 불가 · {0} {1}",
                        &[observed.to_string(), values.to_string()],
                    )
                }
            } else if values.is_empty() {
                crate::i18n::static_text("사용량 확인 전").into()
            } else {
                values
            }
        })
        .unwrap_or_else(|| crate::i18n::static_text("사용량 확인 전").into());
    let observed_failures = d
        .recent_requests
        .iter()
        .filter(|r| {
            r.ts.is_some_and(|t| t >= clock.day_start && t < clock.day_end && t <= clock.utc())
                && matches!(
                    r.outcome.as_str(),
                    "relayed_rate_429" | "relayed_rate429" | "network_error" | "upstream_error"
                )
        })
        .count() as u64;
    let known_failures = s.failures_today.unwrap_or(0).max(observed_failures);
    let mut facts: Vec<(String, String)> = vec![
        (crate::i18n::static_text("계정").into(), account_text),
        (crate::i18n::static_text("남은 양").into(), remaining),
        (crate::i18n::static_text("모델").into(), row.model),
        (
            crate::i18n::static_text("시작").into(),
            format!(
                "{} ({})",
                row.start,
                s.first_seen_at
                    .map(|t| crate::i18n::format(
                        "{0}분째",
                        &[(clock.utc().signed_duration_since(t).num_minutes().max(0)).to_string()]
                    ))
                    .unwrap_or_else(unknown)
            ),
        ),
        (
            crate::i18n::static_text("마지막 활동").into(),
            clock.age(s.last_used_at),
        ),
        (
            crate::i18n::static_text("연결").into(),
            if !s.inflight_known {
                crate::i18n::static_text("— · 연결 정보 확인 전").into()
            } else if s.inflight.ws_open.unwrap_or(0) > 0 {
                crate::i18n::format(
                    "WebSocket {0}개 열림",
                    &[(count(s.inflight.ws_open)).to_string()],
                )
            } else {
                "HTTP".into()
            },
        ),
        (
            crate::i18n::static_text("하위 에이전트").into(),
            match s.subagents_today.filter(|_| s.subagents_known) {
                Some(0) => crate::i18n::static_text("없음").into(),
                Some(n) => crate::i18n::format("{0}개", &[n.to_string()]),
                None => unknown(),
            },
        ),
        (
            crate::i18n::static_text("오늘 요청").into(),
            if s.counters_complete && s.requests_today.is_some() {
                let failures = s
                    .failures_today
                    .map(|n| {
                        crate::i18n::format(
                            "{0}건",
                            &[(count(Some(n.max(observed_failures)))).to_string()],
                        )
                    })
                    .unwrap_or_else(|| {
                        if observed_failures > 0 {
                            crate::i18n::format("최소 {0}건", &[observed_failures.to_string()])
                        } else {
                            crate::i18n::static_text("—건").into()
                        }
                    });
                if s.failures_today.is_none() && observed_failures == 0 {
                    crate::i18n::format("{0}건", &[(count(s.requests_today)).to_string()])
                } else {
                    crate::i18n::format(
                        "{0}건 · 실패 {1}",
                        &[(count(s.requests_today)).to_string(), failures.to_string()],
                    )
                }
            } else if known_failures > 0 {
                crate::i18n::format("실패 최소 {0}건", &[known_failures.to_string()])
            } else {
                crate::i18n::static_text("— · 실패 — · 이전 실행 기록 확인 전").into()
            },
        ),
    ];
    facts.retain(|(key, _)| match key.as_str() {
        value if value == crate::i18n::static_text("하위 에이전트") => {
            s.subagents_known && s.subagents_today.is_some()
        }
        value if value == crate::i18n::static_text("오늘 요청") => {
            (s.counters_complete && s.requests_today.is_some()) || known_failures > 0
        }
        _ => true,
    });
    if s.truncated || s.metadata_truncated {
        facts.push((
            crate::i18n::static_text("기록 범위").into(),
            crate::i18n::static_text("버퍼 한도로 일부 기록만 표시합니다.").into(),
        ));
    }
    let active_wait = s
        .pacing_wait
        .as_ref()
        .filter(|p| p.waiting.unwrap_or(0) > 0);
    let mut wait_facts = Vec::new();
    if let Some(p) = active_wait {
        wait_facts.push(
            p.since
                .map(|t| {
                    crate::i18n::format(
                        "지금 {0}초째",
                        &[(clock.utc().signed_duration_since(t).num_seconds().max(0)).to_string()],
                    )
                })
                .unwrap_or_else(|| crate::i18n::static_text("지금 대기 중").into()),
        );
    }
    if s.counters_complete && s.rate_wait_today.count.unwrap_or(0) > 0 {
        if let Some(n) = s.rate_wait_today.count {
            wait_facts.push(crate::i18n::format(
                "오늘 {0}번",
                &[(count(Some(n))).to_string()],
            ));
        }
        if let Some(ms) = s.rate_wait_today.total_ms {
            wait_facts.push(crate::i18n::format(
                "합계 {0}초",
                &[(count(Some(ms / 1000))).to_string()],
            ));
        }
    }
    if !wait_facts.is_empty() {
        facts.push((
            crate::i18n::static_text("속도 조절 대기").into(),
            wait_facts.join(" · "),
        ));
    }
    facts.push((crate::i18n::static_text("이름 출처").into(), source(s)));
    if !d.recent_requests_known {
        facts.push((
            crate::i18n::static_text("최근 요청 범위").into(),
            crate::i18n::static_text("— · 이전 실행 또는 버퍼 한도로 일부 관측만 표시합니다.")
                .into(),
        ));
    }
    if !d.timeline_known {
        facts.push((
            crate::i18n::static_text("계정 이력 범위").into(),
            crate::i18n::static_text(
                "— · 이전 실행 또는 버퍼 한도로 전체 이력을 확인하지 못했습니다.",
            )
            .into(),
        ));
    }
    let mut placements: Vec<_> = d
        .timeline
        .iter()
        .filter(|t| {
            t.at.is_some_and(|v| v >= clock.day_start && v < clock.day_end)
        })
        .collect();
    placements.sort_by_key(|p| p.at);
    let timeline = placements
        .iter()
        .map(|p| {
            format!(
                "{} · {}{} · {} {}",
                clock.time(p.at),
                account_name(accounts, &p.account),
                if p.account == s.account {
                    crate::i18n::static_text(" (현재)")
                } else {
                    ""
                },
                if p.kind == "placed" && p.reason.as_deref() != Some("launch_model") {
                    crate::i18n::static_text("처음 배정")
                } else {
                    reason(p.reason.as_deref().unwrap_or(""))
                },
                cache_sentence(p.next_turn_cache.as_deref())
            )
        })
        .collect();
    let mut requests: Vec<_> = d.recent_requests.iter().collect();
    requests.sort_by_key(|r| std::cmp::Reverse(r.ts));
    requests.truncate(10);
    let requests = requests
        .into_iter()
        .map(|r| {
            crate::i18n::format(
                "{0} · {1} · 첫 응답 {2} · 걸린 시간 {3}",
                &[
                    (clock.stamp(r.ts)).to_string(),
                    (outcome(r, accounts)).to_string(),
                    (r.ttfb_ms
                        .map(|n| {
                            crate::i18n::format(
                                "{0}.{1}초",
                                &[(n / 1000).to_string(), format!("{:01}", n % 1000 / 100)],
                            )
                        })
                        .unwrap_or_else(unknown))
                    .to_string(),
                    (duration(r.dur_ms)).to_string(),
                ],
            )
        })
        .collect();
    DetailView {
        id: s.id.clone(),
        title: row.label,
        subtitle: crate::i18n::format(
            "{0} · 세션 {1}",
            &[
                ([
                    row.client,
                    s.client.version.as_deref().map(clean).unwrap_or_default(),
                ]
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" "))
                .to_string(),
                (s.id).to_string(),
            ],
        ),
        raw_prefix: d
            .original_short_id
            .as_deref()
            .map(|s| clean(s).chars().take(8).collect::<String>())
            .filter(|s| !s.is_empty()),
        status: if !s.inflight_known {
            crate::i18n::static_text("요청 상태 확인 전").into()
        } else if row.inflight {
            row.activity
        } else if s.active {
            crate::i18n::static_text("5분 안에 활동").into()
        } else {
            crate::i18n::static_text("쉬는 중").into()
        },
        status_tone: if row.inflight {
            v2::Tone::Use
        } else {
            v2::Tone::Off
        },
        account_status: format!(
            "{}({}){}",
            account_name(accounts, &s.account),
            v2::provider_label(&s.provider),
            if row.inflight {
                crate::i18n::static_text("에서 처리 중")
            } else {
                crate::i18n::static_text("에 배정됨")
            }
        ),
        facts,
        timeline,
        requests,
        expired,
    }
}
/// Usage alone rolls with the visible local minute; calendar-day filters stay separate.
pub fn usage_domain(clock: &Clock) -> (DateTime<Utc>, DateTime<Utc>) {
    use chrono::{Duration, Timelike};
    let minute = clock
        .now
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    let now = minute.with_timezone(&Utc);
    (now - Duration::hours(19), now + Duration::hours(5))
}

fn usage_projections(a: &v2::AccountRow, clock: &Clock) -> Vec<UsageForecastSeries> {
    use qb_core::usage_forecast::{ForecastBasis, ForecastOutcome};
    let Some(forecast) = a.raw.usage_forecast.as_ref() else {
        return vec![];
    };
    if a.raw.health.kind == "needs_relogin" || forecast.as_of > clock.utc() {
        return vec![];
    }
    let (_, edge) = usage_domain(clock);
    let start = (edge - chrono::Duration::hours(5)).max(forecast.as_of);
    forecast
        .windows
        .iter()
        .filter(|w| !w.name.starts_with("7d_") && !w.name.contains("model:"))
        .filter_map(|w| {
            if !w.used_pct.is_finite()
                || !(0.0..100.0).contains(&w.used_pct)
                || w.observed_at > forecast.as_of
                || w.awaiting_usage
                || matches!(
                    w.outcome,
                    ForecastOutcome::Measuring
                        | ForecastOutcome::ResetPending
                        | ForecastOutcome::Exhausted
                )
            {
                return None;
            }
            let rate = match w.basis? {
                ForecastBasis::Recent => w.recent_pct_per_hour?,
                ForecastBasis::Average => w.average_pct_per_hour?,
            };
            if !rate.is_finite() || rate < 0.0 {
                return None;
            }
            let end = w
                .reset_at
                .into_iter()
                .chain(w.exhaustion_at)
                .fold(edge, |a, b| a.min(b));
            if end <= start {
                return None;
            }
            // The DTO's value belongs to as_of. Advance with its selected rate,
            // preserving slope when a cached DTO is clipped by a later minute.
            let hours_since_as_of = |at: DateTime<Utc>| {
                at.signed_duration_since(forecast.as_of)
                    .num_microseconds()
                    .map(|micros| micros as f64 / 3_600_000_000.)
            };
            let remaining =
                (100.0 - w.used_pct - rate * hours_since_as_of(start)?).clamp(0.0, 100.0);
            if remaining <= 0.0 {
                return None;
            }
            let end_remaining =
                (100.0 - w.used_pct - rate * hours_since_as_of(end)?).clamp(0.0, 100.0);
            Some(UsageForecastSeries {
                name: v2::window_label(&w.name),
                points: [(start, remaining), (end, end_remaining)],
            })
        })
        .collect()
}

fn usage_tiles(snapshot: &DashboardSnapshot, clock: &Clock) -> Vec<UsageTile> {
    let (from, _) = usage_domain(clock);
    account_rows(&snapshot.accounts)
        .into_iter()
        .filter(|a| a.raw.user_state != "disabled")
        .map(|a| {
            let history = snapshot.usage.accounts.iter().find(|h| h.id == a.raw.id);
            let mut series = vec![];
            if let Some(h) = history {
                for w in h
                    .windows
                    .iter()
                    .filter(|w| !w.name.starts_with("7d_") && !w.name.contains("model:"))
                {
                    let mut normalized = BTreeMap::new();
                    for (t, p) in &w.points {
                        if *t >= from && *t <= clock.utc() {
                            normalized.insert(
                                *t,
                                p.filter(|n| n.is_finite() && *n >= 0.)
                                    .map(|used| (100. - used).clamp(0., 100.)),
                            );
                        }
                    }
                    let points: Vec<_> = normalized.into_iter().collect();
                    let remaining = a
                        .windows
                        .iter()
                        .find(|m| m.raw.name == w.name && m.account_scoped)
                        .and_then(|m| m.remaining);
                    let future_resets = a
                        .windows
                        .iter()
                        .filter(|m| m.raw.name == w.name)
                        .filter_map(|m| m.reset)
                        .filter(|t| *t > clock.utc())
                        .collect();
                    series.push(Series {
                        name: v2::window_label(&w.name),
                        points,
                        resets: w
                            .resets
                            .iter()
                            .copied()
                            .filter(|t| *t >= from && *t <= clock.utc())
                            .collect(),
                        future_resets,
                        remaining,
                    });
                }
            }
            for metric in a.windows.iter().filter(|w| w.account_scoped) {
                let name = v2::window_label(&metric.raw.name);
                if !series.iter().any(|s| s.name == name) {
                    series.push(Series {
                        name,
                        points: vec![],
                        resets: vec![],
                        future_resets: metric
                            .reset
                            .into_iter()
                            .filter(|t| *t > clock.utc())
                            .collect(),
                        remaining: metric.remaining,
                    });
                }
            }
            series.sort_by_key(|s| {
                a.windows
                    .iter()
                    .position(|w| w.account_scoped && w.label == s.name)
                    .unwrap_or(usize::MAX)
            });
            let credits_spans = history
                .map(|h| {
                    h.credits_spans
                        .iter()
                        .filter_map(|(a, b)| {
                            let start = (*a).max(from);
                            let end = b.unwrap_or(clock.utc()).min(clock.utc());
                            (start < end).then_some((start, end))
                        })
                        .collect()
                })
                .unwrap_or_default();
            UsageTile {
                id: a.raw.id.clone(),
                provider: a.raw.provider.clone(),
                name: folder(&a.name),
                plan: a.plan.clone().unwrap_or_else(unknown),
                chip: a.chip.as_ref().map(|c| c.text.clone()).unwrap_or_default(),
                tone: a.chip.as_ref().map_or(v2::Tone::Off, |c| c.tone),
                windows: account_windows(a, clock),
                forecast_lines: a.forecast_lines.clone(),
                forecast_tooltip: a.forecast_tooltip.clone(),
                forecast_warning: a.forecast_warning,
                projections: usage_projections(a, clock),
                last_observed: observed_at(a, clock)
                    .into_iter()
                    .chain(
                        series
                            .iter()
                            .flat_map(|s| &s.points)
                            .filter_map(|(t, p)| p.map(|_| *t)),
                    )
                    .max(),
                paused: a.raw.user_state == "paused",
                series,
                credits_spans,
                unconfirmed: a.raw.health.kind == "needs_relogin",
                truncated: snapshot.usage.truncated
                    || history
                        .is_some_and(|h| h.truncated || h.windows.iter().any(|w| w.truncated)),
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "dashboard_model_tests.rs"]
mod tests;

#[cfg(test)]
mod r14_move_cost_transport_tests {
    use super::*;
    #[test]
    fn r14_admin_transport_retains_cost_and_unknown_coverage() {
        let response: SessionsResponse = serde_json::from_value(serde_json::json!({
            "now":"2026-10-22T04:00:00Z", "sessions":[],
            "move_cost":{"hour":{"tokens":120,"known_count":1,"unknown_count":0,"recording_complete":true},
            "rolling24h":{"tokens":300,"known_count":2,"unknown_count":1,"recording_complete":true,
            "observed_since":"2026-10-22T00:00:00Z"},"clock_adjusted":false}
        })).unwrap();
        let lines = response.move_cost.unwrap().lines();
        assert!(lines[0].contains("1시간 120 토큰"));
        assert!(lines[0].contains("24시간 최소 300 토큰"));
        assert!(lines[1].contains("측정 2회 · 미측정 1회"));
        let mut unknown = MoveCost::default();
        unknown.rolling24h.unknown_count = 1;
        assert!(unknown.lines()[0].contains("미측정"));
        assert!(!unknown.lines()[0].contains("24시간 0 토큰"));
        let old: SessionsResponse = serde_json::from_str("{}").unwrap();
        assert!(old.move_cost.is_none());
    }
}

#[cfg(test)]
mod r16_localization_tests {
    use super::*;
    use crate::i18n::{UiLanguage, with_language};

    #[test]
    fn english_clock_and_duration_keep_numeric_meaning() {
        let now = DateTime::parse_from_rfc3339("2026-10-23T12:00:00+09:00").unwrap();
        let clock = Clock {
            now,
            day_start: now.with_timezone(&Utc),
            day_end: now.with_timezone(&Utc),
        };
        with_language(UiLanguage::En, || {
            assert_eq!(
                clock.age(Some(clock.utc() - chrono::Duration::minutes(2))),
                "2 minutes ago"
            );
            assert_eq!(
                clock.time(Some(clock.utc() + chrono::Duration::days(1))),
                "Oct 24 12:00"
            );
            assert_eq!(duration(Some(125000)), "2m 5s");
            assert_eq!(duration(Some(9_300_000)), "2h 35m");
            assert!(clock.day_label(clock.utc()).contains("Fri"));
        });
        assert_eq!(
            clock.age(Some(clock.utc() - chrono::Duration::minutes(2))),
            "2분 전"
        );
    }

    #[test]
    fn snapshot_wire_serialization_preserves_aliases_and_skips_enrichment() {
        let mut activity: ActivityResponse = serde_json::from_value(serde_json::json!({
            "events":[{"seq":7,"kind":"session_move","data":{"active":12,"recovered":true}}],
            "next_seq":8,"truncated":true
        }))
        .unwrap();
        activity.events[0].data.label = Some("private presentation enrichment".into());
        activity.events[0].data.no_eligible = true;
        let wire = serde_json::to_value(&activity).unwrap();
        assert_eq!(wire["events"][0]["data"]["active"], 12);
        assert_eq!(wire["events"][0]["data"]["recovered"], true);
        assert!(wire["events"][0]["data"].get("active_sessions").is_none());
        assert!(wire["events"][0]["data"].get("label").is_none());
        assert!(wire["events"][0]["data"].get("no_eligible").is_none());
        let restored: ActivityResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(restored.events[0].data.active_sessions, Some(12));
        assert!(restored.events[0].data.after_failure);
        assert!(restored.truncated);
        let sessions: SessionsResponse = serde_json::from_value(serde_json::json!({
            "sessions":[{"id":"s","inflight_known":false,"moves_known":true}],
            "move_cost":{"hour":{"tokens":123,"known_count":1,"unknown_count":2},"rolling24h":{"unknown_count":3},"clock_adjusted":true}
        })).unwrap();
        let restored: SessionsResponse =
            serde_json::from_slice(&serde_json::to_vec(&sessions).unwrap()).unwrap();
        assert!(!restored.sessions[0].inflight_known);
        assert!(restored.sessions[0].moves_known);
        assert_eq!(restored.move_cost.unwrap().hour.tokens, 123);
        let history: UsageHistory = serde_json::from_value(serde_json::json!({"accounts":[{"id":"a","windows":[{"name":"5h","points":[["2026-10-23T03:00:00Z",null]],"truncated":true}]}],"truncated":true})).unwrap();
        let restored: UsageHistory =
            serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
        assert_eq!(restored.accounts[0].windows[0].points[0].1, None);
        assert!(restored.truncated);
    }
}
