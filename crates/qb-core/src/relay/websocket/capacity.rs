use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub(in crate::relay) type Admission = Arc<dyn Fn(&str) -> bool + Send + Sync>;
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
);

impl Capacity {
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
        state.reserved -= 1;
        if state.reserved == 0 && state.cap.is_none() {
            accounts.remove(&self.account);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_candidate_moves_normal_sticky_binding_but_preserves_strict_anchor() {
        use crate::{
            accounts::Provider,
            bindings::{BindingTable, Candidate, Decision},
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
            let decision =
                table.choose(Provider::Codex, Some(key), None, None, &available, None, 0);
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
