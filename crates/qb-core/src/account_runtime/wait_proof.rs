//! Restart-cold provenance fence, deliberately independent of Account and persistence.
//!
//! `E` is the runtime classifier's compact proof (including reset/scope/evidence).
//! Capture an origin BEFORE network IO; cached/restored snapshots cannot construct
//! one. This module cannot enforce that call-site ordering or classify raw reports.
//! Callbacks run under the same gate as invalidation: validate protected runtime
//! state and publish admission there, without network IO, waiting or reentry.
//! Integration must establish a consistent order with any outer account-store lock.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

const MAX_ENTRIES: usize = 4096;
const MAX_ID_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FenceError {
    InvalidId,
    Capacity,
    Missing,
    Stale,
    NoProof,
    Rejected,
    Exhausted,
    Poisoned,
}

// Old tokens retain these allocations, preventing address-reuse ABA. They are
// neither serializable nor derived from a PID, account ID or wall-clock timestamp.
#[derive(Clone, Debug)]
struct Identity(Arc<()>);

impl Identity {
    fn new() -> Self {
        Self(Arc::new(()))
    }

    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Immutable pre-network invalidation fence; cloning never recaptures state.
/// A WS producer may use clones for multiple reports. Every accepted `mint`
/// creates an independent proof/receipt identity, even for identical evidence.
#[derive(Clone, Debug)]
pub(crate) struct ReportOrigin {
    process: Identity,
    entry: Identity,
    id: String,
    generation: u64,
}

#[derive(Debug)]
pub(crate) struct WaitTicket {
    origin: ReportOrigin,
    proof: Identity,
}

struct Proof<E> {
    identity: Identity,
    evidence: E,
}

struct Entry<E> {
    identity: Identity,
    generation: Option<u64>,
    proof: Option<Proof<E>>,
}

pub(crate) struct WaitProofs<E> {
    process: Identity,
    capacity: usize,
    entries: Mutex<BTreeMap<String, Entry<E>>>,
}

impl<E> WaitProofs<E> {
    /// Capacity is capped; zero denies all new entries. No automatic eviction.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            process: Identity::new(),
            capacity: capacity.min(MAX_ENTRIES),
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Only the raw-report producer should call this, before starting network IO.
    /// Capturing does not establish proof or refresh an existing proof.
    pub(crate) fn capture(&self, id: &str) -> Result<ReportOrigin, FenceError> {
        if id.is_empty() || id.len() > MAX_ID_BYTES {
            return Err(FenceError::InvalidId);
        }
        let mut entries = self.entries.lock().map_err(|_| FenceError::Poisoned)?;
        if !entries.contains_key(id) {
            if entries.len() >= self.capacity {
                return Err(FenceError::Capacity);
            }
            entries.insert(
                id.to_owned(),
                Entry {
                    identity: Identity::new(),
                    generation: Some(0),
                    proof: None,
                },
            );
        }
        let entry = &entries[id];
        Ok(ReportOrigin {
            process: self.process.clone(),
            entry: entry.identity.clone(),
            id: id.to_owned(),
            generation: entry.generation.ok_or(FenceError::Exhausted)?,
        })
    }

    fn check(&self, origin: &ReportOrigin, entry: &Entry<E>) -> Result<(), FenceError> {
        if !self.process.same(&origin.process)
            || !entry.identity.same(&origin.entry)
            || entry.generation != Some(origin.generation)
        {
            return Err(FenceError::Stale);
        }
        Ok(())
    }

    /// Consume one handle to a raw origin and store already-classified evidence.
    /// A producer may clone its immutable origin for repeated WS receipts. Each
    /// successful call creates a new proof identity; origin equality, evidence
    /// equality and equal timestamps never preserve a previous ticket.
    /// This is not a classifier, freshness check or out-of-order report filter.
    /// The caller must reject older/contradictory reports under the outer gate.
    /// A replacement
    /// gets a fresh proof identity, so previously acquired tickets fail closed.
    pub(crate) fn mint(&self, origin: ReportOrigin, evidence: E) -> Result<(), FenceError> {
        let mut entries = self.entries.lock().map_err(|_| FenceError::Poisoned)?;
        let entry = entries.get_mut(&origin.id).ok_or(FenceError::Missing)?;
        self.check(&origin, entry)?;
        entry.proof = Some(Proof {
            identity: Identity::new(),
            evidence,
        });
        Ok(())
    }

    /// Check runtime state and publish acquisition while invalidation is excluded.
    /// A rejected callback cannot create a ticket. No evidence clone is required.
    pub(crate) fn acquire<T>(
        &self,
        id: &str,
        validate_and_publish: impl FnOnce(&E) -> Option<T>,
    ) -> Result<(WaitTicket, T), FenceError> {
        let entries = self.entries.lock().map_err(|_| FenceError::Poisoned)?;
        let entry = entries.get(id).ok_or(FenceError::Missing)?;
        let generation = entry.generation.ok_or(FenceError::Exhausted)?;
        let proof = entry.proof.as_ref().ok_or(FenceError::NoProof)?;
        let value = validate_and_publish(&proof.evidence).ok_or(FenceError::Rejected)?;
        Ok((
            WaitTicket {
                origin: ReportOrigin {
                    process: self.process.clone(),
                    entry: entry.identity.clone(),
                    id: id.to_owned(),
                    generation,
                },
                proof: proof.identity.clone(),
            },
            value,
        ))
    }

    /// Revalidate and publish continuation/dispatch under the common gate.
    /// Tickets are fencing capabilities, not an exactly-once dispatch mechanism.
    pub(crate) fn admit<T>(
        &self,
        ticket: &WaitTicket,
        validate_and_publish: impl FnOnce(&E) -> Option<T>,
    ) -> Result<T, FenceError> {
        let entries = self.entries.lock().map_err(|_| FenceError::Poisoned)?;
        let entry = entries.get(&ticket.origin.id).ok_or(FenceError::Missing)?;
        self.check(&ticket.origin, entry)?;
        let proof = entry.proof.as_ref().ok_or(FenceError::NoProof)?;
        if !proof.identity.same(&ticket.proof) {
            return Err(FenceError::Stale);
        }
        validate_and_publish(&proof.evidence).ok_or(FenceError::Rejected)
    }

    /// Invalidate BEFORE attempting a synchronous write/publication. An operation
    /// returning Err never rolls back invalidation. Overflow clears proof and
    /// permanently fences the entry; the operation is not called in that case.
    /// Missing/poisoned entries also do not call it. Integration must preserve
    /// native health operations separately on these errors while denying proof;
    /// this primitive is not permission to skip a native mutation.
    pub(crate) fn invalidate<T>(
        &self,
        id: &str,
        operation: impl FnOnce() -> T,
    ) -> Result<T, FenceError> {
        let mut entries = self.entries.lock().map_err(|_| FenceError::Poisoned)?;
        let entry = entries.get_mut(id).ok_or(FenceError::Missing)?;
        entry.proof = None;
        entry.generation = entry.generation.and_then(|value| value.checked_add(1));
        entry.generation.ok_or(FenceError::Exhausted)?;
        Ok(operation())
    }

    /// Explicit lifecycle retirement is the only way to reclaim capacity. A later
    /// capture for the same ID receives a new, non-reusable entry incarnation.
    pub(crate) fn retire(&self, id: &str) -> Result<bool, FenceError> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| FenceError::Poisoned)?
            .remove(id)
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, TryLockError};

    // The tuple represents classifier-owned reset/scope/evidence; the fence
    // deliberately does not reinterpret it, compare wall time or inspect Account.
    type Evidence = (u64, &'static str, u64);

    fn proven(fence: &WaitProofs<Evidence>) -> WaitTicket {
        let origin = fence.capture("a").unwrap();
        fence.mint(origin, (100, "account", 7)).unwrap();
        fence.acquire("a", |_| Some(())).unwrap().0
    }

    #[test]
    fn r13_wait_proof_equal_walltime_release_fences_old_origin() {
        let fence = WaitProofs::new(2);
        let old = fence.capture("a").unwrap();
        let walltime = 100;
        fence.invalidate("a", || walltime).unwrap();
        assert_eq!(
            fence.mint(old, (walltime, "account", 7)),
            Err(FenceError::Stale)
        );
        assert!(matches!(
            fence.acquire("a", |_| Some(())),
            Err(FenceError::NoProof)
        ));
        proven(&fence);
    }

    #[test]
    fn r13_wait_proof_unknown_rejection_invalidates_same_tuple() {
        let fence = WaitProofs::new(1);
        let ticket = proven(&fence);
        let delayed = fence.capture("a").unwrap();
        fence.invalidate("a", || ()).unwrap();
        assert_eq!(fence.admit(&ticket, |_| Some(())), Err(FenceError::Stale));
        assert_eq!(
            fence.mint(delayed, (100, "account", 7)),
            Err(FenceError::Stale)
        );
    }

    #[test]
    fn r13_wait_proof_failed_write_keeps_invalidation() {
        let fence = WaitProofs::new(1);
        let ticket = proven(&fence);
        let old = fence.capture("a").unwrap();
        assert_eq!(
            fence.invalidate("a", || Err::<(), _>("write failed")),
            Ok(Err("write failed"))
        );
        assert_eq!(fence.admit(&ticket, |_| Some(())), Err(FenceError::Stale));
        assert_eq!(fence.mint(old, (100, "account", 7)), Err(FenceError::Stale));
    }

    #[test]
    fn r13_wait_proof_process_restart_rejects_old_origin_and_ticket() {
        let previous = WaitProofs::new(1);
        let ticket = proven(&previous);
        let old = previous.capture("a").unwrap();
        drop(previous);
        let restarted = WaitProofs::new(1);
        let current = restarted.capture("a").unwrap();
        assert!(matches!(
            restarted.acquire("a", |_| Some(())),
            Err(FenceError::NoProof)
        ));
        assert_eq!(
            restarted.mint(old, (100, "account", 7)),
            Err(FenceError::Stale)
        );
        restarted.mint(current, (100, "account", 7)).unwrap();
        assert_eq!(
            restarted.admit(&ticket, |_| Some(())),
            Err(FenceError::Stale)
        );
    }

    #[test]
    fn r13_wait_proof_retire_readd_rejects_old_incarnation() {
        let fence = WaitProofs::new(1);
        let ticket = proven(&fence);
        let old = fence.capture("a").unwrap();
        assert_eq!(fence.retire("a"), Ok(true));
        proven(&fence);
        assert_eq!(fence.mint(old, (100, "account", 7)), Err(FenceError::Stale));
        assert_eq!(fence.admit(&ticket, |_| Some(())), Err(FenceError::Stale));
    }

    #[test]
    fn r13_wait_proof_capacity_never_evicts_live_proof() {
        let fence = WaitProofs::new(1);
        let ticket = proven(&fence);
        assert!(matches!(fence.capture("b"), Err(FenceError::Capacity)));
        assert_eq!(fence.admit(&ticket, |e| Some(*e)), Ok((100, "account", 7)));
        fence.retire("a").unwrap();
        assert!(fence.capture("b").is_ok());
        assert!(matches!(
            WaitProofs::<()>::new(0).capture("a"),
            Err(FenceError::Capacity)
        ));
        assert!(matches!(fence.capture(""), Err(FenceError::InvalidId)));
        assert!(matches!(
            fence.capture(&"a".repeat(MAX_ID_BYTES + 1)),
            Err(FenceError::InvalidId)
        ));
    }

    #[test]
    fn r13_wait_proof_generation_overflow_fails_closed() {
        let fence = WaitProofs::new(1);
        proven(&fence);
        fence
            .entries
            .lock()
            .unwrap()
            .get_mut("a")
            .unwrap()
            .generation = Some(u64::MAX);
        let old = fence.capture("a").unwrap();
        assert_eq!(
            fence.invalidate("a", || -> () { panic!("must not publish") }),
            Err(FenceError::Exhausted)
        );
        assert_eq!(fence.mint(old, (100, "account", 7)), Err(FenceError::Stale));
        assert!(matches!(fence.capture("a"), Err(FenceError::Exhausted)));
        assert!(matches!(
            fence.acquire("a", |_| Some(())),
            Err(FenceError::Exhausted)
        ));
    }

    #[test]
    fn r13_wait_proof_replacement_and_runtime_validator_are_checked() {
        let fence = WaitProofs::new(1);
        let ticket = proven(&fence);
        assert_eq!(
            fence.admit(&ticket, |_| None::<()>),
            Err(FenceError::Rejected)
        );
        assert!(matches!(
            fence.acquire("a", |_| None::<()>),
            Err(FenceError::Rejected)
        ));
        proven(&fence);
        assert_eq!(
            fence.admit(&ticket, |_| -> Option<()> { panic!("stale callback") }),
            Err(FenceError::Stale)
        );
    }

    #[test]
    fn r13_wait_proof_check_and_admission_are_linearized_with_invalidation() {
        let fence = WaitProofs::new(1);
        let ticket = proven(&fence);
        let checked = Barrier::new(2);
        let publish = Barrier::new(2);
        let invalidate_started = Barrier::new(2);
        std::thread::scope(|scope| {
            let admission = scope.spawn(|| {
                fence.admit(&ticket, |_| {
                    checked.wait();
                    publish.wait();
                    Some("published before invalidation")
                })
            });
            checked.wait();
            let invalidation = scope.spawn(|| {
                invalidate_started.wait();
                fence.invalidate("a", || ())
            });
            invalidate_started.wait();
            assert!(matches!(
                fence.entries.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
            publish.wait();
            assert_eq!(
                admission.join().unwrap(),
                Ok("published before invalidation")
            );
            assert_eq!(invalidation.join().unwrap(), Ok(()));
        });
        assert_eq!(
            fence.admit(&ticket, |_| -> Option<()> {
                panic!("must not publish after invalidation")
            }),
            Err(FenceError::Stale)
        );
    }

    #[test]
    fn r13_wait_proof_pre_release_inflight_origin_stale_at_identical_walltime() {
        let fence = WaitProofs::new(1);
        let evidence = (100, "account", 7);
        let old = fence.capture("a").unwrap();
        fence.mint(old.clone(), evidence).unwrap();
        let old_ticket = fence.acquire("a", |_| Some(())).unwrap().0;
        let in_flight = Barrier::new(2);
        let release_committed = Barrier::new(2);
        std::thread::scope(|scope| {
            let report = scope.spawn(|| {
                // Origin was captured before this simulated operation begins.
                in_flight.wait();
                release_committed.wait();
                fence.mint(old, evidence)
            });
            in_flight.wait();
            // Release/application/report timestamps and evidence are identical.
            assert_eq!(fence.invalidate("a", || evidence.0), Ok(100));
            release_committed.wait();
            assert_eq!(report.join().unwrap(), Err(FenceError::Stale));
        });
        assert_eq!(
            fence.admit(&old_ticket, |_| Some(())),
            Err(FenceError::Stale)
        );
        let fresh = fence.capture("a").unwrap();
        fence.mint(fresh, evidence).unwrap();
        let new_ticket = fence.acquire("a", |_| Some(())).unwrap().0;
        assert_eq!(fence.admit(&new_ticket, |e| Some(*e)), Ok(evidence));
        assert_eq!(
            fence.admit(&old_ticket, |_| Some(())),
            Err(FenceError::Stale)
        );
    }

    #[test]
    fn r13_wait_proof_cloned_ws_origin_gets_independent_receipts_and_tickets() {
        let fence = WaitProofs::new(1);
        let origin = fence.capture("a").unwrap();
        let repeated = origin.clone();
        let evidence = (100, "account", 7);
        fence.mint(origin.clone(), evidence).unwrap();
        let first = fence.acquire("a", |_| Some(())).unwrap().0;
        // Merely cloning/capturing never refreshes or replaces a proof.
        let another_capture = fence.capture("a").unwrap();
        assert_eq!(fence.admit(&first, |e| Some(*e)), Ok(evidence));
        fence.mint(repeated.clone(), evidence).unwrap();
        let second = fence.acquire("a", |_| Some(())).unwrap().0;
        assert!(!first.proof.same(&second.proof));
        assert_eq!(
            fence.admit(&first, |_| panic!("old receipt callback")),
            Err::<(), _>(FenceError::Stale)
        );
        assert_eq!(fence.admit(&second, |e| Some(*e)), Ok(evidence));
        // Even another clone of the exact same pre-network operation yields
        // a new receipt. No pointer/evidence/timestamp equality grants reuse.
        fence.mint(origin, evidence).unwrap();
        let third = fence.acquire("a", |_| Some(())).unwrap().0;
        assert!(!second.proof.same(&third.proof));
        assert_eq!(fence.admit(&second, |_| Some(())), Err(FenceError::Stale));
        assert_eq!(fence.admit(&third, |e| Some(*e)), Ok(evidence));
        fence.invalidate("a", || ()).unwrap();
        assert_eq!(fence.mint(repeated, evidence), Err(FenceError::Stale));
        assert_eq!(
            fence.mint(another_capture, evidence),
            Err(FenceError::Stale)
        );
    }

    #[test]
    fn r13_wait_proof_other_runtime_cannot_replace_current_same_account_proof() {
        let first_runtime = WaitProofs::new(1);
        let other_runtime = WaitProofs::new(1);
        let first_ticket = proven(&first_runtime);
        let other_ticket = proven(&other_runtime);
        let foreign = first_runtime.capture("a").unwrap();
        assert_eq!(
            other_runtime.mint(foreign, (100, "account", 7)),
            Err(FenceError::Stale)
        );
        assert_eq!(
            other_runtime.admit(&first_ticket, |_| Some(())),
            Err(FenceError::Stale)
        );
        assert_eq!(
            first_runtime.admit(&other_ticket, |_| Some(())),
            Err(FenceError::Stale)
        );
        // A rejected cross-owner mint cannot replace the rightful receipt.
        assert_eq!(
            other_runtime.admit(&other_ticket, |e| Some(*e)),
            Ok((100, "account", 7))
        );
    }

    #[test]
    fn r13_wait_proof_last_generation_then_exhaustion_never_wraps_or_publishes() {
        let fence = WaitProofs::new(1);
        fence.capture("a").unwrap();
        fence
            .entries
            .lock()
            .unwrap()
            .get_mut("a")
            .unwrap()
            .generation = Some(u64::MAX - 1);
        let prior = fence.capture("a").unwrap();
        assert_eq!(
            fence.invalidate("a", || "last generation"),
            Ok("last generation")
        );
        assert_eq!(
            fence.mint(prior, (100, "account", 7)),
            Err(FenceError::Stale)
        );
        let last = fence.capture("a").unwrap();
        assert_eq!(last.generation, u64::MAX);
        fence.mint(last.clone(), (100, "account", 7)).unwrap();
        let ticket = fence.acquire("a", |_| Some(())).unwrap().0;
        assert_eq!(
            fence.invalidate("a", || panic!("overflow must not publish")),
            Err::<(), _>(FenceError::Exhausted)
        );
        assert!(fence.entries.lock().unwrap()["a"].proof.is_none());
        assert!(matches!(fence.capture("a"), Err(FenceError::Exhausted)));
        assert_eq!(
            fence.mint(last.clone(), (100, "account", 7)),
            Err(FenceError::Stale)
        );
        assert_eq!(fence.admit(&ticket, |_| Some(())), Err(FenceError::Stale));
        assert_eq!(fence.invalidate("a", || ()), Err(FenceError::Exhausted));
        // Lifecycle retirement restores capacity, never the old incarnation.
        fence.retire("a").unwrap();
        proven(&fence);
        assert_eq!(
            fence.mint(last, (100, "account", 7)),
            Err(FenceError::Stale)
        );
    }

    #[test]
    fn r13_wait_proof_capacity_and_missing_errors_do_not_mutate_other_proof() {
        let fence = WaitProofs::<Evidence>::new(usize::MAX);
        assert_eq!(fence.capacity, MAX_ENTRIES);
        let tiny = WaitProofs::new(1);
        let ticket = proven(&tiny);
        assert_eq!(
            tiny.invalidate("absent", || panic!("missing must not publish")),
            Err::<(), _>(FenceError::Missing)
        );
        assert!(matches!(tiny.capture("b"), Err(FenceError::Capacity)));
        assert_eq!(tiny.admit(&ticket, |e| Some(*e)), Ok((100, "account", 7)));
        assert_eq!(tiny.retire("absent"), Ok(false));
        assert_eq!(tiny.retire("a"), Ok(true));
        let next = tiny.capture("b").unwrap();
        tiny.mint(next, (100, "account", 7)).unwrap();
        assert_eq!(tiny.admit(&ticket, |_| Some(())), Err(FenceError::Missing));
    }

    #[test]
    fn r13_wait_proof_panicking_publication_poison_fails_closed() {
        let fence = WaitProofs::new(1);
        let ticket = proven(&fence);
        let pending = fence.capture("a").unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fence.invalidate("a", || panic!("synthetic publication panic"));
        }));
        assert!(panic.is_err());
        assert!(matches!(fence.capture("a"), Err(FenceError::Poisoned)));
        assert_eq!(
            fence.mint(pending, (100, "account", 7)),
            Err(FenceError::Poisoned)
        );
        assert_eq!(
            fence.admit(&ticket, |_| Some(())),
            Err(FenceError::Poisoned)
        );
        assert!(matches!(
            fence.acquire("a", |_| Some(())),
            Err(FenceError::Poisoned)
        ));
    }

    #[test]
    fn r13_wait_proof_oversized_utf8_identifier_does_not_consume_capacity() {
        let fence = WaitProofs::<Evidence>::new(1);
        // The identifier bound is bytes, not Unicode scalar count.
        let exact = "\u{00e9}".repeat(MAX_ID_BYTES / 2);
        let too_long = format!("{exact}x");
        assert!(matches!(fence.capture(""), Err(FenceError::InvalidId)));
        assert!(matches!(
            fence.capture(&too_long),
            Err(FenceError::InvalidId)
        ));
        assert!(fence.entries.lock().unwrap().is_empty());
        let origin = fence.capture(&exact).unwrap();
        fence.mint(origin, (100, "account", 7)).unwrap();
        assert!(matches!(fence.capture("b"), Err(FenceError::Capacity)));
        let ticket = fence.acquire(&exact, |_| Some(())).unwrap().0;
        assert_eq!(fence.admit(&ticket, |e| Some(*e)), Ok((100, "account", 7)));
    }

    #[test]
    fn r13_wait_proof_zero_capacity_never_creates_authority() {
        let fence = WaitProofs::<Evidence>::new(0);
        assert!(matches!(fence.capture("a"), Err(FenceError::Capacity)));
        assert!(matches!(
            fence.acquire("a", |_| Some(())),
            Err(FenceError::Missing)
        ));
        assert_eq!(fence.retire("a"), Ok(false));
        assert!(fence.entries.lock().unwrap().is_empty());
    }
}
