use std::sync::Arc;

use crate::accounts::AccountStore;
use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use qb_proto::admin_action::{
    AccountActionReply, ClearedReply, ProjectReply, ReleaseHoldReply, RoutingMutationReply,
    UpdatedReply,
};

pub use source_features::{account_routes, account_routes_with_poller};

#[derive(serde::Deserialize)]
pub struct LoginRequest {
    pub provider: crate::accounts::Provider,
    #[serde(default)]
    pub replace_linked: bool,
    #[serde(default)]
    pub no_browser: bool,
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
        .route(
            "/admin/v1/accounts/login/{login_id}/submit",
            post(submit_login).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
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
    let started = if request.no_browser {
        service
            .start_manual(request.provider, request.replace_linked)
            .await
    } else {
        service
            .start(request.provider, request.replace_linked)
            .await
    };
    match started {
        Ok(start) => Json(start).into_response(),
        Err(error) => login_error(error),
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginSubmission {
    input: String,
}
impl Drop for LoginSubmission {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.input);
    }
}

async fn submit_login(
    State(service): State<crate::oauth::LoginService>,
    axum::extract::Path(id): axum::extract::Path<String>,
    request: Result<Json<LoginSubmission>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST, "로그인 입력이 올바르지 않습니다.");
    };
    match service.submit_manual(&id, &request.input).await {
        Ok(()) => match service.status(&id).await {
            Ok(status) => Json(status).into_response(),
            Err(error) => login_error(error),
        },
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

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(qb_proto::admin_error::AdminErrorReply {
            error: qb_proto::admin_error::AdminErrorDetail {
                message: message.to_owned(),
            },
        }),
    )
        .into_response()
}

#[derive(Clone)]
struct RuntimeAdmin {
    relay: Arc<crate::relay::RelayState>,
    data_port: u16,
    #[cfg(feature = "testkit")]
    adoption_probe: Option<fn() -> source_features::ProbeState>,
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
    probe: fn() -> source_features::ProbeState,
) -> Router {
    runtime_router(RuntimeAdmin {
        relay,
        data_port,
        adoption_probe: Some(probe),
    })
}

fn runtime_router(state: RuntimeAdmin) -> Router {
    source_features::runtime_routes(
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
            .route("/admin/v1/routing/move-guard/clear", post(clear_move_guard))
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
            .route("/admin/v1/accounts/{id}/{action}", post(account_action))
            .route(
                "/admin/v1/accounts/{id}",
                axum::routing::patch(account_patch),
            )
            .route("/admin/v1/lifecycle/drain", post(lifecycle_drain))
            .route("/admin/v1/lifecycle/shutdown", post(lifecycle_shutdown)),
    )
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
    ClaudeUrgentLaunch {
        session_id: String,
    },
    Urgent {
        id: String,
        on_5h_limit: crate::routing::On5hLimit,
    },
    Project {
        id: Option<String>,
        name: String,
        settings: crate::routing::SettingsEdit,
    },
    Session {
        id: String,
        settings: Option<crate::routing::SettingsEdit>,
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
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<RoutingMutationReply> {
        match request {
            RoutingUpdate::ClaudeUrgentLaunch { session_id } => {
                table.register_claude_urgent(&session_id)?;
                Ok(RoutingMutationReply::Updated(UpdatedReply {
                    updated: true,
                }))
            }
            RoutingUpdate::Urgent { id, on_5h_limit } => {
                table.set_routing_session_urgent(&id, on_5h_limit)?;
                Ok(RoutingMutationReply::Updated(UpdatedReply {
                    updated: true,
                }))
            }
            RoutingUpdate::Project { id, name, settings } => {
                Ok(RoutingMutationReply::Project(ProjectReply {
                    id: table
                        .policies
                        .put_project_patch(id.as_deref(), name, settings)?,
                }))
            }
            RoutingUpdate::Session {
                id,
                settings,
                project,
                inherit,
            } => {
                table.set_routing_session_patch(&id, settings, project.as_deref(), inherit)?;
                Ok(RoutingMutationReply::Updated(UpdatedReply {
                    updated: true,
                }))
            }
        }
    })
    .await;
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
            Ok(()) => Json(qb_proto::admin_control::LifecycleReply {
                bindings: bindings
                    .get("bindings")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
                lease_released: true,
            })
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
        .typed_views_with_bindings(&state.relay.bindings)
        .await
    {
        Ok(accounts) => {
            let pools = crate::account_runtime::account_pools(&accounts);
            Json(qb_proto::AdminState {
                pools,
                warnings: state.relay.recent_warnings(),
                draining: Some(state.relay.is_draining()),
                capabilities: Some(source_features::capabilities()),
                accounts,
                inflight: qb_proto::StateInflight {
                    requests: state
                        .relay
                        .inflight_requests
                        .load(std::sync::atomic::Ordering::Relaxed),
                    bytes: state.relay.budget.used() as u64,
                },
                memory: qb_proto::StateMemory {
                    private_bytes: crate::guard::private_bytes(),
                },
                bindings: qb_proto::StateBindings {
                    count: state.relay.bindings.count() as u64,
                },
                data_port: state.data_port,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            })
            .into_response()
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
async fn clear_move_guard(State(state): State<RuntimeAdmin>) -> Response {
    let bindings = state.relay.bindings.clone();
    match tokio::task::spawn_blocking(move || bindings.clear_move_guard()).await {
        Ok(()) => Json(ClearedReply { cleared: true }).into_response(),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "이동 안전장치를 해제하지 못했습니다.",
        ),
    }
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
    let mut view = state.relay.dashboard().sessions(range);
    view.move_cost = Some(state.relay.move_cost_snapshot());
    Json(view).into_response()
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
        Ok(page) => Json(crate::dashboard::history::response::ActivityPage::from(
            page,
        ))
        .into_response(),
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
        "adopt" => return source_features::adopt_account(state, id, request).await,
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
        Ok(account) => Json(ReleaseHoldReply {
            id: account.id,
            released: true,
            provider: account.provider,
            label: account.label,
            email: account.email,
            plan: account.plan,
            user_state: account.user_state,
            health: account.health,
            drain: account.drain,
            allow_credits: account.allow_credits,
        })
        .into_response(),
        Err(()) => error(
            StatusCode::CONFLICT,
            "해제할 사용량 한도가 없거나 저장하지 못했습니다.",
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
    match state.relay.refresh_account_typed(id, request.force).await {
        Ok(summary) => Json(summary).into_response(),
        Err(error_value) => {
            use crate::relay::ManualRefreshError;
            let status = match error_value {
                ManualRefreshError::NotFound => StatusCode::NOT_FOUND,
                ManualRefreshError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::CONFLICT,
            };
            (status, Json(account_refresh_error(error_value))).into_response()
        }
    }
}

fn account_refresh_error(
    error_value: crate::relay::ManualRefreshError,
) -> qb_proto::admin_account_error::AccountErrorReply {
    use crate::relay::ManualRefreshError;
    let message = error_value.to_string();
    use qb_proto::admin_account_error::{AccountErrorDetail, AccountErrorReply, RefreshTerminal};
    let (code, outcome) = match error_value {
        ManualRefreshError::Linked => ("linked", None),
        ManualRefreshError::Drain => ("drain", None),
        ManualRefreshError::NeedsRelogin => ("needs_relogin", None),
        ManualRefreshError::Uncertain => ("uncertain", None),
        ManualRefreshError::NotFound => ("not_found", None),
        ManualRefreshError::Unavailable => ("unavailable", None),
        ManualRefreshError::Terminal { outcome } => {
            use crate::relay::ManualRefreshTerminal as T;
            (
                "terminal",
                Some(match outcome {
                    T::RefreshTokenInvalidated => RefreshTerminal::RefreshTokenInvalidated,
                    T::RefreshTokenReused => RefreshTerminal::RefreshTokenReused,
                    T::RefreshTokenExpired => RefreshTerminal::RefreshTokenExpired,
                    T::RefreshTokenRevoked => RefreshTerminal::RefreshTokenRevoked,
                    T::InvalidGrant => RefreshTerminal::InvalidGrant,
                    T::TokenEndpoint401 => RefreshTerminal::TokenEndpoint401,
                }),
            )
        }
    };
    AccountErrorReply {
        error: AccountErrorDetail {
            code: Some(code.to_owned()),
            outcome,
            message,
            retry_after: None,
        },
    }
}

async fn refresh_usage(state: RuntimeAdmin, id: String) -> Response {
    use crate::quota::PollError;
    match state.relay.quota.poll(&id).await {
        Ok(_) => match state
            .relay
            .accounts
            .typed_views_with_bindings(&state.relay.bindings)
            .await
        {
            Ok(views) => match views.into_iter().find(|view| view.id == id) {
                Some(view) => Json(view).into_response(),
                None => error(StatusCode::NOT_FOUND, "Account not found"),
            },
            Err(()) => error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Account state unavailable",
            ),
        },
        Err(PollError::TooSoon { retry_after }) => {
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(qb_proto::admin_account_error::AccountErrorReply {
                    error: qb_proto::admin_account_error::AccountErrorDetail {
                        retry_after: Some(retry_after),
                        ..qb_proto::admin_account_error::AccountErrorDetail::coded(
                            "poll_too_soon",
                            "Usage poll interval has not elapsed",
                        )
                    },
                }),
            )
                .into_response();
            if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after.to_string()) {
                response.headers_mut().insert("retry-after", value);
            }
            response
        }
        Err(PollError::NotFound) => error(StatusCode::NOT_FOUND, "Account not found"),
        Err(PollError::Excluded) => (
            StatusCode::CONFLICT,
            Json(qb_proto::admin_account_error::AccountErrorReply {
                error: qb_proto::admin_account_error::AccountErrorDetail::coded(
                    "poll_excluded",
                    "Account cannot be polled",
                ),
            }),
        )
            .into_response(),
        Err(PollError::Unavailable) => (
            StatusCode::BAD_GATEWAY,
            Json(qb_proto::admin_account_error::AccountErrorReply {
                error: qb_proto::admin_account_error::AccountErrorDetail::coded(
                    "poll_unavailable",
                    "Usage poll unavailable",
                ),
            }),
        )
            .into_response(),
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
            Json(AccountActionReply {
                id: account.id,
                provider: account.provider,
                label: account.label,
                email: account.email,
                plan: account.plan,
                user_state: account.user_state,
                health: account.health,
                drain: account.drain,
                allow_credits: account.allow_credits,
            })
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

#[cfg(test)]
mod n5c_routing_dto_tests {
    use super::RoutingUpdate;

    #[test]
    fn n5c_claude_launch_intent_accepts_only_exact_registration_shape() {
        let request = serde_json::json!({"kind":"claude_urgent_launch",
            "session_id":"12345678-1234-4234-8234-123456789abc"});
        assert!(matches!(
            serde_json::from_value::<RoutingUpdate>(request.clone()).unwrap(),
            RoutingUpdate::ClaudeUrgentLaunch { .. }
        ));
        let mut extra = request;
        extra["settings"] = serde_json::json!({"mode":"rebalance"});
        assert!(serde_json::from_value::<RoutingUpdate>(extra).is_err());
    }

    #[test]
    fn n5c_legacy_admin_edits_preserve_omitted_urgency() {
        let project = serde_json::json!({"kind":"project","name":"synthetic",
            "settings":{"mode":"strict","reset_wait_secs":17}});
        let session = serde_json::json!({"kind":"session","id":"synthetic",
            "settings":{"mode":"strict","reset_wait_secs":17}});
        for value in [project, session] {
            match serde_json::from_value::<RoutingUpdate>(value.clone()).unwrap() {
                RoutingUpdate::Project { settings, .. }
                | RoutingUpdate::Session {
                    settings: Some(settings),
                    ..
                } => {
                    assert!(settings.on_5h_limit.is_none());
                    assert_eq!(settings.mode, crate::routing::Mode::Strict);
                    assert_eq!(settings.reset_wait_secs, 17);
                }
                _ => panic!("expected settings edit"),
            }
            let mut invalid = value;
            invalid["settings"]["on_5h_limit"] = serde_json::Value::Null;
            assert!(serde_json::from_value::<RoutingUpdate>(invalid).is_err());
        }
    }

    #[test]
    fn n5c_admin_urgency_only_request_cannot_smuggle_complete_policy() {
        for (wire, expected) in [
            ("wait", crate::routing::On5hLimit::Wait),
            ("move", crate::routing::On5hLimit::Move),
        ] {
            let value = serde_json::json!({"kind":"urgent","id":"synthetic","on_5h_limit":wire});
            assert!(
                matches!(serde_json::from_value::<RoutingUpdate>(value.clone()).unwrap(),
                RoutingUpdate::Urgent { on_5h_limit, .. } if on_5h_limit == expected)
            );
            let mut invalid = value;
            invalid["settings"] = serde_json::json!({"mode":"rebalance","reset_wait_secs":60});
            assert!(serde_json::from_value::<RoutingUpdate>(invalid).is_err());
        }
        for value in [
            serde_json::Value::Null,
            serde_json::json!("on"),
            serde_json::json!(true),
        ] {
            assert!(
                serde_json::from_value::<RoutingUpdate>(serde_json::json!({
                    "kind":"urgent","id":"synthetic","on_5h_limit":value
                }))
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod r15_sse_boundary_tests {
    use super::*;
    use futures_util::StreamExt;

    async fn body(receiver: tokio::sync::broadcast::Receiver<serde_json::Value>) -> String {
        let response = axum::response::sse::Sse::new(account_events(receiver)).into_response();
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            axum::body::to_bytes(response.into_body(), 8192),
        )
        .await
        .expect("closed adapter must terminate")
        .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn r15_sse_six_names_missing_data_and_closed_channel() {
        let names = [
            "account_changed",
            "quota_updated",
            "login_progress",
            "core_warning",
            "sessions_changed",
            "activity",
        ];
        let (sender, receiver) = tokio::sync::broadcast::channel(32);
        for event in [
            serde_json::json!({"event":"unknown","data":"discarded"}),
            serde_json::json!({"event":"activity\ninjected","data":1}),
            serde_json::json!({"event":null,"data":1}),
        ] {
            sender.send(event).unwrap();
        }
        let mut expected = String::new();
        for name in names {
            sender
                .send(serde_json::json!({"event":name,"data":{"receipt":name}}))
                .unwrap();
            expected.push_str(&format!(
                "event: {name}\ndata: {{\"receipt\":\"{name}\"}}\n\n"
            ));
        }
        sender
            .send(serde_json::json!({"event":"core_warning"}))
            .unwrap();
        expected.push_str("event: core_warning\ndata: null\n\n");
        drop(sender);
        assert_eq!(body(receiver).await, expected);
        let (sender, receiver) = tokio::sync::broadcast::channel(1);
        drop(sender);
        assert_eq!(body(receiver).await, "");
    }

    #[tokio::test]
    async fn r15_sse_lag_exact_resync_then_retained_frame_then_closure() {
        let (sender, receiver) = tokio::sync::broadcast::channel(1);
        sender
            .send(serde_json::json!({"event":"activity","data":"lost"}))
            .unwrap();
        sender
            .send(serde_json::json!({"event":"quota_updated","data":{"id":"retained"}}))
            .unwrap();
        drop(sender);
        assert_eq!(
            body(receiver).await,
            concat!(
                "event: sessions_changed\ndata: {\"changed\":[],\"removed\":[],\"resync\":true}\n\n",
                "event: quota_updated\ndata: {\"id\":\"retained\"}\n\n"
            )
        );
    }

    #[test]
    fn r15_sse_actual_observability_burst_body_shared_roundtrip() {
        // Drop the actual writer outside Tokio and before removing its home,
        // including when the test body panics. No extra public producer seam.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r15-sse-tests");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let obs = crate::observability::Observability::new(home.path(), None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let (sender, receiver) = tokio::sync::broadcast::channel(32);
                obs.set_events(sender);
                let stream = account_events(receiver);
                futures_util::pin_mut!(stream);
                // Also covers zero without promoting it to an actionable UI count.
                for count in [0, 7, u32::MAX] {
                    obs.burst_warning(count);
                    let event =
                        tokio::time::timeout(std::time::Duration::from_secs(2), async {
                            loop {
                                let event =
                                    stream.next().await.expect("producer channel open").unwrap();
                                let response =
                                    axum::response::sse::Sse::new(futures_util::stream::iter([
                                        Ok::<_, std::convert::Infallible>(event),
                                    ]))
                                    .into_response();
                                let bytes = axum::body::to_bytes(response.into_body(), 8192)
                                    .await
                                    .unwrap();
                                let frame = String::from_utf8(bytes.to_vec()).unwrap();
                                if frame.starts_with("event: core_warning\n") {
                                    break frame;
                                }
                                // burst_warning also emits a separate history activity signal.
                                assert!(frame.starts_with("event: activity\n"));
                            }
                        })
                        .await
                        .expect("actual burst producer must reach SSE body");
                    let raw = format!(r#"{{"kind":"burst","burst_new_sessions":{count}}}"#);
                    assert_eq!(event, format!("event: core_warning\ndata: {raw}\n\n"));
                    let data = event
                        .strip_prefix("event: core_warning\ndata: ")
                        .unwrap()
                        .strip_suffix("\n\n")
                        .unwrap();
                    let decoded: qb_proto::admin_sse::BurstWarning =
                        serde_json::from_str(data).unwrap();
                    assert_eq!(decoded.burst_new_sessions, count);
                    assert_eq!(serde_json::to_string(&decoded).unwrap(), data);
                }
            })
        }));
        drop(runtime);
        drop(obs); // synchronous writer stop + join on this ordinary test thread
        home.close().unwrap();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

#[cfg(test)]
mod r15_protected_sse_tests {
    use super::*;
    use futures_util::{FutureExt, StreamExt};

    #[test]
    fn r15_sse_protected_actual_burst_complete_frame_roundtrip() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r15-sse-tests");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let obs = Arc::new(crate::observability::Observability::new(home.path(), None).unwrap());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut relay = None;
        let mut servers: Option<crate::http::RunningServers> = None;
        let (result, joined) = runtime.block_on(async {
            let result = std::panic::AssertUnwindSafe(async {
                let mut config = crate::config::Config::default();
                config.server.data_port = 0;
                config.server.admin_port = 0;
                // No schedulers/accounts/upstream requests: only the protected events route.
                relay = Some(Arc::new(
                    crate::relay::RelayState::new(
                        store.clone(),
                        &config,
                        obs.clone(),
                        Arc::new(crate::debug_pin::DebugPins::default()),
                    )
                    .unwrap(),
                ));
                servers = Some(
                    crate::http::spawn_with_routes(
                        &config,
                        crate::local_secrets::LocalSecrets {
                            client_key: "synthetic-sse-client".into(),
                            admin_token: "synthetic-sse-admin".into(),
                        },
                        Router::new(),
                        runtime_routes(relay.as_ref().unwrap().clone(), 0),
                    )
                    .await
                    .unwrap(),
                );
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(std::time::Duration::from_secs(3))
                    .build()
                    .unwrap();
                let url = format!(
                    "http://{}/admin/v1/events",
                    servers.as_ref().unwrap().admin_addr
                );
                assert_eq!(
                    client.get(&url).send().await.unwrap().status(),
                    StatusCode::UNAUTHORIZED
                );
                let response = client
                    .get(&url)
                    .bearer_auth("synthetic-sse-admin")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert!(
                    response.headers()["content-type"]
                        .to_str()
                        .unwrap()
                        .starts_with("text/event-stream")
                );
                let mut stream = response.bytes_stream();
                obs.burst_warning(23);
                let raw = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    let mut buffer = Vec::new();
                    loop {
                        let chunk = stream
                            .next()
                            .await
                            .expect("SSE closed before burst")
                            .unwrap();
                        buffer.extend_from_slice(&chunk);
                        assert!(buffer.len() <= 8192, "SSE fixture body bound");
                        while let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
                            let frame: Vec<_> = buffer.drain(..end + 2).collect();
                            if frame.starts_with(b"event: core_warning\n") {
                                return String::from_utf8(frame).unwrap();
                            }
                            assert!(
                                frame.starts_with(b"event: activity\n") || frame.starts_with(b":")
                            );
                        }
                    }
                })
                .await
                .expect("complete protected burst frame required");
                assert_eq!(
                    raw,
                    "event: core_warning\ndata: {\"kind\":\"burst\",\"burst_new_sessions\":23}\n\n"
                );
                let data = raw
                    .strip_prefix("event: core_warning\ndata: ")
                    .unwrap()
                    .strip_suffix("\n\n")
                    .unwrap();
                let shared: qb_proto::admin_sse::BurstWarning = serde_json::from_str(data).unwrap();
                assert_eq!(shared.burst_new_sessions, 23);
                assert_eq!(serde_json::to_string(&shared).unwrap(), data);
                drop(stream);
            })
            .catch_unwind()
            .await;
            // The body future owns the HTTP stream, so success or unwind drops it
            // before shutdown. Join both server tasks before dropping their owners.
            let mut joined = true;
            if let Some(servers) = servers.as_mut() {
                servers.shutdown();
                for task in [&mut servers.data_task, &mut servers.admin_task] {
                    match tokio::time::timeout(std::time::Duration::from_secs(3), &mut *task).await
                    {
                        Ok(Ok(Ok(()))) => {}
                        Ok(_) => joined = false,
                        Err(_) => {
                            joined = false;
                            task.abort();
                            let _ = task.await;
                        }
                    }
                }
            }
            (result, joined)
        });
        let root = home.keep(); // retain on teardown failure; no racing TempDir drop
        drop(servers);
        drop(runtime);
        drop(relay);
        let store_exclusive = Arc::strong_count(&store) == 1;
        // AccountRuntime installs an observer owned by the store. Release that
        // observer before checking the writer's remaining owners.
        drop(store);
        let exclusive = store_exclusive && Arc::strong_count(&obs) == 1;
        let flushed = obs.flush().is_ok();
        drop(obs); // ordinary thread joins writer before owned home removal
        if joined && exclusive && flushed {
            std::fs::remove_dir_all(&root).expect("owned SSE home removal failed");
            assert!(!root.exists());
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        assert!(
            exclusive && flushed,
            "SSE fixture owners or writer remain; home retained"
        );
        assert!(
            joined,
            "SSE servers must stop and join within fixture deadline"
        );
    }
}

#[cfg(test)]
mod r15_account_error_producer_tests {
    use super::*;
    #[test]
    fn account_errors_manual_actual_adapter_preserves_original_serde_bytes() {
        use crate::relay::{ManualRefreshError as E, ManualRefreshTerminal as T};
        let mut errors = vec![
            E::Linked,
            E::Drain,
            E::NeedsRelogin,
            E::Uncertain,
            E::NotFound,
            E::Unavailable,
        ];
        for outcome in [
            T::RefreshTokenInvalidated,
            T::RefreshTokenReused,
            T::RefreshTokenExpired,
            T::RefreshTokenRevoked,
            T::InvalidGrant,
            T::TokenEndpoint401,
        ] {
            errors.push(E::Terminal { outcome });
        }
        for error in errors {
            let mut old = serde_json::to_value(&error).unwrap();
            old["message"] = serde_json::json!(error.to_string());
            let before = serde_json::to_vec(&serde_json::json!({"error":old})).unwrap();
            let current = account_refresh_error(error);
            assert_eq!(serde_json::to_vec(&current).unwrap(), before);
            let decoded: qb_proto::admin_account_error::AccountErrorReply =
                serde_json::from_slice(&before).unwrap();
            assert_eq!(serde_json::to_vec(&decoded).unwrap(), before);
        }
    }
}

// Whole account-source features are selected only at this module boundary.
pub(crate) mod boxes;
#[cfg(windows)]
#[path = "admin/source_features_windows.rs"]
mod source_features;
#[cfg(unix)]
#[path = "admin/source_features_unix.rs"]
mod source_features;
