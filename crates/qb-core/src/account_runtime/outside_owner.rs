//! D30 caller coverage for the pure outside-use reducer.
//!
//! Engineering assumptions: a provider's numeric usage is cumulative within an
//! unchanged absolute reset/duration identity, apart from the reducer's noise
//! band. A duration-derived start identifies a window; it is NOT an observed
//! zero or a charge-settlement timestamp. Cold process history is unknown for
//! the first observed window and its successor. Later observed consecutive
//! own-free windows can establish a baseline after those tainted windows.
//!
//! Own sends, active work and uncertain history taint the current and next
//! observed window. Later own-free consecutive windows can recover. This is a
//! bounded attribution heuristic, NOT proof that provider charges have settled;
//! completion, drop, cancellation and inflight zero do not themselves certify a
//! clean window. Known active work keeps extending uncertainty at observations.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::outside_usage::{Observation, OwnTraffic, State as Detector, Window};
use crate::bindings::BindingTable;

const MAX_ACCOUNTS: usize = 4096;
const MAX_WINDOWS: usize = 16;
const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountState {
    generation: u64,
    last_order: u64,
    windows: BTreeMap<String, TrackedWindow>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackedWindow {
    window: Window,
    detector: Detector,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Disk {
    schema: u32,
    next_order: u64,
    accounts: BTreeMap<String, AccountState>,
}

impl Default for Disk {
    fn default() -> Self {
        Self {
            schema: 1,
            next_order: 0,
            accounts: BTreeMap::new(),
        }
    }
}

struct Inner {
    disk: Disk,
    // Shared with native destination admission. Physical sends only latch it;
    // they never acquire binding authority or perform metadata I/O.
    failed: Arc<AtomicBool>,
}

/// Created before the physical quota GET. Neither Clone nor caller-constructible.
pub(crate) struct PollOrigin {
    epoch: Arc<()>,
    account: String,
    order: u64,
    generation: u64,
    active: bool,
    started_at: DateTime<Utc>,
}

/// The outer poll marker survives failure to capture an attribution fence.
pub(super) struct PollApplication {
    pub(super) origin: Option<PollOrigin>,
}

pub(super) struct OutsideOwner {
    path: PathBuf,
    started_at: DateTime<Utc>,
    epoch: Arc<()>,
    inner: Mutex<Inner>,
    bindings: Mutex<Option<Weak<BindingTable>>>,
    publication: Mutex<()>,
}

impl OutsideOwner {
    pub(super) fn new(home: &Path) -> Self {
        Self::new_at(home, Utc::now())
    }

    fn new_at(home: &Path, started_at: DateTime<Utc>) -> Self {
        let dir = home.join("state/outside-use");
        let path = dir.join("owner.json");
        // A missing image in an existing owner directory is loss, not a new
        // install. Do not silently erase a previously protected account.
        let initial = !dir.exists();
        let loaded = (|| {
            fs::create_dir_all(&dir).map_err(|_| ())?;
            if initial {
                let disk = Disk::default();
                Self::write(&path, &disk)?;
                return Ok(disk);
            }
            let file = fs::File::open(&path).map_err(|_| ())?;
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| ())?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(());
            }
            let disk: Disk = serde_json::from_slice(&bytes).map_err(|_| ())?;
            if disk.schema != 1
                || disk.accounts.len() > MAX_ACCOUNTS
                || disk.accounts.iter().any(|(id, account)| {
                    !valid_id(id)
                        || account.last_order > disk.next_order
                        || account.windows.len() > MAX_WINDOWS
                        || account.windows.keys().any(|name| duration(name).is_none())
                })
            {
                return Err(());
            }
            Ok(disk)
        })();
        let (disk, failed) = match loaded {
            Ok(disk) => (disk, false),
            Err(()) => (Disk::default(), true),
        };
        if failed {
            tracing::warn!("outside_usage_state_unavailable");
        }
        Self {
            path,
            started_at,
            epoch: Arc::new(()),
            inner: Mutex::new(Inner {
                disk,
                failed: Arc::new(AtomicBool::new(failed)),
            }),
            bindings: Mutex::new(None),
            publication: Mutex::new(()),
        }
    }

    fn write(path: &Path, disk: &Disk) -> Result<(), ()> {
        let bytes = serde_json::to_vec(disk).map_err(|_| ())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(());
        }
        crate::bindings::atomic_write(path, &bytes).map_err(|_| ())
    }

    fn save(&self, inner: &mut Inner) -> bool {
        if inner.failed.load(Ordering::Acquire) {
            return false;
        }
        if Self::write(&self.path, &inner.disk).is_err() {
            inner.failed.store(true, Ordering::Release);
            tracing::warn!("outside_usage_state_write_failed");
            return false;
        }
        true
    }

    fn ensure(inner: &mut Inner, id: &str) -> bool {
        if !valid_id(id)
            || (!inner.disk.accounts.contains_key(id) && inner.disk.accounts.len() >= MAX_ACCOUNTS)
        {
            inner.failed.store(true, Ordering::Release);
            return false;
        }
        inner.disk.accounts.entry(id.to_owned()).or_default();
        true
    }

    pub(super) fn attach(&self, table: &Arc<BindingTable>, ids: impl Iterator<Item = String>) {
        let failure = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            for id in ids {
                Self::ensure(&mut inner, &id);
            }
            inner.failed.clone()
        };
        table.attach_outside_use_failure(failure);
        *self.bindings.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(table));
        self.publish();
    }

    /// A physical-send fence only. No I/O or binding publication on this path.
    /// A cold restart independently taints current+next observed windows.
    pub(super) fn note_send(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !Self::ensure(&mut inner, id) {
            return;
        }
        let account = inner.disk.accounts.get_mut(id).expect("ensured account");
        match account.generation.checked_add(1) {
            Some(next) => account.generation = next,
            None => inner.failed.store(true, Ordering::Release),
        }
    }

    pub(super) fn capture(&self, id: &str, active: bool) -> Option<PollOrigin> {
        self.capture_at(id, Utc::now(), active)
    }

    fn capture_at(&self, id: &str, started_at: DateTime<Utc>, active: bool) -> Option<PollOrigin> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.failed.load(Ordering::Acquire) || !Self::ensure(&mut inner, id) {
            return None;
        }
        let Some(next) = inner.disk.next_order.checked_add(1) else {
            inner.failed.store(true, Ordering::Release);
            return None;
        };
        inner.disk.next_order = next;
        Some(PollOrigin {
            epoch: self.epoch.clone(),
            account: id.to_owned(),
            order: inner.disk.next_order,
            generation: inner.disk.accounts[id].generation,
            started_at,
            active,
        })
    }

    /// Called only for the actual successfully applied poll under Store's gate.
    /// Never publish into bindings while that transaction is held.
    pub(super) fn observe(&self, id: &str, reported: &Value, origin: PollOrigin, active: bool) {
        if !Arc::ptr_eq(&self.epoch, &origin.epoch) || origin.account != id {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.failed.load(Ordering::Acquire) || !Self::ensure(&mut inner, id) {
            return;
        }
        let account = inner.disk.accounts.get_mut(id).expect("ensured account");
        if origin.order <= account.last_order {
            return;
        }
        let previous = account.clone();
        account.last_order = origin.order;
        let parsed = project(reported);
        // Merge/replacement completeness is not per-window evidence. A valid
        // present window may advance even in a single-window Free poll. Missing
        // windows invalidate their own continuity and retain their marks.
        let valid = parsed.is_some();
        let parsed = parsed.unwrap_or_default();
        let names: BTreeSet<_> = parsed.iter().map(|(name, _, _, _)| name.clone()).collect();
        let missing: Vec<_> = account
            .windows
            .iter()
            .filter(|(name, _)| !names.contains(*name))
            .map(|(name, tracked)| (name.clone(), tracked.window, None, origin.started_at))
            .collect();
        let mut exceeded = false;
        for (name, window, pct, observed_at) in parsed.into_iter().chain(missing) {
            if !account.windows.contains_key(&name) && account.windows.len() >= MAX_WINDOWS {
                exceeded = true;
                break;
            }
            let known = valid
                && pct.is_some()
                && window
                    .start
                    .is_some_and(|start| start >= self.started_at && origin.started_at >= start)
                && observed_at >= origin.started_at
                && origin.generation == account.generation;
            let tracked = account
                .windows
                .entry(name)
                .or_insert_with(|| TrackedWindow {
                    window,
                    detector: Detector::default(),
                });
            tracked.window = window;
            tracked.detector.observe(Observation {
                window,
                used_pct: pct,
                observed_at,
                order: origin.order,
                own: OwnTraffic {
                    generation: account.generation,
                    coverage_known: known,
                    active_or_unknown: origin.active || active,
                },
            });
        }
        if exceeded {
            inner.failed.store(true, Ordering::Release);
        }
        if !self.save(&mut inner) {
            // Failed persistence must never clear an already observed mark,
            // including a single window while another remains protected.
            let account = inner.disk.accounts.get_mut(id).expect("ensured account");
            for (name, old) in previous.windows {
                if old.detector.is_marked()
                    && account
                        .windows
                        .get(&name)
                        .is_none_or(|new| !new.detector.is_marked())
                {
                    account.windows.insert(name, old);
                }
            }
        }
    }

    /// Positive attribution only; unknown state is a separate protection flag.
    pub(super) fn used(&self, id: &str) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.disk.accounts.get(id).is_some_and(marked)
    }

    pub(super) fn protected(&self, id: &str) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.failed.load(Ordering::Acquire) || inner.disk.accounts.get(id).is_some_and(marked)
    }

    pub(super) fn publish(&self) {
        // Serialize publications, then collect the latest view. An older
        // post-Store publisher can never undo a newer protection/clear.
        let _publication = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        let Some(table) = self
            .bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
        else {
            return;
        };
        let (unknown, rows): (_, Vec<_>) = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            (
                inner.failed.load(Ordering::Acquire),
                inner
                    .disk
                    .accounts
                    .iter()
                    .map(|(id, account)| (id.clone(), marked(account)))
                    .collect(),
            )
        };
        if unknown {
            table.set_outside_use_unknown(true);
        }
        for (id, protected) in rows {
            table.set_outside_use(&id, protected);
        }
        if !unknown {
            table.set_outside_use_unknown(false);
        }
    }
}

impl super::AccountRuntime {
    /// Call immediately before every possible upstream data send, including
    /// retries and opaque WS data. Control frames and quota GETs are not sends.
    pub fn note_own_usage_send(&self, id: &str) {
        self.outside_owner.note_send(id);
    }

    /// Startup attachment precedes listener/poller publication. The weak link
    /// creates no runtime/table ownership cycle.
    pub fn attach_outside_bindings(&self, table: Arc<BindingTable>) {
        let ids = match self.store.list() {
            Ok(accounts) => accounts
                .into_iter()
                .map(|account| account.id)
                .collect::<Vec<_>>(),
            Err(_) => {
                self.outside_owner
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .failed
                    .store(true, Ordering::Release);
                Vec::new()
            }
        };
        self.outside_owner.attach(&table, ids.into_iter());
    }

    /// Observed outside-use attribution, suitable for the dashboard label.
    pub fn outside_used(&self, id: &str) -> bool {
        self.outside_owner.used(id)
    }

    /// Placement protection also includes unavailable durable evidence.
    pub fn outside_protected(&self, id: &str) -> bool {
        self.outside_owner.protected(id)
    }

    pub(crate) fn capture_outside_poll_origin(&self, id: &str) -> Option<PollOrigin> {
        let active = self
            .inflight
            .lock()
            .map(|counts| counts.get(id).copied().unwrap_or(0) > 0)
            .unwrap_or(true);
        self.outside_owner.capture(id, active)
    }

    /// Called only for a complete Codex poll response by the C9 producer.
    /// Raw native classification and exact mint/acquire remain under Store.
    pub(crate) async fn apply_polled_quota_snapshot_observed(
        &self,
        id: &str,
        snapshot: Value,
        reported: &Value,
        wait_origin: Option<super::ReportOrigin>,
        outside_origin: Option<PollOrigin>,
    ) -> Result<
        (
            crate::accounts::Account,
            Option<super::wait_owner::ObservedUsagePoll>,
        ),
        (),
    > {
        self.apply_quota_snapshot_reported_inner(
            id,
            snapshot,
            reported,
            None,
            (
                wait_origin,
                super::wait_owner::ReportPublication::ImmediatePollReceipt,
            ),
            Some(PollApplication {
                origin: outside_origin,
            }),
        )
        .await
        .map(|(account, receipt)| {
            (
                account,
                receipt.and_then(|receipt| match receipt {
                    super::wait_owner::ReportReceipt::Poll(receipt) => Some(receipt),
                    super::wait_owner::ReportReceipt::Http(_) => None,
                }),
            )
        })
    }

    /// Poll-only raw report. The existing wait provenance/model stays intact.
    pub(crate) async fn apply_polled_quota_snapshot(
        &self,
        id: &str,
        snapshot: Value,
        reported: &Value,
        wait_origin: Option<super::ReportOrigin>,
        outside_origin: Option<PollOrigin>,
    ) -> Result<crate::accounts::Account, ()> {
        self.apply_quota_snapshot_reported_inner(
            id,
            snapshot,
            reported,
            None,
            (wait_origin, super::wait_owner::ReportPublication::Immediate),
            Some(PollApplication {
                origin: outside_origin,
            }),
        )
        .await
        .map(|(account, _)| account)
    }
}

fn marked(account: &AccountState) -> bool {
    account
        .windows
        .values()
        .any(|window| window.detector.protected_through().is_some())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128
}

fn duration(name: &str) -> Option<i64> {
    let name = match name {
        "7d_oi" | "7d_sonnet" => "7d",
        other => other,
    };
    let (number, unit) = name.split_at_checked(name.len().checked_sub(1)?)?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    number
        .parse::<i64>()
        .ok()?
        .checked_mul(multiplier)
        .filter(|n| *n > 0 && *n <= 366 * 86400)
}

type Projected = (String, Window, Option<f64>, DateTime<Utc>);

fn project(reported: &Value) -> Option<Vec<Projected>> {
    let windows = reported.get("windows")?.as_array()?;
    if windows.is_empty() || windows.len() > MAX_WINDOWS {
        return None;
    }
    let mut names = BTreeSet::new();
    let mut result = Vec::with_capacity(windows.len());
    for raw in windows {
        let name = raw["name"].as_str()?;
        let seconds = duration(name)?;
        if !names.insert(name) {
            return None;
        }
        let reset: DateTime<Utc> = raw["reset_at"].as_str()?.parse().ok()?;
        let start = reset.checked_sub_signed(TimeDelta::seconds(seconds))?;
        let at = raw["observed_at"].as_str()?.parse().ok()?;
        let used = raw["used_pct"]
            .as_f64()
            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p));
        result.push((
            name.to_owned(),
            Window {
                start: Some(start),
                reset,
                duration_secs: seconds,
            },
            used,
            at,
        ));
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "cx-123abc";

    fn at(window: i64, offset: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + window * 10 + offset, 0).unwrap()
    }

    fn raw(window: i64, offset: i64, pct: f64) -> Value {
        json!({"windows":[{
            "name":"10s", "used_pct":pct,
            "reset_at":at(window + 1,0), "observed_at":at(window,offset)
        }]})
    }

    struct Fixture {
        home: tempfile::TempDir,
        owner: OutsideOwner,
    }

    impl Fixture {
        fn new() -> Self {
            let base = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/n10/outside-owner-fixtures");
            fs::create_dir_all(&base).unwrap();
            let home = tempfile::tempdir_in(base).unwrap();
            let owner = OutsideOwner::new_at(home.path(), at(0, 1));
            Self { home, owner }
        }

        fn sample(&self, window: i64, offset: i64, pct: f64, active: bool) {
            let origin = self
                .owner
                .capture_at(ID, at(window, offset), active)
                .unwrap();
            self.owner
                .observe(ID, &raw(window, offset, pct), origin, active);
        }

        fn baseline(&self) {
            self.sample(0, 2, 0., false);
            self.sample(1, 1, 0., false);
            self.sample(2, 1, 0., false);
            assert!(!self.owner.used(ID));
        }

        fn mark(&self) {
            self.baseline();
            self.sample(2, 2, 3., false);
            assert!(self.owner.used(ID));
        }
    }

    fn runtime(f: &Fixture) -> (Arc<super::super::AccountRuntime>, String) {
        let store = Arc::new(crate::accounts::AccountStore::acquire(f.home.path()).unwrap());
        let account = crate::accounts::health::tests::fixture();
        let id = account.id.clone();
        store
            .save(
                &account,
                &crate::accounts::Tokens {
                    access_token: "synthetic-access".into(),
                    refresh_token: "synthetic-refresh".into(),
                    id_token: None,
                },
                None,
            )
            .unwrap();
        let obs = Arc::new(crate::observability::Observability::new(f.home.path(), None).unwrap());
        let mut runtime = super::super::AccountRuntime::new(store, obs, 0.);
        Arc::get_mut(&mut runtime).unwrap().outside_owner =
            Arc::new(OutsideOwner::new_at(f.home.path(), at(0, 1)));
        (runtime, id)
    }

    fn codex_wire(
        now: DateTime<Utc>,
        seconds: i64,
        pct: f64,
        secondary: Option<(i64, f64)>,
    ) -> Value {
        let window = |seconds, pct| {
            let elapsed = (now - at(0, 0)).num_seconds().rem_euclid(seconds);
            json!({"used_percent":pct, "limit_window_seconds":seconds,
                "reset_after_seconds":seconds-elapsed})
        };
        json!({"plan_type":if secondary.is_some() {"plus"} else {"free"},
            "rate_limit":{"primary_window":window(seconds,pct),
                "secondary_window":secondary.map(|(seconds,pct)|window(seconds,pct))}})
    }

    async fn apply_wire(
        runtime: &super::super::AccountRuntime,
        id: &str,
        wire: Value,
        now: DateTime<Utc>,
    ) -> bool {
        let origin = runtime.outside_owner.capture_at(id, now, false).unwrap();
        let bytes = serde_json::to_vec(&wire).unwrap();
        let reported =
            crate::quota::parse_snapshot_for_test(crate::accounts::Provider::Codex, &bytes, now)
                .unwrap();
        let complete =
            crate::quota::is_complete_snapshot_for_test(crate::accounts::Provider::Codex, &bytes);
        let snapshot = runtime
            .obs
            .observe_poll(id, reported.clone(), complete)
            .unwrap();
        runtime
            .apply_polled_quota_snapshot(id, snapshot, &reported, None, Some(origin))
            .await
            .unwrap();
        complete
    }

    async fn cap_failure_blocks_native_move(on_send: bool) {
        use crate::{
            accounts::{Identity, Provider, Tokens},
            bindings::{Candidate, Decision, DispatchAdmission, MoveObservations},
            config::RoutingConfig,
        };

        let f = Fixture::new();
        let (runtime, id) = runtime(&f);
        let table = BindingTable::new(f.home.path(), RoutingConfig::default()).unwrap();
        runtime.attach_outside_bindings(table.clone());
        let source = Candidate {
            account: runtime.store.load(&id).unwrap(),
            quota: None,
            inflight: 0,
        };
        let choose = |key: &str, candidate: &Candidate| {
            // A filtered healthy source is not forced-move evidence. Preserve
            // its snapshot and use a real pin owner for this explicit test move.
            let mut sources = vec![source.clone()];
            if candidate.account.id != source.account.id {
                sources.push(candidate.clone());
            }
            let pins = crate::debug_pin::DebugPins::default();
            pins.set(
                Provider::Codex,
                Some(key.to_owned()),
                candidate.account.id.clone(),
            );
            let proof = pins.selection(Provider::Codex, Some(key)).unwrap();
            table.choose_with_hold_at_admitted_proof(
                Provider::Codex,
                Some(key),
                None,
                None,
                std::slice::from_ref(candidate),
                &sources,
                MoveObservations::untracked(),
                Some(&candidate.account.id),
                0,
                false,
                false,
                Utc::now(),
                None,
                |_| Some(DispatchAdmission::ordinary().with_pin(Some(proof))),
            )
        };
        for key in ["codex:cap-existing", "codex:cap-control"] {
            match choose(key, &source) {
                Decision::Selected { lease, .. } => {
                    lease.dispatched();
                    drop(lease);
                }
                other => panic!("initial native source binding: {other:?}"),
            }
        }
        for index in 0..MAX_ACCOUNTS - 1 {
            assert!(
                runtime
                    .capture_outside_poll_origin(&format!("cx-cap-{index}"))
                    .is_some()
            );
        }
        assert_eq!(
            runtime
                .outside_owner
                .inner
                .lock()
                .unwrap()
                .disk
                .accounts
                .len(),
            MAX_ACCOUNTS
        );

        let mut account = source.account.clone();
        account.identity = Identity::Codex {
            user_id: None,
            account_id: "synthetic-cap-new-account".into(),
            email: "cap-target@example.invalid".into(),
        };
        account.id = account.identity.id();
        account.email = "cap-target@example.invalid".into();
        account.generation = 0;
        let target = Candidate {
            account: runtime
                .store
                .save(
                    &account,
                    &Tokens {
                        access_token: "synthetic-new-access".into(),
                        refresh_token: "synthetic-new-refresh".into(),
                        id_token: None,
                    },
                    None,
                )
                .unwrap(),
            quota: None,
            inflight: 0,
        };
        assert!(matches!(
            choose("codex:cap-control", &target),
            Decision::Selected { .. }
        ));
        assert!(!runtime.outside_protected(&target.account.id));

        // Invoke the real producer while native mutation admission and Store
        // are held. It must only latch failure, without I/O or binding reentry.
        let store_guard = runtime.store.transaction().unwrap();
        let retained = table.choose_with_hold_at_admitted(
            Provider::Codex,
            Some("codex:cap-existing"),
            None,
            None,
            std::slice::from_ref(&source),
            None,
            0,
            false,
            false,
            Utc::now(),
            |_| {
                if on_send {
                    runtime.note_own_usage_send(&target.account.id);
                } else {
                    assert!(
                        runtime
                            .capture_outside_poll_origin(&target.account.id)
                            .is_none()
                    );
                }
                true
            },
        );
        drop(store_guard);
        assert!(matches!(retained, Decision::Selected { .. }));
        drop(retained);
        assert!(runtime.outside_protected(&target.account.id));
        assert!(!runtime.outside_used(&target.account.id));
        assert!(
            matches!(choose("codex:cap-existing", &target), Decision::None),
            "real capture/send cap failure must synchronously veto native destination admission"
        );

        // With no outstanding fence, a successful raw application must still
        // retain the poll marker and publish unavailable-state protection.
        let origin = runtime.capture_outside_poll_origin(&id);
        assert!(origin.is_none());
        let snapshot = raw(0, 2, 0.);
        runtime
            .apply_polled_quota_snapshot(&id, snapshot.clone(), &snapshot, None, origin)
            .await
            .unwrap();
        assert!(matches!(
            choose("codex:cap-existing", &target),
            Decision::None
        ));
        assert!(matches!(
            choose("codex:cap-existing", &source),
            Decision::Selected { .. }
        ));
        assert!(matches!(
            choose("codex:cap-genuinely-new", &target),
            Decision::Selected { .. }
        ));
    }

    #[tokio::test]
    async fn n10_outside_owner_capture_cap_failure_blocks_native_destination_immediately() {
        cap_failure_blocks_native_move(false).await;
    }

    #[tokio::test]
    async fn n10_outside_owner_send_cap_failure_blocks_native_destination_without_binding_lock() {
        cap_failure_blocks_native_move(true).await;
    }

    #[tokio::test]
    async fn n10_outside_owner_free_30d_null_secondary_raw_poll_detects_and_recovers() {
        let f = Fixture::new();
        let (runtime, id) = runtime(&f);
        let seconds = 30 * 86400;
        let now = |window, offset| at(0, 0) + TimeDelta::seconds(window * seconds + offset);
        for (window, offset, pct) in [(0, 2, 0.), (1, 1, 0.), (2, 1, 0.)] {
            let at = now(window, offset);
            assert!(
                !apply_wire(&runtime, &id, codex_wire(at, seconds, pct, None), at).await,
                "Free evidence must not change quota merge completeness"
            );
        }
        assert!(!runtime.outside_used(&id));
        let at = now(2, 2);
        assert!(!apply_wire(&runtime, &id, codex_wire(at, seconds, 3., None), at).await);
        assert!(
            runtime.outside_used(&id),
            "a numeric Free 30d window must detect a rise despite null secondary"
        );
        let snapshot = runtime.obs.quota_snapshot(&id).unwrap();
        assert_eq!(snapshot["windows"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["windows"][0]["name"], "43200m");
        for offset in [1, 8] {
            let at = now(3, offset);
            assert!(!apply_wire(&runtime, &id, codex_wire(at, seconds, 0., None), at).await);
            assert!(runtime.outside_used(&id));
        }
        let at = now(4, 1);
        assert!(!apply_wire(&runtime, &id, codex_wire(at, seconds, 0., None), at).await);
        assert!(
            !runtime.outside_used(&id),
            "a full own-free observed Free window must permit durable recovery"
        );
        let restored = OutsideOwner::new_at(f.home.path(), now(4, 2));
        assert!(!restored.used(&id));
    }

    #[test]
    fn n10_outside_owner_cold_start_recovers_at_observed_windows_not_a_timer() {
        let f = Fixture::new();
        f.sample(0, 2, 10., false);
        f.sample(0, 3, 50., false);
        f.sample(1, 1, 0., false);
        f.sample(1, 2, 70., false);
        assert!(
            !f.owner.used(ID),
            "cold history must not mint an outside mark"
        );
        f.sample(2, 1, 10., false);
        f.sample(2, 2, 11., false);
        assert!(!f.owner.used(ID));
        f.sample(2, 3, 11.1, false);
        assert!(
            f.owner.used(ID),
            "a never-sent account must eventually detect outside use"
        );
    }

    #[test]
    fn n10_outside_owner_active_and_cancelled_sends_taint_then_recover() {
        let f = Fixture::new();
        f.baseline();
        // The same API fences typed, retry, image and opaque data sends; it
        // receives no body/type or terminal-success claim from the producer.
        let before = f.owner.capture_at(ID, at(2, 3), false).unwrap();
        f.owner.note_send(ID);
        f.owner.observe(ID, &raw(2, 4, 50.), before, false);
        assert!(
            !f.owner.used(ID),
            "a send/drop during the poll cannot look outside"
        );
        f.sample(3, 1, 80., true);
        f.sample(4, 1, 90., false);
        assert!(
            !f.owner.used(ID),
            "active work and its next window remain tainted"
        );
        f.sample(5, 1, 0., false);
        f.sample(5, 2, 3., false);
        assert!(
            f.owner.used(ID),
            "own-free observed windows must recover detection"
        );
    }

    #[test]
    fn n10_outside_owner_prebaseline_activity_is_not_zero_coverage() {
        let f = Fixture::new();
        f.sample(0, 2, 0., true);
        f.sample(1, 1, 10., true);
        f.sample(2, 1, 60., false);
        assert!(!f.owner.used(ID));
        f.sample(3, 1, 0., false);
        f.sample(3, 2, 3., false);
        assert!(f.owner.used(ID));
    }

    #[test]
    fn n10_outside_owner_old_or_foreign_poll_cannot_replace_newer_evidence() {
        let f = Fixture::new();
        f.baseline();
        let old = f.owner.capture_at(ID, at(2, 2), false).unwrap();
        f.sample(2, 3, 4., false);
        f.owner.observe(ID, &raw(2, 2, 0.), old, false);
        assert!(f.owner.used(ID));
        let wrong = f.owner.capture_at("cx-456def", at(3, 1), false).unwrap();
        f.owner.observe(ID, &raw(3, 1, 0.), wrong, false);
        assert!(f.owner.used(ID));
        let other = Fixture::new();
        let foreign = other.owner.capture_at(ID, at(3, 1), false).unwrap();
        f.owner.observe(ID, &raw(3, 1, 0.), foreign, false);
        assert!(f.owner.used(ID));
    }

    #[test]
    fn n10_outside_owner_restart_preserves_mark_until_a_later_clean_window() {
        let mut f = Fixture::new();
        f.mark();
        f.owner = OutsideOwner::new_at(f.home.path(), at(2, 3));
        assert!(f.owner.used(ID));
        f.sample(2, 4, 3., false);
        f.sample(3, 1, 0., false);
        assert!(f.owner.used(ID));
        f.sample(4, 1, 0., false);
        f.sample(4, 8, 0., false);
        assert!(f.owner.used(ID));
        f.sample(5, 1, 0., false);
        assert!(!f.owner.used(ID));
        let restored = OutsideOwner::new_at(f.home.path(), at(5, 2));
        assert!(
            !restored.used(ID),
            "only a durable clear may survive restart"
        );
    }

    #[tokio::test]
    async fn n10_outside_owner_partial_report_does_not_clear_saved_mark() {
        let f = Fixture::new();
        let (runtime, id) = runtime(&f);
        for (window, offset, pct) in [(0, 2, 0.), (2, 1, 0.), (4, 1, 0.), (4, 2, 3.)] {
            let now = at(window, offset);
            assert!(apply_wire(&runtime, &id, codex_wire(now, 10, 0., Some((20, pct))), now).await);
        }
        assert!(runtime.outside_used(&id));
        for (window, offset, pct) in [(6, 1, 0.), (7, 1, 0.), (8, 1, 0.), (8, 2, 3.)] {
            let now = at(window, offset);
            let mut partial = codex_wire(now, 10, pct, None);
            partial["plan_type"] = json!("plus");
            partial["rate_limit"]
                .as_object_mut()
                .unwrap()
                .remove("secondary_window");
            assert!(!apply_wire(&runtime, &id, partial, now).await);
            assert!(
                runtime.outside_used(&id),
                "a partial primary report must retain the omitted secondary mark"
            );
        }
        let inner = runtime.outside_owner.inner.lock().unwrap();
        let windows = &inner.disk.accounts[&id].windows;
        assert!(windows["20s"].detector.is_marked());
        assert!(
            windows["10s"].detector.is_marked(),
            "the valid present window may independently establish new evidence"
        );
        drop(inner);
        let merged = runtime.obs.quota_snapshot(&id).unwrap();
        assert!(
            merged["windows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|window| window["name"] == "20s"),
            "native partial-poll merge must still retain the omitted window"
        );
        assert!(OutsideOwner::new_at(f.home.path(), at(8, 3)).used(&id));
    }

    #[test]
    fn n10_outside_owner_failed_clear_preserves_mark_and_unknown_is_not_attribution() {
        let f = Fixture::new();
        f.mark();
        f.sample(3, 1, 0., false);
        f.sample(3, 8, 0., false);
        fs::remove_file(&f.owner.path).unwrap();
        fs::create_dir(&f.owner.path).unwrap();
        f.sample(4, 1, 0., false);
        assert!(
            f.owner.used(ID),
            "a failed durable clear must retain the old mark"
        );
        assert!(f.owner.protected("cx-new"));
        assert!(
            !f.owner.used("cx-new"),
            "state loss must not invent an outside label"
        );
        let restored = OutsideOwner::new_at(f.home.path(), at(4, 2));
        assert!(restored.protected("cx-new"));
        assert!(!restored.used("cx-new"));
    }

    #[test]
    fn n10_outside_owner_normalized_durations_and_projection_are_allowlisted() {
        for (name, seconds) in [
            ("10s", 10),
            ("60m", 3600),
            ("5h", 18000),
            ("7d", 604800),
            ("30d", 2592000),
        ] {
            assert_eq!(duration(name), Some(seconds));
        }
        for name in [
            "",
            "primary",
            "secondary",
            "0s",
            "-1m",
            "99999999999999999d",
        ] {
            assert_eq!(duration(name), None);
        }
        let f = Fixture::new();
        let mut snapshot = raw(0, 2, 0.);
        snapshot["credentials"] = json!("synthetic-not-for-disk");
        snapshot["windows"][0]["body"] = json!("synthetic-not-for-disk");
        let origin = f.owner.capture_at(ID, at(0, 2), false).unwrap();
        f.owner.observe(ID, &snapshot, origin, false);
        let bytes = fs::read(&f.owner.path).unwrap();
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("synthetic-not-for-disk")
        );
    }

    #[tokio::test]
    async fn n10_outside_owner_common_raw_poll_application_uses_native_inflight() {
        let f = Fixture::new();
        let (runtime, id) = runtime(&f);
        for (window, offset, pct) in [(0, 2, 0.), (1, 1, 0.), (2, 1, 0.)] {
            let snapshot = raw(window, offset, pct);
            let origin = runtime
                .outside_owner
                .capture_at(&id, at(window, offset), false)
                .unwrap();
            runtime
                .apply_polled_quota_snapshot(&id, snapshot.clone(), &snapshot, None, Some(origin))
                .await
                .unwrap();
        }
        let active = runtime.track_inflight(&id);
        runtime.note_own_usage_send(&id);
        let origin = runtime
            .outside_owner
            .capture_at(&id, at(2, 2), true)
            .unwrap();
        drop(active);
        let snapshot = raw(2, 3, 50.);
        runtime
            .apply_polled_quota_snapshot(&id, snapshot.clone(), &snapshot, None, Some(origin))
            .await
            .unwrap();
        assert!(
            !runtime.outside_used(&id),
            "native cancellation cannot certify zero own coverage"
        );
        let views = runtime.views().await.unwrap();
        let view = views.iter().find(|view| view["id"] == id).unwrap();
        assert_eq!(
            view["outside_used"],
            json!(false),
            "admin account views must emit an explicit false before outside attribution"
        );
        for (window, offset, pct) in [(3, 1, 50.), (4, 1, 0.), (4, 2, 3.)] {
            let snapshot = raw(window, offset, pct);
            let origin = runtime
                .outside_owner
                .capture_at(&id, at(window, offset), false)
                .unwrap();
            runtime
                .apply_polled_quota_snapshot(&id, snapshot.clone(), &snapshot, None, Some(origin))
                .await
                .unwrap();
        }
        assert!(
            runtime.outside_used(&id),
            "the common raw poll path must really publish a mark"
        );
        let views = runtime.views().await.unwrap();
        let view = views.iter().find(|view| view["id"] == id).unwrap();
        assert_eq!(
            view["outside_used"],
            json!(true),
            "admin account views must expose attribution from the common raw poll path"
        );
    }
}
