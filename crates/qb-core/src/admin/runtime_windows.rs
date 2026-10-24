//! Windows runtime admin state and existing adoption test hook.
use super::*;

#[derive(Clone)]
pub(super) struct RuntimeAdmin {
    pub(super) relay: Arc<crate::relay::RelayState>,
    pub(super) data_port: u16,
    #[cfg(feature = "testkit")]
    pub(super) adoption_probe: Option<fn() -> source_features::ProbeState>,
}

pub fn runtime_routes(relay: Arc<crate::relay::RelayState>, data_port: u16) -> Router {
    runtime_router(RuntimeAdmin {
        relay,
        data_port,
        #[cfg(feature = "testkit")]
        adoption_probe: None,
    })
}

#[cfg(feature = "testkit")]
pub fn runtime_routes_with_adoption_probe(
    relay: Arc<crate::relay::RelayState>,
    data_port: u16,
    probe: fn() -> source_features::ProbeState,
) -> Router {
    runtime_router(RuntimeAdmin {
        relay,
        data_port,
        adoption_probe: Some(probe),
    })
}
