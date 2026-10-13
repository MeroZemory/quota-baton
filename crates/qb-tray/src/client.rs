//! Read-only client for the local core admin API.
//!
//! This module deliberately has no dependency on the Win32 shell.  The tray
//! shell can poll [`AdminClient`] and render the returned snapshot later.

use std::{fs, path::Path, time::Duration};

use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Debug, Clone)]
pub struct AdminClient {
    http: reqwest::Client,
    base_url: String,
    admin_token: String,
}

impl AdminClient {
    /// Load the port from `QB_HOME/config.toml` and the token from
    /// `QB_HOME/admin.token`, using the same home resolution as the core.
    pub fn from_home() -> anyhow::Result<Self> {
        Self::from_home_path(qb_core::home()?)
    }

    pub fn from_home_path(home: impl AsRef<Path>) -> anyhow::Result<Self> {
        let home = home.as_ref();
        let config = qb_core::config::Config::load(home)?;
        let token_path = home.join("admin.token");
        let metadata = fs::symlink_metadata(&token_path)?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "admin.token must be a regular file"
        );
        let token = fs::read_to_string(token_path)?.trim().to_owned();
        anyhow::ensure!(!token.is_empty(), "admin.token is empty");
        Self::new(config.server.admin_port, token)
    }

    /// Construct a client without touching the filesystem.  This is useful
    /// for synthetic loopback fakes and unit tests.
    pub fn new(admin_port: u16, admin_token: impl Into<String>) -> anyhow::Result<Self> {
        anyhow::ensure!(admin_port != 0, "admin port must be non-zero");
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            http,
            base_url: format!("http://127.0.0.1:{admin_port}"),
            admin_token: admin_token.into(),
        })
    }

    pub async fn health(&self) -> Result<CoreHealth, ClientError> {
        match self.get::<HealthResponse>("/admin/v1/health").await {
            Ok(health) => Ok(classify_health(health)),
            Err(ClientError::Transport(error)) => Ok(CoreHealth::Unreachable {
                reason: error.to_string(),
            }),
            Err(error) => Ok(CoreHealth::Unhealthy {
                reason: error.to_string(),
            }),
        }
    }

    pub async fn snapshot(&self) -> Result<TraySnapshot, ClientError> {
        self.get("/admin/v1/state").await
    }

    /// Fetch health and state as one read-only poll.
    pub async fn read(&self) -> Result<TrayRead, ClientError> {
        let health = self.health().await?;
        let snapshot = self.snapshot().await?;
        Ok(TrayRead { health, snapshot })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .http
            .get(url)
            .header(header::HOST, self.host())
            .header(header::ORIGIN, format!("{}/", self.base_url))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.admin_token),
            )
            .send()
            .await
            .map_err(ClientError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ClientError::Http(status));
        }
        response.json().await.map_err(ClientError::Decode)
    }

    fn host(&self) -> String {
        self.base_url.trim_start_matches("http://").to_owned()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("admin API transport failed: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("admin API returned HTTP {0}")]
    Http(StatusCode),
    #[error("admin API returned invalid JSON: {0}")]
    Decode(#[source] reqwest::Error),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub pid: u32,
    pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreHealth {
    Healthy(HealthResponse),
    Unhealthy { reason: String },
    Unreachable { reason: String },
}

pub fn classify_health(health: HealthResponse) -> CoreHealth {
    if health.ok {
        CoreHealth::Healthy(health)
    } else {
        CoreHealth::Unhealthy {
            reason: format!("코어 health가 false입니다 ({})", health.version),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrayRead {
    pub health: CoreHealth,
    pub snapshot: TraySnapshot,
}

/// Convert the admin snapshot into the model consumed by both the live
/// Direct2D flyout and the WIC review renderer.
pub fn layout_rows(
    read: &TrayRead,
    now: std::time::SystemTime,
) -> anyhow::Result<Vec<qb_tray::Row>> {
    let json = serde_json::to_string(&read.snapshot)?;
    Ok(qb_tray::parse_state_at(&json, now)?)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraySnapshot {
    #[serde(default)]
    pub accounts: Vec<TrayAccount>,
    #[serde(default)]
    pub inflight: Inflight,
    #[serde(default)]
    pub memory: Memory,
    #[serde(default)]
    pub bindings: Bindings,
    pub data_port: u16,
    pub version: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TrayAccount {
    pub id: String,
    pub provider: String,
    pub label: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub user_state: String,
    pub health: serde_json::Value,
    #[serde(default)]
    pub probe_pending: bool,
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    pub credits: Option<serde_json::Value>,
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub bound_sessions: u64,
    pub token_expires_at: Option<String>,
    #[serde(default)]
    pub drain: bool,
    #[serde(default)]
    pub allow_credits: bool,
    #[serde(default)]
    pub refresh_attention: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct QuotaWindow {
    pub name: String,
    pub used_pct: Option<f64>,
    pub reset_at: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Inflight {
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Memory {
    #[serde(default)]
    pub private_bytes: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Bindings {
    #[serde(default)]
    pub count: u64,
}

/// Render a compact Korean tooltip line without exposing any secret fields.
pub fn render_status_line(read: &TrayRead) -> String {
    match &read.health {
        CoreHealth::Unreachable { .. } => "코어: 연결할 수 없음".to_owned(),
        CoreHealth::Unhealthy { .. } => "코어: 비정상".to_owned(),
        CoreHealth::Healthy(_) => {
            let active = read
                .snapshot
                .accounts
                .iter()
                .filter(|a| a.user_state == "active")
                .count();
            let attention = read
                .snapshot
                .accounts
                .iter()
                .filter(|a| {
                    a.refresh_attention
                        || a.probe_pending
                        || a.health.get("kind").and_then(serde_json::Value::as_str) != Some("ready")
                })
                .count();
            format!("코어: 실행 중 · 계정 {active}개 · 주의 {attention}개")
        }
    }
}

/// Render a deterministic review image from an admin state fixture. The native
/// flyout consumes the same rows; this compact encoder keeps the aid usable in
/// CI without a desktop compositor.
pub fn render_flyout(read: &TrayRead) -> String {
    let mut lines = vec![render_status_line(read)];
    for provider in ["claude", "codex"] {
        let accounts = read
            .snapshot
            .accounts
            .iter()
            .filter(|account| account.provider == provider)
            .collect::<Vec<_>>();
        if accounts.is_empty() {
            continue;
        }
        lines.push(
            if provider == "claude" {
                "Claude"
            } else {
                "Codex"
            }
            .into(),
        );
        for account in accounts {
            let name = account
                .label
                .as_deref()
                .or(account.email.as_deref())
                .unwrap_or("이름 없는 계정");
            let state = match account.user_state.as_str() {
                "paused" => "일시정지",
                "disabled" => "사용 안 함",
                _ if account
                    .health
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    == Some("needs_relogin") =>
                {
                    "다시 로그인"
                }
                _ => "준비됨",
            };
            let usage = account
                .windows
                .first()
                .and_then(|window| window.used_pct)
                .map(|value| format!(" · 5시간 {:.0}%", value))
                .unwrap_or_default();
            lines.push(format!("  {} · {}{}", name, state, usage));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_synthetic_state_snapshot() {
        let snapshot: TraySnapshot = serde_json::from_str(r#"{
          "accounts":[{"id":"cx-test","provider":"codex","label":"합성","email":null,
            "plan":"plus","user_state":"active","health":{"kind":"ready"},
            "windows":[{"name":"5h","used_pct":42.5,"reset_at":"2030-01-01T00:00:00Z","status":"ok"}],
            "probe_pending":false,"bound_sessions":2,"drain":false,"allow_credits":false}],
          "inflight":{"requests":1,"bytes":12},"memory":{"private_bytes":99},
          "bindings":{"count":2},"data_port":10200,"version":"synthetic"
        }"#).unwrap();
        assert_eq!(snapshot.accounts[0].id, "cx-test");
        assert_eq!(snapshot.accounts[0].windows[0].used_pct, Some(42.5));
        assert_eq!(snapshot.inflight.bytes, 12);
    }

    #[test]
    fn classifies_health_and_unreachable_without_network_or_credentials() {
        let healthy: HealthResponse =
            serde_json::from_str(r#"{"ok":true,"version":"test","pid":0,"started_at":"now"}"#)
                .unwrap();
        assert!(matches!(classify_health(healthy), CoreHealth::Healthy(_)));
        let unhealthy: HealthResponse =
            serde_json::from_str(r#"{"ok":false,"version":"test","pid":0,"started_at":"now"}"#)
                .unwrap();
        assert!(matches!(
            classify_health(unhealthy),
            CoreHealth::Unhealthy { .. }
        ));
        assert!(matches!(
            CoreHealth::Unhealthy {
                reason: "synthetic failure".into()
            },
            CoreHealth::Unhealthy { .. }
        ));
        assert!(matches!(
            CoreHealth::Unreachable {
                reason: "loopback refused".into()
            },
            CoreHealth::Unreachable { .. }
        ));
    }

    #[test]
    fn renders_korean_read_only_account_rows_without_secrets() {
        let snapshot: TraySnapshot = serde_json::from_value(serde_json::json!({
            "accounts": [{"id":"synthetic-id","provider":"codex","email":"fixture@example.invalid",
              "user_state":"active","health":{"kind":"ready"},"windows":[{"name":"5h","used_pct":42.0}]}],
            "data_port":10200,"version":"synthetic"
        })).unwrap();
        let read = TrayRead {
            health: CoreHealth::Healthy(HealthResponse {
                ok: true,
                version: "synthetic".into(),
                pid: 1,
                started_at: "now".into(),
            }),
            snapshot,
        };
        let rendered = render_flyout(&read);
        assert!(
            rendered.contains("Codex") && rendered.contains("준비됨") && rendered.contains("42%")
        );
        assert!(!rendered.contains("synthetic-id"));
    }
}
