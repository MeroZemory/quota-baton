use super::*;
#[cfg(feature = "testkit")]
pub(super) type ProbeState = ();

pub fn account_routes(_store: Arc<AccountStore>) -> Router {
    Router::new()
}
pub fn account_routes_with_poller(
    _store: Arc<AccountStore>,
    _poller: Arc<crate::quota::UsagePoller>,
) -> Router {
    Router::new()
}
pub(super) fn runtime_routes(router: Router<RuntimeAdmin>) -> Router<RuntimeAdmin> {
    router
}
pub(super) fn capabilities() -> qb_proto::StateCapabilities {
    qb_proto::StateCapabilities {
        account_adopt: false,
        manual_refresh: true,
        account_takeover: false,
    }
}
pub(super) async fn adopt_account(
    _state: RuntimeAdmin,
    _id: String,
    _request: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    error(StatusCode::NOT_FOUND, "Unknown account action")
}
