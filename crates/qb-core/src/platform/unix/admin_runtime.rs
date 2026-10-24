//! Linux has no CPA adoption state or callable adoption probe.
use super::*;

#[derive(Clone)]
pub(super) struct RuntimeAdmin {
    pub(super) relay: Arc<crate::relay::RelayState>,
    pub(super) data_port: u16,
}

pub fn runtime_routes(relay: Arc<crate::relay::RelayState>, data_port: u16) -> Router {
    runtime_router(RuntimeAdmin { relay, data_port })
}

#[cfg(feature = "testkit")]
pub fn runtime_routes_with_adoption_probe(
    relay: Arc<crate::relay::RelayState>,
    data_port: u16,
    _probe: fn() -> source_features::ProbeState,
) -> Router {
    runtime_router(RuntimeAdmin { relay, data_port })
}
