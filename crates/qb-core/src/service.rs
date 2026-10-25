//! Headless service lifecycle; native operations live behind one platform boundary.
use crate::{config::Config, relay::RelayState};
use std::{future::Future, path::Path, sync::Arc};

pub fn prepare_process() -> anyhow::Result<()> {
    crate::platform::service::prepare_process()
}
pub async fn handle_takeover(home: &Path, config: &Config, takeover: bool) -> anyhow::Result<bool> {
    crate::platform::service::handle_takeover(home, config, takeover).await
}
pub fn initialize_listeners(config: &Config) -> anyhow::Result<()> {
    crate::platform::service::initialize_listeners(config)
}
pub fn routes(relay: Arc<RelayState>) -> axum::Router {
    crate::platform::service::routes(relay)
}
pub fn notify_ready() -> anyhow::Result<()> {
    crate::platform::service::notify_ready()
}
pub async fn wait_for_shutdown(relay: Arc<RelayState>) -> anyhow::Result<()> {
    crate::platform::service::wait_for_shutdown(relay).await
}
pub fn should_release_lease_early() -> bool {
    crate::platform::service::should_release_lease_early()
}
pub async fn complete_shutdown<F: Future<Output = anyhow::Result<()>>>(
    work: F,
) -> anyhow::Result<()> {
    crate::platform::service::complete_shutdown(work).await
}
pub fn finish() -> anyhow::Result<()> {
    crate::platform::service::finish()
}

pub fn validate_listener_drain(clean: bool) -> anyhow::Result<()> {
    crate::platform::service::validate_listener_drain(clean)
}

pub fn retry_durable_cleanup() -> bool {
    crate::platform::service::retry_durable_cleanup()
}

pub fn defer_error_exit(code: i32) {
    crate::platform::service::defer_error_exit(code)
}
pub fn finish_process() -> anyhow::Result<()> {
    crate::platform::service::finish_process()
}
pub fn retain_observability(obs: Arc<crate::observability::Observability>) -> anyhow::Result<()> {
    crate::platform::service::retain_observability(obs)
}
pub fn retain_store_lease(store: &crate::accounts::AccountStore) -> anyhow::Result<()> {
    store.retain_service_lease()
}

pub async fn drain_connections<F: Future<Output = bool>>(
    work: F,
    limit: std::time::Duration,
) -> bool {
    crate::platform::service::drain_connections(work, limit).await
}
pub fn defer_startup_error(result: anyhow::Result<()>) -> anyhow::Result<Option<anyhow::Error>> {
    crate::platform::service::defer_startup_error(result)
}

pub fn shutdown_retry_notice() {
    tracing::warn!("Waiting for durable service shutdown storage");
}

mod box_service;
pub use box_service::{BoxService, BoxServiceOwner};
