use super::*;
#[cfg(feature = "testkit")]
pub(super) use crate::accounts::adopt::ProbeState;
use crate::accounts::{StoreError, import};
use qb_proto::{CpaImportRequest, ImportMode};
use std::path::PathBuf;

pub(super) fn runtime_routes(router: Router<RuntimeAdmin>) -> Router<RuntimeAdmin> {
    router.route("/admin/v1/accounts/takeover", post(takeover_account))
}
pub(super) fn capabilities() -> qb_proto::StateCapabilities {
    qb_proto::StateCapabilities {
        account_adopt: true,
        manual_refresh: true,
        account_takeover: true,
    }
}
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
    let phase = request.phase;
    match state.relay.takeover_account(request).await {
        Ok(value) => match phase {
            crate::accounts::takeover::Phase::Inspect => {
                match serde_json::from_value::<qb_proto::admin_takeover::TakeoverInspect>(value) {
                    Ok(reply) => Json(reply).into_response(),
                    Err(_) => error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Account response unavailable",
                    ),
                }
            }
            crate::accounts::takeover::Phase::Commit => {
                match serde_json::from_value::<qb_proto::admin_takeover::TakeoverCommit>(value) {
                    Ok(reply) => Json(reply).into_response(),
                    Err(_) => error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Account response unavailable",
                    ),
                }
            }
        },
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
                Json(qb_proto::admin_account_error::AccountErrorReply {
                    error: qb_proto::admin_account_error::AccountErrorDetail::coded(code, message),
                }),
            )
                .into_response()
        }
    }
}

pub(super) async fn adopt_account(
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
                Json(qb_proto::admin_account_error::AccountErrorReply {
                    error: qb_proto::admin_account_error::AccountErrorDetail::coded(code, message),
                }),
            )
                .into_response()
        }
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "소유권 이전 작업을 마치지 못했습니다.",
        ),
    }
}
