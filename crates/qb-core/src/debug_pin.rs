use crate::accounts::{AccountStore, Provider};
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
    pins: RwLock<HashMap<(String, Option<String>), String>>,
}

impl DebugPins {
    pub fn get(&self, provider: Provider, session: Option<&str>) -> Option<String> {
        let pins = self.pins.read().ok()?;
        let family = provider.as_str().to_owned();
        session
            .and_then(|session| pins.get(&(family.clone(), Some(session.to_owned()))))
            .or_else(|| pins.get(&(family, None)))
            .cloned()
    }
    pub fn set(&self, provider: Provider, session: Option<String>, account_id: String) {
        if let Ok(mut pins) = self.pins.write() {
            pins.insert((provider.as_str().to_owned(), session), account_id);
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
    let account = match tokio::task::spawn_blocking(move || store.load(&id)).await {
        Ok(Ok(account)) => account,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":{"message":"계정을 찾을 수 없습니다."}})),
            )
                .into_response();
        }
    };
    // 세션 식별자는 로그에 남기지 않는다.
    tracing::warn!(account_id=%request.account_id, "Phase 0 account pin enabled");
    state
        .pins
        .set(account.provider, request.session_key, request.account_id);
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
        use crate::accounts::Provider::{Claude, Codex};
        let pins = super::DebugPins::default();
        pins.set(Codex, None, "cx-global".into());
        pins.set(Codex, Some("codex:synthetic".into()), "cx-session".into());
        assert_eq!(
            pins.get(Codex, Some("codex:synthetic")).as_deref(),
            Some("cx-session")
        );
        assert_eq!(
            pins.get(Codex, Some("codex:other")).as_deref(),
            Some("cx-global")
        );
        assert!(pins.get(Claude, None).is_none());
        pins.clear();
        assert!(pins.get(Codex, None).is_none());
        assert!(super::DebugPins::default().get(Codex, None).is_none());
    }
}
