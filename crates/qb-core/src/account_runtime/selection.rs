//! Owning evidence of native runtime blockers, independent of persisted health.

use super::AccountRuntime;
use std::sync::{Arc, Mutex, Weak};

const MAX_ID_BYTES: usize = 128;

/// One revision per runtime: no historical IDs or generations are retained.
/// Every native blocker mutation invalidates all outstanding evidence, including
/// evidence for other accounts. Allocation identity avoids revision wrap/ABA.
#[derive(Default)]
pub(super) struct SelectionOwner {
    revision: Mutex<Arc<()>>,
}

impl SelectionOwner {
    /// Called with the mutated probe/volatile guard still held. Poison closes
    /// capture and validation without changing native blocker behaviour.
    pub(super) fn invalidate(&self) {
        if let Ok(mut revision) = self.revision.lock() {
            *revision = Arc::new(());
        }
    }
}

/// Process-local proof for a mandatory move, never a health diagnosis or a
/// breaker fresh-health exception. Private fields prevent caller fabrication.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeUnavailableEvidence {
    runtime: Weak<AccountRuntime>,
    id: String,
    revision: Arc<()>,
}

impl RuntimeUnavailableEvidence {
    /// Valid only for the captured account and the original live runtime.
    /// Successful consumption linearizes admission; later overlapping owner
    /// changes do not require holding runtime locks across policy I/O.
    /// Takes only the revision lock, so admission may already hold native locks.
    pub(crate) fn applies_to(&self, id: &str) -> bool {
        if self.id != id {
            return false;
        }
        let Some(runtime) = self.runtime.upgrade() else {
            return false;
        };
        runtime
            .selection_owner
            .revision
            .lock()
            .is_ok_and(|revision| Arc::ptr_eq(&revision, &self.revision))
    }
}

impl AccountRuntime {
    /// Capture an actual probe or volatile blocker under the native lock order.
    /// Missing, oversized or poisoned context yields no owning proof, even when
    /// the conservative boolean blocker accessors would exclude the account.
    pub(crate) fn selection_unavailable(
        self: &Arc<Self>,
        id: &str,
    ) -> Option<RuntimeUnavailableEvidence> {
        if id.is_empty() || id.len() > MAX_ID_BYTES {
            return None;
        }
        let probes = self.probes.lock().ok()?;
        let volatile = self.volatile.lock().ok()?;
        if !probes.contains(id) && !volatile.contains_key(id) {
            return None;
        }
        let revision = self.selection_owner.revision.lock().ok()?.clone();
        Some(RuntimeUnavailableEvidence {
            runtime: Arc::downgrade(self),
            id: id.to_owned(),
            revision,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::{
            Account, AccountStore, Health, HealthReason, Identity, Provider, Source, TokenMeta,
            UserState,
        },
        observability::Observability,
    };
    use chrono::{Duration, Utc};

    // Fresh project-local homes only; no tokens, persisted accounts, environment
    // configuration or shared account fixtures are needed by these tests.
    fn synthetic_runtime() -> (Arc<AccountRuntime>, tempfile::TempDir, Account) {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/runtime-selection-tests");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let runtime = AccountRuntime::new(store, obs, 0.0);
        let identity = Identity::Codex {
            user_id: None,
            account_id: "synthetic-runtime-selection".into(),
            email: "runtime-selection@example.invalid".into(),
        };
        let account = Account {
            schema: 1,
            id: identity.id(),
            provider: Provider::Codex,
            label: "synthetic-runtime-selection".into(),
            email: "runtime-selection@example.invalid".into(),
            plan: None,
            identity,
            source: Source::Oauth,
            user_state: UserState::Active,
            health: Health::Ready,
            auth_failures: 0,
            payment_failures: 0,
            payment_until: None,
            auth_probe_pending: true,
            quota_cooling: None,
            quota_credit_releasable: false,
            drain: false,
            allow_credits: false,
            generation: 0,
            token_meta: TokenMeta::default(),
        };
        (runtime, home, account)
    }

    #[test]
    fn probe_drop_and_reinsert_never_revives_evidence() {
        let (runtime, _home, account) = synthetic_runtime();
        assert!(runtime.selection_unavailable(&account.id).is_none());
        let probe = runtime.begin_probe(&account).unwrap().unwrap();
        let proof = runtime.selection_unavailable(&account.id).unwrap();
        assert!(proof.applies_to(&account.id));
        assert!(runtime.begin_probe(&account).is_err());
        assert!(proof.applies_to(&account.id));
        drop(probe);
        assert!(!proof.applies_to(&account.id));
        let _replacement = runtime.begin_probe(&account).unwrap().unwrap();
        assert!(!proof.applies_to(&account.id));
        assert!(
            runtime
                .selection_unavailable(&account.id)
                .unwrap()
                .applies_to(&account.id)
        );
    }

    #[test]
    fn volatile_clear_and_identical_reinsert_never_revives_evidence() {
        let (runtime, _home, mut account) = synthetic_runtime();
        account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: Utc::now() + Duration::hours(1),
            scope: "account".into(),
        };
        runtime.emit_account(&account);
        let proof = runtime.selection_unavailable(&account.id).unwrap();
        assert!(proof.applies_to(&account.id));
        runtime.clear_memory_health(&account);
        assert!(!proof.applies_to(&account.id));
        runtime.emit_account(&account);
        assert!(!proof.applies_to(&account.id));
        let proof = runtime.selection_unavailable(&account.id).unwrap();
        runtime.clear_memory_health_if(&account, true);
        assert!(!proof.applies_to(&account.id));
        runtime.emit_account(&account);
        assert!(!proof.applies_to(&account.id));
    }

    #[test]
    fn volatile_replacement_and_quota_only_clear_preserve_native_semantics() {
        let (runtime, _home, mut account) = synthetic_runtime();
        account.health = Health::Cooling {
            reason: HealthReason::Rate,
            until: Utc::now() + Duration::hours(1),
            scope: "account".into(),
        };
        runtime.emit_account(&account);
        let proof = runtime.selection_unavailable(&account.id).unwrap();
        runtime.clear_memory_health_if(&account, true);
        assert!(proof.applies_to(&account.id));
        assert!(runtime.blocked_in_memory(&account.id));
        runtime.emit_account(&account);
        assert!(!proof.applies_to(&account.id));
    }

    #[test]
    fn proof_is_bound_to_account_and_live_owner_not_replacement_context() {
        let (runtime, _home, account) = synthetic_runtime();
        // Even a Ready volatile entry is a native blocker, not health evidence.
        runtime.emit_account(&account);
        let proof = runtime.selection_unavailable(&account.id).unwrap();
        assert!(!proof.applies_to("synthetic-other-account"));
        assert!(runtime.selection_unavailable("").is_none());
        assert!(
            runtime
                .selection_unavailable(&"x".repeat(MAX_ID_BYTES + 1))
                .is_none()
        );
        // Validation must work while admission holds both native blocker locks.
        {
            let _probes = runtime.probes.lock().unwrap();
            let _volatile = runtime.volatile.lock().unwrap();
            assert!(proof.applies_to(&account.id));
        }
        let weak = Arc::downgrade(&runtime);
        drop(runtime);
        assert!(weak.upgrade().is_none());
        let (replacement, _replacement_home, same_account) = synthetic_runtime();
        replacement.emit_account(&same_account);
        assert!(!proof.applies_to(&same_account.id));
        assert!(
            replacement
                .selection_unavailable(&same_account.id)
                .unwrap()
                .applies_to(&same_account.id)
        );
    }

    #[test]
    fn one_blocker_ending_does_not_reuse_its_proof_for_the_remaining_blocker() {
        let (runtime, _home, account) = synthetic_runtime();
        let probe = runtime.begin_probe(&account).unwrap().unwrap();
        runtime.emit_account(&account);
        let proof = runtime.selection_unavailable(&account.id).unwrap();
        drop(probe);
        assert!(!proof.applies_to(&account.id));
        let remaining = runtime.selection_unavailable(&account.id).unwrap();
        assert!(remaining.applies_to(&account.id));
        runtime.clear_memory_health(&account);
        assert!(!remaining.applies_to(&account.id));
        assert!(runtime.selection_unavailable(&account.id).is_none());
    }

    #[test]
    fn unrelated_mutation_conservatively_invalidates_without_retaining_ids() {
        let (runtime, _home, account) = synthetic_runtime();
        let _probe = runtime.begin_probe(&account).unwrap().unwrap();
        let proof = runtime.selection_unavailable(&account.id).unwrap();
        let mut other = account.clone();
        other.id = "synthetic-other-account".into();
        runtime.emit_account(&other);
        assert!(!proof.applies_to(&account.id));
        assert!(
            runtime
                .selection_unavailable(&account.id)
                .unwrap()
                .applies_to(&account.id)
        );
    }
}
