// These isolated credential fixtures have no authenticated box request, so their
// task helpers use raw Tokio. Scoped production wrappers are tested in
// boxes::request_context.
pub(crate) use tokio::{spawn, task::spawn_blocking};

// These fixtures cannot construct a scoped admission; retain the production
// optional-field shape without introducing a mock grant.
pub(crate) type BoxRequest = std::convert::Infallible;
pub(crate) fn current_request() -> Option<BoxRequest> {
    None
}
