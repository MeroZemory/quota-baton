use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    pub routing: RoutingConfig,
    pub guard: GuardConfig,
    pub tray: TrayConfig,
    pub websocket: WebsocketConfig,
    pub quota: QuotaConfig,
    pub dashboard: DashboardConfig,
    pub ui: UiConfig,
    pub routing_replay: RoutingReplayConfig,
    pub accounts: BTreeMap<String, AccountConfig>,
}

/// Body-free owner metadata only; explicit operator opt-in, never enabled by a client.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RoutingReplayConfig {
    pub enabled: bool,
    pub disk_limit_mib: u64,
}
impl Default for RoutingReplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            disk_limit_mib: 256,
        }
    }
}
impl RoutingReplayConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (16..=512).contains(&self.disk_limit_mib),
            "routing replay disk limit must be between 16 and 512 MiB"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct UiConfig {
    pub language: UiLanguage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UiLanguage {
    #[default]
    Ko,
    En,
}

impl<'de> Deserialize<'de> for UiLanguage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(if value.as_str() == Some("en") {
            Self::En
        } else {
            Self::Ko
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DashboardConfig {
    pub resolve_claude_project: bool,
}
impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            resolve_claude_project: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TrayConfig {
    pub theme: TrayTheme,
    pub burst_new_sessions: u32,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            theme: TrayTheme::Light,
            burst_new_sessions: 30,
        }
    }
}

/// Flyout v2 defaults to light; system theme is an explicit opt-in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrayTheme {
    #[default]
    Light,
    Dark,
    System,
}

impl<'de> Deserialize<'de> for TrayTheme {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            "system" => Self::System,
            _ => {
                static WARN: std::sync::Once = std::sync::Once::new();
                WARN.call_once(|| {
                    tracing::warn!(
                        code = "unknown_tray_theme",
                        "Unknown tray theme; using light"
                    )
                });
                Self::Light
            }
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct UpstreamConfig {
    pub anthropic_base: String,
    pub codex_base: String,
    pub claude_usage_base: String,
    pub codex_usage_base: String,
    #[cfg(debug_assertions)]
    pub claude_token_endpoint: Option<String>,
    #[cfg(debug_assertions)]
    pub codex_token_endpoint: Option<String>,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            anthropic_base: "https://api.anthropic.com".into(),
            codex_base: "https://chatgpt.com/backend-api/codex".into(),
            claude_usage_base: "https://api.anthropic.com".into(),
            codex_usage_base: "https://chatgpt.com".into(),
            #[cfg(debug_assertions)]
            claude_token_endpoint: None,
            #[cfg(debug_assertions)]
            codex_token_endpoint: None,
        }
    }
}

impl UpstreamConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        #[cfg(debug_assertions)]
        for value in [&self.claude_token_endpoint, &self.codex_token_endpoint]
            .into_iter()
            .flatten()
        {
            let url = reqwest::Url::parse(value)?;
            anyhow::ensure!(
                url.scheme() == "http"
                    && url.host_str() == Some("127.0.0.1")
                    && url.port().is_some()
                    && !matches!(url.port(), Some(10100 | 10101))
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "invalid synthetic token endpoint"
            );
        }
        for (value, expected) in [
            (&self.anthropic_base, "https://api.anthropic.com"),
            (&self.codex_base, "https://chatgpt.com/backend-api/codex"),
            (&self.claude_usage_base, "https://api.anthropic.com"),
            (&self.codex_usage_base, "https://chatgpt.com"),
        ] {
            if value == expected {
                continue;
            }
            #[cfg(any(test, feature = "testkit"))]
            {
                let url = reqwest::Url::parse(value)
                    .map_err(|_| anyhow::anyhow!("invalid upstream base"))?;
                if url.scheme() == "http"
                    && url.host_str() == Some("127.0.0.1")
                    && url.port().is_some()
                    && !matches!(url.port(), Some(10100 | 10101))
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.path() == "/"
                {
                    continue;
                }
            }
            anyhow::bail!("upstream base is not allowed in this build");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    pub data_port: u16,
    pub admin_port: u16,
    pub drain_timeout_secs: u64,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            data_port: 10200,
            admin_port: 10201,
            drain_timeout_secs: 30 * 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RoutingConfig {
    pub mode: crate::routing::Mode,
    pub churn: crate::routing::ChurnLimits,
    pub move_guard: crate::bindings::MoveGuardLimits,
    pub gate_5h_pct: f64,
    pub gate_weekly_pct: f64,
    pub gate_weekly_near_reset_pct: f64,
    pub near_reset_secs: u64,
    pub inflight_allowance_pct: f64,
    pub placement_allowance_pct: f64,
    pub weak_evidence_new_sessions: usize,
    pub placement_poll_after_secs: u64,
    pub placement_poll_min_interval_secs: u64,
    pub reset_hold_secs: u64,
    pub request_timeout_secs: u64,
    pub forecast_shadow: bool,
    pub rate_hold_secs: u64,
    pub rate_hold_max_retries: u32,
    pub ws_upgrade_hold_secs: u64,
    pub rebalance_idle_secs: u64,
    pub session_binding_max_secs: u64,
    pub spread_threshold: usize,
    pub active_window_secs: u64,
    pub overload_window_secs: u64,
}
impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            mode: crate::routing::Mode::Rebalance,
            churn: crate::routing::ChurnLimits::default(),
            move_guard: crate::bindings::MoveGuardLimits::default(),
            gate_5h_pct: 90.0,
            gate_weekly_pct: 90.0,
            gate_weekly_near_reset_pct: 98.0,
            near_reset_secs: 18000,
            inflight_allowance_pct: 1.0,
            placement_allowance_pct: 2.0,
            weak_evidence_new_sessions: 1,
            placement_poll_after_secs: 120,
            placement_poll_min_interval_secs: 300,
            reset_hold_secs: 60,
            request_timeout_secs: 300,
            forecast_shadow: false,
            rate_hold_secs: 60,
            rate_hold_max_retries: 3,
            ws_upgrade_hold_secs: 10,
            rebalance_idle_secs: 3600,
            session_binding_max_secs: 604800,
            spread_threshold: 5,
            active_window_secs: 300,
            overload_window_secs: 600,
        }
    }
}

impl RoutingConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.churn.validate()?;
        anyhow::ensure!(
            self.mode != crate::routing::Mode::Strict,
            "global routing mode cannot be strict; set a project or root policy"
        );
        anyhow::ensure!(
            (1..=1000).contains(&self.weak_evidence_new_sessions),
            "weak evidence allowance must be 1..1000"
        );
        anyhow::ensure!(
            self.placement_poll_after_secs > 0 && self.placement_poll_min_interval_secs > 0,
            "placement polling intervals must be positive"
        );
        anyhow::ensure!(
            (1..=600).contains(&self.request_timeout_secs),
            "routing request timeout must be 1..600 seconds"
        );
        anyhow::ensure!(
            self.move_guard.clone().bounded().trip_seconds < self.request_timeout_secs,
            "routing move guard trip must be shorter than the request timeout"
        );
        anyhow::ensure!(
            (1..=600).contains(&self.rate_hold_secs)
                && (1..=14).contains(&self.ws_upgrade_hold_secs),
            "rate hold must be 1..600 seconds; WebSocket upgrade must be below 15 seconds"
        );
        anyhow::ensure!(
            self.reset_hold_secs <= 120,
            "routing reset hold cannot exceed 120 seconds"
        );
        anyhow::ensure!(
            [
                self.gate_5h_pct,
                self.gate_weekly_pct,
                self.gate_weekly_near_reset_pct,
                self.inflight_allowance_pct,
                self.placement_allowance_pct
            ]
            .into_iter()
            .all(|value| value.is_finite() && (0.0..=100.0).contains(&value)),
            "routing percentages must be finite and between 0 and 100"
        );
        anyhow::ensure!(
            self.gate_weekly_near_reset_pct >= self.gate_weekly_pct
                && self.rebalance_idle_secs > 0
                && self.session_binding_max_secs > 0
                && self.spread_threshold > 0
                && self.active_window_secs > 0
                && self.overload_window_secs > 0
                && self.rate_hold_secs > 0
                && self.rate_hold_max_retries > 0
                && self.ws_upgrade_hold_secs > 0,
            "invalid routing thresholds or binding lifetime"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GuardConfig {
    pub retain_max_per_request_mib: u64,
    pub inflight_budget_mib: u64,
    pub memory_hard_limit_mib: u64,
}
impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            retain_max_per_request_mib: 64,
            inflight_budget_mib: 384,
            memory_hard_limit_mib: 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct WebsocketConfig {
    pub idle_close_secs: u64,
    pub max_message_size_mib: u64,
    pub upstream_deflate: bool,
}
impl Default for WebsocketConfig {
    fn default() -> Self {
        Self {
            idle_close_secs: 1800,
            max_message_size_mib: 192,
            upstream_deflate: false,
        }
    }
}

impl WebsocketConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.upstream_deflate,
            "websocket.upstream_deflate=true requires a deflate-capable tungstenite fork; this build supports false only"
        );
        anyhow::ensure!(
            self.idle_close_secs > 0 && self.max_message_size_mib > 0,
            "websocket limits must be positive"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QuotaConfig {
    pub poll_min_interval_secs: u64,
}
impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            poll_min_interval_secs: 300,
        }
    }
}

impl QuotaConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=3600).contains(&self.poll_min_interval_secs),
            "quota.poll_min_interval_secs must be between 1 and 3600 to permit hourly cooling polls"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AccountConfig {
    /// Optional dedicated Codex launcher suffix; two ASCII lowercase letters.
    pub short: Option<String>,
    pub drain: bool,
    pub allow_credits: bool,
    pub spread_threshold: Option<usize>,
    /// Unset conservatively treats every Pro account as an image last resort.
    /// Explicit false opts out; true puts any paid account in the last tier.
    /// Priority tiers override affinity; binding preference applies within a tier.
    pub images_last_resort: Option<bool>,
}

#[cfg(test)]
mod image_account_tests {
    use super::*;

    #[test]
    fn images_last_resort_defaults_none_and_roundtrips_explicit_bools() {
        assert_eq!(AccountConfig::default().images_last_resort, None);
        let config: Config = toml::from_str(
            "[accounts.normal]\n[accounts.protected]\nimages_last_resort=true\n[accounts.optout]\nimages_last_resort=false\n",
        ).unwrap();
        assert_eq!(config.accounts["normal"].images_last_resort, None);
        assert_eq!(config.accounts["protected"].images_last_resort, Some(true));
        assert_eq!(config.accounts["optout"].images_last_resort, Some(false));
        let again: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(again.accounts["normal"].images_last_resort, None);
        assert_eq!(again.accounts["protected"].images_last_resort, Some(true));
        assert_eq!(again.accounts["optout"].images_last_resort, Some(false));
        assert!(
            toml::from_str::<Config>("[accounts.invalid]\nimages_last_resort='yes'\n").is_err()
        );
    }
}

impl Config {
    pub fn validate_account_aliases(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for account in self.accounts.values() {
            if let Some(short) = account.short.as_deref() {
                anyhow::ensure!(
                    short.len() == 2 && short.bytes().all(|b| b.is_ascii_lowercase()),
                    "accounts.<id>.short must contain exactly two ASCII lowercase letters"
                );
                anyhow::ensure!(short != "qb", "accounts.<id>.short reserves qb");
                anyhow::ensure!(seen.insert(short), "duplicate accounts.<id>.short");
            }
        }
        Ok(())
    }

    pub fn load(home: &Path) -> anyhow::Result<Self> {
        let text = match fs::read_to_string(home.join("config.toml")) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        // 파싱 오류에 설정 원문이 포함되지 않도록 외부 오류는 고정 문구로 제한한다.
        let mut value: toml::Value =
            toml::from_str(&text).map_err(|_| anyhow::anyhow!("invalid config.toml"))?;
        for key in clamp_numeric_bounds(&mut value) {
            config_warning("config_value_clamped", &key);
        }
        let mut config: Self = value
            .clone()
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid config.toml types"))?;
        if config.routing.gate_weekly_near_reset_pct < config.routing.gate_weekly_pct {
            config.routing.gate_weekly_near_reset_pct = config.routing.gate_weekly_pct;
            config_warning("config_value_clamped", "routing.gate_weekly_near_reset_pct");
        }
        if config.websocket.idle_close_secs >= config.routing.rebalance_idle_secs {
            config.routing.rebalance_idle_secs = config.routing.rebalance_idle_secs.max(2);
            config.websocket.idle_close_secs = config.routing.rebalance_idle_secs - 1;
            config_warning("config_value_clamped", "websocket.idle_close_secs");
        }
        config.upstream.validate()?;
        config.websocket.validate()?;
        config.routing.validate()?;
        config.quota.validate()?;
        config.routing_replay.validate()?;
        config.validate_account_aliases()?;
        anyhow::ensure!(
            config.websocket.idle_close_secs < config.routing.rebalance_idle_secs,
            "websocket.idle_close_secs must be less than routing.rebalance_idle_secs"
        );
        let known = toml::Value::try_from(Self::default())?;
        warn_unknown(&value, &known, "");
        if let Some(accounts) = value.get("accounts").and_then(toml::Value::as_table) {
            let known = toml::Value::try_from(AccountConfig {
                short: Some("rh".into()),
                spread_threshold: Some(1),
                images_last_resort: Some(false),
                ..AccountConfig::default()
            })?;
            for account in accounts.values() {
                warn_unknown(account, &known, "accounts.<id>.");
            }
        }
        Ok(config)
    }
}

fn config_warning(code: &'static str, path: &str) {
    let message = if code == "unknown_config_key" {
        "Unknown configuration key"
    } else {
        "Configuration adjusted"
    };
    if tracing::dispatcher::has_been_set() {
        tracing::warn!(event = "core_warning", code, config_key = path, "{message}");
    } else {
        eprintln!("WARN core_warning {code}: {message}: {path}");
    }
}

#[cfg(test)]
mod r8_bounds_tests {
    use super::*;

    #[test]
    fn startup_clamps_numeric_bounds_and_ignores_unknown_nested_keys_without_rewriting() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/config-clamp-tests");
        fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let text = r#"
future_root = "synthetic"
[routing]
rate_hold_secs = 0
ws_upgrade_hold_secs = 15
request_timeout_secs = 9999
reset_hold_secs = -10
weak_evidence_new_sessions = 0
placement_poll_after_secs = -1
placement_poll_min_interval_secs = 0
rate_hold_max_retries = 0
gate_5h_pct = -5
gate_weekly_pct = 120
gate_weekly_near_reset_pct = 0
inflight_allowance_pct = inf
placement_allowance_pct = nan
future_option = 42
[routing.churn]
target_success_secs = 0
target_quiet_secs = 0
future_nested = "synthetic"
dwell_secs = -1
revisit_secs = 90000
window_secs = 0
max_group_moves = 0
max_pool_moves = 99
max_move_bytes = 0
[quota]
poll_min_interval_secs = 0
future = true
[websocket]
idle_close_secs = 99999
max_message_size_mib = 0
future = true
"#;
        fs::write(home.path().join("config.toml"), text).unwrap();
        let config = Config::load(home.path()).unwrap();
        assert_eq!(config.routing.rate_hold_secs, 1);
        assert_eq!(config.routing.ws_upgrade_hold_secs, 14);
        assert_eq!(config.routing.request_timeout_secs, 600);
        assert_eq!(config.routing.reset_hold_secs, 0);
        assert_eq!(config.routing.weak_evidence_new_sessions, 1);
        assert_eq!(config.routing.placement_poll_after_secs, 1);
        assert_eq!(config.routing.placement_poll_min_interval_secs, 1);
        assert_eq!(config.routing.rate_hold_max_retries, 1);
        assert_eq!(config.routing.gate_5h_pct, 0.0);
        assert_eq!(config.routing.gate_weekly_pct, 100.0);
        assert_eq!(config.routing.gate_weekly_near_reset_pct, 100.0);
        assert_eq!(config.routing.inflight_allowance_pct, 100.0);
        assert_eq!(config.routing.placement_allowance_pct, 2.0);
        assert_eq!(config.routing.churn.dwell_secs, 0);
        assert_eq!(config.routing.churn.revisit_secs, 86400);
        assert_eq!(config.routing.churn.window_secs, 1);
        assert_eq!(config.routing.churn.max_group_moves, 1);
        assert_eq!(config.routing.churn.max_pool_moves, 32);
        assert_eq!(config.routing.churn.max_move_bytes, 1);
        assert_eq!(config.quota.poll_min_interval_secs, 1);
        assert_eq!(
            config.websocket.idle_close_secs,
            config.routing.rebalance_idle_secs - 1
        );
        assert_eq!(config.websocket.max_message_size_mib, 1);
        config.routing.validate().unwrap();
        assert_eq!(
            fs::read_to_string(home.path().join("config.toml")).unwrap(),
            text
        );
        let mut parsed: toml::Value = toml::from_str(text).unwrap();
        let warnings = clamp_numeric_bounds(&mut parsed);
        assert!(warnings.iter().any(|k| k == "routing.rate_hold_secs"));
        assert!(warnings.iter().any(|k| k == "routing.churn.max_pool_moves"));
        assert!(warnings.iter().all(|k| !k.contains("synthetic")));
    }
    #[test]
    fn valid_numeric_bounds_are_unchanged() {
        let mut values = toml::Value::try_from(Config::default()).unwrap();
        let original = values.clone();
        assert!(clamp_numeric_bounds(&mut values).is_empty());
        assert_eq!(values, original);
    }
    #[test]
    fn account_thresholds_and_extreme_intervals_clamp_before_unsigned_deserialization() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/config-clamp-tests");
        fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let text = "[routing]\nactive_window_secs = 9223372036854775807\noverload_window_secs = 9223372036854775807\n[accounts.\"cl-a\"]\nspread_threshold = -1\n[accounts.\"cl-b\"]\nspread_threshold = 0\n[accounts.\"cl-c\"]\nspread_threshold = 7\n";
        fs::write(home.path().join("config.toml"), text).unwrap();
        let config = Config::load(home.path()).unwrap();
        assert_eq!(config.accounts["cl-a"].spread_threshold, Some(1));
        assert_eq!(config.accounts["cl-b"].spread_threshold, Some(1));
        assert_eq!(config.accounts["cl-c"].spread_threshold, Some(7));
        assert_eq!(config.routing.active_window_secs, u64::from(u32::MAX));
        assert_eq!(config.routing.overload_window_secs, u64::from(u32::MAX));
        let delta = chrono::Duration::seconds(config.routing.active_window_secs as i64);
        assert!(chrono::Utc::now().checked_sub_signed(delta).is_some());
        assert!(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(
                    config.routing.overload_window_secs
                ))
                .is_some()
        );
        let mut parsed = toml::from_str(text).unwrap();
        let warnings = clamp_numeric_bounds(&mut parsed);
        assert_eq!(
            warnings
                .iter()
                .filter(|k| *k == "accounts.<id>.spread_threshold")
                .count(),
            2
        );
        assert_eq!(
            fs::read_to_string(home.path().join("config.toml")).unwrap(),
            text
        );
    }
    #[test]
    fn unsigned_schema_bounds_clamp_ports_guards_and_tray_without_rewriting() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/config-clamp-tests");
        fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let text = "[server]\ndata_port=-1\nadmin_port=99999\ndrain_timeout_secs=-1\n[guard]\nretain_max_per_request_mib=-1\ninflight_budget_mib=-1\nmemory_hard_limit_mib=-1\n[tray]\nburst_new_sessions=-1\n";
        fs::write(home.path().join("config.toml"), text).unwrap();
        let config = Config::load(home.path()).unwrap();
        assert_eq!(config.server.data_port, 0);
        assert_eq!(config.server.admin_port, u16::MAX);
        assert_eq!(config.server.drain_timeout_secs, 0);
        assert_eq!(config.guard.retain_max_per_request_mib, 0);
        assert_eq!(config.guard.inflight_budget_mib, 0);
        assert_eq!(config.guard.memory_hard_limit_mib, 0);
        assert_eq!(config.tray.burst_new_sessions, 0);
        assert_eq!(
            fs::read_to_string(home.path().join("config.toml")).unwrap(),
            text
        );
    }
}

// Normalize numbers before deserializing unsigned fields: negative TOML integers
// are out of bounds too. Never include a value or the config text in a warning.
fn clamp_numeric_bounds(value: &mut toml::Value) -> Vec<String> {
    // Keep configured timer arithmetic representable on Windows Instant and
    // Chrono alike (u32 seconds is over 136 years, far beyond operational use).
    const MAX_INTERVAL: i64 = u32::MAX as i64;
    fn field<'a>(value: &'a mut toml::Value, path: &str) -> Option<&'a mut toml::Value> {
        let mut field = value;
        for part in path.split('.') {
            field = field.get_mut(part)?;
        }
        Some(field)
    }
    let mut changed = Vec::new();
    for (path, min, max) in [
        ("server.data_port", 0, u16::MAX as i64),
        ("server.admin_port", 0, u16::MAX as i64),
        ("server.drain_timeout_secs", 0, MAX_INTERVAL),
        ("guard.retain_max_per_request_mib", 0, i64::MAX),
        ("guard.inflight_budget_mib", 0, i64::MAX),
        ("guard.memory_hard_limit_mib", 0, i64::MAX),
        ("tray.burst_new_sessions", 0, u32::MAX as i64),
        ("routing.weak_evidence_new_sessions", 1, 1000),
        ("routing.placement_poll_after_secs", 1, MAX_INTERVAL),
        ("routing.placement_poll_min_interval_secs", 1, MAX_INTERVAL),
        ("routing.request_timeout_secs", 1, 600),
        ("routing.rate_hold_secs", 1, 600),
        ("routing.ws_upgrade_hold_secs", 1, 14),
        ("routing.reset_hold_secs", 0, 120),
        ("routing.rate_hold_max_retries", 1, u32::MAX as i64),
        ("routing.rebalance_idle_secs", 1, MAX_INTERVAL),
        ("routing.session_binding_max_secs", 1, MAX_INTERVAL),
        ("routing.spread_threshold", 1, i64::MAX),
        ("routing.active_window_secs", 1, MAX_INTERVAL),
        ("routing.overload_window_secs", 1, MAX_INTERVAL),
        ("routing.near_reset_secs", 0, MAX_INTERVAL),
        ("routing.churn.dwell_secs", 0, 86400),
        ("routing.churn.revisit_secs", 0, 86400),
        ("routing.churn.window_secs", 1, 86400),
        ("routing.churn.max_group_moves", 1, 16),
        ("routing.churn.max_pool_moves", 1, 32),
        ("routing.churn.max_move_bytes", 1, 64 * 1024 * 1024),
        ("quota.poll_min_interval_secs", 1, 3600),
        ("websocket.idle_close_secs", 1, MAX_INTERVAL),
        ("websocket.max_message_size_mib", 1, i64::MAX),
    ] {
        if let Some(v) = field(value, path)
            && let Some(n) = v.as_integer()
        {
            let clamped = n.clamp(min, max);
            if clamped != n {
                *v = toml::Value::Integer(clamped);
                changed.push(path.to_owned());
            }
        }
    }
    if let Some(accounts) = value
        .get_mut("accounts")
        .and_then(toml::Value::as_table_mut)
    {
        for (_, account) in accounts.iter_mut() {
            if let Some(v) = account.get_mut("spread_threshold")
                && v.as_integer().is_some_and(|n| n < 1)
            {
                *v = toml::Value::Integer(1);
                changed.push("accounts.<id>.spread_threshold".into());
            }
        }
    }
    for path in [
        "routing.gate_5h_pct",
        "routing.gate_weekly_pct",
        "routing.gate_weekly_near_reset_pct",
        "routing.inflight_allowance_pct",
        "routing.placement_allowance_pct",
    ] {
        if let Some(v) = field(value, path)
            && let Some(n) = v.as_float().or_else(|| v.as_integer().map(|n| n as f64))
        {
            let fallback = match path {
                "routing.inflight_allowance_pct" => 1.0,
                "routing.placement_allowance_pct" => 2.0,
                "routing.gate_weekly_near_reset_pct" => 98.0,
                _ => 90.0,
            };
            let clamped = if n.is_nan() {
                fallback
            } else {
                n.clamp(0.0, 100.0)
            };
            if clamped != n {
                *v = toml::Value::Float(clamped);
                changed.push(path.to_owned());
            }
        }
    }
    changed
}

fn warn_unknown(value: &toml::Value, known: &toml::Value, prefix: &str) {
    if let (Some(value), Some(known)) = (value.as_table(), known.as_table()) {
        for (key, value) in value {
            if prefix.is_empty() && key == "accounts" {
                continue;
            }
            let path = format!("{prefix}{key}");
            match known.get(key) {
                Some(known) => warn_unknown(value, known, &format!("{path}.")),
                None => {
                    config_warning("unknown_config_key", &path);
                }
            }
        }
    }
}

#[cfg(test)]
mod upstream_tests {
    use super::*;
    #[test]
    fn websocket_deflate_requires_explicit_dependency_support() {
        let mut config = WebsocketConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.max_message_size_mib, 192);
        assert_eq!(config.idle_close_secs, 1800);
        config.upstream_deflate = true;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("deflate-capable tungstenite fork")
        );
    }
    #[test]
    fn bases_are_exact_and_test_http_is_loopback_only() {
        assert!(UpstreamConfig::default().validate().is_ok());
        let mut config = UpstreamConfig {
            anthropic_base: "http://127.0.0.1:32123".into(),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        for value in [
            "https://api.anthropic.com/other",
            "https://evil.invalid",
            "http://localhost:32123",
            "http://0.0.0.0:32123",
            "http://127.0.0.1:32123/path",
            "http://user:pass@127.0.0.1:32123",
            "http://127.0.0.1:32123?q=x",
            "http://127.0.0.1:32123#fragment",
        ] {
            config.anthropic_base = value.into();
            assert!(config.validate().is_err());
        }
    }
    #[test]
    fn usage_bases_and_interval_preserve_polling_safety() {
        let mut config = UpstreamConfig::default();
        for value in [
            "https://evil.invalid",
            "http://127.0.0.1:10100",
            "http://127.0.0.1:10101",
            "http://localhost:32123",
            "http://127.0.0.1:32123/path",
        ] {
            config.codex_usage_base = value.into();
            assert!(config.validate().is_err());
        }
        config.codex_usage_base = "http://127.0.0.1:32123".into();
        config.claude_usage_base = "http://127.0.0.1:32124".into();
        assert!(config.validate().is_ok());
        assert!(QuotaConfig::default().validate().is_ok());
        for secs in [0, 3601, u64::MAX] {
            assert!(
                QuotaConfig {
                    poll_min_interval_secs: secs
                }
                .validate()
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod ui_language_tests {
    use super::*;

    #[test]
    fn r16_language_roundtrip_and_unknown_values_keep_korean_default() {
        assert_eq!(Config::default().ui.language, UiLanguage::Ko);
        for (value, expected) in [
            ("\"ko\"", UiLanguage::Ko),
            ("\"en\"", UiLanguage::En),
            ("\"future\"", UiLanguage::Ko),
            ("true", UiLanguage::Ko),
            ("42", UiLanguage::Ko),
        ] {
            let config: Config = toml::from_str(&format!("[ui]\nlanguage = {value}\n")).unwrap();
            assert_eq!(config.ui.language, expected);
            let roundtrip: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(roundtrip.ui.language, expected);
        }
        assert_eq!(
            toml::from_str::<Config>("").unwrap().ui.language,
            UiLanguage::Ko
        );
    }
}

#[cfg(test)]
mod tray_v2_theme_tests {
    use super::*;
    #[test]
    fn r5_light_default_and_explicit_dark_system_with_unknown_fallback() {
        assert_eq!(Config::default().tray.theme, TrayTheme::Light);
        for (value, expected) in [
            ("light", TrayTheme::Light),
            ("dark", TrayTheme::Dark),
            ("system", TrayTheme::System),
            ("unknown", TrayTheme::Light),
        ] {
            let config: Config = toml::from_str(&format!("[tray]\ntheme = {value:?}\n")).unwrap();
            assert_eq!(config.tray.theme, expected);
            let roundtrip: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(roundtrip.tray.theme, expected);
        }
        assert_eq!(
            toml::from_str::<Config>("[tray]\nburst_new_sessions=12")
                .unwrap()
                .tray
                .theme,
            TrayTheme::Light
        );
    }
}

#[cfg(test)]
mod r8_routing_config_tests {
    use super::*;
    #[test]
    fn routing_unknown_future_key_is_compatible_but_global_strict_is_rejected() {
        let config: Config = toml::from_str("[routing]\nfuture_option = 42\n").unwrap();
        assert_eq!(config.routing.mode, crate::routing::Mode::Rebalance);
        config.routing.validate().unwrap();
        let mut strict = config.routing;
        strict.mode = crate::routing::Mode::Strict;
        assert!(strict.validate().is_err());
    }
    #[test]
    fn anti_herding_defaults_and_bounds() {
        let mut config = RoutingConfig::default();
        assert_eq!(config.placement_allowance_pct, 2.0);
        assert_eq!(config.weak_evidence_new_sessions, 1);
        config.validate().unwrap();
        config.placement_allowance_pct = f64::NAN;
        assert!(config.validate().is_err());
        config.placement_allowance_pct = 2.0;
        config.weak_evidence_new_sessions = 0;
        assert!(config.validate().is_err());
    }
}

#[cfg(test)]
mod account_alias_tests {
    use super::*;

    #[test]
    fn defaults_and_serde_roundtrip() {
        assert!(AccountConfig::default().short.is_none());
        let config: Config =
            toml::from_str("[accounts.one]\nshort='rh'\n[accounts.two]\n").unwrap();
        config.validate_account_aliases().unwrap();
        let again: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(again.accounts["one"].short.as_deref(), Some("rh"));
        assert!(again.accounts["two"].short.is_none());
        assert!(toml::from_str::<Config>("[accounts.one]\nshort=1\n").is_err());
    }

    #[test]
    fn invalid_reserved_and_duplicate_aliases() {
        for short in [
            "", "r", "rhh", "RH", "rH", "é", "한글", "r1", "qb", "..", "r/", "r\\", "r\n", "%x",
            "&x", "!x", "r\"",
        ] {
            let mut config = Config::default();
            config.accounts.insert(
                "one".into(),
                AccountConfig {
                    short: Some(short.into()),
                    ..Default::default()
                },
            );
            assert!(config.validate_account_aliases().is_err());
        }
        let mut config = Config::default();
        for id in ["one", "two"] {
            config.accounts.insert(
                id.into(),
                AccountConfig {
                    short: Some("rh".into()),
                    ..Default::default()
                },
            );
        }
        assert!(config.validate_account_aliases().is_err());
    }
}

#[cfg(test)]
mod r141_trip_validation_tests {
    use super::RoutingConfig;

    #[test]
    fn r141_default_trip_has_a_margin_before_the_request_deadline() {
        let config = RoutingConfig::default();
        assert_eq!(config.move_guard.trip_seconds, 240);
        assert_eq!(config.request_timeout_secs, 300);
        config.validate().unwrap();
    }

    #[test]
    fn r141_trip_equal_or_longer_than_request_deadline_is_rejected() {
        let mut config = RoutingConfig::default();
        for trip in [240, 241, u64::MAX] {
            config.request_timeout_secs = 240;
            config.move_guard.trip_seconds = trip;
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("move guard trip")
            );
        }
        config.move_guard.trip_seconds = 239;
        config.validate().unwrap();
        config.request_timeout_secs = 1;
        config.move_guard.trip_seconds = 0;
        assert!(
            config.validate().is_err(),
            "bounded minimum trip is one second"
        );
        config.request_timeout_secs = 2;
        config.move_guard.trip_seconds = 1;
        config.validate().unwrap();
    }
}

#[cfg(test)]
mod n6_replay_config_tests {
    use super::*;

    fn synthetic_home() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r142/minimum-config-test-homes");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    #[test]
    fn n6_replay_config_defaults_off_and_roundtrips_with_current_ui() {
        for config in [Config::default(), toml::from_str::<Config>("").unwrap()] {
            assert!(!config.routing_replay.enabled);
            assert_eq!(config.routing_replay.disk_limit_mib, 256);
            config.routing_replay.validate().unwrap();
        }
        let partial: Config = toml::from_str("[routing_replay]\nenabled=true\n").unwrap();
        assert!(partial.routing_replay.enabled);
        assert_eq!(partial.routing_replay.disk_limit_mib, 256);
        let budget_only: Config = toml::from_str("[routing_replay]\ndisk_limit_mib=16\n").unwrap();
        assert!(!budget_only.routing_replay.enabled);
        assert_eq!(budget_only.routing_replay.disk_limit_mib, 16);
        for enabled in [false, true] {
            let config: Config = toml::from_str(&format!(
                "[ui]\nlanguage='en'\n[routing_replay]\nenabled={enabled}\ndisk_limit_mib=512\n"
            ))
            .unwrap();
            let again: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(again.ui.language, UiLanguage::En);
            assert_eq!(again.routing_replay.enabled, enabled);
            assert_eq!(again.routing_replay.disk_limit_mib, 512);
            again.routing_replay.validate().unwrap();
        }
    }

    #[test]
    fn n6_replay_config_budget_bounds_apply_off_and_on() {
        for enabled in [false, true] {
            for limit in [16, 256, 512] {
                RoutingReplayConfig {
                    enabled,
                    disk_limit_mib: limit,
                }
                .validate()
                .unwrap();
            }
            for limit in [0, 15, 513, u64::MAX] {
                let error = RoutingReplayConfig {
                    enabled,
                    disk_limit_mib: limit,
                }
                .validate()
                .unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "routing replay disk limit must be between 16 and 512 MiB"
                );
            }
        }
    }

    #[test]
    fn n6_replay_config_load_validates_without_clamping_or_rewriting() {
        let home = synthetic_home();
        let path = home.path().join("config.toml");
        let missing = Config::load(home.path()).unwrap();
        assert!(!missing.routing_replay.enabled);
        assert_eq!(missing.routing_replay.disk_limit_mib, 256);
        assert!(!path.exists(), "missing config must not be created");
        for enabled in [false, true] {
            for limit in [16i64, 256, 512] {
                let text = format!(
                    "# synthetic fixture\n[ui]\nlanguage='en'\n[routing_replay]\nenabled={enabled}\ndisk_limit_mib={limit}\n"
                );
                fs::write(&path, &text).unwrap();
                let config = Config::load(home.path()).unwrap();
                assert_eq!(config.routing_replay.enabled, enabled);
                assert_eq!(config.routing_replay.disk_limit_mib, limit as u64);
                assert_eq!(config.ui.language, UiLanguage::En);
                assert_eq!(fs::read_to_string(&path).unwrap(), text);
            }
            for limit in [0i64, 15, 513, i64::MAX] {
                let text = format!("[routing_replay]\nenabled={enabled}\ndisk_limit_mib={limit}\n");
                fs::write(&path, &text).unwrap();
                assert_eq!(
                    Config::load(home.path()).unwrap_err().to_string(),
                    "routing replay disk limit must be between 16 and 512 MiB"
                );
                assert_eq!(fs::read_to_string(&path).unwrap(), text);
            }
        }
    }

    #[test]
    fn n6_replay_config_load_rejects_types_without_echoing_values() {
        let home = synthetic_home();
        let path = home.path().join("config.toml");
        for text in [
            "[routing_replay]\nenabled='synthetic-private-value'\n",
            "[routing_replay]\nenabled=1\n",
            "[routing_replay]\ndisk_limit_mib=-1\n",
            "[routing_replay]\ndisk_limit_mib=16.5\n",
            "[routing_replay]\ndisk_limit_mib='synthetic-private-value'\n",
            "routing_replay='synthetic-private-value'\n",
        ] {
            fs::write(&path, text).unwrap();
            assert_eq!(
                Config::load(home.path()).unwrap_err().to_string(),
                "invalid config.toml types"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        fs::write(&path, "[routing_replay]\nenabled=synthetic-private-value\n").unwrap();
        assert_eq!(
            Config::load(home.path()).unwrap_err().to_string(),
            "invalid config.toml"
        );
    }
}
