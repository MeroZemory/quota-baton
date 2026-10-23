//! Process-local attribution only. No wait selection or dispatch is enabled here.

#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod runtime_tests;

use super::{
    AccountRuntime, HealthSignal,
    wait_proof::{
        AcceptPermit, ReportOrigin, ReportSource, SealedHttpReport, WaitProofs, WaitTicket,
    },
};
use crate::accounts::{
    Account, AccountStore, BeforeAccountMutation, Health, HealthReason, ReportMutation,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::sync::{Arc, Weak};
use tokio::sync::{Notify, futures::Notified};

const CAPACITY: usize = 4096;
const MAX_MODEL_BYTES: usize = 256;
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Constructed only from this response's bounded complete decoded body. This is
/// classification data, never registry authority, and retains no body content.
pub(crate) struct QualifiedHttp429Body {
    qualified: bool,
    known: Option<KnownQuotaBody>,
}

/// Exact published proof from this physical response, not wait/send authority.
/// No constructor outside the native final-report adapter; no body is retained.
pub(crate) struct ObservedHttp429 {
    native: WaitTicket,
}

struct KnownQuotaBody {
    reset_at: DateTime<Utc>,
    model: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct QuotaBodyEnvelope {
    error: QuotaBodyError,
    status: Option<u16>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct QuotaBodyError {
    #[serde(rename = "type")]
    kind: Option<String>,
    code: Option<String>,
    #[serde(rename = "message")]
    _message: Option<String>,
    resets_at: i64,
}

/// Plain bounded classifier data; retained evidence is not admission authority.
pub(super) struct ShortQuotaProof {
    reset_at: DateTime<Utc>,
    model: Option<String>,
    observed_at: DateTime<Utc>,
    earliest_reset_at: DateTime<Utc>,
    started_at: DateTime<Utc>,
    scope: String,
    allow_credits: bool,
    quota_credit_releasable: bool,
}

impl ShortQuotaProof {
    /// Applicability of the original report, not request identity. Only a
    /// validated account-scoped report without a model covers other models.
    fn applies_to(&self, request_model: Option<&str>) -> bool {
        if self.scope == "account" {
            return self
                .model
                .as_deref()
                .is_none_or(|reported| request_model == Some(reported));
        }
        self.scope.strip_prefix("model:").is_some_and(|blocked| {
            !blocked.is_empty()
                && self.model.as_deref() == Some(blocked)
                && request_model == Some(blocked)
        })
    }

    fn valid_at(&self, now: DateTime<Utc>) -> bool {
        self.reset_at > now
            && self.earliest_reset_at > now
            && self.observed_at <= now
            && self.started_at <= now
            && now.signed_duration_since(self.observed_at) <= Duration::seconds(300)
            && self
                .model
                .as_ref()
                .is_none_or(|model| model.len() <= MAX_MODEL_BYTES)
    }

    fn matches_current(&self, account: &Account, now: DateTime<Utc>) -> bool {
        self.valid_at(now)
            && account.user_state == crate::accounts::UserState::Active
            && !account.drain
            && !account.auth_probe_pending
            && account.auth_failures == 0
            && account.payment_until.is_none_or(|until| until <= now)
            && account.allow_credits == self.allow_credits
            && account.quota_credit_releasable == self.quota_credit_releasable
            && matches!(&account.health, Health::Cooling {
                reason: HealthReason::Quota, until, scope,
            } if *until == self.reset_at && *scope == self.scope)
            && account.quota_cooling.as_ref().is_some_and(|hold| {
                hold.window == "5h"
                    && hold.reset_at == self.reset_at
                    && hold.started_at == self.started_at
                    && hold.started_at <= now
            })
    }

    /// Compose exactly the sealed header's Codex account-level hardening with
    /// the actual reducer result. Nothing here supplies an origin or permit.
    fn compose_codex_body(
        self,
        before: &Account,
        actual_after: &Account,
        actual_signal: &crate::accounts::HealthSignal,
        body: &KnownQuotaBody,
        known_reset: Option<DateTime<Utc>>,
        applied_at: DateTime<Utc>,
    ) -> Option<Self> {
        if before.provider != crate::accounts::Provider::Codex
            || self.scope != "account"
            || !self.quota_credit_releasable
            || !self.matches_current(before, applied_at)
            || body.model != self.model
            || body.reset_at != self.reset_at
            || known_reset != Some(self.reset_at)
            || !matches!(actual_signal, crate::accounts::HealthSignal::Cooling {
                reason: HealthReason::Quota, until, scope,
            } if *until == self.reset_at && scope == "account")
        {
            return None;
        }
        let mut expected = before.clone();
        // Store sampled expiry before this closure. A further time crossing is
        // independent of the header application and cannot borrow its proof.
        if expected.expire_health(applied_at) || &expected != before {
            return None;
        }
        expected.apply_health_signal(actual_signal.clone(), applied_at);
        if &expected != actual_after || actual_after.quota_credit_releasable {
            return None;
        }
        // Only the candidate changes to describe the actual native output.
        // Its original raw times, literal hold, scope and permissions survive.
        let composed = Self {
            quota_credit_releasable: actual_after.quota_credit_releasable,
            ..self
        };
        composed
            .matches_current(actual_after, applied_at)
            .then_some(composed)
    }
}

pub(super) enum ReportPublication {
    Immediate,
    DeferredHttp429,
}

/// Successful, executed header application, never a published wait ticket.
/// All fields are private, compact and non-serializable; Drop cancels minting.
pub(crate) struct PendingHttp429 {
    owner: Weak<WaitOwner>,
    id: String,
    sealed: SealedHttpReport,
    proof: ShortQuotaProof,
}

/// Revocable wait evidence only. Not a dispatch lease or cancellation admission.
/// No constructors, identity fields, cloning or serialization are exposed.
pub(crate) struct QuotaWaitTicket {
    native: WaitTicket,
    reset_at: DateTime<Utc>,
    request_model: Option<String>,
}

impl QuotaWaitTicket {
    pub(crate) fn reset_at(&self) -> DateTime<Utc> {
        self.reset_at
    }
}

/// Read-only initial-response classification; never a WAIT receipt or send
/// authority. Constructed only under the existing native inspection gates.
pub(super) struct InitialFiveHourLimit {
    ticket: QuotaWaitTicket,
    deadline: tokio::time::Instant,
    policy: WaitPolicySnapshot,
}

impl InitialFiveHourLimit {
    pub(super) fn reset_at(&self) -> DateTime<Utc> {
        self.ticket.reset_at
    }

    // Reject-only handoff check after the blocking response assembly. No Store,
    // mutation, renewal, new ticket or interval is obtained here.
    pub(super) fn reply_current(
        &self,
        owner: &WaitOwner,
        policies: &crate::routing::PolicyStore,
    ) -> bool {
        policies
            .try_with_quota_wait_condition(&self.policy.key, &self.policy.account, |plan| {
                if !same_wait_plan(&self.policy.plan, plan)
                    || tokio::time::Instant::now() >= self.deadline
                {
                    return None;
                }
                let proof = owner.proofs.admit(&self.ticket.native).ok()?;
                (proof.reset_at == self.ticket.reset_at
                    && proof.applies_to(self.ticket.request_model.as_deref())
                    && proof.valid_at(Utc::now()))
                .then_some(())
            })
            .is_some()
    }
}

/// One gate-scoped WAIT receipt. Never permission to dispatch or retain a
/// binding/credential lease. Identity, reset and monotonic bounds cannot change.
pub(crate) struct AdmittedQuotaWait {
    ticket: QuotaWaitTicket,
    account: String,
    key: String,
    model: Option<String>,
    provider: crate::accounts::Provider,
    policy_key: String,
    plan: crate::routing::Plan,
    wake: tokio::time::Instant,
    deadline: tokio::time::Instant,
}

/// Read-only effective policy at admission; no ticket or quota/dispatch authority.
/// Used only to stop optional waiting while a native inspection is blocked.
pub(crate) struct WaitPolicySnapshot {
    key: String,
    account: String,
    plan: crate::routing::Plan,
}

impl WaitPolicySnapshot {
    /// Immutable admission policy data, never a ticket or dispatch permission.
    pub(crate) fn plan(&self) -> &crate::routing::Plan {
        &self.plan
    }

    pub(crate) fn unchanged(&self, policies: &crate::routing::PolicyStore) -> bool {
        policies
            .with_wait_plan(&self.key, &self.account, |current| {
                same_wait_plan(&self.plan, current).then_some(())
            })
            .is_some()
    }
}

impl AdmittedQuotaWait {
    pub(crate) fn policy_snapshot(&self) -> WaitPolicySnapshot {
        WaitPolicySnapshot {
            key: self.policy_key.clone(),
            account: self.account.clone(),
            plan: self.plan.clone(),
        }
    }
    pub(crate) fn account(&self) -> &str {
        &self.account
    }
    #[cfg(test)]
    pub(crate) fn key(&self) -> &str {
        &self.key
    }
    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
    pub(crate) fn reset_at(&self) -> DateTime<Utc> {
        self.ticket.reset_at()
    }
    #[cfg(test)]
    pub(crate) fn policy_revision(&self) -> u64 {
        self.plan.revision
    }
    pub(crate) fn wake(&self) -> tokio::time::Instant {
        self.wake
    }
    pub(crate) fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }
}

fn same_wait_plan(original: &crate::routing::Plan, current: &crate::routing::Plan) -> bool {
    // Global revision can change for unrelated sessions/projects. Compare the
    // effective native semantics under the caller's policy publication guard.
    original.settings == current.settings
        && original.anchor == current.anchor
        && original.project == current.project
        && original.source == current.source
}

fn wait_wake(
    reset: DateTime<Utc>,
    wall: DateTime<Utc>,
    mono: tokio::time::Instant,
    budget_secs: u64,
    deadline: tokio::time::Instant,
) -> Option<tokio::time::Instant> {
    let remaining = reset.signed_duration_since(wall).to_std().ok()?;
    if remaining.is_zero() || remaining > std::time::Duration::from_secs(budget_secs) {
        return None;
    }
    let wake = mono.checked_add(remaining)?;
    (wake < deadline).then_some(wake)
}

pub(super) struct WaitOwner {
    proofs: WaitProofs<ShortQuotaProof>,
    changed: Notify,
}

impl WaitOwner {
    pub(super) fn new() -> Self {
        Self {
            proofs: WaitProofs::new(CAPACITY),
            changed: Notify::new(),
        }
    }

    /// Create and enable BEFORE blocking admission/revalidation. A notification
    /// is only a hint to recheck all native gates, never new authority.
    pub(super) fn notified(&self) -> Notified<'_> {
        self.changed.notified()
    }

    /// Caller holds the existing outer Store/runtime/binding/policy gates and
    /// supplies current metadata and exact request model. No native I/O here.
    pub(super) fn acquire_current_unlocked(
        &self,
        account: &Account,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<QuotaWaitTicket> {
        if model.is_some_and(|model| model.len() > MAX_MODEL_BYTES) {
            return None;
        }
        let (native, proof) = self.proofs.acquire(&account.id).ok()?;
        if !proof.applies_to(model) || !proof.matches_current(account, now) {
            return None;
        }
        Some(QuotaWaitTicket {
            native,
            reset_at: proof.reset_at,
            request_model: model.map(str::to_owned),
        })
    }

    /// Invoke immediately before publishing a revocable WAIT receipt, still under
    /// all outer gates and after their deadline/cancellation/credential checks.
    /// This is neither send admission nor a substitute for those checks.
    pub(super) fn ticket_current_unlocked(
        &self,
        ticket: &QuotaWaitTicket,
        account: &Account,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> bool {
        if ticket.native.account_id() != account.id || ticket.request_model.as_deref() != model {
            return false;
        }
        let Ok(proof) = self.proofs.admit(&ticket.native) else {
            return false;
        };
        proof.reset_at == ticket.reset_at
            && proof.applies_to(model)
            && proof.matches_current(account, now)
    }

    /// Read-only fallback condition for an already observed physical 429.
    /// The caller holds the same native gates as wait inspection. This grants
    /// neither a receipt nor dispatch; it can only retain the original response.
    pub(super) fn observed_http_far_wait_unlocked(
        &self,
        observed: &ObservedHttp429,
        intent: &crate::bindings::WaitIntent<'_>,
        account: &Account,
    ) -> Option<()> {
        if account.id != intent.account()
            || account.provider != intent.provider()
            || observed.native.account_id() != account.id
            || !intent.existing_placement()
            || intent.plan().settings.mode == crate::routing::Mode::Strict
            || intent.plan().settings.on_5h_limit != crate::routing::On5hLimit::Wait
        {
            return None;
        }
        let proof = self.proofs.admit(&observed.native).ok()?;
        let wall = Utc::now();
        let mono = tokio::time::Instant::now();
        if !proof.applies_to(intent.model()) || !proof.matches_current(account, wall) {
            return None;
        }
        let remaining = proof.reset_at.signed_duration_since(wall).to_std().ok()?;
        let beyond_hold =
            remaining > std::time::Duration::from_secs(intent.plan().settings.reset_wait_secs);
        let beyond_deadline = mono.checked_add(remaining)? >= intent.deadline();
        if remaining.is_zero() || !(beyond_hold || beyond_deadline) || !intent.caller_current() {
            return None;
        }
        // Recheck the exact physical report after classification/clock work.
        self.proofs.admit(&observed.native).ok()?;
        Some(())
    }

    /// A full current proof can classify a qb-authored initial response without
    /// claiming that the usage poll was a physical provider rejection.
    pub(super) fn initial_far_wait_unlocked(
        &self,
        intent: &crate::bindings::WaitIntent<'_>,
        account: &Account,
    ) -> Option<InitialFiveHourLimit> {
        if account.id != intent.account()
            || account.provider != intent.provider()
            || !intent.existing_placement()
            || intent.plan().settings.mode == crate::routing::Mode::Strict
            || intent.plan().settings.on_5h_limit != crate::routing::On5hLimit::Wait
        {
            return None;
        }
        let wall = Utc::now();
        let mono = tokio::time::Instant::now();
        let ticket = self.acquire_current_unlocked(account, intent.model(), wall)?;
        let remaining = ticket.reset_at.signed_duration_since(wall).to_std().ok()?;
        if remaining.is_zero()
            || !(remaining > std::time::Duration::from_secs(intent.plan().settings.reset_wait_secs)
                || mono.checked_add(remaining)? >= intent.deadline())
        {
            return None;
        }
        let limit = InitialFiveHourLimit {
            ticket,
            deadline: intent.deadline(),
            policy: WaitPolicySnapshot {
                key: intent.policy_key().to_owned(),
                account: intent.account().to_owned(),
                plan: intent.plan().clone(),
            },
        };
        (intent.caller_current()
            && self.ticket_current_unlocked(&limit.ticket, account, intent.model(), Utc::now()))
        .then_some(limit)
    }

    pub(super) fn initial_far_wait_is_current_unlocked(
        &self,
        limit: &InitialFiveHourLimit,
        intent: &crate::bindings::WaitIntent<'_>,
        account: &Account,
    ) -> bool {
        // Same callback/publication; never acquire a replacement proof.
        intent.caller_current()
            && intent.deadline() == limit.deadline
            && same_wait_plan(&limit.policy.plan, intent.plan())
            && self.ticket_current_unlocked(&limit.ticket, account, intent.model(), Utc::now())
    }

    /// Current metadata/credentials and runtime eligibility were checked by the
    /// caller under Store -> runtime -> binding mutations -> policy publication.
    /// This method does not reacquire those gates or admit IssueOperation/send.
    pub(super) fn admit_current_unlocked(
        &self,
        intent: &crate::bindings::WaitIntent<'_>,
        account: &Account,
    ) -> Option<AdmittedQuotaWait> {
        if account.id != intent.account()
            || account.provider != intent.provider()
            || !intent.existing_placement()
            || intent.account().len() > 128
            || intent.key().len() > 1024
            || intent.policy_key().len() > 1024
            || intent
                .plan()
                .anchor
                .as_ref()
                .is_some_and(|value| value.len() > 1024)
            || intent
                .plan()
                .project
                .as_ref()
                .is_some_and(|value| value.len() > 1024)
            || intent
                .model()
                .is_some_and(|model| model.len() > MAX_MODEL_BYTES)
        {
            return None;
        }
        // Pair after blocking metadata/credential work: wall then monotonic.
        // Preemption between samples can make wake later, never artificially
        // early. A late computed wake still must precede the original deadline.
        let wall = Utc::now();
        let mono = tokio::time::Instant::now();
        let ticket = self.acquire_current_unlocked(account, intent.model(), wall)?;
        let wake = wait_wake(
            ticket.reset_at(),
            wall,
            mono,
            intent.plan().settings.reset_wait_secs,
            intent.deadline(),
        )?;
        let receipt = AdmittedQuotaWait {
            ticket,
            account: intent.account().to_owned(),
            key: intent.key().to_owned(),
            model: intent.model().map(str::to_owned),
            provider: intent.provider(),
            policy_key: intent.policy_key().to_owned(),
            plan: intent.plan().clone(),
            wake,
            deadline: intent.deadline(),
        };
        // All bounded allocations precede the final caller and proof check.
        if tokio::time::Instant::now() >= receipt.wake
            || tokio::time::Instant::now() >= receipt.deadline
            || !intent.caller_current()
            || !self.ticket_current_unlocked(&receipt.ticket, account, intent.model(), Utc::now())
        {
            return None;
        }
        Some(receipt)
    }

    /// Revalidate the SAME receipt. No acquire(), new ticket, wake conversion or
    /// interval refresh; reset/expiry always leads to native reset re-selection.
    #[cfg(test)]
    pub(super) fn revalidate_current_unlocked(
        &self,
        receipt: &AdmittedQuotaWait,
        intent: &crate::bindings::WaitIntent<'_>,
        account: &Account,
    ) -> bool {
        if receipt.account != intent.account()
            || account.id != receipt.account
            || receipt.key != intent.key()
            || receipt.provider != intent.provider()
            || account.provider != receipt.provider
            || receipt.model.as_deref() != intent.model()
            || receipt.policy_key != intent.policy_key()
            || !same_wait_plan(&receipt.plan, intent.plan())
            || receipt.deadline != intent.deadline()
            || !intent.existing_placement()
        {
            return false;
        }
        let wall = Utc::now();
        if tokio::time::Instant::now() >= receipt.wake
            || tokio::time::Instant::now() >= receipt.deadline
            || !intent.caller_current()
        {
            return false;
        }
        self.ticket_current_unlocked(&receipt.ticket, account, intent.model(), wall)
    }

    /// Consume the old envelope under ALL current native outer gates. A newer
    /// full proof may supply a new ticket, never a reset, interval or dispatch.
    pub(super) fn renew_current_unlocked(
        &self,
        old: AdmittedQuotaWait,
        intent: &crate::bindings::WaitIntent<'_>,
        account: &Account,
    ) -> Option<AdmittedQuotaWait> {
        if old.account != intent.account()
            || account.id != old.account
            || old.key != intent.key()
            || old.provider != intent.provider()
            || account.provider != old.provider
            || old.model.as_deref() != intent.model()
            || old.policy_key != intent.policy_key()
            || !same_wait_plan(&old.plan, intent.plan())
            || old.deadline != intent.deadline()
            || !intent.existing_placement()
        {
            return None;
        }
        // Only inspect the original bounds; NEVER map reset into a new wake.
        let wall = Utc::now();
        let mono = tokio::time::Instant::now();
        let candidate = self.renewal_candidate_unlocked(old, account, wall, mono)?;
        // Any new ticket's bounded ID allocation already happened. Check the
        // original operation immediately before its final live-proof check.
        if tokio::time::Instant::now() >= candidate.wake
            || tokio::time::Instant::now() >= candidate.deadline
            || !intent.caller_current()
            || !self.ticket_current_unlocked(&candidate.ticket, account, intent.model(), Utc::now())
        {
            return None;
        }
        Some(candidate)
    }

    /// Internal candidate builder, NOT admission: the caller above must validate
    /// intent, credentials/runtime gates, caller and final ticket before return.
    /// Injected clocks let tests prove preservation without fake WaitIntent.
    fn renewal_candidate_unlocked(
        &self,
        mut old: AdmittedQuotaWait,
        account: &Account,
        wall: DateTime<Utc>,
        mono: tokio::time::Instant,
    ) -> Option<AdmittedQuotaWait> {
        if account.id != old.account
            || account.provider != old.provider
            || mono >= old.wake
            || mono >= old.deadline
            || old.wake >= old.deadline
        {
            return None;
        }
        let remaining = old.reset_at().signed_duration_since(wall).to_std().ok()?;
        if remaining.is_zero()
            || remaining > std::time::Duration::from_secs(old.plan.settings.reset_wait_secs)
        {
            return None;
        }
        if self.ticket_current_unlocked(&old.ticket, account, old.model.as_deref(), wall) {
            return Some(old); // Still current: keep even the original ticket.
        }
        let current = self.acquire_current_unlocked(account, old.model.as_deref(), wall)?;
        if current.reset_at() != old.reset_at() {
            return None;
        }
        // Move, do not clone, the original envelope. This assignment changes
        // ONLY ticket identity; the reset is required equal before replacement.
        old.ticket = current;
        Some(old)
    }

    fn capture(&self, id: &str, source: ReportSource) -> Option<ReportOrigin> {
        let result = self.proofs.capture(id, source);
        // Capture normally preserves proof. Exhaustion can revoke it; even
        // registry failures must wake existing waiters for gated revalidation.
        if result.is_err() {
            self.changed.notify_waiters();
        }
        result.ok()
    }

    fn retire(&self, id: &str) {
        let _ = self.proofs.retire(id);
        self.changed.notify_waiters();
    }

    fn rotate(&self, permit: AcceptPermit) -> Option<AcceptPermit> {
        let result = self.proofs.invalidate_accepted(permit);
        self.changed.notify_waiters();
        result.ok()
    }

    fn mint(&self, permit: AcceptPermit, proof: ShortQuotaProof) {
        let _ = self.proofs.mint(permit, proof);
        self.changed.notify_waiters();
    }

    /// Caller holds the store transaction. The hook owns neither Store nor Runtime.
    pub(super) fn attach_unlocked(self: &Arc<Self>, store: &AccountStore) {
        if store.writes_stopped() {
            self.close();
        }
        let weak = Arc::downgrade(self);
        store.set_pre_mutation_hook_unlocked(Arc::new(move |mutation| {
            let Some(owner) = weak.upgrade() else { return };
            match mutation {
                BeforeAccountMutation::Stop => owner.close(),
                BeforeAccountMutation::Retire { id } => {
                    owner.retire(id);
                }
                BeforeAccountMutation::Metadata { id, .. }
                | BeforeAccountMutation::HealthExpiry { id }
                | BeforeAccountMutation::Settings { id }
                | BeforeAccountMutation::FullSave { id } => owner.invalidate(id),
            }
        }));
    }

    pub(super) fn close(&self) {
        let _ = self.proofs.close();
        self.changed.notify_waiters();
    }

    fn invalidate(&self, id: &str) {
        let _ = self.proofs.invalidate(id);
        self.changed.notify_waiters();
    }

    pub(super) fn accept(
        self: &Arc<Self>,
        id: &str,
        origin: ReportOrigin,
    ) -> Option<ReportApplication> {
        let result = self.proofs.accept(id, origin);
        self.changed.notify_waiters();
        let permit = result.ok()?;
        Some(ReportApplication {
            owner: Arc::downgrade(self),
            id: id.to_owned(),
            permit: Some(permit),
        })
    }

    /// Caller holds the Store transaction. Continue revalidates; the adapter
    /// still MUST own-rotate before the native signal (also for no signal).
    fn continue_http(
        self: &Arc<Self>,
        id: &str,
        pending: Option<PendingHttp429>,
        known_reset: Option<DateTime<Utc>>,
        signal: Option<&HealthSignal>,
        body_qualified: bool,
    ) -> Option<(ReportApplication, ShortQuotaProof)> {
        let continued = pending.and_then(|pending| {
            let owner = pending.owner.upgrade()?;
            let compatible_signal = match signal {
                None => true,
                Some(HealthSignal::Cooling {
                    reason: HealthReason::Quota,
                    until,
                    scope,
                }) => *until == pending.proof.reset_at && *scope == pending.proof.scope,
                Some(_) => false,
            };
            if !Arc::ptr_eq(self, &owner)
                || pending.id != id
                || !body_qualified
                || known_reset != Some(pending.proof.reset_at)
                || !compatible_signal
                || !pending.proof.valid_at(Utc::now())
            {
                return None;
            }
            let permit = self.proofs.continue_http(pending.sealed).ok()?;
            Some((
                ReportApplication {
                    owner: Arc::downgrade(self),
                    id: pending.id,
                    permit: Some(permit),
                },
                pending.proof,
            ))
        });
        if continued.is_none() {
            // EVERY actual quota result cuts, even stale/unknown/same-health
            // reports with no Store signal. Never veto native C5 on failure.
            self.invalidate(id);
        }
        continued
    }
}

/// Store type erasure only carries this operation's accepted permit. Independent
/// expiry/metadata/Stop hooks always revoke it through the normal weak hook.
pub(super) struct ReportApplication {
    owner: Weak<WaitOwner>,
    id: String,
    permit: Option<AcceptPermit>,
}

impl ReportMutation for ReportApplication {
    fn before_store_mutation(&mut self, id: &str) {
        let permit = self.permit.take();
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        if id != self.id {
            owner.invalidate(id);
            owner.invalidate(&self.id);
            return;
        }
        self.permit = permit.and_then(|permit| owner.rotate(permit));
        if self.permit.is_none() {
            owner.invalidate(id);
        }
    }
}

impl ReportApplication {
    pub(super) fn finish(mut self, candidate: Option<ShortQuotaProof>) {
        let Some(proof) = candidate else { return };
        // Persistence may have crossed the deadline. Refuse only optional proof.
        if !proof.valid_at(Utc::now()) {
            return;
        }
        if let Some(owner) = self.owner.upgrade()
            && let Some(permit) = self.permit.take()
        {
            owner.mint(permit, proof);
        }
    }

    // Store remains held from this response's native final classification through
    // mint/acquire. Every competing report publisher also requires Store. A cut
    // can only decline the token; later reports cannot substitute their evidence.
    fn finish_http(mut self, candidate: Option<ShortQuotaProof>) -> Option<ObservedHttp429> {
        let proof = candidate?;
        if !proof.valid_at(Utc::now()) {
            return None;
        }
        let owner = self.owner.upgrade()?;
        let permit = self.permit.take()?;
        let minted = owner.proofs.mint(permit, proof);
        owner.changed.notify_waiters();
        minted.ok()?;
        let (native, _) = owner.proofs.acquire(&self.id).ok()?;
        Some(ObservedHttp429 { native })
    }

    pub(super) fn seal_http(
        mut self,
        candidate: Option<ShortQuotaProof>,
        account: &Account,
    ) -> Option<PendingHttp429> {
        let proof = candidate?;
        if !proof.matches_current(account, Utc::now()) {
            return None;
        }
        let Health::Cooling {
            reason: HealthReason::Quota,
            until,
            scope,
        } = &account.health
        else {
            return None;
        };
        if *until != proof.reset_at
            || !(scope == "account"
                || proof
                    .model
                    .as_ref()
                    .is_some_and(|model| scope == &format!("model:{model}")))
        {
            return None;
        }
        let owner = self.owner.upgrade()?;
        let sealed = owner.proofs.seal_http(self.permit.take()?).ok()?;
        Some(PendingHttp429 {
            owner: self.owner,
            id: self.id,
            sealed,
            proof,
        })
    }
}

pub(super) fn classify(
    before: &Account,
    after: &Account,
    raw: &Value,
    model: Option<&str>,
    applied_at: DateTime<Utc>,
) -> Option<ShortQuotaProof> {
    if model.is_some_and(|model| model.len() > MAX_MODEL_BYTES) {
        return None;
    }
    let reset_at = if before.quota_cooling != after.quota_cooling {
        crate::bindings::quota_wait::newly_applied_short_quota_reset(
            before, after, raw, model, applied_at,
        )?
    } else {
        // Even an existing hold cannot borrow raw attribution from cached merge
        // or release filtering that produced a different native transition.
        let mut expected = before.clone();
        expected.apply_health_signal(
            crate::accounts::HealthSignal::UsageSnapshot {
                snapshot: raw.clone(),
                model: model.map(str::to_owned),
                projected_allowance: 0.0,
            },
            applied_at,
        );
        if &expected != after {
            return None;
        }
        crate::bindings::quota_wait::short_quota_reset(after, Some(raw), model, applied_at)?
    };
    Some(ShortQuotaProof {
        reset_at,
        model: model.map(str::to_owned),
        observed_at: raw["observed_at"].as_str()?.parse().ok()?,
        // The classifier verified every raw timestamp. Preserve the earliest
        // expiry as well as the 300s age bound without retaining arbitrary JSON.
        earliest_reset_at: raw["windows"]
            .as_array()?
            .iter()
            .map(|window| window["reset_at"].as_str()?.parse().ok())
            .collect::<Option<Vec<DateTime<Utc>>>>()?
            .into_iter()
            .min()?,
        started_at: after.quota_cooling.as_ref()?.started_at,
        scope: match &after.health {
            Health::Cooling { scope, .. } => scope.clone(),
            _ => return None,
        },
        allow_credits: after.allow_credits,
        quota_credit_releasable: after.quota_credit_releasable,
    })
}

impl AccountRuntime {
    /// Relay supplies Some ONLY for actual EOF, no stream error and successful
    /// bounded decoding. Native classification still runs separately, unchanged.
    pub(crate) fn qualify_http_429_body(
        decoded_complete: Option<&[u8]>,
        model: Option<&str>,
    ) -> QualifiedHttp429Body {
        let mut body = QualifiedHttp429Body {
            qualified: false,
            known: None,
        };
        let Some(decoded) = decoded_complete.filter(|decoded| decoded.len() <= MAX_BODY_BYTES)
        else {
            return body;
        };
        if decoded.is_empty() {
            body.qualified = true;
            return body;
        }
        body.qualified =
            serde_json::from_slice::<Value>(decoded).is_ok_and(|value| value.is_object());
        if !body.qualified || model.is_some_and(|model| model.len() > MAX_MODEL_BYTES) {
            return body;
        }
        // Struct deserialization rejects duplicate and unknown fields. Only
        // one absolute reset and the ordinary generic quota envelope qualify;
        // new limit/credits/model declarations need separate reviewed handling.
        let Ok(envelope) = serde_json::from_slice::<QuotaBodyEnvelope>(decoded) else {
            return body;
        };
        let error = envelope.error;
        if envelope.status.is_some_and(|status| status != 429)
            || (error.kind.is_none() && error.code.is_none())
            || [error.kind.as_deref(), error.code.as_deref()]
                .into_iter()
                .flatten()
                .any(|claim| claim != "usage_limit_reached")
        {
            return body;
        }
        if let Some(reset_at) = DateTime::from_timestamp(error.resets_at, 0) {
            body.known = Some(KnownQuotaBody {
                reset_at,
                model: model.map(str::to_owned),
            });
        }
        body
    }

    /// Caller has classified/reconciled a real Quota outcome and consumes its
    /// signal here exactly once. Transport completeness supplies no authority.
    #[cfg(test)]
    pub(crate) async fn finish_http_429_report(
        &self,
        id: &str,
        pending: Option<PendingHttp429>,
        known_reset: Option<DateTime<Utc>>,
        signal: Option<HealthSignal>,
        body_qualified: bool,
    ) -> Result<(), ()> {
        // Legacy boolean carries no known-body attestation and keeps its
        // conservative whole-Account-no-op behavior unchanged.
        self.finish_http_429_report_with_body(
            id,
            pending,
            known_reset,
            signal,
            QualifiedHttp429Body {
                qualified: body_qualified,
                known: None,
            },
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn finish_http_429_report_with_body(
        &self,
        id: &str,
        pending: Option<PendingHttp429>,
        known_reset: Option<DateTime<Utc>>,
        signal: Option<HealthSignal>,
        body: QualifiedHttp429Body,
    ) -> Result<(), ()> {
        self.finish_http_429_report_observed(id, pending, known_reset, signal, body)
            .await
            .map(|_| ())
    }

    pub(crate) async fn finish_http_429_report_observed(
        &self,
        id: &str,
        pending: Option<PendingHttp429>,
        known_reset: Option<DateTime<Utc>>,
        signal: Option<HealthSignal>,
        body: QualifiedHttp429Body,
    ) -> Result<Option<ObservedHttp429>, ()> {
        let store = self.store.clone();
        let owner = self.wait_owner.clone();
        let id = id.to_owned();
        let has_signal = signal.is_some();
        let result = tokio::task::spawn_blocking(move || {
            let _gate = match store.transaction() {
                Ok(gate) => gate,
                Err(_) => {
                    owner.close();
                    // Old no-signal handling only invalidated optional proof;
                    // it did not expose Store-gate failure as a native 502.
                    return if has_signal { Err(()) } else { Ok(None) };
                }
            };
            if store.writes_stopped() {
                owner.close();
            }
            let (mut application, mut candidate) = match owner.continue_http(
                &id,
                pending,
                known_reset,
                signal.as_ref(),
                body.qualified,
            ) {
                Some((application, proof)) => (Some(application), Some(proof)),
                None => (None, None),
            };
            let mut observed = None;
            match signal {
                Some(signal) => {
                    // Same native transition/cause as transition(), without a
                    // nested Store acquisition. Optional failure never skips it.
                    let cause = match application.as_mut() {
                        Some(application) => {
                            crate::accounts::MetadataCause::UsageReport(application)
                        }
                        None => crate::accounts::MetadataCause::Mutation(
                            if matches!(
                                &signal,
                                HealthSignal::Cooling {
                                    reason: HealthReason::Quota,
                                    ..
                                }
                            ) {
                                crate::accounts::MetadataMutation::QuotaRejected
                            } else {
                                crate::accounts::MetadataMutation::Ordinary
                            },
                        ),
                    };
                    let mut ran = false;
                    let mut classified = None;
                    store
                        .update_metadata_with_cause_unlocked(&id, cause, |account| {
                            ran = true;
                            let before = candidate.as_ref().map(|_| account.clone());
                            use crate::accounts::HealthSignal as Signal;
                            let signal = match signal {
                                HealthSignal::Cooling {
                                    reason,
                                    until,
                                    scope,
                                } => Signal::Cooling {
                                    reason,
                                    until,
                                    scope,
                                },
                                HealthSignal::Revoked(reason) => Signal::Revoked { reason },
                                HealthSignal::AuthFailure => Signal::AuthAfterRefresh,
                                HealthSignal::Ready => Signal::Ready,
                            };
                            let applied_at = Utc::now();
                            let replay_signal = candidate.as_ref().and_then(|_| match &signal {
                                Signal::Cooling {
                                    reason: HealthReason::Quota,
                                    ..
                                } => Some(signal.clone()),
                                _ => None,
                            });
                            account.apply_health_signal(signal, applied_at);
                            // First retain the original no-op rule. Only the
                            // strict body witness may compose actual hardening.
                            classified = candidate.take().and_then(|proof| {
                                let before = before.as_ref()?;
                                if before == account && proof.matches_current(account, applied_at) {
                                    Some(proof)
                                } else {
                                    proof.compose_codex_body(
                                        before,
                                        account,
                                        replay_signal.as_ref()?,
                                        body.known.as_ref()?,
                                        known_reset,
                                        applied_at,
                                    )
                                }
                            });
                        })
                        .map_err(|_| ())?;
                    if ran
                        && !store.writes_stopped()
                        && let Some(application) = application
                    {
                        observed = application.finish_http(classified);
                    }
                }
                None => {
                    if let Some(mut application) = application.take() {
                        // Reconciliation already applied C5. Still make the
                        // final quota cut; do not fabricate a metadata write.
                        application.before_store_mutation(&id);
                        let current_matches = candidate.as_ref().is_some_and(|proof| {
                            store.read_account_unlocked(&id).is_ok_and(|account| {
                                account.id == id && proof.matches_current(&account, Utc::now())
                            })
                        });
                        if !store.writes_stopped() && current_matches {
                            observed = application.finish_http(candidate);
                        } else {
                            // Missing/malformed/stale metadata declines only
                            // proof. No native signal means no new data error.
                            owner.invalidate(&id);
                        }
                    }
                }
            }
            Ok::<_, ()>(observed)
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                self.wait_owner.close();
                if has_signal { Err(()) } else { Ok(None) }
            }
        }
    }

    pub(crate) async fn capture_http_origin(&self, id: &str) -> Option<ReportOrigin> {
        self.capture_report_origin(id, ReportSource::HttpAttempt)
            .await
    }

    pub(crate) async fn capture_poll_origin(&self, id: &str) -> Option<ReportOrigin> {
        self.capture_report_origin(id, ReportSource::UsagePoll)
            .await
    }

    /// Called only before a physical send, before the final native admission.
    async fn capture_report_origin(&self, id: &str, source: ReportSource) -> Option<ReportOrigin> {
        if id.is_empty() || id.len() > 128 {
            return None;
        }
        let id = id.to_owned();
        let store = self.store.clone();
        let owner = self.wait_owner.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = store.transaction().ok()?;
            if store.writes_stopped() {
                return None;
            }
            let current = store.read_account_unlocked(&id).ok()?;
            if current.id != id {
                return None;
            }
            owner.capture(&id, source)
        })
        .await
        .ok()
        .flatten()
    }

    /// A receipt with no usable report still consumes its pre-send watermark.
    /// No synthetic raw report, receipt-time origin, or proof is constructed.
    pub(crate) async fn discard_report_origin(&self, id: &str, origin: Option<ReportOrigin>) {
        let Some(origin) = origin else { return };
        if id.is_empty() || id.len() > 128 {
            return;
        }
        let id = id.to_owned();
        let store = self.store.clone();
        let owner = self.wait_owner.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let Ok(_gate) = store.transaction() else {
                owner.close();
                return;
            };
            if store.writes_stopped() {
                owner.close();
                return;
            }
            // A stale/unproven receipt may conservatively revoke newer proof,
            // just like ordinary native apply; it can never mint or recapture.
            if owner.accept(&id, origin).is_none() {
                owner.invalidate(&id);
            }
        })
        .await;
    }

    /// Every actual quota rejection must call this, including an unknown reset
    /// with no native Store write. Registry failure never vetoes the native C5.
    pub(crate) async fn invalidate_quota_rejection(&self, id: &str) {
        if id.is_empty() || id.len() > 128 {
            return;
        }
        let id = id.to_owned();
        let store = self.store.clone();
        let owner = self.wait_owner.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let Ok(_gate) = store.transaction() else {
                owner.close();
                return;
            };
            owner.invalidate(&id);
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Health, HealthReason, QuotaCooling};
    use serde_json::json;

    struct Fixture {
        runtime: Arc<AccountRuntime>,
        id: String,
        home: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_account(crate::accounts::health::tests::fixture())
        }

        fn with_account(account: Account) -> Self {
            let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.sandbox/r13-part-c/owner-fixtures");
            std::fs::create_dir_all(&base).unwrap();
            let home = tempfile::tempdir_in(base).unwrap();
            let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
            {
                let _gate = store.transaction().unwrap();
                Self::write(home.path(), &account);
            }
            let obs =
                Arc::new(crate::observability::Observability::new(home.path(), None).unwrap());
            let runtime = AccountRuntime::new(store, obs, 0.0);
            Self {
                runtime,
                id: account.id,
                home,
            }
        }

        // Synthetic opaque ciphertext is never decrypted by metadata operations.
        fn write(home: &std::path::Path, account: &Account) {
            let mut record = serde_json::to_value(account).unwrap();
            record["secret_dpapi_b64"] = json!("synthetic-unused-ciphertext");
            std::fs::write(
                home.join("accounts").join(format!("{}.json", account.id)),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
        }

        fn raw() -> Value {
            let at = Utc::now() - Duration::seconds(10);
            json!({"observed_at":at,"windows":[
                {"name":"5h","used_pct":100.0,"observed_at":at,"reset_at":at+Duration::minutes(10)},
                {"name":"7d","used_pct":20.0,"observed_at":at,"reset_at":at+Duration::days(1)}
            ]})
        }

        async fn apply(&self, raw: Value, origin: Option<ReportOrigin>) -> Account {
            self.runtime
                .apply_quota_snapshot_reported_origin(&self.id, raw.clone(), &raw, None, origin)
                .await
                .unwrap()
        }

        fn proof(&self) -> Option<DateTime<Utc>> {
            let _gate = self.runtime.store.transaction().unwrap();
            self.runtime
                .wait_owner
                .proofs
                .acquire(&self.id)
                .ok()
                .map(|(_, proof)| proof.reset_at)
        }

        fn ticket(&self) -> super::super::wait_proof::WaitTicket {
            let _gate = self.runtime.store.transaction().unwrap();
            self.runtime.wait_owner.proofs.acquire(&self.id).unwrap().0
        }

        fn admits(&self, ticket: &super::super::wait_proof::WaitTicket) -> bool {
            let _gate = self.runtime.store.transaction().unwrap();
            self.runtime.wait_owner.proofs.admit(ticket).is_ok()
        }

        async fn proven(&self) -> Account {
            let origin = self.runtime.capture_http_origin(&self.id).await.unwrap();
            let account = self.apply(Self::raw(), Some(origin)).await;
            assert!(
                self.proof().is_some(),
                "actual introducing reducer must mint"
            );
            account
        }

        fn headers(
            provider: crate::accounts::Provider,
            reset: DateTime<Utc>,
        ) -> axum::http::HeaderMap {
            let mut headers = axum::http::HeaderMap::new();
            let weekly = reset + Duration::days(1);
            let values = match provider {
                crate::accounts::Provider::Codex => vec![
                    ("x-codex-primary-used-percent", "100".to_owned()),
                    ("x-codex-primary-window-minutes", "300".to_owned()),
                    ("x-codex-primary-reset-at", reset.timestamp().to_string()),
                    ("x-codex-secondary-used-percent", "20".to_owned()),
                    ("x-codex-secondary-window-minutes", "10080".to_owned()),
                    ("x-codex-secondary-reset-at", weekly.timestamp().to_string()),
                ],
                crate::accounts::Provider::Claude => vec![
                    ("anthropic-ratelimit-unified-5h-utilization", "1".to_owned()),
                    (
                        "anthropic-ratelimit-unified-5h-status",
                        "rejected".to_owned(),
                    ),
                    (
                        "anthropic-ratelimit-unified-5h-reset",
                        reset.timestamp().to_string(),
                    ),
                    (
                        "anthropic-ratelimit-unified-7d-utilization",
                        "0.2".to_owned(),
                    ),
                    (
                        "anthropic-ratelimit-unified-7d-status",
                        "allowed".to_owned(),
                    ),
                    (
                        "anthropic-ratelimit-unified-7d-reset",
                        weekly.timestamp().to_string(),
                    ),
                ],
            };
            for (name, value) in values {
                headers.insert(name, value.parse().unwrap());
            }
            headers
        }

        async fn observe_pending(
            &self,
            headers: &axum::http::HeaderMap,
        ) -> (super::super::QuotaObservation, Option<PendingHttp429>) {
            let provider = self.runtime.store.load(&self.id).unwrap().provider;
            let origin = self.runtime.capture_http_origin(&self.id).await;
            let bindings = crate::bindings::BindingTable::new(
                self.home.path(),
                crate::config::RoutingConfig::default(),
            )
            .unwrap();
            let asof = bindings.begin_usage_observation_for(&self.id);
            self.runtime
                .observe_http_429_headers_pending(
                    &self.id,
                    provider,
                    headers,
                    None,
                    (&bindings, asof),
                    origin,
                )
                .await
        }

        async fn pending(&self) -> (PendingHttp429, DateTime<Utc>) {
            let reset =
                DateTime::from_timestamp((Utc::now() + Duration::minutes(10)).timestamp(), 0)
                    .unwrap();
            let provider = self.runtime.store.load(&self.id).unwrap().provider;
            let (observation, pending) =
                self.observe_pending(&Self::headers(provider, reset)).await;
            assert_eq!(observation.quota_until, Some(reset));
            assert!(self.proof().is_none(), "429 headers must never mint early");
            (
                pending.expect("successful qualifying header application must seal"),
                reset,
            )
        }

        fn quota_signal(reset: DateTime<Utc>) -> HealthSignal {
            HealthSignal::Cooling {
                reason: HealthReason::Quota,
                until: reset,
                scope: "account".to_owned(),
            }
        }
    }

    fn qualified_body(reset: DateTime<Utc>, model: Option<&str>) -> QualifiedHttp429Body {
        AccountRuntime::qualify_http_429_body(
            Some(
                serde_json::to_vec(&json!({"error": {
                    "type":"usage_limit_reached", "resets_at":reset.timestamp(),
                    "message":"synthetic bounded rejection",
                }}))
                .unwrap()
                .as_slice(),
            ),
            model,
        )
    }

    struct Composition {
        header: ShortQuotaProof,
        before: Account,
        body: KnownQuotaBody,
        signal: crate::accounts::HealthSignal,
        at: DateTime<Utc>,
    }

    impl Composition {
        fn new() -> Self {
            let header_at = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
            let observed = header_at - Duration::seconds(10);
            let reset = header_at + Duration::minutes(10);
            let initial = crate::accounts::health::tests::fixture();
            let raw = json!({"observed_at":observed,"windows":[
                {"name":"5h","used_pct":100.,"observed_at":observed,"reset_at":reset},
                {"name":"7d","used_pct":20.,"observed_at":observed,"reset_at":reset+Duration::days(1)},
            ]});
            let mut before = initial.clone();
            before.apply_health_signal(
                crate::accounts::HealthSignal::UsageSnapshot {
                    snapshot: raw.clone(),
                    model: None,
                    projected_allowance: 0.,
                },
                header_at,
            );
            let header = classify(&initial, &before, &raw, None, header_at).unwrap();
            assert!(before.quota_credit_releasable);
            Self {
                header,
                before,
                body: qualified_body(reset, None).known.unwrap(),
                signal: crate::accounts::HealthSignal::Cooling {
                    reason: HealthReason::Quota,
                    until: reset,
                    scope: "account".into(),
                },
                at: header_at + Duration::seconds(2),
            }
        }
    }

    #[test]
    fn n5a_body_compose_known_envelope_rejects_duplicates_and_competing_claims() {
        for claim in ["type", "code"] {
            let body = AccountRuntime::qualify_http_429_body(
                Some(format!(r#"{{"status":429,"error":{{"{claim}":"usage_limit_reached","resets_at":2000000600}}}}"#).as_bytes()), None,
            );
            assert!(body.qualified && body.known.is_some());
        }
        for raw in [
            r#"{}"#,
            r#"{"error":{"type":"rate_limit_error","resets_at":2000000600}}"#,
            r#"{"error":{"type":"usage_limit_reached","code":"token_revoked","resets_at":2000000600}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600,"resets_at":2000000600}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600,"type":"usage_limit_reached"}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600},"error":{"type":"usage_limit_reached","resets_at":2000000600}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600,"window":"7d"}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600,"serving_from_credits":true}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600,"scope":"model:other"}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600,"model":"other"}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600,"resets_in_seconds":60}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600},"windows":[{"name":"7d","used_pct":100}]}"#,
            r#"{"status":402,"error":{"type":"usage_limit_reached","resets_at":2000000600}}"#,
            r#"{"error":{"type":"usage_limit_reached","resets_at":2000000600.5}}"#,
            r#"{"error":{"type":"usage_limit_reached","message":{"unknown":"shape"},"resets_at":2000000600}}"#,
        ] {
            let body = AccountRuntime::qualify_http_429_body(Some(raw.as_bytes()), None);
            assert!(
                body.qualified,
                "preserve baseline complete-object classification"
            );
            assert!(body.known.is_none(), "no hardening witness for {raw}");
        }
    }

    #[test]
    fn n5a_body_compose_requires_complete_bounded_decoded_body_and_bounded_model() {
        assert!(!AccountRuntime::qualify_http_429_body(None, None).qualified);
        for raw in [b" ".as_slice(), b"null", b"[]", b"{malformed"] {
            assert!(!AccountRuntime::qualify_http_429_body(Some(raw), None).qualified);
        }
        let empty = AccountRuntime::qualify_http_429_body(Some(b""), None);
        assert!(empty.qualified && empty.known.is_none());
        assert!(
            !AccountRuntime::qualify_http_429_body(Some(&vec![b' '; MAX_BODY_BYTES + 1]), None)
                .qualified
        );
        let reset = DateTime::from_timestamp(2_000_000_600, 0).unwrap();
        assert!(
            qualified_body(reset, Some(&"m".repeat(MAX_MODEL_BYTES + 1)))
                .known
                .is_none()
        );
        assert_eq!(
            qualified_body(reset, Some("synthetic-model"))
                .known
                .unwrap()
                .model
                .as_deref(),
            Some("synthetic-model")
        );
    }

    #[test]
    fn n5a_body_compose_full_native_replay_preserves_original_attestation() {
        let c = Composition::new();
        let observed = c.header.observed_at;
        let started = c.header.started_at;
        let earliest = c.header.earliest_reset_at;
        let reset = c.header.reset_at;
        let mut actual_after = c.before.clone();
        actual_after.apply_health_signal(c.signal.clone(), c.at);
        assert_ne!(actual_after, c.before);
        assert_eq!(
            actual_after.health, c.before.health,
            "same tuple alone is insufficient"
        );
        let proof = c
            .header
            .compose_codex_body(
                &c.before,
                &actual_after,
                &c.signal,
                &c.body,
                Some(reset),
                c.at,
            )
            .unwrap();
        assert!(proof.matches_current(&actual_after, c.at));
        assert!(!proof.quota_credit_releasable);
        assert_eq!(
            (proof.observed_at, proof.started_at, proof.earliest_reset_at),
            (observed, started, earliest)
        );
        assert!(
            proof.observed_at < proof.started_at,
            "preserve the causal first-report timestamp gap"
        );
    }

    #[test]
    fn n5a_body_compose_rejects_changed_output_expiry_and_disagreeing_typed_inputs() {
        for case in 0..10 {
            let mut c = Composition::new();
            let mut known = Some(c.header.reset_at);
            match case {
                0 => c.before.payment_until = Some(c.at), // expires at this exact sample
                1 => c.before.allow_credits = !c.before.allow_credits,
                2 => c.body.reset_at += Duration::seconds(1),
                3 => c.body.model = Some("synthetic-other".into()),
                4 => known = None,
                5 => {
                    if let crate::accounts::HealthSignal::Cooling { scope, .. } = &mut c.signal {
                        *scope = "model:other".into();
                    }
                }
                6 => c.signal = crate::accounts::HealthSignal::Ready,
                _ => (),
            }
            let mut after = c.before.clone();
            after.apply_health_signal(c.signal.clone(), c.at);
            match case {
                7 => after.drain = true,
                8 => after.generation += 1,
                9 => after.quota_cooling.as_mut().unwrap().started_at += Duration::seconds(1),
                _ => (),
            }
            assert!(
                c.header
                    .compose_codex_body(&c.before, &after, &c.signal, &c.body, known, c.at)
                    .is_none(),
                "case {case}"
            );
        }
    }

    #[tokio::test]
    async fn n5a_body_compose_actual_store_hardens_then_mints_and_cuts_interposed_origin() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let observed = pending.proof.observed_at;
        let before = f.runtime.store.load(&f.id).unwrap();
        let interposed = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        f.runtime
            .finish_http_429_report_with_body(
                &f.id,
                Some(pending),
                Some(reset),
                Some(Fixture::quota_signal(reset)),
                qualified_body(reset, None),
            )
            .await
            .unwrap();
        let after = f.runtime.store.load(&f.id).unwrap();
        assert!(before.quota_credit_releasable && !after.quota_credit_releasable);
        assert_eq!(after.generation, before.generation + 1);
        assert_eq!(after.quota_cooling, before.quota_cooling);
        assert_eq!(f.proof(), Some(reset));
        let _gate = f.runtime.store.transaction().unwrap();
        let (_, proof) = f.runtime.wait_owner.proofs.acquire(&f.id).unwrap();
        assert_eq!(proof.observed_at, observed);
        assert!(proof.matches_current(&after, Utc::now()));
        assert!(f.runtime.wait_owner.accept(&f.id, interposed).is_none());
    }

    #[tokio::test]
    async fn n5a_body_compose_unsupported_or_conflicting_body_preserves_native_result_unproven() {
        for case in 0..5 {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            let before = f.runtime.store.load(&f.id).unwrap();
            let mut value =
                json!({"error":{"type":"usage_limit_reached","resets_at":reset.timestamp()}});
            match case {
                0 => value["error"]["window"] = json!("7d"),
                1 => value["error"]["serving_from_credits"] = json!(true),
                2 => value["error"]["resets_in_seconds"] = json!(1),
                3 => value["error"]["resets_at"] = json!(reset.timestamp() + 1),
                _ => (),
            }
            let bytes = serde_json::to_vec(&value).unwrap();
            let body = AccountRuntime::qualify_http_429_body(
                Some(&bytes),
                (case == 4).then_some("synthetic-other"),
            );
            f.runtime
                .finish_http_429_report_with_body(
                    &f.id,
                    Some(pending),
                    Some(reset),
                    Some(Fixture::quota_signal(reset)),
                    body,
                )
                .await
                .unwrap();
            let mut expected = before;
            expected.apply_health_signal(
                crate::accounts::HealthSignal::Cooling {
                    reason: HealthReason::Quota,
                    until: reset,
                    scope: "account".into(),
                },
                Utc::now(),
            );
            expected.generation += 1;
            assert_eq!(f.runtime.store.load(&f.id).unwrap(), expected);
            assert!(f.proof().is_none(), "case {case}");
        }
    }

    #[tokio::test]
    async fn n5a_body_compose_newer_partial_release_stop_and_failed_persistence_prevent_mint() {
        for case in 0..4 {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            match case {
                0 => {
                    let newer = f.runtime.capture_poll_origin(&f.id).await.unwrap();
                    let mut partial = f.runtime.obs.quota_snapshot(&f.id).unwrap();
                    partial["windows"] = json!([partial["windows"][1].clone()]);
                    f.apply(partial, Some(newer)).await;
                }
                1 => {
                    f.runtime.release_quota_hold(&f.id).await.unwrap();
                }
                2 => f.runtime.store.stop_writes(),
                _ => {
                    let mut current = f.runtime.store.load(&f.id).unwrap();
                    current.generation = u64::MAX;
                    let _gate = f.runtime.store.transaction().unwrap();
                    Fixture::write(f.home.path(), &current);
                }
            }
            let before_body = f.runtime.store.load(&f.id).unwrap();
            let result = f
                .runtime
                .finish_http_429_report_with_body(
                    &f.id,
                    Some(pending),
                    Some(reset),
                    Some(Fixture::quota_signal(reset)),
                    qualified_body(reset, None),
                )
                .await;
            assert_eq!(
                result.is_err(),
                case == 3,
                "native write failure remains authoritative"
            );
            assert!(f.proof().is_none(), "case {case}");
            let after = f.runtime.store.load(&f.id).unwrap();
            if case >= 2 {
                assert_eq!(after, before_body);
            } else {
                assert!(!after.quota_credit_releasable);
                assert!(matches!(
                    after.health,
                    Health::Cooling {
                        reason: HealthReason::Quota,
                        ..
                    }
                ));
            }
        }
    }

    #[tokio::test]
    async fn n5a_body_compose_known_body_alone_cannot_supply_literal_header_authority() {
        let f = Fixture::new();
        let reset = Utc::now() + Duration::minutes(10);
        f.runtime
            .finish_http_429_report_with_body(
                &f.id,
                None,
                Some(reset),
                Some(Fixture::quota_signal(reset)),
                qualified_body(reset, None),
            )
            .await
            .unwrap();
        let account = f.runtime.store.load(&f.id).unwrap();
        assert!(matches!(
            account.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        assert!(account.quota_cooling.is_none());
        assert!(f.proof().is_none());
    }

    #[tokio::test]
    async fn n5a_body_compose_empty_qualified_completion_keeps_no_signal_behavior() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let before = f.runtime.store.load(&f.id).unwrap();
        f.runtime
            .finish_http_429_report_with_body(
                &f.id,
                Some(pending),
                Some(reset),
                None,
                AccountRuntime::qualify_http_429_body(Some(b""), None),
            )
            .await
            .unwrap();
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), before);
        assert_eq!(f.proof(), Some(reset));
    }

    #[tokio::test]
    async fn n5a_http_owner_headers_apply_immediately_final_no_signal_mints_and_cuts_interposed_origin()
     {
        let f = Fixture::new();
        let mut events = f.runtime.subscribe();
        let (pending, reset) = f.pending().await;
        let before = f.runtime.store.load(&f.id).unwrap();
        assert!(
            matches!(&before.health, Health::Cooling { reason: HealthReason::Quota, until, .. } if *until == reset)
        );
        assert_eq!(before.generation, 1);
        assert!(f.runtime.obs.quota_snapshot(&f.id).is_some());
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| event["event"] == "quota_updated")
        );
        let interposed = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        f.runtime
            .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
            .await
            .unwrap();
        assert_eq!(f.proof(), Some(reset));
        assert_eq!(
            f.runtime.store.load(&f.id).unwrap(),
            before,
            "no invented metadata update"
        );
        let _gate = f.runtime.store.transaction().unwrap();
        assert!(
            f.runtime.wait_owner.accept(&f.id, interposed).is_none(),
            "no-signal finish must make the final quota cut"
        );
    }

    #[tokio::test]
    async fn n5a_http_owner_newer_partial_prevents_older_body_mint() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let newer = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let mut partial = f.runtime.obs.quota_snapshot(&f.id).unwrap();
        partial["windows"] = json!([partial["windows"][1].clone()]);
        f.apply(partial, Some(newer)).await;
        f.runtime
            .finish_http_429_report(
                &f.id,
                Some(pending),
                Some(reset),
                Some(Fixture::quota_signal(reset)),
                true,
            )
            .await
            .unwrap();
        assert!(f.proof().is_none());
        assert!(matches!(
            f.runtime.store.load(&f.id).unwrap().health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn n5a_http_owner_newer_empty_headers_prevent_older_body_mint() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let (observation, newer) = f.observe_pending(&axum::http::HeaderMap::new()).await;
        assert!(observation.quota_until.is_none());
        assert!(newer.is_none());
        f.runtime
            .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
            .await
            .unwrap();
        assert!(f.proof().is_none());
    }

    #[tokio::test]
    async fn n5a_http_owner_release_and_failed_release_write_prevent_mint() {
        for fail_write in [false, true] {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            if fail_write {
                let mut account = f.runtime.store.load(&f.id).unwrap();
                account.generation = u64::MAX;
                let _gate = f.runtime.store.transaction().unwrap();
                Fixture::write(f.home.path(), &account);
            }
            assert_eq!(
                f.runtime.release_quota_hold(&f.id).await.is_err(),
                fail_write
            );
            let after_release = f.runtime.store.load(&f.id).unwrap();
            f.runtime
                .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
                .await
                .unwrap();
            assert!(f.proof().is_none());
            assert_eq!(f.runtime.store.load(&f.id).unwrap(), after_release);
        }
    }

    #[tokio::test]
    async fn n5a_http_owner_stop_prevents_mint_and_preserves_stopped_native_ok() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let before = f.runtime.store.load(&f.id).unwrap();
        f.runtime.store.stop_writes();
        f.runtime
            .finish_http_429_report(
                &f.id,
                Some(pending),
                Some(reset),
                Some(Fixture::quota_signal(reset + Duration::minutes(1))),
                true,
            )
            .await
            .unwrap();
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), before);
    }

    #[tokio::test]
    async fn n5a_http_owner_unknown_same_health_rejection_revokes_without_write() {
        let f = Fixture::new();
        let before = f.proven().await;
        let ticket = f.ticket();
        f.runtime
            .finish_http_429_report(&f.id, None, None, None, false)
            .await
            .unwrap();
        assert!(!f.admits(&ticket));
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), before);
    }

    #[tokio::test]
    async fn n5a_http_owner_matching_known_codex_body_changes_native_account_and_declines() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let before = f.runtime.store.load(&f.id).unwrap();
        assert!(
            before.quota_credit_releasable,
            "real numeric header reducer precondition"
        );
        let mut expected = before.clone();
        expected.apply_health_signal(
            crate::accounts::HealthSignal::Cooling {
                reason: HealthReason::Quota,
                until: reset,
                scope: "account".into(),
            },
            Utc::now(),
        );
        assert_ne!(
            before, expected,
            "same health tuple is not whole Account equality"
        );
        f.runtime
            .finish_http_429_report(
                &f.id,
                Some(pending),
                Some(reset),
                Some(Fixture::quota_signal(reset)),
                true,
            )
            .await
            .unwrap();
        expected.generation += 1;
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), expected);
        assert!(!expected.quota_credit_releasable);
        assert!(
            f.proof().is_none(),
            "preserve C5; do not weaken full Account no-op to a health tuple"
        );
    }

    #[tokio::test]
    async fn n5a_http_owner_unchanged_claude_body_can_mint_after_native_noop() {
        let mut account = crate::accounts::health::tests::fixture();
        account.identity = crate::accounts::Identity::Claude {
            account_uuid: "00000000-0000-4000-8000-000000000065".into(),
            organization_uuid: None,
            email: "synthetic@example.invalid".into(),
        };
        account.id = account.identity.id();
        account.provider = crate::accounts::Provider::Claude;
        let f = Fixture::with_account(account);
        let (pending, reset) = f.pending().await;
        let before = f.runtime.store.load(&f.id).unwrap();
        assert!(!before.quota_credit_releasable);
        f.runtime
            .finish_http_429_report(
                &f.id,
                Some(pending),
                Some(reset),
                Some(Fixture::quota_signal(reset)),
                true,
            )
            .await
            .unwrap();
        assert_eq!(f.proof(), Some(reset));
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), before);
    }

    #[tokio::test]
    async fn n5a_http_owner_reset_scope_transport_and_nonquota_conflicts_decline_but_apply_native()
    {
        for case in 0..5 {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            let (known, signal, qualified) = match case {
                0 => (Some(reset + Duration::seconds(1)), None, true),
                1 => (None, None, true),
                2 => (Some(reset), None, false),
                3 => (
                    Some(reset),
                    Some(HealthSignal::Cooling {
                        reason: HealthReason::Quota,
                        until: reset,
                        scope: "model:synthetic-other".into(),
                    }),
                    true,
                ),
                _ => (Some(reset), Some(HealthSignal::Ready), true),
            };
            let before = f.runtime.store.load(&f.id).unwrap();
            let mut expected = before.clone();
            if case == 3 {
                expected.apply_health_signal(
                    crate::accounts::HealthSignal::Cooling {
                        reason: HealthReason::Quota,
                        until: reset,
                        scope: "model:synthetic-other".into(),
                    },
                    Utc::now(),
                );
            } else if case == 4 {
                expected.apply_health_signal(crate::accounts::HealthSignal::Ready, Utc::now());
            }
            if expected != before {
                expected.generation += 1;
            }
            f.runtime
                .finish_http_429_report(&f.id, Some(pending), known, signal, qualified)
                .await
                .unwrap();
            assert!(f.proof().is_none(), "case {case}");
            assert_eq!(
                f.runtime.store.load(&f.id).unwrap(),
                expected,
                "case {case} native behavior"
            );
        }
    }

    #[tokio::test]
    async fn n5a_http_owner_header_failure_is_swallowed_without_pending_or_early_proof() {
        let mut account = crate::accounts::health::tests::fixture();
        account.generation = u64::MAX;
        let f = Fixture::with_account(account.clone());
        let reset = Utc::now() + Duration::minutes(10);
        let (observation, pending) = f
            .observe_pending(&Fixture::headers(account.provider, reset))
            .await;
        assert!(observation.quota_until.is_none());
        assert!(pending.is_none());
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), account);
    }

    #[tokio::test]
    async fn n5a_http_owner_body_persistence_failure_propagates_without_mint() {
        let f = Fixture::new();
        let (pending, reset) = f.pending().await;
        let mut account = f.runtime.store.load(&f.id).unwrap();
        account.generation = u64::MAX;
        {
            let _gate = f.runtime.store.transaction().unwrap();
            Fixture::write(f.home.path(), &account);
        }
        assert!(
            f.runtime
                .finish_http_429_report(
                    &f.id,
                    Some(pending),
                    Some(reset),
                    Some(Fixture::quota_signal(reset)),
                    true
                )
                .await
                .is_err()
        );
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), account);
    }

    #[tokio::test]
    async fn n5a_http_owner_no_signal_missing_or_malformed_metadata_declines_without_native_error()
    {
        for missing in [false, true] {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            let path = f
                .home
                .path()
                .join("accounts")
                .join(format!("{}.json", f.id));
            {
                let _gate = f.runtime.store.transaction().unwrap();
                if missing {
                    std::fs::remove_file(&path).unwrap();
                } else {
                    std::fs::write(&path, b"{malformed synthetic metadata").unwrap();
                }
            }
            f.runtime
                .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
                .await
                .unwrap();
            assert!(f.proof().is_none());
            assert_eq!(path.exists(), !missing, "no metadata recreation/write");
            if !missing {
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    b"{malformed synthetic metadata"
                );
            }
        }
    }

    #[tokio::test]
    async fn n5a_http_owner_no_signal_current_metadata_applicability_is_rechecked() {
        for case in 0..7 {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            let mut changed = f.runtime.store.load(&f.id).unwrap();
            match case {
                0 => changed.drain = true,
                1 => changed.user_state = crate::accounts::UserState::Paused,
                2 => changed.auth_failures = 1,
                3 => changed.payment_until = Some(reset),
                4 => changed.allow_credits = !changed.allow_credits,
                5 => changed.quota_cooling.as_mut().unwrap().window = "7d".into(),
                _ => {
                    if let Health::Cooling { scope, .. } = &mut changed.health {
                        *scope = "model:synthetic-other".into();
                    }
                }
            }
            // Simulate current metadata disagreement without relying on the
            // prehook. The optional no-signal check must independently refuse.
            {
                let _gate = f.runtime.store.transaction().unwrap();
                Fixture::write(f.home.path(), &changed);
            }
            f.runtime
                .finish_http_429_report(&f.id, Some(pending), Some(reset), None, true)
                .await
                .unwrap();
            assert!(f.proof().is_none(), "case {case}");
            assert_eq!(
                f.runtime.store.load(&f.id).unwrap(),
                changed,
                "case {case} no native mutation"
            );
        }
    }

    #[tokio::test]
    async fn n5a_http_owner_store_gate_failure_preserves_signal_dependent_native_precedence() {
        for has_signal in [false, true] {
            let f = Fixture::new();
            let (pending, reset) = f.pending().await;
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _gate = f.runtime.store.transaction().unwrap();
                    panic!("synthetic Store gate poison");
                }))
                .is_err()
            );
            let result = f
                .runtime
                .finish_http_429_report(
                    &f.id,
                    Some(pending),
                    Some(reset),
                    has_signal.then(|| Fixture::quota_signal(reset)),
                    true,
                )
                .await;
            assert_eq!(
                result.is_err(),
                has_signal,
                "no signal has no native body write failure"
            );
            assert!(matches!(
                f.runtime.wait_owner.proofs.acquire(&f.id),
                Err(super::super::wait_proof::FenceError::Closed)
            ));
        }
    }

    #[test]
    fn n5a_http_owner_compact_candidate_preserves_raw_age_future_and_every_window_reset() {
        let at = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
        let mut proof = ShortQuotaProof {
            reset_at: at + Duration::minutes(10),
            model: None,
            observed_at: at,
            earliest_reset_at: at + Duration::minutes(20),
            started_at: at,
            scope: "account".into(),
            allow_credits: false,
            quota_credit_releasable: false,
        };
        assert!(proof.valid_at(at + Duration::seconds(300)));
        assert!(!proof.valid_at(at + Duration::seconds(300) + Duration::nanoseconds(1)));
        assert!(!proof.valid_at(at - Duration::nanoseconds(1)));
        proof.started_at = at + Duration::seconds(10);
        assert!(!proof.valid_at(proof.started_at - Duration::nanoseconds(1)));
        assert!(proof.valid_at(proof.started_at));
        proof.started_at = at;
        proof.earliest_reset_at = at + Duration::seconds(30);
        assert!(proof.valid_at(at + Duration::seconds(29)));
        assert!(!proof.valid_at(at + Duration::seconds(30)));
        assert!(
            proof.reset_at > at + Duration::seconds(30),
            "nonlimiting window may expire first"
        );
    }

    #[tokio::test]
    async fn n5a_owner_introducing_report_uses_real_delayed_reducer_without_backdating() {
        let f = Fixture::new();
        let raw = Fixture::raw();
        let observed: DateTime<Utc> = raw["observed_at"].as_str().unwrap().parse().unwrap();
        let origin = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let held = f.apply(raw, Some(origin)).await;
        let evidence = held.quota_cooling.unwrap();
        assert!(observed < evidence.started_at);
        assert_eq!(f.proof(), Some(evidence.reset_at));
        let persisted = f.runtime.store.load(&f.id).unwrap();
        assert_eq!(persisted.generation, 1);
        assert_eq!(
            persisted.quota_cooling.unwrap().started_at,
            evidence.started_at
        );
    }

    #[tokio::test]
    async fn n5a_owner_newer_partial_blocks_older_complete_through_actual_store() {
        let f = Fixture::new();
        let full = Fixture::raw();
        let a = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let b = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let mut partial = full.clone();
        partial["windows"] = json!([full["windows"][1].clone()]);
        f.apply(partial, Some(b)).await;
        assert!(f.proof().is_none());
        let held = f.apply(full, Some(a)).await;
        assert!(matches!(
            held.health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        assert!(
            f.proof().is_none(),
            "older report still applies native C5 but cannot mint"
        );
    }

    #[tokio::test]
    async fn n5a_owner_empty_http_receipt_consumes_origin_before_older_complete() {
        let f = Fixture::new();
        let a = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let b = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let bindings = crate::bindings::BindingTable::new(
            f.home.path(),
            crate::config::RoutingConfig::default(),
        )
        .unwrap();
        let asof = bindings.begin_usage_observation_for(&f.id);
        f.runtime
            .observe_headers_asof_origin(
                &f.id,
                crate::accounts::Provider::Codex,
                &axum::http::HeaderMap::new(),
                None,
                (&bindings, asof),
                Some(b),
            )
            .await;
        let held = f.apply(Fixture::raw(), Some(a)).await;
        assert!(held.quota_cooling.is_some());
        assert!(
            f.proof().is_none(),
            "empty headers must advance accepted ordering"
        );
    }

    #[tokio::test]
    async fn n5a_owner_older_native_apply_conservatively_revokes_newer_proof() {
        let f = Fixture::new();
        let raw = Fixture::raw();
        let a = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let b = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let held = f.apply(raw.clone(), Some(b)).await;
        let ticket = f.ticket();
        let later = f.apply(raw, Some(a)).await;
        assert_eq!(later.health, held.health);
        assert!(!f.admits(&ticket));
        assert!(
            f.proof().is_none(),
            "ordinary stale apply is intentionally conservative"
        );
    }

    #[tokio::test]
    async fn n5a_owner_cached_merge_and_public_apply_cannot_borrow_raw_origin() {
        let f = Fixture::new();
        f.proven().await;
        let raw = Fixture::raw();
        let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let partial =
            json!({"observed_at":raw["observed_at"],"windows":[raw["windows"][0].clone()]});
        f.runtime
            .apply_quota_snapshot_reported_origin(&f.id, raw.clone(), &partial, None, Some(origin))
            .await
            .unwrap();
        assert!(f.proof().is_none(), "cache-complete is not raw-complete");
        f.runtime
            .apply_quota_snapshot(&f.id, raw, None)
            .await
            .unwrap();
        assert!(f.proof().is_none(), "public apply never fabricates origin");
    }

    #[tokio::test]
    async fn n5a_owner_failed_release_write_revokes_live_ticket_and_inflight_origin() {
        let f = Fixture::new();
        let mut account = f.proven().await;
        let ticket = f.ticket();
        let inflight = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        // Deterministic native persistence failure, after the real Release hook:
        // write_metadata_record cannot increment MAX generation. No ACL timing.
        account.generation = u64::MAX;
        {
            let _gate = f.runtime.store.transaction().unwrap();
            Fixture::write(f.home.path(), &account);
        }
        assert!(f.runtime.release_quota_hold(&f.id).await.is_err());
        assert!(!f.admits(&ticket));
        assert!(f.proof().is_none());
        assert!(matches!(
            f.runtime.store.load(&f.id).unwrap().health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        let raw = Fixture::raw();
        let _ = f
            .runtime
            .apply_quota_snapshot_reported_origin(&f.id, raw.clone(), &raw, None, Some(inflight))
            .await;
        assert!(f.proof().is_none());
    }

    #[tokio::test]
    async fn n5a_owner_stop_closes_capture_and_stopped_ok_cannot_mint() {
        let f = Fixture::new();
        f.proven().await;
        let ticket = f.ticket();
        let inflight = f.runtime.capture_http_origin(&f.id).await.unwrap();
        f.runtime.store.stop_writes();
        assert!(f.runtime.capture_http_origin(&f.id).await.is_none());
        assert!(f.runtime.capture_poll_origin(&f.id).await.is_none());
        let prior = f.runtime.store.load(&f.id).unwrap();
        let stopped_ok = f.apply(Fixture::raw(), Some(inflight)).await;
        assert_eq!(stopped_ok, prior);
        assert!(!f.admits(&ticket));
        assert!(f.proof().is_none());
        f.runtime.store.stop_writes();
        assert!(f.runtime.capture_http_origin(&f.id).await.is_none());
    }

    #[tokio::test]
    async fn n5a_owner_automatic_expiry_revokes_the_reports_own_rotation() {
        let f = Fixture::new();
        let mut account = crate::accounts::health::tests::fixture();
        let until = Utc::now() - Duration::seconds(1);
        account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until,
            scope: "account".into(),
        };
        account.quota_cooling = Some(QuotaCooling {
            window: "5h".into(),
            reset_at: until,
            started_at: until - Duration::minutes(5),
        });
        // Seed after constructor baseline (which normally persists expiry).
        // Capture reads current metadata without performing that publication.
        {
            let _gate = f.runtime.store.transaction().unwrap();
            Fixture::write(f.home.path(), &account);
        }
        let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let held = f.apply(Fixture::raw(), Some(origin)).await;
        assert!(held.quota_cooling.is_some());
        assert!(
            f.proof().is_none(),
            "independent actual expiry cannot inherit own-report permit"
        );
    }

    #[tokio::test]
    async fn n5a_owner_unknown_rejection_without_store_write_revokes() {
        let f = Fixture::new();
        let before = f.proven().await;
        let ticket = f.ticket();
        f.runtime.invalidate_quota_rejection(&f.id).await;
        assert!(!f.admits(&ticket));
        assert!(f.proof().is_none());
        assert_eq!(f.runtime.store.load(&f.id).unwrap(), before);
    }

    #[tokio::test]
    async fn n5a_owner_oversized_model_refuses_only_optional_proof() {
        let f = Fixture::new();
        let origin = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let raw = Fixture::raw();
        let model = "m".repeat(MAX_MODEL_BYTES + 1);
        let held = f
            .runtime
            .apply_quota_snapshot_reported_origin(
                &f.id,
                raw.clone(),
                &raw,
                Some(&model),
                Some(origin),
            )
            .await
            .unwrap();
        assert!(held.quota_cooling.is_some());
        assert!(f.proof().is_none());
    }
    fn notification_ready(notified: std::pin::Pin<&mut Notified<'_>>) -> bool {
        struct Noop;
        impl std::task::Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        let waker = std::task::Waker::from(Arc::new(Noop));
        let mut context = std::task::Context::from_waker(&waker);
        std::future::Future::poll(notified, &mut context).is_ready()
    }

    #[test]
    fn n5a_wait_notify_capture_quiet_accept_wakes_all_and_stale_failure_wakes() {
        let owner = Arc::new(WaitOwner::new());
        let first = owner.notified();
        let second = owner.notified();
        let mut first = std::pin::pin!(first);
        let mut second = std::pin::pin!(second);
        first.as_mut().enable();
        second.as_mut().enable();
        let origin = owner.capture("a", ReportSource::HttpAttempt).unwrap();
        let duplicate = origin.clone();
        assert!(!notification_ready(first.as_mut()));
        assert!(!notification_ready(second.as_mut()));
        drop(owner.accept("a", origin).unwrap()); // Partial still revokes.
        assert!(notification_ready(first.as_mut()));
        assert!(notification_ready(second.as_mut()));
        let failure = owner.notified();
        let mut failure = std::pin::pin!(failure);
        failure.as_mut().enable();
        assert!(owner.accept("a", duplicate).is_none());
        assert!(notification_ready(failure.as_mut()));
    }

    #[test]
    fn n5a_wait_notify_rotation_rejection_retire_invalidate_and_close_wake() {
        for operation in ["rotate", "rotate-failure", "retire", "invalidate", "close"] {
            let owner = Arc::new(WaitOwner::new());
            let origin = owner.capture("a", ReportSource::UsagePoll).unwrap();
            let mut application = owner.accept("a", origin).unwrap();
            if operation == "rotate-failure" {
                application.before_store_mutation("a");
            }
            let notified = owner.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            match operation {
                "rotate" | "rotate-failure" => application.before_store_mutation("a"),
                "retire" => owner.retire("missing"),
                "invalidate" => owner.invalidate("missing"),
                "close" => owner.close(),
                _ => unreachable!(),
            }
            assert!(notification_ready(notified.as_mut()), "{operation}");
        }
    }

    #[tokio::test]
    async fn n5a_wait_notify_real_failed_store_mutation_wakes_and_revokes() {
        let f = Fixture::new();
        let account = f.proven().await;
        let owner = &f.runtime.wait_owner;
        let ticket = {
            let _gate = f.runtime.store.transaction().unwrap();
            owner
                .acquire_current_unlocked(&account, None, Utc::now())
                .unwrap()
        };
        let notified = owner.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        let error = f
            .runtime
            .store
            .update_metadata(&f.id, |account| account.schema = 0);
        assert!(error.is_err());
        assert!(notification_ready(notified.as_mut()));
        let _gate = f.runtime.store.transaction().unwrap();
        let current = f.runtime.store.read_account_unlocked(&f.id).unwrap();
        assert_eq!(
            current, account,
            "failed mutation preserves native metadata"
        );
        assert!(!owner.ticket_current_unlocked(&ticket, &current, None, Utc::now()));
    }

    #[tokio::test]
    async fn n5a_wait_ticket_checks_exact_model_account_time_and_owner() {
        let f = Fixture::new();
        let account = f.proven().await;
        let owner = &f.runtime.wait_owner;
        let _gate = f.runtime.store.transaction().unwrap();
        let now = Utc::now();
        let ticket = owner.acquire_current_unlocked(&account, None, now).unwrap();
        assert!(ticket.reset_at() > now);
        assert!(owner.ticket_current_unlocked(&ticket, &account, None, now));
        let known = owner
            .acquire_current_unlocked(&account, Some("synthetic-model"), now)
            .unwrap();
        assert!(owner.ticket_current_unlocked(&known, &account, Some("synthetic-model"), now));
        assert!(!owner.ticket_current_unlocked(&known, &account, None, now));
        assert!(!owner.ticket_current_unlocked(&known, &account, Some("synthetic-other"), now));
        assert!(!owner.ticket_current_unlocked(&ticket, &account, Some("synthetic-model"), now));
        let mut wrong_account = account.clone();
        wrong_account.id = "cx-000000".into();
        assert!(!owner.ticket_current_unlocked(&ticket, &wrong_account, None, now));
        let mut paused = account.clone();
        paused.user_state = crate::accounts::UserState::Paused;
        assert!(!owner.ticket_current_unlocked(&ticket, &paused, None, now));
        assert!(!owner.ticket_current_unlocked(&ticket, &account, None, ticket.reset_at()));
        let foreign = WaitOwner::new();
        assert!(!foreign.ticket_current_unlocked(&ticket, &account, None, now));
        owner.invalidate(&f.id);
        assert!(!owner.ticket_current_unlocked(&ticket, &account, None, now));
    }

    #[tokio::test]
    async fn n5a_wait_model_scope_full_poll_preserves_proof_and_bounds_request_ticket() {
        let f = Fixture::new();
        let raw = Fixture::raw();
        let observed: DateTime<Utc> = raw["observed_at"].as_str().unwrap().parse().unwrap();
        let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let account = f.apply(raw, Some(origin)).await;
        let owner = &f.runtime.wait_owner;
        let _gate = f.runtime.store.transaction().unwrap();
        let now = Utc::now();
        // Byte bounds, including multibyte names, apply before retaining a new
        // request identity. The actual poll report has no request model.
        let boundary = "m".repeat(MAX_MODEL_BYTES);
        let unicode_boundary = "\u{00e9}".repeat(MAX_MODEL_BYTES / 2);
        for model in [
            None,
            Some("synthetic-model"),
            Some(&boundary),
            Some(&unicode_boundary),
        ] {
            let ticket = owner
                .acquire_current_unlocked(&account, model, now)
                .unwrap();
            assert_eq!(ticket.request_model.as_deref(), model);
            assert!(owner.ticket_current_unlocked(&ticket, &account, model, now));
            assert!(!owner.ticket_current_unlocked(
                &ticket,
                &account,
                Some("synthetic-other"),
                now
            ));
            if model.is_some() {
                assert!(!owner.ticket_current_unlocked(&ticket, &account, None, now));
            }
            let proof = owner.proofs.admit(&ticket.native).unwrap();
            assert!(
                proof.model.is_none(),
                "never assign a request model to poll evidence"
            );
            assert_eq!(proof.scope, "account");
            assert_eq!(proof.observed_at, observed);
            assert!(proof.matches_current(&account, now));
            assert!(!owner.ticket_current_unlocked(
                &ticket,
                &account,
                model,
                now + Duration::seconds(301)
            ));
            let mut changed = account.clone();
            changed.quota_credit_releasable = !changed.quota_credit_releasable;
            assert!(!owner.ticket_current_unlocked(&ticket, &changed, model, now));
        }
        for oversized in [format!("{boundary}m"), format!("{unicode_boundary}m")] {
            assert!(
                owner
                    .acquire_current_unlocked(&account, Some(&oversized), now)
                    .is_none()
            );
        }
        assert_eq!(
            f.runtime.store.read_account_unlocked(&f.id).unwrap(),
            account
        );
        assert!(owner.proofs.acquire(&f.id).unwrap().1.model.is_none());
    }

    #[tokio::test]
    async fn n5a_wait_model_scope_model_bearing_account_report_requires_exact_request() {
        let f = Fixture::new();
        let raw = Fixture::raw();
        let model = "synthetic-model";
        let origin = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let account = f
            .runtime
            .apply_quota_snapshot_reported_origin(
                &f.id,
                raw.clone(),
                &raw,
                Some(model),
                Some(origin),
            )
            .await
            .unwrap();
        let _gate = f.runtime.store.transaction().unwrap();
        let owner = &f.runtime.wait_owner;
        let now = Utc::now();
        let ticket = owner
            .acquire_current_unlocked(&account, Some(model), now)
            .unwrap();
        let proof = owner.proofs.admit(&ticket.native).unwrap();
        assert_eq!(proof.scope, "account");
        assert_eq!(proof.model.as_deref(), Some(model));
        assert!(owner.ticket_current_unlocked(&ticket, &account, Some(model), now));
        for other in [None, Some("synthetic-other")] {
            assert!(
                owner
                    .acquire_current_unlocked(&account, other, now)
                    .is_none()
            );
            assert!(!owner.ticket_current_unlocked(&ticket, &account, other, now));
        }
    }

    #[test]
    fn n5a_wait_model_scope_model_and_malformed_native_scopes_remain_closed() {
        for (scope, reported, allowed) in [
            ("model:synthetic-model", Some("synthetic-model"), true),
            ("model:synthetic-model", None, false),
            ("model:synthetic-model", Some("synthetic-other"), false),
            ("model:", Some(""), false),
            ("unknown", None, false),
            ("unknown", Some("synthetic-model"), false),
        ] {
            let now = Utc::now();
            let reset_at = now + Duration::minutes(10);
            let mut account = crate::accounts::health::tests::fixture();
            account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: reset_at,
                scope: scope.into(),
            };
            account.quota_cooling = Some(QuotaCooling {
                window: "5h".into(),
                reset_at,
                started_at: now,
            });
            let proof = ShortQuotaProof {
                reset_at,
                model: reported.map(str::to_owned),
                observed_at: now,
                earliest_reset_at: reset_at,
                started_at: now,
                scope: scope.into(),
                allow_credits: account.allow_credits,
                quota_credit_releasable: account.quota_credit_releasable,
            };
            assert!(proof.matches_current(&account, now));
            // Isolate defensive owner applicability with a controlled compact
            // payload; this is not evidence of a native producer minting it.
            let owner = Arc::new(WaitOwner::new());
            let origin = owner.capture(&account.id, ReportSource::UsagePoll).unwrap();
            let mut application = owner.accept(&account.id, origin).unwrap();
            application.before_store_mutation(&account.id);
            application.finish(Some(proof));
            assert!(
                owner
                    .acquire_current_unlocked(&account, None, now)
                    .is_none()
            );
            assert!(
                owner
                    .acquire_current_unlocked(&account, Some("synthetic-other"), now)
                    .is_none()
            );
            let ticket = owner.acquire_current_unlocked(&account, Some("synthetic-model"), now);
            assert_eq!(
                ticket.is_some(),
                allowed,
                "scope {scope}, reported {reported:?}"
            );
            if let Some(ticket) = ticket {
                assert!(owner.ticket_current_unlocked(
                    &ticket,
                    &account,
                    Some("synthetic-model"),
                    now
                ));
                assert!(!owner.ticket_current_unlocked(&ticket, &account, None, now));
                assert!(!owner.ticket_current_unlocked(
                    &ticket,
                    &account,
                    Some("synthetic-other"),
                    now
                ));
            }
        }
    }

    #[tokio::test]
    async fn n5a_wait_notify_real_publish_and_replacement_wake_enabled_waiter() {
        let f = Fixture::new();
        let owner = &f.runtime.wait_owner;
        let notified = owner.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        let account = f.proven().await;
        assert!(notification_ready(notified.as_mut()));
        let ticket = {
            let _gate = f.runtime.store.transaction().unwrap();
            owner
                .acquire_current_unlocked(&account, None, Utc::now())
                .unwrap()
        };
        let notified = owner.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        assert!(
            !notification_ready(notified.as_mut()),
            "ordinary capture alone is quiet"
        );
        f.runtime.discard_report_origin(&f.id, Some(origin)).await;
        assert!(notification_ready(notified.as_mut()));
        let _gate = f.runtime.store.transaction().unwrap();
        assert!(!owner.ticket_current_unlocked(&ticket, &account, None, Utc::now()));
    }
    #[test]
    fn n5a_wait_notify_mint_success_and_stale_failure_both_wake() {
        for stale in [false, true] {
            let owner = Arc::new(WaitOwner::new());
            let origin = owner.capture("a", ReportSource::UsagePoll).unwrap();
            let mut application = owner.accept("a", origin).unwrap();
            application.before_store_mutation("a");
            if stale {
                owner.invalidate("a");
            }
            let now = Utc::now();
            // Synthetic compact payload isolates mint notification only; this
            // test does not claim a successful native store application.
            let proof = ShortQuotaProof {
                reset_at: now + Duration::minutes(5),
                model: None,
                observed_at: now,
                earliest_reset_at: now + Duration::minutes(5),
                started_at: now,
                scope: "account".into(),
                allow_credits: false,
                quota_credit_releasable: false,
            };
            let notified = owner.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            application.finish(Some(proof));
            assert!(notification_ready(notified.as_mut()));
            assert_eq!(owner.proofs.acquire("a").is_ok(), !stale);
        }
    }

    #[test]
    fn n5a_wait_notify_failed_capture_and_repeated_close_still_wake() {
        let owner = WaitOwner::new();
        owner.close();
        let notified = owner.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        assert!(owner.capture("a", ReportSource::HttpAttempt).is_none());
        assert!(notification_ready(notified.as_mut()));
        let notified = owner.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        owner.close();
        assert!(notification_ready(notified.as_mut()));
    }
    #[test]
    fn n5a_wait_receipt_clock_mapping_is_strict_and_does_not_extend_budget() {
        let wall = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
        let mono = tokio::time::Instant::now();
        let second = std::time::Duration::from_secs(1);
        let reset = wall + Duration::seconds(60);
        let wake = mono + second * 60;
        assert_eq!(wait_wake(reset, wall, mono, 60, wake + second), Some(wake));
        assert_eq!(wait_wake(reset, wall, mono, 59, wake + second), None);
        assert_eq!(wait_wake(reset, wall, mono, 0, wake + second), None);
        assert_eq!(wait_wake(reset, wall, mono, 60, wake), None);
        assert_eq!(wait_wake(reset, wall, mono, 60, wake - second), None);
        assert_eq!(wait_wake(wall, wall, mono, 60, wake), None);
        assert_eq!(
            wait_wake(wall - Duration::nanoseconds(1), wall, mono, 60, wake),
            None
        );
        let subsecond = wall + Duration::nanoseconds(1);
        assert_eq!(
            wait_wake(subsecond, wall, mono, 1, wake),
            Some(mono + std::time::Duration::from_nanos(1))
        );
    }
    #[test]
    fn n5a_wait_receipt_unrelated_revision_keeps_effective_plan_but_changes_refuse() {
        let original = crate::routing::Plan {
            settings: crate::routing::Settings::default(),
            anchor: Some("cx-000001".into()),
            project: Some("synthetic-project".into()),
            source: "session",
            revision: 7,
        };
        let mut unrelated = original.clone();
        unrelated.revision += 1;
        assert!(same_wait_plan(&original, &unrelated));
        for field in ["mode", "budget", "anchor", "project", "source"] {
            let mut changed = unrelated.clone();
            match field {
                "mode" => changed.settings.mode = crate::routing::Mode::Strict,
                "budget" => changed.settings.reset_wait_secs -= 1,
                "anchor" => changed.anchor = Some("cx-000002".into()),
                "project" => changed.project = Some("synthetic-other-project".into()),
                "source" => changed.source = "project",
                _ => unreachable!(),
            }
            assert!(!same_wait_plan(&original, &changed), "{field}");
        }
    }

    #[test]
    fn n5a_wait_receipt_late_monotonic_sample_never_extends_original_deadline() {
        let wall = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
        let mono_at_wall = tokio::time::Instant::now();
        let delay = std::time::Duration::from_secs(5);
        let minute = std::time::Duration::from_secs(60);
        let reset = wall + Duration::seconds(60);
        let sampled_mono = mono_at_wall + delay;
        let wake = sampled_mono + minute;
        assert_eq!(
            wait_wake(
                reset,
                wall,
                sampled_mono,
                60,
                wake + std::time::Duration::from_nanos(1)
            ),
            Some(wake)
        );
        assert_eq!(wait_wake(reset, wall, sampled_mono, 60, wake), None);
        // A deadline that fit the undelayed estimate is not extended to fit
        // scheduling delay; decline optional wait and preserve native fallback.
        assert_eq!(
            wait_wake(
                reset,
                wall,
                sampled_mono,
                60,
                mono_at_wall + minute + std::time::Duration::from_secs(1)
            ),
            None
        );
    }
    fn renewal_raw(reset: DateTime<Utc>) -> Value {
        let observed = Utc::now();
        json!({"observed_at":observed,"windows":[
            {"name":"5h","used_pct":100.0,"observed_at":observed,"reset_at":reset},
            {"name":"7d","used_pct":20.0,"observed_at":observed,"reset_at":reset+Duration::days(1)}
        ]})
    }

    async fn renewal_apply(f: &Fixture, reset: DateTime<Utc>, model: Option<&str>) -> Account {
        let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let raw = renewal_raw(reset);
        let account = f
            .runtime
            .apply_quota_snapshot_reported_origin(&f.id, raw.clone(), &raw, model, Some(origin))
            .await
            .unwrap();
        assert_eq!(f.proof(), Some(reset), "normal full producer must publish");
        account
    }

    fn renewal_envelope(
        f: &Fixture,
        account: &Account,
        wall: DateTime<Utc>,
        mono: tokio::time::Instant,
    ) -> AdmittedQuotaWait {
        // Unit helper only: cannot construct BindingTable's private WaitIntent.
        // Actual gate/intent admission remains an integration test requirement.
        let ticket = f
            .runtime
            .wait_owner
            .acquire_current_unlocked(account, None, wall)
            .unwrap();
        let deadline = mono + std::time::Duration::from_secs(90);
        let wake = wait_wake(ticket.reset_at(), wall, mono, 60, deadline).unwrap();
        AdmittedQuotaWait {
            ticket,
            account: f.id.clone(),
            key: "codex:synthetic-renewal".into(),
            model: None,
            provider: account.provider,
            policy_key: "codex:synthetic-renewal".into(),
            plan: crate::routing::Plan {
                settings: crate::routing::Settings::default(),
                anchor: Some(f.id.clone()),
                project: None,
                source: "session",
                revision: 7,
            },
            wake,
            deadline,
        }
    }

    #[tokio::test]
    async fn n5a_wait_renewal_full_replacement_changes_ticket_without_extending_envelope() {
        let f = Fixture::new();
        let wall = Utc::now();
        let reset = wall + Duration::seconds(40);
        let mono = tokio::time::Instant::now();
        let account = renewal_apply(&f, reset, None).await;
        let old_checker = f.ticket();
        let mut receipt = {
            let _gate = f.runtime.store.transaction().unwrap();
            renewal_envelope(&f, &account, Utc::now(), mono)
        };
        let original_wake = receipt.wake;
        let original_deadline = receipt.deadline;
        let original_key = receipt.key.clone();
        let original_policy = receipt.policy_key.clone();
        for step in 1..=3 {
            let current = renewal_apply(&f, reset, None).await;
            assert!(
                !f.admits(&old_checker),
                "replacement must not revive old proof"
            );
            let _gate = f.runtime.store.transaction().unwrap();
            // Deliberately unequal wall/mono advances would move the wake if
            // renewal recomputed it. Exact envelope copying must resist that.
            receipt = f
                .runtime
                .wait_owner
                .renewal_candidate_unlocked(
                    receipt,
                    &current,
                    Utc::now() + Duration::seconds(step),
                    mono + std::time::Duration::from_secs(step as u64 * 5),
                )
                .unwrap();
            assert_eq!(receipt.reset_at(), reset);
            assert_eq!(receipt.wake, original_wake);
            assert_eq!(receipt.deadline, original_deadline);
            assert_eq!(receipt.key, original_key);
            assert_eq!(receipt.policy_key, original_policy);
            assert_eq!(receipt.policy_revision(), 7);
            assert!(f.runtime.wait_owner.ticket_current_unlocked(
                &receipt.ticket,
                &current,
                None,
                Utc::now()
            ));
            assert!(f.runtime.wait_owner.proofs.admit(&old_checker).is_err());
        }
    }

    #[tokio::test]
    async fn n5a_wait_model_scope_http_to_full_poll_renews_same_request_envelope() {
        let f = Fixture::new();
        let model = "synthetic-model";
        let reset =
            DateTime::from_timestamp((Utc::now() + Duration::seconds(40)).timestamp(), 0).unwrap();
        let provider = f.runtime.store.load(&f.id).unwrap().provider;
        let bindings = crate::bindings::BindingTable::new(
            f.home.path(),
            crate::config::RoutingConfig::default(),
        )
        .unwrap();
        let origin = f.runtime.capture_http_origin(&f.id).await.unwrap();
        let asof = bindings.begin_usage_observation_for(&f.id);
        let (observation, pending) = f
            .runtime
            .observe_http_429_headers_pending(
                &f.id,
                provider,
                &Fixture::headers(provider, reset),
                Some(model),
                (&bindings, asof),
                Some(origin),
            )
            .await;
        assert_eq!(observation.quota_until, Some(reset));
        assert!(f.proof().is_none(), "actual headers must not mint early");
        f.runtime
            .finish_http_429_report_with_body(
                &f.id,
                Some(pending.unwrap()),
                Some(reset),
                Some(Fixture::quota_signal(reset)),
                qualified_body(reset, Some(model)),
            )
            .await
            .unwrap();
        let before = f.runtime.store.load(&f.id).unwrap();
        let mono = tokio::time::Instant::now();
        let old_checker = f.ticket();
        let receipt = {
            let _gate = f.runtime.store.transaction().unwrap();
            let owner = &f.runtime.wait_owner;
            let wall = Utc::now();
            let ticket = owner
                .acquire_current_unlocked(&before, Some(model), wall)
                .unwrap();
            assert_eq!(
                owner.proofs.admit(&ticket.native).unwrap().model.as_deref(),
                Some(model)
            );
            let deadline = mono + std::time::Duration::from_secs(90);
            AdmittedQuotaWait {
                wake: wait_wake(reset, wall, mono, 60, deadline).unwrap(),
                deadline,
                ticket,
                account: f.id.clone(),
                key: "codex:synthetic-model-renewal".into(),
                model: Some(model.into()),
                provider,
                policy_key: "codex:synthetic-model-renewal".into(),
                plan: crate::routing::Plan {
                    settings: crate::routing::Settings::default(),
                    anchor: Some(f.id.clone()),
                    project: None,
                    source: "session",
                    revision: 7,
                },
            }
        };
        let envelope = (
            receipt.wake,
            receipt.deadline,
            receipt.account.clone(),
            receipt.key.clone(),
            receipt.model.clone(),
            receipt.provider,
            receipt.policy_key.clone(),
            receipt.plan.clone(),
        );
        let current = renewal_apply(&f, reset, None).await;
        assert!(!f.admits(&old_checker));
        let renewed = {
            let _gate = f.runtime.store.transaction().unwrap();
            let owner = &f.runtime.wait_owner;
            assert!(!owner.ticket_current_unlocked(
                &receipt.ticket,
                &current,
                Some(model),
                Utc::now()
            ));
            let renewed = owner
                .renewal_candidate_unlocked(receipt, &current, Utc::now(), mono)
                .unwrap();
            assert_eq!(renewed.ticket.request_model.as_deref(), Some(model));
            let proof = owner.proofs.admit(&renewed.ticket.native).unwrap();
            assert!(
                proof.model.is_none(),
                "the replacement is the real account poll"
            );
            assert!(owner.ticket_current_unlocked(
                &renewed.ticket,
                &current,
                Some(model),
                Utc::now()
            ));
            assert!(!owner.ticket_current_unlocked(&renewed.ticket, &current, None, Utc::now()));
            assert!(!owner.ticket_current_unlocked(
                &renewed.ticket,
                &current,
                Some("synthetic-other"),
                Utc::now()
            ));
            assert_eq!(renewed.reset_at(), reset);
            assert_eq!(
                (
                    renewed.wake,
                    renewed.deadline,
                    &renewed.account,
                    &renewed.key,
                    &renewed.model,
                    renewed.provider,
                    &renewed.policy_key
                ),
                (
                    envelope.0,
                    envelope.1,
                    &envelope.2,
                    &envelope.3,
                    &envelope.4,
                    envelope.5,
                    &envelope.6
                )
            );
            assert!(same_wait_plan(&renewed.plan, &envelope.7));
            assert_eq!(renewed.plan.revision, envelope.7.revision);
            renewed
        };
        // A later partial still revokes the renewed ticket immediately.
        let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
        let mut partial = renewal_raw(reset);
        partial["windows"].as_array_mut().unwrap().truncate(1);
        let current = f.apply(partial, Some(origin)).await;
        let _gate = f.runtime.store.transaction().unwrap();
        assert!(!f.runtime.wait_owner.ticket_current_unlocked(
            &renewed.ticket,
            &current,
            Some(model),
            Utc::now()
        ));
        assert!(
            f.runtime
                .wait_owner
                .renewal_candidate_unlocked(renewed, &current, Utc::now(), mono)
                .is_none()
        );
        assert!(f.runtime.wait_owner.proofs.admit(&old_checker).is_err());
    }

    #[tokio::test]
    async fn n5a_wait_renewal_still_current_ticket_preserves_original_receipt() {
        let f = Fixture::new();
        let reset = Utc::now() + Duration::seconds(40);
        let current = renewal_apply(&f, reset, None).await;
        let checker = f.ticket();
        let mono = tokio::time::Instant::now();
        let _gate = f.runtime.store.transaction().unwrap();
        let receipt = renewal_envelope(&f, &current, Utc::now(), mono);
        let wake = receipt.wake;
        let deadline = receipt.deadline;
        let receipt = f
            .runtime
            .wait_owner
            .renewal_candidate_unlocked(receipt, &current, Utc::now(), mono)
            .unwrap();
        assert_eq!((receipt.wake, receipt.deadline), (wake, deadline));
        assert!(f.runtime.wait_owner.proofs.admit(&checker).is_ok());
    }

    #[tokio::test]
    async fn n5a_wait_renewal_partial_and_failed_native_mutation_refuse_immediately() {
        for failed_mutation in [false, true] {
            let f = Fixture::new();
            let reset = Utc::now() + Duration::seconds(40);
            let before = renewal_apply(&f, reset, None).await;
            let checker = f.ticket();
            let mono = tokio::time::Instant::now();
            let receipt = {
                let _gate = f.runtime.store.transaction().unwrap();
                renewal_envelope(&f, &before, Utc::now(), mono)
            };
            if failed_mutation {
                assert!(
                    f.runtime
                        .store
                        .update_metadata(&f.id, |account| account.schema = 0)
                        .is_err()
                );
            } else {
                let origin = f.runtime.capture_poll_origin(&f.id).await.unwrap();
                let mut partial = renewal_raw(reset);
                partial["windows"].as_array_mut().unwrap().truncate(1);
                f.apply(partial, Some(origin)).await;
            }
            assert!(f.proof().is_none());
            assert!(!f.admits(&checker));
            let _gate = f.runtime.store.transaction().unwrap();
            let current = f.runtime.store.read_account_unlocked(&f.id).unwrap();
            assert!(
                f.runtime
                    .wait_owner
                    .renewal_candidate_unlocked(receipt, &current, Utc::now(), mono,)
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn n5a_wait_renewal_new_full_changed_reset_or_model_cannot_replace_envelope() {
        for changed_model in [false, true] {
            let f = Fixture::new();
            let reset = Utc::now() + Duration::seconds(40);
            let before = renewal_apply(&f, reset, None).await;
            let mono = tokio::time::Instant::now();
            let receipt = {
                let _gate = f.runtime.store.transaction().unwrap();
                renewal_envelope(&f, &before, Utc::now(), mono)
            };
            let current = renewal_apply(
                &f,
                if changed_model {
                    reset
                } else {
                    reset + Duration::seconds(1)
                },
                if changed_model {
                    Some("synthetic-other-model")
                } else {
                    None
                },
            )
            .await;
            let _gate = f.runtime.store.transaction().unwrap();
            assert!(
                f.runtime
                    .wait_owner
                    .renewal_candidate_unlocked(receipt, &current, Utc::now(), mono,)
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn n5a_wait_renewal_original_wake_deadline_and_wall_budget_are_strict() {
        let f = Fixture::new();
        let reset = Utc::now() + Duration::seconds(40);
        let account = renewal_apply(&f, reset, None).await;
        let mono = tokio::time::Instant::now();
        let _gate = f.runtime.store.transaction().unwrap();
        for boundary in ["wake", "deadline", "wall-reset", "rollback-budget"] {
            let receipt = renewal_envelope(&f, &account, Utc::now(), mono);
            let (wall, at) = match boundary {
                "wake" => (Utc::now(), receipt.wake),
                "deadline" => (Utc::now(), receipt.deadline),
                "wall-reset" => (reset, mono),
                "rollback-budget" => (reset - Duration::seconds(61), mono),
                _ => unreachable!(),
            };
            assert!(
                f.runtime
                    .wait_owner
                    .renewal_candidate_unlocked(receipt, &account, wall, at)
                    .is_none(),
                "{boundary}"
            );
        }
    }

    #[tokio::test]
    async fn n5a_wait_renewal_new_ticket_is_revoked_by_later_failed_mutation() {
        let f = Fixture::new();
        let reset = Utc::now() + Duration::seconds(40);
        let before = renewal_apply(&f, reset, None).await;
        let mono = tokio::time::Instant::now();
        let receipt = {
            let _gate = f.runtime.store.transaction().unwrap();
            renewal_envelope(&f, &before, Utc::now(), mono)
        };
        let current = renewal_apply(&f, reset, None).await;
        let renewed = {
            let _gate = f.runtime.store.transaction().unwrap();
            f.runtime
                .wait_owner
                .renewal_candidate_unlocked(receipt, &current, Utc::now(), mono)
                .unwrap()
        };
        assert!(
            f.runtime
                .store
                .update_metadata(&f.id, |account| account.schema = 0)
                .is_err()
        );
        let _gate = f.runtime.store.transaction().unwrap();
        assert!(!f.runtime.wait_owner.ticket_current_unlocked(
            &renewed.ticket,
            &current,
            None,
            Utc::now()
        ));
        assert!(
            f.runtime
                .wait_owner
                .renewal_candidate_unlocked(renewed, &current, Utc::now(), mono)
                .is_none()
        );
    }
}
