//! Client for the local core admin API. Requests run on the async worker, never the UI thread.
//!
//! This module deliberately has no dependency on the Win32 shell.  The tray
//! shell can poll [`AdminClient`] and render the returned snapshot later.

use std::{fs, path::Path, time::Duration};

pub use qb_proto::admin_login::{LoginStart, LoginStatus};
use reqwest::{StatusCode, header};
use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, Deserializer},
};

#[path = "client/snapshot.rs"]
pub mod snapshot;

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
            // The authenticated admin API is local; redirects must never escape it.
            .redirect(reqwest::redirect::Policy::none())
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

    pub async fn dashboard_sessions(
        &self,
        range: qb_tray::state::Range,
    ) -> Result<qb_tray::dashboard_model::SessionsResponse, ClientError> {
        let bytes = self
            .dashboard_bytes(&format!("/admin/v1/sessions?range={}", range.query()))
            .await?;
        let wire: qb_proto::admin_sessions::SessionsView = Self::decode_dashboard_object(&bytes)?;
        Ok(wire.into())
    }

    /// Set the desired policy once. The acknowledgement does not assert movement.
    pub async fn set_session_urgent(&self, id: &str, urgent: bool) -> Result<(), ClientError> {
        if id.len() != 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ClientError::AccountAction(
                "긴급 설정 대상이 올바르지 않습니다",
            ));
        }
        let body = serde_json::json!({"kind":"urgent", "id":id,
            "on_5h_limit": if urgent { "move" } else { "wait" }});
        let mut response = Self::check_response(
            self.request(reqwest::Method::POST, "/admin/v1/routing")
                .json(&body)
                .send()
                .await
                .map_err(ClientError::Transport)?,
        )
        .await?;
        const LIMIT: usize = 4096;
        let invalid = || ClientError::AccountAction("긴급 설정 응답을 확인할 수 없습니다");
        if response.content_length().is_some_and(|n| n > LIMIT as u64) {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(ClientError::Transport)? {
            if chunk.len() > LIMIT.saturating_sub(bytes.len()) {
                return Err(invalid());
            }
            bytes.extend_from_slice(&chunk);
        }
        let ack: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if ack.get("updated").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(invalid());
        }
        Ok(())
    }

    pub async fn dashboard_session(
        &self,
        id: &str,
    ) -> Result<qb_tray::dashboard_model::SessionDetail, ClientError> {
        if id.len() != 12
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ClientError::AccountAction("세션 번호가 올바르지 않습니다"));
        }
        let bytes = self
            .dashboard_bytes(&format!("/admin/v1/sessions/{id}"))
            .await?;
        let wire: qb_proto::admin_session_detail::SessionDetail =
            Self::decode_dashboard_object(&bytes)?;
        if wire.session.id.as_str() != id {
            return Err(ClientError::AccountAction(
                "대시보드 세션 응답이 요청과 다릅니다",
            ));
        }
        Ok(wire.into())
    }

    pub async fn dashboard_activity(
        &self,
        days: u8,
    ) -> Result<qb_tray::dashboard_model::ActivityResponse, ClientError> {
        if !(1..=7).contains(&days) {
            return Err(ClientError::AccountAction(
                "기록 조회 기간이 올바르지 않습니다",
            ));
        }
        self.dashboard_get(
            &format!("/admin/v1/activity?days={days}&limit=2000"),
            "events",
        )
        .await
    }

    pub async fn dashboard_usage_history(
        &self,
    ) -> Result<qb_tray::dashboard_model::UsageHistory, ClientError> {
        self.dashboard_get("/admin/v1/usage-history?hours=48", "accounts")
            .await
    }

    async fn dashboard_get<T: DeserializeOwned>(
        &self,
        path: &str,
        required: &str,
    ) -> Result<T, ClientError> {
        Self::decode_dashboard(self.dashboard_value(path).await?, required)
    }

    pub(crate) fn decode_dashboard_object<T: DeserializeOwned>(
        bytes: &[u8],
    ) -> Result<T, ClientError> {
        let invalid = || ClientError::AccountAction("대시보드 응답 형식이 올바르지 않습니다");
        // Preserve the previous named-object boundary without normalizing raw
        // JSON through Value before the DTO can reject duplicate fields.
        if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
            return Err(invalid());
        }
        serde_json::from_slice(bytes).map_err(|_| invalid())
    }

    pub(crate) fn decode_dashboard<T: DeserializeOwned>(
        value: serde_json::Value,
        required: &str,
    ) -> Result<T, ClientError> {
        let invalid = || ClientError::AccountAction("대시보드 응답 형식이 올바르지 않습니다");
        if !value.get(required).is_some_and(|field| {
            if required == "id" {
                field.is_string()
            } else {
                field.is_array()
            }
        }) {
            return Err(invalid());
        }
        serde_json::from_value(value).map_err(|_| invalid())
    }

    pub(crate) async fn dashboard_value(
        &self,
        path: &str,
    ) -> Result<serde_json::Value, ClientError> {
        let bytes = self.dashboard_bytes(path).await?;
        let invalid = || ClientError::AccountAction("대시보드 응답 형식이 올바르지 않습니다");
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }

    async fn dashboard_bytes(&self, path: &str) -> Result<Vec<u8>, ClientError> {
        const MAX: usize = 8 * 1024 * 1024;
        test_response_delay().await;
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        let mut response = Self::check_response(response).await?;
        let too_large = || ClientError::AccountAction("대시보드 응답이 너무 큽니다");
        if response.content_length().is_some_and(|n| n > MAX as u64) {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(ClientError::Transport)? {
            if chunk.len() > MAX.saturating_sub(bytes.len()) {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
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
    ) -> Result<PresentationAccount, ClientError> {
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
    ) -> Result<PresentationAccount, ClientError> {
        test_response_delay().await;
        let response = self
            .account_action_request(id, action)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        Self::check_response(response)
            .await?
            .json()
            .await
            .map_err(ClientError::Decode)
    }

    fn account_action_request(&self, id: &str, action: AccountAction) -> reqwest::RequestBuilder {
        let path = format!("/admin/v1/accounts/{id}/{}", action.as_str());
        let body = if action == AccountAction::ReleaseHold {
            serde_json::json!({"confirm": id})
        } else {
            serde_json::json!({})
        };
        self.request(reqwest::Method::POST, &path).json(&body)
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

    pub async fn takeover_inspect(
        &self,
        provider: &str,
        path: &str,
    ) -> Result<TakeoverInspect, ClientError> {
        self.send_json(
            reqwest::Method::POST,
            "/admin/v1/accounts/takeover",
            &serde_json::json!({"provider":provider,"path":path,"phase":"inspect"}),
        )
        .await
    }

    pub async fn takeover_commit(
        &self,
        provider: &str,
        path: &str,
        inspect: &TakeoverInspect,
        replace: bool,
    ) -> Result<TakeoverCommit, ClientError> {
        // Refresh follows commit in core; do not retry this mutation in the tray.
        let response = self
            .request(reqwest::Method::POST, "/admin/v1/accounts/takeover")
            .timeout(Duration::from_secs(40))
            .json(&takeover_commit_body(provider, path, inspect, replace))
            .send()
            .await
            .map_err(ClientError::Transport)?;
        Self::check_response(response)
            .await?
            .json()
            .await
            .map_err(ClientError::Decode)
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
        test_response_delay().await;
        let response = self
            .request(method, path)
            .json(body)
            .send()
            .await
            .map_err(ClientError::Transport)?;
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
        // Delay admission on the worker, not an already-timed response body.
        test_response_delay().await;
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(ClientError::Transport)?;
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
    ReleaseHold,
    Pause,
    Resume,
    Disable,
    Enable,
    RefreshUsage,
}

impl AccountAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReleaseHold => "release-hold",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Disable => "disable",
            Self::Enable => "enable",
            Self::RefreshUsage => "refresh-usage",
        }
    }

    #[cfg(test)]
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "release-hold" => Some(Self::ReleaseHold),
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

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("admin API transport failed: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("admin API returned HTTP {0}")]
    Http(StatusCode),
    #[error("{}", qb_tray::i18n::static_text(.0))]
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
    qb_tray::i18n::static_text(match error {
        ClientError::AccountAction(message) => message,
        ClientError::Transport(_) => "코어에 연결할 수 없습니다",
        ClientError::Http(StatusCode::TOO_MANY_REQUESTS) => "잠시 후 다시 시도해 주세요",
        ClientError::Http(StatusCode::NOT_FOUND) => "계정을 찾을 수 없습니다",
        ClientError::Http(_) => "요청을 처리하지 못했습니다",
        ClientError::Decode(_) => "코어 응답을 읽지 못했습니다",
    })
}

pub use qb_proto::HealthResponse;

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
            reason: qb_tray::i18n::format("코어 health가 false입니다 ({})", &[health.version]),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrayRead<S = TraySnapshot> {
    pub health: CoreHealth,
    pub snapshot: S,
}

/// Convert the admin snapshot into the model consumed by both the live
/// Direct2D flyout and the WIC review renderer.
pub fn layout_rows<S: Serialize>(
    read: &TrayRead<S>,
    now: std::time::SystemTime,
) -> anyhow::Result<Vec<qb_tray::Row>> {
    let json = serde_json::to_string(&read.snapshot)?;
    Ok(qb_tray::parse_state_at(&json, now)?)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MovePair {
    pub from: String,
    pub to: String,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TrayWarning {
    #[serde(default)]
    pub model_limited: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub burst_new_sessions: Option<u64>,
    #[serde(default)]
    pub move_list: Vec<MovePair>,
    #[serde(default)]
    pub reread_tokens: Option<u64>,
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

/// Presentation adapter for /state only. The permissive flat SSE warning DTO
/// remains separate; it cannot add variants to the typed state protocol.
impl From<&StateWarning> for TrayWarning {
    fn from(warning: &StateWarning) -> Self {
        use qb_proto::admin_warning::AdminWarning;
        match warning {
            AdminWarning::Burst {
                burst_new_sessions,
                observed_at,
            } => Self {
                kind: "burst".into(),
                burst_new_sessions: Some(*burst_new_sessions),
                observed_at: Some(*observed_at),
                ..Self::default()
            },
            AdminWarning::MoveReread {
                observed_at,
                reread_tokens,
                ..
            } => Self {
                kind: "move_reread".into(),
                reread_tokens: Some(*reread_tokens),
                observed_at: Some(*observed_at),
                ..Self::default()
            },
            AdminWarning::MoveBreaker {
                observed_at,
                move_list,
                ..
            } => Self {
                kind: "move_breaker".into(),
                observed_at: Some(*observed_at),
                move_list: move_list
                    .iter()
                    .map(|pair| MovePair {
                        from: pair.from.clone(),
                        to: pair.to.clone(),
                    })
                    .collect(),
                ..Self::default()
            },
        }
    }
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
        let account: PresentationAccount =
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
        let account: PresentationAccount =
            serde_json::from_value(json!({"allow_credits":true})).unwrap();
        assert!(account.quota_notice.is_none());
    }
}

pub type StateWarning = qb_proto::admin_warning::AdminWarning<chrono::DateTime<chrono::Utc>>;
pub type StatePools = Option<qb_proto::admin_pool::ReadPools>;
#[path = "client/presentation.rs"]
pub mod presentation;
pub use presentation::{PresentationAccount, PresentationSnapshot};

/// Raw current /state payload. Account fields are the exact shared contract;
/// legacy envelope defaults remain at the explicit envelope read boundary.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct TraySnapshot(pub qb_proto::AdminState<qb_proto::AdminAccount, StateWarning, StatePools>);
impl std::ops::Deref for TraySnapshot {
    type Target = qb_proto::AdminState<qb_proto::AdminAccount, StateWarning, StatePools>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for TraySnapshot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<'de> Deserialize<'de> for TraySnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        qb_proto::admin_state::deserialize_compat(deserializer, false).map(Self)
    }
}
impl TraySnapshot {
    /// A pure, temporary presentation copy. No normalization/default writes back
    /// into the raw snapshot. Existing date validation and source tags stay in
    /// the incumbent presentation decoder.
    pub fn presentation(&self) -> serde_json::Result<PresentationSnapshot> {
        serde_json::from_value(serde_json::to_value(self)?)
    }
}

#[cfg(test)]
mod n10_outside_usage_transport_tests {
    use super::*;

    #[test]
    fn n10_outside_used_survives_actual_dto_and_layout_input_with_legacy_default() {
        for flag in [None, Some(false), Some(true)] {
            let mut wire = serde_json::json!({"accounts":[{
                "id":"synthetic-outside", "provider":"codex", "plan":"plus",
                "user_state":"active", "health":{"kind":"ready"},
                "credits_in_use":true, "allow_credits":true,
                "source":{"kind":"cpa","mode":"adopted"}
            }]});
            if let Some(flag) = flag {
                wire["accounts"][0]["outside_used"] = flag.into();
            }
            let snapshot: PresentationSnapshot = serde_json::from_value(wire).unwrap();
            assert_eq!(snapshot.accounts[0].outside_used, flag.unwrap_or(false));
            let read = TrayRead {
                snapshot,
                health: CoreHealth::Healthy(HealthResponse {
                    ok: true,
                    version: "synthetic".into(),
                    pid: 1,
                    started_at: "2026-10-23T00:00:00Z".into(),
                }),
            };
            let now = "2026-10-23T00:00:00Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap()
                .into();
            let rows = layout_rows(&read, now).unwrap();
            let view = qb_tray::v2::project(rows[0].view_input.as_deref().unwrap(), now).unwrap();
            let row = &view.sections[1].rows[0];
            assert_eq!(row.raw.outside_used, flag.unwrap_or(false));
            assert_eq!(row.state, qb_tray::v2::DisplayState::Credits);
            assert!(row.raw.allow_credits && row.raw.source.owned_or_adopted());
        }
    }
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
            "quota_cooling":{"window":"5h","reset_at":reset,"started_at":at},"quota_credit_releasable":true,
            "windows":[{"name":"5h","used_pct":100.0,"reset_at":reset,"observed_at":at,"scope":"account"}],
            "pacing":{"backoff_until":reset,"waiting":2,"step":3,"release_interval_ms":500}
        }]});
        let snapshot: PresentationSnapshot = serde_json::from_value(input).unwrap();
        let serialized = serde_json::to_value(&snapshot).unwrap();
        let account = &serialized["accounts"][0];
        assert_eq!(account["credits_in_use"], true);
        assert_eq!(account["pool_eligible"], true);
        assert_eq!(account["active_sessions"], 2);
        assert_eq!(account["spread_threshold"], 8);
        assert_eq!(account["credits"]["balance"], 19395.23);
        assert_eq!(account["quota_cooling"]["window"], "5h");
        assert_eq!(account["quota_credit_releasable"], true);
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
        assert!(codex.rows[0].raw.quota_credit_releasable);
        assert!(codex.rows[0].raw.source.owned_or_adopted());
        assert_eq!(view.summaries[1].verdict, qb_tray::v2::Verdict::Credits);
    }
}

/// Render a compact Korean tooltip line without exposing any secret fields.
#[cfg(test)]
pub fn render_status_line(read: &TrayRead<PresentationSnapshot>) -> String {
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
pub fn render_flyout(read: &TrayRead<PresentationSnapshot>) -> String {
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
            AccountAction::ReleaseHold,
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
    fn release_hold_request_is_authenticated_and_confirms_only_the_selected_account() {
        let client = AdminClient::new(10201, "synthetic-token").unwrap();
        for id in ["cx-synthetic-a", "cx-synthetic-b"] {
            let request = client
                .account_action_request(id, AccountAction::ReleaseHold)
                .build()
                .unwrap();
            assert_eq!(request.method(), reqwest::Method::POST);
            assert_eq!(
                request.url().as_str(),
                format!("http://127.0.0.1:10201/admin/v1/accounts/{id}/release-hold")
            );
            assert_eq!(
                request.headers()[header::AUTHORIZATION],
                "Bearer synthetic-token"
            );
            assert_eq!(request.headers()[header::HOST], "127.0.0.1:10201");
            let body: serde_json::Value =
                serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
            assert_eq!(body, serde_json::json!({"confirm": id}));
        }
        let request = client
            .account_action_request("cx-synthetic-a", AccountAction::RefreshUsage)
            .build()
            .unwrap();
        assert_eq!(request.body().unwrap().as_bytes().unwrap(), b"{}");
    }

    #[test]
    fn parses_synthetic_state_snapshot() {
        let snapshot: PresentationSnapshot = serde_json::from_str(r#"{
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
        let snapshot: PresentationSnapshot = serde_json::from_value(serde_json::json!({
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
        let snapshot: PresentationSnapshot = serde_json::from_value(serde_json::json!({
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

/// Allowlist only. Never retain or render unknown response fields or a token.
fn takeover_commit_body(
    provider: &str,
    path: &str,
    inspect: &TakeoverInspect,
    replace: bool,
) -> serde_json::Value {
    serde_json::json!({"provider":provider,"path":path,"phase":"commit",
        "inspect_digest":inspect.inspect_digest,"replace":replace,"keep_source":false})
}

#[derive(Clone, Deserialize)]
pub struct TakeoverInspect {
    pub email: Option<String>,
    pub plan: Option<String>,
    pub provider_account_id: Option<String>,
    pub source_path: String,
    pub source_last_refresh: Option<String>,
    pub source_mtime: Option<String>,
    pub owner_alive: bool,
    pub existing_id: Option<String>,
    pub inspect_digest: String,
}
impl TakeoverInspect {
    pub fn can_confirm(&self) -> bool {
        !self.owner_alive
            && !self.inspect_digest.trim().is_empty()
            && !self.source_path.trim().is_empty()
            && self
                .existing_id
                .as_deref()
                .is_none_or(|id| !id.trim().is_empty())
            && [
                Some(self.source_path.as_str()),
                Some(self.inspect_digest.as_str()),
                self.email.as_deref(),
                self.plan.as_deref(),
                self.provider_account_id.as_deref(),
                self.source_last_refresh.as_deref(),
                self.source_mtime.as_deref(),
                self.existing_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            .all(|s| !s.chars().any(unsafe_display_char))
    }
    pub fn identity(&self, provider: &str) -> String {
        fn field(value: Option<&str>) -> String {
            value
                .map(display_field)
                .unwrap_or_else(|| qb_tray::i18n::text("알 수 없음").into_owned())
        }
        qb_tray::i18n::format(
            "제공자: {}\n이메일: {}\n요금제: {}\n제공자 계정 ID: {}\n원본 경로: {}\n마지막 갱신: {}\n파일 수정 시각: {}\n원래 앱 실행 중: {}{}",
            &[
                field(Some(provider)),
                field(self.email.as_deref()),
                field(self.plan.as_deref()),
                field(self.provider_account_id.as_deref()),
                field(Some(&self.source_path)),
                field(self.source_last_refresh.as_deref()),
                field(self.source_mtime.as_deref()),
                qb_tray::i18n::text(if self.owner_alive {
                    "예 · 먼저 종료하고 다시 검사하세요"
                } else {
                    "감지되지 않음 (최선의 확인)"
                })
                .into_owned(),
                self.existing_id
                    .as_ref()
                    .map(|id| {
                        qb_tray::i18n::format(
                            "\n기존 계정: {} · 다음 단계에서 교체 확인이 필요합니다",
                            &[field(Some(id))],
                        )
                    })
                    .unwrap_or_default(),
            ],
        )
    }
    pub fn warning(&self) -> String {
        // D18 exact Korean wording. No CLI keep-source option is exposed here.
        qb_tray::i18n::format(
            "가져오면 quota-baton이 이 계정의 토큰을 관리합니다. 원래 앱({})은 로그아웃되며, 다시 쓰려면 그 앱에서 새로 로그인해야 합니다. 원래 앱이 켜져 있으면 먼저 종료하세요.",
            &[display_field(&self.source_path)],
        )
    }
}
fn unsafe_display_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}
fn display_field(value: &str) -> String {
    // Preserve evidence that a character was rejected rather than joining its neighbours.
    value
        .chars()
        .map(|c| {
            if unsafe_display_char(c) {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}
#[allow(dead_code)]
#[derive(Deserialize)]
pub struct TakeoverCommit {
    pub id: String,
    pub renamed_to: Option<String>,
    pub refresh: TakeoverRefresh,
}
#[allow(dead_code)]
#[derive(Deserialize)]
pub struct TakeoverRefresh {
    pub ok: bool,
    pub token_expires_at: Option<String>,
}

#[cfg(test)]
mod r6_takeover_tests {
    use super::*;
    fn inspect() -> TakeoverInspect {
        serde_json::from_value(serde_json::json!({"source_path":"C:/synthetic/home",
            "owner_alive":false,"inspect_digest":"digest"}))
        .unwrap()
    }
    #[test]
    fn r6_spoofed_or_incomplete_inspect_cannot_authorize_commit() {
        for c in [
            '\0', '\n', '\r', '\t', '\u{061c}', '\u{200e}', '\u{200f}', '\u{2028}', '\u{2029}',
            '\u{202e}', '\u{2066}',
        ] {
            for field in [
                "email",
                "plan",
                "provider_account_id",
                "source_path",
                "source_last_refresh",
                "source_mtime",
                "existing_id",
                "inspect_digest",
            ] {
                let mut value = serde_json::json!({"source_path":"C:/synthetic/home", "owner_alive":false,"inspect_digest":"digest"});
                value[field] = format!("before{c}after").into();
                let unsafe_inspect: TakeoverInspect = serde_json::from_value(value).unwrap();
                assert!(!unsafe_inspect.can_confirm(), "{field}: {c:?}");
                assert!(!unsafe_inspect.warning().contains(c));
                assert!(!unsafe_inspect.identity("Codex").contains(c) || c == '\n');
            }
        }
        for field in ["owner_alive", "source_path", "inspect_digest"] {
            let mut value = serde_json::json!({"source_path":"C:/synthetic/home", "owner_alive":false,"inspect_digest":"digest"});
            value.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<TakeoverInspect>(value).is_err());
        }
        let mut value = inspect();
        value.existing_id = Some(" ".into());
        assert!(!value.can_confirm());
        value.existing_id = None;
        value.inspect_digest = " ".into();
        assert!(!value.can_confirm());
    }
    // Synthetic loopback only. An admin redirect must not reach even a second
    // local listener, for either the read-only inspect or the commit mutation.
    #[tokio::test]
    async fn r6_takeover_never_follows_admin_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for commit in [false, true] {
            let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            destination.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let target = destination.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 8192);
                }
                let headers = String::from_utf8(request).unwrap();
                assert!(headers.starts_with("POST /admin/v1/accounts/takeover HTTP/1.1\r\n"));
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                assert!(length < 8192);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    body,
                    if commit {
                        takeover_commit_body("codex", "C:/synthetic/home", &inspect(), false)
                    } else {
                        serde_json::json!({"provider":"codex","path":"C:/synthetic/home","phase":"inspect"})
                    }
                );
                stream.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target}/unexpected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            });
            let client = AdminClient::new(port, "synthetic-admin-token").unwrap();
            let result = tokio::time::timeout(Duration::from_secs(3), async {
                if commit {
                    client
                        .takeover_commit("codex", "C:/synthetic/home", &inspect(), false)
                        .await
                        .map(|_| ())
                } else {
                    client
                        .takeover_inspect("codex", "C:/synthetic/home")
                        .await
                        .map(|_| ())
                }
            })
            .await
            .expect("redirect must return without contacting its target");
            assert!(matches!(
                result,
                Err(ClientError::Http(StatusCode::TEMPORARY_REDIRECT))
            ));
            server.await.unwrap();
            assert_eq!(
                destination.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
    #[test]
    fn safe_identity_warning_and_legacy_owned_source() {
        let inspect: TakeoverInspect = serde_json::from_value(serde_json::json!({
            "email":"synthetic@example.invalid", "plan":"pro", "provider_account_id":"synthetic-id",
            "source_path":"C:/synthetic/home", "source_last_refresh":null, "source_mtime":"2026-10-19T00:00:00Z",
            "owner_alive":false,"inspect_digest":"digest", "existing_id":"existing",
            "access_token":"NEVER DISPLAY", "refresh_token":"NEVER DISPLAY"
        })).unwrap();
        assert!(inspect.can_confirm());
        let text = inspect.identity("Codex");
        for field in [
            "synthetic@example.invalid",
            "pro",
            "synthetic-id",
            "C:/synthetic/home",
            "알 수 없음",
            "2026-10-19T00:00:00Z",
            "감지되지 않음",
            "existing",
        ] {
            assert!(text.contains(field));
        }
        assert!(!text.contains("NEVER DISPLAY"));
        for replace in [false, true] {
            assert_eq!(
                takeover_commit_body("codex", "C:/synthetic/home", &inspect, replace),
                serde_json::json!({
                    "provider":"codex","path":"C:/synthetic/home","phase":"commit",
                    "inspect_digest":"digest","replace":replace,"keep_source":false
                })
            );
        }
        assert_eq!(
            inspect.warning(),
            "가져오면 quota-baton이 이 계정의 토큰을 관리합니다. 원래 앱(C:/synthetic/home)은 로그아웃되며, 다시 쓰려면 그 앱에서 새로 로그인해야 합니다. 원래 앱이 켜져 있으면 먼저 종료하세요."
        );
        let mut alive = inspect.clone();
        alive.owner_alive = true;
        assert!(!alive.can_confirm());
        alive.owner_alive = false;
        alive.inspect_digest.clear();
        assert!(!alive.can_confirm());
        for source in [
            serde_json::json!("takeover"),
            serde_json::json!({"kind":"takeover","from":"DO NOT LEAK","at":"now"}),
        ] {
            let account: PresentationAccount =
                serde_json::from_value(serde_json::json!({"source":source})).unwrap();
            assert_eq!(account.source, "oauth");
            assert!(
                !serde_json::to_string(&account)
                    .unwrap()
                    .contains("DO NOT LEAK")
            );
            let rows = qb_tray::parse_state(&serde_json::json!({"accounts":[account]}).to_string())
                .unwrap();
            assert_eq!(rows[0].source, "oauth");
            assert_eq!(qb_tray::account_menu_metadata(&rows[0])[0], "직접 갱신");
            let source: qb_tray::v2::Source = serde_json::from_value(source).unwrap();
            assert!(source.owned_or_adopted());
        }
    }
}

#[cfg(test)]
mod r7_dashboard_client_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn fake(
        body: &str,
        declared: Option<usize>,
    ) -> (AdminClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(![10100, 10101].contains(&port));
        let body = body.as_bytes().to_vec();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                assert!(request.len() < 8192);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.contains("authorization: bearer synthetic-r7-admin"));
            assert!(request.contains(&format!("host: 127.0.0.1:{port}")));
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                declared.unwrap_or(body.len())
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            let _ = socket.write_all(&body).await;
        });
        (AdminClient::new(port, "synthetic-r7-admin").unwrap(), task)
    }
    fn detail_body(id: &str) -> String {
        let mut value: serde_json::Value = serde_json::from_str(r#"{"id":"123456abcdef","provider":"codex","client":{"kind":"codex_cli","product":"synthetic","version":"1"},"label":{"text":"synthetic-folder","source":"claude_project","best_effort":true},"account":"cx-000001","first_seen_at":"2026-10-24T00:00:00Z","bound_since":"2026-10-24T00:00:00Z","last_used_at":"2026-10-24T00:00:00Z","active":true,"inflight":{"requests":2,"oldest_started_at":"2026-10-24T00:00:00Z","ws_open":1,"ws_response_in_flight":true},"inflight_known":false,"pacing_wait":{"waiting":2,"since":"2026-10-24T00:00:00Z"},"routing":{"action_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","on_5h_limit":"move","mode":"recover"},"quota_wait":{"waiting":1,"reset_at":"2026-10-24T00:00:00Z"},"quota_wait_known":true,"model":"synthetic","subagents_today":1,"requests_today":5,"failures_today":2,"rate_wait_today":{"count":1,"total_ms":9},"moves_today":[{"at":"2026-10-24T00:00:00Z","from":"cl-000002","to":"cx-000001","reason":"reset_hold_timeout","next_turn_cache":"miss"}],"first_account_today":"cl-000002","marked_for_move":true,"counters_complete":false,"truncated":true,"subagents_known":false,"moves_known":false,"metadata_truncated":true,"original_short_id":"12345678","recent_requests":[{"ts":"2026-10-24T00:00:00Z","outcome":"relayed_rate429","status":429,"ttfb_ms":3,"dur_ms":17,"rate_wait_ms":10,"rate_retries":4294967295,"failover":{"from":"cl-000002","to":"cx-000001"}},{"ts":"2026-10-24T00:00:00Z","outcome":"ok","status":200,"ttfb_ms":1,"dur_ms":4,"rate_wait_ms":0,"rate_retries":0,"failover":null}],"timeline":[{"at":"2026-10-24T00:00:00Z","kind":"placed","account":"cl-000002","from":null,"reason":"new","next_turn_cache":null},{"at":"2026-10-24T00:00:00Z","kind":"move","account":"cx-000001","from":"cl-000002","reason":"reset_hold_timeout","next_turn_cache":"hit"}],"recent_requests_known":false,"timeline_known":false}"#).unwrap();
        value["id"] = serde_json::json!(id);
        serde_json::to_string(&value).unwrap()
    }

    #[tokio::test]
    async fn dashboard_detail_exact_wire_rejects_missing_and_duplicate_fields_without_echo() {
        let complete = detail_body("123456abcdef");
        let (client, task) = fake(&complete, None).await;
        let detail = client.dashboard_session("123456abcdef").await.unwrap();
        assert!(detail.session.label.as_ref().unwrap().best_effort);
        assert_eq!(detail.original_short_id.as_deref(), Some("12345678"));
        assert_eq!(detail.recent_requests.len(), 2);
        assert!(!detail.recent_requests_known && !detail.timeline_known);
        task.await.unwrap();
        let mut missing_original: serde_json::Value = serde_json::from_str(&complete).unwrap();
        missing_original
            .as_object_mut()
            .unwrap()
            .remove("original_short_id");
        let mut missing_status: serde_json::Value = serde_json::from_str(&complete).unwrap();
        missing_status["recent_requests"][0]
            .as_object_mut()
            .unwrap()
            .remove("status");
        let duplicate = complete.replacen(
            "\"timeline_known\":",
            "\"timeline_known\":false,\"timeline_known\":",
            1,
        );
        for body in [
            serde_json::to_string(&missing_original).unwrap(),
            serde_json::to_string(&missing_status).unwrap(),
            duplicate,
            r#"{"id":"123456abcdef","original_short_id":"12345678"}"#.into(),
        ] {
            let (client, task) = fake(&body, None).await;
            let error = client.dashboard_session("123456abcdef").await.unwrap_err();
            assert!(!error.to_string().contains("12345678"));
            task.await.unwrap();
        }
        let (client, task) = fake("", Some(8 * 1024 * 1024 + 1)).await;
        assert!(
            client
                .dashboard_session("123456abcdef")
                .await
                .unwrap_err()
                .to_string()
                .contains("너무 큽니다")
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn dashboard_empty_container_is_valid_but_missing_schema_never_becomes_no_sessions() {
        let body = r#"{"now":"2026-10-24T00:00:00Z","active_window_secs":60,"sessions":[],"truncated":false,"move_cost":null}"#;
        let (client, task) = fake(body, None).await;
        assert!(
            client
                .dashboard_sessions(qb_tray::state::Range::Hour)
                .await
                .unwrap()
                .sessions
                .is_empty()
        );
        task.await.unwrap();
        for body in [
            r#"[null,"2026-10-24T00:00:00Z",60,[],false]"#,
            r#"{"error":"synthetic-secret-must-not-echo"}"#,
            r#"{"sessions":null}"#,
            r#"{"sessions":[]}"#,
            r#"{"now":"2026-10-24T00:00:00Z","active_window_secs":60,"sessions":[],"sessions":[],"truncated":false,"move_cost":null}"#,
            "[]",
        ] {
            let (client, task) = fake(body, None).await;
            let error = client
                .dashboard_sessions(qb_tray::state::Range::Hour)
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("synthetic-secret-must-not-echo"));
            task.await.unwrap();
        }
    }
    #[tokio::test]
    async fn dashboard_response_cap_and_selected_identity_are_enforced_without_echo() {
        let (client, task) = fake("", Some(8 * 1024 * 1024 + 1)).await;
        assert!(
            client
                .dashboard_usage_history()
                .await
                .unwrap_err()
                .to_string()
                .contains("너무 큽니다")
        );
        task.await.unwrap();
        let other = detail_body("000000000000");
        let (client, task) = fake(&other, None).await;
        let error = client.dashboard_session("123456abcdef").await.unwrap_err();
        assert!(error.to_string().contains("요청과 다릅니다"));
        assert!(!error.to_string().contains("12345678"));
        task.await.unwrap();
    }
}

#[cfg(test)]
mod n5c_urgency_client_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn exchange(reply: String, urgent: bool) -> Result<(), ClientError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = vec![];
            let mut byte = [0];
            while !raw.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).await.unwrap();
                raw.push(byte[0]);
                assert!(raw.len() < 8192);
            }
            let headers = String::from_utf8(raw).unwrap();
            assert!(headers.starts_with("POST /admin/v1/routing HTTP/1.1\r\n"));
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer synthetic-n5c-token\r\n")
            );
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains(&format!("host: 127.0.0.1:{port}\r\n"))
            );
            let n: usize = headers
                .lines()
                .find_map(|line| {
                    let (k, v) = line.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().unwrap())
                })
                .unwrap();
            assert!(n < 1024);
            let mut body = vec![0; n];
            socket.read_exact(&mut body).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                serde_json::json!({"kind":"urgent","id":"a".repeat(64),"on_5h_limit":if urgent {"move"} else {"wait"}})
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            AdminClient::new(port, "synthetic-n5c-token")
                .unwrap()
                .set_session_urgent(&"a".repeat(64), urgent),
        )
        .await
        .unwrap();
        server.await.unwrap();
        result
    }
    fn reply(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
    #[tokio::test]
    async fn n5c_urgency_client_posts_exact_desired_state_and_checks_acknowledgement() {
        for urgent in [false, true] {
            assert!(exchange(reply(r#"{"updated":true}"#), urgent).await.is_ok());
        }
        for body in [
            r#"{"updated":false}"#,
            r#"{}"#,
            r#"{"updated":"true"}"#,
            "not JSON",
        ] {
            assert!(matches!(
                exchange(reply(body), true).await,
                Err(ClientError::AccountAction(_))
            ));
        }
    }
    #[tokio::test]
    async fn n5c_urgency_client_rejects_display_ids_before_io_and_bounds_chunked_ack() {
        let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let client = AdminClient::new(
            destination.local_addr().unwrap().port(),
            "synthetic-n5c-token",
        )
        .unwrap();
        for id in [
            "a".repeat(12),
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(65),
            String::new(),
        ] {
            assert!(matches!(
                client.set_session_urgent(&id, true).await,
                Err(ClientError::AccountAction(_))
            ));
        }
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let body = format!(r#"{{"updated":true,"extra":"{}"}}"#, "x".repeat(4096));
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            body
        );
        assert!(matches!(
            exchange(chunked, true).await,
            Err(ClientError::AccountAction(_))
        ));
    }
}

#[cfg(test)]
mod r15_envelope_transport_tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn r15_complete_state_envelope_survives_tray_and_lossless_cli_decode() {
        let wire = json!({
            "pools":{"claude":{"eligible":0,"cooling":{},"unavailable":0},"codex":{"eligible":1,"cooling":{},"unavailable":0}},
            "warnings":[],
            "draining":false,
            "capabilities":{"account_adopt":true,"manual_refresh":true,"account_takeover":true},
            "accounts":[],
            "inflight":{"requests":2,"bytes":17},
            "memory":{"private_bytes":23},
            "bindings":{"count":1},
            "data_port":12345,
            "version":"synthetic"
        });
        let tray: TraySnapshot = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&tray).unwrap(), wire);
        let cli: qb_proto::admin_state::AdminState<Value, Value, Value> =
            qb_proto::admin_state::deserialize_compat(wire.clone(), true).unwrap();
        assert_eq!(serde_json::to_value(cli).unwrap(), wire);
        assert_eq!(tray.draining, Some(false));
        assert!(tray.capabilities.as_ref().unwrap().manual_refresh);
    }

    #[test]
    fn r15_legacy_decoder_keeps_unknown_and_requires_cli_account_array() {
        let old = json!({"accounts":[]});
        let tray: TraySnapshot = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(tray.draining, None);
        assert_eq!(tray.capabilities, None);
        assert!(tray.pools.is_none());
        assert_eq!(tray.inflight.requests, 0);
        let cli: qb_proto::admin_state::AdminState<Value, Value, Value> =
            qb_proto::admin_state::deserialize_compat(old, true).unwrap();
        assert!(cli.pools.is_null());
        assert!(
            qb_proto::admin_state::deserialize_compat::<_, Value, Value, Value>(json!({}), true)
                .is_err()
        );
        assert!(serde_json::from_value::<TraySnapshot>(json!({"accounts":null})).is_err());
        assert!(serde_json::from_value::<TraySnapshot>(json!({"inflight":null})).is_err());
    }

    #[test]
    fn r15_raw_tray_state_rejects_duplicate_envelope_and_metric_keys() {
        for wire in [
            r#"{"accounts":[],"accounts":[]}"#,
            r#"{"accounts":null,"accounts":[]}"#,
            r#"{"version":"old","version":"new"}"#,
            r#"{"inflight":{"requests":1,"requests":2}}"#,
            r#"{"inflight":{"requests":null,"requests":2}}"#,
            r#"{"inflight":{"bytes":1,"bytes":2}}"#,
            r#"{"inflight":{"bytes":null,"bytes":2}}"#,
            r#"{"memory":{"private_bytes":1,"private_bytes":2}}"#,
            r#"{"memory":{"private_bytes":null,"private_bytes":2}}"#,
            r#"{"bindings":{"count":1,"count":2}}"#,
            r#"{"bindings":{"count":null,"count":2}}"#,
            r#"{"capabilities":{"account_adopt":true,"account_adopt":false,"manual_refresh":true,"account_takeover":true}}"#,
        ] {
            assert!(
                serde_json::from_str::<TraySnapshot>(wire).is_err(),
                "{wire}"
            );
        }
        let legacy: TraySnapshot = serde_json::from_str(
            r#"{"accounts":[],"inflight":{"requests":4},"memory":{},"bindings":{}}"#,
        )
        .unwrap();
        assert_eq!(legacy.inflight.requests, 4);
        assert_eq!(legacy.inflight.bytes, 0);
        assert_eq!(legacy.memory.private_bytes, 0);
        assert_eq!(legacy.bindings.count, 0);
        assert!(legacy.pools.is_none());
        assert_eq!(legacy.draining, None);
        assert_eq!(legacy.capabilities, None);
    }
}

#[cfg(test)]
mod r15_dependency_strictness_tests {
    #[test]
    fn shared_state_and_health_reject_unknown_keys_in_a_consumer_test() {
        type State = qb_proto::AdminState<serde_json::Value, serde_json::Value, serde_json::Value>;
        let current = serde_json::json!({
            "pools":null,"warnings":[],"draining":false,"capabilities":null,
            "accounts":[],"inflight":{"requests":0,"bytes":0},
            "memory":{"private_bytes":0},"bindings":{"count":0},
            "data_port":12345,"version":"synthetic"
        });
        serde_json::from_value::<State>(current.clone()).unwrap();
        let mut unknown = current.clone();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<State>(unknown).is_err());
        let mut nested = current;
        nested["inflight"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<State>(nested).is_err());
        let health = r#"{"ok":true,"version":"synthetic","pid":7,"started_at":"2030-01-01T00:00:00Z","unexpected":true}"#;
        assert!(serde_json::from_str::<qb_proto::HealthResponse>(health).is_err());
    }
}

#[cfg(test)]
mod r15_pool_warning_transport_tests {
    use super::*;

    #[test]
    fn raw_pool_and_state_warning_duplicates_never_collapse() {
        for raw in [
            r#"{"accounts":[],"pools":{"codex":{"eligible":1,"eligible":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"eligible":"bad","eligible":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"eligible":null,"eligible":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":null,"codex":{"eligible":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"unavailable":null,"unavailable":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"cooling":{"auth":1,"auth":2}}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"cooling":{"auth":null,"auth":2}}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"cooling":null,"cooling":{}}}}"#,
            r#"{"accounts":[],"warnings":[{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":0,"unknown_count":1}]}"#,
            r#"{"accounts":[],"warnings":[{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":null,"unknown_count":1}]}"#,
            r#"{"accounts":[],"warnings":[{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":"bad","unknown_count":1}]}"#,
            r#"{"accounts":[],"warnings":[{"kind":"move_breaker","observed_at":"2030-01-01T00:00:00Z","move_list":[{"from":null,"from":"a","to":"b"}],"until":"2030-01-01T00:04:00Z","session":"0123456789ab","from":"a","to":"b","reason":"global_trip","forced":false}]}"#,
        ] {
            assert!(
                serde_json::from_str::<TraySnapshot>(raw).is_err(),
                "accepted {raw}"
            );
        }
    }

    #[test]
    fn legacy_sparse_pools_remain_unknown_and_warning_payload_is_preserved() {
        let raw = r#"{"accounts":[],"pools":{"codex":{"eligible":2}},"warnings":[{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":1}]}"#;
        let state = serde_json::from_str::<TraySnapshot>(raw).unwrap();
        let pools = state.pools.as_ref().unwrap();
        assert!(pools.claude.is_none());
        let codex = pools.codex.as_ref().unwrap();
        assert_eq!(codex.eligible, Some(2));
        assert!(codex.cooling.is_none());
        assert!(codex.unavailable.is_none());
        let wire = serde_json::to_value(&state.warnings[0]).unwrap();
        assert_eq!(wire["known_count"], 2);
        assert_eq!(wire["unknown_count"], 1);
        for raw in [r#"{"accounts":[]}"#, r#"{"accounts":[],"pools":null}"#] {
            assert!(
                serde_json::from_str::<TraySnapshot>(raw)
                    .unwrap()
                    .pools
                    .is_none()
            );
        }
        // /state does not adopt the separate SSE quota_notice schema.
        let raw = r#"{"accounts":[],"warnings":[{"kind":"quota_notice","account":"a"}]}"#;
        assert!(serde_json::from_str::<TraySnapshot>(raw).is_err());
    }
}

#[cfg(test)]
mod login_contract_tests {
    use super::{LoginStart, LoginStatus};
    use qb_proto::admin_login::LoginState;
    use serde_json::json;

    #[test]
    fn shared_login_responses_keep_wire_keys_and_omissions() {
        let start = json!({"login_id":"synthetic-login", "auth_url":"synthetic-url"});
        let decoded: LoginStart = serde_json::from_value(start.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), start);
        for (wire, state) in [
            (json!({"status":"pending"}), LoginState::Pending),
            (
                json!({"status":"completed", "account_id":"synthetic-account"}),
                LoginState::Completed,
            ),
            (
                json!({"status":"failed", "error":"synthetic-error"}),
                LoginState::Failed,
            ),
            (json!({"status":"cancelled"}), LoginState::Cancelled),
            (json!({"status":"expired"}), LoginState::Expired),
        ] {
            let decoded: LoginStatus = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(decoded.status, state);
            assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
        }
    }
}
