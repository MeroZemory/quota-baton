//! Common bootstrap/shutdown owner. No admin mutations or OS logic here.
mod admin_owner;
use crate::{
    boxes::{BoxRegistry, BoxScope},
    config::Config,
    platform::box_sockets::SocketSet,
    relay::RelayState,
};
pub use admin_owner::BoxServiceOwner;
use axum::Router;
use std::{future::ready, path::Path, sync::Arc};
use zeroize::Zeroizing;

fn unavailable() -> anyhow::Error {
    anyhow::anyhow!("box service lifecycle unavailable")
}

/// Own outside RelayState. Keep this SAME handle across start errors, shutdown
/// and cancelled borrowed waits; Drop is neither a terminal join nor cleanup.
#[must_use = "retain and explicitly drain/join/freeze/check before P2 finish"]
pub struct BoxService {
    sockets: SocketSet,
    registry: Option<BoxRegistry>,
    declared: Vec<String>,
    visible_port: u16,
    start_attempted: bool,
    stopping: bool,
    failed: bool,
}
impl BoxService {
    /// Synchronous native/registry IO; caller retains service lease and owns its
    /// execution context. Creates NO socket, router task, actor or auth worker.
    pub fn prepare(home: &Path, config: &Config) -> anyhow::Result<Self> {
        config.validate_boxes().map_err(|_| unavailable())?;
        anyhow::ensure!(
            config
                .boxes
                .iter()
                .all(|endpoint| endpoint.listen == crate::config::BoxListen::Unix),
            "TCP box listeners are not available in this build"
        );
        let declared: Vec<_> = config.boxes.iter().map(|b| b.name.clone()).collect();
        let sockets = SocketSet::new();
        // Separate narrow native preparation query dependency; no OS cfg or
        // registry reads on the unsupported platform's empty/default path.
        let registry = if SocketSet::requires_registry(&declared)? {
            Some(BoxRegistry::open(home).map_err(|_| unavailable())?)
        } else {
            None
        };
        Ok(Self {
            sockets,
            registry,
            declared,
            visible_port: config.server.data_port,
            start_attempted: false,
            stopping: false,
            failed: false,
        })
    }

    /// Borrow a PREPARED, retained owner; errors may follow real socket spawns.
    /// Runtime context required by the native Tokio listener/server operations.
    pub fn start(&mut self, relay: Arc<RelayState>) -> anyhow::Result<()> {
        if self.start_attempted || self.stopping {
            return Err(unavailable());
        }
        self.start_attempted = true;
        let result = (|| {
            let listeners = self.listener_scopes()?;
            match self.registry.as_ref() {
                Some(registry) => self.sockets.start(
                    &self.declared,
                    &listeners,
                    protected_factory(registry.clone(), relay, self.visible_port),
                ),
                None => self.sockets.start(&self.declared, &listeners, |_| {
                    // Native inactive/empty contract never invokes a router factory.
                    // Do not fabricate an unprotected Router or an auth success.
                    unreachable!("inactive box socket set invoked router factory")
                }),
            }
            .map_err(|_| unavailable())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub fn begin_drain(&mut self) {
        self.stopping = true;
        self.sockets.begin_drain();
    }

    /// Actual stored transport/IO terminal waits; no timeout, abort or taken
    /// JoinHandle. Does NOT claim that relay/admin blocking writers are complete.
    pub async fn join(&mut self) -> anyhow::Result<()> {
        self.begin_drain();
        if self.sockets.join_all().await.is_err() {
            self.failed = true;
        }
        if self.failed {
            Err(unavailable())
        } else {
            Ok(())
        }
    }

    /// Common owner has stopped mutations/resolved retirements. Fresh registry
    /// snapshot + bank freeze includes the empty managed bank before P2 exec.
    /// Failure never authorizes skipping join/core durable cleanup.
    pub fn freeze(&mut self) -> anyhow::Result<()> {
        self.stopping = true;
        let result = (|| {
            let listeners = self.listener_scopes()?;
            self.sockets.freeze(&listeners).map_err(|_| unavailable())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub fn check_complete(&self) -> anyhow::Result<()> {
        if !self.start_attempted || !self.stopping || self.failed {
            return Err(unavailable());
        }
        self.sockets.check_complete().map_err(|_| unavailable())
    }

    // A durable key identity alone does not request a listener. Keep this same
    // selection for startup/reconciliation and freeze so inherited undeclared
    // listeners remain a bank mismatch, never silently adopted or unlinked.
    fn listener_scopes(&self) -> anyhow::Result<Vec<BoxScope>> {
        let mut active = self.active_scopes()?;
        active.retain(|scope| {
            self.declared
                .iter()
                .any(|name| name == scope.name().as_str())
        });
        Ok(active)
    }

    fn active_scopes(&self) -> anyhow::Result<Vec<BoxScope>> {
        match &self.registry {
            Some(registry) => Ok(registry
                .list()
                .map_err(|_| unavailable())?
                .into_iter()
                .filter(|entry| !entry.revoked)
                .map(|entry| entry.scope)
                .collect()),
            None => Ok(Vec::new()), // Truly inactive platform path; no fabricated registry.
        }
    }
}

#[cfg(test)]
#[path = "box_service_tests.rs"]
mod tests;

fn protected_factory(
    registry: BoxRegistry,
    relay: Arc<RelayState>,
    port: u16,
) -> impl FnMut(BoxScope) -> Router {
    move |scope| {
        let authenticate = registry.clone();
        crate::http::protect_box_routes(
            crate::relay::router(relay.clone()),
            scope,
            port,
            move |expected: BoxScope, key: Zeroizing<String>| {
                // Coordinator's exact try-lock registry delta, never duplicate
                // hash/validation logic and never spawn per-request blocking work.
                let result = authenticate.try_authenticate(&expected, key.as_str());
                ready(result) // key is zeroized on callback return, before any await.
            },
        )
    }
}
