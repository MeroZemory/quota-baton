use super::*;
use crate::reset_credits::Error;
use axum::extract::Path;
use qb_proto::reset_credit_action::ResetCreditCommitRequest;
use qb_proto::reset_credit_action::{
    ResetCreditErrorCode as Code, ResetCreditErrorDetail, ResetCreditErrorReply,
};

fn refused(value: Error) -> Response {
    let (status, code) = match value {
        Error::Stale => (StatusCode::CONFLICT, Code::Stale),
        Error::Unknown => (StatusCode::CONFLICT, Code::Unknown),
        Error::Busy => (StatusCode::CONFLICT, Code::Busy),
        Error::Unsupported => (StatusCode::CONFLICT, Code::Unsupported),
        Error::Ineligible => (StatusCode::CONFLICT, Code::Ineligible),
        Error::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, Code::Unavailable),
    };
    (
        status,
        Json(ResetCreditErrorReply {
            error: ResetCreditErrorDetail {
                code,
                message: "Reset-credit request refused".into(),
            },
        }),
    )
        .into_response()
}

pub(super) async fn cached(State(state): State<RuntimeAdmin>, Path(id): Path<String>) -> Response {
    let store = state.relay.quota.clone();
    let Some(account) = state
        .relay
        .accounts
        .typed_views_with_bindings(&state.relay.bindings)
        .await
        .ok()
        .and_then(|accounts| accounts.into_iter().find(|account| account.id == id))
    else {
        return error(StatusCode::NOT_FOUND, "Account not found");
    };
    let provider = match account.provider {
        qb_proto::admin_account::AdminProvider::Claude => crate::accounts::Provider::Claude,
        qb_proto::admin_account::AdminProvider::Codex => crate::accounts::Provider::Codex,
    };
    Json(store.reset_credit_cached(&id, provider)).into_response()
}

pub(super) async fn refresh(State(state): State<RuntimeAdmin>, Path(id): Path<String>) -> Response {
    if state.relay.is_draining() {
        return refused(Error::Unavailable);
    }
    match state.relay.quota.refresh_reset_credits(&id).await {
        Ok(value) => Json(value).into_response(),
        Err(value) => refused(value),
    }
}

pub(super) async fn preview(State(state): State<RuntimeAdmin>, Path(id): Path<String>) -> Response {
    if state.relay.is_draining() {
        return refused(Error::Unavailable);
    }
    match state.relay.quota.preview_reset_credit(&id).await {
        Ok(value) => Json(value).into_response(),
        Err(value) => refused(value),
    }
}

pub(super) async fn commit(
    State(state): State<RuntimeAdmin>,
    Path(id): Path<String>,
    request: Result<Json<ResetCreditCommitRequest>, JsonRejection>,
) -> Response {
    if state.relay.is_draining() {
        return refused(Error::Unavailable);
    }
    let Ok(Json(request)) = request else {
        return refused(Error::Stale);
    };
    if request.preview_token.len() != 32 {
        return refused(Error::Stale);
    }
    match state.relay.quota.commit_reset_credit(id, request).await {
        Ok(value) => Json(value).into_response(),
        Err(value) => refused(value),
    }
}
