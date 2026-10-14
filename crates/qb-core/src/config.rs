use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    pub routing: RoutingConfig,
    pub guard: GuardConfig,
    pub websocket: WebsocketConfig,
    pub quota: QuotaConfig,
    pub accounts: BTreeMap<String, AccountConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct UpstreamConfig {
    pub anthropic_base: String,
    pub codex_base: String,
    pub claude_usage_base: String,
    pub codex_usage_base: String,
    #[cfg(all(feature = "testkit", debug_assertions))]
    pub claude_token_endpoint: Option<String>,
    #[cfg(all(feature = "testkit", debug_assertions))]
    pub codex_token_endpoint: Option<String>,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            anthropic_base: "https://api.anthropic.com".into(),
            codex_base: "https://chatgpt.com/backend-api/codex".into(),
            claude_usage_base: "https://api.anthropic.com".into(),
            codex_usage_base: "https://chatgpt.com".into(),
            #[cfg(all(feature = "testkit", debug_assertions))]
            claude_token_endpoint: None,
            #[cfg(all(feature = "testkit", debug_assertions))]
            codex_token_endpoint: None,
        }
    }
}

impl UpstreamConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        #[cfg(all(feature = "testkit", debug_assertions))]
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
            #[cfg(any(test, all(feature = "testkit", debug_assertions)))]
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
    pub gate_5h_pct: f64,
    pub gate_weekly_pct: f64,
    pub gate_weekly_near_reset_pct: f64,
    pub near_reset_secs: u64,
    pub inflight_allowance_pct: f64,
    pub reset_hold_secs: u64,
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
            gate_5h_pct: 90.0,
            gate_weekly_pct: 90.0,
            gate_weekly_near_reset_pct: 98.0,
            near_reset_secs: 18000,
            inflight_allowance_pct: 1.0,
            reset_hold_secs: 60,
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
        anyhow::ensure!(
            [
                self.gate_5h_pct,
                self.gate_weekly_pct,
                self.gate_weekly_near_reset_pct,
                self.inflight_allowance_pct
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
    pub drain: bool,
    pub allow_credits: bool,
    pub spread_threshold: Option<usize>,
}

impl Config {
    pub fn load(home: &Path) -> anyhow::Result<Self> {
        let text = match fs::read_to_string(home.join("config.toml")) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        // 파싱 오류에 설정 원문이 포함되지 않도록 외부 오류는 고정 문구로 제한한다.
        let value: toml::Value =
            toml::from_str(&text).map_err(|_| anyhow::anyhow!("invalid config.toml"))?;
        let config: Self = value
            .clone()
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid config.toml types"))?;
        config.upstream.validate()?;
        config.websocket.validate()?;
        config.routing.validate()?;
        config.quota.validate()?;
        anyhow::ensure!(
            config.websocket.idle_close_secs < config.routing.rebalance_idle_secs,
            "websocket.idle_close_secs must be less than routing.rebalance_idle_secs"
        );
        let known = toml::Value::try_from(Self::default())?;
        warn_unknown(&value, &known, "");
        if let Some(accounts) = value.get("accounts").and_then(toml::Value::as_table) {
            let known = toml::Value::try_from(AccountConfig::default())?;
            for account in accounts.values() {
                warn_unknown(account, &known, "accounts.<id>.");
            }
        }
        Ok(config)
    }
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
                    if tracing::dispatcher::has_been_set() {
                        tracing::warn!(config_key = %path, "Unknown configuration key");
                    } else {
                        eprintln!("WARN Unknown configuration key: {path}");
                    }
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
