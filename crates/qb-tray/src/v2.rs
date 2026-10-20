//! Pure tray-v2 projection. No IO, placement policy, geometry, or persistent UI state.
//! `project` sorts for a fresh open; the caller owns order freeze and disclosure memory.
//! Unknown JSON fields are retained in raw structs, never interpolated into UI text.
use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{collections::BTreeMap, time::SystemTime};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: String,
    pub accounts: Vec<Account>,
    #[serde(deserialize_with = "nullable_pools")]
    pub pools: BTreeMap<String, Pool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Pool {
    pub eligible: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
fn nullable_pools<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Pool>, D::Error> {
    let pools = Option::<BTreeMap<String, Option<Pool>>>::deserialize(deserializer)?;
    Ok(pools
        .unwrap_or_default()
        .into_iter()
        .map(|(provider, pool)| (provider, pool.unwrap_or_default()))
        .collect())
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Account {
    pub id: String,
    pub provider: String,
    pub label: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub user_state: String,
    pub health: Health,
    pub windows: Vec<Window>,
    pub credits: Option<Credits>,
    /// Authoritative actual paid-serving signal, including explicit false.
    pub credits_in_use: Option<bool>,
    pub pool_eligible: Option<bool>,
    pub eligible_for_new_session: Option<bool>,
    pub quota_cooling: Option<QuotaCooling>,
    pub quota_credit_releasable: bool,
    pub observed_at: Option<String>,
    pub source: Source,
    pub pacing: Pacing,
    pub active_sessions: u64,
    pub bound_sessions: u64,
    pub allow_credits: bool,
    pub drain: bool,
    pub probe_pending: bool,
    pub refresh_attention: bool,
    pub token_expires_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Health {
    pub kind: String,
    pub reason: Option<String>,
    pub scope: Option<String>,
    pub until: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Window {
    pub name: String,
    pub scope: Option<String>,
    pub used_pct: Option<f64>,
    pub reset_at: Option<String>,
    pub observed_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Credits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<serde_json::Number>,
    /// Kept for raw compatibility; actual spending uses Account.credits_in_use only.
    pub serving: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QuotaCooling {
    pub window: String,
    pub reset_at: Option<String>,
    pub started_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Pacing {
    pub backoff_until: Option<String>,
    pub waiting: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
/// Accept the current tagged core object and incumbent legacy string.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Source {
    Object(SourceObject),
    Legacy(String),
    Other(Value),
}
impl Default for Source {
    fn default() -> Self {
        Self::Other(Value::Null)
    }
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SourceObject {
    pub kind: String,
    pub mode: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
impl Source {
    pub fn linked(&self) -> bool {
        matches!(self, Self::Object(s) if s.mode.as_deref() == Some("linked"))
            || matches!(self, Self::Legacy(s) if s == "linked")
    }
    pub fn owned_or_adopted(&self) -> bool {
        matches!(self, Self::Object(s) if matches!(s.kind.as_str(), "oauth" | "takeover") || s.mode.as_deref() == Some("adopted"))
            || matches!(self, Self::Legacy(s) if matches!(s.as_str(), "oauth" | "owned" | "adopted" | "takeover"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Use,
    Warn,
    Bad,
    Off,
    Ok,
    Secondary,
    Limit,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    InUse,
    NeedsAction,
    Ready,
    Waiting,
    Paused,
    Disabled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayState {
    Relogin,
    LinkStale,
    StoreError,
    Payment,
    Permission,
    Credits,
    Paused,
    Quota,
    Auth,
    InUse,
    Drain,
    Ready,
    Disabled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    NoAccounts,
    Off,
    Available,
    Credits,
    Unavailable,
    Unknown,
}
impl Verdict {
    pub fn text(self) -> &'static str {
        match self {
            Self::NoAccounts => "계정 없음",
            Self::Off => "꺼짐",
            Self::Available => "사용 가능",
            Self::Credits => "크레딧으로 사용 중",
            Self::Unavailable => "사용 불가",
            Self::Unknown => "상태 확인 전",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServingMode {
    Subscription,
    CreditsOnly,
    Unclassified,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Normal,
    Attention,
}
#[derive(Debug, Clone)]
pub struct View {
    pub raw: State,
    /// Empty when there are no accounts. Otherwise always Claude, Codex.
    pub summaries: Vec<Summary>,
    pub sections: Vec<Section>,
    pub empty: bool,
    pub note: Option<String>,
    pub tray_tooltip: String,
    pub badge: Badge,
}
#[derive(Debug, Clone)]
pub struct Summary {
    pub provider: String,
    pub provider_label: String,
    pub verdict: Verdict,
    pub serving_mode: ServingMode,
    pub tone: Tone,
    pub detail: String,
    pub todo: usize,
    pub eligible: Option<u64>,
    pub earliest_until: Option<DateTime<Utc>>,
    pub tooltip: String,
    pub accessible_name: String,
}
#[derive(Debug, Clone)]
pub struct Section {
    pub provider: String,
    pub label: String,
    pub rows: Vec<AccountRow>,
    pub disabled_rows: Vec<AccountRow>,
    pub auto_expand_recommended: bool,
    pub disclosure: Option<String>,
    pub expansion_hint: Option<String>,
    pub empty_message: Option<String>,
}
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub raw: Account,
    pub name: String,
    pub plan: Option<String>,
    pub plan_tooltip: Option<String>,
    pub name_tooltip: Option<String>,
    pub state: DisplayState,
    pub group: Group,
    pub chip: Option<Chip>,
    /// None for disabled/relogin; Some with no binding means unknown usage.
    pub usage: Option<Usage>,
    pub windows: Vec<WindowMetric>,
    pub credits: Option<CreditsLine>,
    pub reason: Option<String>,
    /// The only inline action; menu controls are not row buttons.
    pub action: Option<RowAction>,
    pub menu: MenuInfo,
    pub chip_tooltip: Option<String>,
    pub accessible_name: String,
}
#[derive(Debug, Clone)]
pub struct Chip {
    pub text: String,
    pub tone: Tone,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarTone {
    Ok,
    Low,
    Critical,
    Paused,
    Limit,
}
#[derive(Debug, Clone)]
pub struct WindowMetric {
    pub raw: Window,
    pub label: String,
    pub account_scoped: bool,
    pub used: Option<u8>,
    pub remaining: Option<u8>,
    pub reset: Option<DateTime<Utc>>,
    pub reset_text: Option<String>,
    pub model_limited: bool,
}
#[derive(Debug, Clone)]
pub struct Usage {
    pub binding: Option<WindowMetric>,
    pub lines: Vec<UsageLine>,
    pub text: String,
    pub tooltip: String,
}
#[derive(Debug, Clone)]
pub struct UsageLine {
    pub window: WindowMetric,
    pub bar_tone: BarTone,
}
#[derive(Debug, Clone)]
pub struct CreditsLine {
    pub text: String,
    pub amount: String,
    pub exact_amount: String,
    pub tone: Tone,
    pub actually_serving: bool,
    pub tooltip: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Relogin,
    OpenLogs,
    Enable,
    AllowCredits,
    DisableCredits,
}
#[derive(Debug, Clone)]
pub struct RowAction {
    pub kind: ActionKind,
    pub label: &'static str,
    pub accent: bool,
    pub confirmation: Option<Confirmation>,
}
#[derive(Debug, Clone)]
pub struct Confirmation {
    pub title: &'static str,
    pub body: String,
    pub confirm: &'static str,
    pub cancel: &'static str,
}
#[derive(Debug, Clone)]
pub struct MenuInfo {
    pub full_name: String,
    pub identity_line: String,
    pub refresh_warning: Option<String>,
    pub window_lines: Vec<String>,
    pub model_cooling_line: Option<String>,
    pub credits_line: Option<String>,
    pub sessions_line: String,
    pub show_refresh_usage: bool,
    pub show_relogin: bool,
    pub show_refresh_token: bool,
    /// Always use this guard for a menu off->on transition too.
    pub credits_enable_confirmation: Option<Confirmation>,
}

pub fn project(json: &str, now: SystemTime) -> serde_json::Result<View> {
    let raw: State = serde_json::from_str(json)?;
    let now = DateTime::<Utc>::from(now);
    let mut sections = Vec::new();
    let mut summaries = Vec::new();
    for provider in ["claude", "codex"] {
        let accounts: Vec<_> = raw
            .accounts
            .iter()
            .filter(|a| a.provider == provider)
            .collect();
        let mut rows: Vec<_> = accounts
            .iter()
            .map(|a| project_row(a, account_name(a, &accounts), now))
            .collect();
        let summary = summarize(
            provider,
            &rows,
            raw.pools.get(provider).and_then(|p| p.eligible),
            now,
        );
        let expand = matches!(summary.verdict, Verdict::Unavailable | Verdict::Off);
        let serving_name = rows
            .iter()
            .filter(|r| r.raw.active_sessions > 0 && r.raw.user_state != "disabled")
            .max_by_key(|r| r.raw.active_sessions)
            .map(|r| r.name.clone());
        for row in &mut rows {
            finish_row(row, &summary, serving_name.as_deref(), now);
        }
        rows.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then_with(|| {
                    if a.group == Group::Waiting {
                        a.raw
                            .health
                            .until
                            .as_deref()
                            .and_then(timestamp)
                            .map_or((true, None), |t| (false, Some(t)))
                            .cmp(
                                &b.raw
                                    .health
                                    .until
                                    .as_deref()
                                    .and_then(timestamp)
                                    .map_or((true, None), |t| (false, Some(t))),
                            )
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.raw.id.cmp(&b.raw.id))
        });
        let (disabled_rows, rows): (Vec<_>, Vec<_>) =
            rows.into_iter().partition(|r| r.group == Group::Disabled);
        let disabled_count = disabled_rows.len();
        sections.push(Section {
            provider: provider.into(),
            label: provider_label(provider).into(),
            empty_message: accounts
                .is_empty()
                .then(|| format!("{} 계정이 없습니다", provider_label(provider))),
            rows,
            disabled_rows,
            auto_expand_recommended: expand && disabled_count > 0,
            disclosure: (disabled_count > 0).then(|| format!("꺼진 계정 {disabled_count}개")),
            expansion_hint: (expand && disabled_count > 0)
                .then(|| "켜면 사용량을 확인한 뒤 바로 이어서 처리합니다.".into()),
        });
        summaries.push(summary);
    }
    let empty = raw.accounts.is_empty();
    let blocked = summaries.iter().any(|s| s.verdict == Verdict::Unavailable);
    let badge = if blocked || summaries.iter().any(|s| s.todo > 0) {
        Badge::Attention
    } else {
        Badge::Normal
    };
    let tooltip = format!(
        "quota-baton{}",
        summaries
            .iter()
            .map(|s| {
                let reset = if s.verdict == Verdict::Unavailable {
                    s.earliest_until
                        .map(|t| format!("({})", local_time(t, now, false)))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                format!(" · {} {}{reset}", s.provider_label, s.verdict.text())
            })
            .collect::<String>()
    );
    Ok(View {
        raw,
        summaries: if empty { vec![] } else { summaries },
        sections: if empty { vec![] } else { sections },
        empty,
        note: blocked.then(|| "사용 불가인 동안 새 요청은 오류로 끝납니다.".into()),
        tray_tooltip: truncate_utf16(&tooltip, 127),
        badge,
    })
}

pub fn provider_label(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        _ => provider,
    }
}
pub fn plan_label(provider: &str, plan: Option<&str>) -> Option<&'static str> {
    match (provider, plan?) {
        ("claude", "pro") | ("codex", "pro") => Some("Pro"),
        ("claude", "max") => Some("Max"),
        ("claude", "max_5x") => Some("Max 5x"),
        ("claude", "max_20x") => Some("Max 20x"),
        ("codex", "free") => Some("Free"),
        ("codex", "plus") => Some("Plus"),
        ("codex", "team") => Some("Team"),
        ("codex", "business") => Some("Business"),
        ("codex", "enterprise") => Some("Enterprise"),
        _ => None,
    }
}
fn nonempty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.trim().is_empty())
}
fn account_name(a: &Account, accounts: &[&Account]) -> String {
    if let Some(label) = nonempty(a.label.as_deref()).filter(|label| {
        // Older imports stored these exact generated defaults as labels.
        // Any other value, including a real organisation name, is user-visible.
        !a.email
            .as_deref()
            .is_some_and(|email| *label == email || *label == format!("{email}'s Organization"))
    }) {
        return clean(label);
    }
    let Some(email) = nonempty(a.email.as_deref()) else {
        return "이름 없는 계정".into();
    };
    let local = email.split('@').next().unwrap_or(email);
    let collisions = accounts
        .iter()
        .filter(|b| {
            nonempty(b.email.as_deref()).is_some_and(|s| s.split('@').next() == Some(local))
        })
        .count();
    clean(if collisions > 1 { email } else { local })
}
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}
fn account_cooling(a: &Account) -> bool {
    a.health.kind == "cooling" && a.health.scope.as_deref() == Some("account")
}
fn quota(a: &Account) -> bool {
    account_cooling(a) && a.health.reason.as_deref() == Some("quota")
}
pub fn credits_eligible_for_resume(a: &Account) -> bool {
    a.provider == "codex"
        && a.user_state == "active"
        && !a.allow_credits
        && !a.drain
        && !a.probe_pending
        && !a.refresh_attention
        && a.extra.get("takeover_pending") != Some(&Value::Bool(true))
        && quota(a)
        && a.quota_credit_releasable
        && a.credits.as_ref().is_some_and(|c| {
            qb_core::credits::available(
                c.has_credits,
                c.unlimited,
                c.balance.as_ref().and_then(|n| n.as_f64()),
            )
        })
}

fn actual_credits(a: &Account) -> bool {
    a.provider == "codex" && a.credits_in_use == Some(true)
}
fn display_state(a: &Account) -> DisplayState {
    if a.user_state == "disabled" {
        return DisplayState::Disabled;
    }
    if a.health.kind == "needs_relogin" {
        return DisplayState::Relogin;
    }
    if account_cooling(a) {
        match a.health.reason.as_deref() {
            Some("link_stale") => return DisplayState::LinkStale,
            Some("store_error") => return DisplayState::StoreError,
            Some("payment") => return DisplayState::Payment,
            Some("permission") => return DisplayState::Permission,
            _ => {}
        }
    }
    if actual_credits(a) {
        DisplayState::Credits
    } else if a.user_state == "paused" {
        DisplayState::Paused
    } else if quota(a) {
        DisplayState::Quota
    } else if account_cooling(a) && a.health.reason.as_deref() == Some("auth") {
        DisplayState::Auth
    } else if a.active_sessions > 0 {
        DisplayState::InUse
    } else if a.drain {
        DisplayState::Drain
    } else {
        DisplayState::Ready
    }
}
fn state_style(s: DisplayState) -> (Group, Option<Chip>) {
    use DisplayState::*;
    let (group, text, tone) = match s {
        Relogin => (Group::NeedsAction, "다시 로그인 필요", Tone::Bad),
        LinkStale => (Group::NeedsAction, "CPA 갱신 멈춤", Tone::Bad),
        StoreError => (Group::NeedsAction, "저장 오류", Tone::Bad),
        Payment => (Group::NeedsAction, "결제 거절", Tone::Bad),
        Permission => (Group::NeedsAction, "접근 거부", Tone::Bad),
        Credits => (Group::InUse, "크레딧 사용 중", Tone::Warn),
        Paused => (Group::Paused, "일시정지", Tone::Off),
        Quota => (Group::Waiting, "한도 도달", Tone::Limit),
        Auth => (Group::Waiting, "인증 확인 대기", Tone::Off),
        InUse => (Group::InUse, "사용 중", Tone::Use),
        Drain => (Group::Ready, "새 대화 안 받음", Tone::Off),
        Ready => return (Group::Ready, None),
        Disabled => return (Group::Disabled, None),
    };
    (
        group,
        Some(Chip {
            text: text.into(),
            tone,
        }),
    )
}

/// Unknown names are not assumed account-wide. Explicit model scope always excludes.
pub fn account_window(w: &Window) -> bool {
    if w.scope.as_deref().is_some_and(|s| s != "account") {
        return false;
    }
    if w.name.starts_with("7d_") || w.name.starts_with("seven_day_") {
        return false;
    }
    w.scope.as_deref() == Some("account")
        || matches!(w.name.as_str(), "5h" | "7d" | "primary" | "secondary")
        || w.name
            .strip_suffix('m')
            .is_some_and(|n| n.parse::<u64>().is_ok_and(|n| n > 0))
}
pub fn window_label(name: &str) -> String {
    match name {
        "5h" | "primary" => "5시간".into(),
        "7d" | "secondary" => "주간".into(),
        "7d_oi" => "모델 주간 (7d_oi)".into(),
        "seven_day_opus" => "Opus 주간".into(),
        "seven_day_sonnet" => "Sonnet 주간".into(),
        _ => {
            if let Some(n) = name.strip_suffix('m').and_then(|n| n.parse::<u64>().ok()) {
                if n > 0 && n % 1440 == 0 {
                    format!("{}일", n / 1440)
                } else {
                    format!("{n}분")
                }
            } else {
                clean(name)
            }
        }
    }
}
fn metric(w: &Window, a: &Account, now: DateTime<Utc>) -> WindowMetric {
    let used = w
        .used_pct
        .filter(|n| n.is_finite())
        .map(|n| n.round().clamp(0., 100.) as u8);
    let reset = if quota(a) {
        a.quota_cooling
            .as_ref()
            .filter(|q| q.window == w.name)
            .and_then(|q| q.reset_at.as_deref())
            .and_then(timestamp)
            .or_else(|| w.reset_at.as_deref().and_then(timestamp))
    } else {
        w.reset_at.as_deref().and_then(timestamp)
    };
    let scoped = account_window(w);
    let scope = a.health.scope.as_deref().unwrap_or("");
    WindowMetric {
        raw: w.clone(),
        label: window_label(&w.name),
        account_scoped: scoped,
        used,
        remaining: used.map(|n| 100 - n),
        reset,
        reset_text: reset.map(|t| reset_label(t, now)),
        model_limited: !scoped
            && a.health.kind == "cooling"
            && scope != "account"
            && (scope == w.name || scope.strip_prefix("model:") == Some(w.name.as_str())),
    }
}
fn window_minutes(name: &str) -> Option<u64> {
    match name {
        "5h" | "primary" => Some(300),
        "7d" | "secondary" => Some(10_080),
        _ => name
            .strip_suffix('m')?
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0),
    }
}
fn bar_tone(a: &Account, remaining: Option<u8>) -> BarTone {
    if a.user_state == "paused" {
        BarTone::Paused
    } else {
        match remaining {
            Some(0) => BarTone::Limit,
            Some(30..=100) | None => BarTone::Ok,
            Some(10..=29) => BarTone::Low,
            Some(_) => BarTone::Critical,
        }
    }
}
fn binding(a: &Account, windows: &[WindowMetric], now: DateTime<Utc>) -> Option<WindowMetric> {
    // C5 metadata is authoritative even when the corresponding snapshot meter is absent.
    if quota(a)
        && let Some(q) = &a.quota_cooling
    {
        let w = windows
            .iter()
            .find(|w| w.raw.name == q.window)
            .map(|w| w.raw.clone())
            .unwrap_or_else(|| Window {
                name: q.window.clone(),
                ..Window::default()
            });
        if account_window(&w) {
            let mut result = metric(&w, a, now);
            if let Some(reset) = q.reset_at.as_deref().and_then(timestamp) {
                result.reset = Some(reset);
                result.reset_text = Some(reset_label(reset, now));
            }
            return Some(result);
        }
        return None; // Never replace an authoritative model-scoped limit with an account window.
    }
    windows
        .iter()
        .filter(|w| w.account_scoped && w.used.is_some())
        .max_by(|a, b| {
            a.raw
                .used_pct
                .partial_cmp(&b.raw.used_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.reset.cmp(&b.reset))
                .then_with(|| b.raw.name.cmp(&a.raw.name))
        })
        .cloned()
}
fn project_row(a: &Account, name: String, now: DateTime<Utc>) -> AccountRow {
    let state = display_state(a);
    let (group, chip) = state_style(state);
    let mut windows: Vec<_> = a.windows.iter().map(|w| metric(w, a, now)).collect();
    windows.sort_by_key(|w| {
        (
            !w.account_scoped,
            window_minutes(&w.raw.name).unwrap_or(u64::MAX),
            w.raw.name.clone(),
        )
    });
    let usage = if matches!(state, DisplayState::Disabled | DisplayState::Relogin) {
        None
    } else {
        let chosen = binding(a, &windows, now);
        let text = "사용량 확인 전".into();
        let lines = windows
            .iter()
            .filter(|w| w.account_scoped && w.used.is_some())
            .cloned()
            .map(|window| UsageLine {
                bar_tone: bar_tone(a, window.remaining),
                window,
            })
            .collect();
        let tooltip = usage_tooltip(chosen.as_ref(), &windows, a.observed_at.as_deref(), now);
        Some(Usage {
            binding: chosen,
            lines,
            text,
            tooltip,
        })
    };
    let credits = if state != DisplayState::Disabled {
        credits_line(a)
    } else {
        None
    };
    let plan = plan_label(&a.provider, a.plan.as_deref()).map(str::to_owned);
    let menu = menu_info(a, &name, plan.as_deref(), &windows, credits.as_ref(), now);
    AccountRow {
        raw: a.clone(),
        plan_tooltip: plan.as_ref().map(|p| format!("요금제: {p}")),
        name_tooltip: nonempty(a.email.as_deref()).map(clean),
        name,
        plan,
        state,
        group,
        chip,
        usage,
        windows,
        credits,
        reason: None,
        action: None,
        menu,
        chip_tooltip: None,
        accessible_name: String::new(),
    }
}

fn summarize(
    provider: &str,
    rows: &[AccountRow],
    eligible: Option<u64>,
    now: DateTime<Utc>,
) -> Summary {
    let active: Vec<_> = rows
        .iter()
        .filter(|r| r.raw.user_state == "active")
        .collect();
    let members: Vec<_> = rows
        .iter()
        .filter(|r| r.raw.pool_eligible == Some(true))
        .collect();
    let complete = eligible.is_some_and(|n| n > 0 && members.len() as u64 == n)
        && rows.iter().all(|r| r.raw.pool_eligible.is_some());
    let would_use = |r: &&AccountRow| {
        actual_credits(&r.raw)
            || (r.raw.provider == "codex"
                && r.raw.allow_credits
                && (quota(&r.raw)
                    || r.windows
                        .iter()
                        .any(|w| w.account_scoped && w.raw.used_pct.is_some_and(|u| u >= 100.))))
    };
    let serving_mode = if complete && members.iter().all(would_use) {
        ServingMode::CreditsOnly
    } else if complete {
        ServingMode::Subscription
    } else {
        ServingMode::Unclassified
    };
    let verdict = if rows.is_empty() {
        Verdict::NoAccounts
    } else if active.is_empty() {
        Verdict::Off
    } else {
        match eligible {
            Some(0) => Verdict::Unavailable,
            Some(_) if serving_mode == ServingMode::CreditsOnly => Verdict::Credits,
            Some(_) => Verdict::Available,
            None => Verdict::Unknown,
        }
    };
    let earliest_until = active
        .iter()
        .filter(|r| account_cooling(&r.raw))
        .filter_map(|r| r.raw.health.until.as_deref().and_then(timestamp))
        .min();
    let earliest_quota = members
        .iter()
        .flat_map(|r| {
            r.windows
                .iter()
                .filter(|w| {
                    w.account_scoped
                        && w.raw
                            .used_pct
                            .is_some_and(|u| u >= 100. || (u >= 99. && actual_credits(&r.raw)))
                })
                .filter_map(|w| w.reset)
                .chain(
                    r.raw
                        .quota_cooling
                        .as_ref()
                        .and_then(|q| q.reset_at.as_deref())
                        .and_then(timestamp),
                )
        })
        .min();
    let mut in_use: Vec<_> = rows
        .iter()
        .filter(|r| r.raw.active_sessions > 0 && r.raw.user_state != "disabled")
        .collect();
    in_use.sort_by(|a, b| {
        b.raw
            .active_sessions
            .cmp(&a.raw.active_sessions)
            .then(a.name.cmp(&b.name))
    });
    let detail = match verdict {
        Verdict::NoAccounts => String::new(),
        Verdict::Off => "켜진 계정 없음".into(),
        Verdict::Unknown => "계정 풀 정보 확인 전".into(),
        Verdict::Available => match in_use.first() {
            Some(r) if in_use.len() == 1 => format!("{} 사용 중", r.name),
            Some(r) => format!("{} 외 {}개 사용 중", r.name, in_use.len() - 1),
            None => format!("계정 {}개 준비됨", eligible.unwrap_or(0)),
        },
        Verdict::Credits => earliest_quota
            .map(|t| format!("{} 한도 초기화", local_time(t, now, false)))
            .unwrap_or_else(|| "한도 초기화 시간 확인 전".into()),
        Verdict::Unavailable => earliest_until
            .map(|t| format!("{}부터 사용 가능", local_time(t, now, false)))
            .unwrap_or_else(|| {
                if rows.iter().any(|r| r.state == DisplayState::Relogin) {
                    "다시 로그인 필요"
                } else {
                    "사용할 수 있는 계정 없음"
                }
                .into()
            }),
    };
    let todo = rows
        .iter()
        .filter(|r| r.group == Group::NeedsAction)
        .count();
    let provider_name = provider_label(provider);
    let mut tooltip = format!(
        "{provider_name} 계정 {}개 · 켜짐 {}개 · 지금 사용 가능 {}.",
        rows.len(),
        active.len(),
        eligible
            .map(|n| format!("{n}개"))
            .unwrap_or_else(|| "확인 전".into())
    );
    if verdict == Verdict::Unavailable {
        if let Some(t) = earliest_until {
            let name = active
                .iter()
                .find(|r| r.raw.health.until.as_deref().and_then(timestamp) == Some(t))
                .map(|r| r.name.as_str())
                .unwrap_or("");
            tooltip.push_str(&format!(
                " 가장 빠른 초기화: {name} {}.",
                local_time(t, now, true)
            ));
        }
        tooltip.push_str(" 그때까지 새 요청은 오류로 끝납니다.");
    } else if let Some(r) = in_use.first() {
        tooltip.push_str(&format!(
            " {}가 대화 {}개를 처리 중입니다.",
            r.name, r.raw.active_sessions
        ));
    }
    let accessible_name = format!(
        "{provider_name} {}{}{}",
        verdict.text(),
        if detail.is_empty() {
            String::new()
        } else {
            format!(" · {detail}")
        },
        if todo > 0 {
            format!(" · 할 일 {todo}")
        } else {
            String::new()
        }
    );
    Summary {
        provider: provider.into(),
        provider_label: provider_name.into(),
        verdict,
        serving_mode,
        tone: match verdict {
            Verdict::Available => Tone::Ok,
            Verdict::Credits => Tone::Warn,
            Verdict::Unavailable => Tone::Bad,
            _ => Tone::Off,
        },
        detail,
        todo,
        eligible,
        earliest_until,
        tooltip,
        accessible_name,
    }
}

fn finish_row(row: &mut AccountRow, summary: &Summary, serving: Option<&str>, now: DateTime<Utc>) {
    let a = &row.raw;
    let until = a
        .health
        .until
        .as_deref()
        .and_then(timestamp)
        .map(|t| local_time(t, now, false));
    use DisplayState::*;
    row.reason = match row.state {
        Relogin => Some(
            if matches!(summary.verdict, Verdict::Available | Verdict::Credits) {
                serving
                    .map(|n| format!("그동안 {n}가 처리합니다."))
                    .unwrap_or_else(|| "그동안 다른 계정이 처리합니다.".into())
            } else {
                "토큰이 만료되어 이 계정을 쓸 수 없습니다.".into()
            },
        ),
        LinkStale => Some("CPA가 토큰을 갱신하지 않습니다. CPA 실행을 확인하세요.".into()),
        StoreError => Some("토큰을 저장하지 못해 다시 시도하는 중입니다.".into()),
        Payment => Some(
            until
                .as_ref()
                .map(|t| format!("결제가 거절되어 {t}까지 쉽니다."))
                .unwrap_or_else(|| "결제가 거절되어 이 계정을 쉽니다.".into()),
        ),
        Permission => Some(
            until
                .as_ref()
                .map(|t| format!("조직 접근이 거부되어 {t}까지 쉽니다."))
                .unwrap_or_else(|| "조직 접근이 거부되어 이 계정을 쉽니다.".into()),
        ),
        Auth => Some(
            until
                .as_ref()
                .map(|t| format!("인증을 {t}에 다시 확인합니다."))
                .unwrap_or_else(|| "인증을 다시 확인합니다.".into()),
        ),
        _ => None,
    };
    if row.reason.is_none()
        && !matches!(row.state, Disabled | Paused)
        && row.group != Group::NeedsAction
    {
        let seconds = a
            .pacing
            .backoff_until
            .as_deref()
            .and_then(timestamp)
            .map(|t| (t - now).num_seconds().max(0) as u64)
            .unwrap_or(0);
        if seconds > 0 || a.pacing.waiting > 0 {
            row.reason = Some(if seconds > 0 {
                format!("요청 속도 조절 중 · {seconds}초")
            } else {
                "요청 속도 조절 중".into()
            });
        }
    }
    row.action = match row.state {
        Relogin if !a.source.linked() => Some(action(ActionKind::Relogin, "다시 로그인", true)),
        StoreError => Some(action(ActionKind::OpenLogs, "로그 열기", false)),
        Disabled => Some(action(
            ActionKind::Enable,
            "켜기",
            matches!(summary.verdict, Verdict::Unavailable | Verdict::Off),
        )),
        _ if row.group != Group::NeedsAction
            && row.credits.as_ref().is_some_and(|c| c.actually_serving) =>
        {
            Some(action(
                ActionKind::DisableCredits,
                "크레딧 사용 끄기",
                false,
            ))
        }
        _ if row.group != Group::NeedsAction
            && credits_eligible_for_resume(a)
            && summary.verdict == Verdict::Unavailable =>
        {
            let mut result = action(ActionKind::AllowCredits, "크레딧으로 계속", false);
            result.confirmation = row.menu.credits_enable_confirmation.clone();
            Some(result)
        }
        _ => None,
    };
    row.chip_tooltip = match row.state {
        InUse => Some(format!(
            "이 계정에서 최근 5분 안에 요청한 대화 {}개 · 연결된 대화 {}개",
            a.active_sessions, a.bound_sessions
        )),
        Quota => Some(
            row.usage
                .as_ref()
                .and_then(|u| u.binding.as_ref())
                .map(|w| {
                    format!(
                        "{} 한도에 도달했습니다. {}에 초기화되면 다시 사용합니다.",
                        w.label,
                        w.reset
                            .map(|t| local_time(t, now, true))
                            .unwrap_or_else(|| "확인된 시간".into())
                    )
                })
                .unwrap_or_else(|| "한도에 도달했습니다. 초기화 시간을 확인하고 있습니다.".into()),
        ),
        Credits => Some("구독 한도에 도달해 유료 크레딧으로 처리하고 있습니다.".into()),
        Paused => Some("새 대화를 이 계정에 배정하지 않습니다. 사용량은 계속 확인합니다.".into()),
        Drain => Some("이미 연결된 대화만 계속 처리합니다.".into()),
        _ => None,
    };
    let usage_accessibility = row.usage.as_ref().map(|usage| {
        if usage.lines.is_empty() {
            usage.text.clone()
        } else {
            usage
                .lines
                .iter()
                .map(|line| {
                    format!(
                        "{} {}% 남음{}",
                        line.window.label,
                        line.window.remaining.unwrap_or_default(),
                        line.window
                            .reset_text
                            .as_ref()
                            .map(|reset| format!(", {reset}"))
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
    });
    row.accessible_name = [
        Some(row.name.clone()),
        row.plan.clone(),
        row.chip.as_ref().map(|c| c.text.clone()),
        usage_accessibility,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
}
fn action(kind: ActionKind, label: &'static str, accent: bool) -> RowAction {
    RowAction {
        kind,
        label,
        accent,
        confirmation: None,
    }
}

fn credits_line(a: &Account) -> Option<CreditsLine> {
    if a.provider != "codex" {
        return None;
    }
    let c = a.credits.as_ref()?;
    if !qb_core::credits::available(
        c.has_credits,
        c.unlimited,
        c.balance.as_ref().and_then(|n| n.as_f64()),
    ) {
        return None;
    }
    let amount = credit_amount(c, false);
    let exact_amount = credit_amount(c, true);
    let actually_serving = actual_credits(a);
    Some(CreditsLine {
        text: format!(
            "크레딧 {amount} · {}",
            if actually_serving {
                "지금 사용 중"
            } else if a.allow_credits {
                "한도 도달 시 사용"
            } else {
                "한도 도달 시 사용 안 함"
            }
        ),
        amount,
        exact_amount: exact_amount.clone(),
        tone: if actually_serving {
            Tone::Warn
        } else {
            Tone::Secondary
        },
        actually_serving,
        tooltip: format!(
            "크레딧 잔액 {exact_amount} · 유료 크레딧 사용 허용: {}. 켜면 구독 한도에 도달해도 크레딧으로 계속 처리합니다.",
            if a.allow_credits { "켜짐" } else { "꺼짐" }
        ),
    })
}
/// Group the JSON number's decimal representation, without rounding it to cents.
fn credit_amount(c: &Credits, exact: bool) -> String {
    if c.unlimited == Some(true) {
        return "무제한".into();
    }
    if c.has_credits == Some(false) {
        return "없음".into();
    }
    let Some(balance) = &c.balance else {
        return match c.has_credits {
            Some(true) => "있음",
            Some(false) => "없음",
            None => "확인 전",
        }
        .into();
    };
    let text = balance.to_string();
    if exact {
        group_number(&text)
    } else {
        // Avoid float conversion for the normal decimal form (large integer balances).
        if !text.contains(['e', 'E']) && !text.starts_with('-') {
            group_number(text.split('.').next().unwrap_or(&text))
        } else {
            balance
                .as_f64()
                .filter(|n| n.is_finite())
                .map(|n| group_number(&format!("{:.0}", n.floor())))
                .unwrap_or_else(|| "있음".into())
        }
    }
}
pub fn group_number(text: &str) -> String {
    let (integer, fraction) = text
        .split_once('.')
        .map_or((text, None), |(a, b)| (a, Some(b)));
    if integer.contains(['e', 'E']) || fraction.is_some_and(|s| s.contains(['e', 'E'])) {
        return text.into();
    }
    let (sign, digits) = integer
        .strip_prefix('-')
        .map_or(("", integer), |s| ("-", s));
    let mut result = sign.to_owned();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    if let Some(fraction) = fraction {
        result.push('.');
        result.push_str(fraction);
    }
    result
}
fn menu_info(
    a: &Account,
    name: &str,
    plan: Option<&str>,
    windows: &[WindowMetric],
    credits: Option<&CreditsLine>,
    now: DateTime<Utc>,
) -> MenuInfo {
    let source = if a.source.linked() {
        "CPA가 토큰 갱신"
    } else if a.source.owned_or_adopted() {
        "토큰 직접 갱신"
    } else {
        "토큰 갱신 방식 확인 전"
    };
    let identity_line = [Some(provider_label(&a.provider)), plan, Some(source)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let refresh_warning = a.refresh_attention.then(|| {
        a.token_expires_at
            .as_deref()
            .and_then(timestamp)
            .map(|t| format!("토큰 갱신 확인 필요 · {} 만료", local_time(t, now, false)))
            .unwrap_or_else(|| "토큰 갱신 확인 필요".into())
    });
    let window_lines = windows
        .iter()
        .filter(|w| w.account_scoped || w.remaining.is_some())
        .map(|w| {
            let value = w
                .remaining
                .map(|n| format!("{n}% 남음"))
                .unwrap_or_else(|| "사용량 확인 전".into());
            format!(
                "{} {value}{}{}",
                w.label,
                w.reset_text
                    .as_ref()
                    .map(|s| format!(" · {s}"))
                    .unwrap_or_default(),
                if w.model_limited {
                    " · 사용 제한"
                } else {
                    ""
                }
            )
        })
        .collect();
    let observed = a
        .observed_at
        .as_deref()
        .and_then(timestamp)
        .map(|t| t.with_timezone(&Local).format("%H:%M").to_string());
    let sessions_line = format!(
        "연결된 대화 {}개 · {}",
        a.bound_sessions,
        observed
            .map(|s| format!("{s} 기준"))
            .unwrap_or_else(|| "기준 시각 확인 전".into())
    );
    let credits_enable_confirmation = (a.provider == "codex").then(|| Confirmation {
        title: "유료 크레딧 사용",
        body: format!(
            "{name}가 한도에 도달한 뒤, 잔액이 있을 때 유료 크레딧 사용을 허용할까요? (현재 잔액 {})",
            a.credits
                .as_ref()
                .map(|c| credit_amount(c, false))
                .unwrap_or_else(|| "확인 전".into())
        ),
        confirm: "크레딧 사용",
        cancel: "취소",
    });
    let model_cooling_line = (a.health.kind == "cooling"
        && a.health.scope.as_deref().is_some_and(|s| s != "account"))
    .then(|| {
        format!(
            "{} · 사용 제한{}",
            clean(a.health.scope.as_deref().unwrap_or("")),
            a.health
                .until
                .as_deref()
                .and_then(timestamp)
                .map(|t| format!(" · {}까지", local_time(t, now, false)))
                .unwrap_or_default()
        )
    });
    let menu_credits = credits.cloned().or_else(|| credits_line(a));
    MenuInfo {
        full_name: clean(nonempty(a.email.as_deref()).unwrap_or(name)),
        identity_line,
        refresh_warning,
        window_lines,
        model_cooling_line,
        credits_line: menu_credits.map(|c| format!("크레딧 {}", c.exact_amount)),
        sessions_line,
        show_refresh_usage: a.user_state != "disabled" && a.health.kind != "needs_relogin",
        show_relogin: a.source.owned_or_adopted(),
        show_refresh_token: a.source.owned_or_adopted()
            && !a.drain
            && a.health.kind != "needs_relogin"
            && (a.refresh_attention
                || a.token_expires_at
                    .as_deref()
                    .and_then(timestamp)
                    .is_some_and(|t| t <= now + chrono::Duration::hours(1))),
        credits_enable_confirmation,
    }
}
fn usage_tooltip(
    binding: Option<&WindowMetric>,
    windows: &[WindowMetric],
    observed: Option<&str>,
    now: DateTime<Utc>,
) -> String {
    let mut lines = Vec::new();
    if let Some(w) = binding {
        if let (Some(used), Some(remaining)) = (w.used, w.remaining) {
            lines.push(format!(
                "{} 사용량: {used}% 사용, {remaining}% 남음",
                w.label
            ));
        } else {
            lines.push("사용량 확인 전".into());
        }
        if let Some(t) = w.reset {
            lines.push(if t <= now {
                "초기화됨".into()
            } else {
                format!("{}에 초기화됩니다.", local_time(t, now, true))
            });
        }
    } else {
        lines.push("사용량 확인 전".into());
    }
    for w in windows
        .iter()
        .filter(|w| binding.is_none_or(|b| b.raw.name != w.raw.name))
    {
        if let (Some(used), Some(remaining)) = (w.used, w.remaining) {
            lines.push(format!(
                "{}: {used}% 사용, {remaining}% 남음{}",
                w.label,
                w.reset
                    .map(|t| format!(
                        " · {}",
                        if t <= now {
                            "초기화됨".into()
                        } else {
                            format!("{} 초기화", local_time(t, now, true))
                        }
                    ))
                    .unwrap_or_default()
            ));
        }
    }
    let observed = observed
        .and_then(timestamp)
        .map(|t| format!(" {} 기준.", t.with_timezone(&Local).format("%H:%M")))
        .unwrap_or_else(|| " 기준 시각 확인 전.".into());
    lines.push(format!(
        "각 막대는 해당 사용량 창의 남은 양입니다.{observed}"
    ));
    lines.join("\n")
}
pub fn timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}
/// Calendar comparisons, not a rolling 24-hour interval. Weekdays are tooltip-only.
pub fn calendar_time<T: TimeZone>(at: DateTime<T>, now: DateTime<T>, tooltip: bool) -> String
where
    T::Offset: std::fmt::Display,
{
    let clock = at.format("%H:%M");
    if at.date_naive() == now.date_naive() {
        return clock.to_string();
    }
    if now.date_naive().succ_opt() == Some(at.date_naive()) {
        return format!("내일 {clock}");
    }
    if tooltip {
        let day = ["월", "화", "수", "목", "금", "토", "일"]
            [at.weekday().num_days_from_monday() as usize];
        format!("{}/{}({day}) {clock}", at.month(), at.day())
    } else {
        format!("{}/{} {clock}", at.month(), at.day())
    }
}
fn local_time(at: DateTime<Utc>, now: DateTime<Utc>, tooltip: bool) -> String {
    calendar_time(at.with_timezone(&Local), now.with_timezone(&Local), tooltip)
}
fn reset_label(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    if at <= now {
        "초기화됨".into()
    } else {
        format!("{} 초기화", local_time(at, now, false))
    }
}
/// Footer uses receipt time of last successful state, never provider observed_at.
pub fn stale_notice(last_success: SystemTime, now: SystemTime) -> Option<String> {
    let age = now.duration_since(last_success).ok()?.as_secs();
    (age > 60).then(|| format!("{}분 전 정보", (age / 60).max(1)))
}
fn truncate_utf16(s: &str, max: usize) -> String {
    if s.encode_utf16().count() <= max {
        return s.into();
    }
    let mut result = String::new();
    let mut units = 0;
    for c in s.chars() {
        if units + c.len_utf16() > max.saturating_sub(1) {
            break;
        }
        units += c.len_utf16();
        result.push(c);
    }
    if max > 0 {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        timestamp("2026-10-18T12:05:00Z").unwrap()
    }
    fn base(id: &str) -> Value {
        json!({"id":id,"provider":"codex","email":format!("{id}@example.test"),"user_state":"active","health":{"kind":"ready"},"source":{"kind":"oauth"},"pool_eligible":true})
    }
    fn view(accounts: Vec<Value>, eligible: u64) -> View {
        project(
            &json!({"accounts":accounts,"pools":{"codex":{"eligible":eligible}}}).to_string(),
            now().into(),
        )
        .unwrap()
    }
    fn codex(v: &View) -> &Section {
        &v.sections[1]
    }
    fn summary(v: &View) -> &Summary {
        &v.summaries[1]
    }
    fn window(name: &str, used: f64, reset: &str) -> Value {
        json!({"name":name,"used_pct":used,"reset_at":reset})
    }

    #[test]
    fn empty_unknown_and_invalid_input() {
        let v = project(r#"{"future":{"anything":true}}"#, now().into()).unwrap();
        assert!(v.empty && v.sections.is_empty() && v.summaries.is_empty());
        assert!(v.raw.extra.contains_key("future"));
        assert!(project("{", now().into()).is_err());
        let mut a = base("a");
        a["future"] = json!({"kind":"not-ui-text"});
        a["plan"] = json!("future-plan");
        a["windows"] =
            json!([{"name":"future-model","scope":"model:new","used_pct":99,"surprise":1}]);
        let v = view(vec![a], 1);
        let r = &codex(&v).rows[0];
        assert!(r.raw.extra.contains_key("future"));
        assert!(r.plan.is_none());
        assert!(r.usage.as_ref().unwrap().binding.is_none());
        assert!(!r.accessible_name.contains("not-ui-text"));
        assert_eq!(r.windows[0].raw.extra["surprise"], 1);
        assert_eq!(v.summaries[0].verdict, Verdict::NoAccounts);
    }
    #[test]
    fn nullable_pool_metadata_keeps_accounts_with_an_unknown_verdict() {
        for pools in [Value::Null, json!({"codex":null})] {
            let v = project(
                &json!({"accounts":[base("kept")],"pools":pools}).to_string(),
                now().into(),
            )
            .unwrap();
            assert_eq!(codex(&v).rows.len(), 1);
            assert_eq!(v.summaries[1].verdict, Verdict::Unknown);
            assert_eq!(v.badge, Badge::Normal);
        }
    }
    #[test]
    fn plans_are_provider_specific() {
        for (provider, raw, label) in [
            ("claude", "pro", "Pro"),
            ("claude", "max", "Max"),
            ("claude", "max_5x", "Max 5x"),
            ("claude", "max_20x", "Max 20x"),
            ("codex", "free", "Free"),
            ("codex", "plus", "Plus"),
            ("codex", "pro", "Pro"),
            ("codex", "team", "Team"),
            ("codex", "business", "Business"),
            ("codex", "enterprise", "Enterprise"),
        ] {
            assert_eq!(plan_label(provider, Some(raw)), Some(label));
        }
        assert_eq!(plan_label("claude", Some("plus")), None);
        assert_eq!(plan_label("codex", Some("max")), None);
        assert_eq!(plan_label("codex", None), None);
    }
    #[test]
    fn names_collide_only_within_provider_and_labels_win() {
        let mut a = base("one");
        a["email"] = json!("same@one.test");
        let mut b = base("two");
        b["email"] = json!("same@two.test");
        let mut c = base("three");
        c["email"] = json!("same@three.test");
        c["provider"] = json!("claude");
        let v = view(vec![a.clone(), b, c], 1);
        assert_eq!(codex(&v).rows[0].name, "same@one.test");
        assert_eq!(v.sections[0].rows[0].name, "same");
        a["label"] = json!("Chosen\nname");
        let v = view(vec![a], 1);
        assert_eq!(codex(&v).rows[0].name, "Chosenname");
        let mut a = base("a");
        a["email"] = Value::Null;
        let v = view(vec![a], 1);
        assert_eq!(codex(&v).rows[0].name, "이름 없는 계정");
    }
    #[test]
    fn generated_email_labels_are_unset_but_real_organisation_and_custom_names_stay() {
        for label in ["same@one.test", "same@one.test's Organization"] {
            let mut a = base("one");
            a["email"] = json!("same@one.test");
            a["label"] = json!(label);
            let projected = view(vec![a.clone()], 1);
            assert_eq!(codex(&projected).rows[0].name, "same");
            let mut b = base("two");
            b["email"] = json!("same@two.test");
            let projected = view(vec![a, b], 1);
            assert_eq!(codex(&projected).rows[0].name, "same@one.test");
        }
        for label in ["My team", "same@one.test's Organization Custom"] {
            let mut a = base("one");
            a["email"] = json!("same@one.test");
            a["label"] = json!(label);
            assert_eq!(codex(&view(vec![a], 1)).rows[0].name, label);
        }
    }
    #[test]
    fn every_known_account_window_is_displayed_but_binding_still_uses_limit() {
        let mut a = base("a");
        a["windows"] = json!([
            window("5h", 62.0, "2026-10-19T00:00:00Z"),
            window("7d", 62.0, "2026-10-22T00:00:00Z"),
            window("7d_oi", 100.0, "2026-10-23T00:00:00Z"),
            window("seven_day_opus", 100.0, "2026-10-24T00:00:00Z")
        ]);
        let v = view(vec![a.clone()], 1);
        let u = codex(&v).rows[0].usage.as_ref().unwrap();
        assert_eq!(u.binding.as_ref().unwrap().raw.name, "7d");
        assert_eq!(
            u.lines
                .iter()
                .map(|l| l.window.raw.name.as_str())
                .collect::<Vec<_>>(),
            ["5h", "7d"]
        );
        assert!(u.tooltip.contains("62% 사용, 38% 남음"));
        assert!(u.tooltip.contains("Opus 주간"));
        a["health"] = json!({"kind":"cooling","reason":"quota","scope":"account"});
        a["quota_cooling"] = json!({"window":"5h","reset_at":"2026-10-20T01:00:00Z","started_at":"2026-10-18T12:00:00Z"});
        let v = view(vec![a.clone()], 0);
        let w = codex(&v).rows[0]
            .usage
            .as_ref()
            .unwrap()
            .binding
            .as_ref()
            .unwrap();
        assert_eq!(w.raw.name, "5h");
        assert_eq!(w.reset, timestamp("2026-10-20T01:00:00Z"));
        let row = &codex(&v).rows[0];
        let usage = row.usage.as_ref().unwrap();
        let first = &usage.lines[0].window;
        assert_eq!(first.reset, w.reset);
        assert_eq!(first.reset_text, w.reset_text);
        assert!(
            row.accessible_name
                .contains(first.reset_text.as_deref().unwrap())
        );
        assert!(
            usage
                .tooltip
                .contains(&local_time(w.reset.unwrap(), now(), true))
        );
        assert_eq!(usage.lines[1].window.raw.name, "7d");
        assert_eq!(
            usage.lines[1].window.reset,
            timestamp("2026-10-22T00:00:00Z")
        );
        a["quota_cooling"]["window"] = json!("7d_oi");
        let v = view(vec![a], 0);
        assert!(codex(&v).rows[0].usage.as_ref().unwrap().binding.is_none());
    }
    #[test]
    fn account_windows_render_in_duration_order_and_free_has_one_thirty_day_line() {
        let mut claude = base("claude");
        claude["provider"] = json!("claude");
        claude["windows"] = json!([
            window("7d", 25., "2026-10-26T00:00:00Z"),
            window("5h", 75., "2026-10-21T20:00:00Z")
        ]);
        let mut plus = base("plus");
        plus["windows"] = claude["windows"].clone();
        let mut free = base("free");
        free["plan"] = json!("free");
        free["windows"] = json!([window("43200m", 40., "2026-11-20T00:00:00Z")]);
        let v = view(vec![claude, plus, free], 3);
        for row in [&v.sections[0].rows[0], &codex(&v).rows[1]] {
            let usage = row.usage.as_ref().unwrap();
            assert_eq!(
                usage
                    .lines
                    .iter()
                    .map(|l| l.window.label.as_str())
                    .collect::<Vec<_>>(),
                ["5시간", "주간"]
            );
            assert_eq!(usage.binding.as_ref().unwrap().raw.name, "5h");
        }
        let free = codex(&v).rows.iter().find(|r| r.raw.id == "free").unwrap();
        let usage = free.usage.as_ref().unwrap();
        assert_eq!(usage.lines.len(), 1);
        assert_eq!(usage.lines[0].window.label, "30일");
        assert_eq!(usage.binding.as_ref().unwrap().raw.name, "43200m");
    }
    #[test]
    fn unknown_meter_and_authoritative_missing_meter() {
        let mut a = base("a");
        a["windows"] = json!([{"name":"5h","used_pct":null,"reset_at":"bad-time"}]);
        let v = view(vec![a.clone()], 1);
        assert_eq!(
            codex(&v).rows[0].usage.as_ref().unwrap().text,
            "사용량 확인 전"
        );
        a["health"] = json!({"kind":"cooling","reason":"quota","scope":"account"});
        a["quota_cooling"] = json!({"window":"7d","reset_at":"2026-10-22T00:00:00Z"});
        let v = view(vec![a], 0);
        let w = codex(&v).rows[0]
            .usage
            .as_ref()
            .unwrap()
            .binding
            .as_ref()
            .unwrap();
        assert_eq!(w.raw.name, "7d");
        assert_eq!(w.remaining, None);
        assert!(w.reset.is_some());
    }
    #[test]
    fn percent_clamping_rounding_and_bar_thresholds() {
        for (used, remaining, tone) in [
            (-1., 100, BarTone::Ok),
            (0., 100, BarTone::Ok),
            (69.5, 30, BarTone::Ok),
            (70.6, 29, BarTone::Low),
            (90., 10, BarTone::Low),
            (90.5, 9, BarTone::Critical),
            (140., 0, BarTone::Limit),
        ] {
            let mut a = base("a");
            a["windows"] = json!([window("5h", used, "2026-10-22T00:00:00Z")]);
            let v = view(vec![a], 1);
            let u = codex(&v).rows[0].usage.as_ref().unwrap();
            assert_eq!(u.binding.as_ref().unwrap().remaining, Some(remaining));
            assert_eq!(u.lines[0].bar_tone, tone);
        }
        assert_eq!(window_label("10080m"), "7일");
        assert_eq!(window_label("300m"), "300분");
    }
    #[test]
    fn row_priority_and_color_semantics() {
        for (reason, state, tone, button) in [
            ("link_stale", DisplayState::LinkStale, Tone::Bad, None),
            (
                "store_error",
                DisplayState::StoreError,
                Tone::Bad,
                Some(ActionKind::OpenLogs),
            ),
            ("payment", DisplayState::Payment, Tone::Bad, None),
            ("permission", DisplayState::Permission, Tone::Bad, None),
        ] {
            let mut a = base("a");
            a["user_state"] = json!("paused");
            a["credits_in_use"] = json!(true);
            a["health"] = json!({"kind":"cooling","reason":reason,"scope":"account"});
            let v = view(vec![a], 0);
            let r = &codex(&v).rows[0];
            assert_eq!(r.state, state);
            assert_eq!(r.chip.as_ref().unwrap().tone, tone);
            assert_eq!(r.action.as_ref().map(|a| a.kind), button);
            assert_eq!(summary(&v).todo, 1);
        }
        let mut a = base("a");
        a["health"] = json!({"kind":"needs_relogin"});
        a["credits_in_use"] = json!(true);
        a["user_state"] = json!("paused");
        let v = view(vec![a.clone()], 0);
        let r = &codex(&v).rows[0];
        assert_eq!(r.state, DisplayState::Relogin);
        assert!(r.usage.is_none());
        assert_eq!(r.action.as_ref().unwrap().kind, ActionKind::Relogin);
        a["source"] = json!({"kind":"cpa","mode":"linked"});
        let v = view(vec![a], 0);
        assert!(codex(&v).rows[0].action.is_none());
        for (user, credits, reason, sessions, drain, state, tone) in [
            (
                "paused",
                true,
                "quota",
                4,
                true,
                DisplayState::Credits,
                Some(Tone::Warn),
            ),
            (
                "paused",
                false,
                "quota",
                4,
                true,
                DisplayState::Paused,
                Some(Tone::Off),
            ),
            (
                "active",
                false,
                "quota",
                4,
                true,
                DisplayState::Quota,
                Some(Tone::Limit),
            ),
            (
                "active",
                false,
                "auth",
                4,
                true,
                DisplayState::Auth,
                Some(Tone::Off),
            ),
            (
                "active",
                false,
                "rate",
                4,
                true,
                DisplayState::InUse,
                Some(Tone::Use),
            ),
            (
                "active",
                false,
                "rate",
                0,
                true,
                DisplayState::Drain,
                Some(Tone::Off),
            ),
            ("active", false, "rate", 0, false, DisplayState::Ready, None),
        ] {
            let mut a = base("a");
            a["user_state"] = json!(user);
            a["credits_in_use"] = json!(credits);
            a["active_sessions"] = json!(sessions);
            a["drain"] = json!(drain);
            a["health"] = json!({"kind":"cooling","reason":reason,"scope":"account"});
            let v = view(vec![a], 1);
            let r = &codex(&v).rows[0];
            assert_eq!(r.state, state);
            assert_eq!(r.chip.as_ref().map(|c| c.tone), tone);
        }
    }
    #[test]
    fn model_cooling_does_not_create_chip_or_bind() {
        let mut a = base("a");
        a["health"] = json!({"kind":"cooling","reason":"permission","scope":"model:opus"});
        let v = view(vec![a], 1);
        let r = &codex(&v).rows[0];
        assert!(r.chip.is_none());
        assert!(
            r.menu
                .model_cooling_line
                .as_ref()
                .unwrap()
                .contains("사용 제한")
        );
        assert_eq!(summary(&v).todo, 0);
    }
    #[test]
    fn authoritative_pools_and_nonvacuous_credits() {
        let mut a = base("a");
        a["windows"] = json!([window("5h", 100., "2026-10-19T00:00:00Z")]);
        a["allow_credits"] = json!(true);
        a["credits_in_use"] = json!(false);
        let v = view(vec![a.clone()], 1);
        assert_eq!(summary(&v).verdict, Verdict::Credits);
        assert_eq!(codex(&v).rows[0].state, DisplayState::Ready);
        let mut b = base("b");
        b["drain"] = json!(true);
        b["eligible_for_new_session"] = json!(false);
        let v = view(vec![a.clone(), b], 2);
        assert_eq!(summary(&v).verdict, Verdict::Available);
        assert_eq!(summary(&v).serving_mode, ServingMode::Subscription);
        a.as_object_mut().unwrap().remove("pool_eligible");
        a["eligible_for_new_session"] = json!(true);
        let v = view(vec![a.clone()], 1);
        assert_eq!(summary(&v).verdict, Verdict::Available);
        assert_eq!(summary(&v).serving_mode, ServingMode::Unclassified);
        let v = view(vec![a.clone()], 0);
        assert_eq!(summary(&v).verdict, Verdict::Unavailable);
        assert!(v.note.is_some());
        assert_eq!(v.badge, Badge::Attention);
        let v = project(&json!({"accounts":[a]}).to_string(), now().into()).unwrap();
        assert_eq!(summary(&v).verdict, Verdict::Unknown);
    }
    #[test]
    fn paid_99_percent_summary_uses_observed_reset_without_inventing_credit_use() {
        let mut a = base("a");
        a["allow_credits"] = json!(true);
        a["windows"] = json!([window("5h", 99., "2026-10-22T00:00:00Z")]);
        let v = view(vec![a.clone()], 1);
        assert_eq!(summary(&v).verdict, Verdict::Available);
        a["credits_in_use"] = json!(true);
        let v = view(vec![a], 1);
        assert_eq!(summary(&v).verdict, Verdict::Credits);
        assert_eq!(
            summary(&v).detail,
            format!(
                "{} 한도 초기화",
                local_time(timestamp("2026-10-22T00:00:00Z").unwrap(), now(), false)
            )
        );
    }
    #[test]
    fn actual_spending_requires_core_flag() {
        let mut a = base("a");
        a["allow_credits"] = json!(true);
        a["credits"] = json!({"has_credits":true,"balance":19395.23,"serving":true});
        let v = view(vec![a.clone()], 1);
        assert_eq!(codex(&v).rows[0].state, DisplayState::Ready);
        assert!(!codex(&v).rows[0].credits.as_ref().unwrap().actually_serving);
        a["credits_in_use"] = json!(false);
        let v = view(vec![a.clone()], 1);
        assert!(!codex(&v).rows[0].credits.as_ref().unwrap().actually_serving);
        a["credits_in_use"] = json!(true);
        let v = view(vec![a], 1);
        let r = &codex(&v).rows[0];
        assert_eq!(r.state, DisplayState::Credits);
        assert_eq!(r.action.as_ref().unwrap().kind, ActionKind::DisableCredits);
        assert!(r.action.as_ref().unwrap().confirmation.is_none());
    }
    #[test]
    fn credits_confirmation_distinguishes_absent_unknown_and_available_balance() {
        for (credits, expected) in [
            (json!({"has_credits":false,"balance":null}), "없음"),
            (json!({"has_credits":null,"balance":null}), "확인 전"),
            (json!({"has_credits":true,"balance":null}), "있음"),
            (json!({"has_credits":false,"balance":0}), "없음"),
            (json!({"has_credits":false,"balance":12}), "없음"),
        ] {
            let mut a = base("a");
            a["credits"] = credits;
            let v = view(vec![a], 1);
            let confirmation = codex(&v).rows[0]
                .menu
                .credits_enable_confirmation
                .as_ref()
                .unwrap();
            assert!(confirmation.body.contains(&format!("현재 잔액 {expected}")));
            assert!(confirmation.body.contains("잔액이 있을 때"));
            assert!(confirmation.body.contains("사용을 허용"));
        }
    }
    #[test]
    fn credit_lines_exact_floor_unlimited_and_every_enable_confirmation() {
        let mut a = base("a");
        a["credits"] = json!({"has_credits":true,"balance":19395.23});
        a["health"] = json!({"kind":"cooling","reason":"quota","scope":"account"});
        a["quota_credit_releasable"] = json!(true);
        let v = view(vec![a.clone()], 0);
        let r = &codex(&v).rows[0];
        let c = r.credits.as_ref().unwrap();
        assert_eq!(c.amount, "19,395");
        assert_eq!(c.exact_amount, "19,395.23");
        assert!(c.text.contains("사용 안 함"));
        assert!(c.tooltip.contains("19,395.23"));
        assert_eq!(r.menu.credits_line.as_deref(), Some("크레딧 19,395.23"));
        let confirmation = r.action.as_ref().unwrap().confirmation.as_ref().unwrap();
        assert_eq!(confirmation.title, "유료 크레딧 사용");
        assert_eq!(confirmation.confirm, "크레딧 사용");
        assert!(confirmation.body.contains("잔액 19,395"));
        assert!(r.menu.credits_enable_confirmation.is_some());
        let mut hard = a.clone();
        hard["quota_credit_releasable"] = json!(false);
        let hard_view = view(vec![hard], 0);
        assert!(codex(&hard_view).rows[0].action.is_none());
        assert!(
            codex(&hard_view).rows[0]
                .menu
                .credits_enable_confirmation
                .is_some()
        );
        let again = view(vec![a.clone()], 0);
        assert!(
            codex(&again).rows[0]
                .action
                .as_ref()
                .unwrap()
                .confirmation
                .is_some()
        );
        let v = view(vec![a.clone()], 1);
        assert!(codex(&v).rows[0].action.is_none());
        assert!(codex(&v).rows[0].menu.credits_enable_confirmation.is_some());
        a["allow_credits"] = json!(true);
        let v = view(vec![a.clone()], 0);
        assert!(codex(&v).rows[0].action.is_none());
        assert!(
            codex(&v).rows[0]
                .credits
                .as_ref()
                .unwrap()
                .text
                .ends_with("한도 도달 시 사용")
        );
        a["credits"] = json!({"unlimited":true});
        let v = view(vec![a.clone()], 1);
        assert_eq!(codex(&v).rows[0].credits.as_ref().unwrap().amount, "무제한");
        a["credits"] = json!({"has_credits":true});
        let v = view(vec![a.clone()], 1);
        assert_eq!(codex(&v).rows[0].credits.as_ref().unwrap().amount, "있음");
        a["credits"] = json!({"has_credits":false,"balance":0});
        let v = view(vec![a], 0);
        assert!(codex(&v).rows[0].credits.is_none());
        assert_eq!(group_number("1234567.2301"), "1,234,567.2301");
        assert_eq!(group_number("-1234.5"), "-1,234.5");
    }
    #[test]
    fn disabled_and_paused_expansion_and_usage() {
        let mut a = base("a");
        a["user_state"] = json!("disabled");
        a["health"] = json!({"kind":"needs_relogin"});
        a["windows"] = json!([window("5h", 40., "2026-10-21T20:00:00Z")]);
        let v = view(vec![a.clone()], 0);
        let s = codex(&v);
        assert_eq!(summary(&v).verdict, Verdict::Off);
        assert!(s.rows.is_empty());
        assert!(s.auto_expand_recommended);
        assert!(s.expansion_hint.is_some());
        assert!(s.disabled_rows[0].usage.is_none());
        assert!(s.disabled_rows[0].action.as_ref().unwrap().accent);
        assert_eq!(summary(&v).todo, 0);
        let v = view(vec![a.clone(), base("b")], 1);
        assert!(!codex(&v).auto_expand_recommended);
        assert!(!codex(&v).disabled_rows[0].action.as_ref().unwrap().accent);
        a["user_state"] = json!("paused");
        a["health"] = json!({"kind":"ready"});
        let v = view(vec![a], 0);
        assert_eq!(
            codex(&v).rows[0].usage.as_ref().unwrap().lines[0].bar_tone,
            BarTone::Paused
        );
    }
    #[test]
    fn group_order_waiting_deadline_and_names() {
        let mut accounts = vec![];
        for (id, state, health, sessions) in [
            ("z-ready", "active", "ready", 0),
            ("p-paused", "paused", "ready", 0),
            ("b-action", "active", "needs_relogin", 0),
            ("z-inuse", "active", "ready", 2),
            ("a-ready", "active", "ready", 0),
        ] {
            let mut a = base(id);
            a["user_state"] = json!(state);
            a["health"] = json!({"kind":health});
            a["active_sessions"] = json!(sessions);
            accounts.push(a);
        }
        for (id, until) in [
            ("a-later", "2026-10-22T00:00:00Z"),
            ("z-sooner", "2026-10-19T00:00:00Z"),
            ("no-time", "invalid"),
        ] {
            let mut a = base(id);
            a["health"] =
                json!({"kind":"cooling","reason":"quota","scope":"account","until":until});
            accounts.push(a);
        }
        let v = view(accounts, 1);
        assert_eq!(
            codex(&v)
                .rows
                .iter()
                .map(|r| r.raw.id.as_str())
                .collect::<Vec<_>>(),
            [
                "z-inuse", "b-action", "a-ready", "z-ready", "z-sooner", "a-later", "no-time",
                "p-paused"
            ]
        );
    }
    #[test]
    fn summary_names_counts_relogin_and_cooling_deadline() {
        let mut a = base("a");
        a["active_sessions"] = json!(2);
        let mut b = base("b");
        b["active_sessions"] = json!(3);
        let mut c = base("c");
        c["health"] = json!({"kind":"needs_relogin"});
        c["pool_eligible"] = json!(false);
        let v = view(vec![a.clone(), b, c.clone()], 2);
        assert_eq!(summary(&v).detail, "b 외 1개 사용 중");
        assert_eq!(summary(&v).todo, 1);
        assert!(
            codex(&v)
                .rows
                .iter()
                .find(|r| r.raw.id == "c")
                .unwrap()
                .reason
                .as_ref()
                .unwrap()
                .contains("b가 처리")
        );
        let v = view(vec![base("a")], 1);
        assert_eq!(summary(&v).detail, "계정 1개 준비됨");
        let v = view(vec![c.clone()], 0);
        assert_eq!(summary(&v).detail, "다시 로그인 필요");
        a["health"] = json!({"kind":"cooling","reason":"quota","scope":"account","until":"2026-10-19T00:00:00Z"});
        c["health"] = json!({"kind":"cooling","reason":"quota","scope":"model:x","until":"2026-10-18T13:00:00Z"});
        let v = view(vec![a, c], 0);
        assert_eq!(
            summary(&v).earliest_until,
            timestamp("2026-10-19T00:00:00Z")
        );
        assert!(summary(&v).tooltip.contains("새 요청은 오류로 끝납니다"));
    }
    #[test]
    fn menu_sources_attention_expiry_and_observed_timestamp() {
        let mut a = base("a");
        a["observed_at"] = json!("2026-10-18T11:00:00Z");
        a["bound_sessions"] = json!(4);
        a["token_expires_at"] = json!("2026-10-18T13:04:00Z");
        let v = view(vec![a.clone()], 1);
        let m = &codex(&v).rows[0].menu;
        assert!(m.show_relogin && m.show_refresh_token && m.show_refresh_usage);
        assert!(m.sessions_line.contains("연결된 대화 4개"));
        assert!(
            m.sessions_line.contains(
                &timestamp("2026-10-18T11:00:00Z")
                    .unwrap()
                    .with_timezone(&Local)
                    .format("%H:%M")
                    .to_string()
            )
        );
        a["drain"] = json!(true);
        a["refresh_attention"] = json!(true);
        let v = view(vec![a.clone()], 1);
        assert!(!codex(&v).rows[0].menu.show_refresh_token);
        assert!(codex(&v).rows[0].menu.refresh_warning.is_some());
        a["source"] = json!({"kind":"cpa","mode":"linked"});
        let v = view(vec![a], 1);
        let m = &codex(&v).rows[0].menu;
        assert!(!m.show_relogin && !m.show_refresh_token);
        assert!(m.identity_line.contains("CPA가 토큰 갱신"));
    }
    #[test]
    fn pacing_is_informational_and_missing_countdown_is_not_invented() {
        let mut a = base("a");
        a["pacing"] = json!({"backoff_until":"2026-10-18T12:05:30Z","waiting":1});
        let v = view(vec![a.clone()], 1);
        let r = &codex(&v).rows[0];
        assert!(r.chip.is_none());
        assert_eq!(r.reason.as_deref(), Some("요청 속도 조절 중 · 30초"));
        assert!(r.action.is_none());
        a["pacing"] = json!({"waiting":1});
        let v = view(vec![a], 1);
        assert_eq!(
            codex(&v).rows[0].reason.as_deref(),
            Some("요청 속도 조절 중")
        );
    }
    #[test]
    fn calendar_boundaries_weekdays_and_past_reset() {
        let n = DateTime::parse_from_rfc3339("2026-10-18T23:55:00+09:00").unwrap();
        for (at, text, tooltip) in [
            ("2026-10-18T14:00:00+09:00", "14:00", "14:00"),
            ("2026-10-19T14:00:00+09:00", "내일 14:00", "내일 14:00"),
            (
                "2026-10-22T15:22:00+09:00",
                "10/22 15:22",
                "10/22(목) 15:22",
            ),
        ] {
            let t = DateTime::parse_from_rfc3339(at).unwrap();
            assert_eq!(calendar_time(t, n, false), text);
            assert_eq!(calendar_time(t, n, true), tooltip);
        }
        let n = DateTime::parse_from_rfc3339("2026-12-31T23:55:00+09:00").unwrap();
        let t = DateTime::parse_from_rfc3339("2027-01-01T00:01:00+09:00").unwrap();
        assert_eq!(calendar_time(t, n, false), "내일 00:01");
        assert_eq!(reset_label(now(), now()), "초기화됨");
    }
    #[test]
    fn stale_footer_and_utf16_tooltip_limit() {
        let n: SystemTime = now().into();
        assert!(stale_notice(n, n).is_none());
        assert!(stale_notice(n, n + std::time::Duration::from_secs(60)).is_none());
        assert_eq!(
            stale_notice(n, n + std::time::Duration::from_secs(61)).as_deref(),
            Some("1분 전 정보")
        );
        assert_eq!(
            stale_notice(n, n + std::time::Duration::from_secs(180)).as_deref(),
            Some("3분 전 정보")
        );
        assert!(
            truncate_utf16(&"😀".repeat(100), 127)
                .encode_utf16()
                .count()
                <= 127
        );
    }
}
