//! Absent boxes preserve existing Windows/TCP behavior; scopes are unsupported.
use crate::boxes::BoxScope;
use axum::Router;
use std::io;

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "box sockets are unavailable on this platform",
    )
}
pub(crate) struct SocketSet;
impl SocketSet {
    pub(crate) fn requires_registry(declared: &[String]) -> io::Result<bool> {
        if declared.is_empty() {
            Ok(false)
        } else {
            Err(unsupported())
        }
    }

    pub(crate) fn new() -> Self {
        Self
    }
    pub(crate) fn start(
        &mut self,
        declared: &[String],
        active: &[BoxScope],
        _make_router: impl FnMut(BoxScope) -> Router,
    ) -> io::Result<()> {
        if declared.is_empty() && active.is_empty() {
            Ok(())
        } else {
            Err(unsupported())
        }
    }
    pub(crate) fn create(
        &mut self,
        _active: &[BoxScope],
        _scope: &BoxScope,
        _make_router: impl FnOnce(BoxScope) -> Router,
    ) -> io::Result<()> {
        Err(unsupported())
    }
    pub(crate) fn reconcile(&mut self, active: &[BoxScope]) -> io::Result<()> {
        if active.is_empty() {
            Ok(())
        } else {
            Err(unsupported())
        }
    }
    pub(crate) fn begin_revoke(&mut self, _scope: &BoxScope) -> io::Result<()> {
        Err(unsupported())
    }
    pub(crate) async fn join_scope(&mut self, _scope: &BoxScope) -> io::Result<()> {
        Err(unsupported())
    }
    pub(crate) fn finish_retirement(&mut self, _scope: &BoxScope) -> io::Result<()> {
        Err(unsupported())
    }
    pub(crate) fn begin_drain(&mut self) {}
    pub(crate) async fn join_all(&mut self) -> io::Result<()> {
        Ok(())
    }
    pub(crate) fn freeze(&mut self, active: &[BoxScope]) -> io::Result<()> {
        self.reconcile(active)
    }
    pub(crate) fn check_complete(&self) -> io::Result<()> {
        Ok(())
    }
}
