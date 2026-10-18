//! Read-only client for the local core admin API.
//!
//! This module deliberately has no dependency on the Win32 shell.  The tray
//! shell can poll [`AdminClient`] and render the returned snapshot later.

use std::{fs, path::Path, time::Duration};

use reqwest::{StatusCode, header};
use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, Deserializer},
};

#[derive(Debug, Clone)]
pub struct AdminClient {
    http: reqwest::Client,
    base_url: String,
    admin_token: String,
}

#[allow(dead_code)]
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

    pub async fn patch_account(
        &self,
        id: &str,
        patch: AccountPatch,
    ) -> Result<TrayAccount, ClientError> {
        self.send_json(
            reqwest::Method::PATCH,
            &format!("/admin/v1/accounts/{id}"),
            &patch,
        )
        .await
    }

    pub async fn account_action(
        &self,
        id: &str,
        action: AccountAction,
    ) -> Result<TrayAccount, ClientError> {
        let path = format!("/admin/v1/accounts/{id}/{}", action.as_str());
        self.send_json(reqwest::Method::POST, &path, &serde_json::json!({}))
            .await
    }

    pub async fn refresh_tokens(&self, id: &str) -> Result<(), ClientError> {
        let response = self
            .http
            .post(format!("{}/admin/v1/accounts/{id}/refresh", self.base_url))
            .timeout(Duration::from_secs(35))
            .bearer_auth(&self.admin_token)
            .json(&serde_json::json!({"force":false}))
            .send()
            .await
            .map_err(ClientError::Transport)?;
        let status = response.status();
        let summary: serde_json::Value = response.json().await.map_err(ClientError::Decode)?;
        refresh_result(status, &summary)
    }

    pub async fn delete_account(&self, id: &str) -> Result<(), ClientError> {
        self.send_empty(reqwest::Method::DELETE, &format!("/admin/v1/accounts/{id}"))
            .await
    }

    pub async fn start_login(&self, provider: &str) -> Result<LoginStart, ClientError> {
        self.send_json(
            reqwest::Method::POST,
            "/admin/v1/accounts/login",
            &serde_json::json!({"provider": provider}),
        )
        .await
    }

    pub async fn login_status(&self, login_id: &str) -> Result<LoginStatus, ClientError> {
        self.get(&format!("/admin/v1/accounts/login/{login_id}"))
            .await
    }

    pub async fn cancel_login(&self, login_id: &str) -> Result<(), ClientError> {
        self.send_empty(
            reqwest::Method::POST,
            &format!("/admin/v1/accounts/login/{login_id}/cancel"),
        )
        .await
    }

    pub async fn shutdown(&self, drain_timeout_secs: u64) -> Result<(), ClientError> {
        self.send_empty_json(
            reqwest::Method::POST,
            "/admin/v1/lifecycle/shutdown",
            &serde_json::json!({"drain_timeout_secs": drain_timeout_secs}),
        )
        .await
        .map(|_| ())
    }

    pub async fn events(&self) -> Result<reqwest::Response, ClientError> {
        self.request(reqwest::Method::GET, "/admin/v1/events")
            .timeout(Duration::from_secs(3600))
            .send()
            .await
            .map_err(ClientError::Transport)
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, ClientError> {
        let response = self.send_empty_json(method, path, body).await?;
        response.json().await.map_err(ClientError::Decode)
    }

    async fn send_empty_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response, ClientError> {
        let response = self
            .request(method, path)
            .json(body)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        test_response_delay().await;
        Self::check_response(response).await
    }

    async fn send_empty(&self, method: reqwest::Method, path: &str) -> Result<(), ClientError> {
        let response = self
            .request(method, path)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        Self::check_response(response).await.map(|_| ())
    }

    async fn check_response(response: reqwest::Response) -> Result<reqwest::Response, ClientError> {
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(ClientError::Http(response.status()))
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        self.http
            .request(method, url)
            .header(header::HOST, self.host())
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.admin_token),
            )
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        test_response_delay().await;
        let response = Self::check_response(response).await?;
        response.json().await.map_err(ClientError::Decode)
    }

    fn host(&self) -> String {
        self.base_url.trim_start_matches("http://").to_owned()
    }
}

async fn test_response_delay() {
    let Some(path) = std::env::var_os("QB_TRAY_TEST_DELAY_FILE") else {
        return;
    };
    if std::path::Path::new(&path).exists() {
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountAction {
    Pause,
    Resume,
    Disable,
    Enable,
    RefreshUsage,
}

impl AccountAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Disable => "disable",
            Self::Enable => "enable",
            Self::RefreshUsage => "refresh-usage",
        }
    }

    #[cfg(test)]
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "pause" => Some(Self::Pause),
            "resume" => Some(Self::Resume),
            "disable" => Some(Self::Disable),
            "enable" => Some(Self::Enable),
            "refresh-usage" => Some(Self::RefreshUsage),
            _ => None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Serialize)]
pub struct AccountPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drain: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_credits: Option<bool>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LoginStart {
    pub login_id: String,
    pub auth_url: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LoginStatus {
    pub status: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("admin API transport failed: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("admin API returned HTTP {0}")]
    Http(StatusCode),
    #[error("{0}")]
    AccountAction(&'static str),
    #[error("admin API returned invalid JSON: {0}")]
    Decode(#[source] reqwest::Error),
}

fn refresh_result(status: StatusCode, value: &serde_json::Value) -> Result<(), ClientError> {
    if status.is_success() {
        return match value["outcome"].as_str() {
            Some("success") => Ok(()),
            Some(
                "refresh_token_invalidated"
                | "refresh_token_reused"
                | "refresh_token_expired"
                | "refresh_token_revoked"
                | "invalid_grant"
                | "token_endpoint_401",
            ) => Err(ClientError::AccountAction(
                "계정 메뉴에서 다시 로그인하세요",
            )),
            _ => Err(ClientError::AccountAction("갱신 응답을 확인할 수 없습니다")),
        };
    }
    let message = match value["error"]["code"].as_str() {
        Some("linked") => "CPA가 갱신하는 계정입니다",
        Some("drain") => "드레인 해제 후 다시 시도하세요",
        Some("needs_relogin") => "계정 메뉴에서 다시 로그인하세요",
        Some("uncertain") => "이전 갱신 결과가 불확실합니다 · CLI의 --force를 확인하세요",
        Some("unavailable") => "잠시 후 다시 갱신해 주세요",
        Some("not_found") => "계정을 찾을 수 없습니다",
        _ => return Err(ClientError::Http(status)),
    };
    Err(ClientError::AccountAction(message))
}

#[allow(dead_code)]
pub fn short_error(error: &ClientError) -> &'static str {
    match error {
        ClientError::AccountAction(message) => message,
        ClientError::Transport(_) => "코어에 연결할 수 없습니다",
        ClientError::Http(StatusCode::TOO_MANY_REQUESTS) => "잠시 후 다시 시도해 주세요",
        ClientError::Http(StatusCode::NOT_FOUND) => "계정을 찾을 수 없습니다",
        ClientError::Http(_) => "요청을 처리하지 못했습니다",
        ClientError::Decode(_) => "코어 응답을 읽지 못했습니다",
    }
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
pub struct TrayWarning {
    #[serde(default)]
    pub model_limited: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub burst_new_sessions: Option<u64>,
    #[serde(default)]
    pub observed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub reset_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub action: Option<qb_tray::QuotaAction>,
    #[serde(default)]
    pub next_account: Option<String>,
    #[serde(default)]
    pub earliest_account: Option<String>,
}

impl TrayWarning {
    /// Same flat fields as the per-account context, plus kind/account.
    /// Event wiring is owned by main; snapshot context remains authoritative.
    #[allow(dead_code)]
    pub fn quota_notice(&self) -> Option<qb_tray::QuotaNotice> {
        if self.kind != "quota_notice" || self.account.as_deref().is_none_or(str::is_empty) {
            return None;
        }
        Some(qb_tray::QuotaNotice {
            model_limited: self.model_limited,
            window: self.window.clone(),
            reset_at: self.reset_at,
            action: self.action.clone()?,
            next_account: self.next_account.clone(),
            earliest_account: self.earliest_account.clone(),
            observed_at: self.observed_at?,
        })
    }
}

#[cfg(test)]
mod quota_wire_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshot_and_flat_core_warning_share_typed_context_without_using_raw_reasons() {
        let context = json!({"window":"5h", "model_limited":true, "reset_at":null, "action":"failover",
            "next_account":"b", "earliest_account":"c", "observed_at":"2026-10-18T12:00:00Z"});
        let account: TrayAccount =
            serde_json::from_value(json!({"id":"a", "quota_notice":context})).unwrap();
        let mut warning = context.clone();
        warning["kind"] = json!("quota_notice");
        warning["account"] = json!("a");
        warning["reason"] = json!("DO NOT DISPLAY raw body");
        let warning: TrayWarning = serde_json::from_value(warning).unwrap();
        let notice = warning.quota_notice().unwrap();
        assert!(notice.model_limited);
        assert_eq!(notice.action, qb_tray::QuotaAction::Failover);
        assert_eq!(notice.window, account.quota_notice.unwrap().window);
        assert_eq!(notice.next_account.as_deref(), Some("b"));
        assert_eq!(notice.earliest_account.as_deref(), Some("c"));
        assert!(notice.reset_at.is_none());
        let warning: TrayWarning =
            serde_json::from_value(json!({"kind":"quota_notice","account":"a"})).unwrap();
        assert!(warning.quota_notice().is_none());
        let account: TrayAccount = serde_json::from_value(json!({"allow_credits":true})).unwrap();
        assert!(account.quota_notice.is_none());
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraySnapshot {
    #[serde(default)]
    pub warnings: Vec<TrayWarning>,
    #[serde(default)]
    pub pools: serde_json::Value,
    #[serde(default)]
    pub accounts: Vec<TrayAccount>,
    #[serde(default)]
    pub inflight: Inflight,
    #[serde(default)]
    pub memory: Memory,
    #[serde(default)]
    pub bindings: Bindings,
    #[serde(default)]
    pub data_port: u16,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TrayAccount {
    #[serde(default)]
    pub has_alternative: bool,
    #[serde(default)]
    pub quota_notice: Option<qb_tray::QuotaNotice>,
    #[serde(default)]
    pub last_relevant_state_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub user_state: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub health: serde_json::Value,
    #[serde(default)]
    pub probe_pending: bool,
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    #[serde(default)]
    pub credits: Option<serde_json::Value>,
    #[serde(default)]
    pub credits_in_use: Option<bool>,
    #[serde(default)]
    pub pool_eligible: Option<bool>,
    #[serde(default)]
    pub quota_cooling: Option<serde_json::Value>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub bound_sessions: u64,
    #[serde(default)]
    pub active_sessions: u64,
    #[serde(default)]
    pub spread_threshold: u64,
    #[serde(default)]
    pub token_expires_at: Option<String>,
    #[serde(default)]
    pub last_refresh_at: Option<String>,
    #[serde(default)]
    pub drain: bool,
    #[serde(default)]
    pub allow_credits: bool,
    #[serde(default)]
    pub refresh_attention: bool,
    #[serde(default, deserialize_with = "decode_source")]
    pub source: String,
    #[serde(default)]
    pub pacing: qb_tray::Pacing,
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct QuotaWindow {
    pub name: String,
    pub used_pct: Option<f64>,
    pub reset_at: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
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

#[cfg(test)]
mod r5_transport_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn r5_core_fields_survive_dto_and_shared_complete_layout_input() {
        let at = "2026-10-18T12:05:00Z";
        let reset = "2026-10-19T01:00:00Z";
        let input = json!({"version":"synthetic","pools":{"codex":{"eligible":1}},"accounts":[{
            "id":"synthetic-r5-codex","provider":"codex","email":"sample@example.invalid","plan":"plus",
            "user_state":"active","health":{"kind":"ready"},"source":{"kind":"cpa","mode":"adopted"},
            "pool_eligible":true,"credits_in_use":true,"active_sessions":2,"bound_sessions":3,"spread_threshold":8,
            "allow_credits":true,"credits":{"has_credits":true,"balance":19395.23},"observed_at":at,
            "quota_cooling":{"window":"5h","reset_at":reset,"started_at":at},
            "windows":[{"name":"5h","used_pct":100.0,"reset_at":reset,"observed_at":at,"scope":"account"}],
            "pacing":{"backoff_until":reset,"waiting":2,"step":3,"release_interval_ms":500}
        }]});
        let snapshot: TraySnapshot = serde_json::from_value(input).unwrap();
        let serialized = serde_json::to_value(&snapshot).unwrap();
        let account = &serialized["accounts"][0];
        assert_eq!(account["credits_in_use"], true);
        assert_eq!(account["pool_eligible"], true);
        assert_eq!(account["active_sessions"], 2);
        assert_eq!(account["spread_threshold"], 8);
        assert_eq!(account["credits"]["balance"], 19395.23);
        assert_eq!(account["quota_cooling"]["window"], "5h");
        assert_eq!(account["observed_at"], at);
        assert_eq!(account["windows"][0]["scope"], "account");
        assert_eq!(account["windows"][0]["observed_at"], at);
        assert_eq!(account["pacing"]["waiting"], 2);
        assert_eq!(account["pacing"]["backoff_until"], reset);
        assert_eq!(account["pacing"]["step"], 3);
        let now: std::time::SystemTime =
            at.parse::<chrono::DateTime<chrono::Utc>>().unwrap().into();
        let read = TrayRead {
            snapshot,
            health: CoreHealth::Healthy(HealthResponse {
                ok: true,
                version: "synthetic".into(),
                pid: 1,
                started_at: at.into(),
            }),
        };
        let rows = layout_rows(&read, now).unwrap();
        let view = qb_tray::v2::project(rows[0].view_input.as_deref().unwrap(), now).unwrap();
        let codex = &view.sections[1];
        assert_eq!(codex.rows[0].state, qb_tray::v2::DisplayState::Credits);
        assert_eq!(codex.rows[0].raw.active_sessions, 2);
        assert!(codex.rows[0].raw.source.owned_or_adopted());
        assert_eq!(view.summaries[1].verdict, qb_tray::v2::Verdict::Credits);
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Bindings {
    #[serde(default)]
    pub count: u64,
}

/// Render a compact Korean tooltip line without exposing any secret fields.
#[cfg(test)]
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
#[cfg(test)]
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
    fn account_action_parser_round_trips_typed_actions_and_rejects_unknown() {
        for action in [
            AccountAction::Pause,
            AccountAction::Resume,
            AccountAction::Disable,
            AccountAction::Enable,
            AccountAction::RefreshUsage,
        ] {
            assert_eq!(AccountAction::from_str(action.as_str()), Some(action));
        }
        assert_eq!(AccountAction::from_str("unknown"), None);
    }

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
    fn manual_refresh_errors_are_actionable_and_never_echo_server_payload() {
        assert!(refresh_result(StatusCode::OK, &serde_json::json!({"outcome":"success"})).is_ok());
        let uncertain = refresh_result(StatusCode::CONFLICT,
            &serde_json::json!({"error":{"code":"uncertain","message":"synthetic-secret-never-echo"}})).unwrap_err();
        assert!(short_error(&uncertain).contains("--force"));
        assert!(!uncertain.to_string().contains("synthetic-secret"));
        let terminal = refresh_result(StatusCode::OK,
            &serde_json::json!({"outcome":"refresh_token_reused","extra":"synthetic-secret-never-echo"})).unwrap_err();
        assert!(short_error(&terminal).contains("다시 로그인"));
        let unknown = refresh_result(StatusCode::CONFLICT,
            &serde_json::json!({"error":{"code":"synthetic-secret-never-echo","message":"synthetic-secret-never-echo"}})).unwrap_err();
        assert!(!unknown.to_string().contains("synthetic-secret"));
    }

    #[test]
    fn decodes_source_object_and_keeps_malformed_source_visible() {
        let snapshot: TraySnapshot = serde_json::from_value(serde_json::json!({
            "accounts": [
                {"id":"oauth", "source":{"kind":"oauth","mode":"owned","future":true}},
                {"id":"linked", "source":{"kind":"cpa","mode":"linked","dir":"ignored"}},
                {"id":"broken", "source":{"future":true}},
                {"id":"adopted", "source":{"kind":"cpa","mode":"adopted"},
                 "token_expires_at":"2099-10-23T02:00:00Z","last_refresh_at":"2026-10-18T08:00:00Z"}
            ]
        }))
        .unwrap();
        assert_eq!(snapshot.accounts[0].source, "oauth");
        assert_eq!(snapshot.accounts[1].source, "linked");
        assert!(
            snapshot.accounts[2]
                .source
                .starts_with("⚠ source decode error")
        );
        assert_eq!(snapshot.accounts[0].provider, "");
        assert_eq!(snapshot.accounts[3].source, "adopted");
        assert_eq!(
            snapshot.accounts[3].last_refresh_at.as_deref(),
            Some("2026-10-18T08:00:00Z")
        );
        let rows = qb_tray::parse_state(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        assert_eq!(rows[3].source, "adopted");
        assert_eq!(qb_tray::account_menu_metadata(&rows[3])[0], "직접 갱신");
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
