//! C7 placement and schema-1 affinity persistence. Callers filter tried accounts,
//! credentials, probes and volatile failures, but retain quota cooling for holds.
//! Move evidence is carried separately from those eligibility exclusions.
use crate::{
    accounts::{Account, Health, HealthReason, HealthSignal, Provider, UserState},
    config::RoutingConfig,
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

const CAPACITY: usize = 20_000;
const COUNT_CHANGE_CAPACITY: usize = CAPACITY * 2;
mod admission;
mod affinity;
pub(crate) use admission::DispatchAdmission;
mod capacity_spill;
mod launch;
mod move_guard;
pub(crate) mod quota_wait;
mod rebind;
mod selection;
pub use launch::{
    LaunchEntitlements, LaunchError, LaunchInfo, LaunchPlacement, LaunchPolicyMove, LaunchRequest,
};
pub use move_guard::Limits as MoveGuardLimits;
pub(crate) use move_guard::{Observation as MoveObservation, Observations as MoveObservations};
#[cfg(test)]
#[path = "bindings/move_guard_tests.rs"]
mod move_guard_tests;
#[cfg(test)]
#[path = "bindings/routing_safety_tests.rs"]
mod routing_safety_tests;

#[cfg(test)]
mod r7_snapshot_tests {
    use super::*;
    #[test]
    fn r7_legacy_binding_snapshot_preserves_metadata_without_counting_idle_pin_as_activity() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("state")).unwrap();
        let now = Utc::now();
        let key = "codex:r7-synthetic-root";
        fs::write(root.path().join("state/bindings.json"),serde_json::to_vec(&serde_json::json!({"schema":1,"bindings":[{"key":key,"account":"cx-123abc","last_used":now-chrono::Duration::seconds(1000)}]})).unwrap()).unwrap();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        let before = table.dashboard_snapshot(now);
        assert_eq!(before.len(), 1);
        assert!(!before[0].active);
        assert!(before[0].first_seen_at.is_none());
        assert!(before[0].client_kind.is_none());
        assert!(before[0].restored);
        let pin = table.pin(Some(key));
        let pinned = table.dashboard_snapshot(now);
        assert!(
            !pinned[0].active,
            "an idle socket protects affinity, not load"
        );
        assert_eq!(
            pinned.iter().filter(|row| row.active).count(),
            table.active_sessions("cx-123abc", now)
        );
        drop(pin);
        assert!(!table.dashboard_snapshot(now)[0].active);
    }
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub account: Account,
    pub quota: Option<Value>,
    pub inflight: usize,
}
/// Proof-free inputs to the optional HTTP wait inspection seam.
/// The request keeps its caller guard alive; this operation is never admitted here.
pub(crate) struct HttpWaitRequest<'a> {
    pub(crate) provider: Provider,
    pub(crate) key: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) expected_source: Option<&'a str>,
    pub(crate) operation: &'a crate::issue_operation::IssueOperation,
}

/// A coherent binding/policy view, not quota evidence or dispatch authority.
/// Only BindingTable constructs it. It cannot escape the inspection callback.
pub(crate) struct WaitIntent<'a> {
    provider: Provider,
    key: &'a str,
    account: &'a str,
    model: Option<&'a str>,
    policy_key: &'a str,
    existing_placement: bool,
    plan: &'a crate::routing::Plan,
    operation: &'a crate::issue_operation::IssueOperation,
}

impl WaitIntent<'_> {
    pub(crate) fn provider(&self) -> Provider {
        self.provider
    }
    pub(crate) fn key(&self) -> &str {
        self.key
    }
    pub(crate) fn account(&self) -> &str {
        self.account
    }
    pub(crate) fn model(&self) -> Option<&str> {
        self.model
    }
    pub(crate) fn policy_key(&self) -> &str {
        self.policy_key
    }
    pub(crate) fn existing_placement(&self) -> bool {
        self.existing_placement
    }
    pub(crate) fn plan(&self) -> &crate::routing::Plan {
        self.plan
    }
    pub(crate) fn deadline(&self) -> tokio::time::Instant {
        self.operation.deadline()
    }
    pub(crate) fn caller_current(&self) -> bool {
        self.operation.can_reply()
    }
}

#[derive(Debug)]
pub enum Decision {
    Blocked,
    MoveHeld {
        until: DateTime<Utc>,
    },
    Selected {
        account: String,
        lease: BindingLease,
        moves: Vec<MoveEvent>,
    },
    Hold {
        until: DateTime<Utc>,
    },
    None,
}
#[derive(Debug, PartialEq, Eq)]
pub enum PreviewDecision {
    Selected { account: String },
    Hold { until: DateTime<Utc> },
    None,
}
#[derive(Clone, Debug, Serialize)]
pub struct MoveEvent {
    pub key: String,
    pub from: Option<String>,
    pub to: String,
    pub reason: String,
    pub bytes: u64,
    pub keys: usize,
    pub debug_pin: bool,
    pub evidence: PlacementEvidence,
    pub placement_charge_pct: f64,
    pub capped: bool,
    pub placed_since_obs: usize,
    pub is_placement: bool,
}
/// Body-free refusal recorded before any admission or policy side effect.
#[derive(Clone, Debug, Serialize)]
pub struct MoveGuardPair {
    pub from: String,
    pub to: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct MoveGuardEvent {
    pub proposed_reason: String,
    pub move_list: Vec<MoveGuardPair>,
    pub key: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    pub until: DateTime<Utc>,
    pub newly_tripped: bool,
    pub forced: bool,
}
type MoveGuardObserver = Arc<dyn Fn(&MoveGuardEvent) + Send + Sync>;

/// Evidence at selection time, before this placement adds its charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementEvidence {
    Fresh,
    Weak,
}

/// A pending placement-driven poll. Reading deadlines does not consume them.
#[derive(Clone, Debug)]
pub struct PlacementPoll {
    pub account: String,
    pub placement_at: DateTime<Utc>,
    pub placement_seq: u64,
    pub due_at: DateTime<Utc>,
}

/// Capture under the binding lock just before sending. Cumulative placement
/// count makes arbitrarily delayed observations exact without retaining history.
#[derive(Clone, Copy, Debug)]
pub struct UsageObservationAsOf {
    pub sequence: u64,
    pub placed_count: u64,
}
#[derive(Clone)]
struct PlacementWindow {
    value: Value,
    asof: Option<UsageObservationAsOf>,
    reported_sequence: u64,
    unresolved_equal_charge: bool,
}
// Timer metadata only. Exact charge/cap counts use cumulative counters above.
// Coalescing retains the earliest time, so partial acknowledgments can make a
// poll early but never postpone it past the oldest remaining placement's due time.
const PLACEMENT_POLL_DEADLINES: usize = 64;
#[derive(Clone)]
struct PlacementPollDeadline {
    at: DateTime<Utc>,
    through: u64,
}
#[derive(Clone, Default)]
struct PlacementState {
    latest: Option<u64>,
    latest_at: Option<DateTime<Utc>>,
    total: u64,
    poll_deadlines: Vec<PlacementPollDeadline>,
    windows: BTreeMap<String, PlacementWindow>,
    window_set_sequence: Option<u64>,
}
impl PlacementState {
    fn observe(&mut self, snapshot: &Value) {
        // A cached/merged candidate must not resurrect a window removed by a
        // complete source-stamped poll. Subsequent updates need a send token.
        if self.window_set_sequence.is_some() {
            return;
        }
        if let Some(windows) = snapshot["windows"].as_array() {
            for w in windows {
                if !placement_account_window(w) {
                    continue;
                }
                let name = w["name"].as_str().expect("gating window");
                self.windows
                    .entry(name.to_owned())
                    .or_insert_with(|| PlacementWindow {
                        value: w.clone(),
                        asof: date(&w["observed_at"]).map(|_| UsageObservationAsOf {
                            sequence: 0,
                            placed_count: 0,
                        }),
                        reported_sequence: 0,
                        unresolved_equal_charge: false,
                    });
            }
        }
    }
    fn observe_asof(&mut self, snapshot: &Value, asof: UsageObservationAsOf) {
        self.observe_asof_set(snapshot, asof, false);
    }
    fn observe_complete_asof(&mut self, snapshot: &Value, asof: UsageObservationAsOf) {
        self.observe_asof_set(snapshot, asof, true);
    }
    fn observe_asof_set(&mut self, snapshot: &Value, asof: UsageObservationAsOf, complete: bool) {
        let Some(windows) = snapshot["windows"].as_array() else {
            return;
        };
        let equal_placement = self.latest == Some(asof.sequence) && self.total > 0;
        if complete
            && self
                .window_set_sequence
                .is_none_or(|sequence| asof.sequence >= sequence)
        {
            let reported: BTreeSet<_> = windows
                .iter()
                .filter(|window| placement_account_window(window))
                .filter_map(|window| window["name"].as_str())
                .collect();
            self.windows.retain(|name, window| {
                reported.contains(name.as_str()) || window.reported_sequence > asof.sequence
            });
            self.window_set_sequence = Some(asof.sequence);
        }
        for w in windows {
            if !placement_account_window(w) {
                continue;
            }
            let name = w["name"].as_str().expect("account window");
            if self
                .window_set_sequence
                .is_some_and(|sequence| asof.sequence < sequence)
                && !self.windows.contains_key(name)
            {
                continue;
            }
            let window = self
                .windows
                .entry(name.to_owned())
                .or_insert_with(|| PlacementWindow {
                    value: w.clone(),
                    asof: None,
                    reported_sequence: asof.sequence,
                    unresolved_equal_charge: false,
                });
            window.reported_sequence = window.reported_sequence.max(asof.sequence);
            if date(&w["observed_at"]).is_some()
                && window.asof.is_none_or(|old| asof.sequence >= old.sequence)
            {
                window.value = w.clone();
                if window.asof.is_none_or(|old| asof.sequence > old.sequence) {
                    // Equality leaves this window's charge unresolved even
                    // after another placement changes the account's latest seq.
                    // Replays of the same proof retain its original count/flag.
                    window.unresolved_equal_charge = equal_placement;
                    window.asof = Some(asof);
                }
            }
        }
        let oldest_asof = if self.windows.is_empty()
            || self.windows.values().any(|window| window.asof.is_none())
        {
            0
        } else {
            self.windows
                .values()
                .filter_map(|window| window.asof.map(|token| token.sequence))
                .min()
                .unwrap_or(0)
        };
        // Equality does not acknowledge a placement. A partially covered
        // coalesced deadline retains its original earliest time.
        self.poll_deadlines
            .retain(|deadline| deadline.through >= oldest_asof);
    }
    fn oldest_count(&self) -> u64 {
        if self.windows.is_empty() || self.windows.values().any(|w| w.asof.is_none()) {
            return 0;
        }
        self.windows
            .values()
            .filter_map(|w| w.asof.map(|at| at.placed_count))
            .min()
            .unwrap_or(0)
            .min(self.total)
    }
    fn weak(&self, now: DateTime<Utc>) -> bool {
        self.windows.is_empty()
            || self.windows.values().any(|w| {
                w.asof.is_none()
                    || self
                        .latest
                        .is_some_and(|placed| w.asof.is_none_or(|at| at.sequence < placed))
                    || date(&w.value["reset_at"]).is_some_and(|reset| {
                        reset <= now && date(&w.value["observed_at"]).is_none_or(|at| at < reset)
                    })
            })
    }
    fn count(&self) -> usize {
        usize::try_from(self.total - self.oldest_count()).unwrap_or(usize::MAX)
    }
    fn charge_count(&self, name: &str) -> usize {
        let window = self.windows.get(name);
        let asof = window.and_then(|w| w.asof);
        let covered = asof.map_or(0, |at| at.placed_count.min(self.total));
        // Equal is not newer: cap count excludes the equal placement while its
        // charge remains. Real send tokens are distinct from placement events.
        let equal = window.is_some_and(|w| w.unresolved_equal_charge);
        usize::try_from((self.total - covered).saturating_add(u64::from(equal)))
            .unwrap_or(usize::MAX)
    }
    fn placed(&mut self, now: DateTime<Utc>, seq: u64) {
        self.total = self
            .total
            .checked_add(1)
            .expect("placement count exhausted");
        self.latest = Some(seq);
        self.latest_at = Some(now);
        if let Some(last) = self.poll_deadlines.last_mut()
            && last.at == now
        {
            last.through = seq;
        } else {
            self.poll_deadlines.push(PlacementPollDeadline {
                at: now,
                through: seq,
            });
        }
        if self.poll_deadlines.len() > PLACEMENT_POLL_DEADLINES {
            let first = self.poll_deadlines.remove(0);
            self.poll_deadlines[0].at = self.poll_deadlines[0].at.min(first.at);
        }
    }
    fn poll_started(&mut self, asof: u64) {
        self.poll_deadlines
            .retain(|deadline| deadline.through > asof);
    }
}

use affinity::Binding;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    schema: u32,
    bindings: Vec<Binding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    launches: BTreeMap<String, launch::Metadata>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    launch_anchors: BTreeMap<String, launch::Anchor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    launch_aliases: BTreeMap<String, String>,
}
#[derive(Clone)]
struct Group {
    keys: BTreeSet<String>,
    account: String,
    last_used: DateTime<Utc>,
}
#[derive(Clone)]
struct OverloadMark {
    expires: DateTime<Utc>,
    from: String,
    target: String,
}
#[derive(Clone, Default)]
struct AccountIndex {
    count: usize,
    groups: usize,
    recent: BTreeSet<(DateTime<Utc>, String)>,
}
/// Memory-only revoke fence for an exact adopted managed owner root.
pub(crate) struct DeferredBindingFence {
    root: String,
    anchor: String,
    account: String,
    model: String,
    revoked: Arc<crate::account_runtime::deferred_reset::Revocation>,
}
impl DeferredBindingFence {
    pub(crate) fn live(&self) -> bool {
        self.revoked.live()
    }
    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.revoked.notified_raw()
    }
    fn matches(&self, data: &Data) -> bool {
        launch::deferred_matches(data, &self.root, &self.anchor, &self.account, &self.model)
            && BindingTable::accepted_in(data, Some(&self.root), &self.account)
    }
}
#[derive(Clone, Default)]
struct Data {
    deferred_fences: Vec<std::sync::Weak<DeferredBindingFence>>,
    bindings: BTreeMap<String, Binding>,
    launches: BTreeMap<String, launch::Metadata>,
    launch_anchors: BTreeMap<String, launch::Anchor>,
    // Process-local activity only; never serialized or used as dispatch authority.
    launch_activity: BTreeMap<String, launch::Activity>,
    launch_aliases: BTreeMap<String, String>,
    launch_retention: BTreeMap<String, DateTime<Utc>>,
    launch_weight: usize,
    active: BTreeMap<String, usize>,
    pins: BTreeMap<String, usize>,
    pending: BTreeMap<String, usize>,
    traffic: BTreeMap<String, DateTime<Utc>>,
    sticky: BTreeMap<String, String>,
    accepted_unbound: BTreeSet<String>,
    epochs: BTreeMap<String, Arc<()>>,
    snapshot_weight: usize,
    after_move: BTreeSet<String>,
    rate_hits: BTreeMap<String, BTreeMap<String, DateTime<Utc>>>,
    overload_marks: BTreeMap<String, OverloadMark>,
    successes: BTreeMap<String, DateTime<Utc>>,
    placements: BTreeMap<String, PlacementState>,
    #[cfg(test)]
    capacity_spills: BTreeMap<String, u64>,
    outside_used: BTreeSet<String>,
    outside_unknown: bool,
    outside_failure: Option<Arc<AtomicBool>>,
    last_request_bytes: BTreeMap<String, u64>,
    unknown_cost: BTreeSet<String>,
    mark_history: BTreeMap<String, Vec<DateTime<Utc>>>,
    pacing: BTreeMap<String, bool>,
    overload_reported: BTreeMap<String, DateTime<Utc>>,
    groups: BTreeMap<String, Group>,
    lru: BTreeSet<(DateTime<Utc>, String)>,
    accounts: BTreeMap<String, AccountIndex>,
    // Memory-only history, including accounts whose last binding was removed.
    count_changes: BTreeMap<String, DateTime<Utc>>,
    count_change_order: BTreeSet<(DateTime<Utc>, String)>,
    count_before: BTreeMap<String, usize>,
    // prune starts launch mutations; commit_binding also supplies its own clock.
    count_change_clock: Option<DateTime<Utc>>,
    defer_count_changes: bool,
    dirty: bool,
    revision: u64,
}
impl Data {
    fn count_before_change(&mut self, id: &str) {
        let count = self.accounts.get(id).map_or(0, |a| a.count);
        self.count_before.entry(id.to_owned()).or_insert(count);
    }

    fn commit_count_changes(&mut self) {
        if self.defer_count_changes || self.count_before.is_empty() {
            return;
        }
        let now = self.count_change_clock.unwrap_or_else(Utc::now);
        for (id, before) in std::mem::take(&mut self.count_before) {
            if before == self.accounts.get(&id).map_or(0, |a| a.count) {
                continue;
            }
            if let Some(old) = self.count_changes.insert(id.clone(), now) {
                self.count_change_order.remove(&(old, id.clone()));
            }
            self.count_change_order.insert((now, id));
        }
        while self.count_changes.len() > COUNT_CHANGE_CAPACITY {
            if let Some((_, id)) = self.count_change_order.pop_first() {
                self.count_changes.remove(&id);
            }
        }
    }

    fn changed(&mut self) {
        let fences = std::mem::take(&mut self.deferred_fences);
        for weak in fences {
            if let Some(fence) = weak.upgrade() {
                if !fence.matches(self) {
                    fence.revoked.cancel();
                }
                self.deferred_fences.push(weak);
            }
        }
        self.commit_count_changes();
        self.dirty = true;
        self.revision = self.revision.wrapping_add(1);
    }
}
pub struct BindingTable {
    pub policies: crate::routing::PolicyStore,
    path: PathBuf,
    config: RoutingConfig,
    thresholds: Mutex<BTreeMap<String, usize>>,
    data: Mutex<Data>,
    writer: Mutex<()>,
    mutations: Mutex<()>,
    #[cfg(test)]
    wait_before_publication: Mutex<
        Option<(
            std::sync::mpsc::SyncSender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
    move_guard: Mutex<move_guard::Breaker>,
    move_guard_observer: Mutex<Option<MoveGuardObserver>>,
    move_accounting: Mutex<Option<std::sync::Weak<crate::observability::Observability>>>,
    writes_stopped: AtomicBool,
    unknown_cost_skips: AtomicUsize,
    usage_sequence: AtomicU64,
}

impl BindingTable {
    pub fn attach_move_accounting(&self, observability: &Arc<crate::observability::Observability>) {
        *self
            .move_accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(observability));
    }
    fn move_observability(&self) -> Option<Arc<crate::observability::Observability>> {
        self.move_accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }
    fn rename_move_state(
        &self,
        source: &str,
        target: &str,
        now: DateTime<Utc>,
    ) -> Result<(), LaunchError> {
        let mut guard = self.move_guard.lock().unwrap_or_else(|e| e.into_inner());
        let mut proposed = guard.clone();
        proposed
            .rename(source, target)
            .map_err(|_| LaunchError::Capacity)?;
        if let Some(obs) = self.move_observability() {
            obs.accounting_alias(Provider::Codex, source, target, now)
                .map_err(|_| LaunchError::Capacity)?;
        }
        *guard = proposed;
        Ok(())
    }
    pub fn clear_move_guard(&self) {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        self.move_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.policies.changed.notify_waiters();
    }

    pub fn move_guard_until(&self) -> Option<DateTime<Utc>> {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        self.move_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state(Utc::now())
    }

    pub fn set_move_guard_observer(&self, observer: MoveGuardObserver) {
        *self
            .move_guard_observer
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(observer);
    }

    /// All calls occur under mutations, before external admission/dispatch.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn prepare_move(
        &self,
        d: &Data,
        key: &str,
        from: Option<&str>,
        to: &str,
        candidates: &[Candidate],
        observations: MoveObservations,
        model: Option<&str>,
        proposed_reason: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<move_guard::Permit>, move_guard::Denied> {
        self.prepare_move_with_source(
            d,
            key,
            from,
            to,
            from.and_then(|id| candidates.iter().find(|c| c.account.id == id)),
            candidates,
            None,
            observations,
            model,
            proposed_reason,
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_move_with_source(
        &self,
        d: &Data,
        key: &str,
        from: Option<&str>,
        to: &str,
        source: Option<&Candidate>,
        candidates: &[Candidate],
        sole_target: Option<&selection::SoleEligibleTarget>,
        observations: MoveObservations,
        model: Option<&str>,
        proposed_reason: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<move_guard::Permit>, move_guard::Denied> {
        let Some(from) = from.filter(|from| *from != to) else {
            return Ok(None);
        };
        let target = candidates.iter().find(|c| c.account.id == to);
        let source_account = source.map(|c| effective_account(c, d, &self.config, model, now));
        let target_account = target.map(|c| effective_account(c, d, &self.config, model, now));
        // Genuinely missing source evidence cannot grant the fresh-unusable
        // exception. A health poll timestamp is not proof.
        let usable = source.is_some_and(|c| self.move_source_usable(d, key, c, model, now));
        let unusable = source_account.as_ref().and_then(|account| {
            crate::accounts::health::UnusableEvidence::observe(
                account,
                model,
                Self::accepted_in(d, Some(key), &account.id),
                now,
            )
        });
        let result = self
            .move_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prepare_observed(
                &self.config.move_guard,
                key,
                from,
                to,
                semantic_evidence(source_account.as_ref()),
                semantic_evidence(target_account.as_ref()),
                unusable.as_ref(),
                sole_target.is_some_and(|proof| proof.applies_to(from, to, model, now)),
                observations,
                now,
            );
        match result {
            Ok(permit) => Ok(Some(permit)),
            Err(denied) => {
                let event = MoveGuardEvent {
                    proposed_reason: proposed_reason.into(),
                    move_list: self
                        .move_guard
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .recent_moves(now),
                    key: key.into(),
                    from: from.into(),
                    to: to.into(),
                    reason: denied.reason.code().into(),
                    until: denied.until,
                    newly_tripped: denied.newly_tripped,
                    forced: !usable,
                };
                let observer = self
                    .move_guard_observer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(observer) = observer {
                    observer(&event);
                }
                tracing::warn!(code="binding_move_refused", reason=?denied.reason, from, to, "Account move refused by token guard");
                Err(denied)
            }
        }
    }
    fn move_source_usable(
        &self,
        d: &Data,
        key: &str,
        candidate: &Candidate,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> bool {
        effective_account(candidate, d, &self.config, model, now).eligible_at(model, now)
            && (!candidate.account.drain || Self::accepted_in(d, Some(key), &candidate.account.id))
    }
}

fn semantic_evidence(candidate: Option<&Account>) -> u64 {
    use sha2::{Digest, Sha256};
    let Some(c) = candidate else {
        return 0;
    };
    // Only stable usability/reset facts; exclude token generation, poll time,
    // utilization noise and quota started_at.
    let reset = c.quota_cooling.as_ref().map(|q| (&q.window, q.reset_at));
    let value = serde_json::to_vec(&(
        &c.user_state,
        &c.health,
        c.payment_until,
        c.drain,
        c.allow_credits,
        reset,
    ))
    .expect("account health serializes");
    u64::from_be_bytes(Sha256::digest(value)[..8].try_into().unwrap())
}

/// A reservation belongs to the actual upstream account, even after a forced
/// remap. Completion never restores an old account into the binding table.
pub struct BindingLease {
    table: Arc<BindingTable>,
    group: Option<String>,
    account: String,
    epoch_key: String,
    epoch: Arc<()>,
    pub after_move: bool,
    defer: bool,
}
impl std::fmt::Debug for BindingLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindingLease")
            .field("account", &self.account)
            .field("after_move", &self.after_move)
            .finish()
    }
}
impl BindingLease {
    /// Linearize accounting with move commits and resolve the live canonical owner.
    /// A reservation selected before a move cannot claim that move's next turn.
    pub(crate) fn accounting_begin(
        &self,
        provider: Provider,
        account: &str,
        model: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<
        Option<crate::observability::reread_accounting::RequestTicket>,
        crate::observability::reread_accounting::Error,
    > {
        let _mutation = self
            .table
            .mutations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d = self.table.data.lock().unwrap_or_else(|e| e.into_inner());
        let Some(key) = self.group.as_deref() else {
            return Ok(None);
        };
        let canonical = launch::ownership_group(&d, group(key));
        let epoch_key = launch::ownership_group(&d, &self.epoch_key);
        if account != self.account
            || canonical != epoch_key
            || !canonical.starts_with(match provider {
                Provider::Claude => "claude:",
                Provider::Codex => "codex:",
            })
            || !d
                .epochs
                .get(epoch_key)
                .is_some_and(|epoch| Arc::ptr_eq(epoch, &self.epoch))
            || d.groups
                .get(canonical)
                .is_none_or(|entry| entry.account != self.account)
        {
            return Ok(None);
        }
        let Some(obs) = self.table.move_observability() else {
            return Ok(None);
        };
        obs.accounting_begin(provider, canonical, account, model, at)
            .map(Some)
    }

    /// Final admission is separate from attempt/ledger accounting.
    pub fn receipt(&self) -> DispatchReceipt {
        DispatchReceipt {
            table: self.table.clone(),
            group: self.group.clone(),
            account: self.account.clone(),
            epoch_key: self.epoch_key.clone(),
            epoch: self.epoch.clone(),
        }
    }
    pub fn dispatched(&self) {
        self.receipt().dispatched();
    }
    pub fn successful(&self) {
        self.table.note_success(&self.account, Utc::now());
    }
    /// Upgrade handshakes have no cache usage. Carry this marker to response.create.
    pub fn defer_after_move(&mut self) {
        self.defer = true;
    }
}
#[derive(Clone)]
pub struct DispatchReceipt {
    table: Arc<BindingTable>,
    group: Option<String>,
    account: String,
    epoch_key: String,
    epoch: Arc<()>,
}
impl DispatchReceipt {
    pub fn dispatched(&self) {
        let mut d = self.table.data.lock().unwrap_or_else(|e| e.into_inner());
        let epoch_key = launch::ownership_group(&d, &self.epoch_key).to_owned();
        if !d
            .epochs
            .get(&epoch_key)
            .is_some_and(|epoch| Arc::ptr_eq(epoch, &self.epoch))
        {
            return;
        }
        let Some(group) = &self.group else {
            if d.sticky.values().any(|id| id == &self.account) {
                d.accepted_unbound.insert(self.account.clone());
            }
            return;
        };
        let group = launch::ownership_group(&d, group);
        let Some(entry) = d.groups.get(group).filter(|g| g.account == self.account) else {
            return;
        };
        let keys = entry.keys.clone();
        let mut changed = false;
        for key in keys {
            if let Some(binding) = d.bindings.get_mut(&key)
                && binding.accepted == Some(false)
            {
                binding.accepted = Some(true);
                changed = true;
            }
        }
        if changed {
            d.changed();
        }
    }
}
impl Drop for BindingLease {
    fn drop(&mut self) {
        let _mutation = self
            .table
            .mutations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut d = self.table.data.lock().unwrap_or_else(|e| e.into_inner());
        decrement(&mut d.pending, &self.account);
        let now = Utc::now();
        if let Some(group) = &self.group {
            let group = launch::ownership_group(&d, group).to_owned();
            decrement(&mut d.active, &group);
            touch_group(&mut d, &group, now);
            release_group(&mut d, &group);
            if self.defer && self.after_move {
                d.after_move.insert(group.clone());
            }
        }
    }
}
pub struct SocketPin {
    table: Arc<BindingTable>,
    group: Option<String>,
}
impl Drop for SocketPin {
    fn drop(&mut self) {
        if let Some(group) = &self.group {
            let _mutation = self
                .table
                .mutations
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut d = self.table.data.lock().unwrap_or_else(|e| e.into_inner());
            let group = launch::ownership_group(&d, group).to_owned();
            decrement(&mut d.pins, &group);
            launch::touch(&mut d, &group, Utc::now());
            release_group(&mut d, &group);
        }
    }
}
fn group(key: &str) -> &str {
    if key.starts_with("claude:") {
        key.split_once(":agent:").map_or(key, |(root, _)| root)
    } else {
        key
    }
}
fn decrement(map: &mut BTreeMap<String, usize>, key: &str) {
    if let Some(n) = map.get_mut(key) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            map.remove(key);
        }
    }
}
fn protected(d: &Data, g: &str) -> bool {
    d.active.contains_key(g) || d.pins.contains_key(g)
}
// Index changes visit only the affected group, never the binding table.
fn unindex_group(d: &mut Data, g: &str) -> Option<Group> {
    let entry = d.groups.remove(g)?;
    d.count_before_change(&entry.account);
    d.lru.remove(&(entry.last_used, g.to_owned()));
    if let Some(account) = d.accounts.get_mut(&entry.account) {
        account.count -= entry.keys.len();
        account.recent.remove(&(entry.last_used, g.to_owned()));
        if account.count == 0 {
            d.accounts.remove(&entry.account);
        }
        if let Some(account) = d.accounts.get_mut(&entry.account) {
            account.groups = account.groups.saturating_sub(1);
        }
    }
    Some(entry)
}
fn index_group(d: &mut Data, g: String, entry: Group) {
    d.count_before_change(&entry.account);
    if !protected(d, &g) {
        d.lru.insert((entry.last_used, g.clone()));
    }
    let account = d.accounts.entry(entry.account.clone()).or_default();
    account.count += entry.keys.len();
    account.groups += 1;
    account.recent.insert((entry.last_used, g.clone()));
    d.groups.insert(g, entry);
}
fn release_group(d: &mut Data, g: &str) {
    if !protected(d, g)
        && let Some(entry) = d.groups.get(g)
    {
        d.lru.insert((entry.last_used, g.to_owned()));
    }
}
fn protect_group(d: &mut Data, g: &str) {
    if let Some(entry) = d.groups.get(g) {
        d.lru.remove(&(entry.last_used, g.to_owned()));
    }
}
fn touch_group(d: &mut Data, g: &str, now: DateTime<Utc>) {
    launch::touch(d, g, now);
    if let Some(mut entry) = unindex_group(d, g) {
        entry.last_used = entry.last_used.max(now);
        for key in &entry.keys {
            if let Some(b) = d.bindings.get_mut(key) {
                b.last_used = entry.last_used;
            }
        }
        index_group(d, g.to_owned(), entry);
        d.changed();
    }
}
fn binding_bound(key: &str, account: &str) -> usize {
    // Exact escaped identifier lengths plus a conservative fixed metadata budget.
    serde_json::to_string(key)
        .expect("JSON string")
        .len()
        .saturating_add(serde_json::to_string(account).expect("JSON string").len())
        .saturating_add(384)
}

fn rebuild_indexes(d: &mut Data) {
    d.groups.clear();
    d.lru.clear();
    d.accounts.clear();
    d.snapshot_weight = d
        .bindings
        .values()
        .map(|b| binding_bound(&b.key, &b.account))
        .fold(0usize, usize::saturating_add);
    launch::rebuild(d);
    let mut groups = BTreeMap::<String, Group>::new();
    for b in d.bindings.values() {
        let entry = groups
            .entry(group(&b.key).to_owned())
            .or_insert_with(|| Group {
                keys: BTreeSet::new(),
                account: b.account.clone(),
                last_used: b.last_used,
            });
        entry.keys.insert(b.key.clone());
        entry.last_used = entry.last_used.max(b.last_used);
    }
    for (g, entry) in groups {
        index_group(d, g, entry);
    }
    // Recovery reconstructs indexes, rather than committing live mutations.
    d.count_before.clear();
}
fn snapshot(d: &Data) -> Snapshot {
    Snapshot {
        schema: 1,
        bindings: d.bindings.values().cloned().collect(),
        launches: d.launches.clone(),
        launch_anchors: d.launch_anchors.clone(),
        launch_aliases: d.launch_aliases.clone(),
    }
}
const SNAPSHOT_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) fn read_binding_snapshot(path: &Path) -> std::io::Result<Vec<u8>> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid binding snapshot trust or size",
        )
    };
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > SNAPSHOT_MAX_BYTES
    {
        return Err(invalid());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid());
        }
        crate::local_secrets::restrict(path).map_err(std::io::Error::other)?;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let actual = file.metadata()?;
    if !actual.is_file() || actual.len() > SNAPSHOT_MAX_BYTES {
        return Err(invalid());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if actual.file_attributes() & 0x400 != 0 {
            return Err(invalid());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if actual.uid() != unsafe { libc::geteuid() } || actual.mode() & 0o077 != 0 {
            return Err(invalid());
        }
    }
    let mut bytes = Vec::new();
    file.take(SNAPSHOT_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > SNAPSHOT_MAX_BYTES {
        return Err(invalid());
    }
    Ok(bytes)
}
#[derive(Debug, thiserror::Error)]
#[error("existing binding snapshot exceeds restore capacity; original retained")]
struct SnapshotCapacity;
#[cfg(test)]
fn decode(value: Value) -> Result<Data> {
    decode_scoped(value, false)
}
fn decode_scoped(value: Value, strict: bool) -> Result<Data> {
    let state: Snapshot = serde_json::from_value(value)?;
    if state.schema != 1 {
        bail!("unsupported binding schema");
    }
    if state.bindings.len() > CAPACITY * 2 {
        return Err(SnapshotCapacity.into());
    }
    let mut d = Data::default();
    let mut groups = BTreeMap::new();
    let mut inconsistent = BTreeSet::new();
    for b in state.bindings {
        if b.key.is_empty()
            || crate::observability::safe_identifier(&b.account).is_none()
            || d.bindings.contains_key(&b.key)
        {
            bail!("invalid binding");
        }
        let g = group(&b.key).to_owned();
        if let Some(previous) = groups.insert(g.clone(), b.account.clone())
            && previous != b.account
        {
            inconsistent.insert(g);
        }
        d.bindings.insert(b.key.clone(), b);
    }
    d.launches = state.launches;
    d.launch_anchors = state.launch_anchors;
    d.launch_aliases = state.launch_aliases;
    launch::validate(&d)?;
    if !inconsistent.is_empty() {
        if strict {
            bail!("strict binding group has conflicting accounts");
        }
        tracing::warn!(
            groups = inconsistent.len(),
            "Dropping inconsistent binding groups"
        );
        d.bindings
            .retain(|key, _| !inconsistent.contains(group(key)));
        launch::remove_missing(&mut d);
        d.changed();
    }
    rebuild_indexes(&mut d);
    d.unknown_cost = d.groups.keys().cloned().collect();
    if d.snapshot_weight as u64 + 64 > SNAPSHOT_MAX_BYTES {
        return Err(SnapshotCapacity.into());
    }
    Ok(d)
}
// Constructible only by the binding owner while its mutation gate is held.
// This is first-association evidence, never dispatch or movement authority.
pub(crate) struct ManagedFirstProject<'a> {
    key: &'a str,
    account: &'a str,
    _gate: std::marker::PhantomData<&'a ()>,
}
impl<'a> ManagedFirstProject<'a> {
    fn new(key: &'a str, account: &'a str, _gate: &'a std::sync::MutexGuard<'_, ()>) -> Self {
        Self {
            key,
            account,
            _gate: std::marker::PhantomData,
        }
    }
    pub(crate) fn key(&self) -> &str {
        self.key
    }
    pub(crate) fn account(&self) -> &str {
        self.account
    }
}

type DashboardRoutingSnapshot = BTreeMap<
    crate::dashboard::SessionId,
    Option<(
        crate::dashboard::AccountId,
        crate::dashboard::SessionRouting,
    )>,
>;

impl BindingTable {
    fn accepted_in(d: &Data, key: Option<&str>, id: &str) -> bool {
        if key.is_none() {
            return d.accepted_unbound.contains(id) && d.sticky.values().any(|sticky| sticky == id);
        }
        key.and_then(|key| d.groups.get(group(key)))
            .is_some_and(|g| {
                g.account == id
                    && g.keys
                        .iter()
                        .any(|k| d.bindings.get(k).is_some_and(|b| b.accepted != Some(false)))
            })
    }
    pub(crate) fn existing_placement(&self, key: Option<&str>, id: &str) -> bool {
        Self::accepted_in(
            &self.data.lock().unwrap_or_else(|e| e.into_inner()),
            key,
            id,
        )
    }
    pub fn associate_project(&self, key: &str, project: &str) -> Result<()> {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let anchor = d.groups.get(group(key)).map(|g| g.account.clone());
        let policy_key = launch::policy_key(&d, Some(key)).unwrap().to_owned();
        let managed_first =
            d.launches.contains_key(group(key)) && launch::idle_placement(&d, group(key));
        drop(d);
        if managed_first && let Some(account) = anchor.as_deref() {
            return self.policies.associate_managed_first_project(
                ManagedFirstProject::new(&policy_key, account, &_mutation),
                project,
            );
        }
        self.policies
            .associate_with_anchor(&policy_key, project, anchor.as_deref())
    }
    pub fn routing_view(&self) -> Value {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let mut view = self.policies.list();
        view["observed_sessions"]=serde_json::to_value(d.groups.iter().map(|(key,g)|serde_json::json!({"id":crate::routing::session_id(key),"account":g.account,"policy":self.policies.plan(launch::policy_key(&d, Some(key)))})).collect::<Vec<_>>()).unwrap_or_default();
        view
    }
    pub fn set_routing_session(
        &self,
        id: &str,
        settings: Option<crate::routing::Settings>,
        project: Option<&str>,
        inherit: bool,
    ) -> Result<()> {
        self.set_routing_session_patch(id, settings.map(Into::into), project, inherit)
    }
    pub fn set_routing_session_patch(
        &self,
        id: &str,
        settings: Option<crate::routing::SettingsEdit>,
        project: Option<&str>,
        inherit: bool,
    ) -> Result<()> {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let binding = d
            .groups
            .iter()
            .find(|(key, _)| crate::routing::session_id(key) == id)
            .map(|(key, g)| {
                (
                    launch::policy_key(&d, Some(key)).unwrap().to_owned(),
                    g.account.clone(),
                )
            });
        drop(d);
        let policy_id = binding
            .as_ref()
            .map(|(key, _)| crate::routing::session_id(key));
        self.policies.edit_session_patch_for_key(
            policy_id.as_deref().unwrap_or(id),
            settings,
            project,
            binding.as_ref().map(|(_, a)| a.as_str()),
            inherit,
            binding.as_ref().map(|(k, _)| k.as_str()),
        )
    }
    /// Authenticated launch intent for an exact native Claude session identity.
    /// This creates no observed binding and grants no dispatch authority.
    pub fn register_claude_urgent(&self, session_id: &str) -> Result<()> {
        if session_id.len() != 36
            || !session_id.bytes().enumerate().all(|(i, byte)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
        {
            anyhow::bail!("invalid native Claude launch identity");
        }
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let key = format!("claude:{session_id}");
        self.policies.set_session_urgent_for_key(
            &crate::routing::session_id(&key),
            crate::routing::On5hLimit::Move,
            Some(&key),
        )
    }
    pub fn set_routing_session_urgent(
        &self,
        id: &str,
        on_5h_limit: crate::routing::On5hLimit,
    ) -> Result<()> {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let binding = d
            .groups
            .iter()
            .find(|(key, _)| crate::routing::session_id(key) == id)
            .map(|(key, g)| {
                (
                    launch::policy_key(&d, Some(key)).unwrap().to_owned(),
                    g.account.clone(),
                )
            });
        drop(d);
        let policy_id = binding
            .as_ref()
            .map(|(key, _)| crate::routing::session_id(key));
        self.policies.set_session_urgent_for_key(
            policy_id.as_deref().unwrap_or(id),
            on_5h_limit,
            binding.as_ref().map(|(key, _)| key.as_str()),
        )
    }
    pub fn new(home: &Path, config: RoutingConfig) -> Result<Arc<Self>> {
        config.validate()?;
        let path = home.join("state/bindings.json");
        fs::create_dir_all(path.parent().context("binding state directory")?)?;
        let policies = crate::routing::PolicyStore::new(
            home,
            crate::routing::Settings {
                mode: config.mode,
                reset_wait_secs: config.reset_hold_secs,
                on_5h_limit: crate::routing::On5hLimit::Wait,
            },
        )?
        .with_limits(config.churn.clone())?;
        let data = match read_binding_snapshot(&path) {
            Ok(bytes) => match crate::json_unique::from_slice(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(|value| decode_scoped(value, policies.requires_strict_recovery()))
            {
                Ok(data) => data,
                Err(error) => {
                    if error.downcast_ref::<SnapshotCapacity>().is_some() {
                        return Err(error);
                    }
                    if policies.requires_strict_recovery() {
                        return Err(error.context(
                            "strict binding recovery failed; original snapshot retained",
                        ));
                    }
                    let quarantine = path.with_file_name(format!(
                        "bindings.json.corrupt-{}-{:08x}",
                        Utc::now().timestamp_millis(),
                        rand::random::<u32>()
                    ));
                    fs::rename(&path, quarantine).context("quarantine corrupt bindings")?;
                    tracing::warn!("Corrupt bindings quarantined; starting empty");
                    Data::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(e.into()),
        };
        // Validate/capture before constructing an auto-flushing table. A failed
        // legacy alias recovery must not prune or overwrite the source on Drop.
        policies.capture_strict_anchors(data.groups.iter().map(|(key, g)| {
            (
                launch::policy_key(&data, Some(key)).unwrap(),
                g.account.as_str(),
            )
        }))?;
        let table = Arc::new(Self {
            policies,
            path,
            config,
            thresholds: Mutex::new(BTreeMap::new()),
            data: Mutex::new(data),
            writer: Mutex::new(()),
            mutations: Mutex::new(()),
            #[cfg(test)]
            wait_before_publication: Mutex::new(None),
            move_guard: Mutex::new(move_guard::Breaker::default()),
            move_guard_observer: Mutex::new(None),
            move_accounting: Mutex::new(None),
            writes_stopped: AtomicBool::new(false),
            unknown_cost_skips: AtomicUsize::new(0),
            usage_sequence: AtomicU64::new(0),
        });
        table.prune(
            &mut table.data.lock().unwrap_or_else(|e| e.into_inner()),
            Utc::now(),
        );
        let weak = Arc::downgrade(&table);
        std::thread::Builder::new()
            .name("qb-bindings".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    let Some(table) = weak.upgrade() else {
                        break;
                    };
                    if let Err(e) = table.flush() {
                        tracing::warn!(error = %e, "Binding write-behind failed; will retry");
                    }
                }
            })?;
        Ok(table)
    }
    fn prune(&self, d: &mut Data, now: DateTime<Utc>) {
        d.count_change_clock = Some(now);
        let deferred = d.defer_count_changes;
        d.defer_count_changes = true;
        launch::prune(d, now);
        while let Some((time, g)) = d
            .lru
            .iter()
            .find(|(_, g)| !launch::retained(d, g, now))
            .cloned()
        {
            if d.bindings.len() <= CAPACITY
                && idle(now, time) < self.config.session_binding_max_secs
            {
                break;
            }
            if let Some(entry) = unindex_group(d, &g) {
                for key in entry.keys {
                    if let Some(binding) = d.bindings.remove(&key) {
                        d.snapshot_weight = d
                            .snapshot_weight
                            .saturating_sub(binding_bound(&binding.key, &binding.account));
                    }
                }
                d.after_move.remove(&g);
                d.overload_marks.remove(&g);
                d.last_request_bytes.remove(&g);
                d.unknown_cost.remove(&g);
                d.epochs.remove(&g);
                for hits in d.rate_hits.values_mut() {
                    hits.remove(&g);
                }
                launch::remove_group(d, &g);
                d.changed();
            }
        }
        d.defer_count_changes = deferred;
        d.commit_count_changes();
    }
    fn dashboard_row(
        &self,
        d: &Data,
        g: &str,
        now: DateTime<Utc>,
    ) -> Option<crate::dashboard::BindingGroup> {
        let entry = d.groups.get(g)?;
        let metadata = entry
            .keys
            .iter()
            .filter_map(|k| d.bindings.get(k))
            .min_by_key(|b| b.first_seen_at);
        Some(crate::dashboard::BindingGroup {
            id: crate::dashboard::SessionId::from_key(g),
            provider: if g.starts_with("claude:") {
                Provider::Claude
            } else {
                Provider::Codex
            },
            account: entry.account.clone().try_into().ok()?,
            first_seen_at: metadata.and_then(|b| b.first_seen_at),
            bound_since: metadata.and_then(|b| b.bound_since),
            last_used_at: entry.last_used,
            active: (entry.last_used
                >= now - chrono::Duration::seconds(self.config.active_window_secs as i64)
                && !launch::idle_placement(d, g))
                || d.active.contains_key(g),
            marked_for_move: d.overload_marks.contains_key(g),
            client_kind: metadata.and_then(|b| b.client_kind),
            restored: metadata.is_none_or(|b| !b.observed_here),
        })
    }
    /// Projection only. Never derive mutation authority from dashboard12.
    pub(crate) fn dashboard_routing_snapshot(&self) -> DashboardRoutingSnapshot {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows = BTreeMap::new();
        for (key, group) in &d.groups {
            let Ok(account) = crate::dashboard::AccountId::try_from(group.account.clone()) else {
                continue;
            };
            let plan = self.policies.plan(launch::policy_key(&d, Some(key)));
            let row = (
                account,
                crate::dashboard::SessionRouting {
                    action_id: crate::routing::session_id(key),
                    on_5h_limit: plan.settings.on_5h_limit,
                    mode: plan.settings.mode,
                },
            );
            // A short-display collision must not choose an arbitrary root.
            rows.entry(crate::dashboard::SessionId::from_key(key))
                .and_modify(|slot| *slot = None)
                .or_insert(Some(row));
        }
        rows
    }
    /// Resolve aliases/anchors read-only for an already-admitted wait's view.
    /// This observes identity; it does not validate a proof or reserve a lease.
    pub(crate) fn dashboard_quota_wait_row(
        &self,
        key: &str,
        source: &str,
    ) -> Option<crate::dashboard::BindingGroup> {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let requested = group(key);
        let canonical = if d.groups.contains_key(requested) {
            requested
        } else {
            d.launch_aliases
                .get(requested)
                .map_or_else(|| launch::ownership_group(&d, requested), String::as_str)
        };
        if d.groups.get(canonical)?.account != source {
            return None;
        }
        self.dashboard_row(&d, canonical, Utc::now())
    }
    pub(crate) fn dashboard_snapshot(
        &self,
        now: DateTime<Utc>,
    ) -> Vec<crate::dashboard::BindingGroup> {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        d.groups
            .keys()
            .filter_map(|g| self.dashboard_row(&d, g, now))
            .collect()
    }
    pub(crate) fn dashboard_group(
        &self,
        key: &str,
        now: DateTime<Utc>,
    ) -> Option<crate::dashboard::BindingGroup> {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.dashboard_row(&d, group(key), now)
    }
    pub(crate) fn dashboard_lookup_candidates(
        &self,
        ids: &BTreeSet<crate::dashboard::SessionId>,
    ) -> Vec<(crate::dashboard::SessionId, String)> {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        d.groups
            .keys()
            .filter_map(|key| {
                let sid = key.strip_prefix("claude:")?;
                if sid.is_empty()
                    || sid.len() > 128
                    || !sid
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                {
                    return None;
                }
                let id = crate::dashboard::SessionId::from_key(key);
                ids.contains(&id).then(|| (id, sid.to_owned()))
            })
            .take(64)
            .collect()
    }
    pub(crate) fn dashboard_client(&self, key: &str, kind: crate::dashboard::ClientKind) {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let keys = d
            .groups
            .get(group(key))
            .map(|entry| entry.keys.clone())
            .unwrap_or_default();
        let mut changed = false;
        for key in keys {
            if let Some(binding) = d.bindings.get_mut(&key)
                && binding.client_kind != Some(kind)
            {
                binding.client_kind = Some(kind);
                changed = true;
            }
        }
        if changed {
            d.changed();
        }
    }

    pub fn count(&self) -> usize {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut d, Utc::now());
        d.bindings.len()
    }
    #[cfg(test)]
    fn has_traffic(&self, id: &str) -> bool {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        d.traffic.contains_key(id) || d.pending.get(id).is_some_and(|count| *count > 0)
    }
    pub fn account_stats(&self, id: &str) -> (usize, Option<DateTime<Utc>>) {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut d, Utc::now());
        d.accounts.get(id).map_or((0, None), |a| {
            (a.count, a.recent.last().map(|(time, _)| *time))
        })
    }

    /// Last committed change to account_stats(id).0, including changes back to
    /// an earlier count. Read-only: does not prune or acknowledge the change.
    /// History starts cold on restart and retains the latest bounded records.
    pub fn account_count_changed_at(&self, id: &str) -> Option<DateTime<Utc>> {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .count_changes
            .get(id)
            .copied()
    }

    pub fn active_sessions(&self, id: &str, now: DateTime<Utc>) -> usize {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.active_sessions_in(&d, id, now)
    }

    fn active_sessions_in(&self, d: &Data, id: &str, now: DateTime<Utc>) -> usize {
        let Some(account) = d.accounts.get(id) else {
            return 0;
        };
        let cutoff = now - chrono::Duration::seconds(self.config.active_window_secs as i64);
        account
            .recent
            .iter()
            .filter(|(last, group)| {
                (*last >= cutoff && !launch::idle_placement(d, group))
                    || d.active.contains_key(group)
            })
            .count()
    }

    pub fn spread_threshold(&self) -> usize {
        self.config.spread_threshold
    }

    pub fn set_account_threshold(&self, id: &str, threshold: Option<usize>) {
        let mut thresholds = self.thresholds.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(threshold) = threshold {
            thresholds.insert(id.to_owned(), threshold.max(1));
        } else {
            thresholds.remove(id);
        }
    }

    pub fn spread_threshold_for(&self, id: &str) -> usize {
        self.thresholds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .copied()
            .unwrap_or(self.config.spread_threshold)
    }

    pub fn note_client_rate(
        &self,
        provider: Provider,
        account: &str,
        key: Option<&str>,
        candidates: &[Candidate],
        now: DateTime<Utc>,
    ) -> (usize, usize, usize, usize) {
        let Some(key) = key else { return (0, 0, 0, 0) };
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let group = group(key).to_owned();
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = now - chrono::Duration::seconds(self.config.overload_window_secs as i64);
        let bound: BTreeSet<_> = d
            .groups
            .iter()
            .filter(|(_, g)| g.account == account)
            .map(|(k, _)| k.clone())
            .collect();
        let hit_count = {
            let hits = d.rate_hits.entry(account.to_owned()).or_default();
            hits.retain(|key, at| *at >= cutoff && bound.contains(key));
            if bound.contains(&group) {
                hits.insert(group, now);
            }
            hits.len()
        };
        let active = self.active_sessions_in(&d, account, now);
        let threshold = self.spread_threshold_for(account);
        let mut targets: Vec<_> = candidates
            .iter()
            .filter(|c| {
                c.account.id != account
                    && c.account.provider == provider
                    && self.rebalance_target_base(&d, account, c, None, now)
            })
            .map(|c| (c, metrics(c, &d, &self.config, now)))
            .collect();
        prefer_non_pacing(&mut targets, &d);
        targets.retain(|(c, _)| !placement_capped(c, &d, &self.config, now));
        let anchor = targets.iter().filter_map(|(_, m)| m.weekly).min();
        targets.sort_by(|(a, x), (b, y)| compare_gated(a, x, b, y, anchor));
        let Some((choice, _)) = targets.first() else {
            return (active, threshold, hit_count, 0);
        };
        let target = choice.account.id.clone();
        let target_active =
            self.active_sessions_in(&d, &target, now) + self.reserved_in(&d, &target, None, now);
        let target_threshold = self.spread_threshold_for(&target);
        if hit_count < 3 || active <= threshold || active.saturating_sub(target_active) < 2 {
            return (active, threshold, hit_count, 0);
        }
        d.overload_marks.retain(|_, mark| mark.expires > now);
        let history = d.mark_history.entry(account.to_owned()).or_default();
        history.retain(|at| *at > cutoff);
        let already = history.len();
        let budget = 2usize.saturating_sub(already);
        let imbalance_budget = active.saturating_sub(target_active).saturating_sub(2) / 2;
        let (evidence, placements) = placement_evidence(choice, &d, now);
        let confidence_budget = if evidence == PlacementEvidence::Weak {
            self.config
                .weak_evidence_new_sessions
                .saturating_sub(placements.saturating_add(self.reserved_in(&d, &target, None, now)))
        } else {
            usize::MAX
        };
        let excess = active
            .saturating_sub(threshold)
            .min(budget)
            .min(imbalance_budget)
            .min(target_threshold.saturating_sub(target_active))
            .min(confidence_budget);
        let skipped = d
            .groups
            .iter()
            .filter(|(g, entry)| {
                entry.account == account
                    && !d.active.contains_key(*g)
                    && !d.overload_marks.contains_key(*g)
                    && entry.last_used
                        >= now - chrono::Duration::seconds(self.config.active_window_secs as i64)
                    && d.unknown_cost.contains(*g)
            })
            .count();
        self.unknown_cost_skips
            .fetch_add(skipped, Ordering::Relaxed);
        let mut victims: Vec<_> = d
            .groups
            .iter()
            .filter(|(g, entry)| {
                entry.account == account
                    && !d.active.contains_key(*g)
                    && !d.overload_marks.contains_key(*g)
                    && entry.last_used
                        >= now - chrono::Duration::seconds(self.config.active_window_secs as i64)
                    && !d.unknown_cost.contains(*g)
                    && d.last_request_bytes.get(*g).is_some_and(|bytes| {
                        *bytes > 0 && *bytes <= self.config.churn.max_move_bytes
                    })
                    && self.policies.discretionary_allowed(
                        launch::policy_key(&d, Some(g)).unwrap(),
                        Some(&target),
                        now,
                    )
            })
            .map(|(g, entry)| {
                (
                    g.clone(),
                    d.last_request_bytes.get(g).copied().unwrap_or(u64::MAX),
                    entry.last_used,
                )
            })
            .collect();
        victims.sort_by(|(_, bytes_a, last_a), (_, bytes_b, last_b)| {
            bytes_a.cmp(bytes_b).then_with(|| last_a.cmp(last_b))
        });
        victims.truncate(excess);
        let victims: Vec<_> = victims.into_iter().map(|(g, _, _)| g).collect();
        for victim in &victims {
            d.overload_marks.insert(
                victim.clone(),
                OverloadMark {
                    expires: now
                        + chrono::Duration::seconds(self.config.overload_window_secs as i64),
                    from: account.to_owned(),
                    target: target.clone(),
                },
            );
        }
        d.mark_history
            .entry(account.to_owned())
            .or_default()
            .extend(std::iter::repeat_n(now, victims.len()));
        (active, threshold, hit_count, victims.len())
    }

    fn has_overload_mark(&self, d: &Data, key: &str, now: DateTime<Utc>) -> bool {
        let g = group(key);
        d.overload_marks
            .get(g)
            .is_some_and(|mark| mark.expires > now)
    }
    pub fn overload_marked(&self, key: Option<&str>, now: DateTime<Utc>) -> bool {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        key.is_some_and(|key| {
            !d.active.contains_key(group(key)) && self.has_overload_mark(&d, key, now)
        })
    }
    fn reserved_in(&self, d: &Data, id: &str, except: Option<&str>, now: DateTime<Utc>) -> usize {
        d.overload_marks
            .iter()
            .filter(|(g, mark)| {
                Some(g.as_str()) != except && mark.target == id && mark.expires > now
            })
            .count()
    }
    fn rebalance_target_waiting(
        &self,
        d: &Data,
        source: &str,
        c: &Candidate,
        except: Option<&str>,
        now: DateTime<Utc>,
    ) -> bool {
        let m = metrics(c, d, &self.config, now);
        let target = self.active_sessions_in(d, &c.account.id, now)
            + self.reserved_in(d, &c.account.id, except, now);
        let source_count = self.active_sessions_in(d, source, now);
        let require_balance =
            except.is_none_or(|g| d.overload_marks.get(g).is_some_and(|m| m.expires > now));
        !c.account.drain
            && effective_account(c, d, &self.config, None, now).eligible_at(None, now)
            && m.gate
            && !m.unknown
            && (m.weekly.is_some() || m.five.is_some())
            && target < self.spread_threshold_for(&c.account.id)
            && (!require_balance || target.saturating_add(2) <= source_count)
    }
    fn rebalance_target_base(
        &self,
        d: &Data,
        source: &str,
        c: &Candidate,
        except: Option<&str>,
        now: DateTime<Utc>,
    ) -> bool {
        // Pacing remains the ordinary C7 preference, not a hard move veto.
        self.rebalance_target_waiting(d, source, c, except, now)
    }
    fn rebalance_target(
        &self,
        d: &Data,
        source: &str,
        c: &Candidate,
        except: Option<&str>,
        now: DateTime<Utc>,
    ) -> bool {
        self.rebalance_target_base(d, source, c, except, now)
            && !placement_capped(c, d, &self.config, now)
    }
    pub fn overload_report_due(&self, account: &str, now: DateTime<Utc>) -> bool {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let due = d.overload_reported.get(account).is_none_or(|at| {
            *at + chrono::Duration::seconds(self.config.overload_window_secs as i64) <= now
        });
        if due {
            d.overload_reported.insert(account.to_owned(), now);
        }
        due
    }
    pub fn set_pacing(&self, id: &str, pacing: bool) {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pacing
            .insert(id.to_owned(), pacing);
    }
    pub fn request_bytes(&self, key: Option<&str>, bytes: u64) {
        if bytes == 0 {
            return;
        }
        if let Some(key) = key {
            let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
            if d.groups.contains_key(group(key)) {
                // Only a complete root input establishes group-wide cost.
                if key == group(key) {
                    d.unknown_cost.remove(group(key));
                }
                if d.unknown_cost.contains(group(key)) {
                    return;
                }
                let entry = d
                    .last_request_bytes
                    .entry(group(key).to_owned())
                    .or_default();
                *entry = (*entry).max(bytes);
            }
        }
    }
    pub fn unknown_request_cost(&self, key: Option<&str>) {
        if let Some(key) = key {
            let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
            if d.groups.contains_key(group(key)) {
                d.unknown_cost.insert(group(key).to_owned());
            }
        }
    }
    pub fn note_success(&self, id: &str, now: DateTime<Utc>) {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .successes
            .insert(id.to_owned(), now);
    }
    /// Compatibility hook: rate observations no longer impose destination quiet.
    pub fn note_rate(&self, _id: &str, _now: DateTime<Utc>) {}
    /// Capture immediately before an upstream request/event-producing send.
    /// Tokens and placements share one process-local monotonic sequence.
    pub fn begin_usage_observation(&self) -> u64 {
        self.usage_sequence
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .expect("usage sequence exhausted")
    }
    /// Exact typed token. The lock makes token/count capture atomic with placement.
    pub fn begin_usage_observation_for(&self, id: &str) -> UsageObservationAsOf {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let sequence = self.begin_usage_observation();
        let placed_count = data.placements.entry(id.to_owned()).or_default().total;
        UsageObservationAsOf {
            sequence,
            placed_count,
        }
    }
    /// Only newly reported fields; never pass borrowed merged windows.
    pub fn note_usage_observation_asof_token(
        &self,
        id: &str,
        snapshot: &Value,
        asof: UsageObservationAsOf,
    ) {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .placements
            .entry(id.to_owned())
            .or_default()
            .observe_asof(snapshot, asof);
    }
    /// A complete poll replaces older window names while preserving reports
    /// newer than its send token. Headers/events remain partial through note_*.
    pub fn replace_usage_windows_asof_token(
        &self,
        id: &str,
        snapshot: &Value,
        asof: UsageObservationAsOf,
    ) {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .placements
            .entry(id.to_owned())
            .or_default()
            .observe_complete_asof(snapshot, asof);
    }
    /// Compatibility only: old sequence-only callers remain conservative for
    /// late observations. Production senders should use the exact typed API.
    pub fn note_usage_observation_asof(&self, id: &str, snapshot: &Value, sequence: u64) {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let state = data.placements.entry(id.to_owned()).or_default();
        let placed_count = if state.latest.is_none_or(|latest| sequence >= latest) {
            state.total
        } else {
            0
        };
        state.observe_asof(
            snapshot,
            UsageObservationAsOf {
                sequence,
                placed_count,
            },
        );
    }
    pub fn note_placement_poll_started(&self, id: &str, asof: u64) {
        if let Some(state) = self
            .data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .placements
            .get_mut(id)
        {
            state.poll_started(asof);
        }
    }
    /// Reconcile a merged candidate without inventing send-time provenance.
    pub fn note_usage_observation(&self, id: &str, snapshot: &Value) {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .placements
            .entry(id.to_owned())
            .or_default()
            .observe(snapshot);
    }
    /// One descriptor per account; later placements never postpone the oldest.
    pub fn due_placement_polls(&self, now: DateTime<Utc>) -> Vec<PlacementPoll> {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .placements
            .iter()
            .filter_map(|(id, state)| {
                placement_poll(id, state, now, None, self.config.placement_poll_after_secs)
            })
            .collect()
    }
    /// Bounded per-account timer lookup; never scans other accounts.
    pub fn due_placement_poll(
        &self,
        id: &str,
        now: DateTime<Utc>,
        polled_through: Option<u64>,
    ) -> Option<PlacementPoll> {
        let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        placement_poll(
            id,
            data.placements.get(id)?,
            now,
            polled_through,
            self.config.placement_poll_after_secs,
        )
    }
    pub fn take_unknown_cost_skips(&self) -> usize {
        self.unknown_cost_skips.swap(0, Ordering::Relaxed)
    }
    pub fn pending_count(&self, id: &str) -> usize {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .get(id)
            .copied()
            .unwrap_or(0)
    }
    pub fn current(&self, key: Option<&str>) -> Option<String> {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut d, Utc::now());
        key.and_then(|k| {
            d.bindings.get(k).or_else(|| {
                d.groups
                    .get(group(k))
                    .and_then(|g| g.keys.first())
                    .and_then(|key| d.bindings.get(key))
            })
        })
        .map(|b| b.account.clone())
    }

    /// Source identities to check in the store, including a durable anchor
    /// whose live binding was pruned. This list itself grants no authority.
    pub(crate) fn recovery_sources(
        &self,
        provider: Provider,
        key: Option<&str>,
    ) -> BTreeSet<String> {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let mut sources = BTreeSet::new();
        if let Some(key) = key {
            if let Some(anchor) = self.policies.plan(launch::policy_key(&d, Some(key))).anchor {
                sources.insert(anchor);
            }
            if let Some(entry) = d.groups.get(group(key)) {
                sources.insert(entry.account.clone());
            }
        } else if let Some(source) = d.sticky.get(provider.as_str()) {
            sources.insert(source.clone());
        }
        sources
    }
    #[cfg(test)]
    pub(crate) fn fill_deferred_watch_slots_for_test(&self) {
        let mut data = self.data.lock().unwrap();
        let fence = data
            .deferred_fences
            .iter()
            .find_map(|f| f.upgrade())
            .unwrap();
        data.deferred_fences
            .resize_with(4096, || Arc::downgrade(&fence));
        drop(data);
        self.policies.fill_deferred_watch_slots_for_test();
    }
    /// Specialized deferred inspection: Data remains held through nonblocking
    /// publication acquisition and the final leaf registration/consumption.
    pub(crate) fn with_deferred_launch<T>(
        &self,
        request: HttpWaitRequest<'_>,
        scope: &crate::launch_tokens::LaunchOwnerScope,
        existing: Option<(
            Arc<DeferredBindingFence>,
            Arc<crate::routing::DeferredPolicyFence>,
        )>,
        inspect: impl FnOnce(
            &WaitIntent<'_>,
            Arc<DeferredBindingFence>,
            Arc<crate::routing::DeferredPolicyFence>,
        ) -> Option<T>,
    ) -> Option<T> {
        let _mutation = self.mutations.lock().ok()?;
        let mut data = self.data.lock().ok()?;
        if self.writes_stopped.load(Ordering::Acquire)
            || !request.operation.can_reply()
            || !scope.live()
            || request.provider != Provider::Codex
        {
            return None;
        }
        let root = request.key?;
        if !root
            .strip_prefix("codex:")
            .is_some_and(crate::launch_tokens::valid_native_thread)
            || scope.root().is_some_and(|bound| bound != root)
        {
            return None;
        }
        let account = request.expected_source?;
        let model = request.model?;
        if !launch::deferred_matches(&data, root, scope.anchor(), account, model)
            || !Self::accepted_in(&data, Some(root), account)
            || self.retained_binding(&data, root, Utc::now())?.account != account
        {
            return None;
        }
        let policy_key = launch::policy_key(&data, Some(root))?.to_owned();
        // Revalidation reuses the episode's pinned fences. It must not reserve
        // another slot: a full owner table must still permit existing consumers.
        let (binding, existing_policy) = if let Some((binding, policy)) = existing {
            if !binding.live()
                || binding.root != root
                || binding.anchor != scope.anchor()
                || binding.account != account
                || binding.model != model
                || !binding.matches(&data)
            {
                return None;
            }
            (binding, Some(policy))
        } else {
            data.deferred_fences.retain(|weak| weak.strong_count() != 0);
            if data.deferred_fences.len() >= 4096 {
                return None;
            }
            let binding = Arc::new(DeferredBindingFence {
                root: root.to_owned(),
                anchor: scope.anchor().to_owned(),
                account: account.to_owned(),
                model: model.to_owned(),
                revoked: crate::account_runtime::deferred_reset::Revocation::new(),
            });
            data.deferred_fences.push(Arc::downgrade(&binding));
            (binding, None)
        };
        self.policies
            .try_with_quota_wait_condition(&policy_key, account, |plan| {
                if plan.settings.mode == crate::routing::Mode::Strict
                    || !request.operation.can_reply()
                {
                    return None;
                }
                let policy = if let Some(policy) = existing_policy {
                    if !policy.matches_plan(&policy_key, account, plan) {
                        return None;
                    }
                    policy
                } else {
                    self.policies
                        .track_deferred_plan(&policy_key, account, plan)?
                };
                let intent = WaitIntent {
                    provider: Provider::Codex,
                    key: root,
                    account,
                    model: Some(model),
                    policy_key: &policy_key,
                    existing_placement: true,
                    plan,
                    operation: request.operation,
                };
                inspect(&intent, binding, policy)
            })
    }
    /// Inspect a current bound HTTP source without changing routing or C7.
    ///
    /// The runtime caller holds Store -> released -> probes -> volatile throughout
    /// this call. We hold mutations, release Data, then hold policy publication
    /// read access through inspect. inspect must not reacquire those gates, edit
    /// policy, or await. Its result is optional data, never dispatch permission.
    pub(crate) fn try_current_quota_wait<T>(
        &self,
        request: HttpWaitRequest<'_>,
        inspect: impl FnOnce(&WaitIntent<'_>) -> Option<T>,
    ) -> Option<T> {
        self.try_current_quota_condition(request, false, inspect)
    }

    /// Read-only fallback projection includes zero hold budgets. All current
    /// identity/model/native policy and final prune/publication checks are shared.
    pub(crate) fn try_current_quota_fallback<T>(
        &self,
        request: HttpWaitRequest<'_>,
        inspect: impl FnOnce(&WaitIntent<'_>) -> Option<T>,
    ) -> Option<T> {
        self.try_current_quota_condition(request, true, inspect)
    }

    fn try_current_quota_condition<T>(
        &self,
        request: HttpWaitRequest<'_>,
        fallback: bool,
        inspect: impl FnOnce(&WaitIntent<'_>) -> Option<T>,
    ) -> Option<T> {
        // An uncalled closure and rejected result are dropped after our gates.
        let mut inspect = Some(inspect);
        // Keep a rejected T outside publication, mutations and final Data scopes.
        let mut result = None;
        let current = {
            let _mutation = self.mutations.lock().ok()?;
            if self.writes_stopped.load(Ordering::Acquire) || !request.operation.can_reply() {
                return None;
            }
            let requested = group(request.key?);
            // Existing read-only owner projection exposes private launch metadata.
            // mutations keeps its model stable; Data below verifies the same live
            // identity and retention, including pruning by read-like native APIs.
            let source = self.models_launch_snapshot(Some(requested), None).ok()?;
            if request
                .expected_source
                .is_some_and(|id| id != source.account)
                || (source.managed
                    && (request.model.is_none()
                        || source.current_model.as_deref() != request.model))
            {
                return None;
            }
            let (policy_key, existing_placement) = {
                let data = self.data.lock().ok()?;
                let canonical = if data.groups.contains_key(requested) {
                    requested
                } else {
                    data.launch_aliases
                        .get(requested)
                        .map_or_else(|| launch::ownership_group(&data, requested), String::as_str)
                };
                let provider = if canonical.starts_with("claude:") {
                    Provider::Claude
                } else if canonical.starts_with("codex:")
                    || (source.managed && canonical.starts_with("launch:"))
                {
                    Provider::Codex
                } else {
                    return None;
                };
                if provider != request.provider
                    || canonical != source.canonical_key
                    || data.groups.get(canonical)?.account != source.account
                    || data.launches.contains_key(canonical) != source.managed
                    || self.retained_binding(&data, canonical, Utc::now())?.account
                        != source.account
                {
                    return None;
                }
                (
                    launch::policy_key(&data, Some(canonical))?.to_owned(),
                    Self::accepted_in(&data, Some(canonical), &source.account),
                )
            };
            #[cfg(test)]
            if let Some((entered, release)) =
                { self.wait_before_publication.lock().unwrap().take() }
            {
                entered.try_send(()).unwrap();
                release
                    .recv_timeout(Duration::from_secs(5))
                    .expect("wait publication gate timed out");
            }
            let inspected = self.policies.with_quota_wait_condition(
                &policy_key,
                &source.account,
                fallback,
                |plan| {
                    if !request.operation.can_reply() {
                        return None;
                    }
                    let intent = WaitIntent {
                        provider: request.provider,
                        key: &source.canonical_key,
                        account: &source.account,
                        model: request.model,
                        policy_key: &policy_key,
                        existing_placement,
                        plan,
                        operation: request.operation,
                    };
                    result = inspect.take()?(&intent);
                    result.as_ref()?;
                    // Native read-like APIs can prune without mutations. Keep
                    // publication held through the final coherent identity check.
                    // Other readers may take Data -> publication: never block in
                    // the reverse order just to grant optional wait authority.
                    let data = self.data.try_lock().ok()?;
                    let canonical = if data.groups.contains_key(requested) {
                        requested
                    } else {
                        data.launch_aliases.get(requested).map_or_else(
                            || launch::ownership_group(&data, requested),
                            String::as_str,
                        )
                    };
                    if canonical != source.canonical_key
                        || data.groups.get(canonical)?.account != source.account
                        || data.launches.contains_key(canonical) != source.managed
                        // All current-model writers take mutations; a surviving
                        // managed group therefore retains this protected model.
                        || (source.managed && source.current_model.as_deref() != request.model)
                        || launch::policy_key(&data, Some(canonical))? != policy_key
                        || Self::accepted_in(&data, Some(canonical), &source.account)
                            != existing_placement
                        || self.retained_binding(&data, canonical, Utc::now())?.account
                            != source.account
                        || self.writes_stopped.load(Ordering::Acquire)
                        || !request.operation.can_reply()
                    {
                        return None;
                    }
                    Some(())
                },
            );
            inspected.is_some()
                && !self.writes_stopped.load(Ordering::Acquire)
                && request.operation.can_reply()
        };
        if current { result } else { None }
    }

    pub fn touch(&self, key: Option<&str>) {
        if let Some(k) = key {
            touch_group(
                &mut self.data.lock().unwrap_or_else(|e| e.into_inner()),
                group(k),
                Utc::now(),
            );
        }
    }
    pub fn pin(self: &Arc<Self>, key: Option<&str>) -> SocketPin {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let g = key.map(|k| launch::ownership_group(&d, group(k)).to_owned());
        if let Some(g) = &g {
            *d.pins.entry(g.clone()).or_default() += 1;
            protect_group(&mut d, g);
        }
        SocketPin {
            table: self.clone(),
            group: g,
        }
    }
    pub fn begin_request(self: &Arc<Self>, key: Option<&str>, account: &str) -> BindingLease {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        self.reserve(
            &mut self.data.lock().unwrap_or_else(|e| e.into_inner()),
            key,
            account,
        )
    }
    pub(crate) fn account_dispatch_eligible(
        &self,
        candidate: &Candidate,
        model: Option<&str>,
    ) -> bool {
        let data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        effective_account(candidate, &data, &self.config, model, Utc::now()).eligible(model)
    }
    pub async fn begin_request_async(
        self: &Arc<Self>,
        key: Option<&str>,
        account: &str,
    ) -> Result<BindingLease> {
        let table = self.clone();
        let key = key.map(str::to_owned);
        let account = account.to_owned();
        tokio::task::spawn_blocking(move || table.begin_request(key.as_deref(), &account))
            .await
            .context("routing reservation worker")
    }
    fn reserve(self: &Arc<Self>, d: &mut Data, key: Option<&str>, account: &str) -> BindingLease {
        *d.pending.entry(account.into()).or_default() += 1;
        d.traffic.insert(account.into(), Utc::now());
        let g = key.map(|k| launch::ownership_group(d, group(k)).to_owned());
        if let Some(g) = &g {
            launch::request_started(d, g, Utc::now());
        }
        let after_move = if let Some(g) = &g {
            *d.active.entry(g.clone()).or_default() += 1;
            protect_group(d, g);
            d.after_move.remove(g)
        } else {
            false
        };
        let epoch_key = g.clone().unwrap_or_else(|| format!("unbound:{account}"));
        let known_placement = g.as_ref().map_or_else(
            || d.sticky.values().any(|id| id == account),
            |g| d.groups.contains_key(g),
        );
        let epoch = if known_placement {
            d.epochs
                .entry(epoch_key.clone())
                .or_insert_with(|| Arc::new(()))
                .clone()
        } else {
            Arc::new(())
        };
        BindingLease {
            epoch_key,
            epoch,
            table: self.clone(),
            group: g,
            account: account.into(),
            after_move,
            defer: false,
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn preview(
        &self,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        _request_bytes: u64,
    ) -> PreviewDecision {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        self.select(
            &d,
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            Utc::now(),
            true,
        )
        .0
    }
    #[allow(clippy::too_many_arguments)]
    pub fn choose(
        self: &Arc<Self>,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        request_bytes: u64,
    ) -> Decision {
        self.choose_with_hold(
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            request_bytes,
            true,
            false,
        )
    }
    /// Hold state belongs to this request, not to its persisted session group.
    #[allow(clippy::too_many_arguments)]
    pub fn choose_with_hold(
        self: &Arc<Self>,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        request_bytes: u64,
        hold_allowed: bool,
        hold_timed_out: bool,
    ) -> Decision {
        self.choose_with_hold_at(
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            request_bytes,
            hold_allowed,
            hold_timed_out,
            Utc::now(),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn choose_with_hold_at(
        self: &Arc<Self>,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        request_bytes: u64,
        hold_allowed: bool,
        hold_timed_out: bool,
        now: DateTime<Utc>,
    ) -> Decision {
        self.choose_with_hold_at_admitted(
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            request_bytes,
            hold_allowed,
            hold_timed_out,
            now,
            |_| true,
        )
    }

    /// Reserve before publishing routing policy or moving a group.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn choose_with_hold_at_admitted(
        self: &Arc<Self>,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        request_bytes: u64,
        hold_allowed: bool,
        hold_timed_out: bool,
        now: DateTime<Utc>,
        admit: impl FnOnce(&str) -> bool,
    ) -> Decision {
        self.choose_with_hold_at_admitted_coverage(
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            request_bytes,
            hold_allowed,
            hold_timed_out,
            now,
            None,
            admit,
        )
    }

    /// D22 retains C7's gates and ordering, with full-menu coverage preferred
    /// within the ordinary eligible pools. It never bypasses routing policy.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn choose_with_hold_at_admitted_coverage(
        self: &Arc<Self>,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        request_bytes: u64,
        hold_allowed: bool,
        hold_timed_out: bool,
        now: DateTime<Utc>,
        coverage: Option<&BTreeSet<String>>,
        admit: impl FnOnce(&str) -> bool,
    ) -> Decision {
        self.choose_with_hold_at_admitted_evidence(
            provider,
            key,
            parent,
            model,
            candidates,
            candidates,
            MoveObservations::untracked(),
            pinned,
            request_bytes,
            hold_allowed,
            hold_timed_out,
            now,
            coverage,
            admit,
        )
    }

    /// Evidence may include excluded accounts, but only candidates can be sent.
    /// Resolve the actual source under mutations, never an earlier relay lookup.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn choose_with_hold_at_admitted_evidence(
        self: &Arc<Self>,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        move_evidence: &[Candidate],
        observations: MoveObservations,
        pinned: Option<&str>,
        request_bytes: u64,
        hold_allowed: bool,
        hold_timed_out: bool,
        now: DateTime<Utc>,
        coverage: Option<&BTreeSet<String>>,
        admit: impl FnOnce(&str) -> bool,
    ) -> Decision {
        self.choose_with_hold_at_admitted_proof(
            provider,
            key,
            parent,
            model,
            candidates,
            move_evidence,
            observations,
            pinned,
            request_bytes,
            hold_allowed,
            hold_timed_out,
            now,
            coverage,
            |account| admit(account).then(DispatchAdmission::ordinary),
        )
    }

    /// A capacity admission carries its owner's live reservation evidence.
    /// Validate it against the actual source resolved under mutations, before
    /// policy dispatch or any breaker charge/canonical account publication.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn choose_with_hold_at_admitted_proof(
        self: &Arc<Self>,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        move_evidence: &[Candidate],
        observations: MoveObservations,
        pinned: Option<&str>,
        request_bytes: u64,
        hold_allowed: bool,
        hold_timed_out: bool,
        now: DateTime<Utc>,
        coverage: Option<&BTreeSet<String>>,
        admit: impl FnOnce(&str) -> Option<DispatchAdmission>,
    ) -> Decision {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        for c in candidates {
            if let Some(snapshot) = &c.quota {
                d.placements
                    .entry(c.account.id.clone())
                    .or_default()
                    .observe(snapshot);
            }
        }
        self.prune(&mut d, now);
        // Current positive full-input evidence must constrain the choice,
        // rather than being recorded after approving a move. A handshake's
        // zero bytes never creates known full-context cost.
        if let Some(key) = key.filter(|_| request_bytes > 0)
            && d.groups.contains_key(group(key))
            && !d.unknown_cost.contains(group(key))
        {
            let known = d
                .last_request_bytes
                .entry(group(key).to_owned())
                .or_default();
            *known = (*known).max(request_bytes);
        }
        let preference = coverage.map(|coverage| launch::SelectionPreference {
            account: None,
            coverage: Some(coverage),
            model_move: false,
        });
        let (choice, selection, sole_target) = self.select_with_return_guard(
            &d,
            provider,
            key,
            parent,
            model,
            candidates,
            move_evidence,
            None,
            observations,
            pinned,
            now,
            hold_allowed,
            preference.as_ref(),
        );
        let mut reason = selection.code();
        let mut account = match choice {
            PreviewDecision::Hold { until } => return Decision::Hold { until },
            PreviewDecision::None => return Decision::None,
            PreviewDecision::Selected { account } => account,
        };
        if let Some(key) = key {
            let mut extra = BTreeSet::new();
            if !d.bindings.contains_key(key) {
                extra.insert(key);
            }
            if let Some(parent) =
                parent.filter(|p| group(p) == group(key) && !d.bindings.contains_key(*p))
            {
                extra.insert(parent);
            }
            let mut weight = d.snapshot_weight;
            if let Some(entry) = d.groups.get(group(key))
                && entry.account != account
            {
                for key in &entry.keys {
                    weight = weight
                        .saturating_sub(binding_bound(key, &entry.account))
                        .saturating_add(binding_bound(key, &account));
                }
            }
            weight = extra.iter().fold(weight, |n, key| {
                n.saturating_add(binding_bound(key, &account))
            });
            if d.bindings.len().saturating_add(extra.len()) > CAPACITY * 2
                || weight as u64 + 64 > SNAPSHOT_MAX_BYTES
            {
                tracing::warn!(
                    code = "binding_capacity",
                    "New binding refused at the restore-safe snapshot bound"
                );
                return Decision::Blocked;
            }
        }
        let previous = key
            .and_then(|key| d.groups.get(group(key)))
            .map(|g| g.account.clone());
        let guard_key = key
            .map(group)
            .map(|key| launch::ownership_group(&d, key))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("unidentified:{}", provider.as_str()));
        let physical_source = self.rebind_source(&d, provider, key);
        let guard_source = self.rebind_budget_source(&d, provider, key, &account);
        let guard_from = guard_source.as_deref();
        // Retry exclusion removes a source from selectable candidates, not
        // from the full source evidence. Label before guard/policy publication.
        if reason == "forced"
            && guard_from.is_some_and(|id| id != account)
            && let Some(source) =
                guard_from.and_then(|id| move_evidence.iter().find(|c| c.account.id == id))
        {
            let source = effective_account(source, &d, &self.config, model, now);
            let plan = self.policies.plan(launch::policy_key(&d, key));
            if source.user_state == UserState::Active
                && let Health::Cooling {
                    reason: HealthReason::Quota,
                    until,
                    scope,
                } = &source.health
                && *until > now
                && (scope == "account"
                    || scope
                        .strip_prefix("model:")
                        .is_some_and(|m| model == Some(m)))
                && let Some(quota) = &source.quota_cooling
                && quota.window == "5h"
                && quota
                    .reset_at
                    .signed_duration_since(now)
                    .to_std()
                    .is_ok_and(|wait| wait > Duration::from_secs(plan.settings.reset_wait_secs))
            {
                reason = "five_hour_far_move";
            }
        }
        let permit = match self.prepare_move_with_source(
            &d,
            &guard_key,
            guard_from,
            &account,
            guard_from.and_then(|id| move_evidence.iter().find(|c| c.account.id == id)),
            candidates,
            sole_target.as_ref(),
            observations,
            model,
            reason,
            now,
        ) {
            Ok(permit) => permit,
            Err(denied) => {
                if let Some(old) =
                    guard_from.and_then(|id| candidates.iter().find(|c| c.account.id == id))
                    && self
                        .policies
                        .plan(launch::policy_key(&d, key))
                        .anchor
                        .as_ref()
                        .is_none_or(|anchor| physical_source.as_ref() == Some(anchor))
                    && self.move_source_usable(&d, &guard_key, old, model, now)
                {
                    account = old.account.id.clone();
                    reason = "guard_keep";
                    None
                } else {
                    return Decision::MoveHeld {
                        until: denied.until,
                    };
                }
            }
        };
        let policy_key = launch::policy_key(&d, key).map(str::to_owned);
        let admitted_from = self
            .policies
            .plan(policy_key.as_deref())
            .anchor
            .or_else(|| physical_source.clone());
        drop(d);
        let Some(admission) = admit(&account) else {
            return Decision::None;
        };
        if !selection.valid_for(key, admitted_from.as_deref(), &account, model, now) {
            return Decision::None;
        }
        if !admission.valid_for(admitted_from.as_deref(), &account) {
            return Decision::None;
        }
        if !admission.valid_pin_for(provider, key, &account) {
            return Decision::None;
        }
        if !admission.valid_store_for(
            admitted_from.as_deref(),
            admitted_from.as_deref().and_then(|id| {
                move_evidence
                    .iter()
                    .find(|candidate| candidate.account.id == id)
                    .map(|candidate| &candidate.account)
            }),
            &account,
        ) {
            return Decision::None;
        }
        if !admission.valid_failure_for(
            key,
            admitted_from.as_deref().and_then(|id| {
                move_evidence
                    .iter()
                    .find(|c| c.account.id == id)
                    .map(|c| &c.account)
            }),
            &account,
            model,
        ) {
            return Decision::None;
        }
        let context = rebind::RebindContext {
            provider,
            key,
            guard_key: &guard_key,
            from: physical_source.as_deref(),
            account: &account,
            parent,
            model,
            candidates,
            sources: move_evidence,
            pinned,
            request_bytes,
            hold_timed_out,
            now,
            label: reason,
        };
        let authority = self.owner_move_reason(&context, &admission, Some(&selection), None);
        let rebound = self.rebind(context, authority, permit, |reason| {
            self.policies.dispatch(
                policy_key.as_deref(),
                &account,
                previous.as_deref(),
                reason,
                now,
            )
        });
        let rebind::Rebound { mut data, moves } = match rebound {
            Ok(rebound) => rebound,
            Err(rebind::RebindError::Evidence) => return Decision::None,
            Err(rebind::RebindError::Policy) => return Decision::Blocked,
        };
        let d = &mut *data;
        let lease = self.reserve(d, key, &account);
        self.prune(d, now);
        d.defer_count_changes = false;
        d.commit_count_changes();
        Decision::Selected {
            account,
            lease,
            moves,
        }
    }
    /// Read-only counterpart of prune for selection previews. Ordinary lookups
    /// visit one group; only outstanding capacity eviction walks the LRU prefix.
    fn retained_binding<'a>(
        &self,
        d: &'a Data,
        key: &str,
        now: DateTime<Utc>,
    ) -> Option<&'a Binding> {
        let g = group(key);
        let entry = d.groups.get(g)?;
        if !protected(d, g) && !launch::retained(d, g, now) {
            if idle(now, entry.last_used) >= self.config.session_binding_max_secs {
                return None;
            }
            if d.bindings.len() > CAPACITY {
                let mut size = d.bindings.len();
                for (_, oldest) in &d.lru {
                    if launch::retained(d, oldest, now) {
                        continue;
                    }
                    if size <= CAPACITY {
                        break;
                    }
                    if oldest == g {
                        return None;
                    }
                    size -= d.groups[oldest].keys.len();
                }
            }
        }
        d.bindings
            .get(key)
            .or_else(|| entry.keys.first().and_then(|key| d.bindings.get(key)))
    }
    #[allow(clippy::too_many_arguments)]
    fn select(
        &self,
        d: &Data,
        provider: Provider,
        key: Option<&str>,
        parent: Option<&str>,
        model: Option<&str>,
        candidates: &[Candidate],
        pinned: Option<&str>,
        now: DateTime<Utc>,
        hold_allowed: bool,
    ) -> (PreviewDecision, &'static str) {
        let (choice, reason) = self.select_ordered(
            d,
            provider,
            key,
            parent,
            model,
            candidates,
            pinned,
            now,
            hold_allowed,
            None,
        );
        (choice, reason.code())
    }
    pub fn export(&self) -> Result<Value> {
        let state = snapshot(&self.data.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(serde_json::to_value(state)?)
    }
    /// Takeover only: reject import while any request or socket owns the table.
    pub fn import(&self, value: Value) -> Result<()> {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if !d.active.is_empty() || !d.pins.is_empty() || !d.pending.is_empty() {
            bail!("cannot import active binding table");
        }
        drop(d);
        self.policies.begin_recovery();
        self.policies.reload()?;
        let mut next = decode_scoped(value, self.policies.requires_strict_recovery())?;
        self.policies
            .capture_strict_anchors(next.groups.iter().map(|(key, g)| {
                (
                    launch::policy_key(&next, Some(key)).unwrap(),
                    g.account.as_str(),
                )
            }))?;
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now();
        self.prune(&mut next, now);
        // Import must advance the existing table's revision, rather than use
        // the decoded default: a flush may already own an older snapshot.
        next.revision = d.revision;
        next.count_changes = std::mem::take(&mut d.count_changes);
        next.count_change_order = std::mem::take(&mut d.count_change_order);
        next.count_before = d
            .accounts
            .keys()
            .chain(next.accounts.keys())
            .map(|id| (id.clone(), d.accounts.get(id).map_or(0, |a| a.count)))
            .collect();
        next.deferred_fences = std::mem::take(&mut d.deferred_fences);
        // Whole-container recovery cannot silently retain old owner authority.
        for weak in &next.deferred_fences {
            if let Some(fence) = weak.upgrade() {
                fence.revoked.cancel();
            }
        }
        next.changed();
        *d = next;
        self.policies.finish_recovery();
        Ok(())
    }
    pub fn flush(&self) -> Result<()> {
        self.flush_with(atomic_write)
    }
    /// Finish the last snapshot under the writer lock, then retire this
    /// process as a state-file writer while its streams finish in memory.
    pub fn freeze(&self) -> Result<()> {
        let _mutation = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        self.flush_with_mode(atomic_write, true)?;
        self.policies.freeze();
        Ok(())
    }
    fn flush_with(&self, write: impl FnOnce(&Path, &[u8]) -> std::io::Result<()>) -> Result<()> {
        self.flush_with_mode(write, false)
    }
    fn flush_with_mode(
        &self,
        write: impl FnOnce(&Path, &[u8]) -> std::io::Result<()>,
        freeze: bool,
    ) -> Result<()> {
        // Acquire before taking the snapshot so an older flush can never
        // replace a newer file. Selectors and completion never take this lock.
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if self.writes_stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let (state, revision) = {
            let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
            self.prune(&mut d, Utc::now());
            if !d.dirty {
                if freeze {
                    self.writes_stopped.store(true, Ordering::Release);
                }
                return Ok(());
            }
            (snapshot(&d), d.revision)
        };
        if state.bindings.len() > CAPACITY * 2
            || launch::snapshot_bound(&state) as u64 > SNAPSHOT_MAX_BYTES
        {
            bail!("binding snapshot cannot be restored within its bounds");
        }
        let bytes = serde_json::to_vec(&state)?;
        if bytes.len() as u64 > SNAPSHOT_MAX_BYTES {
            bail!("binding snapshot byte cap exceeded");
        }
        write(&self.path, &bytes)?;
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if d.revision == revision {
            d.dirty = false;
        }
        if freeze {
            self.writes_stopped.store(true, Ordering::Release);
        }
        Ok(())
    }
}
impl Drop for BindingTable {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            tracing::warn!(error = %e, "Final binding flush failed");
        }
    }
}
fn idle(now: DateTime<Utc>, last: DateTime<Utc>) -> u64 {
    (now - last).num_seconds().max(0) as u64
}
struct Metrics {
    gate: bool,
    unknown: bool,
    weekly: Option<DateTime<Utc>>,
    five: Option<DateTime<Utc>>,
    usage: f64,
}
fn compare_gated(
    a: &Candidate,
    x: &Metrics,
    b: &Candidate,
    y: &Metrics,
    anchor: Option<DateTime<Utc>>,
) -> std::cmp::Ordering {
    let cohort = |t: Option<DateTime<Utc>>| match (anchor, t) {
        (Some(start), Some(t)) if (t - start).num_seconds() <= 10800 => (0, None),
        (_, Some(t)) => (1, Some(t)),
        (_, None) => (2, None),
    };
    x.unknown
        .cmp(&y.unknown)
        .then_with(|| cohort(x.weekly).cmp(&cohort(y.weekly)))
        .then_with(|| x.five.is_none().cmp(&y.five.is_none()))
        .then_with(|| x.five.cmp(&y.five))
        .then_with(|| x.usage.total_cmp(&y.usage))
        .then_with(|| a.account.id.cmp(&b.account.id))
}

fn date(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}
fn stale_window(c: &Candidate, d: &Data, w: &Value, now: DateTime<Utc>) -> bool {
    date(&w["observed_at"]).is_some_and(|observed| {
        (now - observed).num_seconds() > 900
            && (d.traffic.get(&c.account.id).is_some_and(|t| *t > observed)
                || d.pending
                    .get(&c.account.id)
                    .copied()
                    .unwrap_or(0)
                    .max(c.inflight)
                    > 0)
    })
}
/// Observed limits may construct C5 cooling; projection never constructs health.
fn effective_account(
    c: &Candidate,
    d: &Data,
    _cfg: &RoutingConfig,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> Account {
    let mut account = c.account.clone();
    if account.eligible_at(model, now)
        && let Some(snapshot) = &c.quota
    {
        account.health = Health::Ready;
        account.quota_cooling = None;
        let mut snapshot = snapshot.clone();
        if let Some(windows) = snapshot["windows"].as_array_mut() {
            windows.retain(|w| !stale_window(c, d, w, now));
        }
        snapshot["signals"] = Value::Null;
        account.apply_health_signal(
            HealthSignal::UsageSnapshot {
                snapshot,
                model: model.map(str::to_owned),
                projected_allowance: 0.0,
            },
            now,
        );
    }
    account
}
fn account_window(provider: Provider, w: &Value) -> Option<bool> {
    match w["name"].as_str()? {
        "7d" | "secondary" => Some(true),
        "5h" | "primary" => Some(false),
        name if provider == Provider::Codex => {
            let minutes = name.strip_suffix('m')?;
            if minutes.is_empty() || !minutes.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let minutes = minutes.parse::<u64>().ok()?;
            (minutes > 0).then_some(minutes > 1440)
        }
        _ => None,
    }
}
// C9 names Codex primary/secondary windows by their actual duration, including
// free-plan 43200m and non-minute poll durations. Track their evidence/cost;
// account_window applies C7's provider-specific duration slots for selection.
fn placement_account_window(w: &Value) -> bool {
    let Some(name) = w["name"].as_str() else {
        return false;
    };
    if matches!(name, "primary" | "secondary") {
        return true;
    }
    if name.len() < 2
        || name.len() > 21
        || !matches!(name.as_bytes().last(), Some(b's' | b'm' | b'h' | b'd'))
    {
        return false;
    }
    let digits = &name[..name.len() - 1];
    digits.bytes().all(|byte| byte.is_ascii_digit())
        && digits.parse::<u64>().is_ok_and(|duration| duration > 0)
}
fn placement_poll(
    id: &str,
    state: &PlacementState,
    now: DateTime<Utc>,
    through: Option<u64>,
    after_secs: u64,
) -> Option<PlacementPoll> {
    let sequence = state.latest?;
    if through.is_some_and(|at| sequence <= at) {
        return None;
    }
    let delay = chrono::Duration::from_std(Duration::from_secs(after_secs)).ok()?;
    let due_at = state
        .poll_deadlines
        .iter()
        .filter(|deadline| through.is_none_or(|at| deadline.through > at))
        .filter_map(|deadline| deadline.at.checked_add_signed(delay))
        .min()?;
    (due_at <= now).then(|| PlacementPoll {
        account: id.to_owned(),
        placement_at: state.latest_at.expect("placement has time"),
        placement_seq: sequence,
        due_at,
    })
}
fn prefer_non_pacing<T>(pool: &mut Vec<(&Candidate, T)>, d: &Data) {
    let non_pacing = pool
        .iter()
        .any(|(c, _)| !d.pacing.get(&c.account.id).copied().unwrap_or(false));
    if non_pacing {
        pool.retain(|(c, _)| !d.pacing.get(&c.account.id).copied().unwrap_or(false));
    }
}
// Call separately for the step-4 gate-passing set or step-5 fallback set,
// after that step's room/ratio partition. This helper never admits a candidate.
fn prefer_pacing_and_evidence(
    pool: &mut Vec<(&Candidate, &Metrics)>,
    d: &Data,
    cfg: &RoutingConfig,
    now: DateTime<Utc>,
) {
    prefer_non_pacing(pool, d);
    if pool.iter().any(|(c, _)| !placement_capped(c, d, cfg, now)) {
        pool.retain(|(c, _)| !placement_capped(c, d, cfg, now));
    } else if let Some(minimum) = pool
        .iter()
        .map(|(c, _)| placement_evidence(c, d, now).1)
        .min()
    {
        pool.retain(|(c, _)| placement_evidence(c, d, now).1 == minimum);
    }
}
fn placement_capped(c: &Candidate, d: &Data, cfg: &RoutingConfig, now: DateTime<Utc>) -> bool {
    let (evidence, count) = placement_evidence(c, d, now);
    evidence == PlacementEvidence::Weak && count >= cfg.weak_evidence_new_sessions
}
fn placement_state(c: &Candidate, d: &Data) -> PlacementState {
    let mut state = d.placements.get(&c.account.id).cloned().unwrap_or_default();
    if let Some(snapshot) = &c.quota {
        state.observe(snapshot);
    }
    state
}
fn placement_evidence(c: &Candidate, d: &Data, now: DateTime<Utc>) -> (PlacementEvidence, usize) {
    let state = placement_state(c, d);
    (
        if state.weak(now) {
            PlacementEvidence::Weak
        } else {
            PlacementEvidence::Fresh
        },
        state.count(),
    )
}
fn metrics_without_placement_charge(
    c: &Candidate,
    d: &Data,
    cfg: &RoutingConfig,
    now: DateTime<Utc>,
) -> Metrics {
    let mut config = cfg.clone();
    config.placement_allowance_pct = 0.0;
    metrics(c, d, &config, now)
}
fn metrics(c: &Candidate, d: &Data, cfg: &RoutingConfig, now: DateTime<Utc>) -> Metrics {
    let mut m = Metrics {
        gate: true,
        unknown: false,
        weekly: None,
        five: None,
        usage: 0.0,
    };
    let Some(windows) = c.quota.as_ref().and_then(|q| q["windows"].as_array()) else {
        m.unknown = true;
        m.usage = f64::INFINITY;
        return m;
    };
    let pending = d
        .pending
        .get(&c.account.id)
        .copied()
        .unwrap_or(0)
        .max(c.inflight);
    let state = placement_state(c, d);
    let mut seen = false;
    let mut slot_usage = [None::<f64>; 2];
    for w in windows {
        let Some(weekly) = account_window(c.account.provider, w) else {
            continue;
        };
        seen = true;
        let reset = date(&w["reset_at"]);
        let expired = reset.is_some_and(|r| r <= now);
        let reset = reset.filter(|r| *r > now);
        let charge = cfg.placement_allowance_pct
            * state.charge_count(w["name"].as_str().expect("gating window")) as f64;
        let usage = if expired {
            Some(cfg.inflight_allowance_pct * pending as f64 + charge)
        } else if stale_window(c, d, w, now) {
            None
        } else {
            w["used_pct"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n + cfg.inflight_allowance_pct * pending as f64 + charge)
        };
        // Gate every window; the highest projected usage (including D20
        // charges) supplies its slot's reset key. Unknown still sorts last.
        let projected = usage.unwrap_or(f64::INFINITY);
        let slot = if weekly { &mut m.weekly } else { &mut m.five };
        let previous = &mut slot_usage[usize::from(weekly)];
        if previous.is_none_or(|old| projected > old) {
            *previous = Some(projected);
            *slot = reset;
        } else if *previous == Some(projected) {
            *slot = match (*slot, reset) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        let Some(usage) = usage else {
            m.unknown = true;
            m.usage = f64::INFINITY;
            continue;
        };
        m.usage = m.usage.max(usage);
        let near = reset.is_some_and(|r| (r - now).num_seconds() as u64 <= cfg.near_reset_secs);
        let gate = if weekly {
            if near {
                cfg.gate_weekly_near_reset_pct
            } else {
                cfg.gate_weekly_pct
            }
        } else {
            cfg.gate_5h_pct
        };
        if usage >= gate {
            m.gate = false;
        }
    }
    if !seen {
        m.unknown = true;
        m.usage = f64::INFINITY;
    }
    m
}
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{:032x}.tmp", rand::random::<u128>()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        #[cfg(windows)]
        crate::local_secrets::restrict(&tmp).map_err(std::io::Error::other)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::{
                Win32::Storage::FileSystem::{
                    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
                },
                core::PCWSTR,
            };
            let src: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
            let dst: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(src.as_ptr()),
                    PCWSTR(dst.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            }
            .map_err(std::io::Error::other)
        }
        #[cfg(not(windows))]
        {
            fs::rename(&tmp, path)?;
            fs::File::open(
                path.parent()
                    .ok_or_else(|| std::io::Error::other("missing state directory"))?,
            )?
            .sync_all()
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Identity, Source, TokenMeta};
    use chrono::Duration as Delta;
    use serde_json::json;

    fn n5a_authority_ready(
        future: std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
    ) -> bool {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        std::future::Future::poll(future, &mut context).is_ready()
    }

    #[test]
    fn n5a_authority_wake_source_commit_publishes_members_and_skips_same_source() {
        let (_root, table) = table();
        let candidates = [
            candidate("wake-a", 0.0, 168, 5),
            candidate("wake-b", 0.0, 168, 5),
        ];
        let root = "claude:authority-wake";
        let key = "claude:authority-wake:agent:child";
        let now = Utc::now();
        let mut first = Box::pin(table.policies.changed.notified());
        let mut second = Box::pin(table.policies.changed.notified());
        first.as_mut().enable();
        second.as_mut().enable();
        {
            // Exercise the actual publication setter, without PolicyStore's
            // earlier dispatch notification masking this binding-only wake.
            let _mutation = table.mutations.lock().unwrap();
            let mut data = table.data.lock().unwrap();
            let moves = table.commit_binding(
                &mut data,
                Provider::Claude,
                Some(key),
                Some(root),
                &candidates,
                None,
                7,
                false,
                now,
                "wake-a",
                "new",
            );
            assert_eq!(moves.len(), 1);
            assert_eq!(data.groups[root].account, "wake-a");
            assert_eq!(data.groups[root].keys.len(), 2);
            assert!(
                data.groups[root]
                    .keys
                    .iter()
                    .all(|member| data.bindings[member].account == "wake-a")
            );
        }
        assert!(n5a_authority_ready(first.as_mut()));
        assert!(
            n5a_authority_ready(second.as_mut()),
            "wake every enabled waiter"
        );
        let mut changed = Box::pin(table.policies.changed.notified());
        changed.as_mut().enable();
        {
            let _mutation = table.mutations.lock().unwrap();
            let mut data = table.data.lock().unwrap();
            let moves = table.commit_binding(
                &mut data,
                Provider::Claude,
                Some(key),
                Some(root),
                &candidates,
                None,
                9,
                false,
                now,
                "wake-a",
                "keep",
            );
            assert!(moves.is_empty());
        }
        table.touch(Some(key));
        assert!(
            !n5a_authority_ready(changed.as_mut()),
            "reindex/cost/touch are not new source authority"
        );
        {
            let _mutation = table.mutations.lock().unwrap();
            let mut data = table.data.lock().unwrap();
            let moves = table.commit_binding(
                &mut data,
                Provider::Claude,
                Some(key),
                Some(root),
                &candidates,
                None,
                9,
                false,
                now,
                "wake-b",
                "forced",
            );
            assert_eq!(moves.len(), 1);
            assert_eq!(data.groups[root].account, "wake-b");
            assert_eq!(data.groups[root].keys.len(), 2);
            assert!(
                data.groups[root]
                    .keys
                    .iter()
                    .all(|member| data.bindings[member].account == "wake-b")
            );
        }
        assert!(n5a_authority_ready(changed.as_mut()));
    }

    #[test]
    fn n5a_authority_wake_table_freeze_publishes_retirement_once() {
        let (_root, table) = table();
        let mut first = Box::pin(table.policies.changed.notified());
        let mut second = Box::pin(table.policies.changed.notified());
        first.as_mut().enable();
        second.as_mut().enable();
        table.freeze().unwrap();
        assert!(table.writes_stopped.load(Ordering::Acquire));
        assert!(!table.policies.permits_dispatch(None, "wake-a"));
        assert!(n5a_authority_ready(first.as_mut()));
        assert!(n5a_authority_ready(second.as_mut()));
        let mut repeated = Box::pin(table.policies.changed.notified());
        repeated.as_mut().enable();
        table.freeze().unwrap();
        assert!(!n5a_authority_ready(repeated.as_mut()));
    }

    const N5C_FIRST: &str = "launch:synthetic-first-project";
    const N5C_REAL: &str = "codex:synthetic-first-project";

    #[test]
    fn n5c_claude_launch_intent_preserves_exact_native_identity_without_binding() {
        let (_home, table) = table();
        for invalid in [
            "label",
            "12345678123442348234123456789abc",
            "claude:12345678-1234-4234-8234-123456789abc",
        ] {
            assert!(table.register_claude_urgent(invalid).is_err());
        }
        let native = "12345678-1234-4234-8234-123456789ABC";
        table.register_claude_urgent(native).unwrap();
        let key = format!("claude:{native}");
        let plan = table.routing_plan(Some(&key));
        assert_eq!(plan.settings.on_5h_limit, crate::routing::On5hLimit::Move);
        assert!(plan.anchor.is_none());
        assert!(table.current(Some(&key)).is_none());
        assert_eq!(
            table
                .routing_plan(Some(&key.to_lowercase()))
                .settings
                .on_5h_limit,
            crate::routing::On5hLimit::Wait
        );
        let d = table.data.lock().unwrap();
        assert!(d.groups.is_empty());
        assert!(d.bindings.is_empty());
    }

    fn n5c_managed_first_fixture() -> (tempfile::TempDir, Arc<BindingTable>, String) {
        let (home, table) = table();
        table
            .policies
            .set_session_urgent_for_key(
                &crate::routing::session_id(N5C_FIRST),
                crate::routing::On5hLimit::Move,
                Some(N5C_FIRST),
            )
            .unwrap();
        let mut c = candidate("cx-first", 0.0, 168, 5);
        c.account.provider = Provider::Codex;
        c.account.identity = Identity::Codex {
            user_id: None,
            account_id: "synthetic-first".into(),
            email: "synthetic@example.invalid".into(),
        };
        let placed = table
            .place_launch(
                LaunchRequest {
                    anchor_key: N5C_FIRST,
                    resume_key: None,
                    model: Some("synthetic-model"),
                    model_is_explicit: true,
                    account_pref: Some("cx-first"),
                    allow_move_for_model: false,
                },
                &[c],
                &BTreeMap::from([(
                    "cx-first".into(),
                    BTreeSet::from(["synthetic-model".into()]),
                )]),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(placed.account, "cx-first");
        assert_eq!(placed.moves.len(), 1);
        let initial = &placed.moves[0];
        assert_eq!(initial.key, N5C_FIRST);
        assert!(initial.from.is_none());
        assert_eq!(initial.to, "cx-first");
        assert!(initial.is_placement);
        let before = table.routing_plan(Some(N5C_FIRST));
        assert_eq!(before.anchor.as_deref(), Some("cx-first"));
        let adopted = table
            .resolve_launch(Some(N5C_FIRST), Some(N5C_REAL), Utc::now())
            .unwrap();
        assert_eq!(adopted.canonical_key, N5C_REAL);
        assert_eq!(table.data.lock().unwrap().launches.len(), 1);
        let project = table
            .policies
            .put_project(
                None,
                crate::routing::Project {
                    name: "synthetic first project".into(),
                    settings: crate::routing::Settings {
                        mode: crate::routing::Mode::Strict,
                        reset_wait_secs: 23,
                        on_5h_limit: crate::routing::On5hLimit::Wait,
                    },
                },
            )
            .unwrap();
        (home, table, project)
    }

    #[test]
    fn n5c_managed_idle_overlay_associates_once_preserving_native_anchor_and_identity() {
        let (home, table, project) = n5c_managed_first_fixture();
        // Generic anchored overlay is still rejected; only the owner path differs.
        assert!(
            table
                .policies
                .associate_with_anchor(N5C_FIRST, &project, Some("cx-first"))
                .is_err()
        );
        table.associate_project(N5C_REAL, &project).unwrap();
        table.associate_project(N5C_REAL, &project).unwrap();
        let plan = table.routing_plan(Some(N5C_REAL));
        assert_eq!(plan.project.as_deref(), Some(project.as_str()));
        assert_eq!(plan.anchor.as_deref(), Some("cx-first"));
        assert_eq!(plan.settings.mode, crate::routing::Mode::Strict);
        assert_eq!(plan.settings.reset_wait_secs, 23);
        assert_eq!(plan.settings.on_5h_limit, crate::routing::On5hLimit::Move);
        assert!(table.permits_dispatch(Some(N5C_REAL), "cx-first"));
        assert!(!table.permits_dispatch(Some(N5C_REAL), "cx-other"));
        assert_eq!(
            table
                .launch_info(N5C_REAL, Utc::now())
                .unwrap()
                .current_model
                .as_deref(),
            Some("synthetic-model")
        );
        {
            let d = table.data.lock().unwrap();
            assert_eq!(launch::policy_key(&d, Some(N5C_REAL)), Some(N5C_FIRST));
            assert!(launch::idle_placement(&d, N5C_REAL));
        }
        let other = table
            .policies
            .put_project(
                None,
                crate::routing::Project {
                    name: "synthetic other project".into(),
                    settings: crate::routing::Settings::default(),
                },
            )
            .unwrap();
        let before = table.policies.list();
        assert!(table.associate_project(N5C_REAL, &other).is_err());
        assert_eq!(table.policies.list(), before);
        drop(table);
        let restarted =
            crate::routing::PolicyStore::new(home.path(), crate::routing::Settings::default())
                .unwrap();
        let persisted = restarted.plan(Some(N5C_FIRST));
        assert_eq!(persisted.project.as_deref(), Some(project.as_str()));
        assert_eq!(persisted.anchor.as_deref(), Some("cx-first"));
        assert_eq!(persisted.settings, plan.settings);
    }

    #[test]
    fn n5c_managed_first_project_refuses_after_native_reservation_even_without_dispatch() {
        let (_home, table, project) = n5c_managed_first_fixture();
        let lease = table.begin_request(Some(N5C_REAL), "cx-first");
        drop(lease);
        assert!(!launch::idle_placement(
            &table.data.lock().unwrap(),
            N5C_REAL
        ));
        let before = table.policies.list();
        assert!(table.associate_project(N5C_REAL, &project).is_err());
        assert_eq!(table.policies.list(), before);
        assert_eq!(table.current(Some(N5C_REAL)).as_deref(), Some("cx-first"));
    }

    #[test]
    fn n5c_managed_first_project_writer_rechecks_interposed_policy_and_account() {
        for account in ["cx-first", "cx-other"] {
            let (_home, table, project) = n5c_managed_first_fixture();
            let mutation = table.mutations.lock().unwrap();
            let d = table.data.lock().unwrap();
            assert!(launch::idle_placement(&d, N5C_REAL));
            let key = launch::policy_key(&d, Some(N5C_REAL)).unwrap().to_owned();
            let selected = d.groups[N5C_REAL].account.clone();
            let evidence = ManagedFirstProject::new(&key, &selected, &mutation);
            drop(d);
            // Independent policy owners may publish while the binding gate is
            // held. Interpose an actual setter after proof construction; no
            // timing/FIFO assumption or synthetic accepted-bit manipulation.
            table
                .policies
                .set_session(
                    &crate::routing::session_id(&key),
                    Some(crate::routing::Settings {
                        mode: crate::routing::Mode::Recover,
                        reset_wait_secs: 31,
                        on_5h_limit: crate::routing::On5hLimit::Move,
                    }),
                    None,
                    Some(account),
                )
                .unwrap();
            if account != selected {
                table
                    .policies
                    .dispatch(Some(&key), account, Some(&selected), "forced", Utc::now())
                    .unwrap();
            }
            let before = table.policies.list();
            assert!(
                table
                    .policies
                    .associate_managed_first_project(evidence, &project)
                    .is_err()
            );
            assert_eq!(table.policies.list(), before);
        }
    }

    #[test]
    fn n5c_managed_same_project_still_checks_owner_account_and_retired_writer() {
        let (_home, table, project) = n5c_managed_first_fixture();
        table.associate_project(N5C_REAL, &project).unwrap();
        table
            .policies
            .set_session(
                &crate::routing::session_id(N5C_FIRST),
                Some(crate::routing::Settings::default()),
                Some(&project),
                Some("cx-other"),
            )
            .unwrap();
        table
            .policies
            .dispatch(
                Some(N5C_FIRST),
                "cx-other",
                Some("cx-first"),
                "forced",
                Utc::now(),
            )
            .unwrap();
        let before = table.policies.list();
        assert!(table.associate_project(N5C_REAL, &project).is_err());
        assert_eq!(table.policies.list(), before);
        let (_retired_home, table, project) = n5c_managed_first_fixture();
        table.freeze().unwrap();
        assert!(table.associate_project(N5C_REAL, &project).is_err());
    }

    #[test]
    fn n5c_ui_managed_projection_uses_observed_root_handle_and_original_policy() {
        let (_home, table) = table();
        table
            .policies
            .set_session_urgent_for_key(
                &crate::routing::session_id(N5C_FIRST),
                crate::routing::On5hLimit::Move,
                Some(N5C_FIRST),
            )
            .unwrap();
        let mut c = candidate("cx-abc123", 0.0, 168, 5);
        c.account.provider = Provider::Codex;
        c.account.identity = Identity::Codex {
            user_id: None,
            account_id: "synthetic-ui-account".into(),
            email: "synthetic@example.invalid".into(),
        };
        let placed = table
            .place_launch(
                LaunchRequest {
                    anchor_key: N5C_FIRST,
                    resume_key: None,
                    model: Some("synthetic-model"),
                    model_is_explicit: true,
                    account_pref: Some("cx-abc123"),
                    allow_move_for_model: false,
                },
                &[c],
                &BTreeMap::from([(
                    "cx-abc123".into(),
                    BTreeSet::from(["synthetic-model".into()]),
                )]),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(placed.account, "cx-abc123");
        let adopted = table
            .resolve_launch(Some(N5C_FIRST), Some(N5C_REAL), Utc::now())
            .unwrap();
        assert_eq!(adopted.canonical_key, N5C_REAL);
        let rows = table.dashboard_routing_snapshot();
        let (account, routing) = rows[&crate::dashboard::SessionId::from_key(N5C_REAL)]
            .as_ref()
            .unwrap();
        assert_eq!(account.as_str(), "cx-abc123");
        assert_eq!(routing.action_id, crate::routing::session_id(N5C_REAL));
        assert_ne!(routing.action_id, crate::routing::session_id(N5C_FIRST));
        assert_eq!(routing.on_5h_limit, crate::routing::On5hLimit::Move);
        table
            .set_routing_session_urgent(&routing.action_id, crate::routing::On5hLimit::Wait)
            .unwrap();
        assert_eq!(
            table.policies.plan(Some(N5C_FIRST)).settings.on_5h_limit,
            crate::routing::On5hLimit::Wait
        );
        let row = table
            .dashboard_quota_wait_row(N5C_FIRST, "cx-abc123")
            .unwrap();
        assert_eq!(row.id, crate::dashboard::SessionId::from_key(N5C_REAL));
        assert!(
            table
                .dashboard_quota_wait_row(N5C_FIRST, "cx-def456")
                .is_none()
        );
    }

    fn n5a_wait_fixture() -> (tempfile::TempDir, Arc<BindingTable>) {
        let root = fixture_dir();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        let mut c = candidate("cx-synthetic-wait", 0.0, 168, 5);
        c.account.provider = Provider::Codex;
        c.account.identity = Identity::Codex {
            user_id: None,
            account_id: "synthetic-wait-account".into(),
            email: "synthetic@example.invalid".into(),
        };
        table
            .place_launch(
                LaunchRequest {
                    anchor_key: "launch:synthetic-wait",
                    resume_key: None,
                    model: Some("synthetic-model-1"),
                    model_is_explicit: true,
                    account_pref: Some(&c.account.id),
                    allow_move_for_model: false,
                },
                std::slice::from_ref(&c),
                &BTreeMap::from([(
                    c.account.id.clone(),
                    BTreeSet::from(["synthetic-model-1".into(), "synthetic-model-2".into()]),
                )]),
                Utc::now(),
            )
            .unwrap();
        table
            .resolve_launch(
                Some("launch:synthetic-wait"),
                Some("codex:synthetic-wait"),
                Utc::now(),
            )
            .unwrap();
        // Placement alone is provisional; exercise the actual native dispatch
        // receipt before asking for an existing-session wait intent.
        table
            .begin_request(Some("codex:synthetic-wait"), "cx-synthetic-wait")
            .dispatched();
        (root, table)
    }

    fn n5a_wait_operation() -> crate::issue_operation::IssueOperation {
        crate::issue_operation::IssueOperation::new(
            tokio::time::Instant::now() + Duration::from_secs(60),
        )
    }

    fn n5a_wait_request(operation: &crate::issue_operation::IssueOperation) -> HttpWaitRequest<'_> {
        HttpWaitRequest {
            provider: Provider::Codex,
            key: Some("codex:synthetic-wait"),
            model: Some("synthetic-model-1"),
            expected_source: Some("cx-synthetic-wait"),
            operation,
        }
    }

    #[test]
    fn n5a_wait_intent_projects_managed_identity_and_releases_data() {
        let (_root, table) = n5a_wait_fixture();
        let operation = n5a_wait_operation();
        let before = table.routing_view();
        let revision = table.data.lock().unwrap().revision;
        let result = table.try_current_quota_wait(n5a_wait_request(&operation), |intent| {
            assert!(
                table.mutations.try_lock().is_err(),
                "wait inspection must hold mutations"
            );
            assert!(
                table.data.try_lock().is_ok(),
                "wait inspection must release Data"
            );
            assert_eq!(intent.provider(), Provider::Codex);
            assert_eq!(intent.key(), "codex:synthetic-wait");
            assert_eq!(intent.account(), "cx-synthetic-wait");
            assert_eq!(intent.model(), Some("synthetic-model-1"));
            assert_eq!(intent.policy_key(), "launch:synthetic-wait");
            assert!(intent.existing_placement());
            assert_eq!(intent.plan().settings.reset_wait_secs, 60);
            assert_eq!(intent.deadline(), operation.deadline());
            assert!(intent.caller_current());
            Some(intent.account().to_owned())
        });
        assert_eq!(result.as_deref(), Some("cx-synthetic-wait"));
        assert_eq!(table.routing_view(), before);
        assert_eq!(table.data.lock().unwrap().revision, revision);
        assert_eq!(table.pending_count("cx-synthetic-wait"), 0);
    }

    #[test]
    fn n5a_wait_intent_rejects_missing_provider_source_and_model_mismatches() {
        let (_root, table) = n5a_wait_fixture();
        let operation = n5a_wait_operation();
        for case in 0..6 {
            let mut request = n5a_wait_request(&operation);
            match case {
                0 => request.key = None,
                1 => request.key = Some("codex:synthetic-unknown"),
                2 => request.provider = Provider::Claude,
                3 => request.expected_source = Some("cx-synthetic-other"),
                4 => request.model = Some("synthetic-model-2"),
                _ => request.model = None,
            }
            assert!(
                table
                    .try_current_quota_wait(request, |_| -> Option<()> {
                        panic!("mismatched wait input must not reach inspection")
                    })
                    .is_none(),
                "case {case}"
            );
        }
    }

    #[test]
    fn n5a_wait_intent_reads_new_model_and_strict_permission() {
        let (_root, table) = n5a_wait_fixture();
        let operation = n5a_wait_operation();
        table
            .record_launch_model(
                "codex:synthetic-wait",
                "cx-synthetic-wait",
                "synthetic-model-2",
                Utc::now(),
            )
            .unwrap();
        assert!(
            table
                .try_current_quota_wait(n5a_wait_request(&operation), |_| Some(()))
                .is_none()
        );
        let mut request = n5a_wait_request(&operation);
        request.model = Some("synthetic-model-2");
        assert_eq!(
            table
                .try_current_quota_wait(request, |intent| {
                    Some(intent.model().unwrap().to_owned())
                })
                .as_deref(),
            Some("synthetic-model-2")
        );
        let id = crate::routing::session_id("codex:synthetic-wait");
        table
            .set_routing_session(
                &id,
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Strict,
                    reset_wait_secs: 60,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                false,
            )
            .unwrap();
        let mut request = n5a_wait_request(&operation);
        request.model = Some("synthetic-model-2");
        assert_eq!(
            table.try_current_quota_wait(request, |intent| {
                assert_eq!(intent.plan().anchor.as_deref(), Some(intent.account()));
                Some(intent.plan().settings.mode)
            }),
            Some(crate::routing::Mode::Strict)
        );
        table
            .set_routing_session(
                &id,
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Strict,
                    reset_wait_secs: 0,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                false,
            )
            .unwrap();
        let mut request = n5a_wait_request(&operation);
        request.model = Some("synthetic-model-2");
        assert!(
            table
                .try_current_quota_wait(request, |_| -> Option<()> {
                    panic!("current zero wait policy must refuse inspection")
                })
                .is_none()
        );
    }

    #[test]
    fn n5a_wait_intent_never_changes_c7_or_cooling_dispatch_eligibility() {
        let root = fixture_dir();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        let key = "claude:synthetic-c7-wait";
        let mut c = candidate("cl-synthetic-wait", 0.0, 168, 5);
        match table.choose(
            Provider::Claude,
            Some(key),
            None,
            None,
            std::slice::from_ref(&c),
            None,
            0,
        ) {
            Decision::Selected { lease, .. } => {
                lease.dispatched();
                drop(lease);
            }
            other => panic!("initial synthetic binding: {other:?}"),
        }
        let now = Utc::now();
        let reset = now + Delta::seconds(30);
        c.account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: reset,
            scope: "account".into(),
        };
        c.account.quota_cooling = Some(crate::accounts::QuotaCooling {
            window: "5h".into(),
            reset_at: reset,
            started_at: now,
        });
        let operation = n5a_wait_operation();
        let before = table.routing_view();
        assert_eq!(
            table
                .try_current_quota_wait(
                    HttpWaitRequest {
                        provider: Provider::Claude,
                        key: Some(key),
                        model: None,
                        expected_source: Some(&c.account.id),
                        operation: &operation,
                    },
                    |intent| Some(intent.account().to_owned())
                )
                .as_deref(),
            Some(c.account.id.as_str())
        );
        assert!(!table.account_dispatch_eligible(&c, None));
        let called = std::cell::Cell::new(false);
        let choice = table.choose_with_hold_at_admitted(
            Provider::Claude,
            Some(key),
            None,
            None,
            &[c],
            None,
            0,
            true,
            false,
            now,
            |_| {
                called.set(true);
                true
            },
        );
        assert!(matches!(choice, Decision::Hold { until } if until == reset));
        assert!(
            !called.get(),
            "legacy C7 must still precede dispatch admission"
        );
        assert_eq!(table.routing_view(), before);
        assert_eq!(table.pending_count("cl-synthetic-wait"), 0);
    }

    #[test]
    fn n5a_wait_intent_checks_caller_and_drops_rejected_result_after_gates() {
        struct Rejected<'a> {
            table: &'a BindingTable,
            dropped: &'a std::cell::Cell<bool>,
        }
        impl Drop for Rejected<'_> {
            fn drop(&mut self) {
                assert!(
                    self.table.mutations.try_lock().is_ok(),
                    "rejected result must drop after mutations"
                );
                assert!(self.table.data.try_lock().is_ok());
                self.dropped.set(true);
            }
        }
        let (_root, table) = n5a_wait_fixture();
        let expired = crate::issue_operation::IssueOperation::new(
            tokio::time::Instant::now() - Duration::from_secs(1),
        );
        assert!(
            table
                .try_current_quota_wait(n5a_wait_request(&expired), |_| -> Option<()> {
                    panic!("expired caller must not reach wait inspection")
                })
                .is_none()
        );
        let cancelled = n5a_wait_operation();
        drop(cancelled.caller_guard());
        assert!(
            table
                .try_current_quota_wait(n5a_wait_request(&cancelled), |_| -> Option<()> {
                    panic!("cancelled caller must not reach wait inspection")
                })
                .is_none()
        );
        let operation = n5a_wait_operation();
        let caller = operation.caller_guard();
        let dropped = std::cell::Cell::new(false);
        assert!(
            table
                .try_current_quota_wait(n5a_wait_request(&operation), |_| {
                    drop(caller);
                    Some(Rejected {
                        table: &table,
                        dropped: &dropped,
                    })
                })
                .is_none()
        );
        assert!(dropped.get());
    }

    struct N5aPruneRejected {
        table: Arc<BindingTable>,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for N5aPruneRejected {
        fn drop(&mut self) {
            assert!(self.table.mutations.try_lock().is_ok());
            assert!(self.table.data.try_lock().is_ok());
            // An actual policy writer must finish before this destructor returns:
            // a retained publication read guard would make this bounded check fail.
            let table = self.table.clone();
            let (done, finished) = std::sync::mpsc::sync_channel(1);
            let writer = std::thread::spawn(move || {
                let result = table.policies.put_project(
                    Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                    crate::routing::Project {
                        name: "synthetic drop witness".into(),
                        settings: crate::routing::Settings::default(),
                    },
                );
                done.send(result.is_ok()).unwrap();
            });
            assert!(finished.recv_timeout(Duration::from_secs(5)).unwrap());
            writer.join().unwrap();
            self.dropped.store(true, Ordering::Release);
        }
    }

    fn n5a_prune_publish_policy(table: &BindingTable, wait_secs: u64) {
        table
            .policies
            .set_session(
                &crate::routing::session_id("launch:synthetic-wait"),
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Rebalance,
                    reset_wait_secs: wait_secs,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                None,
            )
            .unwrap();
    }

    fn n5a_prune_actual_reader(table: &BindingTable) {
        // Public native read-like path: no direct test deletion, fake receipt or
        // elapsed sleep. Past both launch24h and ordinary7d retention thresholds.
        assert!(
            table
                .launch_info("codex:synthetic-wait", Utc::now() + Delta::days(8),)
                .is_err()
        );
        assert!(
            !table
                .data
                .lock()
                .unwrap()
                .groups
                .contains_key("codex:synthetic-wait")
        );
    }

    #[test]
    fn n5a_wait_prune_before_new_policy_publication_rejects_result_after_gates() {
        let (_root, table) = n5a_wait_fixture();
        n5a_prune_publish_policy(&table, 60);
        let operation = n5a_wait_operation();
        assert_eq!(
            table.try_current_quota_wait(n5a_wait_request(&operation), |intent| {
                Some(intent.plan().settings.reset_wait_secs)
            }),
            Some(60)
        );

        let (entered, at_gate) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        *table.wait_before_publication.lock().unwrap() = Some((entered, resume));
        let dropped = Arc::new(AtomicBool::new(false));
        let called = Arc::new(AtomicBool::new(false));
        let (done, finished) = std::sync::mpsc::sync_channel(1);
        let worker = {
            let table = table.clone();
            let dropped = dropped.clone();
            let called = called.clone();
            std::thread::spawn(move || {
                let operation = n5a_wait_operation();
                let result = table.try_current_quota_wait(n5a_wait_request(&operation), |intent| {
                    assert_eq!(intent.plan().settings.reset_wait_secs, 17);
                    called.store(true, Ordering::Release);
                    Some(N5aPruneRejected {
                        table: table.clone(),
                        dropped,
                    })
                });
                done.send(result.is_none()).unwrap();
            })
        };
        at_gate.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(table.mutations.try_lock().is_err());
        n5a_prune_actual_reader(&table);
        // Actual PolicyStore writer publishes between the old Data snapshot and
        // publication acquisition, independently of binding mutations. Project
        // edits freeze existing session baselines, so use an effective session edit.
        n5a_prune_publish_policy(&table, 17);
        release.send(()).unwrap();
        assert!(finished.recv_timeout(Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
        assert!(called.load(Ordering::Acquire));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn n5a_wait_prune_during_inspect_rejects_result_after_gates() {
        let (_root, table) = n5a_wait_fixture();
        let operation = n5a_wait_operation();
        let dropped = Arc::new(AtomicBool::new(false));
        assert!(
            table
                .try_current_quota_wait(n5a_wait_request(&operation), |_| {
                    n5a_prune_actual_reader(&table);
                    Some(N5aPruneRejected {
                        table: table.clone(),
                        dropped: dropped.clone(),
                    })
                })
                .is_none()
        );
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn n5a_wait_final_data_contention_declines_without_blocking() {
        let (_root, table) = n5a_wait_fixture();
        let operation = n5a_wait_operation();
        let (lock_now, start) = std::sync::mpsc::sync_channel(1);
        let (locked, is_locked) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        let holder = {
            let table = table.clone();
            std::thread::spawn(move || {
                start.recv_timeout(Duration::from_secs(5)).unwrap();
                let _data = table.data.lock().unwrap();
                locked.send(()).unwrap();
                resume
                    .recv_timeout(Duration::from_secs(5))
                    .expect("optional wait blocked on Data");
            })
        };
        let result = table.try_current_quota_wait(n5a_wait_request(&operation), |_| {
            lock_now.send(()).unwrap();
            is_locked.recv_timeout(Duration::from_secs(5)).unwrap();
            Some(())
        });
        // Only the completed method can release the holder: a blocking reverse
        // lock would trip its timeout instead of completing this causal sequence.
        release.send(()).unwrap();
        holder.join().unwrap();
        assert!(result.is_none());
        assert_eq!(
            table.try_current_quota_wait(n5a_wait_request(&operation), |_| Some(())),
            Some(())
        );
    }

    pub(super) fn candidate(id: &str, used: f64, weekly_hours: i64, five_hours: i64) -> Candidate {
        let now = Utc::now();
        Candidate {
            account: Account {
                schema: 1,
                id: id.into(),
                provider: Provider::Claude,
                label: id.into(),
                email: "synthetic@example.invalid".into(),
                plan: None,
                identity: Identity::Claude {
                    account_uuid: id.into(),
                    organization_uuid: None,
                    email: "synthetic@example.invalid".into(),
                },
                source: Source::Oauth,
                user_state: UserState::Active,
                health: Health::Ready,
                auth_failures: 0,
                payment_failures: 0,
                payment_until: None,
                auth_probe_pending: false,
                quota_cooling: None,
                drain: false,
                allow_credits: false,
                quota_credit_releasable: false,
                generation: 0,
                token_meta: TokenMeta::default(),
            },
            quota: Some(json!({"observed_at":now,"windows":[
                {"name":"5h","used_pct":used,"reset_at":now+Delta::hours(five_hours),"observed_at":now},
                {"name":"7d","used_pct":used,"reset_at":now+Delta::hours(weekly_hours),"observed_at":now}
            ]})),
            inflight: 0,
        }
    }
    pub(super) fn fixture_dir() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/bindings-tests");
        fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("bindings-")
            .tempdir_in(root)
            .unwrap()
    }
    fn strict_root_policy(home: &Path, key: &str) -> crate::routing::PolicyStore {
        let store = crate::routing::PolicyStore::new(
            home,
            crate::routing::Settings {
                mode: crate::routing::Mode::Rebalance,
                reset_wait_secs: 60,
                on_5h_limit: crate::routing::On5hLimit::Wait,
            },
        )
        .unwrap();
        store
            .set_session(
                &crate::routing::session_id(key),
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Strict,
                    reset_wait_secs: 0,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                None,
            )
            .unwrap();
        assert!(store.requires_strict_recovery());
        assert_eq!(
            store.plan(Some(key)).settings.mode,
            crate::routing::Mode::Strict
        );
        store
    }
    fn table() -> (tempfile::TempDir, Arc<BindingTable>) {
        let dir = fixture_dir();
        let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
        (dir, t)
    }
    /// These older cases isolate cold placement/drain mechanics. Churn defaults
    /// are exercised separately with clocks and actual alternating failures.
    fn cold_table() -> (tempfile::TempDir, Arc<BindingTable>) {
        let dir = fixture_dir();
        let mut config = RoutingConfig::default();
        config.churn.dwell_secs = 0;
        config.churn.revisit_secs = 0;
        config.churn.max_group_moves = 16;
        let table = BindingTable::new(dir.path(), config).unwrap();
        (dir, table)
    }
    // Explicit pin fixtures use the real owner, never a bare authority label.
    pub(super) fn with_pin(
        provider: Provider,
        key: Option<&str>,
        target: &str,
        select: impl FnOnce(DispatchAdmission) -> Decision,
    ) -> Decision {
        let pins = crate::debug_pin::DebugPins::default();
        pins.set(provider, key.map(str::to_owned), target.to_owned());
        let proof = pins.selection(provider, key).unwrap();
        select(DispatchAdmission::ordinary().with_pin(Some(proof)))
    }

    fn choose(
        t: &Arc<BindingTable>,
        key: Option<&str>,
        cs: &[Candidate],
    ) -> (String, BindingLease, Vec<MoveEvent>) {
        match t.choose(Provider::Claude, key, None, None, cs, None, 42) {
            Decision::Selected {
                account,
                lease,
                moves,
            } => {
                lease.dispatched();
                (account, lease, moves)
            }
            other => panic!("{other:?}"),
        }
    }
    fn preview(t: &BindingTable, cs: &[Candidate]) -> String {
        match t.preview(Provider::Claude, None, None, None, cs, None, 0) {
            PreviewDecision::Selected { account } => account,
            other => panic!("{other:?}"),
        }
    }
    fn age(t: &BindingTable, key: &str, seconds: i64) {
        let mut d = t.data.lock().unwrap();
        for b in d
            .bindings
            .values_mut()
            .filter(|b| group(&b.key) == group(key))
        {
            b.last_used = Utc::now() - Delta::seconds(seconds);
        }
        rebuild_indexes(&mut d);
    }
    #[test]
    fn gates_project_all_windows_and_near_reset() {
        let (_dir, t) = table();
        let d = Data::default();
        let now = Utc::now();
        let mut c = candidate("a", 89.0, 8, 2);
        assert!(metrics(&c, &d, &t.config, now).gate);
        c.inflight = 1;
        assert!(!metrics(&c, &d, &t.config, now).gate);
        c.inflight = 0;
        c.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(97);
        assert!(!metrics(&c, &d, &t.config, now).gate);
        c.quota.as_mut().unwrap()["windows"][1]["reset_at"] = json!(now + Delta::hours(4));
        assert!(metrics(&c, &d, &t.config, now).gate);
        c.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(98);
        assert!(!metrics(&c, &d, &t.config, now).gate);
    }
    #[test]
    fn reservation_uses_max_not_sum_and_burst_spreads() {
        let dir = fixture_dir();
        // Isolate max(runtime reservations, relay inflight): D20's separate
        // placement charge must not obscure the 89 + 1 = 90 assertion.
        let config = RoutingConfig {
            placement_allowance_pct: 0.0,
            ..RoutingConfig::default()
        };
        let t = BindingTable::new(dir.path(), config).unwrap();
        let mut cs = vec![candidate("a", 89.0, 8, 1), candidate("b", 10.0, 30, 2)];
        let (id, lease, _) = choose(&t, Some("claude:one"), &cs);
        assert_eq!(id, "a");
        cs[0].inflight = 1;
        assert_eq!(
            metrics(&cs[0], &t.data.lock().unwrap(), &t.config, Utc::now()).usage,
            90.0
        );
        let (id, second, _) = choose(&t, Some("claude:two"), &cs);
        assert_eq!(id, "b");
        drop(lease);
        drop(second);
        assert!(t.data.lock().unwrap().pending.is_empty());
    }
    #[test]
    fn ordering_anchored_weekly_cohort_then_five_usage_id() {
        let (_dir, t) = table();
        let a = candidate("a", 10.0, 8, 4);
        let b = candidate("b", 20.0, 10, 2);
        let c = candidate("c", 1.0, 12, 1);
        assert_eq!(preview(&t, &[c.clone(), b.clone(), a.clone()]), "b");
        assert_eq!(preview(&t, &[a.clone(), c, b]), "b");
        let mut b = a.clone();
        b.account.id = "b".into();
        b.quota.as_mut().unwrap()["windows"][0]["used_pct"] = json!(5);
        b.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(5);
        assert_eq!(preview(&t, &[a.clone(), b.clone()]), "b");
        b.quota = a.quota.clone();
        assert_eq!(preview(&t, &[b.clone(), a.clone()]), "a");
        b.quota.as_mut().unwrap()["windows"][0]["reset_at"] = Value::Null;
        assert_eq!(preview(&t, &[b, a]), "a");
    }
    #[test]
    fn unknown_and_traffic_stale_sort_last_in_fallback() {
        let (_dir, t) = table();
        let mut unknown = candidate("a", 0.0, 8, 1);
        unknown.quota = None;
        let known = candidate("b", 95.0, 20, 2);
        assert_eq!(preview(&t, &[unknown, known.clone()]), "a");
        let mut stale = candidate("a", 0.0, 8, 1);
        for w in stale.quota.as_mut().unwrap()["windows"]
            .as_array_mut()
            .unwrap()
        {
            w["observed_at"] = json!(Utc::now() - Delta::minutes(16));
        }
        assert_eq!(preview(&t, &[stale.clone(), known.clone()]), "a");
        let lease = t.begin_request(None, "a");
        drop(lease);
        assert_eq!(preview(&t, &[stale, known]), "a");
    }
    #[test]
    fn fallback_headroom_and_drain_exclusion() {
        let (_dir, t) = table();
        let mut a = candidate("a", 91.0, 8, 1);
        let b = candidate("b", 95.0, 30, 2);
        assert_eq!(preview(&t, &[a.clone(), b.clone()]), "a");
        a.account.drain = true;
        assert_eq!(preview(&t, &[a, b]), "b");
    }
    #[test]
    fn unbound_selection_without_dispatch_does_not_create_drain_privilege() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        let pending = t.choose(Provider::Claude, None, None, None, &[a.clone()], None, 42);
        assert!(!t.existing_placement(None, "a"));
        a.account.drain = true;
        assert_eq!(choose(&t, None, &[a, b]).0, "b");
        if let Decision::Selected { lease, .. } = &pending {
            lease.dispatched();
        }
        assert!(
            !t.existing_placement(None, "a"),
            "a stale admission cannot warm the successor"
        );
        assert!(t.existing_placement(None, "b"));
    }
    #[test]
    fn aba_old_receipt_cannot_warm_a_new_placement_for_root_or_unbound() {
        for key in [Some("claude:aba"), None] {
            let (_dir, t) = table();
            let cs = [candidate("a", 10.0, 8, 1), candidate("b", 10.0, 30, 2)];
            let now = Utc::now();
            // Keep the actual ABA moves outside R14's unchanged-account return window.
            let choose_raw = |id, at| match with_pin(Provider::Claude, key, id, |admission| {
                t.choose_with_hold_at_admitted_proof(
                    Provider::Claude,
                    key,
                    None,
                    None,
                    &cs,
                    &cs,
                    MoveObservations::untracked(),
                    Some(id),
                    42,
                    true,
                    false,
                    at,
                    None,
                    |_| Some(admission),
                )
            }) {
                Decision::Selected { account, lease, .. } => {
                    assert_eq!(account, id);
                    lease
                }
                other => panic!("{other:?}"),
            };
            let old_a = choose_raw("a", now);
            let b = choose_raw("b", now + Delta::seconds(1));
            let new_a = choose_raw("a", now + Delta::minutes(31) + Delta::seconds(1));
            old_a.dispatched();
            b.dispatched();
            assert!(
                !t.existing_placement(key, "a"),
                "old A→B→A receipt authorized new A"
            );
            new_a.dispatched();
            assert!(t.existing_placement(key, "a"));
        }
    }
    #[test]
    fn oversized_binding_file_is_refused_before_legacy_anchor_capture() {
        let dir = fixture_dir();
        fs::create_dir_all(dir.path().join("state")).unwrap();
        let file = fs::File::create(dir.path().join("state/bindings.json")).unwrap();
        file.set_len(SNAPSHOT_MAX_BYTES + 1).unwrap();
        drop(file);
        assert!(BindingTable::new(dir.path(), RoutingConfig::default()).is_err());
    }
    #[test]
    fn strict_corrupt_snapshot_is_retained_and_cannot_be_reinterpreted_as_first_placement() {
        for raw in [b"{".as_slice(), br#"{"schema":99,"bindings":[]}"#,
            br#"{"schema":1,"bindings":[{"key":"codex:old","account":"cx-a","last_used":"2026-10-20T00:00:00Z"},{"key":"codex:old","account":"cx-b","last_used":"2026-10-20T00:00:00Z"}]}"#] {
            let dir = fixture_dir();
            let policy = strict_root_policy(dir.path(), "codex:old");
            let policy_path = dir.path().join("state/routing-policies.json");
            let policy_bytes = fs::read(&policy_path).unwrap();
            let path = dir.path().join("state/bindings.json");
            fs::write(&path, raw).unwrap();
            crate::local_secrets::restrict(&path).unwrap();
            let cfg = RoutingConfig::default();
            cfg.validate().unwrap();
            assert_eq!(cfg.mode, crate::routing::Mode::Rebalance);
            for _ in 0..2 {
                let Err(error) = BindingTable::new(dir.path(), cfg.clone()) else {
                    panic!("strict corrupt binding must refuse startup");
                };
                assert!(error.to_string().contains("strict binding recovery failed"), "{error:#}");
                assert_eq!(fs::read(&path).unwrap(), raw, "refusal must retain corrupt binding evidence");
                assert_eq!(fs::read(&policy_path).unwrap(), policy_bytes);
                assert_eq!(policy.plan(Some("codex:old")).anchor, None,
                    "corruption cannot be reinterpreted as strict first placement");
            }
        }
    }
    #[test]
    fn pinned_child_capacity_refuses_growth_and_freeze_round_trips_within_reader_bounds() {
        let (_dir, t) = table();
        let pin = t.pin(Some("claude:bounded"));
        {
            let mut d = t.data.lock().unwrap();
            let mut weight = 64usize;
            for i in 0..CAPACITY * 2 {
                let key = format!("claude:bounded:agent:{}{i}", "x".repeat(1024));
                let next = binding_bound(&key, "a");
                if weight + next > SNAPSHOT_MAX_BYTES as usize {
                    break;
                }
                weight += next;
                d.bindings.insert(
                    key.clone(),
                    Binding::new(key, "a".into(), Utc::now(), None, None, None, None, false),
                );
            }
            rebuild_indexes(&mut d);
            d.changed();
        }
        let before = t.count();
        assert!(before < CAPACITY);
        let key = format!("claude:bounded:agent:{}", "y".repeat(2048));
        assert!(matches!(
            t.choose(
                Provider::Claude,
                Some(&key),
                None,
                None,
                &[candidate("a", 10.0, 8, 1)],
                None,
                42
            ),
            Decision::Blocked
        ));
        assert_eq!(t.count(), before);
        t.freeze().unwrap();
        assert!(fs::metadata(&t.path).unwrap().len() <= SNAPSHOT_MAX_BYTES);
        let restored = BindingTable::new(
            t.path.parent().unwrap().parent().unwrap(),
            RoutingConfig::default(),
        )
        .unwrap();
        assert_eq!(restored.count(), before);
        drop(pin);
    }
    #[test]
    fn unknown_reservations_cannot_accumulate_placement_epochs() {
        let (_dir, t) = table();
        for n in 0..=CAPACITY {
            drop(t.begin_request(Some(&format!("claude:unknown-{n}")), "a"));
        }
        assert!(t.data.lock().unwrap().epochs.is_empty());
    }
    #[test]
    fn strict_group_conflict_refuses_start_and_import_without_losing_the_current_state() {
        // Both timings are explicit root policies under global rebalance.
        for strict_before_binding in [true, false] {
            let dir = fixture_dir();
            let cfg = RoutingConfig::default();
            cfg.validate().unwrap();
            if strict_before_binding {
                strict_root_policy(dir.path(), "claude:conflict");
            }
            let t = BindingTable::new(dir.path(), cfg.clone()).unwrap();
            let (_, lease, _) = choose(&t, Some("claude:conflict"), &[candidate("a", 10.0, 8, 1)]);
            if !strict_before_binding {
                t.set_routing_session(
                    &crate::routing::session_id("claude:conflict"),
                    Some(crate::routing::Settings {
                        mode: crate::routing::Mode::Strict,
                        reset_wait_secs: 0,
                        on_5h_limit: crate::routing::On5hLimit::Wait,
                    }),
                    None,
                    false,
                )
                .unwrap();
            }
            assert_eq!(
                t.policies.plan(Some("claude:conflict")).settings.mode,
                crate::routing::Mode::Strict
            );
            assert!(t.policies.requires_strict_recovery());
            drop(lease);
            assert!(t.data.lock().unwrap().active.is_empty());
            let before = t.export().unwrap();
            let conflict = json!({"schema":1,"bindings":[
                {"key":"claude:conflict","account":"a","last_used":Utc::now()},
                {"key":"claude:conflict:agent:x","account":"b","last_used":Utc::now()}]});
            let error = t.import(conflict.clone()).unwrap_err();
            assert!(
                error.to_string().contains("strict binding group"),
                "{error:#}"
            );
            assert_eq!(t.export().unwrap(), before);
            t.freeze().unwrap();
            let bytes = serde_json::to_vec(&conflict).unwrap();
            fs::write(&t.path, &bytes).unwrap();
            crate::local_secrets::restrict(&t.path).unwrap();
            let policy_path = dir.path().join("state/routing-policies.json");
            let policy_bytes = fs::read(&policy_path).unwrap();
            for _ in 0..2 {
                let Err(error) = BindingTable::new(dir.path(), cfg.clone()) else {
                    panic!("conflicting strict binding group must refuse startup");
                };
                assert!(
                    format!("{error:#}").contains("strict binding group"),
                    "{error:#}"
                );
                assert_eq!(fs::read(&t.path).unwrap(), bytes);
                assert_eq!(fs::read(&policy_path).unwrap(), policy_bytes);
            }
        }
    }
    #[test]
    fn strict_duplicate_object_field_is_refused_while_private_non_strict_corruption_is_quarantined()
    {
        for strict in [false, true] {
            let dir = fixture_dir();
            fs::create_dir_all(dir.path().join("state")).unwrap();
            let path = dir.path().join("state/bindings.json");
            let raw=br#"{"schema":1,"bindings":[{"key":"codex:old","account":"cx-a","account":"cx-b","last_used":"2026-10-20T00:00:00Z"}]}"#;
            fs::write(&path, raw).unwrap();
            crate::local_secrets::restrict(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            let cfg = RoutingConfig::default();
            cfg.validate().unwrap();
            if strict {
                strict_root_policy(dir.path(), "codex:old");
            }
            let result = BindingTable::new(dir.path(), cfg);
            if strict {
                let Err(error) = result else {
                    panic!("duplicate binding field must refuse strict recovery");
                };
                assert!(
                    error.to_string().contains("strict binding recovery failed"),
                    "{error:#}"
                );
                assert!(format!("{error:#}").contains("duplicate"), "{error:#}");
                assert_eq!(fs::read(&path).unwrap(), raw);
            } else {
                assert_eq!(result.unwrap().count(), 0);
                assert!(
                    !path.exists(),
                    "non-strict contrast never reached JSON quarantine"
                );
                let quarantined: Vec<_> = fs::read_dir(dir.path().join("state"))
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with("bindings.json.corrupt-")
                    })
                    .collect();
                assert_eq!(quarantined.len(), 1);
                assert_eq!(fs::read(&quarantined[0]).unwrap(), raw);
            }
        }
    }
    #[test]
    fn legacy_twenty_thousand_normal_eighty_byte_keys_remain_readable() {
        let dir = fixture_dir();
        fs::create_dir_all(dir.path().join("state")).unwrap();
        let now = Utc::now();
        let bindings: Vec<_> = (0..CAPACITY)
            .map(|n| json!({"key":format!("codex:{n:074}"),"account":"cx-account","last_used":now}))
            .collect();
        let bytes = serde_json::to_vec(&json!({"schema":1,"bindings":bindings})).unwrap();
        assert!((bytes.len() as u64) < SNAPSHOT_MAX_BYTES);
        let path = dir.path().join("state/bindings.json");
        fs::write(&path, bytes).unwrap();
        crate::local_secrets::restrict(&path).unwrap();
        let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
        assert_eq!(t.count(), CAPACITY);
        let original = fs::read(&path).unwrap();
        drop(t.begin_request(Some(&format!("codex:{:074}", 0)), "cx-account"));
        assert!(t.data.lock().unwrap().dirty);
        t.freeze().unwrap();
        assert_ne!(
            fs::read(&path).unwrap(),
            original,
            "freeze never exercised the writer"
        );
        assert_eq!(
            BindingTable::new(dir.path(), RoutingConfig::default())
                .unwrap()
                .count(),
            CAPACITY
        );
    }
    #[test]
    fn legacy_policy_alias_conflict_refuses_start_and_idle_import_before_pruning() {
        for schema in [1, 2] {
            let dir = fixture_dir();
            let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
            let id = crate::routing::session_id("codex:alias");
            let mut policy = json!({"schema":schema,"revision":1,"projects":{},"sessions":{id:{"project":null,"baseline":{"mode":"rebalance","reset_wait_secs":60},"explicit":{"mode":"strict","reset_wait_secs":0},"anchor":null,"movements":[]}}});
            if schema == 2 {
                policy["legacy_codex_roots"] = json!([]);
                let child = crate::routing::session_id("codex:alias:agent:x");
                policy["sessions"][child] = json!({"project":null,
                    "baseline":{"mode":"rebalance","reset_wait_secs":60},
                    "explicit":{"mode":"strict","reset_wait_secs":0},
                    "anchor":null,"movements":[]});
            }
            let policy_path = dir.path().join("state/routing-policies.json");
            let policy_bytes = serde_json::to_vec(&policy).unwrap();
            fs::write(&policy_path, &policy_bytes).unwrap();
            crate::local_secrets::restrict(&policy_path).unwrap();
            let value = json!({"schema":1,"bindings":[{"key":"codex:alias","account":"a","last_used":"2020-01-01T00:00:00Z"},{"key":"codex:alias:agent:x","account":"b","last_used":"2020-01-01T00:00:00Z"}]});
            let bytes = serde_json::to_vec(&value).unwrap();
            fs::write(&t.path, &bytes).unwrap();
            crate::local_secrets::restrict(&t.path).unwrap();
            if schema == 1 {
                let before = t.export().unwrap();
                for _ in 0..2 {
                    let error = t.import(value.clone()).unwrap_err();
                    assert!(error.to_string().contains("strict policy alias"), "{error}");
                    assert_eq!(t.export().unwrap(), before);
                    assert_eq!(fs::read(&t.path).unwrap(), bytes);
                    assert_eq!(fs::read(&policy_path).unwrap(), policy_bytes);
                }
                for _ in 0..2 {
                    assert!(BindingTable::new(dir.path(), RoutingConfig::default()).is_err());
                    assert_eq!(fs::read(&t.path).unwrap(), bytes);
                    assert_eq!(fs::read(&policy_path).unwrap(), policy_bytes);
                }
            } else {
                let cfg = RoutingConfig::default();
                cfg.validate().unwrap();
                let restored = BindingTable::new(dir.path(), cfg).unwrap();
                assert_eq!(
                    restored
                        .policies
                        .plan(Some("codex:alias"))
                        .anchor
                        .as_deref(),
                    Some("a")
                );
                assert_eq!(
                    restored
                        .policies
                        .plan(Some("codex:alias:agent:x"))
                        .anchor
                        .as_deref(),
                    Some("b")
                );
                assert!(!restored.policies.permits_dispatch(Some("codex:alias"), "b"));
                assert!(
                    restored
                        .policies
                        .permits_dispatch(Some("codex:alias:agent:x"), "b")
                );
            }
        }
    }

    #[test]
    fn invalid_recovered_account_never_reaches_policy_writer() {
        for bad in [
            "bad/account",
            "bad:account",
            "sk-secret",
            "",
            &"x".repeat(101),
        ] {
            let (dir, t) = table();
            t.policies
                .set_session(
                    &crate::routing::session_id("codex:invalid-account"),
                    Some(crate::routing::Settings {
                        mode: crate::routing::Mode::Strict,
                        reset_wait_secs: 0,
                        on_5h_limit: crate::routing::On5hLimit::Wait,
                    }),
                    None,
                    None,
                )
                .unwrap();
            assert!(t.policies.requires_strict_recovery());
            t.flush().unwrap();
            let before = t.export().unwrap();
            let path = dir.path().join("state/routing-policies.json");
            let existed = fs::read(&path).ok();
            let value = json!({"schema":1,"bindings":[{"key":"codex:invalid-account","account":bad,"last_used":Utc::now()}]});
            let error = t.import(value.clone()).unwrap_err();
            assert!(
                format!("{error:#}").contains("invalid binding"),
                "{error:#}"
            );
            assert_eq!(t.export().unwrap(), before);
            assert_eq!(fs::read(&path).ok(), existed);
            fs::write(&t.path, serde_json::to_vec(&value).unwrap()).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&t.path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            let config = RoutingConfig::default();
            config.validate().unwrap();
            let Err(error) = BindingTable::new(dir.path(), config) else {
                panic!("invalid strict recovered account must refuse startup");
            };
            assert!(
                format!("{error:#}").contains("invalid binding"),
                "{error:#}"
            );
            assert_eq!(fs::read(&path).ok(), existed);
            assert_eq!(
                crate::json_unique::from_slice(&fs::read(&t.path).unwrap()).unwrap(),
                value
            );
        }
    }
    #[test]
    fn idle_import_validates_the_latest_disk_strict_policy_and_preserves_the_old_table() {
        let (dir, t) = table();
        let (_, lease, _) = choose(&t, Some("claude:disk-newer"), &[candidate("a", 10.0, 8, 1)]);
        drop(lease);
        t.flush().unwrap();
        let before = t.export().unwrap();
        let original = fs::read(&t.path).unwrap();
        let latest =
            crate::routing::PolicyStore::new(dir.path(), crate::routing::Settings::default())
                .unwrap();
        latest
            .set_session(
                &crate::routing::session_id("claude:disk-newer"),
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Strict,
                    reset_wait_secs: 0,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            t.policies.plan(Some("claude:disk-newer")).settings.mode,
            crate::routing::Mode::Rebalance
        );
        let policy_path = dir.path().join("state/routing-policies.json");
        let policy_bytes = fs::read(&policy_path).unwrap();
        let value = json!({"schema":1,"bindings":[{"key":"claude:disk-newer","account":"a","last_used":Utc::now()},{"key":"claude:disk-newer:agent:x","account":"b","last_used":Utc::now()}]});
        let error = t.import(value).unwrap_err();
        assert!(
            error.to_string().contains("strict binding group"),
            "{error}"
        );
        assert_eq!(t.export().unwrap(), before);
        assert_eq!(fs::read(&t.path).unwrap(), original);
        assert_eq!(fs::read(policy_path).unwrap(), policy_bytes);
        assert_eq!(
            t.policies.plan(Some("claude:disk-newer")).settings.mode,
            crate::routing::Mode::Strict
        );
    }
    #[test]
    fn warm_binding_and_unbound_stay_despite_thresholds_and_drain() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        drop(choose(&t, Some("claude:s"), &[a.clone(), b.clone()]));
        // Establish sticky A explicitly; D20 may otherwise place this first
        // unbound request on fresh B after the bound session charged A.
        assert_eq!(choose(&t, None, std::slice::from_ref(&a)).0, "a");
        a.account.drain = true;
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(99);
        }
        assert_eq!(choose(&t, Some("claude:s"), &[a.clone(), b.clone()]).0, "a");
        assert_eq!(choose(&t, None, &[a, b]).0, "a");
    }
    #[test]
    fn cold_stays_when_gate_passes_and_moves_only_when_gate_fails() {
        let (_dir, t) = cold_table();
        let mut a = candidate("a", 10.0, 30, 2);
        let b = candidate("b", 5.0, 8, 1);
        drop(choose(&t, Some("claude:s"), &[a.clone()]));
        age(&t, "claude:s", 3601);
        assert_eq!(choose(&t, Some("claude:s"), &[a.clone(), b.clone()]).0, "a");
        age(&t, "claude:s", 3601);
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(91);
        }
        let (id, lease, moves) = choose(&t, Some("claude:s"), &[a, b]);
        assert_eq!(id, "b");
        assert!(lease.after_move);
        assert_eq!(moves[0].reason, "cold_rebalance");
    }
    #[test]
    fn hold_bound_and_timeout_reason() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        drop(choose(&t, Some("claude:s"), &[a.clone()]));
        let until = Utc::now() + Delta::seconds(30);
        a.account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until,
            scope: "account".into(),
        };
        a.account.quota_cooling = Some(crate::accounts::QuotaCooling {
            window: "5h".into(),
            reset_at: until,
            started_at: Utc::now(),
        });
        assert!(
            matches!(t.choose(Provider::Claude, Some("claude:s"), None, None, &[a.clone(), b.clone()], None, 1), Decision::Hold { until: u } if u == until)
        );
        let result = t.choose_with_hold_at_admitted_proof(
            Provider::Claude,
            Some("claude:s"),
            None,
            None,
            std::slice::from_ref(&b),
            &[a.clone(), b.clone()],
            MoveObservations::untracked(),
            None,
            42,
            false,
            true,
            Utc::now(),
            None,
            |_| Some(DispatchAdmission::ordinary()),
        );
        assert!(matches!(result, Decision::Selected { account, moves, .. }
            if account == "b" && moves[0].reason == "reset_hold_timeout"));
        drop(choose(
            &t,
            Some("claude:far"),
            &[candidate("a", 10.0, 8, 1)],
        ));
        a.account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: Utc::now() + Delta::seconds(120),
            scope: "account".into(),
        };
        a.account.quota_cooling.as_mut().unwrap().reset_at = Utc::now() + Delta::seconds(120);
        assert_eq!(choose(&t, Some("claude:far"), &[a, b]).0, "b");
    }
    #[test]
    fn groups_inherit_move_atomically_and_old_completion_does_not_undo() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        let (_, old, _) = choose(&t, Some("claude:s"), &[a.clone()]);
        let result = t.choose(
            Provider::Claude,
            Some("claude:s:agent:c"),
            Some("claude:s"),
            None,
            &[a.clone(), b.clone()],
            None,
            9,
        );
        assert!(matches!(&result, Decision::Selected { account, .. } if account == "a"));
        drop(result);
        a.account.user_state = UserState::Paused;
        let (_, lease, moves) = choose(&t, Some("claude:s:agent:c"), &[a, b]);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].keys, 2);
        assert_eq!(moves[0].bytes, 42);
        assert_eq!(moves[0].to, "b");
        assert!(lease.after_move);
        drop(old);
        assert_eq!(t.current(Some("claude:s")).as_deref(), Some("b"));
    }
    #[test]
    fn pins_and_active_requests_protect_cold_expiry_and_eviction() {
        let (_dir, t) = table();
        let a = candidate("a", 10.0, 8, 1);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        let pin = t.pin(Some("claude:s"));
        age(&t, "claude:s", 700000);
        assert_eq!(t.current(Some("claude:s")).as_deref(), Some("a"));
        {
            let mut d = t.data.lock().unwrap();
            for i in 0..CAPACITY {
                let key = format!("claude:other{i}");
                d.bindings.insert(
                    key.clone(),
                    Binding::new(key, "a".into(), Utc::now(), None, None, None, None, false),
                );
            }
            rebuild_indexes(&mut d);
            t.prune(&mut d, Utc::now());
            assert_eq!(d.bindings.len(), CAPACITY);
            assert!(d.bindings.contains_key("claude:s"));
        }
        drop(pin);
        assert_eq!(t.current(Some("claude:s")), None);
        let (_, lease, _) = choose(&t, Some("claude:active"), &[a]);
        age(&t, "claude:active", 700000);
        assert!(t.current(Some("claude:active")).is_some());
        drop(lease);
        assert!(t.current(Some("claude:active")).is_some());
        drop(t.pin(None));
    }
    #[test]
    fn preview_does_not_mutate_and_pin_never_overrides_health() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        let before = t.export().unwrap();
        assert!(matches!(
            t.preview(
                Provider::Claude,
                Some("claude:s"),
                None,
                None,
                &[a.clone()],
                None,
                0
            ),
            PreviewDecision::Selected { .. }
        ));
        assert_eq!(before, t.export().unwrap());
        assert!(t.data.lock().unwrap().pending.is_empty());
        a.account.user_state = UserState::Paused;
        assert!(
            matches!(t.preview(Provider::Claude, None, None, None, &[a, b], Some("a"), 0), PreviewDecision::Selected { account } if account == "b")
        );
    }
    #[test]
    fn model_cooling_filters_only_known_affected_model() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        a.account.health = Health::Cooling {
            reason: HealthReason::Permission,
            until: Utc::now() + Delta::hours(1),
            scope: "model:opus".into(),
        };
        assert_eq!(preview(&t, &[a.clone(), b.clone()]), "a");
        assert!(
            matches!(t.preview(Provider::Claude, None, None, Some("opus"), &[a, b], None, 0), PreviewDecision::Selected { account } if account == "b")
        );
    }
    #[test]
    fn handshake_preserves_after_move_for_first_response_create() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 5.0, 30, 2);
        drop(choose(&t, Some("claude:s"), &[a.clone()]));
        a.account.user_state = UserState::Paused;
        let (_, mut lease, _) = choose(&t, Some("claude:s"), &[a, b]);
        assert!(lease.after_move);
        lease.defer_after_move();
        drop(lease);
        let lease = t.begin_request(Some("claude:s"), "b");
        assert!(lease.after_move);
        drop(lease);
        assert!(!t.begin_request(Some("claude:s"), "b").after_move);
    }
    #[test]
    fn persistence_restart_import_and_corrupt_quarantine() {
        let (dir, t) = table();
        drop(choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]));
        t.flush().unwrap();
        t.flush().unwrap();
        let value = t.export().unwrap();
        assert_eq!(value["schema"], 1);
        drop(t);
        let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
        assert_eq!(t.current(Some("claude:s")).as_deref(), Some("a"));
        assert_eq!(t.account_stats("a").0, 1);
        t.import(value).unwrap();
        let pin = t.pin(Some("claude:s"));
        assert!(t.import(json!({"schema":1,"bindings":[]})).is_err());
        drop(pin);
        drop(t);
        fs::write(dir.path().join("state/bindings.json"), "{broken").unwrap();
        let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
        assert_eq!(t.count(), 0);
        assert!(fs::read_dir(dir.path().join("state")).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("bindings.json.corrupt-")
        }));
    }

    #[test]
    fn concurrent_placements_see_every_prior_reservation() {
        let (_dir, t) = table();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let cs = vec![candidate("a", 89.0, 8, 1), candidate("b", 10.0, 30, 2)];
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let t = t.clone();
                let barrier = barrier.clone();
                let cs = cs.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    choose(&t, Some(&format!("claude:burst{i}")), &cs)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|(id, _, _)| id == "a").count(), 1);
        assert_eq!(results.iter().filter(|(id, _, _)| id == "b").count(), 7);
        drop(results);
        assert!(t.data.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn constructor_rejects_invalid_config_and_schema_is_quarantined() {
        let dir = fixture_dir();
        let config = RoutingConfig {
            gate_5h_pct: f64::NAN,
            ..RoutingConfig::default()
        };
        assert!(BindingTable::new(dir.path(), config).is_err());
        fs::create_dir_all(dir.path().join("state")).unwrap();
        fs::write(
            dir.path().join("state/bindings.json"),
            r#"{"schema":2,"bindings":[]}"#,
        )
        .unwrap();
        let t = BindingTable::new(dir.path(), RoutingConfig::default()).unwrap();
        assert_eq!(t.count(), 0);
        assert!(matches!(
            t.choose(Provider::Claude, None, None, None, &[], None, 0),
            Decision::None
        ));
    }

    #[test]
    fn flush_releases_data_lock_and_preserves_mutations_until_next_write() {
        use std::sync::mpsc;
        let (_dir, t) = table();
        let (_, lease, _) = choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]);
        let (captured_tx, captured_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = t.clone();
        let flush = std::thread::spawn(move || {
            writer.flush_with(|path, bytes| {
                captured_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(std::io::Error::other)?;
                atomic_write(path, bytes)
            })
        });
        captured_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (mutated_tx, mutated_rx) = mpsc::channel();
        let mutation = t.clone();
        let mutate = std::thread::spawn(move || {
            // Completion, import, placement and touch must all proceed while
            // the first writer is stopped after capturing its old snapshot.
            drop(lease);
            mutation
                .import(json!({"schema":1,"bindings":[{
                    "key":"claude:s", "account":"b", "last_used":Utc::now()
                }]}))
                .unwrap();
            drop(choose(
                &mutation,
                Some("claude:new"),
                &[candidate("b", 10.0, 8, 1)],
            ));
            mutation.touch(Some("claude:s"));
            mutated_tx.send(()).unwrap();
        });
        let progressed = mutated_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        flush.join().unwrap().unwrap();
        mutate.join().unwrap();
        assert!(
            progressed.is_ok(),
            "completion and selection blocked behind disk writer"
        );
        assert!(
            t.data.lock().unwrap().dirty,
            "old flush erased newer mutations"
        );
        let old: Snapshot = serde_json::from_slice(&fs::read(&t.path).unwrap()).unwrap();
        assert_eq!(old.bindings.len(), 1);
        assert_eq!(old.bindings[0].account, "a");
        t.flush().unwrap();
        let latest: Snapshot = serde_json::from_slice(&fs::read(&t.path).unwrap()).unwrap();
        assert_eq!(latest.bindings.len(), 2);
        assert!(latest.bindings.iter().all(|b| b.account == "b"));
        assert!(!t.data.lock().unwrap().dirty);
    }

    #[test]
    fn failed_flush_keeps_dirty_and_import_advances_existing_revision() {
        let (_dir, t) = table();
        drop(choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]));
        assert!(
            t.flush_with(|_, _| Err(std::io::Error::other("synthetic write failure")))
                .is_err()
        );
        let revision = t.data.lock().unwrap().revision;
        assert!(t.data.lock().unwrap().dirty);
        t.import(json!({"schema":1,"bindings":[]})).unwrap();
        assert!(t.data.lock().unwrap().revision > revision);
        t.flush().unwrap();
        let state: Snapshot = serde_json::from_slice(&fs::read(&t.path).unwrap()).unwrap();
        assert!(state.bindings.is_empty());
    }

    fn codex_candidate(id: &str, used: f64) -> Candidate {
        let mut c = candidate(id, used, 30, 2);
        c.account.provider = Provider::Codex;
        c.account.identity = Identity::Codex {
            user_id: None,
            account_id: id.into(),
            email: "synthetic@example.invalid".into(),
        };
        c
    }

    #[test]
    fn projected_credit_usage_preserves_affinity_and_remains_eligible_for_fallback() {
        let dir = fixture_dir();
        // This legacy proof isolates projected 98 + inflight 1 = 99 from D20's
        // placement allowance; default charge/health behavior has separate tests.
        let config = RoutingConfig {
            placement_allowance_pct: 0.0,
            ..RoutingConfig::default()
        };
        let t = BindingTable::new(dir.path(), config).unwrap();
        let mut a = codex_candidate("a", 98.0);
        let b = codex_candidate("b", 99.0);
        // Establish both bound and unbound sticky placement below the limit.
        drop(t.choose(
            Provider::Codex,
            Some("codex:s"),
            None,
            None,
            std::slice::from_ref(&a),
            None,
            0,
        ));
        drop(t.choose(
            Provider::Codex,
            None,
            None,
            None,
            std::slice::from_ref(&a),
            None,
            0,
        ));
        // Fresh post-placement evidence keeps the confidence cap from choosing
        // B before these in-flight-only fallback and affinity assertions.
        let asof = t.begin_usage_observation_for("a");
        let observed = Utc::now();
        a.quota.as_mut().unwrap()["observed_at"] = json!(observed);
        observe_at(&mut a, observed);
        t.note_usage_observation_asof_token("a", a.quota.as_ref().unwrap(), asof);
        let reservation = t.begin_request(Some("codex:s"), "a");
        assert_eq!(t.pending_count("a"), 1);
        a.inflight = 1;
        let cfg = &t.config;
        let d = t.data.lock().unwrap();
        let effective = effective_account(&a, &d, cfg, None, Utc::now());
        assert!(effective.eligible_at(None, Utc::now()));
        assert_eq!(a.account.health, Health::Ready);
        drop(d);
        let mut b = b;
        b.account.allow_credits = true;
        for (key, parent, pinned) in [
            (Some("codex:new"), None, None),
            (Some("codex:s"), None, None),
            (Some("codex:child"), Some("codex:s"), None),
            (None, None, None),
            (Some("codex:pin"), None, Some("a")),
        ] {
            assert_eq!(
                t.preview(
                    Provider::Codex,
                    key,
                    parent,
                    None,
                    &[a.clone(), b.clone()],
                    pinned,
                    0
                ),
                PreviewDecision::Selected {
                    account: "a".into()
                }
            );
        }
        assert!(
            matches!(t.choose(Provider::Codex, Some("codex:s"), None, None, &[a, b], None, 0), Decision::Selected { account, moves, .. } if account == "a" && moves.is_empty())
        );
        drop(reservation);
        assert_eq!(t.pending_count("a"), 0);
    }

    #[test]
    fn quota_limit_uses_observed_100_and_codex_credit_guard_only() {
        let (_dir, t) = table();
        let mut c = codex_candidate("a", 98.0);
        c.account.allow_credits = true;
        c.inflight = 2; // Projected 100 is still below the observed limit.
        assert!(matches!(
            t.preview(
                Provider::Codex,
                None,
                None,
                None,
                std::slice::from_ref(&c),
                None,
                0
            ),
            PreviewDecision::Selected { .. }
        ));
        c.quota.as_mut().unwrap()["windows"][0]["used_pct"] = json!(100);
        assert_eq!(
            t.preview(
                Provider::Codex,
                None,
                None,
                None,
                std::slice::from_ref(&c),
                None,
                0
            ),
            PreviewDecision::None
        );
        c.quota = None;
        assert!(matches!(
            t.preview(Provider::Codex, None, None, None, &[c], None, 0),
            PreviewDecision::Selected { .. }
        ));
        let mut claude = candidate("cl", 98.0, 30, 2);
        claude.inflight = 2;
        assert!(matches!(
            t.preview(
                Provider::Claude,
                None,
                None,
                None,
                std::slice::from_ref(&claude),
                None,
                0
            ),
            PreviewDecision::Selected { .. }
        ));
        claude.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(100);
        assert_eq!(
            t.preview(Provider::Claude, None, None, None, &[claude], None, 0),
            PreviewDecision::None
        );
    }

    #[test]
    fn restored_bindings_never_seed_traffic_after_restart_or_import() {
        let (_dir, t) = table();
        let last = Utc::now() - Delta::minutes(20);
        let state = json!({"schema":1,"bindings":[
            {"key":"claude:older","account":"a","last_used":last-Delta::minutes(1)},
            {"key":"claude:newer","account":"a","last_used":last}
        ]});
        t.import(state).unwrap();
        assert!(t.data.lock().unwrap().traffic.is_empty());
        t.flush().unwrap();
        let mut stale = candidate("a", 0.0, 8, 1);
        for w in stale.quota.as_mut().unwrap()["windows"]
            .as_array_mut()
            .unwrap()
        {
            w["observed_at"] = json!(last);
        }
        let known = candidate("b", 95.0, 30, 2);
        assert_eq!(preview(&t, &[stale.clone(), known.clone()]), "a");
        let home = t.path.parent().unwrap().parent().unwrap().to_path_buf();
        drop(t);
        let restored = BindingTable::new(&home, RoutingConfig::default()).unwrap();
        assert!(restored.data.lock().unwrap().traffic.is_empty());
        assert_eq!(preview(&restored, &[stale, known]), "a");
    }

    #[test]
    fn stale_exhaustion_does_not_move_warm_binding_or_exclude_unbound_fallback() {
        let (_dir, t) = table();
        let mut c = codex_candidate("a", 10.0);
        drop(t.choose(
            Provider::Codex,
            Some("codex:warm"),
            None,
            None,
            std::slice::from_ref(&c),
            None,
            0,
        ));
        assert!(t.has_traffic("a"));
        for w in c.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(100);
            w["observed_at"] = json!(Utc::now() - Delta::minutes(20));
        }
        c.quota.as_mut().unwrap()["signals"] = json!({"limit_reached":true});
        for key in [Some("codex:warm"), None] {
            assert_eq!(
                t.preview(
                    Provider::Codex,
                    key,
                    None,
                    None,
                    std::slice::from_ref(&c),
                    None,
                    0
                ),
                PreviewDecision::Selected {
                    account: "a".into()
                }
            );
        }
        drop(t.choose(
            Provider::Codex,
            None,
            None,
            None,
            std::slice::from_ref(&c),
            None,
            0,
        ));
        assert!(t.has_traffic("a"));
    }

    #[test]
    fn drained_binding_stays_warm_but_rebalances_cold_even_below_gate() {
        let (_dir, t) = cold_table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 10.0, 30, 2);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        a.account.drain = true;
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(91);
        }
        assert_eq!(choose(&t, Some("claude:s"), &[a.clone(), b.clone()]).0, "a");
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(10);
        }
        age(&t, "claude:s", 3601);
        let (id, lease, moves) = choose(&t, Some("claude:s"), &[a.clone(), b.clone()]);
        assert_eq!(id, "b");
        assert_eq!(moves[0].reason, "cold_rebalance");
        drop(lease);
        a.account.drain = false;
        // The high-usage phase is an independent placement, not an immediate return to A.
        drop(choose(
            &t,
            Some("claude:drain-high"),
            std::slice::from_ref(&a),
        ));
        a.account.drain = true;
        assert_eq!(
            choose(&t, Some("claude:new"), &[a.clone(), b.clone()]).0,
            "b"
        );
        age(&t, "claude:drain-high", 3601);
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(91);
        }
        // The new group on B above used its confidence allowance. This case
        // tests drain/cold behavior with an available destination, rather than
        // the separate D20 cap-wait rule, so refresh B after that placement.
        let asof = t.begin_usage_observation_for("b");
        t.note_usage_observation_asof_token("b", b.quota.as_ref().unwrap(), asof);
        let (account, _, moves) = choose(&t, Some("claude:drain-high"), &[a, b]);
        assert_eq!(account, "b");
        assert_eq!(moves[0].reason, "cold_rebalance");
    }

    #[test]
    fn flush_prunes_oversized_group_after_its_last_pin_is_released() {
        let (_dir, t) = table();
        let pin = t.pin(Some("claude:oversized"));
        {
            let mut d = t.data.lock().unwrap();
            let now = Utc::now();
            for i in 0..=CAPACITY {
                let key = format!("claude:oversized:agent:{i}");
                d.bindings.insert(
                    key.clone(),
                    Binding::new(key, "a".into(), now, None, None, None, None, false),
                );
            }
            rebuild_indexes(&mut d);
            d.changed();
        }
        t.flush().unwrap();
        assert_eq!(t.data.lock().unwrap().bindings.len(), CAPACITY + 1);
        assert!(!t.data.lock().unwrap().dirty);
        // Pin release itself does not mark persistence dirty or scan the table.
        drop(pin);
        t.flush().unwrap();
        assert!(t.data.lock().unwrap().bindings.len() <= CAPACITY);
        let saved: Snapshot = serde_json::from_slice(&fs::read(&t.path).unwrap()).unwrap();
        assert!(saved.bindings.len() <= CAPACITY);
        assert!(
            saved.bindings.is_empty(),
            "eviction must preserve group atomicity"
        );
    }
    #[test]
    fn n10_outside_use_is_last_resort_before_reset_preference() {
        let (_dir, table) = table();
        let outside = candidate("outside", 1.0, 1, 1);
        let ordinary = candidate("ordinary", 85.0, 40, 5);
        table.set_outside_use("outside", true);
        let (selected, lease, _) = choose(&table, Some("claude:new"), &[outside.clone(), ordinary]);
        assert_eq!(
            selected, "ordinary",
            "a reset preference cannot override outside-use protection"
        );
        drop(lease);
        let (selected, lease, _) = choose(&table, Some("claude:last-resort"), &[outside]);
        assert_eq!(
            selected, "outside",
            "native last resort must still serve a new session"
        );
        drop(lease);
    }

    #[test]
    fn n10_outside_use_reuses_source_but_never_receives_an_existing_group() {
        let (_dir, table) = table();
        let source = candidate("source", 10.0, 40, 5);
        let outside = candidate("outside", 1.0, 1, 1);
        let (_, lease, _) = choose(&table, Some("claude:source"), std::slice::from_ref(&source));
        drop(lease);
        let (_, lease, _) = choose(
            &table,
            Some("claude:outside"),
            std::slice::from_ref(&outside),
        );
        drop(lease);
        table.set_outside_use("outside", true);
        let (selected, lease, _) =
            choose(&table, Some("claude:outside"), &[source, outside.clone()]);
        assert_eq!(
            selected, "outside",
            "an existing outside-used source keeps serving"
        );
        drop(lease);
        assert!(
            matches!(
                table.choose(
                    Provider::Claude,
                    Some("claude:source"),
                    None,
                    None,
                    std::slice::from_ref(&outside),
                    None,
                    42
                ),
                Decision::None
            ),
            "even forced recovery cannot transfer a running group onto outside-used capacity"
        );
    }

    fn observe_at(c: &mut Candidate, time: DateTime<Utc>) {
        for w in c.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["observed_at"] = json!(time);
        }
    }

    #[test]
    fn stale_cold_groups_never_ping_pong_and_fresh_burst_spills_to_unknown() {
        let (_dir, t) = table();
        let mut a = candidate("a", 89.0, 8, 1);
        let mut b = candidate("b", 95.0, 30, 2);
        drop(choose(&t, Some("claude:cold"), std::slice::from_ref(&b)));
        let old = Utc::now() - Delta::minutes(20);
        observe_at(&mut a, old);
        observe_at(&mut b, old);
        drop(t.begin_request(None, "a"));
        for _ in 0..3 {
            age(&t, "claude:cold", 3601);
            let (id, lease, moves) = choose(&t, Some("claude:cold"), &[a.clone(), b.clone()]);
            assert_eq!(id, "b");
            assert!(moves.is_empty());
            drop(lease);
        }
        observe_at(&mut a, Utc::now());
        let (id, first, _) = choose(&t, Some("claude:burst1"), &[a.clone(), b.clone()]);
        assert_eq!(id, "a");
        let (id, second, _) = choose(&t, Some("claude:burst2"), &[a, b]);
        assert_eq!(id, "b");
        drop((first, second));
    }

    #[test]
    fn traffic_before_observation_does_not_make_old_usage_stale() {
        let (_dir, t) = table();
        let now = Utc::now();
        let observed = now - Delta::minutes(20);
        let mut c = candidate("a", 91.0, 30, 2);
        observe_at(&mut c, observed);
        let mut d = Data::default();
        d.traffic.insert("a".into(), observed - Delta::seconds(1));
        assert!(!metrics(&c, &d, &t.config, now).unknown);
        assert!(!metrics(&c, &d, &t.config, now).gate);
        d.traffic.insert("a".into(), observed + Delta::seconds(1));
        assert!(metrics(&c, &d, &t.config, now).unknown);
        assert!(metrics(&c, &d, &t.config, now).gate);
        d.traffic.insert("a".into(), observed - Delta::seconds(1));
        d.pending.insert("a".into(), 1);
        assert!(metrics(&c, &d, &t.config, now).unknown);
        assert!(metrics(&c, &d, &t.config, now).gate);
        d.pending.clear();
        observe_at(&mut c, now - Delta::seconds(900));
        d.traffic.insert("a".into(), now);
        assert!(!metrics(&c, &d, &t.config, now).unknown);
    }

    #[test]
    fn reset_expired_usage_is_zero_and_cold_binding_stays() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 30, 2);
        let b = candidate("b", 5.0, 8, 1);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(100);
            w["reset_at"] = json!(Utc::now() - Delta::seconds(1));
            w["observed_at"] = json!(Utc::now() - Delta::hours(2));
        }
        let m = metrics(&a, &Data::default(), &t.config, Utc::now());
        assert!(m.gate && !m.unknown);
        assert_eq!(m.usage, 0.0);
        assert!(m.weekly.is_none() && m.five.is_none());
        age(&t, "claude:s", 3601);
        let (id, lease, moves) = choose(&t, Some("claude:s"), &[a, b]);
        assert_eq!(id, "a");
        assert!(moves.is_empty());
        drop(lease);
    }

    #[test]
    fn only_account_windows_gate_and_aliases_set_the_correct_threshold() {
        let (_dir, t) = table();
        let now = Utc::now();
        let mut c = candidate("a", 10.0, 30, 2);
        c.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap().extend([
            json!({"name":"7d_oi","used_pct":100,"reset_at":now+Delta::hours(1),"observed_at":now}),
            json!({"name":"60m","used_pct":100,"reset_at":now+Delta::hours(1),"observed_at":now}),
        ]);
        let m = metrics(&c, &Data::default(), &t.config, now);
        assert!(m.gate && !m.unknown);
        assert_eq!(m.usage, 10.0);
        assert_eq!(
            m.weekly,
            date(&c.quota.as_ref().unwrap()["windows"][1]["reset_at"])
        );
        c.quota.as_mut().unwrap()["windows"][0]["name"] = json!("primary");
        c.quota.as_mut().unwrap()["windows"][1] = json!({
            "name":"secondary","used_pct":97,"reset_at":now+Delta::hours(4),"observed_at":now
        });
        assert!(metrics(&c, &Data::default(), &t.config, now).gate);
        c.quota.as_mut().unwrap()["windows"][0]["used_pct"] = Value::Null;
        let m = metrics(&c, &Data::default(), &t.config, now);
        assert!(m.gate && m.unknown);
        c.quota.as_mut().unwrap()["windows"][1]["used_pct"] = json!(98);
        assert!(!metrics(&c, &Data::default(), &t.config, now).gate);
    }

    #[test]
    fn codex_nonstandard_window_slots_gate_at_1440_minute_boundary() {
        let now = Utc::now();
        let cfg = RoutingConfig {
            gate_5h_pct: 80.0,
            gate_weekly_pct: 90.0,
            ..RoutingConfig::default()
        };
        let mut c = codex_candidate("a", 0.0);
        for (name, weekly) in [
            ("60m", false),
            ("1440m", false),
            ("1441m", true),
            ("43200m", true),
        ] {
            c.quota = Some(json!({"windows":[{
                "name":name,"used_pct":85,"reset_at":now+Delta::hours(8),"observed_at":now
            }]}));
            let m = metrics(&c, &Data::default(), &cfg, now);
            assert_eq!(m.gate, weekly, "{name}: slot-specific threshold");
            assert!(!m.unknown);
            assert_eq!(m.weekly.is_some(), weekly);
            assert_eq!(m.five.is_some(), !weekly);
            c.quota.as_mut().unwrap()["windows"][0]["used_pct"] =
                json!(if weekly { 89 } else { 79 });
            assert!(metrics(&c, &Data::default(), &cfg, now).gate);
            c.inflight = 1;
            assert!(
                !metrics(&c, &Data::default(), &cfg, now).gate,
                "{name}: projection reaching the threshold fails the gate"
            );
            c.inflight = 0;
            c.quota.as_mut().unwrap()["windows"][0]["used_pct"] = json!(97);
            c.quota.as_mut().unwrap()["windows"][0]["reset_at"] = json!(now + Delta::hours(4));
            assert_eq!(
                metrics(&c, &Data::default(), &cfg, now).gate,
                weekly,
                "{name}: only weekly slots get the near-reset threshold"
            );
            c.quota.as_mut().unwrap()["windows"][0]["used_pct"] = json!(98);
            assert!(!metrics(&c, &Data::default(), &cfg, now).gate);
        }
        for name in ["0m", "-1m", "+60m", "1.5m", "60m_oi", "7d_oi", "unknown"] {
            assert_eq!(
                account_window(Provider::Codex, &json!({"name":name})),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn codex_same_slot_uses_highest_projected_window_for_ordering_but_all_gate() {
        let (_dir, t) = table();
        let now = Utc::now();
        // Both short and weekly collisions must pick the high-usage reset,
        // rather than the earliest reset of a less-used window in that slot.
        for (canonical, extra, first_hours, higher_hours, competitor_hours) in
            [("5h", "1440m", 1, 4, 2), ("7d", "43200m", 8, 30, 16)]
        {
            let mut a = codex_candidate("a", 0.0);
            a.inflight = 1;
            a.quota = Some(json!({"windows":[
                {"name":canonical,"used_pct":10,"reset_at":now+Delta::hours(first_hours),"observed_at":now},
                {"name":extra,"used_pct":80,"reset_at":now+Delta::hours(higher_hours),"observed_at":now}
            ]}));
            let mut b = codex_candidate("b", 0.0);
            b.quota = Some(json!({"windows":[
                {"name":extra,"used_pct":50,"reset_at":now+Delta::hours(competitor_hours),"observed_at":now}
            ]}));
            let d = Data::default();
            let m = metrics(&a, &d, &t.config, now);
            assert!(m.gate && !m.unknown);
            assert_eq!(m.usage, 81.0);
            assert_eq!(
                if canonical == "7d" { m.weekly } else { m.five },
                Some(now + Delta::hours(higher_hours))
            );
            let select = |cs: &[Candidate]| match t
                .select(&d, Provider::Codex, None, None, None, cs, None, now, false)
                .0
            {
                PreviewDecision::Selected { account } => account,
                other => panic!("{other:?}"),
            };
            assert_eq!(select(&[a.clone(), b.clone()]), "b");
            a.quota.as_mut().unwrap()["windows"]
                .as_array_mut()
                .unwrap()
                .reverse();
            assert_eq!(select(&[b.clone(), a.clone()]), "b");
            // Equal projected usage keeps a deterministic earlier-reset tie.
            for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
                w["used_pct"] = json!(80);
            }
            for _ in 0..2 {
                let tied = metrics(&a, &d, &t.config, now);
                assert_eq!(
                    if canonical == "7d" {
                        tied.weekly
                    } else {
                        tied.five
                    },
                    Some(now + Delta::hours(first_hours))
                );
                a.quota.as_mut().unwrap()["windows"]
                    .as_array_mut()
                    .unwrap()
                    .reverse();
            }
            // A near-reset high-usage window may pass its own relaxed gate,
            // while the less-used, far-reset window must still fail its gate.
            if canonical == "7d" {
                a.quota = Some(json!({"windows":[
                    {"name":extra,"used_pct":96,"reset_at":now+Delta::hours(4),"observed_at":now},
                    {"name":canonical,"used_pct":90,"reset_at":now+Delta::hours(30),"observed_at":now}
                ]}));
                let m = metrics(&a, &d, &t.config, now);
                assert_eq!(m.weekly, Some(now + Delta::hours(4)));
                assert!(
                    !m.gate,
                    "ordering representative cannot hide a failing window"
                );
            }
        }
    }

    #[test]
    fn round2_codex_free_43200m_at_91_fails_weekly_gate_with_d20_charge_preserved() {
        let now = Utc::now();
        let cfg = RoutingConfig::default();
        let mut c = codex_candidate("cx-free", 0.0);
        c.quota = Some(json!({"windows":[{
            "name":"43200m","used_pct":91,"reset_at":now+Delta::days(20),"observed_at":now
        }]}));
        let m = metrics(&c, &Data::default(), &cfg, now);
        assert!(!m.gate && !m.unknown);
        assert_eq!(m.weekly, Some(now + Delta::days(20)));
        assert_eq!(m.usage, 91.0);
        c.quota.as_mut().unwrap()["windows"][0]["used_pct"] = json!(89);
        let mut d = Data::default();
        let mut state = PlacementState::default();
        state.observe(c.quota.as_ref().unwrap());
        state.placed(now, 1);
        d.placements.insert(c.account.id.clone(), state);
        let m = metrics(&c, &d, &cfg, now);
        assert!(!m.gate && !m.unknown);
        assert_eq!(
            m.usage, 91.0,
            "D20 charge participates in the new weekly slot"
        );
        assert!(metrics_without_placement_charge(&c, &d, &cfg, now).gate);
    }

    #[test]
    fn codex_nonstandard_windows_keep_unknown_stale_and_past_reset_rules() {
        let now = Utc::now();
        let cfg = RoutingConfig::default();
        let mut c = codex_candidate("a", 0.0);
        for name in ["1440m", "43200m"] {
            c.quota = Some(json!({"windows":[{
                "name":name,"used_pct":null,"reset_at":now+Delta::hours(8),"observed_at":now
            }]}));
            let m = metrics(&c, &Data::default(), &cfg, now);
            assert!(m.gate && m.unknown && m.usage.is_infinite());
            c.quota.as_mut().unwrap()["windows"][0]["used_pct"] = json!(95);
            c.quota.as_mut().unwrap()["windows"][0]["observed_at"] =
                json!(now - Delta::seconds(901));
            assert!(!metrics(&c, &Data::default(), &cfg, now).gate);
            let mut d = Data::default();
            d.traffic.insert(c.account.id.clone(), now);
            let m = metrics(&c, &d, &cfg, now);
            assert!(m.gate && m.unknown && m.usage.is_infinite());
            // Exactly 900 seconds is still fresh, even after traffic.
            c.quota.as_mut().unwrap()["windows"][0]["observed_at"] =
                json!(now - Delta::seconds(900));
            assert!(!metrics(&c, &d, &cfg, now).gate);
            c.quota.as_mut().unwrap()["windows"][0]["reset_at"] = json!(now - Delta::seconds(1));
            let m = metrics(&c, &d, &cfg, now);
            assert!(m.gate && !m.unknown);
            assert_eq!(m.usage, 0.0);
            assert!(m.weekly.is_none() && m.five.is_none());
        }
    }

    #[test]
    fn claude_numeric_and_model_subbuckets_never_gate_or_order() {
        let (_dir, t) = table();
        let now = Utc::now();
        let mut a = candidate("a", 10.0, 30, 2);
        let expected = metrics(&a, &Data::default(), &t.config, now);
        for name in [
            "60m",
            "1440m",
            "1441m",
            "43200m",
            "7d_oi",
            "7d_sonnet",
            "43200m_oi",
        ] {
            a.quota.as_mut().unwrap()["windows"]
                .as_array_mut()
                .unwrap()
                .push(json!({
                    "name":name,"used_pct":100,"reset_at":now+Delta::minutes(1),"observed_at":now
                }));
        }
        let m = metrics(&a, &Data::default(), &t.config, now);
        assert!(m.gate && !m.unknown);
        assert_eq!(m.usage, expected.usage);
        assert_eq!(m.weekly, expected.weekly);
        assert_eq!(m.five, expected.five);
        // An ignored early-reset subbucket cannot beat B's real account reset.
        let b = candidate("b", 20.0, 8, 3);
        assert_eq!(preview(&t, &[a, b]), "b");
    }

    #[test]
    fn request_local_hold_timeout_does_not_disable_next_request() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        let until = Utc::now() + Delta::seconds(30);
        a.account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until,
            scope: "account".into(),
        };
        a.account.quota_cooling = Some(crate::accounts::QuotaCooling {
            window: "5h".into(),
            reset_at: until,
            started_at: Utc::now(),
        });
        assert!(matches!(
            t.choose_with_hold(
                Provider::Claude,
                Some("claude:s"),
                None,
                None,
                std::slice::from_ref(&a),
                None,
                0,
                false,
                true
            ),
            Decision::None
        ));
        assert!(
            matches!(t.choose(Provider::Claude, Some("claude:s"), None, None,
            &[a], None, 0), Decision::Hold { until: u } if u == until)
        );
    }

    #[test]
    fn projected_credit_usage_never_constructs_cooling_hold_or_affinity_exclusion() {
        let (_dir, t) = table();
        let mut a = codex_candidate("a", 98.0);
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["reset_at"] = json!(Utc::now() + Delta::seconds(30));
        }
        drop(t.choose(
            Provider::Codex,
            Some("codex:s"),
            None,
            None,
            std::slice::from_ref(&a),
            None,
            0,
        ));
        a.inflight = 1;
        assert_eq!(
            effective_account(&a, &Data::default(), &t.config, None, Utc::now()).health,
            Health::Ready
        );
        assert!(matches!(
            t.choose(Provider::Codex, Some("codex:s"), None, None, &[a], None, 0),
            Decision::Selected { account, moves, .. } if account == "a" && moves.is_empty()
        ));
    }

    #[test]
    fn new_group_and_debug_pin_events_have_group_metadata_without_duplicate_bytes() {
        let (_dir, t) = table();
        let a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 10.0, 30, 2);
        let result = t.choose(
            Provider::Claude,
            Some("claude:s:agent:c"),
            Some("claude:s"),
            None,
            std::slice::from_ref(&a),
            None,
            77,
        );
        if let Decision::Selected { lease, moves, .. } = result {
            assert!(!lease.after_move);
            assert_eq!(moves.len(), 1);
            assert_eq!(moves[0].key, "claude:s");
            assert_eq!(moves[0].keys, 2);
            assert_eq!(moves[0].reason, "new");
            assert_eq!(moves[0].bytes, 77);
            assert!(!moves[0].debug_pin);
        } else {
            panic!("new group not selected");
        }
        let result = with_pin(
            Provider::Claude,
            Some("claude:s:agent:c"),
            "b",
            |admission| {
                t.choose_with_hold_at_admitted_proof(
                    Provider::Claude,
                    Some("claude:s:agent:c"),
                    None,
                    None,
                    &[a.clone(), b.clone()],
                    &[a, b],
                    MoveObservations::untracked(),
                    Some("b"),
                    81,
                    true,
                    false,
                    Utc::now(),
                    None,
                    |_| Some(admission),
                )
            },
        );
        if let Decision::Selected { lease, moves, .. } = result {
            assert!(lease.after_move);
            assert_eq!(moves.len(), 1);
            assert_eq!(moves[0].keys, 2);
            assert_eq!(moves[0].bytes, 81);
            assert_eq!(moves[0].reason, "forced");
            assert!(moves[0].debug_pin);
        } else {
            panic!("pinned group not selected");
        }
        assert_eq!(t.account_stats("a").0, 0);
        assert_eq!(t.account_stats("b").0, 2);
    }

    #[test]
    fn inconsistent_group_is_salvaged_without_losing_valid_groups() {
        let now = Utc::now();
        let d = decode(json!({"schema":1,"bindings":[
            {"key":"claude:bad","account":"a","last_used":now},
            {"key":"claude:bad:agent:c","account":"b","last_used":now},
            {"key":"codex:good","account":"a","last_used":now}
        ]}))
        .unwrap();
        assert_eq!(d.bindings.len(), 1);
        assert!(d.bindings.contains_key("codex:good"));
        assert_eq!(d.groups.len(), 1);
        assert_eq!(d.accounts["a"].count, 1);
        assert!(d.traffic.is_empty());
        assert!(d.dirty);
    }

    #[test]
    fn poisoned_mutexes_recover_for_selection_stats_completion_and_flush() {
        let (_dir, t) = table();
        drop(choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = t.data.lock().unwrap();
            panic!("synthetic poison");
        }));
        assert!(panic.is_err());
        assert_eq!(t.current(Some("claude:s")).as_deref(), Some("a"));
        assert_eq!(t.count(), 1);
        assert_eq!(t.account_stats("a").0, 1);
        drop(choose(&t, Some("claude:s"), &[candidate("a", 10.0, 8, 1)]));
        let pin = t.pin(Some("claude:s"));
        drop(pin);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = t.writer.lock().unwrap();
            panic!("synthetic writer poison");
        }));
        t.flush().unwrap();
    }

    #[test]
    fn pinned_socket_keeps_cold_binding_even_when_gate_fails() {
        let (_dir, t) = cold_table();
        let mut a = candidate("a", 10.0, 30, 2);
        let b = candidate("b", 5.0, 8, 1);
        drop(choose(&t, Some("claude:s"), std::slice::from_ref(&a)));
        let pin = t.pin(Some("claude:s"));
        age(&t, "claude:s", 3601);
        for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
            w["used_pct"] = json!(91);
        }
        let (id, lease, moves) = choose(&t, Some("claude:s"), &[a.clone(), b.clone()]);
        assert_eq!(id, "a");
        assert!(moves.is_empty());
        drop(lease);
        drop(pin);
        age(&t, "claude:s", 3601);
        assert_eq!(choose(&t, Some("claude:s"), &[a, b]).0, "b");
    }
    fn assert_indexes(d: &Data) {
        let mut expected = d.clone();
        rebuild_indexes(&mut expected);
        assert_eq!(d.lru, expected.lru);
        assert_eq!(d.groups.len(), expected.groups.len());
        assert_eq!(d.accounts.len(), expected.accounts.len());
        for (g, entry) in &d.groups {
            let next = &expected.groups[g];
            assert_eq!(entry.keys, next.keys);
            assert_eq!(entry.account, next.account);
            assert_eq!(entry.last_used, next.last_used);
        }
        for (id, entry) in &d.accounts {
            assert_eq!(entry.count, expected.accounts[id].count);
            assert_eq!(entry.recent, expected.accounts[id].recent);
        }
    }

    fn count_change_place(t: &Arc<BindingTable>, key: &str, id: &str, now: DateTime<Utc>) {
        let c = candidate(id, 10.0, 8, 1);
        let decision = with_pin(Provider::Claude, Some(key), id, |admission| {
            t.choose_with_hold_at_admitted_proof(
                Provider::Claude,
                Some(key),
                None,
                None,
                std::slice::from_ref(&c),
                std::slice::from_ref(&c),
                MoveObservations::untracked(),
                Some(id),
                0,
                false,
                false,
                now,
                None,
                |_| Some(admission),
            )
        });
        let Decision::Selected { account, lease, .. } = decision else {
            panic!("{decision:?}");
        };
        assert_eq!(account, id);
        drop(lease);
    }

    #[test]
    fn r13_count_change_move_records_both_accounts_and_change_back_between_polls() {
        let (_dir, t) = table();
        let now = Utc::now();
        assert_eq!(t.account_count_changed_at("a"), None);
        count_change_place(&t, "claude:count", "a", now);
        assert_eq!(t.account_count_changed_at("a"), Some(now));
        // These historical returns respect R14 without changing the count-publication test.
        let moved = now + Delta::minutes(31);
        count_change_place(&t, "claude:count", "b", moved);
        assert_eq!(t.account_count_changed_at("a"), Some(moved));
        assert_eq!(t.account_count_changed_at("b"), Some(moved));
        let back = now + Delta::minutes(62);
        count_change_place(&t, "claude:count", "a", back);
        // No timestamp API poll between the two committed moves below.
        count_change_place(&t, "claude:count", "b", back + Delta::minutes(31));
        count_change_place(&t, "claude:count", "a", back + Delta::minutes(62));
        assert_eq!(t.account_stats("a").0, 1);
        assert_eq!(t.account_stats("b").0, 0);
        for id in ["a", "b"] {
            assert_eq!(
                t.account_count_changed_at(id),
                Some(back + Delta::minutes(62))
            );
        }
    }

    #[test]
    fn r13_count_change_launch_adoption_merge_and_abandonment_use_mutation_clock() {
        let (_dir, t) = table();
        let now = Utc::now();
        let mut c = candidate("a", 10.0, 8, 1);
        c.account.provider = Provider::Codex;
        c.account.identity = Identity::Codex {
            user_id: None,
            account_id: "a".into(),
            email: "synthetic@example.invalid".into(),
        };
        let entitlements = BTreeMap::from([("a".into(), BTreeSet::from(["m1".into()]))]);
        for (anchor, at) in [
            ("launch:first", now),
            ("launch:second", now + Delta::seconds(1)),
        ] {
            t.place_launch(
                LaunchRequest {
                    anchor_key: anchor,
                    resume_key: None,
                    model: Some("m1"),
                    model_is_explicit: true,
                    account_pref: Some("a"),
                    allow_move_for_model: false,
                },
                std::slice::from_ref(&c),
                &entitlements,
                at,
            )
            .unwrap();
        }
        assert_eq!(t.account_stats("a").0, 2);
        let placed = now + Delta::seconds(1);
        assert_eq!(t.account_count_changed_at("a"), Some(placed));
        t.resolve_launch(
            Some("launch:first"),
            Some("codex:real"),
            now + Delta::seconds(2),
        )
        .unwrap();
        assert_eq!(t.account_count_changed_at("a"), Some(placed));
        let merged = now + Delta::seconds(3);
        t.resolve_launch(Some("launch:second"), Some("codex:real"), merged)
            .unwrap();
        assert_eq!(t.account_stats("a").0, 1);
        assert_eq!(t.account_count_changed_at("a"), Some(merged));
        let expired = now + Delta::days(10);
        {
            let mut d = t.data.lock().unwrap();
            t.prune(&mut d, expired);
            assert!(d.bindings.is_empty());
        }
        assert_eq!(t.account_count_changed_at("a"), Some(expired));
    }

    #[test]
    fn r13_count_change_expiry_and_import_clear_retain_zero_count_history() {
        let (_dir, t) = table();
        let now = Utc::now();
        count_change_place(&t, "claude:expire", "a", now);
        let expired = now + Delta::seconds(t.config.session_binding_max_secs as i64 + 1);
        {
            let mut d = t.data.lock().unwrap();
            t.prune(&mut d, expired);
            assert!(d.bindings.is_empty());
        }
        assert_eq!(t.account_count_changed_at("a"), Some(expired));
        count_change_place(&t, "claude:clear", "b", now);
        let before = Utc::now();
        t.import(json!({"schema":1,"bindings":[]})).unwrap();
        let removed = t.account_count_changed_at("b").unwrap();
        assert!(removed >= before && removed <= Utc::now());
        assert_eq!(t.account_stats("b").0, 0);
        assert_eq!(t.account_count_changed_at("a"), Some(expired));
    }

    #[test]
    fn r13_count_change_touch_and_same_count_membership_replacement_are_unchanged() {
        let (_dir, t) = table();
        let now = Utc::now();
        count_change_place(&t, "claude:old", "a", now);
        count_change_place(&t, "claude:old", "a", now + Delta::seconds(1));
        t.touch(Some("claude:old"));
        {
            let mut d = t.data.lock().unwrap();
            touch_group(&mut d, "claude:old", now + Delta::seconds(2));
        }
        let mut state = t.export().unwrap();
        state["bindings"][0]["key"] = json!("claude:replacement");
        t.import(state).unwrap();
        assert_eq!(t.account_stats("a").0, 1);
        assert_eq!(t.account_count_changed_at("a"), Some(now));
        let d = t.data.lock().unwrap();
        assert!(d.bindings.contains_key("claude:replacement"));
        assert!(d.count_before.is_empty());
        let revision = d.revision;
        let dirty = d.dirty;
        drop(d);
        assert_eq!(t.account_count_changed_at("unknown"), None);
        assert_eq!(t.account_count_changed_at("a"), Some(now));
        let d = t.data.lock().unwrap();
        assert_eq!((d.revision, d.dirty), (revision, dirty));
    }

    #[test]
    fn r13_count_change_history_is_bounded_and_restart_starts_cold() {
        let (_dir, t) = table();
        let now = Utc::now();
        count_change_place(&t, "claude:cold", "a", now);
        let recovered = decode(t.export().unwrap()).unwrap();
        assert!(recovered.count_changes.is_empty());
        assert!(recovered.count_change_order.is_empty());
        assert!(recovered.count_before.is_empty());

        // Exercise the shared index/commit hooks without retaining bindings.
        let mut d = Data::default();
        for n in 0..COUNT_CHANGE_CAPACITY + 2 {
            let id = format!("account-{n:06}");
            let at = now + Delta::seconds(n as i64);
            d.count_change_clock = Some(at);
            index_group(
                &mut d,
                "claude:one".into(),
                Group {
                    keys: BTreeSet::from(["claude:one".into()]),
                    account: id.clone(),
                    last_used: at,
                },
            );
            d.changed();
            unindex_group(&mut d, "claude:one").unwrap();
            d.changed();
            assert_eq!(d.count_changes.get(&id), Some(&at));
            assert!(d.count_changes.len() <= COUNT_CHANGE_CAPACITY);
            assert_eq!(d.count_changes.len(), d.count_change_order.len());
            assert!(d.count_before.is_empty());
        }
        assert_eq!(d.count_changes.len(), COUNT_CHANGE_CAPACITY);
        assert!(!d.count_changes.contains_key("account-000000"));
        assert!(!d.count_changes.contains_key("account-000001"));
        assert!(d.accounts.is_empty());
    }

    #[test]
    fn indexes_track_group_moves_completion_pin_release_and_expiry() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let b = candidate("b", 10.0, 30, 2);
        let (_, old, _) = choose(&t, Some("claude:s"), std::slice::from_ref(&a));
        drop(t.choose(
            Provider::Claude,
            Some("claude:s:agent:c"),
            Some("claude:s"),
            None,
            std::slice::from_ref(&a),
            None,
            0,
        ));
        let pin1 = t.pin(Some("claude:s"));
        let pin2 = t.pin(Some("claude:s"));
        assert_indexes(&t.data.lock().unwrap());
        a.account.user_state = UserState::Paused;
        let (_, new, _) = choose(&t, Some("claude:s"), &[a, b]);
        assert_indexes(&t.data.lock().unwrap());
        drop(old);
        assert_eq!(t.account_stats("a"), (0, None));
        assert_eq!(t.account_stats("b").0, 2);
        drop(new);
        drop(pin1);
        assert_indexes(&t.data.lock().unwrap());
        assert!(t.data.lock().unwrap().lru.is_empty());
        drop(pin2);
        assert_indexes(&t.data.lock().unwrap());
        assert_eq!(t.data.lock().unwrap().lru.len(), 1);
        t.touch(Some("claude:s:agent:unknown"));
        assert_indexes(&t.data.lock().unwrap());
        age(&t, "claude:s", 700000);
        assert_eq!(t.count(), 0);
        let d = t.data.lock().unwrap();
        assert!(d.groups.is_empty() && d.accounts.is_empty() && d.lru.is_empty());
    }
    #[test]
    fn completion_does_not_record_traffic_newer_than_response_observation() {
        let (_dir, t) = table();
        let lease = t.begin_request(None, "a");
        let started = t.data.lock().unwrap().traffic["a"];
        let mut c = candidate("a", 91.0, 30, 2);
        let observed = started + Delta::seconds(1);
        observe_at(&mut c, observed);
        drop(lease);
        let d = t.data.lock().unwrap();
        assert_eq!(d.traffic["a"], started);
        let m = metrics(&c, &d, &t.config, observed + Delta::minutes(20));
        assert!(!m.unknown);
        assert!(!m.gate);
    }

    #[test]
    fn weekly_cohort_includes_exact_three_hours_but_not_one_second_more() {
        let (_dir, t) = table();
        let now = Utc::now();
        let mut a = candidate("a", 10.0, 8, 4);
        let mut b = candidate("b", 10.0, 11, 2);
        let mut c = candidate("c", 10.0, 11, 1);
        a.quota.as_mut().unwrap()["windows"][1]["reset_at"] = json!(now + Delta::hours(8));
        b.quota.as_mut().unwrap()["windows"][1]["reset_at"] = json!(now + Delta::hours(11));
        c.quota.as_mut().unwrap()["windows"][1]["reset_at"] =
            json!(now + Delta::hours(11) + Delta::seconds(1));
        assert_eq!(preview(&t, &[c.clone(), b.clone(), a.clone()]), "b");
        assert_eq!(preview(&t, &[a, c, b]), "b");
    }

    #[test]
    fn missing_weekly_reset_sorts_after_known_then_uses_five_hour_order() {
        let (_dir, t) = table();
        let mut a = candidate("a", 10.0, 8, 1);
        let mut b = candidate("b", 10.0, 30, 2);
        a.quota.as_mut().unwrap()["windows"][1]["reset_at"] = Value::Null;
        assert_eq!(preview(&t, &[a.clone(), b.clone()]), "b");
        b.quota.as_mut().unwrap()["windows"][1]["reset_at"] = Value::Null;
        assert_eq!(preview(&t, &[b, a]), "a");
    }

    #[test]
    fn twenty_thousand_group_lookups_stats_and_completion_keep_indexes_consistent() {
        let (_dir, t) = table();
        let now = Utc::now();
        let mut bindings = Vec::with_capacity(CAPACITY);
        for i in 0..CAPACITY {
            bindings.push(json!({
                "key":format!("claude:g{i}"),
                "account":if i % 2 == 0 { "a" } else { "b" },
                "last_used":now
            }));
        }
        t.import(json!({"schema":1,"bindings":bindings})).unwrap();
        assert_eq!(t.count(), CAPACITY);
        assert_eq!(t.account_stats("a"), (CAPACITY / 2, Some(now)));
        assert_eq!(t.account_stats("b"), (CAPACITY / 2, Some(now)));
        // Missing child key resolves directly through its group index.
        assert_eq!(
            t.current(Some("claude:g0:agent:unknown")).as_deref(),
            Some("a")
        );
        let a = candidate("a", 10.0, 30, 2);
        let (id, lease, moves) = choose(&t, Some("claude:g0"), &[a]);
        assert_eq!(id, "a");
        assert!(moves.is_empty());
        assert_eq!(t.data.lock().unwrap().lru.len(), CAPACITY - 1);
        drop(lease);
        assert_eq!(t.data.lock().unwrap().lru.len(), CAPACITY);
        assert_eq!(t.account_stats("a").0, CAPACITY / 2);
        assert!(t.account_stats("a").1.unwrap() >= now);
        assert_indexes(&t.data.lock().unwrap());
    }
    #[test]
    fn projected_99_or_100_keeps_single_actual_eligible_account_in_2b() {
        let (_dir, t) = table();
        let mut a = codex_candidate("a", 98.0);
        let reservation = t.begin_request(None, "a");
        let m = metrics(&a, &t.data.lock().unwrap(), &t.config, Utc::now());
        assert_eq!(m.usage, 99.0);
        assert!(!m.gate);
        for pending in [1, 2] {
            a.inflight = pending;
            let result = t.choose(
                Provider::Codex,
                Some("codex:2b"),
                None,
                None,
                std::slice::from_ref(&a),
                None,
                0,
            );
            assert!(matches!(result, Decision::Selected { account, .. } if account == "a"));
        }
        // Projection still directs a new group to an account passing the gate.
        let b = codex_candidate("b", 10.0);
        assert_eq!(
            t.preview(
                Provider::Codex,
                Some("codex:new"),
                None,
                None,
                &[a.clone(), b],
                None,
                0
            ),
            PreviewDecision::Selected {
                account: "b".into()
            }
        );
        // Observed credit thresholds remain actual C5 exclusions.
        a.quota.as_mut().unwrap()["credits"] = json!({"has_credits":true});
        for observed in [99, 100] {
            for w in a.quota.as_mut().unwrap()["windows"].as_array_mut().unwrap() {
                w["used_pct"] = json!(observed);
            }
            assert!(matches!(
                t.choose(
                    Provider::Codex,
                    Some("codex:2b"),
                    None,
                    None,
                    std::slice::from_ref(&a),
                    None,
                    0
                ),
                Decision::None
            ));
        }
        drop(reservation);
    }
    #[test]
    fn preview_at_twenty_thousand_groups_uses_read_only_indexes_and_reservations() {
        let (_dir, t) = table();
        let now = Utc::now();
        let bindings: Vec<_> = (0..CAPACITY)
            .map(|i| {
                json!({
                    "key":format!("claude:g{i}"), "account":"a", "last_used":now
                })
            })
            .collect();
        t.import(json!({"schema":1,"bindings":bindings})).unwrap();
        let a = candidate("a", 89.0, 8, 1);
        let b = candidate("b", 10.0, 30, 2);
        let reservation = t.begin_request(None, "a");
        let before = t.export().unwrap();
        let revision = t.data.lock().unwrap().revision;
        for key in [Some("claude:g0"), Some("claude:g19999:agent:missing")] {
            assert!(matches!(
                t.preview(
                    Provider::Claude,
                    key,
                    None,
                    None,
                    &[a.clone(), b.clone()],
                    None,
                    0
                ),
                PreviewDecision::Selected { .. }
            ));
        }
        assert_eq!(
            t.preview(
                Provider::Claude,
                Some("claude:new"),
                None,
                None,
                &[a.clone(), b.clone()],
                None,
                0
            ),
            PreviewDecision::Selected {
                account: "b".into()
            }
        );
        assert_eq!(t.export().unwrap(), before);
        assert_eq!(t.data.lock().unwrap().revision, revision);
        assert_eq!(t.pending_count("a"), 1);
        // An expired binding is absent in the read view, while its persisted
        // entry and all indexes remain untouched until ordinary pruning.
        age(&t, "claude:g0", 700000);
        let before = t.export().unwrap();
        assert_eq!(
            t.preview(
                Provider::Claude,
                Some("claude:g0"),
                None,
                None,
                &[a, b],
                None,
                0
            ),
            PreviewDecision::Selected {
                account: "b".into()
            }
        );
        assert_eq!(t.export().unwrap(), before);
        assert_eq!(t.data.lock().unwrap().bindings.len(), CAPACITY);
        assert_indexes(&t.data.lock().unwrap());
        drop(reservation);
    }
}
