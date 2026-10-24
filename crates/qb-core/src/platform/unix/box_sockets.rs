//! Owned Unix box transports backed by the retained activation bank.
pub(super) mod transport;
use self::transport::{BoundBoxSocket, BoxTransport, SocketDirectory, SocketIdentity, SocketNode};
use super::listener;
use crate::boxes::BoxScope;
use axum::Router;
use std::{collections::BTreeMap, io, path::PathBuf};

type Key = (String, u64);
fn key(scope: &BoxScope) -> Key {
    (scope.name().as_str().to_owned(), scope.epoch())
}
fn unavailable() -> io::Error {
    io::Error::other("box socket lifecycle unavailable")
}
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "box sockets require managed Linux service",
    )
}
struct Entry {
    scope: BoxScope,
    transport: Option<BoxTransport>,
    bank_owned: bool,
    retiring: bool,
    joined: bool,
    join_failed: bool,
}
#[must_use = "retain through startup errors and terminally join before RuntimeDrop"]
pub(crate) struct SocketSet {
    directory: Option<SocketDirectory>,
    entries: BTreeMap<Key, Entry>,
    unretained: Option<BoundBoxSocket>,
    rejected_node: Option<SocketNode>,
    started: bool,
    managed: bool,
    draining: bool,
    frozen: bool,
    failed: bool,
}
impl SocketSet {
    /// Preparation only: no socket or registry IO; empty managed still loads
    /// durable dynamic scopes. Actual activation must already be initialized.
    pub(crate) fn requires_registry(declared: &[String]) -> io::Result<bool> {
        if listener::box_activation_managed()? {
            return Ok(true);
        }
        if declared.is_empty() {
            Ok(false)
        } else {
            Err(unsupported())
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            directory: None,
            entries: BTreeMap::new(),
            unretained: None,
            rejected_node: None,
            started: false,
            managed: false,
            draining: false,
            frozen: false,
            failed: false,
        }
    }
    pub(crate) fn start(
        &mut self,
        declared: &[String],
        active: &[BoxScope],
        mut make_router: impl FnMut(BoxScope) -> Router,
    ) -> io::Result<()> {
        if self.started {
            return Err(unavailable());
        }
        self.started = true;
        let result = (|| {
            self.managed = listener::box_activation_managed()?;
            if !self.managed {
                return if declared.is_empty() && active.is_empty() {
                    Ok(())
                } else {
                    Err(unsupported())
                };
            }
            let root = std::env::var_os("RUNTIME_DIRECTORY").ok_or_else(unavailable)?;
            self.directory = Some(SocketDirectory::open(&PathBuf::from(root), unsafe {
                libc::getegid()
            })?);
            self.reconcile(active)?; // Required even when the durable list is empty.
            for scope in active {
                self.serve_scope(scope, &mut make_router)?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub(crate) fn reconcile(&mut self, active: &[BoxScope]) -> io::Result<()> {
        if !self.started {
            return Err(unavailable());
        }
        if !self.managed {
            return if active.is_empty() {
                Ok(())
            } else {
                Err(unsupported())
            };
        }
        listener::reconcile_boxes(self.directory.as_ref().ok_or_else(unavailable)?, active)
    }
    pub(crate) fn create(
        &mut self,
        active: &[BoxScope],
        scope: &BoxScope,
        make_router: impl FnOnce(BoxScope) -> Router,
    ) -> io::Result<()> {
        if !self.managed {
            return Err(unsupported());
        }
        if self.draining || self.failed || !active.contains(scope) {
            return Err(unavailable());
        }
        let result = (|| {
            self.reconcile(active)?;
            self.serve_scope(scope, make_router)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn serve_scope(
        &mut self,
        scope: &BoxScope,
        make_router: impl FnOnce(BoxScope) -> Router,
    ) -> io::Result<()> {
        if self.draining || self.failed || self.entries.keys().any(|k| k.0 == scope.name().as_str())
        {
            return Err(unavailable());
        }
        // Store partial ownership BEFORE any fallible operation or spawn.
        let id = key(scope);
        self.entries.insert(
            id.clone(),
            Entry {
                scope: scope.clone(),
                transport: None,
                bank_owned: false,
                retiring: false,
                joined: false,
                join_failed: false,
            },
        );
        let present = listener::has_box_scope(scope)?;
        if present {
            self.entries.get_mut(&id).unwrap().bank_owned = true;
        } else {
            self.unretained = Some(BoundBoxSocket::bind_new(
                self.directory.as_ref().ok_or_else(unavailable)?,
                SocketIdentity::new(scope.clone()),
            )?);
            let result = listener::retain_box(&mut self.unretained);
            self.entries.get_mut(&id).unwrap().bank_owned = self.unretained.is_none();
            result?;
        }
        let serving = listener::duplicate_box(scope)?;
        // Actual listener scope invokes the factory; no caller-selected extension.
        let transport = BoxTransport::start(serving, make_router);
        self.entries.get_mut(&id).unwrap().transport = Some(transport);
        Ok(())
    }
    pub(crate) fn begin_revoke(&mut self, scope: &BoxScope) -> io::Result<()> {
        let entry = self.entries.get_mut(&key(scope)).ok_or_else(unavailable)?;
        let bank = if entry.bank_owned {
            listener::begin_box_retirement(scope)
        } else {
            Ok(())
        };
        entry.retiring = true;
        // Even a bank error must not skip fail-closed shutdown of owned IO.
        let close = match &entry.transport {
            Some(t) => t.revoke(),
            None => Ok(()),
        };
        if bank.is_err() || close.is_err() {
            self.failed = true;
            return Err(unavailable());
        }
        Ok(())
    }
    pub(crate) async fn join_scope(&mut self, scope: &BoxScope) -> io::Result<()> {
        let entry = self.entries.get_mut(&key(scope)).ok_or_else(unavailable)?;
        if !entry.retiring {
            return Err(unavailable());
        }
        if let Some(transport) = entry.transport.as_mut() {
            // Stored handle remains owned if this borrowed wait is cancelled.
            entry.join_failed |= transport.join().await.is_err();
        }
        entry.joined = true; // None means no transport ever spawned, not a fake server.
        if entry.join_failed {
            self.failed = true;
            return Err(unavailable());
        }
        Ok(())
    }
    /// Common owner must durably revoke and terminally join/flush ALL scoped
    /// writers between join_scope and this call. No native inference of that fact.
    pub(crate) fn finish_retirement(&mut self, scope: &BoxScope) -> io::Result<()> {
        let id = key(scope);
        let entry = self.entries.get(&id).ok_or_else(unavailable)?;
        if !entry.retiring || !entry.joined {
            return Err(unavailable());
        }
        if entry.bank_owned {
            listener::finish_box_retirement(&entry.scope)?;
        } else {
            self.cleanup_rejected()?;
        }
        self.entries.remove(&id); // Transport parent/IO already terminally joined.
        Ok(()) // Sticky set/service/transport failure is NOT cleared.
    }
    pub(crate) fn begin_drain(&mut self) {
        self.draining = true;
        for entry in self.entries.values() {
            if let Some(transport) = &entry.transport {
                transport.begin_drain();
            }
        }
    }
    pub(crate) async fn join_all(&mut self) -> io::Result<()> {
        self.begin_drain();
        for entry in self.entries.values_mut() {
            if let Some(transport) = entry.transport.as_mut() {
                entry.join_failed |= transport.join().await.is_err();
            }
            entry.joined = true;
            self.failed |= entry.join_failed; // Continue ALL remaining terminal joins.
        }
        if self.cleanup_rejected().is_err() {
            self.failed = true;
        }
        if self.failed {
            Err(unavailable())
        } else {
            Ok(())
        }
    }
    fn cleanup_rejected(&mut self) -> io::Result<()> {
        if let Some(bound) = self.unretained.take() {
            let (original, _identity, node) = bound.into_parts();
            self.rejected_node = Some(node); // Record captured evidence before closing.
            drop(original); // Never retained/served: there are no associated handlers.
        }
        if let Some(node) = self.rejected_node.as_ref() {
            node.unlink_owned_or_sync_absent()?;
        }
        self.rejected_node.take();
        Ok(())
    }
    pub(crate) fn freeze(&mut self, active: &[BoxScope]) -> io::Result<()> {
        if !self.started {
            return Err(unavailable());
        }
        if !self.managed {
            return if active.is_empty() {
                Ok(())
            } else {
                Err(unsupported())
            };
        }
        if self.failed || self.entries.values().any(|e| e.retiring) {
            return Err(unavailable());
        }
        self.reconcile(active)?;
        listener::freeze_boxes()?;
        self.frozen = true;
        self.draining = true; // Stop enrollment; begin_drain signals existing servers.
        Ok(())
    }
    pub(crate) fn check_complete(&self) -> io::Result<()> {
        if self.failed
            || self.entries.values().any(|e| !e.joined || e.retiring)
            || self.unretained.is_some()
            || self.rejected_node.is_some()
            || (self.managed && !self.frozen)
        {
            return Err(unavailable());
        }
        Ok(())
    }
}
