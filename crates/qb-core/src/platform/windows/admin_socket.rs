//! Unix peer-authorized admin ingress is unavailable on Windows.
use std::io;
use tokio::sync::watch;

pub(crate) fn admin_socket_path_if_allowed() -> Option<std::path::PathBuf> {
    None
}

#[must_use = "signal shutdown and join task before runtime drop"]
pub(crate) struct AdminSocketServer {
    pub closed: watch::Receiver<bool>,
}
impl AdminSocketServer {
    pub async fn join(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) async fn start_admin_socket(
    _routes: axum::Router,
    _admin_port: u16,
    _shutdown: watch::Receiver<bool>,
) -> anyhow::Result<Option<AdminSocketServer>> {
    Ok(None)
}
