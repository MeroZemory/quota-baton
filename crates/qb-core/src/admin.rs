use std::{path::PathBuf, sync::Arc};

use crate::accounts::{AccountStore, StoreError, import};
use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use qb_proto::{CpaImportRequest, ImportMode};

#[derive(Clone)]
struct AccountAdmin {
    store: Arc<AccountStore>,
    poller: Option<Arc<crate::quota::UsagePoller>>,
}

pub fn account_routes(store: Arc<AccountStore>) -> Router {
    account_router(AccountAdmin {
        store,
        poller: None,
    })
}

/// Live core imports queue profile work; store-only callers remain offline.
pub fn account_routes_with_poller(
    store: Arc<AccountStore>,
    poller: Arc<crate::quota::UsagePoller>,
) -> Router {
    account_router(AccountAdmin {
        store,
        poller: Some(poller),
    })
}

fn account_router(state: AccountAdmin) -> Router {
    Router::new()
        .route("/admin/v1/accounts/import", post(import_cpa))
        .with_state(state)
}

#[derive(serde::Deserialize)]
pub struct LoginRequest {
    pub provider: crate::accounts::Provider,
    #[serde(default)]
    pub replace_linked: bool,
}

pub fn login_routes(service: crate::oauth::LoginService) -> Router {
    Router::new()
        .route("/admin/v1/accounts/login", post(start_login))
        .route(
            "/admin/v1/accounts/login/{login_id}",
            axum::routing::get(login_status),
        )
        .route(
            "/admin/v1/accounts/login/{login_id}/cancel",
            post(cancel_login),
        )
        .with_state(service)
}
async fn start_login(
    State(service): State<crate::oauth::LoginService>,
    request: Result<Json<LoginRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST, "로그인 요청이 올바르지 않습니다.");
    };
    match service
        .start(request.provider, request.replace_linked)
        .await
    {
        Ok(start) => Json(start).into_response(),
        Err(error) => login_error(error),
    }
}
async fn login_status(
    State(service): State<crate::oauth::LoginService>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match service.status(&id).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => login_error(error),
    }
}
async fn cancel_login(
    State(service): State<crate::oauth::LoginService>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match service.cancel(&id).await {
        Ok(()) => match service.status(&id).await {
            Ok(status) => Json(status).into_response(),
            Err(error) => login_error(error),
        },
        Err(error) => login_error(error),
    }
}
fn login_error(error_value: crate::oauth::LoginError) -> Response {
    use crate::oauth::LoginError;
    let status = match error_value {
        LoginError::NotFound => StatusCode::NOT_FOUND,
        LoginError::CallbackPortBusy { .. }
        | LoginError::LinkedConflict
        | LoginError::IdentityConflict => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    error(status, &error_value.to_string())
}

async fn import_cpa(
    State(state): State<AccountAdmin>,
    request: Result<Json<CpaImportRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid import request"),
    };
    if request.source != "cpa" {
        return error(StatusCode::BAD_REQUEST, "Unsupported account source");
    }
    if request.mode != ImportMode::Linked {
        return error(
            StatusCode::BAD_REQUEST,
            "소유권 이전은 계정별 adopt 명령을 사용해 주세요.",
        );
    }
    let dir = match request.dir {
        Some(dir) => dir,
        None => match std::env::var_os("USERPROFILE") {
            Some(profile) => PathBuf::from(profile).join(".cli-proxy-api/auths"),
            None => return error(StatusCode::INTERNAL_SERVER_ERROR, "USERPROFILE is missing"),
        },
    };
    match tokio::task::spawn_blocking(move || {
        let store = state.store;
        let rows = import::run(&store, &dir, &request.activate, request.dry_run)?;
        if !request.dry_run
            && let Some(poller) = state.poller
        {
            for row in &rows {
                if matches!(
                    row.action,
                    import::ImportAction::Created | import::ImportAction::Updated
                ) && let Some(id) = &row.id
                    && let Ok(account) = store.load(id)
                {
                    poller.queue_profile(&account);
                }
            }
        }
        Ok::<_, StoreError>(rows)
    })
    .await
    {
        Ok(Ok(rows)) => Json(rows).into_response(),
        Ok(Err(StoreError::GenerationConflict { .. } | StoreError::IdentityCollision)) => {
            error(StatusCode::CONFLICT, "Account store conflict")
        }
        Ok(Err(_)) => error(StatusCode::BAD_REQUEST, "Account import failed"),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Account import task failed",
        ),
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"message": message}})),
    )
        .into_response()
}

#[derive(Clone)]
struct RuntimeAdmin {
    relay: Arc<crate::relay::RelayState>,
    data_port: u16,
    #[cfg(feature = "testkit")]
    adoption_probe: Option<fn() -> crate::accounts::adopt::ProbeState>,
}

pub fn runtime_routes(relay: Arc<crate::relay::RelayState>, data_port: u16) -> Router {
    runtime_router(RuntimeAdmin {
        relay,
        data_port,
        #[cfg(feature = "testkit")]
        adoption_probe: None,
    })
}

#[cfg(feature = "testkit")]
pub fn runtime_routes_with_adoption_probe(
    relay: Arc<crate::relay::RelayState>,
    data_port: u16,
    probe: fn() -> crate::accounts::adopt::ProbeState,
) -> Router {
    runtime_router(RuntimeAdmin {
        relay,
        data_port,
        adoption_probe: Some(probe),
    })
}

fn runtime_router(state: RuntimeAdmin) -> Router {
    Router::new()
        .route(
            "/admin/v1/launches",
            post(issue_launch)
                .layer(axum::extract::DefaultBodyLimit::max(8192))
                .layer(axum::middleware::from_fn(issue_operation_entry)),
        )
        .route(
            "/admin/v1/routing",
            axum::routing::get(routing_view).post(routing_update),
        )
        .route("/admin/v1/sessions", axum::routing::get(dashboard_sessions))
        .route(
            "/admin/v1/sessions/{id}",
            axum::routing::get(dashboard_detail),
        )
        .route("/admin/v1/activity", axum::routing::get(dashboard_activity))
        .route(
            "/admin/v1/usage-history",
            axum::routing::get(dashboard_usage),
        )
        .route("/admin/v1/state", axum::routing::get(runtime_state))
        .route("/admin/v1/events", axum::routing::get(runtime_events))
        .route("/admin/v1/accounts/takeover", post(takeover_account))
        .route("/admin/v1/accounts/{id}/{action}", post(account_action))
        .route(
            "/admin/v1/accounts/{id}",
            axum::routing::patch(account_patch),
        )
        .route("/admin/v1/lifecycle/drain", post(lifecycle_drain))
        .route("/admin/v1/lifecycle/shutdown", post(lifecycle_shutdown))
        .with_state(state)
}

// Candidate admission/wait budget, not a filesystem completion deadline.
const ISSUE_OPERATION_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

async fn issue_operation_entry(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let operation = crate::issue_operation::IssueOperation::new(
        tokio::time::Instant::now() + ISSUE_OPERATION_BUDGET,
    );
    let _caller = operation.caller_guard();
    request.extensions_mut().insert(operation.clone());
    let result = tokio::time::timeout_at(operation.deadline(), next.run(request)).await;
    match result {
        Ok(response) if operation.can_reply() => response,
        _ => {
            let error = crate::relay::LaunchError::Unavailable;
            (
                error.status(),
                Json(serde_json::json!({"error": {
                    "code": error.code(), "message": error.message()
                }})),
            )
                .into_response()
        }
    }
}

async fn issue_launch(
    State(state): State<RuntimeAdmin>,
    axum::Extension(operation): axum::Extension<crate::issue_operation::IssueOperation>,
    Json(request): Json<crate::relay::LaunchRequest>,
) -> Response {
    match state
        .relay
        .issue_launch_for_operation(request, state.data_port, operation)
        .await
    {
        Ok(reply) => {
            let mut response = Json(reply).into_response();
            response.headers_mut().insert(
                "cache-control",
                axum::http::HeaderValue::from_static("no-store"),
            );
            response
                .headers_mut()
                .insert("pragma", axum::http::HeaderValue::from_static("no-cache"));
            response
        }
        Err(error) => (
            error.status(),
            Json(serde_json::json!({"error":{
                "code":error.code(),"message":error.message(),
            }})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RoutingUpdate {
    Project {
        id: Option<String>,
        name: String,
        settings: crate::routing::Settings,
    },
    Session {
        id: String,
        settings: Option<crate::routing::Settings>,
        project: Option<String>,
        #[serde(default)]
        inherit: bool,
    },
}
async fn routing_view(State(state): State<RuntimeAdmin>) -> Response {
    let relay = state.relay.clone();
    match tokio::task::spawn_blocking(move || relay.routing_view()).await {
        Ok(value) => Json(value).into_response(),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "계정 전환 정책을 조회하지 못했습니다.",
        ),
    }
}
async fn routing_update(
    State(state): State<RuntimeAdmin>,
    request: Result<Json<RoutingUpdate>, JsonRejection>,
) -> Response {
    if state.relay.is_draining() {
        return error(
            StatusCode::CONFLICT,
            "코어 인계 중에는 정책을 변경할 수 없습니다.",
        );
    }
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST, "정책 요청이 올바르지 않습니다.");
    };
    let table = state.relay.bindings.clone();
    let result=tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        match request {
            RoutingUpdate::Project{id,name,settings}=>Ok(serde_json::json!({"id":table.policies.put_project(id.as_deref(),crate::routing::Project{name,settings})?})),
            RoutingUpdate::Session{id,settings,project,inherit}=>{table.set_routing_session(&id,settings,project.as_deref(),inherit)?;Ok(serde_json::json!({"updated":true}))},
        }
    }).await;
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        _ => error(
            StatusCode::BAD_REQUEST,
            "정책을 저장하지 못했습니다. 식별자·대기 시간·저장소를 확인해 주세요.",
        ),
    }
}

#[derive(Default, serde::Deserialize)]
struct DrainRequest {
    #[serde(default = "default_drain_timeout")]
    drain_timeout_secs: u64,
}

fn default_drain_timeout() -> u64 {
    30 * 60
}

async fn lifecycle_drain(
    State(state): State<RuntimeAdmin>,
    request: Result<Json<DrainRequest>, JsonRejection>,
) -> Response {
    let timeout = request.ok().map(|Json(value)| value.drain_timeout_secs);
    let timeout = timeout
        .unwrap_or_else(default_drain_timeout)
        .clamp(1, 30 * 60);
    state.relay.set_drain_timeout(timeout);
    match state.relay.begin_drain().await {
        Ok(bindings) => match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            state.relay.wait_for_lease_release(),
        )
        .await
        {
            Ok(()) => Json(serde_json::json!({
                "bindings": bindings.get("bindings").cloned().unwrap_or_else(|| serde_json::json!([])),
                "lease_released": true
            }))
            .into_response(),
            Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "Lease handover timed out"),
        },
        Err(()) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Drain is still in progress",
        ),
    }
}

async fn lifecycle_shutdown(
    State(state): State<RuntimeAdmin>,
    request: Result<Json<DrainRequest>, JsonRejection>,
) -> Response {
    lifecycle_drain(State(state), request).await
}

async fn runtime_state(State(state): State<RuntimeAdmin>) -> Response {
    state.relay.quota.mark_tray_open();
    match state
        .relay
        .accounts
        .views_with_bindings(&state.relay.bindings)
        .await
    {
        Ok(accounts) => {
            Json(serde_json::json!({
            "pools": crate::account_runtime::pool_counts(&accounts),
            "warnings": state.relay.recent_warnings(),
            "draining": state.relay.is_draining(),
            "capabilities": {"account_adopt":true,"manual_refresh":true,"account_takeover":true},
            "accounts": accounts,
            "inflight": {
                "requests": state.relay.inflight_requests.load(std::sync::atomic::Ordering::Relaxed),
                "bytes": state.relay.budget.used()
            },
            "memory": {"private_bytes": crate::guard::private_bytes()},
            "bindings": {"count": state.relay.bindings.count()},
            "data_port": state.data_port,
            "version": env!("CARGO_PKG_VERSION")
        })).into_response()
        }
        Err(()) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Account state unavailable",
        ),
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardQuery {
    range: Option<String>,
    since_seq: Option<u64>,
    limit: Option<usize>,
    days: Option<u8>,
    hours: Option<u8>,
}
type DashboardParameters =
    Result<axum::extract::Query<DashboardQuery>, axum::extract::rejection::QueryRejection>;
fn dashboard_invalid() -> Response {
    error(StatusCode::BAD_REQUEST, "Invalid dashboard query")
}
async fn dashboard_sessions(
    State(state): State<RuntimeAdmin>,
    query: DashboardParameters,
) -> Response {
    let Ok(axum::extract::Query(query)) = query else {
        return dashboard_invalid();
    };
    let Ok(range) = crate::dashboard::SessionRange::parse(query.range.as_deref()) else {
        return dashboard_invalid();
    };
    Json(state.relay.dashboard().sessions(range)).into_response()
}
async fn dashboard_detail(
    State(state): State<RuntimeAdmin>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let Ok(id) = crate::dashboard::SessionId::try_from(id) else {
        return dashboard_invalid();
    };
    match state.relay.dashboard().detail(&id) {
        Some(detail) => Json(detail).into_response(),
        None => error(StatusCode::NOT_FOUND, "Session expired"),
    }
}
async fn dashboard_activity(
    State(state): State<RuntimeAdmin>,
    query: DashboardParameters,
) -> Response {
    let Ok(axum::extract::Query(query)) = query else {
        return dashboard_invalid();
    };
    match state.relay.dashboard().activity_page(
        query.since_seq,
        query.limit.unwrap_or(200),
        query.days.unwrap_or(7),
    ) {
        Ok(page) => Json(page).into_response(),
        Err(_) => dashboard_invalid(),
    }
}
async fn dashboard_usage(
    State(state): State<RuntimeAdmin>,
    query: DashboardParameters,
) -> Response {
    let Ok(axum::extract::Query(query)) = query else {
        return dashboard_invalid();
    };
    match state
        .relay
        .dashboard()
        .usage_page(query.hours.unwrap_or(24))
    {
        Ok(page) => Json(page).into_response(),
        Err(_) => dashboard_invalid(),
    }
}

async fn runtime_events(State(state): State<RuntimeAdmin>) -> Response {
    state.relay.quota.mark_tray_open();
    use axum::response::sse::{KeepAlive, Sse};
    let guard = state.relay.quota.open_events();
    let stream = account_events(state.relay.accounts.subscribe());
    let stream = async_stream::stream! {
        let _guard = guard;
        futures_util::pin_mut!(stream);
        use futures_util::StreamExt;
        while let Some(event) = stream.next().await { yield event; }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn account_events(
    receiver: tokio::sync::broadcast::Receiver<serde_json::Value>,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    use axum::response::sse::Event;
    futures_util::stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(value) => {
                    let Some(
                        name @ ("account_changed" | "quota_updated" | "login_progress"
                        | "core_warning" | "sessions_changed" | "activity"),
                    ) = value["event"].as_str()
                    else {
                        continue;
                    };
                    let Ok(event) = Event::default().event(name).json_data(&value["data"]) else {
                        continue;
                    };
                    return Some((Ok::<_, std::convert::Infallible>(event), receiver));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let event = Event::default()
                        .event("sessions_changed")
                        .json_data(serde_json::json!({"changed":[],"removed":[],"resync":true}))
                        .expect("safe resync event");
                    return Some((Ok::<_, std::convert::Infallible>(event), receiver));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountPatch {
    label: Option<String>,
    drain: Option<bool>,
    allow_credits: Option<bool>,
}

async fn account_action(
    State(state): State<RuntimeAdmin>,
    axum::extract::Path((id, action)): axum::extract::Path<(String, String)>,
    request: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    if state.relay.is_draining() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Core is draining");
    }
    use crate::accounts::UserState;
    let user_state = match action.as_str() {
        "release-hold" => return release_hold(state, id, request).await,
        "refresh-usage" => return refresh_usage(state, id).await,
        "adopt" => return adopt_account(state, id, request).await,
        "refresh" => return refresh_account(state, id, request).await,
        "pause" => UserState::Paused,
        "resume" | "enable" => UserState::Active,
        "disable" => UserState::Disabled,
        _ => return error(StatusCode::NOT_FOUND, "Unknown account action"),
    };
    change_account(state, id, Some(user_state), AccountPatch::default()).await
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseHoldRequest {
    confirm: String,
}

async fn release_hold(
    state: RuntimeAdmin,
    id: String,
    request: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    let Ok(Json(value)) = request else {
        return error(StatusCode::BAD_REQUEST, "해제할 계정 ID를 확인해 주세요.");
    };
    let Ok(request) = serde_json::from_value::<ReleaseHoldRequest>(value) else {
        return error(StatusCode::BAD_REQUEST, "해제할 계정 ID를 확인해 주세요.");
    };
    if request.confirm != id {
        return error(StatusCode::BAD_REQUEST, "계정 ID 확인이 일치하지 않습니다.");
    }
    match state.relay.accounts.release_quota_hold(&id).await {
        Ok(account) => Json(serde_json::json!({
            "id":account.id,"released":true,"provider":account.provider,
            "label":account.label,"email":account.email,"plan":account.plan,
            "user_state":account.user_state,"health":account.health,"drain":account.drain,
            "allow_credits":account.allow_credits
        }))
        .into_response(),
        Err(()) => error(
            StatusCode::CONFLICT,
            "해제할 사용량 한도가 없거나 저장하지 못했습니다.",
        ),
    }
}

async fn takeover_account(
    State(state): State<RuntimeAdmin>,
    request: Result<Json<crate::accounts::takeover::TakeoverRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = request else {
        return error(
            StatusCode::BAD_REQUEST,
            "계정 가져오기 요청이 올바르지 않습니다.",
        );
    };
    match state.relay.takeover_account(request).await {
        Ok(value) => Json(value).into_response(),
        Err(problem) => {
            use crate::accounts::takeover::TakeoverError;
            let (status, code, message) = match problem {
                TakeoverError::Conflict => (
                    StatusCode::CONFLICT,
                    "takeover_conflict",
                    "원래 앱 실행 상태, 최근 파일 변경 또는 기존 계정 충돌을 확인하고 다시 검사하세요.",
                ),
                TakeoverError::ApiKey => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "api_key_auth",
                    "API 키 계정은 가져올 수 없습니다. 구독 계정의 OAuth 로그인이 필요합니다.",
                ),
                TakeoverError::Invalid => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_takeover",
                    "원본 계정 형식이나 사용자를 확인할 수 없습니다.",
                ),
                TakeoverError::Unavailable => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "takeover_unavailable",
                    "가져오기에 필요한 파일이나 저장소를 사용할 수 없습니다. 계정 상태를 확인하세요.",
                ),
                TakeoverError::Uncertain => (
                    StatusCode::CONFLICT,
                    "takeover_uncertain",
                    "앞선 토큰 갱신의 결과를 확인할 수 없어 다시 보내지 않았습니다. 계정에서 다시 로그인해 주세요.",
                ),
            };
            (
                status,
                Json(serde_json::json!({"error":{"code":code,"message":message}})),
            )
                .into_response()
        }
    }
}

async fn adopt_account(
    state: RuntimeAdmin,
    id: String,
    request: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    let Ok(Json(value)) = request else {
        return error(
            StatusCode::BAD_REQUEST,
            "소유권 이전 요청이 올바르지 않습니다.",
        );
    };
    let Ok(request) = serde_json::from_value::<crate::accounts::adopt::AdoptRequest>(value) else {
        return error(
            StatusCode::BAD_REQUEST,
            "소유권 이전 요청이 올바르지 않습니다.",
        );
    };
    let store = state.relay.accounts.store.clone();
    let poller = state.relay.quota.clone();
    #[cfg(feature = "testkit")]
    let probe = state.adoption_probe;
    let result = tokio::task::spawn_blocking(move || {
        #[cfg(feature = "testkit")]
        let result = match probe {
            Some(probe) => crate::accounts::adopt::run_with_probe(&store, &id, request, probe),
            None => crate::accounts::adopt::run(&store, &id, request),
        }?;
        #[cfg(not(feature = "testkit"))]
        let result = crate::accounts::adopt::run(&store, &id, request)?;
        if let Ok(account) = store.load(&result.id) {
            poller.queue_profile(&account);
        }
        Ok::<_, crate::accounts::adopt::AdoptError>(result)
    })
    .await;
    match result {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(error_value)) => {
            use crate::accounts::adopt::AdoptError;
            let (status, code, message) = match error_value {
                AdoptError::NotFound => (
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "계정을 찾을 수 없습니다.",
                ),
                AdoptError::NotLinked => (
                    StatusCode::CONFLICT,
                    "not_linked",
                    "CPA 연결 계정만 소유권을 이전할 수 있습니다.",
                ),
                AdoptError::Generation { .. } => (
                    StatusCode::CONFLICT,
                    "generation_conflict",
                    "계정이 변경됐습니다. 상태를 다시 읽고 시도해 주세요.",
                ),
                AdoptError::IdentityMismatch | AdoptError::AmbiguousIdentity => (
                    StatusCode::CONFLICT,
                    "identity_conflict",
                    "계정의 사용자를 확인할 수 없거나 서로 다른 사용자 파일이 겹칩니다.",
                ),
                AdoptError::CpaRunning => (
                    StatusCode::CONFLICT,
                    "cpa_running",
                    "CPA가 아직 이 계정을 갱신할 수 있습니다.",
                ),
                AdoptError::ProbeUnavailable => (
                    StatusCode::CONFLICT,
                    "cpa_unknown",
                    "CPA 실행 상태를 확인할 수 없습니다.",
                ),
                AdoptError::SourceIo | AdoptError::Store(_) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "store_unavailable",
                    "소유권 이전에 필요한 계정 자료를 읽거나 저장할 수 없습니다.",
                ),
            };
            (
                status,
                Json(serde_json::json!({"error":{"code":code,"message":message}})),
            )
                .into_response()
        }
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "소유권 이전 작업을 마치지 못했습니다.",
        ),
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRequest {
    #[serde(default)]
    force: bool,
}

async fn refresh_account(
    state: RuntimeAdmin,
    id: String,
    request: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    let Ok(Json(value)) = request else {
        return error(StatusCode::BAD_REQUEST, "갱신 요청이 올바르지 않습니다.");
    };
    let Ok(request) = serde_json::from_value::<RefreshRequest>(value) else {
        return error(StatusCode::BAD_REQUEST, "갱신 요청이 올바르지 않습니다.");
    };
    match state.relay.refresh_account(id, request.force).await {
        Ok(summary) => Json(summary).into_response(),
        Err(error_value) => {
            use crate::relay::ManualRefreshError;
            let status = match error_value {
                ManualRefreshError::NotFound => StatusCode::NOT_FOUND,
                ManualRefreshError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::CONFLICT,
            };
            let message = error_value.to_string();
            let mut value = serde_json::to_value(error_value).unwrap_or_default();
            value["message"] = serde_json::json!(message);
            (status, Json(serde_json::json!({"error":value}))).into_response()
        }
    }
}

async fn refresh_usage(state: RuntimeAdmin, id: String) -> Response {
    use crate::quota::PollError;
    match state.relay.quota.poll(&id).await {
        Ok(_) => match state
            .relay
            .accounts
            .views_with_bindings(&state.relay.bindings)
            .await
        {
            Ok(views) => match views.into_iter().find(|view| view["id"] == id) {
                Some(view) => Json(view).into_response(),
                None => error(StatusCode::NOT_FOUND, "Account not found"),
            },
            Err(()) => error(StatusCode::INTERNAL_SERVER_ERROR, "Account state unavailable"),
        },
        Err(PollError::TooSoon { retry_after }) => {
            let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(serde_json::json!({"error":{"code":"poll_too_soon","message":"Usage poll interval has not elapsed","retry_after":retry_after}}))).into_response();
            if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after.to_string()) { response.headers_mut().insert("retry-after", value); }
            response
        }
        Err(PollError::NotFound) => error(StatusCode::NOT_FOUND, "Account not found"),
        Err(PollError::Excluded) => (StatusCode::CONFLICT, Json(serde_json::json!({"error":{"code":"poll_excluded","message":"Account cannot be polled"}}))).into_response(),
        Err(PollError::Unavailable) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error":{"code":"poll_unavailable","message":"Usage poll unavailable"}}))).into_response(),
    }
}

async fn account_patch(
    State(state): State<RuntimeAdmin>,
    axum::extract::Path(id): axum::extract::Path<String>,
    request: Result<Json<AccountPatch>, JsonRejection>,
) -> Response {
    if state.relay.is_draining() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Core is draining");
    }
    match request {
        Ok(Json(patch)) => change_account(state, id, None, patch).await,
        Err(_) => error(StatusCode::BAD_REQUEST, "Invalid account settings"),
    }
}

async fn change_account(
    state: RuntimeAdmin,
    id: String,
    user_state: Option<crate::accounts::UserState>,
    patch: AccountPatch,
) -> Response {
    if patch
        .label
        .as_ref()
        .is_some_and(|label| label.len() > 256 || label.chars().any(char::is_control))
    {
        return error(StatusCode::BAD_REQUEST, "Invalid account label");
    }
    match state.relay.accounts.views().await {
        Ok(views) if !views.iter().any(|view| view["id"] == id) => {
            return error(StatusCode::NOT_FOUND, "Account not found");
        }
        Err(()) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Account state unavailable",
            );
        }
        _ => {}
    }
    match state
        .relay
        .accounts
        .change(
            &id,
            user_state,
            patch.label,
            patch.drain,
            patch.allow_credits,
        )
        .await
    {
        Ok(account) => {
            if patch.drain == Some(true) {
                let bindings = state.relay.bindings.clone();
                if !matches!(
                    tokio::task::spawn_blocking(move || bindings.flush()).await,
                    Ok(Ok(()))
                ) {
                    return error(StatusCode::INTERNAL_SERVER_ERROR, "Binding flush failed");
                }
            }
            Json(serde_json::json!({
            "id": account.id, "provider": account.provider, "label": account.label,
            "email": account.email, "plan": account.plan, "user_state": account.user_state,
            "health": account.health, "drain": account.drain, "allow_credits": account.allow_credits
        }))
        .into_response()
        }
        Err(()) => error(StatusCode::INTERNAL_SERVER_ERROR, "Account update failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn c11_events_skip_bounded_lag_and_stream_both_event_types() {
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        for _ in 0..300 {
            sender
                .send(serde_json::json!({"event":"unsupported", "data":"discarded"}))
                .unwrap();
        }
        sender.send(serde_json::json!({"event":"account_changed", "data":{"id":"cx-synthetic", "user_state":"paused"}})).unwrap();
        sender.send(serde_json::json!({"event":"quota_updated", "data":{"id":"cx-synthetic", "windows":[]}})).unwrap();
        drop(sender);
        let response = axum::response::sse::Sse::new(account_events(receiver)).into_response();
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            axum::body::to_bytes(response.into_body(), 4096),
        )
        .await
        .unwrap()
        .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("event: account_changed"));
        assert!(body.contains("event: quota_updated"));
        assert!(body.contains("\"user_state\":\"paused\""));
        assert!(!body.contains("discarded"));
    }

    #[tokio::test]
    async fn c11_events_ignore_unknown_event_names() {
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        sender
            .send(serde_json::json!({"event":"arbitrary\nevent", "data":"discarded"}))
            .unwrap();
        drop(sender);
        let response = axum::response::sse::Sse::new(account_events(receiver)).into_response();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(body.is_empty());
    }
}
