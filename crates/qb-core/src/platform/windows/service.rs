//! Windows retains its existing foreground lifecycle and takeover path.
use crate::{config::Config, relay::RelayState};
use std::{future::Future, path::Path, sync::Arc};
pub(crate) fn prepare_process() -> anyhow::Result<()> {
    Ok(())
}
pub(crate) async fn handle_takeover(_: &Path, _: &Config, _: bool) -> anyhow::Result<bool> {
    Ok(false)
}
pub(crate) fn initialize_listeners(_: &Config) -> anyhow::Result<()> {
    Ok(())
}
pub(crate) fn routes(_: Arc<RelayState>) -> axum::Router {
    axum::Router::new()
}
pub(crate) fn notify_ready() -> anyhow::Result<()> {
    Ok(())
}
pub(crate) async fn wait_for_shutdown(relay: Arc<RelayState>) -> anyhow::Result<()> {
    tokio::select! {
        result = tokio::signal::ctrl_c() => { result?; }
        _ = relay.wait_for_drain() => {}
    }
    Ok(())
}
pub(crate) fn should_release_lease_early() -> bool {
    true
}
pub(crate) async fn complete_shutdown<F: Future<Output = anyhow::Result<()>>>(
    work: F,
) -> anyhow::Result<()> {
    work.await
}
pub(crate) fn finish() -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn validate_listener_drain(_: bool) -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn health_ready() -> bool {
    true
}
pub(crate) fn retry_durable_cleanup() -> bool {
    false
}

pub(crate) fn defer_error_exit(code: i32) {
    std::process::exit(code)
}
pub(crate) fn finish_process() -> anyhow::Result<()> {
    Ok(())
}
pub(crate) fn retain_observability(
    _: Arc<crate::observability::Observability>,
) -> anyhow::Result<()> {
    Ok(())
}
pub(crate) async fn drain_connections<F: Future<Output = bool>>(
    work: F,
    limit: std::time::Duration,
) -> bool {
    tokio::time::timeout(limit, work)
        .await
        .is_ok_and(|clean| clean)
}
pub(crate) fn defer_startup_error(
    result: anyhow::Result<()>,
) -> anyhow::Result<Option<anyhow::Error>> {
    result?;
    Ok(None)
}
