//! Per-box registry, ingress scope and optional native service ownership.
mod lineage;
mod registry;
mod request_context;

pub(crate) use lineage::{RootHint, take_root};
pub(crate) use registry::{
    BoxAuthority, BoxName, BoxRegistry, BoxRequest, BoxScope, BoxSummary, RegistryError,
};
pub(crate) use request_context::{
    admit_operation, current as current_request, dispatch_allowed, retain_response, scope_request,
    spawn, spawn_blocking,
};
