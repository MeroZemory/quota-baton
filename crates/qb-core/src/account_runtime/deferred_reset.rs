//! Process-local launch-owned reset obligations, never expired proof tickets.
use super::{AccountRuntime, wait_owner::ObservedHttp429};
use crate::{
    accounts::{Account, Health, HealthReason, Provider},
    bindings::{BindingTable, DeferredBindingFence, HttpWaitRequest, WaitIntent},
    issue_operation::IssueOperation,
    launch_tokens::{LaunchOwnerScope, LaunchPin},
    routing::{DeferredPolicyFence, Plan},
};
use chrono::{DateTime, Utc};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};
use tokio::{sync::Notify, time::Instant};

const CAPACITY: usize = 4096;
const WAITING: u8 = 0;
const SUSPENDED: u8 = 1;
const EXPIRING: u8 = 2;
const READY: u8 = 3;
const CANCELLED: u8 = 4;
const CONSUMED: u8 = 5;
const RECOVER_CREDENTIAL: u8 = 1;
const RECOVER_EXPIRY: u8 = 2;
const BLOCK_REPORT: u8 = 4;

/// Native typed fences can only revoke; they never construct report authority.
pub(crate) struct Revocation {
    live: AtomicBool,
    changed: Notify,
}
impl Revocation {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            live: AtomicBool::new(true),
            changed: Notify::new(),
        })
    }
    pub(crate) fn live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
    pub(crate) fn cancel(&self) {
        self.live.store(false, Ordering::Release);
        self.changed.notify_waiters();
    }
    pub(crate) fn notified_raw(&self) -> tokio::sync::futures::Notified<'_> {
        self.changed.notified()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeferredResetId(u64);
impl DeferredResetId {
    pub(crate) fn as_u64(self) -> u64 {
        self.0
    }
    // A parsed lookup ID alone grants no authority; every API checks owner scope.
    pub(crate) fn from_u64(id: u64) -> Self {
        Self(id)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeferredResetStatus {
    Empty,
    Waiting {
        id: DeferredResetId,
        reset_at: DateTime<Utc>,
        wake: Instant,
    },
    Suspended {
        id: DeferredResetId,
        reset_at: DateTime<Utc>,
        wake: Instant,
    },
    Cancelled,
}
/// Inspection is a gated hint; only Consume can authorize a native resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeferredResetInspection {
    Ready,
    Pending,
    Refused,
}
/// A single final consumption, not credentials, account choice or send permission.
pub(crate) struct DeferredResumePermit {
    _private: (),
}

pub(super) struct PhysicalQualification {
    pub(super) reset_at: DateTime<Utc>,
    pub(super) started_at: DateTime<Utc>,
    pub(super) scope: String,
    pub(super) allow_credits: bool,
    pub(super) quota_credit_releasable: bool,
}
// Finite current evidence for this original registered episode only.
struct ReadinessStamp {
    generation: u64,
    observed_at: DateTime<Utc>,
    fresh_until: DateTime<Utc>,
    mono_until: Instant,
    revision: u64,
}
struct Record {
    id: DeferredResetId,
    owner: LaunchOwnerScope,
    account: String,
    identity: crate::accounts::Identity,
    source: crate::accounts::Source,
    root: String,
    model: String,
    policy_key: String,
    plan: Plan,
    qualification: PhysicalQualification,
    wake: Instant,
    binding: Arc<DeferredBindingFence>,
    policy: Arc<DeferredPolicyFence>,
    phase: AtomicU8,
    recovery: AtomicU8,
    expiry_generation: AtomicU64,
    readiness: Mutex<Option<ReadinessStamp>>,
    readiness_revision: AtomicU64,
}
impl Record {
    fn readiness_current(
        &self,
        account: &Account,
        wall: DateTime<Utc>,
        mono: Instant,
    ) -> Option<bool> {
        let Ok(stamp) = self.readiness.lock() else {
            return Some(false);
        };
        stamp.as_ref().map(|stamp| {
            stamp.generation == account.generation
                && stamp.observed_at <= wall
                && stamp.observed_at >= self.qualification.reset_at
                && wall < stamp.fresh_until
                && mono < stamp.mono_until
                && stamp.revision == self.readiness_revision.load(Ordering::Acquire)
        })
    }
    fn live(&self) -> bool {
        self.owner.live()
            && self.binding.live()
            && self.policy.live()
            && self.phase.load(Ordering::Acquire) < CANCELLED
    }
    fn same_identity(&self, account: &Account) -> bool {
        account.id == self.account
            && account.provider == Provider::Codex
            && account.identity == self.identity
            && account.source == self.source
            && account.allow_credits == self.qualification.allow_credits
    }
    fn same_hold(&self, account: &Account) -> bool {
        self.same_identity(account)
            // This belongs to the original cooling hold, not account identity:
            // native expiry clears it before the final consume transaction.
            && account.quota_credit_releasable == self.qualification.quota_credit_releasable
            && matches!(&account.health, Health::Cooling { reason: HealthReason::Quota, until, scope }
                if *until == self.qualification.reset_at && *scope == self.qualification.scope)
            && account.quota_cooling.as_ref().is_some_and(|hold| {
                hold.window == "5h"
                    && hold.reset_at == self.qualification.reset_at
                    && hold.started_at == self.qualification.started_at
            })
    }
}
struct State {
    next: u64,
    closed: bool,
    records: BTreeMap<u64, Arc<Record>>,
}
pub(super) struct DeferredResets {
    identity: Arc<()>,
    state: Mutex<State>,
    changed: Notify,
}
impl DeferredResets {
    pub(super) fn new() -> Self {
        Self {
            identity: Arc::new(()),
            state: Mutex::new(State {
                next: 0,
                closed: false,
                records: BTreeMap::new(),
            }),
            changed: Notify::new(),
        }
    }
    fn register(
        &self,
        owner: &LaunchOwnerScope,
        intent: &WaitIntent<'_>,
        qualification: PhysicalQualification,
        account: &Account,
        binding: Arc<DeferredBindingFence>,
        policy: Arc<DeferredPolicyFence>,
    ) -> Option<DeferredResetId> {
        // Complete bounded allocations before the final leaf publication/check.
        let wall = Utc::now();
        let mono = Instant::now();
        let wake = mono.checked_add(
            qualification
                .reset_at
                .signed_duration_since(wall)
                .to_std()
                .ok()?,
        )?;
        let model = intent.model()?.to_owned();
        let mut state = self.state.lock().ok()?;
        if state.closed {
            return None;
        }
        let id = DeferredResetId(state.next.checked_add(1)?);
        // Retire only unusable entries; live obligations are never evicted for room.
        let dead: Vec<_> = state
            .records
            .iter()
            .filter(|(_, r)| !r.live())
            .map(|(id, _)| *id)
            .collect();
        let removed: Vec<_> = dead
            .into_iter()
            .filter_map(|id| state.records.remove(&id))
            .collect();
        if state.records.len() >= CAPACITY {
            drop(state);
            drop(removed);
            return None;
        }
        let record = Arc::new(Record {
            id,
            owner: owner.clone(),
            account: intent.account().to_owned(),
            identity: account.identity.clone(),
            source: account.source.clone(),
            root: intent.key().to_owned(),
            model,
            policy_key: intent.policy_key().to_owned(),
            plan: intent.plan().clone(),
            qualification,
            wake,
            binding,
            policy,
            phase: AtomicU8::new(WAITING),
            recovery: AtomicU8::new(0),
            expiry_generation: AtomicU64::new(0),
            readiness: Mutex::new(None),
            readiness_revision: AtomicU64::new(0),
        });
        let _owner_live = owner.live_guard()?;
        if !intent.caller_current()
            || !record.live()
            || !owner.claim(
                intent.key(),
                id.0,
                record.qualification.started_at,
                &self.identity,
            )
        {
            drop(state);
            drop(removed);
            return None;
        }
        state.next = id.0;
        state.records.insert(id.0, record);
        drop(_owner_live);
        drop(state);
        drop(removed);
        self.changed.notify_waiters();
        Some(id)
    }
    fn record(&self, owner: &LaunchOwnerScope, id: DeferredResetId) -> Option<Arc<Record>> {
        let state = self.state.lock().ok()?;
        if state.closed {
            return None;
        }
        let record = state.records.get(&id.0)?;
        (record.owner.same_owner(owner) && owner.episode_id().ok()? == Some(id.0) && record.live())
            .then(|| record.clone())
    }
    // Caller holds Store, after exact accepted raw UsagePoll persistence and
    // runtime/probe/credential gates. No registration, reset or wake is changed.
    pub(super) fn requalify_readiness(
        &self,
        account: &Account,
        readiness: &super::wait_owner::AppliedReadiness,
    ) {
        let wall = Utc::now();
        let mono = Instant::now();
        if !super::wait_owner::readiness_native_current(account, wall)
            || readiness.observed_at > wall
            || readiness.fresh_until <= wall
        {
            return;
        }
        let Some(mono_until) = readiness
            .fresh_until
            .signed_duration_since(wall)
            .to_std()
            .ok()
            .and_then(|remaining| mono.checked_add(remaining))
        else {
            return;
        };
        let Ok(state) = self.state.lock() else {
            return;
        };
        if state.closed {
            return;
        }
        let mut changed = false;
        for record in state.records.values().filter(|r| r.account == account.id) {
            if !record.live()
                || record.phase.load(Ordering::Acquire) != SUSPENDED
                || !record.same_identity(account)
                || wall < record.qualification.reset_at
                || mono < record.wake
                || readiness.observed_at < record.qualification.reset_at
                || !account.eligible_at(Some(&record.model), wall)
                || !record
                    .owner
                    .matches_thread(record.root.strip_prefix("codex:").unwrap_or(""))
                || record.owner.episode_id().ok().flatten() != Some(record.id.0)
                || !(record.qualification.scope == "account"
                    || (record.qualification.scope == format!("model:{}", record.model)
                        && readiness.model.as_deref() == Some(record.model.as_str())))
                || readiness
                    .model
                    .as_deref()
                    .is_some_and(|model| model != record.model)
            {
                continue;
            }
            let Some(revision) = record
                .readiness_revision
                .load(Ordering::Acquire)
                .checked_add(1)
            else {
                continue;
            };
            let Ok(mut stamp) = record.readiness.lock() else {
                continue;
            };
            // Leaf gate serializes cancellation/consumption; a dead episode is
            // never revived. Owner is rechecked with the existing leaf guard.
            let Some(_owner_live) = record.owner.live_guard() else {
                continue;
            };
            if !record.live() || record.phase.load(Ordering::Acquire) != SUSPENDED {
                continue;
            }
            *stamp = Some(ReadinessStamp {
                generation: account.generation,
                observed_at: readiness.observed_at,
                fresh_until: readiness.fresh_until,
                mono_until,
                revision,
            });
            record.readiness_revision.store(revision, Ordering::Release);
            record.recovery.store(0, Ordering::Release);
            record.phase.store(READY, Ordering::Release);
            changed = true;
        }
        drop(state);
        if changed {
            self.changed.notify_waiters();
        }
    }

    pub(super) fn requalify_account(
        &self,
        id: &str,
        qualification: &PhysicalQualification,
        model: Option<&str>,
    ) {
        let Ok(state) = self.state.lock() else {
            return;
        };
        if state.closed {
            return;
        }
        for record in state
            .records
            .values()
            .filter(|r| r.account == id && r.live())
        {
            let same = record.qualification.reset_at == qualification.reset_at
                && record.qualification.started_at == qualification.started_at
                && record.qualification.scope == qualification.scope
                && record.qualification.allow_credits == qualification.allow_credits
                && record.qualification.quota_credit_releasable
                    == qualification.quota_credit_releasable
                && model.is_none_or(|reported| reported == record.model);
            if !same {
                record.phase.store(CANCELLED, Ordering::Release);
            } else {
                if record.phase.load(Ordering::Acquire) == SUSPENDED {
                    let Some(revision) = record
                        .readiness_revision
                        .load(Ordering::Acquire)
                        .checked_add(1)
                    else {
                        continue;
                    };
                    record.readiness_revision.store(revision, Ordering::Release);
                }
                record.recovery.store(0, Ordering::Release);
                let _ = record.phase.compare_exchange(
                    SUSPENDED,
                    WAITING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }
    pub(super) fn cancel_account(&self, id: &str) {
        self.change_account(id, CANCELLED, false, 0);
    }
    pub(super) fn suspend_account(&self, id: &str) {
        self.change_account(id, SUSPENDED, false, BLOCK_REPORT);
    }
    pub(super) fn credential_save(&self, id: &str) {
        self.change_account(id, SUSPENDED, false, RECOVER_CREDENTIAL);
    }
    pub(super) fn expiry(&self, id: &str) {
        self.change_account(id, SUSPENDED, true, RECOVER_EXPIRY);
    }
    fn change_account(&self, id: &str, phase: u8, keep_expiring: bool, recovery: u8) {
        let Ok(state) = self.state.lock() else {
            return;
        };
        for record in state.records.values().filter(|record| record.account == id) {
            let old = record.phase.load(Ordering::Acquire);
            if old < CANCELLED && !(keep_expiring && old == EXPIRING) {
                // Once fresh readiness is cut, no leftover native-expiry
                // generation may substitute for another accepted raw poll.
                match record.readiness.lock() {
                    Ok(mut stamp) => {
                        if stamp.take().is_some() {
                            record.recovery.fetch_or(BLOCK_REPORT, Ordering::AcqRel);
                        }
                    }
                    Err(_) => {
                        record.recovery.fetch_or(BLOCK_REPORT, Ordering::AcqRel);
                    }
                }
                record.recovery.fetch_or(recovery, Ordering::AcqRel);
                record.phase.store(phase, Ordering::Release);
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }
    pub(super) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            let records = std::mem::take(&mut state.records);
            for record in records.values() {
                if record.phase.load(Ordering::Acquire) < CANCELLED {
                    record.phase.store(CANCELLED, Ordering::Release);
                }
            }
            drop(state);
            drop(records);
        }
        self.changed.notify_waiters();
    }
}
impl AccountRuntime {
    /// Called only at the physical far-429 fallback handoff, with its actual pin.
    pub(crate) fn register_http_deferred_reset(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        observed: &ObservedHttp429,
        pin: &LaunchPin,
    ) -> Result<Option<(DeferredResetId, LaunchOwnerScope)>, ()> {
        let Some(scope) = pin.deferred_owner_scope() else {
            return Ok(None);
        };
        if !scope.matches_pin(pin) {
            return Ok(None);
        }
        let _transaction = self.store.transaction().map_err(|_| ())?;
        let released = self.released_quota.lock().map_err(|_| ())?;
        let probes = self.probes.lock().map_err(|_| ())?;
        let volatile = self.volatile.lock().map_err(|_| ())?;
        let Some(id) = request.expected_source else {
            return Ok(None);
        };
        if self.store.writes_stopped()
            || probes.contains(id)
            || volatile.contains_key(id)
            || released.get(id).is_some_and(|release| release.pending)
        {
            return Ok(None);
        }
        let account = self.store.read_account_unlocked(id).map_err(|_| ())?;
        if !self.deferred_credentials_current(&account) {
            return Ok(None);
        }
        Ok(
            bindings.with_deferred_launch(request, &scope, None, |intent, binding, policy| {
                let qualification = self
                    .wait_owner
                    .qualify_deferred_physical_unlocked(observed, intent, &account)?;
                self.wait_owner
                    .deferred
                    .register(&scope, intent, qualification, &account, binding, policy)
                    .map(|id| (id, scope.clone()))
            }),
        )
    }
    /// Actual WS producer: original full C9 receipt plus one physically sent
    /// create on the same account/root/model. Transport witness is consumed
    /// inside the existing Store, runtime, binding and policy gates.
    pub(crate) fn register_ws_deferred_reset(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        observed: &super::wait_owner::ObservedUsagePoll,
        pin: &LaunchPin,
        original_owner: &LaunchOwnerScope,
        candidate: crate::relay::WsQuotaCandidate,
    ) -> Result<Option<(DeferredResetId, LaunchOwnerScope)>, ()> {
        let Some(scope) = pin.deferred_owner_scope() else {
            return Ok(None);
        };
        if !scope.same_owner(original_owner) || !original_owner.live() || !scope.matches_pin(pin) {
            return Ok(None);
        }
        let _transaction = self.store.transaction().map_err(|_| ())?;
        let released = self.released_quota.lock().map_err(|_| ())?;
        let probes = self.probes.lock().map_err(|_| ())?;
        let volatile = self.volatile.lock().map_err(|_| ())?;
        let Some(id) = request.expected_source else {
            return Ok(None);
        };
        if self.store.writes_stopped()
            || probes.contains(id)
            || volatile.contains_key(id)
            || released.get(id).is_some_and(|release| release.pending)
        {
            return Ok(None);
        }
        let account = self.store.read_account_unlocked(id).map_err(|_| ())?;
        if !self.deferred_credentials_current(&account) {
            return Ok(None);
        }
        let key = request.key;
        let model = request.model;
        Ok(
            bindings.with_deferred_launch(request, &scope, None, |intent, binding, policy| {
                candidate
                    .with_current(id, key, model, || {
                        let qualification = self
                            .wait_owner
                            .qualify_deferred_ws_poll_unlocked(observed, intent, &account)?;
                        self.wait_owner
                            .deferred
                            .register(&scope, intent, qualification, &account, binding, policy)
                            .map(|id| (id, scope.clone()))
                    })
                    .flatten()
            }),
        )
    }

    /// Initial producer facade: preserves the opaque fallback's exact original
    /// proof and owner before fresh registration gates; never reacquires a proof.
    pub(crate) fn register_initial_deferred_reset(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        fallback: &super::wait_admission::InitialQuotaFallback,
        pin: &LaunchPin,
    ) -> Result<Option<(DeferredResetId, LaunchOwnerScope)>, ()> {
        fallback.register_deferred(self, bindings, request, pin)
    }

    /// Fresh native owner gates around the ORIGINAL fallback's opaque ticket.
    /// The caller constructs RAII publication before releasing its task result.
    pub(super) fn register_initial_limit_deferred_reset(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        limit: &super::wait_owner::InitialFiveHourLimit,
        pin: &LaunchPin,
    ) -> Result<Option<(DeferredResetId, LaunchOwnerScope)>, ()> {
        let Some(scope) = pin.deferred_owner_scope() else {
            return Ok(None);
        };
        if !scope.matches_pin(pin)
            || request
                .expected_source
                .is_some_and(|source| source != limit.account())
        {
            return Ok(None);
        }
        let _transaction = self.store.transaction().map_err(|_| ())?;
        let released = self.released_quota.lock().map_err(|_| ())?;
        let probes = self.probes.lock().map_err(|_| ())?;
        let volatile = self.volatile.lock().map_err(|_| ())?;
        let source = limit.account();
        if self.store.writes_stopped()
            || probes.contains(source)
            || volatile.contains_key(source)
            || released.get(source).is_some_and(|release| release.pending)
        {
            return Ok(None);
        }
        let account = self.store.read_account_unlocked(source).map_err(|_| ())?;
        if !self.deferred_credentials_current(&account) {
            return Ok(None);
        }
        let request = HttpWaitRequest {
            expected_source: Some(source),
            ..request
        };
        Ok(
            bindings.with_deferred_launch(request, &scope, None, |intent, binding, policy| {
                let qualification = self
                    .wait_owner
                    .qualify_deferred_initial_unlocked(limit, intent, &account)?;
                self.wait_owner
                    .deferred
                    .register(&scope, intent, qualification, &account, binding, policy)
                    .map(|id| (id, scope.clone()))
            }),
        )
    }

    fn deferred_credentials_current(&self, account: &Account) -> bool {
        let now = Utc::now();
        account.user_state == crate::accounts::UserState::Active
            && !account.drain
            && !account.auth_probe_pending
            && account.auth_failures == 0
            && !account.token_meta.refresh_uncertain
            && account.payment_until.is_none_or(|until| until <= now)
            && account
                .token_meta
                .expires_at
                .is_none_or(|expiry| expiry > now)
            && self
                .store
                .tokens_unlocked(account)
                .is_ok_and(|tokens| !tokens.access_token.is_empty())
    }
    pub(crate) fn deferred_reset_status(
        &self,
        scope: &LaunchOwnerScope,
        native_thread_uuid: &str,
    ) -> DeferredResetStatus {
        if !scope.live() {
            return DeferredResetStatus::Cancelled;
        }
        let Ok(state) = self.wait_owner.deferred.state.lock() else {
            return DeferredResetStatus::Cancelled;
        };
        if state.closed {
            return DeferredResetStatus::Cancelled;
        }
        if scope.root().is_some() && !scope.matches_thread(native_thread_uuid) {
            return DeferredResetStatus::Cancelled;
        }
        let Ok(id) = scope.episode_id() else {
            return DeferredResetStatus::Cancelled;
        };
        let Some(id) = id else {
            return DeferredResetStatus::Empty;
        };
        let Some(record) = state.records.get(&id).filter(|r| r.owner.same_owner(scope)) else {
            return DeferredResetStatus::Cancelled;
        };
        if !scope.matches_thread(native_thread_uuid) || !record.live() {
            return DeferredResetStatus::Cancelled;
        }
        if record.phase.load(Ordering::Acquire) == SUSPENDED {
            DeferredResetStatus::Suspended {
                id: record.id,
                reset_at: record.qualification.reset_at,
                wake: record.wake,
            }
        } else {
            DeferredResetStatus::Waiting {
                id: record.id,
                reset_at: record.qualification.reset_at,
                wake: record.wake,
            }
        }
    }
    /// Positive recovery cursor only, never readiness or consumption authority.
    /// Include suspended records so the initial Due retains its real baseline.
    /// Cuts do not advance it, and this counter never asserts current readiness.
    pub(crate) fn deferred_reset_readiness_revision(
        &self,
        scope: &LaunchOwnerScope,
        id: DeferredResetId,
    ) -> Option<u64> {
        let state = self.wait_owner.deferred.state.lock().ok()?;
        if state.closed || scope.episode_id().ok()? != Some(id.0) {
            return None;
        }
        let record = state.records.get(&id.0)?;
        if !record.owner.same_owner(scope) || !record.live() {
            return None;
        }
        Some(record.readiness_revision.load(Ordering::Acquire))
    }
    /// Enable all native cancellation notifications before inspecting status.
    pub(crate) async fn deferred_reset_changed(&self, scope: &LaunchOwnerScope) {
        let changed = self.wait_owner.deferred.changed.notified();
        let owner = scope.notified();
        tokio::pin!(changed, owner);
        changed.as_mut().enable();
        owner.as_mut().enable();
        let record = self
            .wait_owner
            .deferred
            .state
            .lock()
            .ok()
            .and_then(|state| {
                scope
                    .episode_id()
                    .ok()
                    .flatten()
                    .and_then(|id| state.records.get(&id))
                    .filter(|r| r.owner.same_owner(scope))
                    .cloned()
            });
        let Some(record) = record else {
            if !scope.live()
                || self
                    .wait_owner
                    .deferred
                    .state
                    .lock()
                    .map_or(true, |state| state.closed)
            {
                return;
            }
            tokio::select! { _ = changed => {}, _ = owner => {} }
            return;
        };
        let binding = record.binding.notified();
        let policy = record.policy.notified();
        tokio::pin!(binding, policy);
        binding.as_mut().enable();
        policy.as_mut().enable();
        if !record.live() {
            return;
        }
        tokio::select! { _ = changed => {}, _ = owner => {}, _ = binding => {}, _ = policy => {} }
    }
    pub(crate) fn cancel_deferred_reset(
        &self,
        scope: &LaunchOwnerScope,
        id: DeferredResetId,
    ) -> bool {
        // Share the final consume leaf fence: cancellation cannot overwrite a
        // consumed terminal phase or report success after consumption.
        let Ok(state) = self.wait_owner.deferred.state.lock() else {
            return false;
        };
        if state.closed {
            return false;
        }
        let Some(record) = state.records.get(&id.0) else {
            return false;
        };
        if !record.owner.same_owner(scope) || record.phase.load(Ordering::Acquire) >= CANCELLED {
            return false;
        }
        record.phase.store(CANCELLED, Ordering::Release);
        drop(state);
        self.wait_owner.deferred.changed.notify_waiters();
        true
    }
    /// Retire only this owner's exact consumed receipt, once. This opens one
    /// future registration with fresh native qualification on the same root/pin.
    /// A new registration racing the parent's turn-progress processing may be
    /// pending already; revalidate it under all native gates before publishing.
    /// This supplies neither a reset nor a resume permit.
    pub(crate) fn advance_deferred_reset(
        &self,
        bindings: &BindingTable,
        scope: &LaunchOwnerScope,
        id: DeferredResetId,
        operation: &IssueOperation,
    ) -> Result<bool, ()> {
        let record = {
            let state = self.wait_owner.deferred.state.lock().map_err(|_| ())?;
            if state.closed || state.next == u64::MAX || !operation.can_reply() {
                return Ok(false);
            }
            let Ok(pending) = scope.pending_episode(id.0, &self.wait_owner.deferred.identity)
            else {
                return Ok(false);
            };
            if let Some(pending) = pending {
                let Some(record) = state.records.get(&pending) else {
                    return Ok(false);
                };
                record.clone()
            } else {
                let Some(owner_live) = scope.live_guard() else {
                    return Ok(false);
                };
                if !operation.can_reply() {
                    return Ok(false);
                }
                let advanced =
                    scope.advance_episode(id.0, &self.wait_owner.deferred.identity, None);
                drop(owner_live);
                drop(state);
                if advanced {
                    self.wait_owner.deferred.changed.notify_waiters();
                }
                return Ok(advanced);
            }
        };
        // Never acquire Store while holding the deferred leaf gate: registration
        // and final consumption use Store -> bindings/policy -> deferred -> owner.
        let _transaction = self.store.transaction().map_err(|_| ())?;
        let released = self.released_quota.lock().map_err(|_| ())?;
        let probes = self.probes.lock().map_err(|_| ())?;
        let volatile = self.volatile.lock().map_err(|_| ())?;
        if !operation.can_reply()
            || self.store.writes_stopped()
            || !record.owner.same_owner(scope)
            || !record.live()
            || record.phase.load(Ordering::Acquire) != WAITING
            || probes.contains(&record.account)
            || volatile.contains_key(&record.account)
            || released
                .get(&record.account)
                .is_some_and(|release| release.pending)
        {
            return Ok(false);
        }
        let account = self
            .store
            .read_account_unlocked(&record.account)
            .map_err(|_| ())?;
        if !record.same_hold(&account) || !self.deferred_credentials_current(&account) {
            return Ok(false);
        }
        let request = HttpWaitRequest {
            provider: Provider::Codex,
            key: Some(&record.root),
            model: Some(&record.model),
            expected_source: Some(&record.account),
            operation,
        };
        let result = bindings.with_deferred_launch(
            request,
            scope,
            Some((record.binding.clone(), record.policy.clone())),
            |intent, _, _| {
                if intent.policy_key() != record.policy_key
                    || !same_plan(&record.plan, intent.plan())
                    || !record.live()
                    || record.phase.load(Ordering::Acquire) != WAITING
                    || !intent.caller_current()
                {
                    return None;
                }
                let state = self.wait_owner.deferred.state.lock().ok()?;
                if state.closed
                    || !state
                        .records
                        .get(&record.id.0)
                        .is_some_and(|r| Arc::ptr_eq(r, &record))
                {
                    return None;
                }
                let _owner_live = scope.live_guard()?;
                if !intent.caller_current() {
                    return None;
                }
                scope
                    .advance_episode(id.0, &self.wait_owner.deferred.identity, Some(record.id.0))
                    .then_some(())
            },
        );
        if result.is_some() {
            self.wait_owner.deferred.changed.notify_waiters();
        }
        Ok(result.is_some())
    }
    /// Readiness hint before native final-get. May persist exact native expiry;
    /// never consumes the episode or grants automatic activation authority.
    #[cfg(test)]
    pub(crate) fn inspect_deferred_reset(
        &self,
        bindings: &BindingTable,
        scope: &LaunchOwnerScope,
        id: DeferredResetId,
        operation: &IssueOperation,
    ) -> Result<bool, ()> {
        self.inspect_deferred_reset_state(bindings, scope, id, operation)
            .map(|state| state == DeferredResetInspection::Ready)
    }
    /// Pending is decided under the same native/binding/policy/owner gates as
    /// Ready; it cannot be inferred from an unlocked status read after false.
    pub(crate) fn inspect_deferred_reset_state(
        &self,
        bindings: &BindingTable,
        scope: &LaunchOwnerScope,
        id: DeferredResetId,
        operation: &IssueOperation,
    ) -> Result<DeferredResetInspection, ()> {
        self.check_deferred_reset(bindings, scope, id, operation, false)
    }
    /// Final parent authorize callback only. No original HTTP ticket is admitted.
    pub(crate) fn consume_deferred_reset(
        &self,
        bindings: &BindingTable,
        scope: &LaunchOwnerScope,
        id: DeferredResetId,
        operation: &IssueOperation,
    ) -> Result<Option<DeferredResumePermit>, ()> {
        self.check_deferred_reset(bindings, scope, id, operation, true)
            .map(|state| {
                (state == DeferredResetInspection::Ready)
                    .then_some(DeferredResumePermit { _private: () })
            })
    }
    fn check_deferred_reset(
        &self,
        bindings: &BindingTable,
        scope: &LaunchOwnerScope,
        id: DeferredResetId,
        operation: &IssueOperation,
        consume: bool,
    ) -> Result<DeferredResetInspection, ()> {
        let Some(record) = self.wait_owner.deferred.record(scope, id) else {
            return Ok(DeferredResetInspection::Refused);
        };
        let _transaction = self.store.transaction().map_err(|_| ())?;
        let released = self.released_quota.lock().map_err(|_| ())?;
        let probes = self.probes.lock().map_err(|_| ())?;
        let volatile = self.volatile.lock().map_err(|_| ())?;
        let wall = Utc::now();
        if !operation.can_reply()
            || !record.live()
            || self.store.writes_stopped()
            || probes.contains(&record.account)
            || volatile.contains_key(&record.account)
            || released
                .get(&record.account)
                .is_some_and(|release| release.pending)
        {
            return Ok(DeferredResetInspection::Refused);
        }
        let mut account = self
            .store
            .read_account_unlocked(&record.account)
            .map_err(|_| ())?;
        if !record.same_identity(&account) || !self.deferred_credentials_current(&account) {
            return Ok(DeferredResetInspection::Refused);
        }
        let due = Instant::now() >= record.wake && wall >= record.qualification.reset_at;
        let readiness = record.readiness_current(&account, wall, Instant::now());
        let mut pending = !due || readiness == Some(false);
        if readiness == Some(false) {
            // Exact generation/freshness mismatch never falls through to the
            // original native expiry lineage, even after evidence is cleared.
            let state = self.wait_owner.deferred.state.lock().map_err(|_| ())?;
            if let Ok(mut stamp) = record.readiness.lock() {
                *stamp = None;
            }
            record.recovery.fetch_or(BLOCK_REPORT, Ordering::AcqRel);
            let _ = record.phase.compare_exchange(
                READY,
                SUSPENDED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            drop(state);
            self.wait_owner.deferred.changed.notify_waiters();
        }
        if !pending && record.phase.load(Ordering::Acquire) == SUSPENDED {
            // Only credential-only saves and the native expiry boundary preserve
            // the original hold's lineage. Store's restricted save compares all
            // other metadata; generic writes cancel, reports independently block.
            // Read the durable current record under Store, never infer success
            // from the prehook or refresh result. Failed writes may leave usable
            // old credentials; they convey no new proof or reset interval.
            let recovery = record.recovery.load(Ordering::Acquire);
            if recovery == 0 || recovery & BLOCK_REPORT != 0 {
                pending = true;
            } else {
                let phase = if record.same_hold(&account) {
                    WAITING
                } else if (recovery & RECOVER_EXPIRY != 0
                    || record.expiry_generation.load(Ordering::Acquire) != 0)
                    && matches!(account.health, Health::Ready)
                    && account.quota_cooling.is_none()
                {
                    // An independent native expiry can win first. Its prehook plus
                    // durable Ready state certify this original hold only because
                    // every other metadata/report mutation cancelled or blocked it.
                    record
                        .expiry_generation
                        .store(account.generation, Ordering::Release);
                    READY
                } else {
                    return Ok(DeferredResetInspection::Refused);
                };
                if record
                    .phase
                    .compare_exchange(SUSPENDED, phase, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return Ok(DeferredResetInspection::Refused);
                }
                record.recovery.store(0, Ordering::Release);
            }
        }
        if !pending {
            match record.phase.load(Ordering::Acquire) {
                WAITING => {
                    if !record.same_hold(&account)
                        || record
                            .phase
                            .compare_exchange(
                                WAITING,
                                EXPIRING,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_err()
                    {
                        return Ok(DeferredResetInspection::Refused);
                    }
                    // Exact hold checked above, under the same Store transaction.
                    // Native hook still revokes proof. Failed durability never grants.
                    account = match self.store.expire_health_unlocked(&record.account) {
                        Ok(account) => account,
                        Err(_) => {
                            // A failed expiry grants nothing. Preserve only the
                            // exact-hold retry lineage, checked again under Store.
                            record.recovery.fetch_or(RECOVER_EXPIRY, Ordering::AcqRel);
                            let _ = record.phase.compare_exchange(
                                EXPIRING,
                                SUSPENDED,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            );
                            self.wait_owner.deferred.changed.notify_waiters();
                            return Err(());
                        }
                    };
                    record
                        .expiry_generation
                        .store(account.generation, Ordering::Release);
                    if record
                        .phase
                        .compare_exchange(EXPIRING, READY, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        return Ok(DeferredResetInspection::Refused);
                    }
                }
                READY => {
                    if readiness != Some(true)
                        && record.expiry_generation.load(Ordering::Acquire) != account.generation
                    {
                        return Ok(DeferredResetInspection::Refused);
                    }
                }
                _ => return Ok(DeferredResetInspection::Refused),
            }
        }
        // Only the exact original quota hold or natively Ready awaiting fresh
        // evidence is recoverable. Other health/scope/reset changes are terminal.
        let recoverable = record.same_hold(&account)
            || (matches!(account.health, Health::Ready) && account.quota_cooling.is_none());
        if !record.live()
            || (pending && !recoverable)
            || (!pending
                && (!matches!(account.health, Health::Ready)
                    || account.quota_cooling.is_some()
                    || !account.eligible_at(Some(&record.model), Utc::now())))
            || !self.deferred_credentials_current(&account)
        {
            record.phase.store(CANCELLED, Ordering::Release);
            return Ok(DeferredResetInspection::Refused);
        }
        let request = HttpWaitRequest {
            provider: Provider::Codex,
            key: Some(&record.root),
            model: Some(&record.model),
            expected_source: Some(&record.account),
            operation,
        };
        let result = bindings.with_deferred_launch(
            request,
            scope,
            Some((record.binding.clone(), record.policy.clone())),
            |intent, _, _| {
                if intent.policy_key() != record.policy_key
                    || !same_plan(&record.plan, intent.plan())
                    || !record.live()
                    || !operation.can_reply()
                {
                    return None;
                }
                let state = self.wait_owner.deferred.state.lock().ok()?;
                if state.closed
                    || !state
                        .records
                        .get(&id.0)
                        .is_some_and(|r| Arc::ptr_eq(r, &record))
                {
                    return None;
                }
                // The owner gate may block; freshness must be checked after it.
                let _owner_live = scope.live_guard()?;
                if !record.live() || !operation.can_reply() {
                    return None;
                }
                if readiness == Some(true)
                    && record.readiness_current(&account, Utc::now(), Instant::now()) != Some(true)
                {
                    if let Ok(mut stamp) = record.readiness.lock() {
                        *stamp = None;
                    }
                    record.recovery.fetch_or(BLOCK_REPORT, Ordering::AcqRel);
                    let _ = record.phase.compare_exchange(
                        READY,
                        SUSPENDED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return Some(DeferredResetInspection::Pending);
                }
                if pending
                    || Utc::now() < record.qualification.reset_at
                    || Instant::now() < record.wake
                {
                    return matches!(
                        record.phase.load(Ordering::Acquire),
                        WAITING | SUSPENDED | READY
                    )
                    .then_some(DeferredResetInspection::Pending);
                }
                if consume {
                    operation.admit().ok()?;
                    record
                        .phase
                        .compare_exchange(READY, CONSUMED, Ordering::AcqRel, Ordering::Acquire)
                        .ok()?;
                    if !scope.consumed_episode(id.0) {
                        return None;
                    }
                } else if record.phase.load(Ordering::Acquire) != READY {
                    return None;
                }
                Some(DeferredResetInspection::Ready)
            },
        );
        if consume && result == Some(DeferredResetInspection::Ready) {
            self.wait_owner.deferred.changed.notify_waiters();
        }
        Ok(result.unwrap_or(DeferredResetInspection::Refused))
    }
}
pub(crate) fn same_plan(a: &Plan, b: &Plan) -> bool {
    a.settings == b.settings
        && a.anchor == b.anchor
        && a.project == b.project
        && a.source == b.source
}

#[cfg(test)]
mod tests {
    mod advance_tests;

    use super::*;
    use crate::{
        account_runtime::HealthSignal,
        accounts::{AccountStore, Tokens},
        bindings::{Candidate, LaunchRequest},
        config::RoutingConfig,
        launch_tokens::{LaunchMetadata, LaunchOwnerLease, LaunchTokenRegistry},
        observability::Observability,
        routing::{On5hLimit, session_id},
    };
    use chrono::Duration;
    use std::collections::BTreeSet;
    const THREAD: &str = "11111111-2222-4333-8444-555555555555";
    const ROOT: &str = "codex:11111111-2222-4333-8444-555555555555";
    const ANCHOR: &str = "launch:synthetic-deferred";
    const MODEL: &str = "synthetic-model";
    // These tests exercise readiness after the original reset, not a race
    // between real Store writes and a one-second setup window. Keep the reset
    // beyond the registration deadline without changing the production gates.
    const NEAR_RESET_LEAD: i64 = 10;
    struct Fixture {
        runtime: Arc<AccountRuntime>,
        table: Arc<BindingTable>,
        pin: LaunchPin,
        lease: LaunchOwnerLease,
        id: String,
        _home: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_reset_hold(60)
        }
        fn with_reset_hold(reset_hold_secs: u64) -> Self {
            let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(".sandbox/r13-part-c/n5d-native/fixtures");
            std::fs::create_dir_all(&base).unwrap();
            let home = tempfile::tempdir_in(base).unwrap();
            let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
            let account = store
                .save(
                    &crate::accounts::health::tests::fixture(),
                    &Tokens {
                        access_token: "synthetic-access".into(),
                        refresh_token: "synthetic-refresh".into(),
                        id_token: None,
                    },
                    None,
                )
                .unwrap();
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            let runtime = AccountRuntime::new(store, obs, 0.0);
            let table = BindingTable::new(
                home.path(),
                RoutingConfig {
                    reset_hold_secs,
                    ..RoutingConfig::default()
                },
            )
            .unwrap();
            let models = BTreeSet::from([MODEL.to_owned(), "synthetic-other".to_owned()]);
            table
                .place_launch(
                    LaunchRequest {
                        anchor_key: ANCHOR,
                        resume_key: None,
                        model: Some(MODEL),
                        model_is_explicit: true,
                        account_pref: Some(&account.id),
                        allow_move_for_model: false,
                    },
                    &[Candidate {
                        account: account.clone(),
                        quota: None,
                        inflight: 0,
                    }],
                    &BTreeMap::from([(account.id.clone(), models.clone())]),
                    Utc::now(),
                )
                .unwrap();
            table
                .resolve_launch(Some(ANCHOR), Some(ROOT), Utc::now())
                .unwrap();
            let lease = table.begin_request(Some(ROOT), &account.id);
            lease.dispatched();
            drop(lease);
            let registry = LaunchTokenRegistry::default();
            let token = registry
                .issue(LaunchMetadata {
                    binding_anchor_key: ANCHOR.into(),
                    advertised_models: models,
                })
                .unwrap();
            let pin = registry.get_or_pin(token.expose_secret()).unwrap();
            let lease = pin.open_deferred_owner().unwrap();
            Self {
                runtime,
                table,
                pin,
                lease,
                id: account.id,
                _home: home,
            }
        }
        fn request<'a>(&'a self, operation: &'a IssueOperation) -> HttpWaitRequest<'a> {
            HttpWaitRequest {
                provider: Provider::Codex,
                key: Some(ROOT),
                model: Some(MODEL),
                expected_source: Some(&self.id),
                operation,
            }
        }
        async fn physical(&self, seconds: i64) -> (ObservedHttp429, DateTime<Utc>) {
            let phase_started = std::time::Instant::now();
            let desired = Utc::now() + Duration::seconds(seconds);
            let reset = DateTime::from_timestamp(
                desired.timestamp() + i64::from(desired.timestamp_subsec_nanos() != 0),
                0,
            )
            .unwrap();
            let mut headers = axum::http::HeaderMap::new();
            for (name, value) in [
                ("x-codex-primary-used-percent", "100".into()),
                ("x-codex-primary-window-minutes", "300".into()),
                ("x-codex-primary-reset-at", reset.timestamp().to_string()),
                ("x-codex-secondary-used-percent", "20".into()),
                ("x-codex-secondary-window-minutes", "10080".into()),
                (
                    "x-codex-secondary-reset-at",
                    (reset + Duration::days(1)).timestamp().to_string(),
                ),
            ] {
                headers.insert(name, value.parse().unwrap());
            }
            let origin = self.runtime.capture_http_origin(&self.id).await.unwrap();
            let asof = self.table.begin_usage_observation_for(&self.id);
            let (_, pending) = self
                .runtime
                .observe_http_429_headers_pending(
                    &self.id,
                    Provider::Codex,
                    &headers,
                    Some(MODEL),
                    (&self.table, asof),
                    Some(origin),
                )
                .await;
            let body = serde_json::to_vec(&serde_json::json!({"error": {
                "type": "usage_limit_reached", "resets_at": reset.timestamp()
            }}))
            .unwrap();
            let body = AccountRuntime::qualify_http_429_body(Some(&body), Some(MODEL));
            let observed = self
                .runtime
                .finish_http_429_report_observed(
                    &self.id,
                    pending,
                    Some(reset),
                    Some(HealthSignal::Cooling {
                        reason: HealthReason::Quota,
                        until: reset,
                        scope: "account".into(),
                    }),
                    body,
                )
                .await
                .unwrap()
                .unwrap_or_else(|| {
                    panic!(
                        "actual complete physical header/body adapter must qualify: reset={reset}, completed={}, elapsed={:?}",
                        Utc::now(),
                        phase_started.elapsed()
                    )
                });
            (observed, reset)
        }
        async fn poll(&self, reset: DateTime<Utc>, complete: bool) {
            let at = Utc::now();
            let mut raw = serde_json::json!({"observed_at":at,"windows":[
                {"name":"5h","used_pct":100.0,"observed_at":at,"reset_at":reset},
                {"name":"7d","used_pct":20.0,"observed_at":at,"reset_at":at+Duration::days(1)}
            ]});
            if !complete {
                raw["windows"].as_array_mut().unwrap().pop();
            }
            let origin = self.runtime.capture_poll_origin(&self.id).await.unwrap();
            self.runtime
                .apply_quota_snapshot_reported_origin(
                    &self.id,
                    raw.clone(),
                    &raw,
                    None,
                    Some(origin),
                )
                .await
                .unwrap();
        }
        fn initial_operation() -> IssueOperation {
            IssueOperation::new(Instant::now() + std::time::Duration::from_secs(20))
        }
        fn fallback(
            &self,
            operation: &IssueOperation,
        ) -> super::super::wait_admission::InitialQuotaFallback {
            self.runtime
                .current_http_initial_far_wait(&self.table, self.request(operation))
                .unwrap()
                .expect("full current proof should produce initial fallback")
        }
        fn register(&self, observed: &ObservedHttp429) -> (DeferredResetId, LaunchOwnerScope) {
            let operation = IssueOperation::new(Instant::now() + std::time::Duration::from_secs(1));
            let _caller = operation.caller_guard();
            self.runtime
                .register_http_deferred_reset(
                    &self.table,
                    self.request(&operation),
                    observed,
                    &self.pin,
                )
                .unwrap()
                .unwrap_or_else(|| {
                    panic!(
                        "native existing managed physical fallback must register: completed={}",
                        Utc::now()
                    )
                })
        }
        fn operation() -> IssueOperation {
            IssueOperation::new(Instant::now() + std::time::Duration::from_secs(5))
        }
    }
    #[tokio::test]
    async fn n5d_initial_deferred_full_poll_original_fallback_registers_without_send() {
        let f = Fixture::new();
        let reset = Utc::now() + Duration::minutes(3);
        f.poll(reset, true).await;
        let operation = Fixture::initial_operation();
        let _caller = operation.caller_guard();
        let fallback = f.fallback(&operation);
        let request = HttpWaitRequest {
            expected_source: None,
            ..f.request(&operation)
        };
        let (id, scope) = f
            .runtime
            .register_initial_deferred_reset(&f.table, request, &fallback, &f.pin)
            .unwrap()
            .expect("exact initial proof must register");
        assert!(matches!(f.runtime.deferred_reset_status(&scope, THREAD),
            DeferredResetStatus::Waiting { id: actual, reset_at, .. }
                if actual == id && reset_at == reset));
        assert!(
            fallback
                .register_deferred(&f.runtime, &f.table, f.request(&operation), &f.pin)
                .unwrap()
                .is_none(),
            "one owner episode only"
        );
        let response = fallback.into_response(&f.table.policies).unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(f.runtime.cancel_deferred_reset(&scope, id));
    }

    #[tokio::test]
    async fn n5d_initial_deferred_replaced_full_or_partial_report_cannot_borrow_new_ticket() {
        for complete in [true, false] {
            let f = Fixture::new();
            let reset = Utc::now() + Duration::minutes(3);
            f.poll(reset, true).await;
            let operation = Fixture::initial_operation();
            let _caller = operation.caller_guard();
            let stale = f.fallback(&operation);
            f.poll(reset, complete).await;
            assert!(
                stale
                    .register_deferred(&f.runtime, &f.table, f.request(&operation), &f.pin)
                    .unwrap()
                    .is_none(),
                "old ticket cannot borrow replacement, complete={complete}"
            );
            assert!(stale.into_response(&f.table.policies).is_none());
            assert_eq!(
                f.runtime.deferred_reset_status(&f.lease.scope(), THREAD),
                DeferredResetStatus::Empty
            );
            if complete {
                let fresh = f.fallback(&operation);
                let (id, scope) = fresh
                    .register_deferred(&f.runtime, &f.table, f.request(&operation), &f.pin)
                    .unwrap()
                    .expect("genuinely fresh fallback remains usable");
                assert!(f.runtime.cancel_deferred_reset(&scope, id));
            } else {
                assert!(
                    f.runtime
                        .current_http_initial_far_wait(&f.table, f.request(&operation))
                        .unwrap()
                        .is_none(),
                    "missing long-window raw evidence cannot borrow cache"
                );
            }
        }
    }

    #[tokio::test]
    async fn n5d_initial_deferred_refuses_changed_identity_deadline_model_owner_and_policy() {
        let f = Fixture::new();
        let reset = Utc::now() + Duration::minutes(3);
        f.poll(reset, true).await;
        let operation = Fixture::initial_operation();
        let caller = operation.caller_guard();
        let fallback = f.fallback(&operation);
        let other = Fixture::new();
        assert!(
            fallback
                .register_deferred(&other.runtime, &f.table, f.request(&operation), &f.pin)
                .unwrap()
                .is_none(),
            "other runtime cannot interpret opaque limit"
        );
        let changed = Fixture::operation();
        let _changed_caller = changed.caller_guard();
        assert!(
            fallback
                .register_deferred(&f.runtime, &f.table, f.request(&changed), &f.pin)
                .unwrap()
                .is_none(),
            "original deadline cannot be replaced"
        );
        for request in [
            HttpWaitRequest {
                expected_source: Some("synthetic-wrong-account"),
                ..f.request(&operation)
            },
            HttpWaitRequest {
                model: Some("synthetic-other"),
                ..f.request(&operation)
            },
        ] {
            assert!(
                fallback
                    .register_deferred(&f.runtime, &f.table, request, &f.pin)
                    .unwrap()
                    .is_none()
            );
        }
        f.table
            .set_routing_session_urgent(&session_id(ROOT), On5hLimit::Move)
            .unwrap();
        assert!(
            fallback
                .register_deferred(&f.runtime, &f.table, f.request(&operation), &f.pin)
                .unwrap()
                .is_none(),
            "effective policy change refuses original limit"
        );
        drop(caller);
        assert!(
            fallback
                .register_deferred(&f.runtime, &f.table, f.request(&operation), &f.pin)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            f.runtime.deferred_reset_status(&f.lease.scope(), THREAD),
            DeferredResetStatus::Empty
        );
    }

    #[tokio::test]
    async fn n5d_poll_credit_release_native_expiry_consumes_exactly_once() {
        let f = Fixture::with_reset_hold(0);
        let reset = Utc::now() + Duration::seconds(2);
        f.poll(reset, true).await;
        let before = f.runtime.store.load(&f.id).unwrap();
        assert!(
            before.quota_credit_releasable,
            "poll limit is credit-releasable independently of permission"
        );
        assert!(!before.allow_credits);
        let operation = Fixture::initial_operation();
        let _caller = operation.caller_guard();
        let fallback = f.fallback(&operation);
        let (id, scope) = f
            .runtime
            .register_initial_deferred_reset(&f.table, f.request(&operation), &fallback, &f.pin)
            .unwrap()
            .expect("same full poll must register");
        let original = original_due(&f, &scope).await;
        assert!(
            f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
        );
        let after = f.runtime.store.load(&f.id).unwrap();
        assert!(matches!(after.health, Health::Ready));
        assert!(
            !after.quota_credit_releasable,
            "native reset clears only the old limit's release flag"
        );
        assert_original_episode(&f, &scope, id, original);
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_some(),
            "native expiry may clear the old quota flag without changing account identity"
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn n5d_poll_credit_release_metadata_change_cancels_original_episode() {
        let f = Fixture::with_reset_hold(0);
        f.poll(Utc::now() + Duration::minutes(2), true).await;
        let operation = Fixture::initial_operation();
        let _caller = operation.caller_guard();
        let fallback = f.fallback(&operation);
        let (id, scope) = f
            .runtime
            .register_initial_deferred_reset(&f.table, f.request(&operation), &fallback, &f.pin)
            .unwrap()
            .unwrap();
        f.runtime
            .store
            .update_metadata(&f.id, |account| {
                account.quota_credit_releasable = false;
            })
            .unwrap();
        assert_eq!(
            f.runtime.deferred_reset_status(&scope, THREAD),
            DeferredResetStatus::Cancelled
        );
        assert_eq!(
            f.runtime
                .inspect_deferred_reset_state(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap(),
            DeferredResetInspection::Refused
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
                .is_none(),
            "a generic flag edit cannot borrow expiry authority"
        );
    }

    #[tokio::test]
    async fn n5d_poll_credit_release_independent_expiry_keeps_original_episode() {
        let f = Fixture::with_reset_hold(0);
        f.poll(Utc::now() + Duration::seconds(2), true).await;
        let operation = Fixture::initial_operation();
        let _caller = operation.caller_guard();
        let fallback = f.fallback(&operation);
        let (id, scope) = f
            .runtime
            .register_initial_deferred_reset(&f.table, f.request(&operation), &fallback, &f.pin)
            .unwrap()
            .unwrap();
        let original = original_due(&f, &scope).await;
        let after = f.runtime.store.load(&f.id).unwrap();
        assert!(matches!(after.health, Health::Ready) && !after.quota_credit_releasable);
        assert!(
            f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
        );
        assert_original_episode(&f, &scope, id, original);
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_some()
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn n5d_physical_deferred_native_expiry_readiness_then_single_fresh_consume() {
        let f = Fixture::new();
        let (observed, reset) = f.physical(4).await;
        let (id, scope) = f.register(&observed);
        let status = f.runtime.deferred_reset_status(&scope, THREAD);
        let DeferredResetStatus::Waiting { wake, reset_at, .. } = status else {
            panic!("missing physical obligation");
        };
        assert_eq!(reset_at, reset);
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_none()
        );
        let old_ticket = {
            let _gate = f.runtime.store.transaction().unwrap();
            let account = f.runtime.store.read_account_unlocked(&f.id).unwrap();
            f.runtime
                .wait_owner
                .acquire_current_unlocked(&account, Some(MODEL), Utc::now())
                .unwrap()
        };
        // Wait for the actual original native wall/mono reset, not a fabricated
        // DTO or elapsed arbitrary sleep. Bound prevents a broken clock hanging.
        wait_until_original_reset(wake, reset).await;
        assert!(Utc::now() >= reset);
        // Synthetic live weak-slot pressure exercises both actual capacity
        // checks without constructing thousands of unrelated native accounts.
        // Existing readiness/consume must reuse their reserved owner fences.
        f.table.fill_deferred_watch_slots_for_test();
        assert!(
            f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
        );
        let after = f.runtime.store.load(&f.id).unwrap();
        assert!(matches!(after.health, Health::Ready));
        assert!(!f.runtime.wait_owner.ticket_current_unlocked(
            &old_ticket,
            &after,
            Some(MODEL),
            Utc::now()
        ));
        assert!(
            f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
        );
        let terminal = f.runtime.wait_owner.deferred.record(&scope, id).unwrap();
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_some()
        );
        assert!(!f.runtime.cancel_deferred_reset(&scope, id));
        assert_eq!(terminal.phase.load(Ordering::Acquire), CONSUMED);
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            after.generation,
            f.runtime.store.load(&f.id).unwrap().generation,
            "readiness/consume must not expire twice"
        );
        f.runtime.wait_owner.deferred.close();
        assert_eq!(terminal.phase.load(Ordering::Acquire), CONSUMED);
    }
    #[tokio::test]
    async fn n5d_physical_deferred_credential_recovery_and_independent_expiry_keep_original_reset()
    {
        // 0: refresh crosses reset; 1: independent expiry wins during refresh;
        // 2: independent expiry alone. No new quota report is issued in any case.
        for mode in 0..3 {
            let f = Fixture::new();
            let (observed, reset) = f.physical(4).await;
            let (id, scope) = f.register(&observed);
            let DeferredResetStatus::Waiting { wake, reset_at, .. } =
                f.runtime.deferred_reset_status(&scope, THREAD)
            else {
                panic!("missing original episode");
            };
            assert_eq!(reset_at, reset);
            let read = || {
                let _gate = f.runtime.store.transaction().unwrap();
                f.runtime.store.read_account_unlocked(&f.id).unwrap()
            };
            let tokens = Tokens {
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                id_token: None,
            };
            if mode != 2 {
                let mut uncertain = read();
                uncertain.token_meta.refresh_uncertain = true;
                f.runtime
                    .store
                    .save_credentials(&uncertain, &tokens, uncertain.generation)
                    .unwrap();
            }
            wait_until_original_reset(wake, reset).await;
            assert!(Utc::now() >= reset);
            if mode != 0 {
                let expired = f.runtime.store.load(&f.id).unwrap();
                assert!(matches!(expired.health, Health::Ready));
                assert!(expired.quota_cooling.is_none());
            }
            if mode != 2 {
                assert!(
                    !f.runtime
                        .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                        .unwrap(),
                    "write-ahead uncertainty is not usable credentials"
                );
                let mut certain = read();
                certain.token_meta.refresh_uncertain = false;
                assert!(
                    f.runtime
                        .store
                        .save_credentials(&certain, &tokens, certain.generation + 1)
                        .is_err()
                );
                assert!(
                    !f.runtime
                        .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                        .unwrap(),
                    "rejected save must not clear durable uncertainty"
                );
                f.runtime
                    .store
                    .save_credentials(&certain, &tokens, certain.generation)
                    .unwrap();
            }
            assert!(
                f.runtime
                    .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                    .unwrap(),
                "fresh exact native continuity must recover, mode={mode}"
            );
            let DeferredResetStatus::Waiting {
                wake: after,
                reset_at: after_reset,
                ..
            } = f.runtime.deferred_reset_status(&scope, THREAD)
            else {
                panic!("recovery lost original episode");
            };
            assert_eq!((after, after_reset), (wake, reset));
            assert!(
                f.runtime
                    .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                    .unwrap()
                    .is_some()
            );
            assert!(
                f.runtime
                    .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                    .unwrap()
                    .is_none()
            );
        }
    }
    #[tokio::test]
    async fn n5d_physical_deferred_scope_root_and_one_episode_are_exact() {
        let f = Fixture::new();
        let (observed, _) = f.physical(120).await;
        let operation = Fixture::operation();
        let mut request = f.request(&operation);
        request.key = Some(ANCHOR);
        assert!(
            f.runtime
                .register_http_deferred_reset(&f.table, request, &observed, &f.pin)
                .unwrap()
                .is_none()
        );
        let child = format!("{ROOT}:agent:synthetic");
        let mut request = f.request(&operation);
        request.key = Some(&child);
        assert!(
            f.runtime
                .register_http_deferred_reset(&f.table, request, &observed, &f.pin)
                .unwrap()
                .is_none()
        );
        let (id, scope) = f.register(&observed);
        assert!(
            f.runtime
                .register_http_deferred_reset(&f.table, f.request(&operation), &observed, &f.pin)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            f.runtime
                .deferred_reset_status(&scope, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"),
            DeferredResetStatus::Cancelled
        );
        let registry = LaunchTokenRegistry::default();
        let token = registry
            .issue(LaunchMetadata {
                binding_anchor_key: ANCHOR.into(),
                advertised_models: BTreeSet::new(),
            })
            .unwrap();
        let pin = registry.get_or_pin(token.expose_secret()).unwrap();
        let lease = pin.open_deferred_owner().unwrap();
        assert!(!f.runtime.cancel_deferred_reset(&lease.scope(), id));
        assert!(matches!(
            f.runtime.deferred_reset_status(&scope, THREAD),
            DeferredResetStatus::Waiting { .. }
        ));
        assert!(f.runtime.cancel_deferred_reset(&scope, id));
        assert_eq!(
            f.runtime.deferred_reset_status(&scope, THREAD),
            DeferredResetStatus::Cancelled
        );
    }
    #[tokio::test]
    async fn n5d_physical_deferred_native_model_and_policy_away_back_cannot_resurrect() {
        for policy in [false, true] {
            let f = Fixture::new();
            let (observed, _) = f.physical(120).await;
            let (_id, scope) = f.register(&observed);
            if policy {
                f.table
                    .set_routing_session_urgent(&session_id(ROOT), On5hLimit::Move)
                    .unwrap();
                f.table
                    .set_routing_session_urgent(&session_id(ROOT), On5hLimit::Wait)
                    .unwrap();
            } else {
                f.table
                    .record_launch_model(ROOT, &f.id, "synthetic-other", Utc::now())
                    .unwrap();
                f.table
                    .record_launch_model(ROOT, &f.id, MODEL, Utc::now())
                    .unwrap();
            }
            assert_eq!(
                f.runtime.deferred_reset_status(&scope, THREAD),
                DeferredResetStatus::Cancelled
            );
        }
    }
    #[tokio::test]
    async fn n5d_physical_deferred_owner_drop_settings_stop_and_unrelated_policy() {
        let f = Fixture::new();
        let unrelated = "codex:synthetic-unrelated";
        let decision = f.table.choose(
            Provider::Codex,
            Some(unrelated),
            None,
            Some(MODEL),
            &[Candidate {
                account: f.runtime.store.load(&f.id).unwrap(),
                quota: None,
                inflight: 0,
            }],
            Some(&f.id),
            0,
        );
        let crate::bindings::Decision::Selected { account, lease, .. } = decision else {
            panic!("unrelated native placement required");
        };
        assert_eq!(account, f.id);
        lease.dispatched();
        drop(lease);
        let (observed, _) = f.physical(120).await;
        let (id, scope) = f.register(&observed);
        f.table
            .set_routing_session_urgent(&session_id(unrelated), On5hLimit::Move)
            .unwrap();
        assert!(matches!(
            f.runtime.deferred_reset_status(&scope, THREAD),
            DeferredResetStatus::Waiting { .. }
        ));
        f.runtime
            .store
            .update_metadata(&f.id, |account| {
                account.user_state = crate::accounts::UserState::Paused
            })
            .unwrap();
        assert_eq!(
            f.runtime.deferred_reset_status(&scope, THREAD),
            DeferredResetStatus::Cancelled
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_none()
        );
        let f = Fixture::new();
        let (observed, _) = f.physical(120).await;
        let (_, scope) = f.register(&observed);
        drop(f.lease);
        assert_eq!(
            f.runtime.deferred_reset_status(&scope, THREAD),
            DeferredResetStatus::Cancelled
        );
        let f = Fixture::new();
        let (observed, _) = f.physical(120).await;
        let (_, scope) = f.register(&observed);
        f.runtime.store.stop_writes();
        assert_eq!(
            f.runtime.deferred_reset_status(&scope, THREAD),
            DeferredResetStatus::Cancelled
        );
    }

    // Existing Fixture only; no readiness setter or replacement registry.
    async fn readiness_raw(f: &Fixture, complete: bool) {
        readiness_raw_extra(f, complete, serde_json::json!({})).await;
    }

    async fn readiness_raw_extra(f: &Fixture, complete: bool, extra: serde_json::Value) {
        let at = Utc::now();
        let mut raw = serde_json::json!({"observed_at":at,"windows":[
            {"name":"5h","used_pct":10.0,"observed_at":at,
             "reset_at":at+Duration::hours(5)},
            {"name":"7d","used_pct":20.0,"observed_at":at,
             "reset_at":at+Duration::days(1)}
        ]});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        if !complete {
            raw["windows"].as_array_mut().unwrap().pop();
        }
        let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        // Deliberately complete merged snapshot even when raw is partial.
        let mut merged = raw.clone();
        if !complete {
            merged["windows"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "name":"7d","used_pct":20.0,"observed_at":at,
                    "reset_at":at+Duration::days(1)
                }));
        }
        f.runtime
            .apply_quota_snapshot_reported_origin(&f.id, merged, &raw, None, Some(origin))
            .await
            .unwrap();
    }

    async fn wait_until_original_reset(wake: Instant, reset_at: DateTime<Utc>) {
        let wait_bound =
            wake.saturating_duration_since(Instant::now()) + std::time::Duration::from_secs(5);
        let deadline = std::time::Instant::now() + wait_bound;
        tokio::time::timeout(wait_bound, tokio::time::sleep_until(wake))
            .await
            .unwrap();
        // The monotonic wake and UTC reset are sampled separately. A virtual
        // timer returning does not prove UTC advanced; use a real-time budget,
        // rather than a fixed number of potentially early timer completions.
        while Instant::now() < wake || Utc::now() < reset_at {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "original reset boundaries not reached"
            );
            tokio::time::sleep(remaining.min(std::time::Duration::from_millis(10))).await;
        }
        assert!(Instant::now() >= wake);
        assert!(Utc::now() >= reset_at);
    }

    async fn original_due(f: &Fixture, scope: &LaunchOwnerScope) -> (Instant, DateTime<Utc>) {
        let (wake, reset_at) = match f.runtime.deferred_reset_status(scope, THREAD) {
            DeferredResetStatus::Waiting { wake, reset_at, .. }
            | DeferredResetStatus::Suspended { wake, reset_at, .. } => (wake, reset_at),
            _ => panic!("original obligation missing"),
        };
        wait_until_original_reset(wake, reset_at).await;
        assert!(Utc::now() >= reset_at);
        (wake, reset_at)
    }

    fn assert_original_episode(
        f: &Fixture,
        scope: &LaunchOwnerScope,
        id: DeferredResetId,
        original: (Instant, DateTime<Utc>),
    ) {
        match f.runtime.deferred_reset_status(scope, THREAD) {
            DeferredResetStatus::Waiting {
                id: actual,
                wake,
                reset_at,
            }
            | DeferredResetStatus::Suspended {
                id: actual,
                wake,
                reset_at,
            } => {
                assert_eq!(actual, id);
                assert_eq!((wake, reset_at), original);
            }
            _ => panic!("recovery replaced or lost original episode"),
        }
    }

    #[tokio::test]
    async fn n5d_typed_inspection_pending_ready_and_positive_revision_are_same_episode() {
        let f = Fixture::new();
        let (observed, reset) = f.physical(NEAR_RESET_LEAD).await;
        let (id, scope) = f.register(&observed);
        assert_eq!(
            f.runtime
                .inspect_deferred_reset_state(&f.table, &scope, id, &Fixture::operation())
                .unwrap(),
            DeferredResetInspection::Pending
        );
        assert!(
            !f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
        );
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(&scope, id),
            Some(0)
        );
        f.poll(reset, false).await;
        let original = original_due(&f, &scope).await;
        assert_eq!(
            f.runtime
                .inspect_deferred_reset_state(&f.table, &scope, id, &Fixture::operation())
                .unwrap(),
            DeferredResetInspection::Pending
        );
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(&scope, id),
            Some(0)
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_none()
        );
        readiness_raw(&f, true).await;
        let first = f
            .runtime
            .deferred_reset_readiness_revision(&scope, id)
            .unwrap();
        assert!(first > 0);
        assert_eq!(
            f.runtime
                .inspect_deferred_reset_state(&f.table, &scope, id, &Fixture::operation())
                .unwrap(),
            DeferredResetInspection::Ready
        );
        assert!(
            f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
        );
        readiness_raw(&f, false).await;
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(&scope, id),
            Some(first)
        );
        assert_eq!(
            f.runtime
                .inspect_deferred_reset_state(&f.table, &scope, id, &Fixture::operation())
                .unwrap(),
            DeferredResetInspection::Pending
        );
        // Cuts did not advance the underlying successful-publication cursor.
        let record = f.runtime.wait_owner.deferred.record(&scope, id).unwrap();
        assert_eq!(record.readiness_revision.load(Ordering::Acquire), first);
        readiness_raw(&f, true).await;
        let second = f
            .runtime
            .deferred_reset_readiness_revision(&scope, id)
            .unwrap();
        assert!(second > first);
        assert_original_episode(&f, &scope, id, original);
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_some()
        );
        assert_eq!(
            f.runtime
                .inspect_deferred_reset_state(&f.table, &scope, id, &Fixture::operation())
                .unwrap(),
            DeferredResetInspection::Refused
        );
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(&scope, id),
            None
        );
    }

    #[tokio::test]
    async fn n5d_typed_pending_does_not_bypass_terminal_native_binding_or_policy_gates() {
        for veto in 0..6 {
            let f = Fixture::new();
            let (observed, reset) = f.physical(NEAR_RESET_LEAD).await;
            let (id, scope) = f.register(&observed);
            f.poll(reset, false).await;
            original_due(&f, &scope).await;
            assert_eq!(
                f.runtime
                    .inspect_deferred_reset_state(&f.table, &scope, id, &Fixture::operation())
                    .unwrap(),
                DeferredResetInspection::Pending
            );
            let operation = Fixture::operation();
            let mut caller = None;
            match veto {
                0 => {
                    f.runtime.cancel_deferred_reset(&scope, id);
                }
                1 => {
                    f.runtime
                        .store
                        .update_metadata(&f.id, |a| {
                            a.user_state = crate::accounts::UserState::Paused;
                        })
                        .unwrap();
                }
                2 => {
                    f.table
                        .record_launch_model(ROOT, &f.id, "synthetic-other", Utc::now())
                        .unwrap();
                }
                3 => {
                    f.table
                        .set_routing_session_urgent(&session_id(ROOT), On5hLimit::Move)
                        .unwrap();
                }
                4 => {
                    caller = Some(operation.caller_guard());
                    drop(caller.take());
                }
                5 => {
                    f.runtime.store.stop_writes();
                }
                _ => unreachable!(),
            }
            assert_eq!(
                f.runtime
                    .inspect_deferred_reset_state(&f.table, &scope, id, &operation)
                    .unwrap(),
                DeferredResetInspection::Refused
            );
            assert!(
                !f.runtime
                    .inspect_deferred_reset(&f.table, &scope, id, &operation)
                    .unwrap()
            );
            drop(caller);
        }
    }

    #[tokio::test]
    async fn n5d_pending_inspection_revalidates_binding_table_before_returning_pending() {
        let f = Fixture::new();
        let (observed, reset) = f.physical(NEAR_RESET_LEAD).await;
        let (id, scope) = f.register(&observed);
        f.poll(reset, false).await;
        original_due(&f, &scope).await;
        // The original record/fences remain live; this other table has no
        // native placement. An early Pending return would bypass its gate.
        let home = tempfile::tempdir_in(f._home.path()).unwrap();
        let empty = BindingTable::new(home.path(), RoutingConfig::default()).unwrap();
        assert!(f.runtime.wait_owner.deferred.record(&scope, id).is_some());
        assert_eq!(
            f.runtime
                .inspect_deferred_reset_state(&empty, &scope, id, &Fixture::operation())
                .unwrap(),
            DeferredResetInspection::Refused
        );
        assert_eq!(
            f.runtime
                .inspect_deferred_reset_state(&f.table, &scope, id, &Fixture::operation())
                .unwrap(),
            DeferredResetInspection::Pending
        );
    }

    #[tokio::test]
    async fn n5d_positive_revision_advances_on_successful_same_hold_restoration_only() {
        let f = Fixture::new();
        let (observed, reset) = f.physical(120).await;
        let (id, scope) = f.register(&observed);
        let original = match f.runtime.deferred_reset_status(&scope, THREAD) {
            DeferredResetStatus::Waiting { reset_at, wake, .. } => (wake, reset_at),
            _ => panic!("original episode missing"),
        };
        f.poll(reset, false).await;
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(&scope, id),
            Some(0)
        );
        let record = f.runtime.wait_owner.deferred.record(&scope, id).unwrap();
        assert_eq!(record.readiness_revision.load(Ordering::Acquire), 0);
        f.poll(reset, true).await;
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(&scope, id),
            Some(1)
        );
        assert_original_episode(&f, &scope, id, original);
        f.poll(reset, false).await;
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(&scope, id),
            Some(1)
        );
        assert_eq!(record.readiness_revision.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn n5d_positive_readiness_revision_requires_exact_owner_and_episode() {
        let f = Fixture::new();
        let (observed, _) = f.physical(120).await;
        let (id, scope) = f.register(&observed);
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(&scope, id),
            Some(0)
        );
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(
                &scope,
                DeferredResetId::from_u64(id.as_u64() + 1)
            ),
            None
        );
        let other = Fixture::new();
        assert_eq!(
            f.runtime
                .deferred_reset_readiness_revision(&other.lease.scope(), id),
            None
        );
        assert!(f.runtime.cancel_deferred_reset(&scope, id));
        assert_eq!(
            f.runtime.deferred_reset_readiness_revision(&scope, id),
            None
        );
    }

    #[tokio::test]
    async fn n5d_registered_partial_through_reset_late_full_readiness_consumes_once() {
        let f = Fixture::new();
        let (observed, reset) = f.physical(NEAR_RESET_LEAD).await;
        let (id, scope) = f.register(&observed);
        f.poll(reset, false).await;
        let original = original_due(&f, &scope).await;
        assert!(
            !f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap(),
            "original reset alone cannot clear BLOCK_REPORT"
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
                .is_none()
        );
        assert_original_episode(&f, &scope, id, original);
        readiness_raw(&f, true).await;
        assert!(
            f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap(),
            "fresh accepted full native readiness repairs registered wait"
        );
        assert_original_episode(&f, &scope, id, original);
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
                .is_some()
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
                .is_none()
        );
        assert!(!f.runtime.cancel_deferred_reset(&scope, id));
    }

    #[tokio::test]
    async fn n5d_registered_healthy_available_credits_without_permission_recovers() {
        let f = Fixture::new();
        assert!(!f.runtime.store.load(&f.id).unwrap().allow_credits);
        let (observed, reset) = f.physical(NEAR_RESET_LEAD).await;
        let (id, scope) = f.register(&observed);
        f.poll(reset, false).await;
        let original = original_due(&f, &scope).await;
        let extra = serde_json::json!({"credits":{"has_credits":true,"balance":5.0}});
        assert!(crate::credits::entitled(&extra));
        readiness_raw_extra(&f, true, extra).await;
        let account = f.runtime.store.load(&f.id).unwrap();
        assert!(!account.allow_credits);
        assert!(!account.quota_credit_releasable);
        assert!(matches!(account.health, Health::Ready));
        assert!(account.quota_cooling.is_none());
        assert!(
            f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
        );
        assert_original_episode(&f, &scope, id, original);
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_some()
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn n5d_registered_available_credits_do_not_excuse_paid_serving() {
        for paid in [
            serde_json::json!({"signals":{"serving_from_credits":true}}),
            serde_json::json!({"serving_from_credits":true}),
            serde_json::json!({"signals":{"active_limit":"credits"}}),
            serde_json::json!({"paid_serving_observed_at":Utc::now()}),
        ] {
            let f = Fixture::new();
            assert!(!f.runtime.store.load(&f.id).unwrap().allow_credits);
            let (observed, reset) = f.physical(NEAR_RESET_LEAD).await;
            let (id, scope) = f.register(&observed);
            f.poll(reset, false).await;
            original_due(&f, &scope).await;
            let mut extra = paid;
            extra["credits"] = serde_json::json!({"has_credits":true,"balance":5.0});
            readiness_raw_extra(&f, true, extra).await;
            assert!(
                !f.runtime
                    .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                    .unwrap()
            );
            assert!(
                f.runtime
                    .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation())
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn n5d_registered_ready_account_partial_raw_cannot_borrow_merged_window() {
        let f = Fixture::new();
        let (observed, reset) = f.physical(NEAR_RESET_LEAD).await;
        let (id, scope) = f.register(&observed);
        f.poll(reset, false).await;
        let original = original_due(&f, &scope).await;
        // Native account expiry may run independently before the poll.
        assert!(matches!(
            f.runtime.store.load(&f.id).unwrap().health,
            Health::Ready
        ));
        readiness_raw(&f, false).await;
        assert!(
            !f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
                .is_none(),
            "durable Ready plus merged complete is insufficient"
        );
        assert_original_episode(&f, &scope, id, original);
    }

    #[tokio::test]
    async fn n5d_registered_new_partial_revokes_readiness_then_full_recovers_same_id() {
        let f = Fixture::new();
        let (observed, reset) = f.physical(NEAR_RESET_LEAD).await;
        let (id, scope) = f.register(&observed);
        f.poll(reset, false).await;
        let original = original_due(&f, &scope).await;
        readiness_raw(&f, true).await;
        assert!(
            f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
        );
        readiness_raw(&f, false).await;
        assert!(
            !f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
        );
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
                .is_none()
        );
        readiness_raw(&f, true).await;
        assert!(
            f.runtime
                .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
        );
        assert_original_episode(&f, &scope, id, original);
        assert!(
            f.runtime
                .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn n5d_registered_cancel_or_account_pause_cannot_be_recovered_by_full_poll() {
        for pause in [false, true] {
            let f = Fixture::new();
            let (observed, reset) = f.physical(NEAR_RESET_LEAD).await;
            let (id, scope) = f.register(&observed);
            f.poll(reset, false).await;
            original_due(&f, &scope).await;
            if pause {
                f.runtime
                    .store
                    .update_metadata(&f.id, |account| {
                        account.user_state = crate::accounts::UserState::Paused;
                    })
                    .unwrap();
            } else {
                assert!(f.runtime.cancel_deferred_reset(&scope, id));
            }
            readiness_raw(&f, true).await;
            assert_eq!(
                f.runtime.deferred_reset_status(&scope, THREAD),
                DeferredResetStatus::Cancelled
            );
            assert!(
                !f.runtime
                    .inspect_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                    .unwrap()
            );
            assert!(
                f.runtime
                    .consume_deferred_reset(&f.table, &scope, id, &Fixture::operation(),)
                    .unwrap()
                    .is_none()
            );
        }
    }
}
