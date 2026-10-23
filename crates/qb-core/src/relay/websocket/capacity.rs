use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(in crate::relay) type Admission =
    Arc<dyn Fn(&str) -> Option<crate::bindings::DispatchAdmission> + Send + Sync>;

/// One request's first-source grace, always inside its original upgrade limit.
pub(super) struct OwnWait {
    source: String,
    capacity: Weak<Capacity>,
    until: Instant,
    waited: bool,
}

/// A capacity-only hint. Binding admission must independently recheck the source.
pub(super) struct SpillSource {
    source: String,
    capacity: Weak<Capacity>,
}

/// A move justified by one live destination reservation and its capped source.
/// Neither weak reference keeps a reservation or its capacity owner alive.
pub(crate) struct CapacityMoveEvidence {
    source: String,
    target: String,
    reservation: Weak<AtomicBool>,
    capacity: Weak<Capacity>,
}

impl CapacityMoveEvidence {
    pub(crate) fn applies_to(&self, from: &str, to: &str) -> bool {
        if self.source != from || self.target != to {
            return false;
        }
        let (Some(capacity), Some(reservation)) =
            (self.capacity.upgrade(), self.reservation.upgrade())
        else {
            return false;
        };
        // Drop releases the exact reservation under this same lock. Observe
        // its liveness and source capacity at a single linearization point.
        let accounts = capacity.0.lock().unwrap_or_else(|e| e.into_inner());
        accounts
            .get(&self.source)
            .is_some_and(AccountCapacity::full)
            && reservation.load(Ordering::Acquire)
    }
}

fn own_wait_deadline(now: Instant, deadline: Instant) -> Option<Instant> {
    let grace = (deadline.saturating_duration_since(now) / 2).min(Duration::from_millis(250));
    (!grace.is_zero()).then(|| now + grace)
}

impl OwnWait {
    pub(super) fn start(capacity: &Arc<Capacity>, source: &str, deadline: Instant) -> Option<Self> {
        let until = own_wait_deadline(Instant::now(), deadline)?;
        capacity.is_capped(source).then(|| Self {
            source: source.to_owned(),
            capacity: Arc::downgrade(capacity),
            until,
            waited: false,
        })
    }

    pub(super) fn source(&self) -> &str {
        &self.source
    }

    pub(super) async fn wait_once(&mut self, capacity: &Capacity) {
        if !std::ptr::eq(self.capacity.as_ptr(), capacity) {
            return;
        }
        self.waited = true;
        #[cfg(feature = "testkit")]
        capacity.before_own_wait().await;
        capacity.wait_until(self.until).await;
    }

    pub(super) fn spill_source(&self, capacity: &Capacity) -> Option<SpillSource> {
        self.spill_source_at(capacity, Instant::now())
    }

    fn spill_source_at(&self, capacity: &Capacity, now: Instant) -> Option<SpillSource> {
        (std::ptr::eq(self.capacity.as_ptr(), capacity)
            && self.waited
            && now >= self.until
            && capacity.is_capped(&self.source))
        .then(|| SpillSource {
            source: self.source.clone(),
            capacity: self.capacity.clone(),
        })
    }
}

impl SpillSource {
    pub(super) fn source(&self) -> &str {
        &self.source
    }

    pub(super) fn still_capped(&self, capacity: &Capacity) -> bool {
        std::ptr::eq(self.capacity.as_ptr(), capacity) && capacity.is_capped(&self.source)
    }

    pub(super) fn admit(&self, permit: &Permit) -> Option<CapacityMoveEvidence> {
        (self.source != permit.account && self.still_capped(&permit.capacity)).then(|| {
            CapacityMoveEvidence {
                source: self.source.clone(),
                target: permit.account.clone(),
                reservation: Arc::downgrade(&permit.lifetime),
                capacity: self.capacity.clone(),
            }
        })
    }
}
#[cfg(feature = "testkit")]
type SelectionPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(feature = "testkit")]
type AdmissionPause = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[derive(Default)]
struct AccountCapacity {
    reserved: usize,
    cap: Option<usize>,
    retry_at: Option<Instant>,
}

impl AccountCapacity {
    fn full(&self) -> bool {
        self.cap.is_some_and(|cap| self.reserved >= cap)
            || self.retry_at.is_some_and(|at| Instant::now() < at)
    }
}

/// Process-local upstream socket capacity, independent of account health and
/// response.create pacing. A reservation covers pending dial through pump drop.
#[derive(Default)]
pub(in crate::relay) struct Capacity(
    Mutex<BTreeMap<String, AccountCapacity>>,
    #[cfg(feature = "testkit")] Mutex<Option<SelectionPause>>,
    #[cfg(feature = "testkit")] std::sync::atomic::AtomicUsize,
    #[cfg(feature = "testkit")] Mutex<Option<AdmissionPause>>,
    #[cfg(feature = "testkit")] Mutex<Option<SelectionPause>>,
);

impl Capacity {
    #[cfg(feature = "testkit")]
    pub(in crate::relay) fn pause_own_wait(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered, observed) = tokio::sync::oneshot::channel();
        let (resume, released) = tokio::sync::oneshot::channel();
        *self.4.lock().unwrap_or_else(|e| e.into_inner()) = Some((entered, released));
        (observed, resume)
    }

    #[cfg(feature = "testkit")]
    async fn before_own_wait(&self) {
        let hook = self.4.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((entered, released)) = hook {
            let _ = entered.send(());
            let _ = released.await;
        }
    }

    #[cfg(feature = "testkit")]
    pub(in crate::relay) fn pause_admission(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered, observed) = tokio::sync::oneshot::channel();
        let (resume, released) = std::sync::mpsc::channel();
        *self.3.lock().unwrap_or_else(|e| e.into_inner()) = Some((entered, released));
        (observed, resume)
    }

    #[cfg(feature = "testkit")]
    pub(in crate::relay) fn before_admission(&self) {
        let hook = self.3.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((entered, released)) = hook {
            let _ = entered.send(());
            let _ = released.recv_timeout(Duration::from_secs(5));
        }
    }

    #[cfg(feature = "testkit")]
    pub(in crate::relay) fn pause_selection(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered, observed) = tokio::sync::oneshot::channel();
        let (resume, released) = tokio::sync::oneshot::channel();
        *self.1.lock().unwrap_or_else(|e| e.into_inner()) = Some((entered, released));
        (observed, resume)
    }

    #[cfg(feature = "testkit")]
    pub(in crate::relay) async fn before_selection(&self) {
        let hook = self.1.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((entered, released)) = hook {
            let _ = entered.send(());
            let _ = released.await;
        }
    }

    #[cfg(feature = "testkit")]
    pub(in crate::relay) fn reserved_for_test(&self, account: &str) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .map_or(0, |state| state.reserved)
    }

    #[cfg(feature = "testkit")]
    pub(in crate::relay) fn waits_for_test(&self) -> usize {
        self.2.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) fn capped(&self) -> BTreeSet<String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, state)| state.full())
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub(super) fn is_capped(&self, account: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
            .is_some_and(AccountCapacity::full)
    }

    pub(super) fn reserve(self: &Arc<Self>, account: &str) -> Option<Permit> {
        let mut accounts = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = accounts.entry(account.into()).or_default();
        if state.full() {
            return None;
        }
        state.reserved += 1;
        Some(Permit {
            capacity: self.clone(),
            account: account.into(),
            lifetime: Arc::new(AtomicBool::new(true)),
        })
    }

    pub(super) fn stream_connection_limit(&self, account: &str) {
        let mut accounts = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = accounts.entry(account.into()).or_default();
        // Keep the rejected socket occupied until it drops, but do not count
        // it as evidence of usable upstream capacity.
        let observed = state.reserved.saturating_sub(1).max(1);
        state.cap = Some(state.cap.map_or(observed, |cap| cap.min(observed)));
        if state.reserved <= 1 {
            state.retry_at = Some(Instant::now() + Duration::from_millis(250));
        }
    }

    /// Poll only while inside the caller's existing upgrade envelope. Waiting
    /// requests retain no slot and create no unbounded account-local queue.
    pub(super) async fn wait_until(&self, deadline: Instant) -> bool {
        #[cfg(feature = "testkit")]
        self.2.fetch_add(1, std::sync::atomic::Ordering::Release);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
        Instant::now() < deadline
    }
}

pub(super) struct Permit {
    capacity: Arc<Capacity>,
    account: String,
    lifetime: Arc<AtomicBool>,
}

impl Permit {
    pub(super) fn connection_limit(&self) {
        let mut accounts = self.capacity.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = accounts.get_mut(&self.account).expect("reserved account");
        // Exclude this rejected dial. Other pending dials still occupy slots;
        // repeated rejections may tighten the estimate, never raise it. A
        // rejection with no other local socket permits one future probe.
        let observed = state.reserved.saturating_sub(1).max(1);
        state.cap = Some(state.cap.map_or(observed, |cap| cap.min(observed)));
        // A shared credential may have sockets outside this core. Even if no
        // local socket remains, back off rather than redial in a tight loop.
        if state.reserved == 1 {
            state.retry_at = Some(Instant::now() + Duration::from_millis(250));
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut accounts = self.capacity.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = accounts.get_mut(&self.account).expect("reserved account");
        self.lifetime.store(false, Ordering::Release);
        state.reserved -= 1;
        if state.reserved == 0 && state.cap.is_none() {
            accounts.remove(&self.account);
        }
    }
}

/// Synthetic owner fixture; retains real occupied and reserved slots.
#[cfg(test)]
pub(crate) fn test_move_evidence(from: &str, to: &str) -> (impl Sized, CapacityMoveEvidence) {
    let capacity = Arc::new(Capacity::default());
    let live = capacity.reserve(from).unwrap();
    let rejected = capacity.reserve(from).unwrap();
    rejected.connection_limit();
    drop(rejected);
    let mut wait =
        OwnWait::start(&capacity, from, Instant::now() + Duration::from_secs(10)).unwrap();
    wait.waited = true;
    let source = wait.spill_source_at(&capacity, wait.until).unwrap();
    let target = capacity.reserve(to).unwrap();
    let proof = source.admit(&target).unwrap();
    ((capacity, live, target), proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capped_source(capacity: &Arc<Capacity>) -> (Permit, OwnWait) {
        let live = capacity.reserve("a").unwrap();
        let rejected = capacity.reserve("a").unwrap();
        rejected.connection_limit();
        drop(rejected);
        let wait = OwnWait::start(capacity, "a", Instant::now() + Duration::from_secs(10)).unwrap();
        (live, wait)
    }

    #[tokio::test]
    async fn n6t_wait_and_spill_reject_another_capacity_owner() {
        let capacity = Arc::new(Capacity::default());
        let other = Arc::new(Capacity::default());
        let (_live, mut wait) = capped_source(&capacity);
        let (_other_live, _other_wait) = capped_source(&other);
        wait.wait_once(&other).await;
        assert!(!wait.waited);
        assert!(wait.spill_source_at(&capacity, wait.until).is_none());
        wait.waited = true;
        assert!(wait.spill_source_at(&other, wait.until).is_none());
        let source = wait.spill_source_at(&capacity, wait.until).unwrap();
        assert!(!source.still_capped(&other));
        let foreign_target = other.reserve("b").unwrap();
        assert!(source.admit(&foreign_target).is_none());
    }

    #[test]
    fn n6t_evidence_matches_only_its_source_and_destination() {
        let capacity = Arc::new(Capacity::default());
        let (live, mut wait) = capped_source(&capacity);
        // Even an elapsed deadline cannot yield a hint without a wait.
        assert!(wait.spill_source_at(&capacity, wait.until).is_none());
        wait.waited = true;
        assert!(
            wait.spill_source_at(&capacity, wait.until - Duration::from_nanos(1))
                .is_none()
        );
        let source = wait.spill_source_at(&capacity, wait.until).unwrap();
        assert!(source.admit(&live).is_none());
        let target = capacity.reserve("b").unwrap();
        let evidence = source.admit(&target).unwrap();
        assert!(evidence.applies_to("a", "b"));
        assert!(!evidence.applies_to("c", "b"));
        assert!(!evidence.applies_to("a", "c"));
        assert!(!evidence.applies_to("b", "a"));
    }

    #[test]
    fn n6t_evidence_dies_with_exact_destination_permit() {
        let capacity = Arc::new(Capacity::default());
        let (_live, mut wait) = capped_source(&capacity);
        wait.waited = true;
        let source = wait.spill_source_at(&capacity, wait.until).unwrap();
        let target = capacity.reserve("b").unwrap();
        let other_target = capacity.reserve("b").unwrap();
        let evidence = source.admit(&target).unwrap();
        assert!(evidence.applies_to("a", "b"));
        assert_eq!(Arc::strong_count(&target.lifetime), 1);
        // Keep the allocation alive past slot release, just as a concurrent
        // proof check may hold an upgraded marker while Permit::drop runs.
        let retained_marker = target.lifetime.clone();
        drop(target);
        assert_eq!(evidence.reservation.strong_count(), 1);
        assert!(!retained_marker.load(Ordering::Acquire));
        assert!(
            !evidence.applies_to("a", "b"),
            "released reservation allocation is not a live destination permit"
        );
        assert!(source.admit(&other_target).unwrap().applies_to("a", "b"));
        let replacement = capacity.reserve("b").unwrap();
        assert!(!evidence.applies_to("a", "b"));
        drop((other_target, replacement));
    }

    #[test]
    fn n6t_freeing_source_slot_invalidates_evidence_and_admission() {
        let capacity = Arc::new(Capacity::default());
        let (live, mut wait) = capped_source(&capacity);
        wait.waited = true;
        let source = wait.spill_source_at(&capacity, wait.until).unwrap();
        let target = capacity.reserve("b").unwrap();
        let evidence = source.admit(&target).unwrap();
        assert!(evidence.applies_to("a", "b"));
        drop(live);
        assert!(!evidence.applies_to("a", "b"));
        assert!(source.admit(&target).is_none());
    }

    #[test]
    fn n6t_table_veto_preserves_binding_policy_and_ordinary_move_budget() {
        use crate::{
            accounts::Provider,
            bindings::{BindingTable, Candidate, Decision, DispatchAdmission, MoveObservations},
        };

        fn exhausted_source(capacity: &Arc<Capacity>, source: &str) -> SpillSource {
            let mut wait =
                OwnWait::start(capacity, source, Instant::now() + Duration::from_secs(10)).unwrap();
            wait.waited = true;
            wait.spill_source_at(capacity, wait.until).unwrap()
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/ws-capacity-tests");
        std::fs::create_dir_all(&root).unwrap();
        for case in ["wrong_target", "wrong_source", "dead_reservation"] {
            let home = tempfile::tempdir_in(&root).unwrap();
            let mut config = crate::config::RoutingConfig::default();
            // One successful ordinary move exhausts this budget. A veto must
            // leave that move available, and capacity proof must still charge it.
            config.move_guard.global_ten_minutes = 1;
            let table = BindingTable::new(home.path(), config).unwrap();
            let candidates = ["cx-6c0001", "cx-6c0002", "cx-6c0003"].map(|id| {
                let mut account = crate::accounts::health::tests::fixture();
                account.provider = Provider::Codex;
                account.identity = crate::accounts::Identity::Codex {
                    user_id: None,
                    account_id: id.into(),
                    email: "capacity@example.test".into(),
                };
                account.id = id.into();
                account.email = "capacity@example.test".into();
                Candidate {
                    account,
                    quota: None,
                    inflight: 0,
                }
            });
            let a = candidates[0].account.id.as_str();
            let b = candidates[1].account.id.as_str();
            let c = candidates[2].account.id.as_str();
            let actual_source = if case == "wrong_source" { c } else { a };
            let actual_target = if case == "wrong_target" { c } else { b };
            let key = "codex:n6t-capacity-proof";
            let now = chrono::Utc::now();
            let pick = |target: &str, admission: DispatchAdmission| {
                table.choose_with_hold_at_admitted_proof(
                    Provider::Codex,
                    Some(key),
                    None,
                    None,
                    &candidates,
                    &candidates,
                    MoveObservations::untracked(),
                    Some(target),
                    0,
                    false,
                    false,
                    now,
                    None,
                    |selected| {
                        assert_eq!(selected, target, "{case}");
                        Some(admission)
                    },
                )
            };
            let Decision::Selected { account, lease, .. } =
                pick(actual_source, DispatchAdmission::ordinary())
            else {
                panic!("initial placement: {case}")
            };
            assert_eq!(account, actual_source, "{case}");
            lease.dispatched();
            drop(lease);
            // Default sessions need no durable policy row. Give this fixture
            // an actual saved anchor so the veto also proves no policy write.
            table
                .policies
                .set_session(
                    &crate::routing::session_id(key),
                    Some(crate::routing::Settings {
                        mode: crate::routing::Mode::Recover,
                        reset_wait_secs: 0,
                        on_5h_limit: crate::routing::On5hLimit::Wait,
                    }),
                    None,
                    Some(actual_source),
                )
                .unwrap();
            assert_eq!(
                table.policies.plan(Some(key)).anchor.as_deref(),
                Some(actual_source),
                "{case}: fixture needs a saved policy anchor"
            );

            let capacity = Arc::new(Capacity::default());
            let live_a = capacity.reserve(a).unwrap();
            let rejected_a = capacity.reserve(a).unwrap();
            rejected_a.connection_limit();
            drop(rejected_a);
            let live_c = (case == "wrong_source").then(|| {
                let live = capacity.reserve(c).unwrap();
                let rejected = capacity.reserve(c).unwrap();
                rejected.connection_limit();
                drop(rejected);
                live
            });
            let source_a = exhausted_source(&capacity, a);
            let target_b = capacity.reserve(b).unwrap();
            let another_b = capacity.reserve(b).unwrap();
            let stale = source_a.admit(&target_b).unwrap();
            let target_b = if case == "dead_reservation" {
                drop(target_b);
                None
            } else {
                Some(target_b)
            };

            let before_binding = table.export().unwrap();
            let before_policy = table.policies.list();
            let before_plan = serde_json::to_value(table.policies.plan(Some(key))).unwrap();
            assert!(
                matches!(
                    pick(actual_target, DispatchAdmission::capacity(stale)),
                    Decision::None
                ),
                "{case}: invalid capacity evidence must precede publication"
            );
            assert_eq!(table.export().unwrap(), before_binding, "{case}");
            assert_eq!(table.policies.list(), before_policy, "{case}");
            assert_eq!(
                serde_json::to_value(table.policies.plan(Some(key))).unwrap(),
                before_plan,
                "{case}"
            );
            assert_eq!(
                table.current(Some(key)).as_deref(),
                Some(actual_source),
                "{case}"
            );

            let valid_source = exhausted_source(&capacity, actual_source);
            let valid_target = capacity.reserve(actual_target).unwrap();
            let proof = valid_source.admit(&valid_target).unwrap();
            let Decision::Selected { account, lease, .. } =
                pick(actual_target, DispatchAdmission::capacity(proof))
            else {
                panic!("veto consumed the only ordinary move: {case}")
            };
            assert_eq!(account, actual_target, "{case}");
            lease.dispatched();
            drop(lease);
            assert_eq!(
                table.current(Some(key)).as_deref(),
                Some(actual_target),
                "{case}"
            );
            assert_eq!(
                table.policies.plan(Some(key)).anchor.as_deref(),
                Some(actual_target),
                "{case}"
            );

            let next = if actual_target == b { 2 } else { 1 };
            assert!(
                matches!(
                    table.choose_with_hold_at_admitted_proof(
                        Provider::Codex,
                        Some(key),
                        None,
                        None,
                        &candidates[next..next + 1],
                        &candidates,
                        MoveObservations::untracked(),
                        Some(&candidates[next].account.id),
                        0,
                        false,
                        false,
                        now,
                        None,
                        |_| panic!("ordinary move budget was not charged: {case}"),
                    ),
                    Decision::MoveHeld { .. }
                ),
                "{case}"
            );
            drop((live_a, live_c, target_b, another_b, valid_target));
        }
    }

    #[test]
    fn n10_own_wait_grace_is_inside_the_original_upgrade_envelope() {
        let now = Instant::now();
        assert_eq!(
            own_wait_deadline(now, now + Duration::from_secs(10)),
            Some(now + Duration::from_millis(250))
        );
        assert_eq!(
            own_wait_deadline(now, now + Duration::from_millis(100)),
            Some(now + Duration::from_millis(50))
        );
        assert_eq!(own_wait_deadline(now, now), None);
        assert_eq!(own_wait_deadline(now, now - Duration::from_millis(1)), None);
    }

    #[test]
    fn n10_spill_source_requires_wait_exhaustion_and_still_capped_source() {
        let capacity = Arc::new(Capacity::default());
        let now = Instant::now();
        let deadline = now + Duration::from_secs(10);
        assert!(OwnWait::start(&capacity, "a", deadline).is_none());
        let live = capacity.reserve("a").unwrap();
        let rejected = capacity.reserve("a").unwrap();
        rejected.connection_limit();
        drop(rejected);
        let mut wait = OwnWait::start(&capacity, "a", deadline).unwrap();
        let until = wait.until;
        assert!(wait.spill_source_at(&capacity, until).is_none());
        // Explicit historical wait observation; no real-time sleeps are needed
        // to test both sides of the original grace's exact boundary.
        wait.waited = true;
        assert!(
            wait.spill_source_at(&capacity, until - Duration::from_nanos(1))
                .is_none()
        );
        let source = wait.spill_source_at(&capacity, until).unwrap();
        assert_eq!(source.source(), "a");
        assert!(source.still_capped(&capacity));
        drop(live);
        assert!(wait.spill_source_at(&capacity, until).is_none());
        assert!(!source.still_capped(&capacity));
        assert!(capacity.reserve("a").is_some());
    }

    #[test]
    fn capped_candidate_moves_normal_sticky_binding_but_preserves_strict_anchor() {
        use crate::{
            accounts::Provider,
            bindings::{BindingTable, Candidate, Decision, DispatchAdmission, MoveObservations},
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/ws-capacity-tests");
        std::fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let table =
            BindingTable::new(home.path(), crate::config::RoutingConfig::default()).unwrap();
        let identity = crate::accounts::Identity::Codex {
            user_id: None,
            account_id: "synthetic-capacity-first".into(),
            email: "capacity@example.invalid".into(),
        };
        let a = crate::accounts::Account {
            schema: 1,
            id: identity.id(),
            provider: Provider::Codex,
            label: "synthetic".into(),
            email: "capacity@example.invalid".into(),
            plan: None,
            identity,
            source: crate::accounts::Source::Oauth,
            user_state: crate::accounts::UserState::Active,
            health: crate::accounts::Health::Ready,
            auth_failures: 0,
            payment_failures: 0,
            payment_until: None,
            auth_probe_pending: false,
            quota_cooling: None,
            quota_credit_releasable: false,
            drain: false,
            allow_credits: false,
            generation: 0,
            token_meta: crate::accounts::TokenMeta::default(),
        };
        let mut b = a.clone();
        b.identity = crate::accounts::Identity::Codex {
            user_id: None,
            account_id: "synthetic-capacity-other".into(),
            email: "capacity@example.invalid".into(),
        };
        b.id = b.identity.id();
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        let candidates = [
            Candidate {
                account: a,
                quota: None,
                inflight: 0,
            },
            Candidate {
                account: b,
                quota: None,
                inflight: 0,
            },
        ];
        let capacity = Arc::new(Capacity::default());
        let live = capacity.reserve(&a_id).unwrap();
        let rejected = capacity.reserve(&a_id).unwrap();
        rejected.connection_limit();
        drop(rejected);
        for strict in [false, true] {
            let key = if strict {
                "codex:capacity-strict"
            } else {
                "codex:capacity-sticky"
            };
            let Decision::Selected { account, lease, .. } = table.choose(
                Provider::Codex,
                Some(key),
                None,
                None,
                &candidates[..1],
                None,
                0,
            ) else {
                panic!("initial synthetic placement")
            };
            assert_eq!(account, a_id);
            lease.dispatched();
            drop(lease);
            if strict {
                table
                    .policies
                    .set_session(
                        &crate::routing::session_id(key),
                        Some(crate::routing::Settings {
                            mode: crate::routing::Mode::Strict,
                            reset_wait_secs: 0,
                            on_5h_limit: crate::routing::On5hLimit::Wait,
                        }),
                        None,
                        Some(&a_id),
                    )
                    .unwrap();
            }
            let capped = capacity.capped();
            let available: Vec<_> = candidates
                .iter()
                .filter(|candidate| !capped.contains(&candidate.account.id))
                .cloned()
                .collect();
            // Model the end of the source grace at its explicit std-clock
            // deadline, as the other capacity-owner unit fixtures do.
            let mut wait =
                OwnWait::start(&capacity, &a_id, Instant::now() + Duration::from_secs(10)).unwrap();
            wait.waited = true;
            let source = wait.spill_source_at(&capacity, wait.until).unwrap();
            let target = capacity.reserve(&b_id).unwrap();
            let proof = source.admit(&target).unwrap();
            // Keep this exact capacity owner and destination reservation alive
            // through admission; filtering alone is not capacity move authority.
            let decision = table.choose_with_hold_at_admitted_proof(
                Provider::Codex,
                Some(key),
                None,
                None,
                &available,
                &candidates,
                MoveObservations::untracked(),
                None,
                0,
                false,
                false,
                chrono::Utc::now(),
                None,
                |_| Some(DispatchAdmission::capacity(proof)),
            );
            if strict {
                assert!(matches!(decision, Decision::None | Decision::Blocked));
                assert_eq!(table.current(Some(key)).as_deref(), Some(a_id.as_str()));
            } else {
                let Decision::Selected { account, .. } = decision else {
                    panic!("spread to available account")
                };
                assert_eq!(account, b_id);
            }
        }
        drop(live);
        assert!(capacity.reserve(&a_id).is_some());
    }

    #[test]
    fn external_sockets_cause_backoff_and_stream_rejection_keeps_its_slot() {
        let capacity = Arc::new(Capacity::default());
        let rejected = capacity.reserve("external").unwrap();
        rejected.connection_limit();
        drop(rejected);
        assert!(capacity.reserve("external").is_none());
        let live = capacity.reserve("stream").unwrap();
        capacity.stream_connection_limit("stream");
        assert!(capacity.reserve("stream").is_none());
        drop(live);
        assert!(
            capacity.reserve("stream").is_none(),
            "external occupancy backs off even after close"
        );
    }

    #[test]
    fn learned_cap_spreads_pending_and_live_sockets_and_drop_reopens_room() {
        let capacity = Arc::new(Capacity::default());
        let live = capacity.reserve("a").unwrap();
        let pending = capacity.reserve("a").unwrap();
        let rejected = capacity.reserve("a").unwrap();
        rejected.connection_limit();
        drop(rejected);
        assert!(capacity.capped().contains("a"));
        assert!(capacity.reserve("a").is_none());
        let alternative = capacity.reserve("b").unwrap();
        drop(pending);
        assert!(!capacity.capped().contains("a"));
        let replacement = capacity.reserve("a").unwrap();
        assert!(capacity.reserve("a").is_none());
        drop((live, alternative, replacement));
        assert!(capacity.capped().is_empty());
    }

    #[test]
    fn concurrent_reservations_cannot_overbook_learned_cap() {
        let capacity = Arc::new(Capacity::default());
        let live = capacity.reserve("a").unwrap();
        let rejected = capacity.reserve("a").unwrap();
        rejected.connection_limit();
        drop((live, rejected));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let capacity = capacity.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let permit = capacity.reserve("a");
                    barrier.wait();
                    permit
                })
            })
            .collect();
        let permits: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(permits.iter().filter(|p| p.is_some()).count(), 1);
    }

    #[tokio::test]
    async fn cancellation_drops_pending_slot_and_wait_is_deadline_bounded() {
        let capacity = Arc::new(Capacity::default());
        let live = capacity.reserve("a").unwrap();
        let rejected = capacity.reserve("a").unwrap();
        rejected.connection_limit();
        drop((live, rejected));
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let capacity = capacity.clone();
            async move {
                let _pending = capacity.reserve("a").unwrap();
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            }
        });
        started.await.unwrap();
        assert!(capacity.reserve("a").is_none());
        let deadline = Instant::now() + Duration::from_millis(5);
        assert!(!capacity.wait_until(deadline).await);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(capacity.reserve("a").is_some());
    }
}
