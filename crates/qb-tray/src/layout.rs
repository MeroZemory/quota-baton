//! Pure tray layout model. Rendering is implemented by the shared Windows
//! Direct2D/DirectWrite backend in `native_render.rs`.
//!
//! This module intentionally knows nothing about Win32, HTTP, or credentials.  The
//! JSON shape is the read-only `/admin/v1/state` shape, so fixture and UI tests can
//! exercise it without starting the core.

use chrono::{DateTime, Datelike, FixedOffset, Local, Timelike};
use serde::Deserialize;
use std::time::{Duration, SystemTime};

pub mod actions;

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
    pub source: String,
    #[serde(default)]
    pub pacing: Pacing,
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
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
    pub pacing_until: Option<String>,
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

pub fn parse_state(json: &str) -> serde_json::Result<Vec<Row>> {
    parse_state_at(json, SystemTime::now())
}

pub fn parse_state_at(json: &str, now: SystemTime) -> serde_json::Result<Vec<Row>> {
    let state: State = serde_json::from_str(json)?;
    Ok(state
        .accounts
        .iter()
        .map(|account| row(account, now))
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
    let health_tone =
        if account.probe_pending || account.drain || account.refresh_attention || pacing {
            Tone::Warning
        } else {
            health_tone(account)
        };
    let five_window = account
        .windows
        .iter()
        .find(|w| w.name == "5h")
        .or_else(|| account.windows.first());
    let weekly_window = account.windows.iter().find(|w| w.name == "7d").or_else(|| {
        five_window
            .filter(|window| window.name == "5h")
            .and_then(|_| account.windows.iter().find(|w| w.name != "5h"))
    });
    let meter_windows = account
        .windows
        .iter()
        .map(|window| usage(Some(window), now))
        .collect::<Vec<_>>();
    Row {
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
        allow_credits: account.allow_credits,
        drain: account.drain,
        bound_sessions: account.bound_sessions,
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
        || !rows
            .iter()
            .any(|r| r.user_chip == "활성" && r.health_tone == Tone::Good)
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
        parse_state(FIXTURE).unwrap()
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
    #[test]
    fn tooltip_and_badge_are_bounded() {
        let r = rows();
        assert!(tooltip(&r, true).chars().count() < 128);
        assert_eq!(badge(&r, true), Badge::Attention);
        assert_eq!(badge(&r, false), Badge::Error);
    }
    #[test]
    fn colors_differ() {
        assert_ne!(colors(false).background, colors(true).background);
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
    fn reference_fixture_uses_specified_row_and_flyout_heights() {
        let rows = rows();
        assert_eq!(row_height(&rows[0]), 92.0);
        assert_eq!(row_height(&rows[1]), 124.0);
        assert_eq!(row_height(&rows[3]), 92.0);
        assert_eq!(row_height(&rows[4]), 116.0);
        assert_eq!(row_height(&rows[5]), 92.0);
        assert_eq!(native_render::content_height(&rows, false), 765.0);
        assert_eq!(native_render::content_height(&rows, true), 810.0);
        let mut pacing = rows.clone();
        pacing[3].health_tone = Tone::Warning;
        pacing[3].health_chip = "속도 조절 중 · 10초".into();
        assert_eq!(native_render::content_height(&pacing, false), 789.0);
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
