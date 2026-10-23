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
    sync::{Arc, RwLock, Weak},
};

#[derive(Default)]
pub struct DebugPins {
    owner: Arc<RwLock<DebugPinOwner>>,
}

#[derive(Default)]
struct DebugPinOwner {
    pins: HashMap<(String, Option<String>), String>,
    revision: u64,
}

impl DebugPinOwner {
    fn selected(&self, provider: &str, session: Option<&str>) -> Option<&String> {
        session
            .and_then(|session| {
                self.pins
                    .get(&(provider.to_owned(), Some(session.to_owned())))
            })
            .or_else(|| self.pins.get(&(provider.to_owned(), None)))
    }

    fn advance(&mut self) {
        // Never reuse a revision, even if the selected account is unchanged.
        self.revision = self
            .revision
            .checked_add(1)
            .expect("debug pin revision exhausted");
    }
}

/// Proof of a stored selection for one exact request, revoked by owner mutation or drop.
pub(crate) struct DebugPinEvidence {
    owner: Weak<RwLock<DebugPinOwner>>,
    revision: u64,
    provider: String,
    session: Option<String>,
    target: String,
}

impl DebugPinEvidence {
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    pub(crate) fn applies_to(
        &self,
        provider: Provider,
        session: Option<&str>,
        target: &str,
    ) -> bool {
        if self.provider != provider.as_str()
            || self.session.as_deref() != session
            || self.target != target
        {
            return false;
        }
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        let Ok(owner) = owner.read() else {
            return false;
        };
        owner.revision == self.revision
            && owner
                .selected(provider.as_str(), session)
                .map(String::as_str)
                == Some(target)
    }
}

impl DebugPins {
    pub fn get(&self, provider: Provider, session: Option<&str>) -> Option<String> {
        self.owner
            .read()
            .ok()?
            .selected(provider.as_str(), session)
            .cloned()
    }

    pub(crate) fn selection(
        &self,
        provider: Provider,
        session: Option<&str>,
    ) -> Option<DebugPinEvidence> {
        let owner = self.owner.read().ok()?;
        let target = owner.selected(provider.as_str(), session)?.clone();
        Some(DebugPinEvidence {
            owner: Arc::downgrade(&self.owner),
            revision: owner.revision,
            provider: provider.as_str().to_owned(),
            session: session.map(str::to_owned),
            target,
        })
    }
    pub fn set(&self, provider: Provider, session: Option<String>, account_id: String) {
        if let Ok(mut owner) = self.owner.write() {
            owner.advance();
            owner
                .pins
                .insert((provider.as_str().to_owned(), session), account_id);
        }
    }
    pub fn clear(&self) {
        if let Ok(mut owner) = self.owner.write() {
            owner.advance();
            owner.pins.clear();
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
    use super::DebugPins;
    use crate::accounts::Provider::{Claude, Codex};

    #[test]
    fn evidence_requires_exact_provider_session_and_target() {
        let pins = DebugPins::default();
        assert!(pins.selection(Codex, Some("session-a")).is_none());
        pins.set(Codex, Some("session-a".into()), "account-a".into());
        pins.set(Claude, Some("session-a".into()), "account-a".into());
        pins.set(Codex, Some("session-b".into()), "account-a".into());
        let evidence = pins.selection(Codex, Some("session-a")).unwrap();
        assert_eq!(evidence.target(), "account-a");
        assert!(evidence.applies_to(Codex, Some("session-a"), "account-a"));
        assert!(!evidence.applies_to(Claude, Some("session-a"), "account-a"));
        assert!(!evidence.applies_to(Codex, Some("session-b"), "account-a"));
        assert!(!evidence.applies_to(Codex, None, "account-a"));
        assert!(!evidence.applies_to(Codex, Some("session-a"), "account-b"));
    }

    #[test]
    fn global_fallback_evidence_is_bound_to_request_session() {
        let pins = DebugPins::default();
        pins.set(Codex, None, "global".into());
        let fallback = pins.selection(Codex, Some("session-a")).unwrap();
        let global = pins.selection(Codex, None).unwrap();
        assert_eq!(fallback.target(), "global");
        assert!(fallback.applies_to(Codex, Some("session-a"), "global"));
        assert!(!fallback.applies_to(Codex, Some("session-b"), "global"));
        assert!(!fallback.applies_to(Codex, None, "global"));
        assert!(global.applies_to(Codex, None, "global"));
        assert!(!global.applies_to(Codex, Some("session-a"), "global"));
        assert!(pins.selection(Claude, None).is_none());
    }

    #[test]
    fn session_precedence_change_revokes_global_fallback_evidence() {
        let pins = DebugPins::default();
        pins.set(Codex, None, "global".into());
        let fallback = pins.selection(Codex, Some("session-a")).unwrap();
        pins.set(Codex, Some("session-a".into()), "session".into());
        assert!(!fallback.applies_to(Codex, Some("session-a"), "global"));
        let selected = pins.selection(Codex, Some("session-a")).unwrap();
        assert_eq!(selected.target(), "session");
        assert!(selected.applies_to(Codex, Some("session-a"), "session"));
        assert_eq!(
            pins.selection(Codex, Some("session-b")).unwrap().target(),
            "global"
        );
    }

    #[test]
    fn clear_and_identical_reinsert_do_not_restore_evidence() {
        let pins = DebugPins::default();
        pins.set(Codex, None, "account".into());
        let original = pins.selection(Codex, None).unwrap();
        pins.clear();
        assert!(pins.selection(Codex, None).is_none());
        assert!(!original.applies_to(Codex, None, "account"));
        pins.set(Codex, None, "account".into());
        assert!(!original.applies_to(Codex, None, "account"));
        let reinserted = pins.selection(Codex, None).unwrap();
        assert!(reinserted.applies_to(Codex, None, "account"));
        pins.set(Codex, None, "account".into());
        assert!(!reinserted.applies_to(Codex, None, "account"));
        assert!(
            pins.selection(Codex, None)
                .unwrap()
                .applies_to(Codex, None, "account")
        );
    }

    #[test]
    fn dropping_owner_revokes_evidence_even_with_identical_new_owner() {
        let evidence = {
            let pins = DebugPins::default();
            pins.set(Codex, None, "account".into());
            let evidence = pins.selection(Codex, None).unwrap();
            assert!(evidence.applies_to(Codex, None, "account"));
            evidence
        };
        let replacement = DebugPins::default();
        replacement.set(Codex, None, "account".into());
        assert_eq!(evidence.target(), "account");
        assert!(!evidence.applies_to(Codex, None, "account"));
        assert!(
            replacement
                .selection(Codex, None)
                .unwrap()
                .applies_to(Codex, None, "account")
        );
    }

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
