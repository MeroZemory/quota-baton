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
