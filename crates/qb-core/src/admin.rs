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

pub fn account_routes(store: Arc<AccountStore>) -> Router {
    Router::new()
        .route("/admin/v1/accounts/import", post(import_cpa))
        .with_state(store)
}

async fn import_cpa(
    State(store): State<Arc<AccountStore>>,
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
        return error(StatusCode::BAD_REQUEST, "adopt is a cutover step (M8)");
    }
    let dir = match request.dir {
        Some(dir) => dir,
        None => match std::env::var_os("USERPROFILE") {
            Some(profile) => PathBuf::from(profile).join(".cli-proxy-api/auths"),
            None => return error(StatusCode::INTERNAL_SERVER_ERROR, "USERPROFILE is missing"),
        },
    };
    match tokio::task::spawn_blocking(move || {
        import::run(&store, &dir, &request.activate, request.dry_run)
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
}

pub fn runtime_routes(relay: Arc<crate::relay::RelayState>, data_port: u16) -> Router {
    Router::new()
        .route("/admin/v1/state", axum::routing::get(runtime_state))
        .route("/admin/v1/events", axum::routing::get(runtime_events))
        .route("/admin/v1/accounts/{id}/{action}", post(account_action))
        .route(
            "/admin/v1/accounts/{id}",
            axum::routing::patch(account_patch),
        )
        .with_state(RuntimeAdmin { relay, data_port })
}

async fn runtime_state(State(state): State<RuntimeAdmin>) -> Response {
    match state.relay.accounts.views().await {
        Ok(mut accounts) => {
            for account in &mut accounts {
                if let Some(id) = account["id"].as_str().map(str::to_owned) {
                    let (count, last_used) = state.relay.bindings.account_stats(&id);
                    account["bound_sessions"] = serde_json::json!(count);
                    account["last_used_at"] = serde_json::json!(last_used);
                }
            }
            Json(serde_json::json!({
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

async fn runtime_events(State(state): State<RuntimeAdmin>) -> Response {
    use axum::response::sse::{KeepAlive, Sse};
    Sse::new(account_events(state.relay.accounts.subscribe()))
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
                    let Some(name @ ("account_changed" | "quota_updated")) =
                        value["event"].as_str()
                    else {
                        continue;
                    };
                    let Ok(event) = Event::default().event(name).json_data(&value["data"]) else {
                        continue;
                    };
                    return Some((Ok::<_, std::convert::Infallible>(event), receiver));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
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
) -> Response {
    use crate::accounts::UserState;
    let user_state = match action.as_str() {
        "pause" => UserState::Paused,
        "resume" | "enable" => UserState::Active,
        "disable" => UserState::Disabled,
        _ => return error(StatusCode::NOT_FOUND, "Unknown account action"),
    };
    change_account(state, id, Some(user_state), AccountPatch::default()).await
}

async fn account_patch(
    State(state): State<RuntimeAdmin>,
    axum::extract::Path(id): axum::extract::Path<String>,
    request: Result<Json<AccountPatch>, JsonRejection>,
) -> Response {
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
