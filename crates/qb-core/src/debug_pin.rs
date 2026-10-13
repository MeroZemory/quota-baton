use crate::accounts::AccountStore;
use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

#[derive(Default)]
pub struct DebugPins {
    pins: RwLock<HashMap<Option<String>, String>>,
}

impl DebugPins {
    pub fn get(&self, session: Option<&str>) -> Option<String> {
        let pins = self.pins.read().ok()?;
        session
            .and_then(|session| pins.get(&Some(session.to_owned())))
            .or_else(|| pins.get(&None))
            .cloned()
    }
    pub fn set(&self, session: Option<String>, account_id: String) {
        if let Ok(mut pins) = self.pins.write() {
            pins.insert(session, account_id);
        }
    }
    pub fn clear(&self) {
        if let Ok(mut pins) = self.pins.write() {
            pins.clear();
        }
    }
}

#[derive(Clone)]
struct PinState {
    store: Arc<AccountStore>,
    pins: Arc<DebugPins>,
}

pub fn routes(store: Arc<AccountStore>, pins: Arc<DebugPins>) -> Router {
    Router::new()
        .route("/admin/v1/debug/pin", post(set_pin).delete(clear_pin))
        .with_state(PinState { store, pins })
}

async fn set_pin(
    State(state): State<PinState>,
    request: Result<Json<qb_proto::DebugPinRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":{"message":"잘못된 계정 고정 요청입니다."}})),
            )
                .into_response();
        }
    };
    let store = state.store.clone();
    let id = request.account_id.clone();
    if !matches!(
        tokio::task::spawn_blocking(move || store.load(&id)).await,
        Ok(Ok(_))
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":{"message":"계정을 찾을 수 없습니다."}})),
        )
            .into_response();
    }
    // 세션 식별자는 로그에 남기지 않는다.
    tracing::warn!(account_id=%request.account_id, "Phase 0 account pin enabled");
    state.pins.set(request.session_key, request.account_id);
    Json(serde_json::json!({"ok":true})).into_response()
}

async fn clear_pin(State(state): State<PinState>) -> Json<serde_json::Value> {
    state.pins.clear();
    tracing::warn!("Phase 0 account pins cleared");
    Json(serde_json::json!({"ok":true}))
}

#[cfg(test)]
mod tests {
    #[test]
    fn session_precedes_global_and_restart_clears() {
        let pins = super::DebugPins::default();
        pins.set(None, "cx-global".into());
        pins.set(Some("codex:synthetic".into()), "cx-session".into());
        assert_eq!(
            pins.get(Some("codex:synthetic")).as_deref(),
            Some("cx-session")
        );
        assert_eq!(pins.get(Some("codex:other")).as_deref(), Some("cx-global"));
        pins.clear();
        assert!(pins.get(None).is_none());
        assert!(super::DebugPins::default().get(None).is_none());
    }
}
