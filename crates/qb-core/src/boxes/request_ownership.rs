//! Exact-grant lifetime accounting. No registry ownership, I/O, or task spawning.
use super::{BoxScope, Grant, RegistryError};
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct RequestOwnership {
    active: Mutex<usize>,
    changed: Notify,
}

impl RequestOwnership {
    pub(super) fn close(&self, revoked: &AtomicBool) {
        // The same gate protects the closed check and increment in admit.
        // Recover poison here so revocation remains fail-closed even on Drop.
        let _active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        revoked.store(true, Ordering::Release);
    }

    pub(super) async fn wait_quiescent(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Register before checking: the final owner may drop immediately
            // after the zero check and notify_waiters does not retain permits.
            changed.as_mut().enable();
            {
                let active = self
                    .active
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if *active == 0 {
                    return;
                }
            }
            changed.await;
        }
    }
}

/// One admitted request lifetime. Cloning retains the SAME lifetime; only its
/// final descendant releases the grant's count. This is not fresh admission.
#[derive(Clone)]
pub(crate) struct BoxRequest(Arc<Admitted>);

struct Admitted {
    grant: Arc<Grant>,
}

impl BoxRequest {
    pub(super) fn admit(grant: &Arc<Grant>) -> Result<Self, RegistryError> {
        let mut active = grant
            .requests
            .active
            .lock()
            .map_err(|_| RegistryError::Synchronization)?;
        if grant.revoked.load(Ordering::Acquire) {
            return Err(RegistryError::Unauthorized);
        }
        *active = active.checked_add(1).ok_or(RegistryError::Capacity)?;
        Ok(Self(Arc::new(Admitted {
            grant: grant.clone(),
        })))
    }

    /// Linearize fresh write admission with exact-grant revocation. Hold only
    /// the ownership gate and the operation's short state gate, in that order.
    pub(crate) fn admit_operation(
        &self,
        operation: &crate::issue_operation::IssueOperation,
    ) -> Result<(), ()> {
        // Required completion must not wait for or fail on a later revoke gate.
        if operation.is_admitted() {
            return operation.admit();
        }
        let _active = self.0.grant.requests.active.lock().map_err(|_| ())?;
        if !operation.is_admitted() && self.0.grant.revoked.load(Ordering::Acquire) {
            return Err(());
        }
        operation.admit()
    }

    pub fn scope(&self) -> &BoxScope {
        &self.0.grant.scope
    }

    /// A retained lifetime is not permission for fresh dispatch after revoke.
    pub fn is_revoked(&self) -> bool {
        self.0.grant.revoked.load(Ordering::Acquire)
    }
}

impl fmt::Debug for BoxRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoxRequest")
            .field("scope", self.scope())
            .finish_non_exhaustive()
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        let mut active = self
            .grant
            .requests
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *active -= 1;
        let drained = *active == 0;
        drop(active);
        if drained {
            self.grant.requests.changed.notify_waiters();
        }
    }
}
