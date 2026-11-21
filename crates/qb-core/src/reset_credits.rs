//! Single-use reset entitlements. No routing, paid-credit or quota policy lives here.
pub(crate) mod model;

use crate::accounts::Account;
use qb_proto::{reset_credit_action::*, reset_credits::*};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub(crate) struct ResetCredits {
    accounts: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<Entry>>>>,
    admission: Arc<tokio::sync::RwLock<()>>,
    closed: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub(crate) fail_terminal_write: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
pub(crate) struct Entry {
    pub(crate) inventory: Option<ResetCreditInventory>,
    pub(crate) preview: Option<Preview>,
}

pub(crate) struct Preview {
    pub(crate) wire: ResetCreditPreview,
    pub(crate) account: Account,
    // Observed-at is not an inventory revision. Compare semantic rows, count,
    // account metadata and the actual quota view separately before committing.
    pub(crate) windows: serde_json::Value,
}

impl ResetCredits {
    pub(crate) fn admit(&self) -> Result<tokio::sync::OwnedRwLockReadGuard<()>, Error> {
        use std::sync::atomic::Ordering;
        let guard = self
            .admission
            .clone()
            .try_read_owned()
            .map_err(|_| Error::Unavailable)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Unavailable);
        }
        Ok(guard)
    }

    pub(crate) async fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        // Join every retained commit through terminal persistence before the
        // native AccountStore lease may be handed to another process.
        let _joined = self.admission.write().await;
    }

    pub(crate) fn entry(&self, id: &str) -> Result<Arc<tokio::sync::Mutex<Entry>>, Error> {
        let mut accounts = self.accounts.lock().map_err(|_| Error::Unavailable)?;
        if !accounts.contains_key(id) && accounts.len() >= 128 {
            return Err(Error::Unavailable);
        }
        Ok(accounts.entry(id.into()).or_default().clone())
    }

    /// Drops the cached inventory and preview of a deleted account. An operation
    /// that already holds the entry keeps its own reference and finishes.
    pub(crate) fn forget(&self, id: &str) {
        if let Ok(mut accounts) = self.accounts.lock() {
            accounts.remove(id);
        }
    }

    #[cfg(test)]
    pub(crate) fn holds(&self, id: &str) -> bool {
        self.accounts.lock().is_ok_and(|a| a.contains_key(id))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Unavailable,
    Unsupported,
    Busy,
    Stale,
    Unknown,
    Ineligible,
}

pub(crate) fn inventory(state: ResetCreditState) -> ResetCreditInventory {
    ResetCreditInventory {
        state,
        observed_at: None,
        available_count: None,
        grants: Vec::new(),
        refusal: Some(match state {
            ResetCreditState::Unsupported => ResetCreditRefusal::Unsupported,
            _ => ResetCreditRefusal::Unknown,
        }),
        claude: None,
    }
}
