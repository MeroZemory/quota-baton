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

pub(super) async fn preview(
    State(state): State<RuntimeAdmin>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    if state.relay.is_draining() {
        return refused(Error::Unavailable);
    }
    if body.len() > 1024 {
        return refused(Error::Stale);
    }
    let request = if body.is_empty() {
        qb_proto::reset_credit_action::ResetCreditPreviewRequest::default()
    } else {
        let Ok(request) = serde_json::from_slice::<
            qb_proto::reset_credit_action::ResetCreditPreviewRequest,
        >(&body) else {
            return refused(Error::Stale);
        };
        request
    };
    if request
        .grant_id
        .as_ref()
        .is_some_and(|id| id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control))
    {
        return refused(Error::Stale);
    }
    let result = match request.grant_id {
        Some(grant) => {
            state
                .relay
                .quota
                .preview_reset_credit_grant(&id, Some(grant))
                .await
        }
        None => state.relay.quota.preview_reset_credit(&id).await,
    };
    match result {
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
