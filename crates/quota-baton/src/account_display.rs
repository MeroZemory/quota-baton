//! CLI compatibility display projection, extracted verbatim from tray v2.
//! Keep parsing and forecast visibility identical on Windows and Unix.
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};
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
    pub usage_forecast: Option<qb_core::usage_forecast::AccountUsageForecast>,
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
    /// Read-only core status, independent of health and credential ownership.
    #[serde(default)]
    pub outside_used: bool,
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
fn account_cooling(a: &Account) -> bool {
    a.health.kind == "cooling" && a.health.scope.as_deref() == Some("account")
}

fn quota(a: &Account) -> bool {
    account_cooling(a) && a.health.reason.as_deref() == Some("quota")
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

pub fn summary_forecast(
    a: &Account,
    now: DateTime<Utc>,
) -> qb_core::usage_forecast::AccountUsageForecast {
    let mut forecast =
        a.usage_forecast
            .clone()
            .unwrap_or_else(|| qb_core::usage_forecast::AccountUsageForecast {
                as_of: now,
                observed_at: None,
                windows: Vec::new(),
                earliest_window: None,
                credit: None,
                truncated: false,
            });
    if a.user_state != "active"
        || a.health.kind == "needs_relogin"
        || forecast.as_of > now
        || matches!(
            display_state(a),
            DisplayState::LinkStale
                | DisplayState::StoreError
                | DisplayState::Payment
                | DisplayState::Permission
                | DisplayState::Auth
        )
    {
        forecast.windows.clear();
        forecast.credit = None;
        forecast.observed_at = None;
    } else {
        forecast.windows.retain(|window| {
            a.windows
                .iter()
                .find(|w| w.name == window.name)
                .map_or_else(
                    || {
                        account_window(&Window {
                            name: window.name.clone(),
                            ..Window::default()
                        })
                    },
                    account_window,
                )
        });
    }
    forecast.as_of = now;
    forecast
}
