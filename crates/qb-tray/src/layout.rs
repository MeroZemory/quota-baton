//! Pure tray layout model. Rendering is implemented by the shared Windows
//! Direct2D/DirectWrite backend in `native_render.rs`.
//!
//! This module intentionally knows nothing about Win32, HTTP, or credentials.  The
//! JSON shape is the read-only `/admin/v1/state` shape, so fixture and UI tests can
//! exercise it without starting the core.

use chrono::{DateTime, Datelike, FixedOffset, Local, Timelike};
use serde::{Deserialize, Deserializer};
use std::time::{Duration, SystemTime};

pub mod actions;
#[cfg(test)]
mod dashboard_fixtures;
pub mod dashboard_history;
pub mod dashboard_model;
pub mod geometry;
#[cfg(windows)]
pub mod render;
pub mod state;
pub mod v2;
pub mod v2_colors;
pub mod v2_geometry;
pub mod v2_notices;
#[cfg(windows)]
pub mod v2_render;
pub mod v2_session;
#[cfg(windows)]
pub mod v2_text;

#[cfg(windows)]
pub mod native_render;
#[cfg(windows)]
pub use native_render::render_png;

#[derive(Debug, Clone, Deserialize)]
pub struct State {
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub quota_notice: Option<QuotaNotice>,
    #[serde(default)]
    pub last_relevant_state_at: Option<DateTime<chrono::Utc>>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub provider: String,
    pub label: Option<String>,
    pub email: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub user_state: String,
    #[serde(default)]
    pub health: Health,
    #[serde(default)]
    pub probe_pending: bool,
    #[serde(default)]
    pub windows: Vec<Window>,
    #[serde(default)]
    pub refresh_attention: bool,
    #[serde(default)]
    pub drain: bool,
    #[serde(default)]
    pub allow_credits: bool,
    #[serde(default)]
    pub bound_sessions: u64,
    #[serde(default)]
    pub active_sessions: u64,
    #[serde(default)]
    pub spread_threshold: u64,
    #[serde(default, deserialize_with = "decode_source")]
    pub source: String,
    #[serde(default)]
    pub pacing: Pacing,
    #[serde(default)]
    pub token_expires_at: Option<String>,
    #[serde(default)]
    pub last_refresh_at: Option<String>,
}

/// Core evidence only. UI preferences and quota meters cannot create this context.
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaAction {
    Credits,
    Failover,
    Exhausted,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct QuotaNotice {
    #[serde(default)]
    pub model_limited: bool,
    pub window: Option<String>,
    pub reset_at: Option<DateTime<chrono::Utc>>,
    pub action: QuotaAction,
    pub next_account: Option<String>,
    #[serde(default)]
    pub earliest_account: Option<String>,
    pub observed_at: DateTime<chrono::Utc>,
}

impl QuotaNotice {
    pub fn current_since(
        &self,
        now: SystemTime,
        last_relevant_state_at: Option<DateTime<chrono::Utc>>,
    ) -> bool {
        self.current(now) && last_relevant_state_at.is_none_or(|at| self.observed_at >= at)
    }
    pub fn current(&self, now: SystemTime) -> bool {
        let now = DateTime::<chrono::Utc>::from(now);
        self.observed_at <= now && self.reset_at.is_none_or(|at| at > now)
    }
}

pub fn notice_account_name(email: Option<&str>, label: Option<&str>, id: &str) -> String {
    email
        .and_then(|s| s.split('@').next())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| label.filter(|s| !s.trim().is_empty()))
        .unwrap_or(id)
        .chars()
        .filter(|ch| !ch.is_control())
        .take(80)
        .collect()
}

pub struct QuotaMessage {
    pub title: String,
    pub text: String,
    pub short: String,
}

// Shell reserves 64 UTF-16 units including NUL. Shorten the name, not the action.
fn fit_account_title(name: &str, prefix: &str, suffix: &str) -> String {
    let budget =
        63usize.saturating_sub(prefix.encode_utf16().count() + suffix.encode_utf16().count());
    if name.encode_utf16().count() <= budget {
        return format!("{prefix}{name}{suffix}");
    }
    let mut kept = String::new();
    let mut used = 0;
    for ch in name.chars() {
        if used + ch.len_utf16() > budget.saturating_sub(1) {
            break;
        }
        used += ch.len_utf16();
        kept.push(ch);
    }
    if budget > 0 {
        kept.push('…');
    }
    format!("{prefix}{kept}{suffix}")
}

pub fn account_notice_title(name: &str, provider: &str, reason: &str) -> String {
    fit_account_title(
        name,
        "",
        &format!(" · {} {reason}", provider_name(provider)),
    )
}

/// Shared notification/row wording; callers resolve only the core-selected ID.
pub fn quota_message(
    notice: &QuotaNotice,
    name: &str,
    provider: &str,
    next_name: Option<&str>,
    earliest_name: Option<&str>,
    now: SystemTime,
) -> Option<QuotaMessage> {
    if !notice.current(now) {
        return None;
    }
    let window = match notice.window.as_deref() {
        Some("5h") => "5시간 한도",
        Some(name) if name == "7d" || name.starts_with("7d_") => "주간 한도",
        _ => "한도",
    };
    let reset = notice.reset_at.map(|at| {
        let local = at.with_timezone(&Local);
        if at.signed_duration_since(DateTime::<chrono::Utc>::from(now))
            < chrono::Duration::hours(24)
        {
            local.format("%H:%M").to_string()
        } else {
            format!(
                "{}/{} {}",
                local.month(),
                local.day(),
                local.format("%H:%M")
            )
        }
    });
    let reset_text = reset.as_ref().map_or_else(
        || "초기화 시간 확인 중입니다.".into(),
        |at| format!("{at} 초기화."),
    );
    let scope = if notice.model_limited {
        "이 모델 "
    } else {
        ""
    };
    let requests = if notice.model_limited {
        "이 모델의 새 요청"
    } else {
        "새 요청"
    };
    let (action, short): (String, String) = match notice.action {
        QuotaAction::Credits => (
            "유료 크레딧으로 계속 사용합니다.".into(),
            "크레딧 사용 중".into(),
        ),
        QuotaAction::Failover => next_name.map_or_else(
            || {
                (
                    "다른 계정으로 이어서 처리합니다.".into(),
                    "다른 계정이 처리".into(),
                )
            },
            |next| {
                (
                    format!("{next} 계정으로 이어서 처리합니다."),
                    format!("{next}가 처리"),
                )
            },
        ),
        QuotaAction::Exhausted => (
            format!("처리할 다른 계정이 없습니다. {requests}은 한도 오류가 납니다."),
            "처리할 다른 계정 없음".into(),
        ),
    };
    Some(QuotaMessage {
        title: if notice.action == QuotaAction::Exhausted {
            if notice.model_limited {
                fit_account_title(
                    name,
                    &format!(
                        "{} · 이 모델 처리 계정 모두 한도 도달 · ",
                        provider_name(provider)
                    ),
                    "",
                )
            } else {
                fit_account_title(
                    name,
                    &format!("{} 계정 모두 한도 도달 · ", provider_name(provider)),
                    "",
                )
            }
        } else {
            account_notice_title(
                name,
                provider,
                &format!(
                    "{}{window} 도달",
                    if notice.model_limited {
                        "이 모델의 "
                    } else {
                        ""
                    }
                ),
            )
        },
        text: if notice.action == QuotaAction::Exhausted {
            match (&reset, earliest_name) {
                (Some(at), Some(earliest)) => format!(
                    "가장 빠른 초기화: {earliest} {at}. 그때까지 {requests}은 한도 오류가 납니다."
                ),
                (Some(at), None) => {
                    format!("가장 빠른 초기화: {at}. 그때까지 {requests}은 한도 오류가 납니다.")
                }
                (None, _) => format!("{action} {reset_text}"),
            }
        } else {
            format!("{action} {reset_text}")
        },
        short: format!(
            "{scope}{window} · {} · {short}",
            reset.unwrap_or_else(|| "초기화 시간 확인 중".into())
        ),
    })
}

#[cfg(test)]
mod quota_notice_tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn context(action: QuotaAction, now: DateTime<Utc>, hours: i64) -> QuotaNotice {
        QuotaNotice {
            model_limited: false,
            window: Some("7d".into()),
            reset_at: Some(now + chrono::Duration::hours(hours)),
            action,
            next_account: Some("next-id".into()),
            earliest_account: None,
            observed_at: now,
        }
    }

    #[test]
    fn local_time_boundary_names_windows_and_truthful_unknown_reset() {
        let now = "2026-10-18T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            notice_account_name(Some("hello.sample-acct@example.test"), Some("label"), "id"),
            "hello.sample-acct"
        );
        assert_eq!(notice_account_name(Some(""), Some("label"), "id"), "label");
        assert_eq!(notice_account_name(None, None, "id"), "id");
        assert_eq!(
            notice_account_name(Some("a\nb@example.test"), None, "id"),
            "ab"
        );
        for (seconds, dated) in [(1, false), (86399, false), (86400, true), (86401, true)] {
            let mut q = context(QuotaAction::Credits, now, 1);
            q.reset_at = Some(now + chrono::Duration::seconds(seconds));
            let local = q.reset_at.unwrap().with_timezone(&Local);
            let expected = if dated {
                format!(
                    "{}/{} {}",
                    local.month(),
                    local.day(),
                    local.format("%H:%M")
                )
            } else {
                local.format("%H:%M").to_string()
            };
            let m = quota_message(&q, "account", "codex", None, None, now.into()).unwrap();
            assert!(m.text.contains(&format!("{expected} 초기화.")));
            assert!(m.title.contains("account · Codex 주간 한도"));
            assert!(m.short.contains("크레딧 사용 중"));
        }
        let mut q = context(QuotaAction::Failover, now, 1);
        q.window = Some("5h".into());
        q.reset_at = None;
        let m = quota_message(&q, "account", "codex", Some("next"), None, now.into()).unwrap();
        assert!(m.title.contains("5시간 한도"));
        assert!(m.text.contains("next 계정으로 이어서 처리"));
        assert!(m.text.contains("초기화 시간 확인 중"));
        assert!(m.short.contains("next가 처리"));
        q.action = QuotaAction::Exhausted;
        let m = quota_message(&q, "account", "codex", None, None, now.into()).unwrap();
        assert!(m.title.contains("Codex 계정 모두 한도 도달"));
        assert!(m.text.contains("새 요청은 한도 오류"));
        q.reset_at = Some(now);
        assert!(quota_message(&q, "account", "codex", None, None, now.into()).is_none());
        q.reset_at = None;
        q.observed_at = now + chrono::Duration::seconds(1);
        assert!(!q.current(now.into()));
    }

    #[test]
    fn scoped_exhaustion_does_not_claim_every_model_is_exhausted() {
        let now: DateTime<Utc> = "2030-01-01T00:00:00Z".parse().unwrap();
        let mut notice = context(QuotaAction::Exhausted, now, 48);
        notice.model_limited = true;
        notice.window = Some("7d_oi".into());
        let message = quota_message(
            &notice,
            "fixture",
            "claude",
            None,
            Some("earliest"),
            now.into(),
        )
        .unwrap();
        assert!(message.title.contains("이 모델 처리 계정 모두 한도"));
        assert!(message.text.contains("이 모델의 새 요청은 한도 오류"));
        assert!(message.short.starts_with("이 모델 주간 한도"));
        assert!(!message.text.contains("그때까지 새 요청은"));
    }

    #[test]
    fn row_uses_authoritative_name_and_clears_expired_or_removed_context() {
        let now = "2026-10-18T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let mut payload = json!({"accounts":[{"id":"a","provider":"codex","user_state":"active",
            "health":{"kind":"ready"}, "quota_notice":context(QuotaAction::Failover, now, 1)},
            {"id":"next-id","provider":"codex","email":"next@example.test","health":{"kind":"ready"}}]});
        let rows = parse_state_at(&payload.to_string(), now.into()).unwrap();
        assert!(rows[0].health_chip.contains("next가 처리"));
        assert_eq!(rows[0].health_tone, Tone::Warning);
        payload["accounts"][1]["email"] = serde_json::Value::Null;
        payload["accounts"][1]["label"] = json!("Next label");
        let rows = parse_state_at(&payload.to_string(), now.into()).unwrap();
        assert!(rows[0].health_chip.contains("Next label가 처리"));
        payload["accounts"][1]["label"] = serde_json::Value::Null;
        let rows = parse_state_at(&payload.to_string(), now.into()).unwrap();
        assert!(rows[0].health_chip.contains("next-id가 처리"));
        payload["accounts"][0]["last_relevant_state_at"] =
            json!(now + chrono::Duration::seconds(1));
        let rows = parse_state_at(
            &payload.to_string(),
            (now + chrono::Duration::seconds(2)).into(),
        )
        .unwrap();
        assert_eq!(rows[0].health_chip, "준비됨");
        payload["accounts"][0]["last_relevant_state_at"] = serde_json::Value::Null;
        let rows = parse_state_at(
            &payload.to_string(),
            (now + chrono::Duration::hours(1)).into(),
        )
        .unwrap();
        assert_eq!(rows[0].health_chip, "준비됨");
        payload["accounts"][0]["quota_notice"] = serde_json::Value::Null;
        let rows = parse_state_at(&payload.to_string(), now.into()).unwrap();
        assert_eq!(rows[0].health_chip, "준비됨");
    }
}

fn decode_source<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(value) => value,
        serde_json::Value::Object(mut object) => {
            let kind = object
                .remove("kind")
                .and_then(|kind| kind.as_str().map(str::to_owned));
            if kind.as_deref() == Some("cpa") {
                match object.get("mode").and_then(serde_json::Value::as_str) {
                    Some(mode @ ("linked" | "adopted")) => mode.to_owned(),
                    _ => "⚠ source decode error (unknown mode)".into(),
                }
            } else {
                kind.unwrap_or_else(|| "⚠ source decode error (missing kind)".into())
            }
        }
        _ => "⚠ source decode error".into(),
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Health {
    #[serde(default)]
    pub kind: String,
    pub reason: Option<String>,
    pub until: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Window {
    #[serde(default)]
    pub name: String,
    pub used_pct: Option<f64>,
    pub reset_at: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Pacing {
    pub backoff_secs: Option<u64>,
    #[serde(default)]
    pub rate_step: u8,
    #[serde(default)]
    pub backoff_until: Option<String>,
    #[serde(default)]
    pub waiting: u64,
    #[serde(default)]
    pub step: u8,
    #[serde(default)]
    pub release_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Shared complete input for the v2 HWND/WIC projection. No credential fields
    /// occur in core state. Manual compatibility rows may omit this context.
    pub view_input: Option<std::sync::Arc<str>>,
    pub id: String,
    pub provider: String,
    pub name: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub user_chip: String,
    pub health_chip: String,
    pub health_tone: Tone,
    pub five_hour: Usage,
    pub seven_day: Usage,
    pub meter_windows: Vec<Usage>,
    pub reason_button: bool,
    pub attention: bool,
    pub source: String,
    pub allow_credits: bool,
    pub drain: bool,
    pub bound_sessions: u64,
    pub session_text: Option<String>,
    pub pacing_until: Option<String>,
    pub token_expires_at: Option<String>,
    pub last_refresh_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub label: String,
    pub percent: Option<f64>,
    pub percent_text: String,
    pub reset_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Good,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Normal,
    Attention,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub background: Rgb,
    pub surface: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub good: Rgb,
    pub warning: Rgb,
    pub critical: Rgb,
    pub track: Rgb,
    pub divider: Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

const FLYOUT_WIDTH: f32 = 360.0;
const FOOTER_HEIGHT: f32 = 48.0;

/// Resolve an exact menu account; stale or malformed selections have no actions.
pub fn menu_target<'a>(rows: &'a [Row], state: &str) -> Option<(&'a Row, usize)> {
    let mut parts = state.strip_prefix("menu:")?.split(':');
    let id = parts.next().filter(|id| !id.is_empty())?;
    let index = match parts.next() {
        Some(index) => index.parse::<usize>().ok()?,
        None => 0,
    };
    if !matches!(parts.next(), None | Some("pending")) || parts.next().is_some() {
        return None;
    }
    let row = rows.iter().find(|row| row.id == id)?;
    (index < actions::menu_items(row).len()).then_some((row, index))
}

/// Explicit provider states override the legacy Codex-only bool. A menu may
/// reveal a disabled account, but never changes its user state or activates it.
pub fn disabled_expanded(
    rows: &[Row],
    expand_disabled: bool,
    state: Option<&str>,
    provider: &str,
) -> bool {
    let provider = match provider {
        "Claude" | "claude" => "claude",
        "Codex" | "codex" => "codex",
        _ => return false,
    };
    if let Some(expanded) = state.and_then(|state| state.strip_prefix("expanded:")) {
        return expanded == provider;
    }
    let menu_disabled = state
        .and_then(|state| menu_target(rows, state))
        .is_some_and(|(row, _)| {
            row.provider == provider_name(provider) && row.user_chip == "비활성"
        });
    menu_disabled || ((expand_disabled || state == Some("expanded")) && provider == "codex")
}

fn row_anchor(rows: &[Row], expand_disabled: bool, state: Option<&str>, id: &str) -> Option<Rect> {
    let mut y = 52.0;
    for provider in ["Claude", "Codex"] {
        let active = rows
            .iter()
            .filter(|r| r.provider == provider && r.user_chip != "비활성")
            .collect::<Vec<_>>();
        let disabled = rows
            .iter()
            .filter(|r| r.provider == provider && r.user_chip == "비활성")
            .collect::<Vec<_>>();
        if active.is_empty() && disabled.is_empty() {
            continue;
        }
        y += 36.0;
        let expanded = disabled_expanded(rows, expand_disabled, state, provider);
        if expanded && !disabled.is_empty() {
            y += 28.0;
        }
        for row in active.into_iter().chain(if expanded {
            disabled.clone()
        } else {
            Vec::new()
        }) {
            let h = row_height(row);
            if row.id == id {
                return Some(Rect::new(16.0, y, FLYOUT_WIDTH - 16.0, y + h));
            }
            y += h;
        }
        if !expanded && !disabled.is_empty() {
            y += 28.0;
        }
    }
    None
}

const ACCOUNT_MENU_META_HEIGHT: f32 = 68.0;

pub fn account_menu_metadata(row: &Row) -> [String; 3] {
    let time = |value: Option<&str>| {
        value
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|time| time.with_timezone(&Local).format("%m/%d %H:%M").to_string())
            .unwrap_or_else(|| "확인 불가".into())
    };
    [
        match row.source.as_str() {
            "linked" => "CPA 갱신".into(),
            "oauth" | "adopted" => "직접 갱신".into(),
            _ => "갱신 방식 확인 불가".into(),
        },
        format!("만료 {}", time(row.token_expires_at.as_deref())),
        format!("최근 갱신 {}", time(row.last_refresh_at.as_deref())),
    ]
}

pub fn account_menu_height(row: &Row) -> f32 {
    let items = actions::menu_items(row);
    let separators = items
        .iter()
        .enumerate()
        .filter(|(i, _)| *i == 2 || *i == 3)
        .count() as f32;
    // Keep a bottom inset for the final glyph and the popup shadow. The inset
    // is part of the geometry so a grown flyout cannot clip the last action.
    ACCOUNT_MENU_META_HEIGHT + 9.0 + items.len() as f32 * 32.0 + separators * 10.0 + 16.0
}

pub fn account_menu_item_top(row: &Row, index: usize) -> f32 {
    let separators_before = actions::menu_items(row)
        .iter()
        .enumerate()
        .filter(|(i, _)| *i <= index && (*i == 2 || *i == 3))
        .count() as f32;
    ACCOUNT_MENU_META_HEIGHT + 9.0 + index as f32 * 32.0 + separators_before * 9.0
}

pub fn popup_rect(rows: &[Row], expand_disabled: bool, state: &str, height: f32) -> Option<Rect> {
    let footer_y = height - FOOTER_HEIGHT;
    if state.starts_with("menu:") {
        let (row, _) = menu_target(rows, state)?;
        let anchor = row_anchor(rows, expand_disabled, Some(state), &row.id)?;
        let popup_h = account_menu_height(row);
        let down = anchor.bottom + 4.0;
        let top = if down + popup_h <= footer_y {
            down
        } else if anchor.top >= popup_h + 4.0 {
            anchor.top - popup_h - 4.0
        } else {
            0.0
        };
        return Some(Rect::new(136.0, top, 352.0, top + popup_h));
    }
    if state == "add-menu" {
        return Some(Rect::new(128.0, footer_y - 130.0, 352.0, footer_y - 4.0));
    }
    if state == "quit" {
        return Some(Rect::new(152.0, footer_y - 92.0, 352.0, footer_y - 4.0));
    }
    None
}

pub fn popup_required_height(rows: &[Row], expand_disabled: bool, state: &str, base: f32) -> f32 {
    // Footer popups must fit even before the first account exists.
    if state == "add-menu" {
        return base.max(FOOTER_HEIGHT + 130.0);
    }
    if state == "quit" {
        return base.max(FOOTER_HEIGHT + 92.0);
    }
    let Some(popup) = popup_rect(rows, expand_disabled, state, base) else {
        return base;
    };
    base.max(popup.bottom + if popup.top == 0.0 { FOOTER_HEIGHT } else { 0.0 })
}

impl Rect {
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HitTarget {
    Row {
        id: String,
        rect: Rect,
    },
    RowMenu {
        id: String,
        rect: Rect,
    },
    RowRelogin {
        id: String,
        rect: Rect,
    },
    Disclosure {
        provider: String,
        rect: Rect,
    },
    MenuSurface {
        rect: Rect,
    },
    MenuItem {
        id: String,
        index: usize,
        rect: Rect,
    },
    AddChoice {
        provider: Option<String>,
        rect: Rect,
    },
    Footer {
        action: FooterAction,
        rect: Rect,
    },
    Login {
        provider: String,
        action: LoginAction,
        rect: Rect,
    },
    Quit {
        action: QuitAction,
        rect: Rect,
    },
    Dialog {
        action: DialogAction,
        rect: Rect,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterAction {
    AddAccount,
    Settings,
    Logs,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginAction {
    CopyAddress,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitAction {
    TrayOnly,
    TrayAndProxy,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAction {
    ConfirmDelete,
    Cancel,
}

impl HitTarget {
    pub fn rect(&self) -> Rect {
        match self {
            Self::Row { rect, .. }
            | Self::RowMenu { rect, .. }
            | Self::RowRelogin { rect, .. }
            | Self::MenuSurface { rect }
            | Self::MenuItem { rect, .. }
            | Self::AddChoice { rect, .. }
            | Self::Disclosure { rect, .. }
            | Self::Footer { rect, .. }
            | Self::Login { rect, .. }
            | Self::Quit { rect, .. }
            | Self::Dialog { rect, .. } => *rect,
        }
    }
}

/// Return targets in paint order. Hit testing should use the last matching
/// target so modal and menu targets take precedence over the flyout body.
pub fn layout_targets(rows: &[Row], expand_disabled: bool, state: Option<&str>) -> Vec<HitTarget> {
    const WIDTH: f32 = FLYOUT_WIDTH;
    const PADDING: f32 = 16.0;
    let mut targets = Vec::new();
    let login = state.is_some_and(|value| value.starts_with("login:"));
    let height = state_height(rows, expand_disabled, state);
    let mut y = 52.0;

    if !login {
        for provider in ["claude", "codex"] {
            let active = rows
                .iter()
                .filter(|r| r.provider == provider_name(provider) && r.user_chip != "비활성")
                .collect::<Vec<_>>();
            let disabled = rows
                .iter()
                .filter(|r| r.provider == provider_name(provider) && r.user_chip == "비활성")
                .collect::<Vec<_>>();
            if active.is_empty() && disabled.is_empty() {
                continue;
            }
            y += 36.0;
            let expanded = disabled_expanded(rows, expand_disabled, state, provider);
            if expanded && !disabled.is_empty() {
                targets.push(HitTarget::Disclosure {
                    provider: provider.into(),
                    rect: Rect::new(PADDING, y, WIDTH - PADDING, y + 28.0),
                });
                y += 28.0;
            }
            for row in active.into_iter().chain(if expanded {
                disabled.clone()
            } else {
                Vec::new()
            }) {
                targets.push(HitTarget::Row {
                    id: row.id.clone(),
                    rect: Rect::new(PADDING, y, WIDTH - PADDING, y + row_height(row)),
                });
                targets.push(HitTarget::RowMenu {
                    id: row.id.clone(),
                    rect: Rect::new(324.0, y + 10.0, 352.0, y + 34.0),
                });
                if row.reason_button && row.user_chip != "비활성" {
                    targets.push(HitTarget::RowRelogin {
                        id: row.id.clone(),
                        rect: Rect::new(
                            258.0,
                            y + row_height(row) - 32.0,
                            344.0,
                            y + row_height(row) - 8.0,
                        ),
                    });
                }
                y += row_height(row);
            }
            if !expanded && !disabled.is_empty() {
                targets.push(HitTarget::Disclosure {
                    provider: provider.into(),
                    rect: Rect::new(PADDING, y, WIDTH - PADDING, y + 28.0),
                });
                y += 28.0;
            }
        }
    } else {
        targets.push(HitTarget::Login {
            provider: if state.is_some_and(|v| v.contains("claude")) {
                "claude".into()
            } else {
                "codex".into()
            },
            action: LoginAction::CopyAddress,
            rect: Rect::new(16.0, 176.0, 176.0, 208.0),
        });
        targets.push(HitTarget::Login {
            provider: if state.is_some_and(|v| v.contains("claude")) {
                "claude".into()
            } else {
                "codex".into()
            },
            action: LoginAction::Cancel,
            rect: Rect::new(184.0, 176.0, 344.0, 208.0),
        });
        y = 228.0;
    }
    let footer_y = if login { y } else { height - 48.0 };
    for (action, left, right) in [
        (FooterAction::AddAccount, 88.0, 169.0),
        (FooterAction::Settings, 171.0, 225.0),
        (FooterAction::Logs, 227.0, 281.0),
        (FooterAction::Quit, 283.0, 337.0),
    ] {
        if action != FooterAction::AddAccount || !login {
            targets.push(HitTarget::Footer {
                action,
                rect: Rect::new(left, footer_y + 8.0, right, footer_y + 40.0),
            });
        }
    }
    if state.is_some_and(|v| v.starts_with("menu:")) {
        if let Some(((row, _), popup)) = menu_target(rows, state.unwrap_or_default()).zip(
            popup_rect(rows, expand_disabled, state.unwrap_or_default(), height),
        ) {
            // The opaque menu metadata and padding consume pointer hits too.
            targets.push(HitTarget::MenuSurface { rect: popup });
            for (index, _) in actions::menu_items(row).iter().enumerate() {
                targets.push(HitTarget::MenuItem {
                    id: row.id.clone(),
                    index,
                    rect: Rect::new(
                        popup.left,
                        popup.top + account_menu_item_top(row, index),
                        popup.right,
                        popup.top + account_menu_item_top(row, index) + 32.0,
                    ),
                });
            }
        }
    } else if state == Some("add-menu") {
        let popup = popup_rect(rows, expand_disabled, "add-menu", height).unwrap();
        targets.push(HitTarget::MenuSurface { rect: popup });
        for (index, provider) in [Some("claude"), Some("codex"), None]
            .into_iter()
            .enumerate()
        {
            let top = popup.top + 36.0 + index as f32 * 27.0;
            targets.push(HitTarget::AddChoice {
                provider: provider.map(str::to_owned),
                rect: Rect::new(popup.left, top, popup.right, top + 27.0),
            });
        }
    } else if state.is_some_and(|v| v.starts_with("confirm-delete:")) {
        let top = (height - 193.0).round() / 2.0;
        // A modal consumes hits outside its buttons instead of activating rows below.
        targets.push(HitTarget::MenuSurface {
            rect: Rect::new(0.0, 0.0, WIDTH, height),
        });
        targets.push(HitTarget::Dialog {
            action: DialogAction::ConfirmDelete,
            rect: Rect::new(16.0, top + 129.0, 180.0, top + 193.0),
        });
        targets.push(HitTarget::Dialog {
            action: DialogAction::Cancel,
            rect: Rect::new(180.0, top + 129.0, 344.0, top + 193.0),
        });
    } else if state == Some("quit") {
        let popup = popup_rect(rows, expand_disabled, "quit", height).unwrap();
        let top = popup.top;
        targets.push(HitTarget::MenuSurface { rect: popup });
        targets.push(HitTarget::Quit {
            action: QuitAction::TrayOnly,
            rect: Rect::new(popup.left, top + 36.0, popup.right, top + 63.0),
        });
        targets.push(HitTarget::Quit {
            action: QuitAction::TrayAndProxy,
            rect: Rect::new(popup.left, top + 63.0, popup.right, popup.bottom),
        });
    }
    targets
}

pub fn hit_test(targets: &[HitTarget], x: f32, y: f32) -> Option<&HitTarget> {
    targets
        .iter()
        .rev()
        .find(|target| target.rect().contains(x, y))
}

fn layout_content_height(rows: &[Row], expand_disabled: bool, state: Option<&str>) -> f32 {
    let mut height = 52.0;
    let mut sections = 0;
    for provider in ["Claude", "Codex"] {
        let active = rows
            .iter()
            .filter(|r| r.provider == provider && r.user_chip != "비활성")
            .count();
        let disabled = rows
            .iter()
            .filter(|r| r.provider == provider && r.user_chip == "비활성")
            .count();
        if active + disabled == 0 {
            continue;
        }
        sections += 1;
        height += 36.0
            + rows
                .iter()
                .filter(|r| r.provider == provider && r.user_chip != "비활성")
                .map(row_height)
                .sum::<f32>();
        if disabled_expanded(rows, expand_disabled, state, provider) {
            if disabled > 0 {
                height += 32.0;
            }
            height += rows
                .iter()
                .filter(|r| r.provider == provider && r.user_chip == "비활성")
                .map(row_height)
                .sum::<f32>();
            if disabled > 0 {
                height += 1.0;
            }
        } else if disabled > 0 {
            height += 32.0;
        }
    }
    height + sections as f32 + 3.0 + 8.0 + FOOTER_HEIGHT
}

pub fn state_height(rows: &[Row], expand_disabled: bool, state: Option<&str>) -> f32 {
    if state.is_some_and(|value| value.starts_with("login:")) {
        return 276.0;
    }
    let base = layout_content_height(rows, expand_disabled, state);
    state
        .map(|value| popup_required_height(rows, expand_disabled, value, base))
        .unwrap_or(base)
}

pub fn parse_state(json: &str) -> serde_json::Result<Vec<Row>> {
    parse_state_at(json, SystemTime::now())
}

pub fn parse_state_at(json: &str, now: SystemTime) -> serde_json::Result<Vec<Row>> {
    let state: State = serde_json::from_str(json)?;
    let view_input: std::sync::Arc<str> = json.into();
    Ok(state
        .accounts
        .iter()
        .map(|account| {
            let mut row = row(account, now);
            row.view_input = Some(view_input.clone());
            if account.user_state == "active"
                && let Some(notice) = account
                    .quota_notice
                    .as_ref()
                    .filter(|q| q.current_since(now, account.last_relevant_state_at))
            {
                let next = notice.next_account.as_deref().map(|id| {
                    state
                        .accounts
                        .iter()
                        .find(|a| a.id == id && a.provider == account.provider)
                        .map_or_else(
                            || notice_account_name(None, None, id),
                            |a| notice_account_name(a.email.as_deref(), a.label.as_deref(), id),
                        )
                });
                if let Some(message) =
                    quota_message(notice, "", &account.provider, next.as_deref(), None, now)
                {
                    row.health_chip = message.short;
                    row.health_tone = Tone::Warning;
                    row.attention = true;
                }
            }
            row
        })
        .collect())
}

pub fn row(account: &Account, now: SystemTime) -> Row {
    let email = account.email.clone();
    let name = email
        .clone()
        .or_else(|| account.label.clone())
        .unwrap_or_else(|| "이름 없는 계정".into());
    let health_chip = health_chip(account, now);
    let pacing = account
        .pacing
        .backoff_secs
        .is_some_and(|seconds| seconds > 0);
    let health_tone = if account.probe_pending
        || account.drain
        || account.refresh_attention
        || pacing
        || account
            .quota_notice
            .as_ref()
            .is_some_and(|notice| notice.current_since(now, account.last_relevant_state_at))
    {
        Tone::Warning
    } else {
        health_tone(account)
    };
    let windows = account
        .windows
        .iter()
        .filter(|window| drawable_window_name(&window.name))
        .collect::<Vec<_>>();
    let five_window = windows
        .iter()
        .find(|w| w.name == "5h")
        .copied()
        .or_else(|| windows.first().copied());
    let weekly_window = windows
        .iter()
        .find(|w| w.name == "7d")
        .copied()
        .or_else(|| {
            five_window
                .filter(|window| window.name == "5h")
                .and_then(|_| windows.iter().find(|w| w.name != "5h").copied())
        });
    let meter_windows = windows
        .iter()
        .map(|window| usage(Some(*window), now))
        .collect::<Vec<_>>();
    Row {
        view_input: None,
        id: account.id.clone(),
        provider: provider_name(&account.provider).into(),
        name,
        email,
        plan: account.plan.clone(),
        user_chip: if account.user_state == "active" && account.bound_sessions > 0 {
            "사용 중".into()
        } else {
            user_chip(&account.user_state)
        },
        health_chip,
        health_tone,
        five_hour: usage(five_window, now),
        seven_day: usage(weekly_window, now),
        meter_windows,
        reason_button: account.source != "linked" && account.health.kind == "needs_relogin",
        attention: account.refresh_attention
            || account.probe_pending
            || account.drain
            || pacing
            || health_tone == Tone::Warning
            || health_tone == Tone::Critical,
        source: account.source.clone(),
        token_expires_at: account.token_expires_at.clone(),
        last_refresh_at: account.last_refresh_at.clone(),
        allow_credits: account.allow_credits,
        drain: account.drain,
        bound_sessions: account.bound_sessions,
        session_text: (account.spread_threshold > 0).then(|| {
            format!(
                "세션 {}/{}",
                account.active_sessions, account.spread_threshold
            )
        }),
        pacing_until: account
            .pacing
            .backoff_secs
            .map(|seconds| seconds.to_string()),
    }
}

pub fn row_height(row: &Row) -> f32 {
    if row.user_chip == "비활성" {
        return 44.0;
    }
    let has_windows = !row.meter_windows.is_empty();
    let has_reason = row.health_tone != Tone::Good;
    10.0 + 24.0
        + if has_windows { 6.0 + 42.0 } else { 0.0 }
        + if has_reason {
            8.0 + if row.reason_button { 24.0 } else { 16.0 }
        } else {
            0.0
        }
        + 10.0
}

pub fn user_chip(state: &str) -> String {
    match state {
        "active" => "활성",
        "paused" => "일시정지",
        "disabled" => "비활성",
        _ => "알 수 없음",
    }
    .into()
}

pub fn health_chip(account: &Account, now: SystemTime) -> String {
    if account.user_state == "disabled" {
        return "비활성".into();
    }
    if account.user_state == "paused" {
        return "일시정지".into();
    }
    if let Some(notice) = account
        .quota_notice
        .as_ref()
        .filter(|q| q.current_since(now, account.last_relevant_state_at))
    {
        let next = notice
            .next_account
            .as_deref()
            .map(|id| notice_account_name(None, None, id));
        if let Some(message) =
            quota_message(notice, "", &account.provider, next.as_deref(), None, now)
        {
            return message.short;
        }
    }
    if account.probe_pending {
        return "인증 확인 대기".into();
    }
    if account.drain {
        return "전환 대기".into();
    }
    if let Some(seconds) = account.pacing.backoff_secs.filter(|seconds| *seconds > 0) {
        return format!("속도 조절 중 · {seconds}초");
    }
    match account.health.kind.as_str() {
        "ready" => "준비됨".into(),
        "cooling" => {
            let label = match account.health.reason.as_deref() {
                Some("quota") => "한도",
                Some("rate") | Some("auth") | Some("link_stale") => "대기",
                Some("permission") | Some("payment") | Some("store_error") => "확인 필요",
                _ => "대기",
            };
            account.health.until.as_deref().map_or_else(
                || label.into(),
                |until| format!("{label} · {}", short_time(until, now)),
            )
        }
        "needs_relogin" => "재로그인 필요".into(),
        _ => "알 수 없음".into(),
    }
}

fn short_time(value: &str, now: SystemTime) -> String {
    let rendered = format_reset(value, now);
    rendered
        .strip_suffix(" 초기화")
        .unwrap_or(&rendered)
        .to_string()
}

pub fn health_tone(account: &Account) -> Tone {
    if account.user_state != "active" {
        return Tone::Neutral;
    }
    match account.health.kind.as_str() {
        "ready" => Tone::Good,
        "cooling" if account.health.reason.as_deref() == Some("store_error") => Tone::Critical,
        "cooling" => Tone::Warning,
        "needs_relogin" => Tone::Critical,
        _ => Tone::Neutral,
    }
}

pub fn provider_name(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        _ => provider,
    }
}

fn usage(window: Option<&Window>, now: SystemTime) -> Usage {
    let Some(window) = window else {
        return Usage {
            label: String::new(),
            percent: None,
            percent_text: "–".into(),
            reset_text: "–".into(),
        };
    };
    let expired = window
        .reset_at
        .as_deref()
        .and_then(parse_rfc3339)
        .is_some_and(|t| t <= now);
    let percent = (!expired)
        .then_some(window.used_pct)
        .flatten()
        .filter(|p| p.is_finite() && *p >= 0.0);
    Usage {
        label: window_label(&window.name),
        percent,
        percent_text: percent.map_or_else(
            || {
                if expired {
                    "리셋됨".into()
                } else {
                    "–".into()
                }
            },
            |p| format!("{p:.0}%"),
        ),
        reset_text: window
            .reset_at
            .as_deref()
            .map_or_else(|| "–".into(), |s| format_reset(s, now)),
    }
}

pub fn format_reset(value: &str, now: SystemTime) -> String {
    let Some(at) = parse_rfc3339(value) else {
        return value.into();
    };
    if at <= now {
        return "리셋됨".into();
    }
    let remaining = at.duration_since(now).unwrap_or_default();
    let local = value
        .parse::<DateTime<FixedOffset>>()
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Local));
    if remaining > Duration::from_secs(24 * 3600)
        && let Some(local) = local
    {
        return format!(
            "{}/{} {:02}:{:02} 초기화",
            local.month(),
            local.day(),
            local.hour(),
            local.minute()
        );
    }
    local.map_or_else(
        || value.into(),
        |local| format!("{} 초기화", local.format("%H:%M")),
    )
}

fn window_label(name: &str) -> String {
    match name {
        "5h" => "5시간".into(),
        "7d" => "주간".into(),
        value if value.ends_with('m') => value
            .trim_end_matches('m')
            .parse::<u64>()
            .map(|minutes| format!("{}일", minutes / (24 * 60)))
            .unwrap_or_else(|_| "기타".into()),
        _ => "기타".into(),
    }
}

fn drawable_window_name(name: &str) -> bool {
    match name {
        "5h" | "7d" | "7d_oi" | "7d_sonnet" => true,
        value if value.ends_with('m') => value
            .trim_end_matches('m')
            .parse::<u64>()
            .is_ok_and(|minutes| minutes > 0),
        _ => false,
    }
}

pub fn tooltip(rows: &[Row], core_ok: bool) -> String {
    let badge = match badge(rows, core_ok) {
        Badge::Error => "코어 연결 끊김",
        Badge::Attention => "주의 필요",
        Badge::Normal => "정상",
    };
    let text = format!("quota-baton · {badge} · 계정 {}개", rows.len());
    text.chars().take(127).collect()
}

pub fn badge(rows: &[Row], core_ok: bool) -> Badge {
    if !core_ok {
        Badge::Error
    } else if rows.is_empty()
        || rows
            .iter()
            .any(|r| r.attention || r.health_tone == Tone::Critical)
        || !rows.iter().any(|r| r.health_tone == Tone::Good)
    {
        Badge::Attention
    } else {
        Badge::Normal
    }
}

pub fn badge_color(rows: &[Row], core_ok: bool, dark: bool) -> Rgb {
    match badge(rows, core_ok) {
        Badge::Normal => colors(dark).good,
        Badge::Attention => {
            if dark {
                Rgb(247, 194, 92)
            } else {
                colors(false).warning
            }
        }
        Badge::Error => {
            if dark {
                Rgb(255, 139, 139)
            } else {
                colors(false).critical
            }
        }
    }
}

pub fn colors(dark: bool) -> Colors {
    if dark {
        Colors {
            background: Rgb(30, 30, 30),
            surface: Rgb(45, 45, 48),
            text: Rgb(245, 245, 245),
            muted: Rgb(180, 180, 185),
            accent: Rgb(91, 156, 255),
            good: Rgb(74, 190, 110),
            warning: Rgb(235, 174, 58),
            critical: Rgb(235, 83, 83),
            track: Rgb(75, 75, 80),
            divider: Rgb(46, 46, 46),
        }
    } else {
        Colors {
            background: Rgb(248, 249, 251),
            surface: Rgb(255, 255, 255),
            text: Rgb(28, 31, 36),
            muted: Rgb(99, 106, 116),
            accent: Rgb(35, 104, 210),
            good: Rgb(30, 145, 73),
            warning: Rgb(174, 108, 0),
            critical: Rgb(190, 45, 45),
            track: Rgb(224, 228, 234),
            divider: Rgb(236, 236, 236),
        }
    }
}

pub fn chip_colors(dark: bool, tone: Tone, inactive: bool) -> (Rgb, Rgb) {
    if dark {
        return if inactive {
            (Rgb(70, 70, 76), Rgb(220, 220, 226))
        } else {
            match tone {
                Tone::Good => (Rgb(35, 76, 52), Rgb(132, 225, 160)),
                Tone::Warning => (Rgb(87, 68, 29), Rgb(247, 194, 92)),
                Tone::Critical => (Rgb(91, 42, 42), Rgb(255, 139, 139)),
                Tone::Neutral => (Rgb(70, 70, 76), Rgb(220, 220, 226)),
            }
        };
    }
    if inactive {
        (Rgb(238, 240, 242), Rgb(99, 106, 116))
    } else {
        match tone {
            Tone::Good => (Rgb(231, 242, 235), Rgb(30, 145, 73)),
            Tone::Warning => (Rgb(250, 240, 223), Rgb(174, 108, 0)),
            Tone::Critical => (Rgb(247, 229, 228), Rgb(190, 45, 45)),
            Tone::Neutral => (Rgb(238, 240, 242), Rgb(99, 106, 116)),
        }
    }
}

fn parse_rfc3339(value: &str) -> Option<SystemTime> {
    // Reuse the core's chrono-backed timestamp without another dependency.
    let meta = qb_core::accounts::TokenMeta {
        expires_at: serde_json::from_value(serde_json::Value::String(value.into())).ok(),
        ..Default::default()
    };
    meta.expires_at.map(SystemTime::from)
}

#[cfg(test)]
#[derive(Debug, Default)]
struct SupervisorBackoff {
    failures: u32,
}

#[cfg(test)]
impl SupervisorBackoff {
    fn delay_seconds(&self) -> u64 {
        5_u64.saturating_mul(1_u64 << self.failures.min(6)).min(300)
    }

    fn next(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    fn reset(&mut self) {
        self.failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;
    const FIXTURE: &str = include_str!("../fixtures/m6a.1-state.json");
    fn rows() -> Vec<Row> {
        parse_state_at(
            FIXTURE,
            "2026-10-18T12:00:00Z"
                .parse::<DateTime<chrono::Utc>>()
                .unwrap()
                .into(),
        )
        .unwrap()
    }
    #[cfg(windows)]
    #[test]
    fn fixture_has_both_windows_and_chips() {
        let r = rows();
        assert_eq!(r.len(), 7);
        assert_eq!(r[0].five_hour.percent_text, "45%");
        assert_eq!(r[0].seven_day.percent_text, "76%");
        assert_eq!(r[1].health_chip, "재로그인 필요");
        assert_eq!(r[6].user_chip, "비활성");
    }
    #[cfg(windows)]
    #[test]
    fn reset_and_date_are_stable() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let expected_reset = "1/1 09:00";
        assert_eq!(
            format_reset("2027-01-01T00:00:00Z", now),
            format!("{expected_reset} 초기화")
        );
        assert_eq!(format_reset("2020-01-01T00:00:00Z", now), "리셋됨");
    }
    fn disclosure_rows() -> Vec<Row> {
        parse_state(
            r#"{"accounts":[
            {"id":"cl-active","provider":"claude","user_state":"active"},
            {"id":"cl-disabled","provider":"claude","user_state":"disabled","source":"oauth"},
            {"id":"cx-active","provider":"codex","user_state":"active"},
            {"id":"cx-disabled","provider":"codex","user_state":"disabled","source":"adopted"}
        ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn provider_disclosures_share_row_anchors_and_legacy_codex_expansion() {
        let rows = disclosure_rows();
        for (provider, id, other) in [
            ("claude", "cl-disabled", "cx-disabled"),
            ("codex", "cx-disabled", "cl-disabled"),
        ] {
            let collapsed = layout_targets(&rows, false, Some("list"));
            assert!(collapsed.iter().any(|target| matches!(target,
                HitTarget::Disclosure { provider: actual, .. } if actual == provider)));
            assert!(!collapsed.iter().any(|target| matches!(target,
                HitTarget::Row { id: actual, .. } if actual == id)));
            let state = format!("expanded:{provider}");
            // Explicit provider state takes precedence over the old bool.
            let targets = layout_targets(&rows, true, Some(&state));
            let anchor = row_anchor(&rows, true, Some(&state), id).unwrap();
            assert!(targets.iter().any(|target| matches!(target,
                HitTarget::Row { id: actual, rect } if actual == id && *rect == anchor)));
            assert!(!targets.iter().any(|target| matches!(target,
                HitTarget::Row { id: actual, .. } if actual == other)));
            let disclosure = targets
                .iter()
                .find(|target| {
                    matches!(target,
                HitTarget::Disclosure { provider: actual, .. } if actual == provider)
                })
                .unwrap()
                .rect();
            assert!(
                matches!(hit_test(&targets, disclosure.left + 1.0, disclosure.top + 1.0),
                Some(HitTarget::Disclosure { provider: actual, .. }) if actual == provider)
            );
            let height = state_height(&rows, true, Some(&state));
            assert!(anchor.bottom <= height - FOOTER_HEIGHT);
        }
        assert_eq!(
            layout_targets(&rows, true, None),
            layout_targets(&rows, false, Some("expanded"))
        );
        assert_eq!(
            state_height(&rows, true, None),
            state_height(&rows, false, Some("expanded"))
        );
    }

    #[test]
    fn disabled_account_menus_reveal_exact_row_and_fit_above_footer() {
        let rows = disclosure_rows();
        for id in ["cl-disabled", "cx-disabled"] {
            for count in [1, 2, 8] {
                let mut fixture = (0..count - 1)
                    .map(|n| {
                        let mut active = rows[if n % 2 == 0 { 0 } else { 2 }].clone();
                        active.id = format!("extra-{n}");
                        active
                    })
                    .collect::<Vec<_>>();
                let mut wanted = rows.iter().find(|row| row.id == id).unwrap().clone();
                // Even a disabled row needing re-login paints only its compact row.
                wanted.reason_button = true;
                fixture.push(wanted);
                assert_eq!(fixture.len(), count);
                let before = fixture.clone();
                let state = format!("menu:{id}:0");
                let (row, selected) = menu_target(&fixture, &state).unwrap();
                assert_eq!(row.id, id);
                assert_eq!(selected, 0);
                assert_eq!(row.user_chip, "비활성");
                assert_eq!(actions::menu_items(row)[0], actions::MenuItem::Enable);
                let height = state_height(&fixture, false, Some(&state));
                let popup = popup_rect(&fixture, false, &state, height).unwrap();
                assert!(popup.top >= 0.0 && popup.bottom <= height - FOOTER_HEIGHT);
                assert!(popup.left >= 0.0 && popup.right <= FLYOUT_WIDTH);
                let anchor = row_anchor(&fixture, false, Some(&state), id).unwrap();
                let targets = layout_targets(&fixture, false, Some(&state));
                assert!(!targets.iter().any(|target| matches!(target,
                    HitTarget::RowRelogin { id: actual, .. } if actual == id)));
                assert!(targets.iter().any(|target| matches!(target,
                    HitTarget::Row { id: actual, rect } if actual == id && *rect == anchor)));
                assert!(matches!(
                    hit_test(&targets, popup.left + 4.0, popup.top + 4.0),
                    Some(HitTarget::MenuSurface { .. })
                ));
                let mut items = 0;
                for target in &targets {
                    if let HitTarget::MenuItem {
                        id: actual,
                        index,
                        rect,
                    } = target
                    {
                        items += 1;
                        assert_eq!(actual, id);
                        assert!(rect.top >= popup.top && rect.bottom <= popup.bottom);
                        assert!(
                            matches!(hit_test(&targets, rect.left + 1.0, rect.top + 1.0),
                            Some(HitTarget::MenuItem { id: actual, index: actual_index, .. })
                            if actual == id && actual_index == index)
                        );
                    }
                }
                assert_eq!(items, actions::menu_items(row).len());
                assert_eq!(
                    fixture, before,
                    "opening a menu must not activate an account"
                );
            }
        }
    }

    #[test]
    fn removed_or_unknown_menu_selection_has_no_popup_or_menu_actions() {
        let mut rows = disclosure_rows();
        rows.retain(|row| row.id != "cl-disabled");
        for state in [
            "menu:cl-disabled:0",
            "menu:missing",
            "menu::0",
            "menu:cx-active:999",
            "menu:cx-active:bad",
            "menu:cx-active:0:extra",
            "menu:cx-active:0:",
            "menu:cx-active:pending",
            "menu:cx-active:0:pending:extra",
            "menu:cx-active:0:pending:",
            "menu:cx-active:0:pending:pending",
        ] {
            assert!(menu_target(&rows, state).is_none());
            let height = state_height(&rows, false, Some(state));
            assert!(popup_rect(&rows, false, state, height).is_none());
            assert_eq!(height, state_height(&rows, false, None));
            assert!(
                !layout_targets(&rows, false, Some(state))
                    .iter()
                    .any(|target| matches!(
                        target,
                        HitTarget::MenuSurface { .. } | HitTarget::MenuItem { .. }
                    ))
            );
        }
        assert!(menu_target(&[], "menu:missing:0").is_none());
    }

    #[test]
    fn pending_menu_selection_preserves_exact_row_index_and_layout() {
        let rows = disclosure_rows();
        for row in &rows {
            for index in 0..actions::menu_items(row).len() {
                let state = format!("menu:{}:{index}", row.id);
                let pending = format!("{state}:pending");
                assert_eq!(menu_target(&rows, &pending), Some((row, index)));
                assert_eq!(
                    layout_targets(&rows, false, Some(&pending)),
                    layout_targets(&rows, false, Some(&state))
                );
                assert_eq!(
                    state_height(&rows, false, Some(&pending)),
                    state_height(&rows, false, Some(&state))
                );
            }
        }
        assert!(menu_target(&rows, "menu:missing:0:pending").is_none());
        assert!(menu_target(&rows, "menu:cx-active:999:pending").is_none());
    }

    /// Exercise padding, icon/checkbox, text, right edge, and vertical insets.
    /// Shared horizontal row boundaries belong to the later painted item.
    fn assert_full_item_hits(targets: &[HitTarget], target: &HitTarget) {
        let rect = target.rect();
        let middle = (rect.top + rect.bottom) / 2.0;
        for (x, y) in [
            (rect.left, middle),
            (rect.left + 1.0, middle),
            (rect.left + 8.0, middle),
            (rect.left + 22.0, middle),
            ((rect.left + rect.right) / 2.0, middle),
            (rect.right - 1.0, middle),
            (rect.right - 0.01, middle),
            (rect.left + 1.0, rect.top + 0.25),
            (rect.right - 1.0, rect.bottom - 0.25),
        ] {
            assert_eq!(hit_test(targets, x, y), Some(target), "at ({x}, {y})");
        }
    }

    #[test]
    fn every_account_menu_item_hits_full_width_including_checkbox_and_padding() {
        let mut rows = rows();
        rows.extend(disclosure_rows().into_iter().map(|mut row| {
            row.id = format!("disclosure-{}", row.id);
            row
        }));
        let mut paused = rows[0].clone();
        paused.id = "paused".into();
        paused.user_chip = "일시정지".into();
        rows.push(paused);
        for row in &rows {
            for pending in [false, true] {
                let state = format!("menu:{}:0{}", row.id, if pending { ":pending" } else { "" });
                let targets = layout_targets(&rows, false, Some(&state));
                let popup = popup_rect(
                    &rows,
                    false,
                    &state,
                    state_height(&rows, false, Some(&state)),
                )
                .unwrap();
                let items = targets
                    .iter()
                    .filter(|target| matches!(target, HitTarget::MenuItem { .. }));
                assert_eq!(items.clone().count(), actions::menu_items(row).len());
                for target in items {
                    let rect = target.rect();
                    assert_eq!((rect.left, rect.right), (popup.left, popup.right));
                    assert_eq!(rect.bottom - rect.top, 32.0);
                    assert_full_item_hits(&targets, target);
                }
            }
        }
    }

    #[test]
    fn empty_account_footer_popups_and_every_item_fit_final_window() {
        for state in ["add-menu", "quit"] {
            let height = state_height(&[], false, Some(state));
            let popup = popup_rect(&[], false, state, height).unwrap();
            assert!(popup.top >= 0.0 && popup.bottom <= height - FOOTER_HEIGHT);
            let targets = layout_targets(&[], false, Some(state));
            let items: Vec<_> = targets
                .iter()
                .filter(|t| matches!(t, HitTarget::AddChoice { .. } | HitTarget::Quit { .. }))
                .collect();
            assert_eq!(items.len(), if state == "add-menu" { 3 } else { 2 });
            for item in items {
                let rect = item.rect();
                assert!(rect.top >= 0.0 && rect.bottom <= height);
                assert_full_item_hits(&targets, item);
            }
        }
    }

    #[test]
    fn add_and_quit_items_hit_full_popup_width_without_header_clickthrough() {
        let rows = disclosure_rows();
        for state in ["add-menu", "quit"] {
            let targets = layout_targets(&rows, false, Some(state));
            let popup =
                popup_rect(&rows, false, state, state_height(&rows, false, Some(state))).unwrap();
            assert!(matches!(
                hit_test(&targets, popup.left + 1.0, popup.top + 1.0),
                Some(HitTarget::MenuSurface { .. })
            ));
            let items = targets.iter().filter(|target| {
                matches!(target, HitTarget::AddChoice { .. } | HitTarget::Quit { .. })
            });
            assert_eq!(
                items.clone().count(),
                if state == "add-menu" { 3 } else { 2 }
            );
            for (index, target) in items.enumerate() {
                let rect = target.rect();
                assert_eq!((rect.left, rect.right), (popup.left, popup.right));
                assert!(rect.top >= popup.top && rect.bottom <= popup.bottom);
                assert_full_item_hits(&targets, target);
                // Sample the actual rendered label row, not only the declared target.
                assert_eq!(
                    hit_test(
                        &targets,
                        popup.left + 16.0,
                        popup.top + 38.0 + index as f32 * 27.0
                    ),
                    Some(target)
                );
                if let HitTarget::AddChoice { provider, .. } = target {
                    assert_eq!(
                        provider.as_deref(),
                        [Some("claude"), Some("codex"), None][index]
                    );
                }
            }
        }
    }

    #[test]
    fn footer_disclosure_and_delete_buttons_hit_whole_controls() {
        let rows = disclosure_rows();
        for state in [
            "list",
            "expanded:claude",
            "expanded:codex",
            "login:codex",
            "confirm-delete:cx-active",
        ] {
            let targets = layout_targets(&rows, false, Some(state));
            let mut footers = 0;
            let mut disclosures = 0;
            let mut buttons = 0;
            for target in &targets {
                match target {
                    HitTarget::Footer { .. } if !state.starts_with("confirm-delete:") => {
                        footers += 1;
                        assert_full_item_hits(&targets, target);
                    }
                    HitTarget::Disclosure { rect, .. } if !state.starts_with("confirm-delete:") => {
                        disclosures += 1;
                        assert_eq!((rect.left, rect.right), (16.0, FLYOUT_WIDTH - 16.0));
                        assert_full_item_hits(&targets, target);
                    }
                    HitTarget::Dialog { .. } => {
                        buttons += 1;
                        assert_full_item_hits(&targets, target);
                    }
                    _ => {}
                }
            }
            if state.starts_with("confirm-delete:") {
                assert_eq!(buttons, 2);
                assert!(matches!(
                    hit_test(&targets, 20.0, 20.0),
                    Some(HitTarget::MenuSurface { .. })
                ));
            } else {
                assert_eq!(footers, if state.starts_with("login:") { 3 } else { 4 });
                assert_eq!(disclosures, if state.starts_with("login:") { 0 } else { 2 });
            }
        }
    }

    #[test]
    fn tooltip_and_badge_are_bounded() {
        let r = rows();
        assert!(tooltip(&r, true).chars().count() < 128);
        assert_eq!(badge(&r, true), Badge::Attention);
        assert_eq!(badge(&r, false), Badge::Error);
    }
    #[test]
    fn healthy_in_use_accounts_keep_normal_badge_and_paused_accounts_do_not() {
        for bound in [0, 3] {
            let value = serde_json::json!({"accounts":[{"id":"synthetic-badge", "provider":"codex",
                "user_state":"active", "health":{"kind":"ready"}, "bound_sessions":bound}]});
            let rows = parse_state(&value.to_string()).unwrap();
            assert_eq!(
                rows[0].user_chip,
                if bound == 0 { "활성" } else { "사용 중" }
            );
            assert_eq!(badge(&rows, true), Badge::Normal);
            let mut paused = value;
            paused["accounts"][0]["user_state"] = "paused".into();
            assert_eq!(
                badge(&parse_state(&paused.to_string()).unwrap(), true),
                Badge::Attention
            );
        }
    }

    #[test]
    fn colors_differ() {
        assert_ne!(colors(false).background, colors(true).background);
    }

    #[test]
    fn source_object_decodes_without_discarding_account() {
        let rows = parse_state(
            r#"{
          "accounts":[{"id":"a","provider":"codex","source":{"kind":"oauth","extra":true}}]
        }"#,
        )
        .unwrap();
        assert_eq!(rows[0].source, "oauth");
    }

    #[test]
    fn adoption_metadata_and_manual_action_survive_decode_and_popup_growth() {
        let rows = parse_state(r#"{"accounts":[
            {"id":"adopted","provider":"codex","user_state":"active","source":{"kind":"cpa","mode":"adopted"},
             "token_expires_at":"2099-10-23T02:00:00Z","last_refresh_at":"2026-10-18T08:00:00Z"},
            {"id":"linked","provider":"claude","source":{"kind":"cpa","mode":"linked"}},
            {"id":"unknown","provider":"codex","source":{"kind":"cpa","mode":"future"}}
        ]}"#).unwrap();
        assert_eq!(rows[0].source, "adopted");
        assert_eq!(account_menu_metadata(&rows[0])[0], "직접 갱신");
        assert!(account_menu_metadata(&rows[0])[1].starts_with("만료 10/"));
        assert!(account_menu_metadata(&rows[0])[2].starts_with("최근 갱신 10/"));
        assert!(actions::menu_items(&rows[0]).contains(&actions::MenuItem::RefreshTokens));
        assert_eq!(account_menu_metadata(&rows[1])[0], "CPA 갱신");
        assert!(!actions::menu_items(&rows[1]).contains(&actions::MenuItem::RefreshTokens));
        assert_eq!(account_menu_metadata(&rows[2])[0], "갱신 방식 확인 불가");
        assert!(!actions::menu_items(&rows[2]).contains(&actions::MenuItem::RefreshTokens));
        for count in [1, 8] {
            let list = (0..count)
                .map(|n| {
                    let mut row = rows[0].clone();
                    row.id = format!("row-{n}");
                    row
                })
                .collect::<Vec<_>>();
            let row = list.last().unwrap();
            let state = format!("menu:{}:0", row.id);
            let height = state_height(&list, false, Some(&state));
            let popup = popup_rect(&list, false, &state, height).unwrap();
            assert!(popup.top >= 0.0 && popup.bottom <= height);
            assert!(
                account_menu_item_top(row, 0) >= 64.0,
                "metadata and first action overlap"
            );
            let last = actions::menu_items(row).len() - 1;
            assert!(popup.top + account_menu_item_top(row, last) + 32.0 <= popup.bottom);
        }
    }

    #[test]
    fn phantom_codex_windows_are_not_drawn() {
        let rows = parse_state(
            r#"{"accounts":[{"id":"cx","provider":"codex","windows":[
                {"name":"0m","used_pct":0,"reset_at":"2020-01-01T00:00:00Z"},
                {"name":"secondary","used_pct":0,"reset_at":"2020-01-01T00:00:00Z"}
            ]}]}"#,
        )
        .unwrap();
        assert!(rows[0].meter_windows.is_empty());
        assert_eq!(rows[0].five_hour.percent_text, "–");
    }

    #[test]
    fn hit_testing_prefers_menu_overlay_and_exposes_footer_targets() {
        let rows = rows();
        let targets = layout_targets(&rows, false, Some("menu:cx-ready"));
        let menu = popup_rect(
            &rows,
            false,
            "menu:cx-ready",
            state_height(&rows, false, Some("menu:cx-ready")),
        )
        .unwrap();
        assert!(matches!(
            hit_test(&targets, menu.left + 50.0,
                menu.top + account_menu_item_top(rows.iter().find(|row| row.id == "cx-ready").unwrap(), 0) + 15.0),
            Some(HitTarget::MenuItem { id, index: 0, .. }) if id == "cx-ready"
        ));
        assert!(matches!(
            hit_test(&targets, menu.left + 50.0, menu.top + 20.0),
            Some(HitTarget::MenuSurface { .. })
        ));
        assert!(targets.iter().any(|target| matches!(
            target,
            HitTarget::Footer {
                action: FooterAction::Quit,
                ..
            }
        )));
        let login = layout_targets(&rows, false, Some("login:codex"));
        assert!(matches!(
            hit_test(&login, 20.0, 180.0),
            Some(HitTarget::Login {
                action: LoginAction::CopyAddress,
                ..
            })
        ));
    }

    #[test]
    fn popups_fit_one_two_and_eight_row_lists_and_footer_menus_open_up() {
        let base = rows();
        for count in [1usize, 2, 8] {
            let mut fixture = Vec::new();
            for index in 0..count {
                let mut row = base[index % base.len()].clone();
                row.id = format!("row-{index}");
                row.provider = "Claude".into();
                fixture.push(row);
            }
            let list_height = state_height(&fixture, false, None);
            let menu_state = format!("menu:{}", fixture.last().unwrap().id);
            let menu_height = state_height(&fixture, false, Some(&menu_state));
            let popup = popup_rect(&fixture, false, &menu_state, menu_height).unwrap();
            assert!(popup.left >= 0.0 && popup.top >= 0.0);
            assert!(popup.right <= FLYOUT_WIDTH && popup.bottom <= menu_height - FOOTER_HEIGHT);
            let menu_targets = layout_targets(&fixture, false, Some(&menu_state));
            let last_item_bottom = menu_targets
                .iter()
                .filter_map(|target| match target {
                    HitTarget::MenuItem { rect, .. } => Some(rect.bottom),
                    _ => None,
                })
                .fold(0.0, f32::max);
            assert!(last_item_bottom + 16.0 <= menu_height);
            let quit_height = state_height(&fixture, false, Some("quit"));
            let quit = popup_rect(&fixture, false, "quit", quit_height).unwrap();
            assert!(quit.top >= 0.0 && quit.bottom <= quit_height);
            assert!(quit.bottom <= list_height - FOOTER_HEIGHT);
            assert!(quit.top < list_height - FOOTER_HEIGHT);
        }
    }

    #[test]
    fn dark_chip_tokens_cover_every_style() {
        for tone in [Tone::Good, Tone::Warning, Tone::Critical, Tone::Neutral] {
            assert_ne!(
                chip_colors(true, tone, false),
                chip_colors(false, tone, false)
            );
        }
        assert_ne!(
            chip_colors(true, Tone::Neutral, true),
            chip_colors(false, Tone::Neutral, true)
        );
    }

    #[cfg(windows)]
    #[test]
    fn reference_fixture_keeps_compatibility_rows_and_uses_v2_flyout_heights() {
        let rows = rows();
        assert_eq!(row_height(&rows[0]), 92.0);
        assert_eq!(row_height(&rows[1]), 124.0);
        assert_eq!(row_height(&rows[3]), 92.0);
        assert_eq!(row_height(&rows[4]), 116.0);
        assert_eq!(row_height(&rows[5]), 92.0);
        // V2: header105 + provider sections294/289 + list padding8 + footer48.
        // The first section gained three account-window lines (26 DIP each).
        // Expanding both disclosures adds two44-DIP rows.
        assert_eq!(native_render::content_height(&rows, false), 744.0);
        assert_eq!(native_render::content_height(&rows, true), 832.0);
        let mut pacing = rows.clone();
        pacing[3].health_tone = Tone::Warning;
        pacing[3].health_chip = "속도 조절 중 · 10초".into();
        // A translated compatibility chip cannot fabricate core pacing evidence.
        assert_eq!(native_render::content_height(&pacing, false), 744.0);
        let mut source: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        source["accounts"][3]["pacing"] = serde_json::json!({
            "backoff_until":"2099-01-01T00:00:00Z", "waiting":1
        });
        let pacing = parse_state(&source.to_string()).unwrap();
        // A real pacing reason adds eight DIPs of gap and a16-DIP line.
        assert_eq!(native_render::content_height(&pacing, false), 768.0);
    }

    #[test]
    fn supervisor_backoff_is_bounded_and_resettable() {
        let mut backoff = SupervisorBackoff::default();
        assert_eq!(backoff.delay_seconds(), 5);
        for _ in 0..10 {
            backoff.next();
        }
        assert_eq!(backoff.delay_seconds(), 300);
        backoff.reset();
        assert_eq!(backoff.delay_seconds(), 5);
    }
}
