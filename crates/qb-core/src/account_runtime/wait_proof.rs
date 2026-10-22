//! Process-local causal report provenance; native wait admission is not activated.
//!
//! The runtime MUST serialize capture/accept/invalidation/native persistence/final
//! publication with its existing outer account transaction. No registry lock is
//! held across native work, callbacks or awaits. This registry cannot enforce that
//! external order, raw-report classification, caller memory bounds or actual send.
//! Restart is cold; nothing here is serializable or constructible from wall time.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

const MAX_ENTRIES: usize = 4096;
const MAX_ID_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FenceError {
    InvalidId,
    Capacity,
    Missing,
    Stale,
    #[cfg(test)]
    NoProof,
    Rejected,
    Exhausted,
    Poisoned,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReportSource {
    HttpAttempt,
    UsagePoll,
}

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

/// Allocate BEFORE a physical send. Clone preserves the same ordinal and epoch;
/// it cannot recapture an old report or admit it twice. HTTP/poll share one clock.
#[derive(Clone, Debug)]
pub(crate) struct ReportOrigin {
    process: Identity,
    entry: Identity,
    id: String,
    epoch: u64,
    ordinal: u64,
    source: ReportSource,
}

/// Single accepted raw report, consumed by publication or its own invalidation.
/// Dropping cancels publication, but NEVER rewinds the accepted watermark.
#[derive(Debug)]
pub(crate) struct AcceptPermit {
    origin: ReportOrigin,
    epoch: u64,
    acceptance: Identity,
    phase: Phase,
}

/// Finite owner protocol, not evidence of successful native persistence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Fresh,
    Applied,
    HttpFinalPending,
    HttpFinalApplied,
}

/// Unpublished header continuation. Only an own-rotated HTTP acceptance can
/// create it. Dropping cancels final publication; no Clone or public fields.
#[derive(Debug)]
pub(crate) struct SealedHttpReport {
    permit: AcceptPermit,
}

/// Fences a proof, not exactly-once dispatch or an unrevocable native permission.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct WaitTicket {
    process: Identity,
    entry: Identity,
    id: String,
    epoch: u64,
    proof: Identity,
}

struct Proof<E> {
    #[cfg(test)]
    identity: Identity,
    _evidence: Arc<E>,
}
struct Entry<E> {
    identity: Identity,
    epoch: Option<u64>,
    issued: u64,
    accepted: u64,
    active: Option<Identity>,
    proof: Option<Proof<E>>,
}

struct State<E> {
    closed: bool,
    entries: BTreeMap<String, Entry<E>>,
}

pub(crate) struct WaitProofs<E> {
    process: Identity,
    capacity: usize,
    state: Mutex<State<E>>,
}

impl<E> WaitProofs<E> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            process: Identity::new(),
            capacity: capacity.min(MAX_ENTRIES),
            state: Mutex::new(State {
                closed: false,
                entries: BTreeMap::new(),
            }),
        }
    }

    fn open(&self) -> Result<MutexGuard<'_, State<E>>, FenceError> {
        let state = self.state.lock().map_err(|_| FenceError::Poisoned)?;
        if state.closed {
            return Err(FenceError::Closed);
        }
        Ok(state)
    }

    /// Sticky process-local closure. Caller still holds the outer account gate
    /// through native Stop; this mutex does not serialize external publication.
    /// Previously returned evidence remains data, never admission authority.
    pub(crate) fn close(&self) -> Result<(), FenceError> {
        let retired = {
            let mut state = self.state.lock().map_err(|_| FenceError::Poisoned)?;
            state.closed = true;
            std::mem::take(&mut state.entries)
        };
        // Arbitrary evidence destructors must never run under the registry lock.
        drop(retired);
        Ok(())
    }

    pub(crate) fn capture(
        &self,
        id: &str,
        source: ReportSource,
    ) -> Result<ReportOrigin, FenceError> {
        if id.is_empty() || id.len() > MAX_ID_BYTES {
            return Err(FenceError::InvalidId);
        }
        // Retire payloads outside the mutex, including their arbitrary E::drop.
        let mut retired = None;
        let result = {
            let mut state = self.open()?;
            let entries = &mut state.entries;
            if !entries.contains_key(id) {
                if entries.len() >= self.capacity {
                    return Err(FenceError::Capacity);
                }
                entries.insert(
                    id.to_owned(),
                    Entry {
                        identity: Identity::new(),
                        epoch: Some(0),
                        issued: 0,
                        accepted: 0,
                        active: None,
                        proof: None,
                    },
                );
            }
            let entry = entries.get_mut(id).ok_or(FenceError::Missing)?;
            let epoch = entry.epoch.ok_or(FenceError::Exhausted)?;
            if let Some(ordinal) = entry.issued.checked_add(1) {
                entry.issued = ordinal;
                Ok(ReportOrigin {
                    process: self.process.clone(),
                    entry: entry.identity.clone(),
                    id: id.to_owned(),
                    epoch,
                    ordinal,
                    source,
                })
            } else {
                entry.epoch = None;
                entry.active = None;
                retired = entry.proof.take();
                Err(FenceError::Exhausted)
            }
        };
        drop(retired);
        result
    }

    fn check_origin(&self, origin: &ReportOrigin, entry: &Entry<E>) -> Result<(), FenceError> {
        if !self.process.same(&origin.process)
            || !entry.identity.same(&origin.entry)
            || entry.epoch != Some(origin.epoch)
            || origin.ordinal == 0
            || origin.ordinal > entry.issued
        {
            return Err(FenceError::Stale);
        }
        Ok(())
    }

    fn check_permit(&self, permit: &AcceptPermit, entry: &Entry<E>) -> Result<(), FenceError> {
        if !self.process.same(&permit.origin.process)
            || !entry.identity.same(&permit.origin.entry)
            || entry.epoch != Some(permit.epoch)
            || entry.accepted != permit.origin.ordinal
            || !entry
                .active
                .as_ref()
                .is_some_and(|active| active.same(&permit.acceptance))
        {
            return Err(FenceError::Stale);
        }
        Ok(())
    }

    /// Accept BEFORE classification, including partial/nonqualifying reports.
    /// Newer accepted reports revoke the old proof and every older pending permit.
    /// Missing raw origin is never accepted by a receive-time recapture fallback.
    pub(crate) fn accept(
        &self,
        id: &str,
        origin: ReportOrigin,
    ) -> Result<AcceptPermit, FenceError> {
        if id != origin.id {
            return Err(FenceError::Stale);
        }
        let retired;
        let permit = {
            let mut state = self.open()?;
            let entries = &mut state.entries;
            let entry = entries.get_mut(id).ok_or(FenceError::Missing)?;
            self.check_origin(&origin, entry)?;
            if origin.ordinal <= entry.accepted {
                return Err(FenceError::Stale);
            }
            let acceptance = Identity::new();
            entry.accepted = origin.ordinal;
            retired = entry.proof.take();
            entry.active = Some(acceptance.clone());
            AcceptPermit {
                epoch: origin.epoch,
                origin,
                acceptance,
                phase: Phase::Fresh,
            }
        };
        drop(retired);
        Ok(permit)
    }

    /// Rotate only THIS accepted report across its own required invalidation.
    /// Call before native mutation. Other captured origins retain the old epoch.
    /// External release/rejection uses invalidate(), which revokes this permit.
    /// Generic reports rotate once. A sealed HTTP continuation gets exactly one
    /// final rotation; this never bypasses independent invalidations.
    pub(crate) fn invalidate_accepted(
        &self,
        mut permit: AcceptPermit,
    ) -> Result<AcceptPermit, FenceError> {
        let retired;
        let result = {
            let mut state = self.open()?;
            let entries = &mut state.entries;
            let entry = entries
                .get_mut(&permit.origin.id)
                .ok_or(FenceError::Missing)?;
            self.check_permit(&permit, entry)?;
            let next_phase = match permit.phase {
                Phase::Fresh => Phase::Applied,
                Phase::HttpFinalPending => Phase::HttpFinalApplied,
                Phase::Applied | Phase::HttpFinalApplied => return Err(FenceError::Rejected),
            };
            retired = entry.proof.take();
            entry.epoch = permit.epoch.checked_add(1);
            if let Some(epoch) = entry.epoch {
                permit.epoch = epoch;
                permit.phase = next_phase;
                Ok(permit)
            } else {
                entry.active = None;
                Err(FenceError::Exhausted)
            }
        };
        drop(retired);
        result
    }

    /// Call only after the header's native persistence/closure succeeded. The
    /// registry checks provenance and phase, not persistence or raw classification.
    pub(crate) fn seal_http(&self, permit: AcceptPermit) -> Result<SealedHttpReport, FenceError> {
        let state = self.open()?;
        let entry = state
            .entries
            .get(&permit.origin.id)
            .ok_or(FenceError::Missing)?;
        self.check_permit(&permit, entry)?;
        if permit.phase != Phase::Applied || permit.origin.source != ReportSource::HttpAttempt {
            return Err(FenceError::Rejected);
        }
        Ok(SealedHttpReport { permit })
    }

    /// Consume the header continuation once under the final outer transaction.
    /// Caller MUST call invalidate_accepted even if there is no body signal;
    /// HttpFinalPending cannot mint. No receive-time origin is captured here.
    pub(crate) fn continue_http(
        &self,
        sealed: SealedHttpReport,
    ) -> Result<AcceptPermit, FenceError> {
        let mut permit = sealed.permit;
        let state = self.open()?;
        let entry = state
            .entries
            .get(&permit.origin.id)
            .ok_or(FenceError::Missing)?;
        self.check_permit(&permit, entry)?;
        if permit.phase != Phase::Applied || permit.origin.source != ReportSource::HttpAttempt {
            return Err(FenceError::Rejected);
        }
        permit.phase = Phase::HttpFinalPending;
        Ok(permit)
    }

    /// Publish compact classified evidence only AFTER successful native persistence,
    /// while the external transaction is still held. No native callback is invoked.
    pub(crate) fn mint(&self, permit: AcceptPermit, evidence: E) -> Result<(), FenceError> {
        // Declared before the lock so E is also dropped unlocked on rejection.
        let proof = Proof {
            #[cfg(test)]
            identity: Identity::new(),
            _evidence: Arc::new(evidence),
        };
        let retired;
        {
            let mut state = self.open()?;
            let entries = &mut state.entries;
            let entry = entries
                .get_mut(&permit.origin.id)
                .ok_or(FenceError::Missing)?;
            self.check_permit(&permit, entry)?;
            if permit.phase == Phase::HttpFinalPending {
                return Err(FenceError::Rejected);
            }
            entry.active = None;
            retired = entry.proof.replace(proof);
        }
        drop(retired);
        Ok(())
    }

    /// Revoke before native publication; ALWAYS execute native release/rejection
    /// separately even if this returns Missing/Exhausted/Poisoned. No callback API.
    pub(crate) fn invalidate(&self, id: &str) -> Result<(), FenceError> {
        let retired;
        let result = {
            let mut state = self.open()?;
            let entries = &mut state.entries;
            let entry = entries.get_mut(id).ok_or(FenceError::Missing)?;
            retired = entry.proof.take();
            entry.active = None;
            entry.epoch = entry.epoch.and_then(|epoch| epoch.checked_add(1));
            entry.epoch.map(|_| ()).ok_or(FenceError::Exhausted)
        };
        drop(retired);
        result
    }

    /// The returned immutable evidence can outlive invalidation; it is DATA, not
    /// authority. Hold the outer transaction through eligibility and final check.
    #[cfg(test)]
    pub(crate) fn acquire(&self, id: &str) -> Result<(WaitTicket, Arc<E>), FenceError> {
        let state = self.open()?;
        let entries = &state.entries;
        let entry = entries.get(id).ok_or(FenceError::Missing)?;
        let epoch = entry.epoch.ok_or(FenceError::Exhausted)?;
        let proof = entry.proof.as_ref().ok_or(FenceError::NoProof)?;
        Ok((
            WaitTicket {
                process: self.process.clone(),
                entry: entry.identity.clone(),
                id: id.to_owned(),
                epoch,
                proof: proof.identity.clone(),
            },
            Arc::clone(&proof._evidence),
        ))
    }

    #[cfg(test)]
    pub(crate) fn admit(&self, ticket: &WaitTicket) -> Result<Arc<E>, FenceError> {
        let state = self.open()?;
        let entries = &state.entries;
        let entry = entries.get(&ticket.id).ok_or(FenceError::Missing)?;
        if !self.process.same(&ticket.process)
            || !entry.identity.same(&ticket.entry)
            || entry.epoch != Some(ticket.epoch)
        {
            return Err(FenceError::Stale);
        }
        let proof = entry.proof.as_ref().ok_or(FenceError::NoProof)?;
        if !proof.identity.same(&ticket.proof) {
            return Err(FenceError::Stale);
        }
        Ok(Arc::clone(&proof._evidence))
    }

    pub(crate) fn retire(&self, id: &str) -> Result<bool, FenceError> {
        let removed = { self.open()?.entries.remove(id) };
        let existed = removed.is_some();
        drop(removed);
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Weak,
        atomic::{AtomicUsize, Ordering},
    };

    type Evidence = (u64, &'static str);
    fn proven(fence: &WaitProofs<Evidence>) -> WaitTicket {
        let origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let permit = fence.accept("a", origin).unwrap();
        fence.mint(permit, (100, "account")).unwrap();
        fence.acquire("a").unwrap().0
    }

    #[test]
    fn n5a_origin_http_poll_share_checked_pre_send_order() {
        let fence = WaitProofs::<Evidence>::new(1);
        let first = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        let second = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let retry = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        assert_eq!((first.ordinal, second.ordinal, retry.ordinal), (1, 2, 3));
        assert_eq!(first.source, ReportSource::HttpAttempt);
        assert_eq!(second.source, ReportSource::UsagePoll);
        assert_eq!(first.clone().ordinal, first.ordinal);
        assert!(matches!(fence.acquire("a"), Err(FenceError::NoProof)));
    }

    #[test]
    fn n5a_origin_newer_partial_blocks_older_full_and_revokes_previous_proof() {
        let fence = WaitProofs::new(1);
        let prior = proven(&fence);
        let a = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        let b = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let partial = fence.accept("a", b).unwrap();
        drop(partial); // Native partial classification returns no compact proof.
        assert!(matches!(fence.accept("a", a), Err(FenceError::Stale)));
        assert!(matches!(fence.acquire("a"), Err(FenceError::NoProof)));
        assert!(fence.admit(&prior).is_err());
    }

    #[test]
    fn n5a_origin_newer_full_survives_older_completion() {
        let fence = WaitProofs::new(1);
        let a = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        let b = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let permit = fence.accept("a", b).unwrap();
        fence.mint(permit, (100, "account")).unwrap();
        let ticket = fence.acquire("a").unwrap().0;
        assert!(matches!(fence.accept("a", a), Err(FenceError::Stale)));
        assert_eq!(*fence.admit(&ticket).unwrap(), (100, "account"));
    }

    #[test]
    fn n5a_origin_newer_partial_revokes_an_older_unpublished_permit() {
        let fence = WaitProofs::new(1);
        let a = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        let b = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let earlier = fence.accept("a", a).unwrap();
        drop(fence.accept("a", b).unwrap());
        assert_eq!(
            fence.mint(earlier, (100, "account")),
            Err(FenceError::Stale)
        );
        assert!(matches!(fence.acquire("a"), Err(FenceError::NoProof)));
    }

    #[test]
    fn n5a_origin_permit_crosses_only_its_own_epoch_without_recapture() {
        let fence = WaitProofs::new(1);
        let previous = proven(&fence);
        let unrelated = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        let introducing = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let duplicate = introducing.clone();
        let ordinal = introducing.ordinal;
        let epoch = introducing.epoch;
        let permit = fence.accept("a", introducing).unwrap();
        let permit = fence.invalidate_accepted(permit).unwrap();
        assert_eq!(permit.origin.ordinal, ordinal);
        assert_eq!(permit.origin.epoch, epoch);
        assert_eq!(permit.epoch, epoch + 1);
        fence.mint(permit, (100, "account")).unwrap();
        assert!(matches!(
            fence.accept("a", unrelated),
            Err(FenceError::Stale)
        ));
        assert!(matches!(
            fence.accept("a", duplicate),
            Err(FenceError::Stale)
        ));
        assert_eq!(fence.admit(&previous).unwrap_err(), FenceError::Stale);
        assert_eq!(fence.state.lock().unwrap().entries["a"].issued, ordinal);
        let ticket = fence.acquire("a").unwrap().0;
        assert_eq!(*fence.admit(&ticket).unwrap(), (100, "account"));
    }

    #[test]
    fn n5a_origin_external_release_after_own_rotation_revokes_permit() {
        let fence = WaitProofs::new(1);
        let origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let permit = fence.accept("a", origin).unwrap();
        let permit = fence.invalidate_accepted(permit).unwrap();
        fence.invalidate("a").unwrap();
        assert_eq!(fence.mint(permit, (100, "account")), Err(FenceError::Stale));
        assert!(matches!(fence.acquire("a"), Err(FenceError::NoProof)));
        proven(&fence); // A genuinely new post-release send is the positive control.
    }

    #[test]
    fn n5a_origin_cancelled_failed_write_and_duplicate_do_not_rewind_watermark() {
        let fence = WaitProofs::<Evidence>::new(1);
        let origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let repeated = origin.clone();
        let permit = fence.accept("a", origin).unwrap();
        let permit = fence.invalidate_accepted(permit).unwrap();
        drop(permit); // Native persistence failed; publication is skipped.
        assert!(matches!(
            fence.accept("a", repeated),
            Err(FenceError::Stale)
        ));
        assert!(matches!(fence.acquire("a"), Err(FenceError::NoProof)));
        assert_eq!(fence.state.lock().unwrap().entries["a"].accepted, 1);
    }

    #[test]
    fn n5a_origin_equal_walltime_never_resurrects_pre_release_report() {
        let fence = WaitProofs::new(1);
        let old = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        let walltime_and_payload = (100, "account");
        fence.invalidate("a").unwrap();
        assert!(matches!(fence.accept("a", old), Err(FenceError::Stale)));
        let fresh = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        let permit = fence.accept("a", fresh).unwrap();
        fence.mint(permit, walltime_and_payload).unwrap();
        assert_eq!(*fence.acquire("a").unwrap().1, walltime_and_payload);
    }

    #[test]
    fn n5a_origin_restart_retire_and_wrong_account_are_fail_closed() {
        let old = WaitProofs::new(1);
        let old_ticket = proven(&old);
        let old_origin = old.capture("a", ReportSource::UsagePoll).unwrap();
        let foreign = old.capture("a", ReportSource::UsagePoll).unwrap();
        let current = WaitProofs::new(1);
        let current_ticket = proven(&current);
        assert!(matches!(
            current.accept("a", foreign),
            Err(FenceError::Stale)
        ));
        assert_eq!(*current.admit(&current_ticket).unwrap(), (100, "account"));
        assert_eq!(current.admit(&old_ticket).unwrap_err(), FenceError::Stale);
        old.retire("a").unwrap();
        proven(&old);
        assert!(matches!(
            old.accept("a", old_origin),
            Err(FenceError::Stale)
        ));
        assert_eq!(old.admit(&old_ticket).unwrap_err(), FenceError::Stale);
        let origin = current.capture("a", ReportSource::HttpAttempt).unwrap();
        assert!(matches!(
            current.accept("b", origin.clone()),
            Err(FenceError::Stale)
        ));
        assert!(current.accept("a", origin).is_ok());
    }

    #[test]
    fn n5a_origin_counter_exhaustion_revokes_without_wrap_or_eviction() {
        let fence = WaitProofs::new(1);
        fence.capture("a", ReportSource::UsagePoll).unwrap();
        fence
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut("a")
            .unwrap()
            .issued = u64::MAX - 1;
        let last = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        assert_eq!(last.ordinal, u64::MAX);
        let permit = fence.accept("a", last).unwrap();
        fence.mint(permit, (100, "account")).unwrap();
        let ticket = fence.acquire("a").unwrap().0;
        assert!(matches!(
            fence.capture("a", ReportSource::UsagePoll),
            Err(FenceError::Exhausted)
        ));
        assert_eq!(fence.admit(&ticket).unwrap_err(), FenceError::Stale);
        assert!(matches!(fence.acquire("a"), Err(FenceError::Exhausted)));
        assert!(matches!(
            fence.capture("b", ReportSource::UsagePoll),
            Err(FenceError::Capacity)
        ));
        fence.retire("a").unwrap();
        let first_new_incarnation = fence.capture("a", ReportSource::UsagePoll).unwrap();
        assert_eq!(first_new_incarnation.ordinal, 1);
    }

    #[test]
    fn n5a_origin_epoch_exhaustion_and_second_own_rotation_deny_authority() {
        let fence = WaitProofs::<Evidence>::new(1);
        fence.capture("a", ReportSource::UsagePoll).unwrap();
        fence
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut("a")
            .unwrap()
            .epoch = Some(u64::MAX);
        let last = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let permit = fence.accept("a", last).unwrap();
        assert!(matches!(
            fence.invalidate_accepted(permit),
            Err(FenceError::Exhausted)
        ));
        assert!(matches!(fence.acquire("a"), Err(FenceError::Exhausted)));
        fence.retire("a").unwrap();
        let origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let permit = fence.accept("a", origin).unwrap();
        let permit = fence.invalidate_accepted(permit).unwrap();
        assert!(matches!(
            fence.invalidate_accepted(permit),
            Err(FenceError::Rejected)
        ));
        assert!(matches!(fence.acquire("a"), Err(FenceError::NoProof)));
    }

    #[test]
    fn n5a_origin_bounds_missing_and_poison_never_create_authority() {
        let fence = WaitProofs::<Evidence>::new(usize::MAX);
        assert_eq!(fence.capacity, MAX_ENTRIES);
        for bad in [String::new(), "\u{00e9}".repeat(MAX_ID_BYTES / 2 + 1)] {
            assert!(matches!(
                fence.capture(&bad, ReportSource::UsagePoll),
                Err(FenceError::InvalidId)
            ));
        }
        assert!(fence.state.lock().unwrap().entries.is_empty());
        assert_eq!(fence.invalidate("missing"), Err(FenceError::Missing));
        assert_eq!(fence.retire("missing"), Ok(false));
        assert!(matches!(
            WaitProofs::<Evidence>::new(0).capture("a", ReportSource::UsagePoll),
            Err(FenceError::Capacity)
        ));
        let ticket = proven(&fence);
        let origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = fence.state.lock().unwrap();
            panic!("synthetic registry poison");
        }));
        assert!(panic.is_err());
        assert!(matches!(
            fence.accept("a", origin),
            Err(FenceError::Poisoned)
        ));
        assert_eq!(fence.admit(&ticket).unwrap_err(), FenceError::Poisoned);
        assert_eq!(fence.invalidate("a"), Err(FenceError::Poisoned));
    }

    struct DropProbe {
        owner: Weak<WaitProofs<DropProbe>>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for DropProbe {
        fn drop(&mut self) {
            if let Some(owner) = self.owner.upgrade() {
                assert!(
                    !matches!(
                        owner.state.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ),
                    "payload drop held registry mutex"
                );
            }
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn publish_probe(fence: &Arc<WaitProofs<DropProbe>>, drops: &Arc<AtomicUsize>) {
        let origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let permit = fence.accept("a", origin).unwrap();
        fence
            .mint(
                permit,
                DropProbe {
                    owner: Arc::downgrade(fence),
                    drops: Arc::clone(drops),
                },
            )
            .unwrap();
    }

    #[test]
    fn n5a_origin_payload_destruction_never_runs_under_registry_mutex() {
        let fence = Arc::new(WaitProofs::new(1));
        let drops = Arc::new(AtomicUsize::new(0));
        publish_probe(&fence, &drops);
        fence.invalidate("a").unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        publish_probe(&fence, &drops);
        let next = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        drop(fence.accept("a", next).unwrap());
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        publish_probe(&fence, &drops);
        fence.retire("a").unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        publish_probe(&fence, &drops);
        fence
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut("a")
            .unwrap()
            .issued = u64::MAX;
        assert!(matches!(
            fence.capture("a", ReportSource::UsagePoll),
            Err(FenceError::Exhausted)
        ));
        assert_eq!(drops.load(Ordering::SeqCst), 4);
        fence.retire("a").unwrap();
        let origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let permit = fence.accept("a", origin).unwrap();
        fence.invalidate("a").unwrap();
        assert_eq!(
            fence.mint(
                permit,
                DropProbe {
                    owner: Arc::downgrade(&fence),
                    drops: Arc::clone(&drops)
                }
            ),
            Err(FenceError::Stale)
        );
        assert_eq!(drops.load(Ordering::SeqCst), 5);
    }
    #[test]
    fn n5a_origin_close_is_sticky_for_origins_tickets_and_permits() {
        let fence = WaitProofs::new(3);
        let ticket = proven(&fence);
        let evidence = fence.admit(&ticket).unwrap();
        let origin = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        let b = fence.capture("b", ReportSource::UsagePoll).unwrap();
        let mint = fence.accept("b", b).unwrap();
        let c = fence.capture("c", ReportSource::UsagePoll).unwrap();
        let rotate = fence.accept("c", c).unwrap();
        fence.close().unwrap();
        assert!(fence.state.lock().unwrap().entries.is_empty());
        assert_eq!(*evidence, (100, "account"));
        assert_eq!(fence.admit(&ticket).unwrap_err(), FenceError::Closed);
        assert_eq!(fence.accept("a", origin).unwrap_err(), FenceError::Closed);
        assert_eq!(fence.mint(mint, (100, "account")), Err(FenceError::Closed));
        assert_eq!(
            fence.invalidate_accepted(rotate).unwrap_err(),
            FenceError::Closed
        );
        assert_eq!(fence.acquire("a").unwrap_err(), FenceError::Closed);
        assert_eq!(fence.invalidate("a"), Err(FenceError::Closed));
        assert_eq!(fence.retire("a"), Err(FenceError::Closed));
        fence.close().unwrap();
        assert_eq!(
            fence.capture("fresh", ReportSource::UsagePoll).unwrap_err(),
            FenceError::Closed
        );
    }

    #[test]
    fn n5a_origin_close_drops_evidence_outside_mutex() {
        let fence = Arc::new(WaitProofs::new(1));
        let drops = Arc::new(AtomicUsize::new(0));
        publish_probe(&fence, &drops);
        fence.close().unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        fence.close().unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn n5a_origin_unknown_rejection_and_failed_write_keep_old_authority_revoked() {
        for failed_write in [false, true] {
            let fence = WaitProofs::new(1);
            let ticket = proven(&fence);
            let origin = fence.capture("a", ReportSource::HttpAttempt).unwrap();
            let same_payload = (100, "account");
            fence.invalidate("a").unwrap();
            // Either an unknown quota rejection or a native failure AFTER
            // pre-invalidation. Neither can restore the old report or ticket.
            let native_result: Result<(), ()> = if failed_write { Err(()) } else { Ok(()) };
            assert_eq!(native_result.is_err(), failed_write);
            assert_eq!(fence.accept("a", origin).unwrap_err(), FenceError::Stale);
            assert_eq!(fence.admit(&ticket).unwrap_err(), FenceError::Stale);
            assert_eq!(fence.acquire("a").unwrap_err(), FenceError::NoProof);
            let fresh = fence.capture("a", ReportSource::HttpAttempt).unwrap();
            let permit = fence.accept("a", fresh).unwrap();
            fence.mint(permit, same_payload).unwrap();
            assert_eq!(*fence.acquire("a").unwrap().1, same_payload);
        }
    }

    #[test]
    fn n5a_origin_capacity_and_missing_operations_preserve_live_resident() {
        let fence = WaitProofs::new(1);
        let resident = proven(&fence);
        assert_eq!(
            fence.capture("b", ReportSource::UsagePoll).unwrap_err(),
            FenceError::Capacity
        );
        assert_eq!(fence.invalidate("missing"), Err(FenceError::Missing));
        assert_eq!(fence.retire("missing"), Ok(false));
        assert_eq!(*fence.admit(&resident).unwrap(), (100, "account"));
        assert_eq!(fence.state.lock().unwrap().entries.len(), 1);
        assert_eq!(fence.retire("a"), Ok(true));
        let b = fence.capture("b", ReportSource::UsagePoll).unwrap();
        let permit = fence.accept("b", b).unwrap();
        fence.mint(permit, (100, "account")).unwrap();
        assert!(fence.admit(&resident).is_err());
        assert!(fence.acquire("b").is_ok());
    }

    #[test]
    fn n5a_origin_equal_evidence_replacement_changes_ticket_without_epoch_rotation() {
        let fence = WaitProofs::new(1);
        let prior = proven(&fence);
        let origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        assert!(fence.admit(&prior).is_ok()); // Capture alone preserves proof.
        let duplicate_before = origin.clone();
        let duplicate_after = origin.clone();
        let permit = fence.accept("a", origin).unwrap();
        assert_eq!(
            fence.accept("a", duplicate_before).unwrap_err(),
            FenceError::Stale
        );
        fence.mint(permit, (100, "account")).unwrap();
        let current = fence.acquire("a").unwrap().0;
        assert_eq!(prior.epoch, current.epoch);
        assert_eq!(
            fence.accept("a", duplicate_after).unwrap_err(),
            FenceError::Stale
        );
        assert_eq!(fence.admit(&prior).unwrap_err(), FenceError::Stale);
        assert_eq!(*fence.admit(&current).unwrap(), (100, "account"));
        let next = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        assert!(fence.admit(&current).is_ok());
        let permit = fence.accept("a", next).unwrap();
        fence.mint(permit, (100, "account")).unwrap();
        assert_eq!(fence.admit(&current).unwrap_err(), FenceError::Stale);
        assert!(fence.acquire("a").is_ok());
    }

    #[test]
    fn n5a_origin_foreign_accepted_permits_cannot_rotate_or_mint() {
        let rightful = WaitProofs::new(1);
        let ticket = proven(&rightful);
        let foreign = WaitProofs::<Evidence>::new(1);
        let origin = foreign.capture("a", ReportSource::UsagePoll).unwrap();
        let rotate = foreign.accept("a", origin).unwrap();
        assert_eq!(
            rightful.invalidate_accepted(rotate).unwrap_err(),
            FenceError::Stale
        );
        let origin = foreign.capture("a", ReportSource::UsagePoll).unwrap();
        let mint = foreign.accept("a", origin).unwrap();
        assert_eq!(
            rightful.mint(mint, (100, "account")),
            Err(FenceError::Stale)
        );
        assert_eq!(*rightful.admit(&ticket).unwrap(), (100, "account"));
    }

    #[test]
    fn n5a_origin_external_epoch_last_value_exhaustion_and_readd() {
        let fence = WaitProofs::new(1);
        fence.capture("a", ReportSource::UsagePoll).unwrap();
        fence
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut("a")
            .unwrap()
            .epoch = Some(u64::MAX - 1);
        let before = proven(&fence);
        let old_origin = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        fence.invalidate("a").unwrap();
        assert_eq!(
            fence.state.lock().unwrap().entries["a"].epoch,
            Some(u64::MAX)
        );
        let last = proven(&fence);
        let last_origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        assert_eq!(last.epoch, u64::MAX);
        assert_eq!(fence.invalidate("a"), Err(FenceError::Exhausted));
        assert_eq!(fence.invalidate("a"), Err(FenceError::Exhausted));
        assert_eq!(fence.acquire("a").unwrap_err(), FenceError::Exhausted);
        for ticket in [&before, &last] {
            assert_eq!(fence.admit(ticket).unwrap_err(), FenceError::Stale);
        }
        assert_eq!(
            fence.accept("a", old_origin.clone()).unwrap_err(),
            FenceError::Stale
        );
        fence.retire("a").unwrap();
        let current = proven(&fence);
        for origin in [old_origin, last_origin] {
            assert_eq!(fence.accept("a", origin).unwrap_err(), FenceError::Stale);
        }
        assert_eq!(fence.admit(&last).unwrap_err(), FenceError::Stale);
        assert!(fence.admit(&current).is_ok());
    }

    #[test]
    fn n5a_origin_exact_utf8_boundary_and_zero_capacity() {
        let fence = WaitProofs::new(1);
        let id = "\u{00e9}".repeat(MAX_ID_BYTES / 2);
        assert_eq!(id.len(), MAX_ID_BYTES);
        let origin = fence.capture(&id, ReportSource::UsagePoll).unwrap();
        let permit = fence.accept(&id, origin).unwrap();
        fence.mint(permit, (100, "account")).unwrap();
        let ticket = fence.acquire(&id).unwrap().0;
        assert_eq!(
            fence.capture("other", ReportSource::UsagePoll).unwrap_err(),
            FenceError::Capacity
        );
        assert_eq!(*fence.admit(&ticket).unwrap(), (100, "account"));
        let zero = WaitProofs::<Evidence>::new(0);
        assert_eq!(
            zero.capture("a", ReportSource::UsagePoll).unwrap_err(),
            FenceError::Capacity
        );
        assert_eq!(zero.acquire("a").unwrap_err(), FenceError::Missing);
        assert_eq!(zero.retire("a"), Ok(false));
        assert!(zero.state.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn n5a_origin_inflight_release_barrier_rejects_delayed_completion() {
        use std::{sync::mpsc, thread, time::Duration};
        let fence = Arc::new(WaitProofs::new(1));
        let prior = proven(&fence);
        let (sent, captured) = mpsc::sync_channel(0);
        let (released, resume) = mpsc::sync_channel(0);
        let worker = Arc::clone(&fence);
        let inflight = thread::spawn(move || {
            let origin = worker.capture("a", ReportSource::HttpAttempt).unwrap();
            sent.send(()).unwrap(); // Pre-send origin captured, completion delayed.
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(worker.accept("a", origin).unwrap_err(), FenceError::Stale);
            let fresh = worker.capture("a", ReportSource::HttpAttempt).unwrap();
            let permit = worker.accept("a", fresh).unwrap();
            worker.mint(permit, (100, "account")).unwrap();
        });
        captured.recv_timeout(Duration::from_secs(5)).unwrap();
        fence.invalidate("a").unwrap();
        assert_eq!(fence.admit(&prior).unwrap_err(), FenceError::Stale);
        released.send(()).unwrap();
        inflight.join().unwrap();
        assert_eq!(*fence.acquire("a").unwrap().1, (100, "account"));
    }

    #[test]
    fn n5a_origin_close_before_delayed_registry_admission_is_observed() {
        use std::{sync::mpsc, thread, time::Duration};
        let fence = Arc::new(WaitProofs::new(1));
        let ticket = proven(&fence);
        let (ready, waiting) = mpsc::sync_channel(0);
        let (closed, resume) = mpsc::sync_channel(0);
        let worker = Arc::clone(&fence);
        let admission = thread::spawn(move || {
            ready.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(worker.admit(&ticket).unwrap_err(), FenceError::Closed);
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        fence.close().unwrap();
        closed.send(()).unwrap();
        admission.join().unwrap();
        // This is the registry barrier only, NOT native outer-store publication.
    }

    #[test]
    fn n5a_origin_poison_rejects_capture_mint_acquire_and_close() {
        let fence = WaitProofs::<Evidence>::new(1);
        let origin = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let permit = fence.accept("a", origin).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = fence.state.lock().unwrap();
                panic!("synthetic poison");
            }))
            .is_err()
        );
        assert_eq!(
            fence.capture("a", ReportSource::UsagePoll).unwrap_err(),
            FenceError::Poisoned
        );
        assert_eq!(
            fence.mint(permit, (100, "account")),
            Err(FenceError::Poisoned)
        );
        assert_eq!(fence.acquire("a").unwrap_err(), FenceError::Poisoned);
        assert_eq!(fence.close(), Err(FenceError::Poisoned));
    }

    #[test]
    fn n5a_origin_rejected_mint_payload_drop_is_unlocked_including_poison() {
        for scenario in ["missing", "foreign", "poison", "closed"] {
            let source = Arc::new(WaitProofs::<DropProbe>::new(1));
            let foreign = Arc::new(WaitProofs::<DropProbe>::new(1));
            let origin = source.capture("a", ReportSource::UsagePoll).unwrap();
            let permit = source.accept("a", origin).unwrap();
            let target = if scenario == "foreign" {
                &foreign
            } else {
                &source
            };
            let expected = match scenario {
                "missing" => {
                    source.retire("a").unwrap();
                    FenceError::Missing
                }
                "foreign" => {
                    foreign.capture("a", ReportSource::UsagePoll).unwrap();
                    FenceError::Stale
                }
                "closed" => {
                    source.close().unwrap();
                    FenceError::Closed
                }
                "poison" => {
                    assert!(
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let _guard = source.state.lock().unwrap();
                            panic!("synthetic poison");
                        }))
                        .is_err()
                    );
                    FenceError::Poisoned
                }
                _ => unreachable!(),
            };
            let drops = Arc::new(AtomicUsize::new(0));
            assert_eq!(
                target.mint(
                    permit,
                    DropProbe {
                        owner: Arc::downgrade(target),
                        drops: Arc::clone(&drops),
                    }
                ),
                Err(expected)
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }
    fn applied_http<E>(fence: &WaitProofs<E>) -> AcceptPermit {
        let origin = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        let permit = fence.accept("a", origin).unwrap();
        fence.invalidate_accepted(permit).unwrap()
    }

    #[test]
    fn n5a_http_phase_legal_continuation_cuts_interposed_origins() {
        let fence = WaitProofs::new(1);
        let previous = proven(&fence);
        let header = applied_http(&fence);
        let ordinal = header.origin.ordinal;
        let captured_epoch = header.origin.epoch;
        let header_epoch = header.epoch;
        let duplicate = header.origin.clone();
        let sealed = fence.seal_http(header).unwrap();
        assert_eq!(fence.acquire("a").unwrap_err(), FenceError::NoProof);
        let interposed = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let body = fence.continue_http(sealed).unwrap();
        assert_eq!(body.phase, Phase::HttpFinalPending);
        let final_permit = fence.invalidate_accepted(body).unwrap();
        assert_eq!(final_permit.phase, Phase::HttpFinalApplied);
        assert_eq!(final_permit.origin.ordinal, ordinal);
        assert_eq!(final_permit.origin.epoch, captured_epoch);
        assert_eq!(final_permit.epoch, header_epoch + 1);
        fence.mint(final_permit, (100, "account")).unwrap();
        let current = fence.acquire("a").unwrap().0;
        for origin in [duplicate, interposed] {
            assert_eq!(fence.accept("a", origin).unwrap_err(), FenceError::Stale);
        }
        assert_eq!(fence.admit(&previous).unwrap_err(), FenceError::Stale);
        assert_eq!(*fence.admit(&current).unwrap(), (100, "account"));
        let fresh = fence.capture("a", ReportSource::HttpAttempt).unwrap();
        assert!(fence.accept("a", fresh).is_ok());
    }

    #[test]
    fn n5a_http_phase_only_applied_http_can_seal() {
        for (source, rotate) in [
            (ReportSource::HttpAttempt, false),
            (ReportSource::UsagePoll, false),
            (ReportSource::UsagePoll, true),
        ] {
            let fence = WaitProofs::<Evidence>::new(1);
            let origin = fence.capture("a", source).unwrap();
            let permit = fence.accept("a", origin).unwrap();
            let permit = if rotate {
                fence.invalidate_accepted(permit).unwrap()
            } else {
                permit
            };
            assert_eq!(fence.seal_http(permit).unwrap_err(), FenceError::Rejected);
            assert_eq!(fence.acquire("a").unwrap_err(), FenceError::NoProof);
        }
    }

    #[test]
    fn n5a_http_phase_pending_cannot_mint_and_body_permits_cannot_reseal() {
        for step in [
            "pending-mint",
            "pending-reseal",
            "applied-reseal",
            "third-rotation",
        ] {
            let fence = WaitProofs::<Evidence>::new(1);
            let header = applied_http(&fence);
            let sealed = fence.seal_http(header).unwrap();
            let body = fence.continue_http(sealed).unwrap();
            match step {
                "pending-mint" => assert_eq!(
                    fence.mint(body, (100, "account")),
                    Err(FenceError::Rejected)
                ),
                "pending-reseal" => {
                    assert_eq!(fence.seal_http(body).unwrap_err(), FenceError::Rejected)
                }
                "applied-reseal" => {
                    let final_permit = fence.invalidate_accepted(body).unwrap();
                    assert_eq!(
                        fence.seal_http(final_permit).unwrap_err(),
                        FenceError::Rejected
                    );
                }
                "third-rotation" => {
                    let final_permit = fence.invalidate_accepted(body).unwrap();
                    assert_eq!(
                        fence.invalidate_accepted(final_permit).unwrap_err(),
                        FenceError::Rejected
                    );
                }
                _ => unreachable!(),
            }
            assert_eq!(fence.acquire("a").unwrap_err(), FenceError::NoProof);
        }
    }

    #[test]
    fn n5a_http_phase_sealed_continuation_revoked_by_owner_changes() {
        for event in ["invalidate", "partial", "close", "retire"] {
            let fence = WaitProofs::<Evidence>::new(1);
            let header = applied_http(&fence);
            let sealed = fence.seal_http(header).unwrap();
            let expected = match event {
                "invalidate" => {
                    fence.invalidate("a").unwrap();
                    FenceError::Stale
                }
                "partial" => {
                    let newer = fence.capture("a", ReportSource::UsagePoll).unwrap();
                    drop(fence.accept("a", newer).unwrap());
                    FenceError::Stale
                }
                "close" => {
                    fence.close().unwrap();
                    FenceError::Closed
                }
                "retire" => {
                    fence.retire("a").unwrap();
                    fence.capture("a", ReportSource::HttpAttempt).unwrap();
                    FenceError::Stale
                }
                _ => unreachable!(),
            };
            assert_eq!(fence.continue_http(sealed).unwrap_err(), expected);
        }
    }

    #[test]
    fn n5a_http_phase_resumed_permit_revoked_before_final_rotation_or_mint() {
        for after_rotation in [false, true] {
            let fence = WaitProofs::<Evidence>::new(1);
            let header = applied_http(&fence);
            let sealed = fence.seal_http(header).unwrap();
            let body = fence.continue_http(sealed).unwrap();
            let permit = if after_rotation {
                fence.invalidate_accepted(body).unwrap()
            } else {
                body
            };
            fence.invalidate("a").unwrap();
            if after_rotation {
                assert_eq!(fence.mint(permit, (100, "account")), Err(FenceError::Stale));
            } else {
                assert_eq!(
                    fence.invalidate_accepted(permit).unwrap_err(),
                    FenceError::Stale
                );
            }
        }
    }

    #[test]
    fn n5a_http_phase_final_epoch_exhaustion_never_wraps_or_mints() {
        let fence = WaitProofs::<Evidence>::new(1);
        fence.capture("a", ReportSource::HttpAttempt).unwrap();
        fence
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut("a")
            .unwrap()
            .epoch = Some(u64::MAX - 1);
        let header = applied_http(&fence);
        assert_eq!(header.epoch, u64::MAX);
        let sealed = fence.seal_http(header).unwrap();
        let interposed = fence.capture("a", ReportSource::UsagePoll).unwrap();
        let body = fence.continue_http(sealed).unwrap();
        assert_eq!(
            fence.invalidate_accepted(body).unwrap_err(),
            FenceError::Exhausted
        );
        assert_eq!(fence.acquire("a").unwrap_err(), FenceError::Exhausted);
        assert_eq!(
            fence.accept("a", interposed).unwrap_err(),
            FenceError::Stale
        );
        assert_eq!(
            fence.capture("a", ReportSource::HttpAttempt).unwrap_err(),
            FenceError::Exhausted
        );
    }

    #[test]
    fn n5a_http_phase_foreign_continuation_cannot_touch_resident_proof() {
        let source = WaitProofs::<Evidence>::new(1);
        let header = applied_http(&source);
        let sealed = source.seal_http(header).unwrap();
        let target = WaitProofs::new(1);
        let resident = proven(&target);
        assert_eq!(target.continue_http(sealed).unwrap_err(), FenceError::Stale);
        assert_eq!(*target.admit(&resident).unwrap(), (100, "account"));
    }

    #[test]
    fn n5a_http_phase_drop_cancels_and_rejected_pending_mint_drops_unlocked() {
        let fence = Arc::new(WaitProofs::<DropProbe>::new(1));
        let drops = Arc::new(AtomicUsize::new(0));
        let header = applied_http(&fence);
        let duplicate = header.origin.clone();
        drop(fence.seal_http(header).unwrap());
        assert_eq!(fence.accept("a", duplicate).unwrap_err(), FenceError::Stale);
        assert!(matches!(fence.acquire("a"), Err(FenceError::NoProof)));
        let header = applied_http(&fence);
        let sealed = fence.seal_http(header).unwrap();
        let body = fence.continue_http(sealed).unwrap();
        assert_eq!(
            fence.mint(
                body,
                DropProbe {
                    owner: Arc::downgrade(&fence),
                    drops: Arc::clone(&drops),
                }
            ),
            Err(FenceError::Rejected)
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let header = applied_http(&fence);
        let sealed = fence.seal_http(header).unwrap();
        drop(fence.continue_http(sealed).unwrap());
        assert!(matches!(fence.acquire("a"), Err(FenceError::NoProof)));
    }
}
