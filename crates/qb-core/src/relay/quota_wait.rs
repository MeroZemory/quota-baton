//! Revocable proof-backed waiting, never send authority.
use super::RelayState;
use crate::{
    accounts::Provider,
    bindings::HttpWaitRequest,
    issue_operation::{CallerGuard, IssueOperation},
};
use chrono::{DateTime, Utc};
use tokio::time::Instant;

/// Some means a wait was admitted, including when subsequently revoked.
/// Native selection must still recollect and independently authorize any send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WaitEnd {
    Reset,
    NativeCacheReset,
    Revoked,
    WallResetPending,
}

pub(super) struct WaitHint {
    pub(super) source: String,
    pub(super) end: WaitEnd,
    native_cache: Option<NativeCacheInterval>,
}

impl WaitHint {
    pub(super) fn reset_reached(&self) -> bool {
        matches!(self.end, WaitEnd::Reset | WaitEnd::NativeCacheReset)
    }
}

// Immutable data from an already admitted wait, not a second hold/proof lease.
// Cloning for blocking reads preserves the same operation, wake and deadline.
#[derive(Clone)]
struct NativeCacheInterval {
    inputs: Inputs,
    plan: crate::routing::Plan,
    wake: Instant,
    reset: DateTime<Utc>,
}

#[derive(Clone)]
struct Inputs {
    provider: Provider,
    key: String,
    model: Option<String>,
    source: String,
    operation: IssueOperation,
}

impl Inputs {
    fn request(&self) -> HttpWaitRequest<'_> {
        HttpWaitRequest {
            provider: self.provider,
            key: Some(&self.key),
            model: self.model.as_deref(),
            expected_source: Some(&self.source),
            operation: &self.operation,
        }
    }
}

/// Borrow the ORIGINAL request's guard, never construct/disarm/admit another.
/// This borrow is a lifetime requirement, not an identity check: caller wiring
/// must pass the guard paired with `operation` and keep the operation Pending.
#[allow(clippy::too_many_arguments)]
pub(super) async fn wait_http_quota(
    relay: &RelayState,
    provider: Provider,
    key: &str,
    model: Option<&str>,
    source: &str,
    operation: &IssueOperation,
    _caller: &CallerGuard,
) -> Option<WaitHint> {
    wait_quota(
        relay,
        provider,
        key,
        model,
        source,
        operation,
        _caller,
        WaitTransport::Http,
    )
    .await
}

#[derive(Clone, Copy)]
enum WaitTransport {
    Http,
    WsCreate,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wait_ws_quota(
    relay: &RelayState,
    key: &str,
    model: Option<&str>,
    source: &str,
    operation: &IssueOperation,
    caller: &CallerGuard,
) -> Option<WaitHint> {
    wait_quota(
        relay,
        Provider::Codex,
        key,
        model,
        source,
        operation,
        caller,
        WaitTransport::WsCreate,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn wait_quota(
    relay: &RelayState,
    provider: Provider,
    key: &str,
    model: Option<&str>,
    source: &str,
    operation: &IssueOperation,
    _caller: &CallerGuard,
    transport: WaitTransport,
) -> Option<WaitHint> {
    // One display-only lifetime survives both the optional engine and a native
    // cache remainder; policy cancellation does not hide a still-pending hold.
    let mut quota_wait_visibility = None;
    let mut hint = wait_optional_quota(
        relay,
        provider,
        key,
        model,
        source,
        operation,
        _caller,
        transport,
        &mut quota_wait_visibility,
    )
    .await?;
    // The optional engine has returned and dropped its receipt/JoinHandle before
    // a native remainder starts. A running blocking task remains inspection-only.
    let result = match hint.native_cache.take() {
        Some(interval) => Some(wait_native_cache(relay, interval).await),
        None => Some(hint),
    };
    drop(quota_wait_visibility);
    result
}

#[allow(clippy::too_many_arguments)]
async fn wait_optional_quota(
    relay: &RelayState,
    provider: Provider,
    key: &str,
    model: Option<&str>,
    source: &str,
    operation: &IssueOperation,
    _caller: &CallerGuard,
    transport: WaitTransport,
    quota_wait_visibility: &mut Option<crate::dashboard::runtime::QuotaWaitGuard>,
) -> Option<WaitHint> {
    // Bound copies before any detached blocking work. These are lookup inputs,
    // not independently trusted account/binding identities.
    if key.is_empty()
        || key.len() > 1024
        || source.is_empty()
        || source.len() > 128
        || model.is_some_and(|m| m.len() > 256)
    {
        return None;
    }
    let inputs = Inputs {
        provider,
        key: key.to_owned(),
        model: model.map(str::to_owned),
        source: source.to_owned(),
        operation: operation.clone(),
    };
    #[cfg(not(feature = "testkit"))]
    let _ = transport;
    let deadline = operation.deadline();
    let mut account_events = relay.accounts.subscribe();
    // Infer the crate-visible receipt returned through the runtime API; the
    // private wait_owner module need not be exposed to this relay child.
    let mut receipt = None;
    let mut policy_snapshot: Option<crate::account_runtime::WaitPolicySnapshot> = None;
    // Once set, this envelope is immutable even after replacement proof tickets.
    let mut envelope: Option<(Instant, DateTime<Utc>)> = None;
    let mut native_cache = None;

    loop {
        // Arm BOTH authority notifications before EVERY native inspection. An
        // event racing that inspection remains ready for the sleeping phase.
        let proof = relay.accounts.http_quota_wait_changed();
        let policy = relay.bindings.policies.changed.notified();
        tokio::pin!(proof, policy);
        proof.as_mut().enable();
        policy.as_mut().enable();
        if !caller_live(operation, deadline, relay.is_draining()) {
            return stopped(source, envelope.is_some());
        }
        // A queued notification at the original reset is not a reason to
        // demand renewal of a now-expired proof. This is a reselect hint only.
        if let Some(ended) = ended_envelope(source, envelope, Instant::now(), Utc::now()) {
            return Some(ended);
        }
        // Reject-only policy view: no Store/runtime/binding gates or new proof.
        // Global revision alone is not a change to this admitted wait.
        let policy_current =
            match wait_policy_unchanged(relay, policy_snapshot.as_ref(), &inputs, envelope).await {
                Ok(current) => current,
                Err(ended) => return ended,
            };
        if !policy_current {
            return policy_stopped(source, envelope.is_some(), native_cache.take());
        }
        let accounts = relay.accounts.clone();
        let bindings = relay.bindings.clone();
        let work_inputs = inputs.clone();
        let previous = receipt.take();
        #[cfg(feature = "testkit")]
        let result_hold = if previous.is_none() && matches!(transport, WaitTransport::Http) {
            relay.take_http_quota_wait_result_hold(key, source)
        } else {
            None
        };
        let task = tokio::task::spawn_blocking(move || {
            if !caller_live(
                &work_inputs.operation,
                work_inputs.operation.deadline(),
                false,
            ) {
                return Ok(None);
            }
            let inspected = match previous {
                None => accounts.current_http_quota_wait(&bindings, work_inputs.request()),
                Some(old) => {
                    accounts.renew_current_http_quota_wait(&bindings, work_inputs.request(), old)
                }
            };
            // The actual native API has returned: ALL native guards are gone.
            // This owner-local test gate delays delivery, never creates proof.
            #[cfg(feature = "testkit")]
            if let (Some(hold), Ok(Some(current))) = (result_hold, &inspected) {
                hold.wait(super::HttpQuotaWaitResultBounds {
                    source: current.account().to_owned(),
                    reset_at: current.reset_at(),
                    wake: current.wake(),
                    deadline: current.deadline(),
                });
            }
            inspected
        });
        #[cfg(feature = "testkit")]
        if matches!(transport, WaitTransport::Http) {
            relay.observe_http_quota_wait_inspection("inspection_started", source);
        }
        tokio::pin!(task);
        let inspected = loop {
            tokio::select! {
            biased;
            _ = relay.wait_for_drain_start() => return stopped(source, envelope.is_some()),
            _ = tokio::time::sleep_until(deadline) => return stopped(source, envelope.is_some()),
            _ = async {
                match envelope {
                    Some((wake, _)) => tokio::time::sleep_until(wake).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                if !caller_live(operation, deadline, relay.is_draining()) {
                    return stopped(source, envelope.is_some());
                }
                return ended_envelope(source, envelope, Instant::now(), Utc::now());
            },
            _ = &mut policy => {
                // Rearm BEFORE observing publication: notify_waiters stores no
                // permit for a waiter that was not yet registered.
                policy.set(relay.bindings.policies.changed.notified());
                policy.as_mut().enable();
                if !caller_live(operation, deadline, relay.is_draining()) {
                    return stopped(source, envelope.is_some());
                }
                if let Some(ended) = ended_envelope(source, envelope, Instant::now(), Utc::now()) {
                    return Some(ended);
                }
                let policy_current = match wait_policy_unchanged(
                    relay,
                    policy_snapshot.as_ref(),
                    &inputs,
                    envelope,
                ).await {
                    Ok(current) => current,
                    Err(ended) => return ended,
                };
                if !policy_current {
                    #[cfg(feature = "testkit")]
                    if matches!(transport, WaitTransport::Http) {
                        relay.observe_http_quota_wait_inspection("policy_revoked", source);
                    }
                    return policy_stopped(source, envelope.is_some(), native_cache.take());
                }
                #[cfg(feature = "testkit")]
                if matches!(transport, WaitTransport::Http) {
                    relay.observe_http_quota_wait_inspection(
                        if policy_snapshot.is_some() {
                            "policy_unchanged"
                        } else {
                            "policy_before_snapshot"
                        },
                        source,
                    );
                }
                // Unrelated/same-value publication, or no admitted receipt yet:
                // keep THIS task. Never spawn another blocked inspection.
            },
            result = &mut task => break result,
            }
        };
        // Dropping a JoinHandle cannot stop running blocking work. It only
        // performs inspection, never dispatch, writes, or operation admission.
        if !caller_live(operation, deadline, relay.is_draining()) {
            return stopped(source, envelope.is_some());
        }
        // Blocking inspection can cross reset and correctly refuse an expired
        // ticket. The original envelope, not that expired ticket, defines wake.
        if let Some(ended) = ended_envelope(source, envelope, Instant::now(), Utc::now()) {
            return Some(ended);
        }
        let current = match inspected {
            Ok(Ok(Some(current))) => current,
            _ => {
                #[cfg(feature = "testkit")]
                if envelope.is_none() && matches!(transport, WaitTransport::Http) {
                    relay.observe_http_quota_wait_inspection("initial_refused", source);
                }
                return envelope.map(|_| WaitHint {
                    source: inputs.source.clone(),
                    end: WaitEnd::Revoked,
                    native_cache: None,
                });
            }
        };
        let first = envelope.is_none();
        let fixed = *envelope.get_or_insert((current.wake(), current.reset_at()));
        if current.account() != source
            || current.model() != model
            || current.deadline() != deadline
            || (current.wake(), current.reset_at()) != fixed
        {
            return Some(WaitHint {
                source: inputs.source.clone(),
                end: WaitEnd::Revoked,
                native_cache: None,
            });
        }
        if first {
            policy_snapshot = Some(current.policy_snapshot());
        }
        // Publication may have been consumed before the first snapshot was
        // available. Recheck the receipt's policy before retaining or sleeping.
        // First delivery uses the actual receipt's immutable bounds for timers
        // only. No cache interval, display guard or handoff exists until below.
        #[cfg(all(test, feature = "testkit"))]
        if first && matches!(transport, WaitTransport::Http) {
            relay.observe_http_quota_wait_inspection("result_policy_check", source);
        }
        let policy_current = match wait_policy_unchanged(
            relay,
            policy_snapshot.as_ref(),
            &inputs,
            Some(fixed),
        )
        .await
        {
            Ok(current) => current,
            Err(ended) => return ended,
        };
        if !policy_current {
            #[cfg(feature = "testkit")]
            if matches!(transport, WaitTransport::Http) {
                relay.observe_http_quota_wait_inspection("result_policy_revoked", source);
            }
            return policy_stopped(source, envelope.is_some(), native_cache.take());
        }
        if first {
            // Capture policy from the actual receipt, not a later plan lookup.
            native_cache = Some(NativeCacheInterval {
                inputs: inputs.clone(),
                plan: policy_snapshot
                    .as_ref()
                    .expect("first wait policy")
                    .plan()
                    .clone(),
                wake: fixed.0,
                reset: fixed.1,
            });
            *quota_wait_visibility =
                relay
                    .dashboard()
                    .quota_wait(key, source, current.reset_at(), deadline);
        }
        receipt = Some(current);
        #[cfg(feature = "testkit")]
        if first {
            match transport {
                WaitTransport::Http => relay.observe_http_quota_wait("admitted", source),
                WaitTransport::WsCreate => {
                    relay.observe_ws_quota_wait("admitted", source, deadline)
                }
            }
        }
        #[cfg(not(feature = "testkit"))]
        let _ = first;

        tokio::select! {
                biased;
                _ = relay.wait_for_drain_start() => return stopped(source, envelope.is_some()),
                _ = tokio::time::sleep_until(deadline) => return stopped(source, envelope.is_some()),
                _ = &mut proof => {},
                _ = &mut policy => {},
                event = account_events.recv() => {
                    match event {
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Some(WaitHint { source: inputs.source.clone(), end: WaitEnd::Revoked, native_cache: None });
                        }
                    }
                }
                _ = tokio::time::sleep_until(fixed.0) => {
                    if !caller_live(operation, deadline, relay.is_draining()) {
                        return stopped(source, envelope.is_some());
                    }
                    return ended_envelope(source, envelope, Instant::now(), Utc::now());
                }
        }
        // Notification -> rearm -> native renewal. A missing/partial/changed
        // proof exits; no new admission or interval after the first receipt.
    }
}

// Publication contention is neither unchanged policy nor an urgency handoff.
// Keep the existing receipt/task, if any, and the ORIGINAL timer envelope while
// waiting for a definite try-only comparison. No Store work or admission here.
async fn wait_policy_unchanged(
    relay: &RelayState,
    snapshot: Option<&crate::account_runtime::WaitPolicySnapshot>,
    inputs: &Inputs,
    envelope: Option<(Instant, DateTime<Utc>)>,
) -> Result<bool, Option<WaitHint>> {
    let Some(snapshot) = snapshot else {
        return Ok(true);
    };
    loop {
        let changed = relay.bindings.policies.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !caller_live(
            &inputs.operation,
            inputs.operation.deadline(),
            relay.is_draining(),
        ) {
            return Err(stopped(&inputs.source, envelope.is_some()));
        }
        if let Some(ended) = ended_envelope(&inputs.source, envelope, Instant::now(), Utc::now()) {
            return Err(Some(ended));
        }
        if let Some(current) = snapshot.try_unchanged(&relay.bindings.policies) {
            return Ok(current);
        }
        tokio::select! {
            biased;
            _ = relay.wait_for_drain_start() => {
                return Err(stopped(&inputs.source, envelope.is_some()));
            },
            _ = tokio::time::sleep_until(inputs.operation.deadline()) => {
                return Err(stopped(&inputs.source, envelope.is_some()));
            },
            _ = async {
                match envelope {
                    Some((wake, _)) => tokio::time::sleep_until(wake).await,
                    None => std::future::pending::<()>().await,
                }
            } => {},
            _ = &mut changed => {},
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
        }
    }
}

// This classification only preserves the unused part of an admitted interval.
// It cannot turn an inspection/proof failure into native hold permission.
fn urgency_only_move(original: &crate::routing::Plan, current: &crate::routing::Plan) -> bool {
    use crate::routing::On5hLimit;
    original.settings.on_5h_limit == On5hLimit::Wait
        && current.settings.on_5h_limit == On5hLimit::Move
        && same_native_cache_plan(original, current)
}

fn same_native_cache_plan(original: &crate::routing::Plan, current: &crate::routing::Plan) -> bool {
    // Once handed off, another urgency-only toggle still uses this SAME native
    // interval. It cannot readmit optional waiting or renew its original bounds.
    original.settings.reset_wait_secs != 0
        && original.settings.reset_wait_secs == current.settings.reset_wait_secs
        && original.settings.mode == current.settings.mode
        && original.anchor == current.anchor
        && original.project == current.project
    // Creating an urgency overlay legitimately changes source to "session".
    // Revision/source labels do not grant different native settings/authority.
}

fn policy_stopped(
    source: &str,
    admitted: bool,
    interval: Option<NativeCacheInterval>,
) -> Option<WaitHint> {
    let mut hint = stopped(source, admitted)?;
    // A candidate only. Exact urgency classification and native retention are
    // checked together by the nonblocking owner view, not on this async branch.
    hint.native_cache = interval;
    Some(hint)
}

fn native_cache_view_current(
    bindings: &crate::bindings::BindingTable,
    interval: &NativeCacheInterval,
    handed_off: bool,
) -> Option<bool> {
    bindings.try_native_cache_view(interval.inputs.request(), |current| {
        if handed_off {
            same_native_cache_plan(&interval.plan, current)
        } else {
            urgency_only_move(&interval.plan, current)
        }
    })
}

fn native_cache_account_current(
    account: &crate::accounts::Account,
    interval: &NativeCacheInterval,
    now: DateTime<Utc>,
) -> bool {
    use crate::accounts::{Health, HealthReason, UserState};
    account.id == interval.inputs.source
        && account.provider == interval.inputs.provider
        && account.user_state == UserState::Active
        && !account.drain
        && !account.auth_probe_pending
        && account.auth_failures == 0
        && account.payment_until.is_none_or(|until| until <= now)
        && account
            .token_meta
            .expires_at
            .is_none_or(|until| until > now)
        && matches!(&account.health, Health::Cooling {
            reason: HealthReason::Quota, until, scope,
        } if *until == interval.reset && *until > now
            && (scope == "account"
                || scope.strip_prefix("model:").is_some_and(|blocked| {
                    !blocked.is_empty() && interval.inputs.model.as_deref() == Some(blocked)
                })))
        && account.quota_cooling.as_ref().is_some_and(|quota| {
            quota.window == "5h" && quota.reset_at == interval.reset && quota.started_at <= now
        })
}

fn native_cache_ended(
    interval: &NativeCacheInterval,
    mono: Instant,
    wall: DateTime<Utc>,
) -> Option<WaitHint> {
    let mut hint = ended_envelope(
        &interval.inputs.source,
        Some((interval.wake, interval.reset)),
        mono,
        wall,
    )?;
    if hint.end == WaitEnd::Reset {
        hint.end = WaitEnd::NativeCacheReset;
    }
    Some(hint)
}

// Ordinary native metadata checks only: never acquire/renew a proof, reserve a
// binding, admit the operation, refresh credentials, or change any deadline.
async fn wait_native_cache(relay: &RelayState, interval: NativeCacheInterval) -> WaitHint {
    use std::time::Duration;
    // Native read-like pruning does not notify policy waiters. At most four
    // quiet reject-only checks per second; original wake/deadline/drain and
    // account/policy notifications remain independent. This cadence grants no
    // permission and never starts another interval or quiet Store read.
    const RECHECK: Duration = Duration::from_millis(250);
    let inputs = &interval.inputs;
    let deadline = inputs.operation.deadline();
    let revoked = || stopped(&inputs.source, true).expect("admitted interval");
    let mut events = relay.accounts.subscribe();
    let mut handed_off = false;
    loop {
        let proof = relay.accounts.http_quota_wait_changed();
        let policy = relay.bindings.policies.changed.notified();
        tokio::pin!(proof, policy);
        proof.as_mut().enable();
        policy.as_mut().enable();
        if !caller_live(&inputs.operation, deadline, relay.is_draining()) {
            return revoked();
        }
        if let Some(ended) = native_cache_ended(&interval, Instant::now(), Utc::now()) {
            return ended;
        }
        match native_cache_view_current(&relay.bindings, &interval, handed_off) {
            Some(false) => return revoked(),
            Some(true) => handed_off = true,
            None => {
                // Busy is not a refusal or permission. No new interval/job is
                // created; caller/drain/original timers remain pollable.
                tokio::select! {
                    biased;
                    _ = relay.wait_for_drain_start() => return revoked(),
                    _ = tokio::time::sleep_until(deadline) => return revoked(),
                    _ = tokio::time::sleep_until(interval.wake) => {},
                    _ = &mut policy => {},
                    _ = &mut proof => {},
                    event = events.recv() => {
                        if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                            return revoked();
                        }
                    },
                    _ = tokio::time::sleep(RECHECK) => {},
                }
                continue;
            }
        }
        let accounts = relay.accounts.clone();
        let bindings = relay.bindings.clone();
        let read = interval.clone();
        let task = tokio::task::spawn_blocking(move || {
            let Ok(_transaction) = accounts.store.transaction() else {
                return Some(false);
            };
            if accounts.store.writes_stopped()
                || accounts.blocked_in_memory(&read.inputs.source)
                || accounts.probing(&read.inputs.source)
            {
                return Some(false);
            }
            let Ok(account) = accounts.store.read_account_unlocked(&read.inputs.source) else {
                return Some(false);
            };
            let credentials = accounts
                .store
                .tokens_unlocked(&account)
                .is_ok_and(|tokens| !tokens.access_token.is_empty());
            if !credentials
                || !caller_live(
                    &read.inputs.operation,
                    read.inputs.operation.deadline(),
                    false,
                )
                || !native_cache_account_current(&account, &read, Utc::now())
            {
                return Some(false);
            }
            native_cache_view_current(&bindings, &read, true)
        });
        tokio::pin!(task);
        let mut dirty = false;
        let current = loop {
            if !caller_live(&inputs.operation, deadline, relay.is_draining()) {
                return revoked();
            }
            if let Some(ended) = native_cache_ended(&interval, Instant::now(), Utc::now()) {
                return ended;
            }
            if native_cache_view_current(&relay.bindings, &interval, true) == Some(false) {
                return revoked();
            }
            tokio::select! {
                biased;
                _ = relay.wait_for_drain_start() => return revoked(),
                _ = tokio::time::sleep_until(deadline) => return revoked(),
                _ = tokio::time::sleep_until(interval.wake) => {},
                _ = &mut policy => {
                    policy.set(relay.bindings.policies.changed.notified());
                    policy.as_mut().enable();
                    dirty = true;
                },
                _ = &mut proof => {
                    proof.set(relay.accounts.http_quota_wait_changed());
                    proof.as_mut().enable();
                    dirty = true;
                },
                event = events.recv() => match event {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => dirty = true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return revoked(),
                },
                result = &mut task => break result.unwrap_or(Some(false)),
                _ = tokio::time::sleep(RECHECK) => {},
            }
        };
        if !caller_live(&inputs.operation, deadline, relay.is_draining()) {
            return revoked();
        }
        if let Some(ended) = native_cache_ended(&interval, Instant::now(), Utc::now()) {
            return ended;
        }
        match current {
            Some(false) => return revoked(),
            None => continue,
            Some(true) if dirty => continue,
            Some(true) => {}
        }
        loop {
            if !caller_live(&inputs.operation, deadline, relay.is_draining()) {
                return revoked();
            }
            if let Some(ended) = native_cache_ended(&interval, Instant::now(), Utc::now()) {
                return ended;
            }
            if native_cache_view_current(&relay.bindings, &interval, true) == Some(false) {
                return revoked();
            }
            tokio::select! {
                biased;
                _ = relay.wait_for_drain_start() => return revoked(),
                _ = tokio::time::sleep_until(deadline) => return revoked(),
                _ = tokio::time::sleep_until(interval.wake) => {},
                _ = &mut policy => break,
                _ = &mut proof => break,
                event = events.recv() => {
                    if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        return revoked();
                    }
                    break;
                },
                _ = tokio::time::sleep(RECHECK) => {},
            }
            // A quiet periodic retention check does not reread credentials or
            // create a blocking job. Notifications trigger ordinary recollection.
        }
    }
}

fn ended_envelope(
    source: &str,
    envelope: Option<(Instant, DateTime<Utc>)>,
    mono: Instant,
    wall: DateTime<Utc>,
) -> Option<WaitHint> {
    let (wake, _) = envelope?;
    if mono < wake {
        return None;
    }
    Some(WaitHint {
        source: source.to_owned(),
        native_cache: None,
        end: if reset_due(envelope, mono, wall) {
            WaitEnd::Reset
        } else {
            WaitEnd::WallResetPending
        },
    })
}

fn stopped(source: &str, admitted: bool) -> Option<WaitHint> {
    admitted.then(|| WaitHint {
        source: source.to_owned(),
        end: WaitEnd::Revoked,
        native_cache: None,
    })
}

fn reset_due(
    envelope: Option<(Instant, DateTime<Utc>)>,
    mono: Instant,
    wall: DateTime<Utc>,
) -> bool {
    envelope.is_some_and(|(wake, reset)| reached(wake, reset, mono, wall))
}

fn caller_live(operation: &IssueOperation, deadline: Instant, draining: bool) -> bool {
    !draining && Instant::now() < deadline && operation.can_reply() && operation.check().is_ok()
}

fn reached(wake: Instant, reset: DateTime<Utc>, mono: Instant, wall: DateTime<Utc>) -> bool {
    mono >= wake && wall >= reset
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reset_hint_requires_original_monotonic_and_actual_wall_boundary() {
        let wake = Instant::now();
        let reset = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        assert!(!reached(wake, reset, wake - Duration::from_nanos(1), reset));
        assert!(!reached(
            wake,
            reset,
            wake,
            reset - chrono::Duration::nanoseconds(1)
        ));
        assert!(reached(wake, reset, wake, reset));
        assert_eq!(
            ended_envelope("synthetic-source", Some((wake, reset)), wake, reset)
                .unwrap()
                .end,
            WaitEnd::Reset
        );
        assert_eq!(
            ended_envelope(
                "synthetic-source",
                Some((wake, reset)),
                wake,
                reset - chrono::Duration::seconds(1)
            )
            .unwrap()
            .end,
            WaitEnd::WallResetPending
        );
    }

    #[test]
    fn admitted_exit_and_notification_or_inspection_crossing_reset_use_original_envelope() {
        let wake = Instant::now();
        let reset = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let envelope = Some((wake, reset));
        assert!(stopped("synthetic-source", false).is_none());
        let exit = stopped("synthetic-source", true).unwrap();
        assert_eq!(exit.source, "synthetic-source");
        assert_eq!(exit.end, WaitEnd::Revoked);
        // Same branch before renewal after a notification and after a blocking
        // refusal: no replacement ticket is required to return a reset hint.
        assert!(reset_due(envelope, wake, reset));
        assert!(reset_due(envelope, wake + Duration::from_secs(1), reset));
        assert!(!reset_due(
            envelope,
            wake,
            reset - chrono::Duration::nanoseconds(1)
        ));
        assert!(!reset_due(None, wake, reset));
        // Outer caller/deadline/drain gate must win, even when reset is due.
        let deadline = Instant::now();
        let operation = IssueOperation::new(deadline);
        assert!(!caller_live(&operation, deadline, false));
        assert_eq!(
            stopped("synthetic-source", true).unwrap().end,
            WaitEnd::Revoked
        );
    }

    #[test]
    fn pending_operation_caller_drop_and_drain_refuse_without_admission() {
        let deadline = Instant::now() + Duration::from_secs(10);
        let operation = IssueOperation::new(deadline);
        let caller = operation.caller_guard();
        assert!(caller_live(&operation, deadline, false));
        assert!(!caller_live(&operation, deadline, true));
        drop(caller);
        assert!(!caller_live(&operation, deadline, false));
        assert!(operation.admit().is_err());
    }

    struct NativeDataHolder {
        release: Option<std::sync::mpsc::SyncSender<()>>,
        worker: Option<std::thread::JoinHandle<()>>,
        released: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl NativeDataHolder {
        fn start(table: std::sync::Arc<crate::bindings::BindingTable>) -> Self {
            let (entered, locked) = std::sync::mpsc::sync_channel(1);
            let (release, resume) = std::sync::mpsc::sync_channel(1);
            let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = released.clone();
            let worker = std::thread::spawn(move || {
                table.hold_native_cache_data_for_test(entered, resume, &flag);
            });
            let holder = Self {
                release: Some(release),
                worker: Some(worker),
                released,
            };
            locked.recv_timeout(Duration::from_secs(5)).unwrap();
            holder
        }

        fn assert_still_locked(&self) {
            assert!(
                !self.released.load(std::sync::atomic::Ordering::Acquire),
                "native cache view must not block Tokio on Data"
            );
        }
    }

    impl Drop for NativeDataHolder {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.try_send(());
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn native_relay() -> (tempfile::TempDir, RelayState, NativeCacheInterval) {
        use std::sync::Arc;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r13-part-c/native-cache-unit-runs");
        std::fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let store = Arc::new(crate::accounts::AccountStore::acquire(home.path()).unwrap());
        let obs = Arc::new(crate::observability::Observability::new(home.path(), None).unwrap());
        let mut config = crate::config::Config::default();
        config.upstream.codex_base = "http://127.0.0.1:9".into();
        config.upstream.anthropic_base = "http://127.0.0.1:9".into();
        let relay = RelayState::new(
            store,
            &config,
            obs,
            Arc::new(crate::debug_pin::DebugPins::default()),
        )
        .unwrap();
        let mut interval = native_interval();
        relay
            .bindings
            .import(serde_json::json!({
                "schema": 1,
                "bindings": [{
                    "key": interval.inputs.key,
                    "account": interval.inputs.source,
                    "last_used": Utc::now(),
                }],
            }))
            .unwrap();
        interval.plan = relay.bindings.routing_plan(Some(&interval.inputs.key));
        relay
            .bindings
            .policies
            .set_session_urgent_for_key(
                &crate::routing::session_id(&interval.inputs.key),
                crate::routing::On5hLimit::Move,
                Some(&interval.inputs.key),
            )
            .unwrap();
        assert_eq!(
            native_cache_view_current(&relay.bindings, &interval, false),
            Some(true)
        );
        (home, relay, interval)
    }

    #[tokio::test(start_paused = true)]
    async fn n5c_native_cache_data_held_original_deadline_still_completes() {
        // Deliberately test the private cache engine, not producer admission.
        // The interval preserves the valid ordering wake(45s) < deadline(60s).
        let (_home, relay, interval) = native_relay();
        let caller = interval.inputs.operation.caller_guard();
        let operation = interval.inputs.operation.clone();
        let deadline = interval.inputs.operation.deadline();
        let holder = NativeDataHolder::start(relay.bindings.clone());
        let mut waiting = Box::pin(wait_native_cache(&relay, interval));
        let first = futures_util::poll!(&mut waiting);
        // This assertion catches a Data.lock mutant even if its safety holder
        // has timed out before the first poll can return.
        holder.assert_still_locked();
        assert!(first.is_pending());
        tokio::time::advance(deadline.saturating_duration_since(Instant::now())).await;
        let end = waiting.await;
        assert_eq!(
            end.end,
            WaitEnd::Revoked,
            "original operation deadline must win"
        );
        holder.assert_still_locked();
        assert!(!caller_live(&operation, deadline, false));
        drop(caller);
    }

    #[tokio::test(start_paused = true)]
    async fn n5c_native_cache_data_held_original_wake_does_not_extend_wall_reset() {
        let (_home, relay, mut interval) = native_relay();
        let caller = interval.inputs.operation.caller_guard();
        interval.reset = Utc::now() + chrono::Duration::days(1);
        let wake = interval.wake;
        let holder = NativeDataHolder::start(relay.bindings.clone());
        let mut waiting = Box::pin(wait_native_cache(&relay, interval));
        let first = futures_util::poll!(&mut waiting);
        holder.assert_still_locked();
        assert!(first.is_pending());
        tokio::time::advance(wake.saturating_duration_since(Instant::now())).await;
        let end = waiting.await;
        assert_eq!(end.end, WaitEnd::WallResetPending);
        assert!(!end.reset_reached());
        holder.assert_still_locked();
        drop(caller);
    }

    fn native_interval() -> NativeCacheInterval {
        let wake = Instant::now() + Duration::from_secs(45);
        NativeCacheInterval {
            inputs: Inputs {
                provider: Provider::Codex,
                key: "codex:synthetic-cache-remainder".into(),
                source: "synthetic-cache-source".into(),
                model: Some("synthetic-cache-model".into()),
                operation: IssueOperation::new(wake + Duration::from_secs(15)),
            },
            plan: crate::routing::Plan {
                settings: crate::routing::Settings {
                    mode: crate::routing::Mode::Recover,
                    ..Default::default()
                },
                anchor: Some("synthetic-cache-source".into()),
                project: Some("synthetic-cache-project".into()),
                source: "project",
                revision: 7,
            },
            wake,
            reset: DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn n5c_native_cache_handoff_requires_only_effective_wait_to_move_change() {
        use crate::routing::{Mode, On5hLimit};
        let interval = native_interval();
        let original = &interval.plan;
        assert!(!urgency_only_move(original, original));
        let mut urgent = original.clone();
        urgent.settings.on_5h_limit = On5hLimit::Move;
        urgent.revision += 1;
        urgent.source = "session";
        assert!(urgency_only_move(original, &urgent));
        assert!(!urgency_only_move(&urgent, &urgent));
        for budget in [0, 59, 61] {
            let mut changed = urgent.clone();
            changed.settings.reset_wait_secs = budget;
            assert!(!urgency_only_move(original, &changed));
        }
        for mode in [Mode::Strict, Mode::Rebalance] {
            let mut changed = urgent.clone();
            changed.settings.mode = mode;
            assert!(!urgency_only_move(original, &changed));
        }
        for value in [None, Some("synthetic-other".to_owned())] {
            let mut changed = urgent.clone();
            changed.anchor = value.clone();
            assert!(!urgency_only_move(original, &changed));
            changed = urgent.clone();
            changed.project = value;
            assert!(!urgency_only_move(original, &changed));
        }
        // A subsequent urgency toggle cannot replace the interval or start a
        // second proof wait. The same native interval applies for either value.
        assert!(same_native_cache_plan(original, &urgent));
        assert!(same_native_cache_plan(original, original));
    }

    #[test]
    fn n5c_native_cache_uses_original_mono_wall_and_operation_deadline() {
        let interval = native_interval();
        let deadline = interval.inputs.operation.deadline();
        assert!(
            native_cache_ended(
                &interval,
                interval.wake - Duration::from_nanos(1),
                interval.reset,
            )
            .is_none()
        );
        let wall_pending = native_cache_ended(
            &interval,
            interval.wake,
            interval.reset - chrono::Duration::nanoseconds(1),
        )
        .unwrap();
        assert_eq!(wall_pending.end, WaitEnd::WallResetPending);
        assert!(!wall_pending.reset_reached());
        let ended = native_cache_ended(&interval, interval.wake, interval.reset).unwrap();
        assert_eq!(ended.end, WaitEnd::NativeCacheReset);
        assert!(ended.reset_reached());
        assert!(ended.native_cache.is_none());
        assert_eq!(interval.inputs.operation.deadline(), deadline);
        assert_eq!(deadline, interval.wake + Duration::from_secs(15));
        assert!(!caller_live(&interval.inputs.operation, deadline, true));
        let caller = interval.inputs.operation.caller_guard();
        drop(caller);
        assert!(!caller_live(&interval.inputs.operation, deadline, false));
    }

    #[test]
    fn n5c_native_cache_rejects_changed_native_source_model_reset_and_health() {
        use crate::accounts::{Health, HealthReason, QuotaCooling, UserState};
        let mut interval = native_interval();
        let now = interval.reset - chrono::Duration::seconds(45);
        let mut account = crate::accounts::health::tests::fixture();
        account.id = interval.inputs.source.clone();
        account.provider = interval.inputs.provider;
        account.user_state = UserState::Active;
        account.drain = false;
        account.auth_probe_pending = false;
        account.auth_failures = 0;
        account.payment_until = None;
        account.token_meta.expires_at = None;
        account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: interval.reset,
            scope: "model:synthetic-cache-model".into(),
        };
        account.quota_cooling = Some(QuotaCooling {
            window: "5h".into(),
            reset_at: interval.reset,
            started_at: now,
        });
        assert!(native_cache_account_current(&account, &interval, now));
        let change: &[fn(&mut crate::accounts::Account)] = &[
            |a| a.id = "synthetic-other-source".into(),
            |a| a.provider = Provider::Claude,
            |a| a.user_state = UserState::Paused,
            |a| a.drain = true,
            |a| a.auth_probe_pending = true,
            |a| a.auth_failures = 1,
            |a| a.health = Health::Ready,
            |a| a.quota_cooling = None,
            |a| a.quota_cooling.as_mut().unwrap().window = "7d".into(),
            |a| a.quota_cooling.as_mut().unwrap().reset_at += chrono::Duration::seconds(1),
            |a| {
                if let Health::Cooling { until, .. } = &mut a.health {
                    *until += chrono::Duration::seconds(1);
                }
            },
            |a| {
                if let Health::Cooling { scope, .. } = &mut a.health {
                    *scope = "model:synthetic-other-model".into();
                }
            },
        ];
        for (case, change) in change.iter().enumerate() {
            let mut changed = account.clone();
            change(&mut changed);
            assert!(
                !native_cache_account_current(&changed, &interval, now),
                "case {case}"
            );
        }
        for expires in [now - chrono::Duration::seconds(1), now] {
            let mut changed = account.clone();
            changed.token_meta.expires_at = Some(expires);
            assert!(!native_cache_account_current(&changed, &interval, now));
        }
        let mut changed = account.clone();
        changed.payment_until = Some(now + chrono::Duration::seconds(1));
        assert!(!native_cache_account_current(&changed, &interval, now));
        // Missing request model must not make an arbitrary unknown scope match
        // by comparing two None values.
        interval.inputs.model = None;
        if let Health::Cooling { scope, .. } = &mut account.health {
            *scope = "synthetic-unknown-scope".into();
        }
        assert!(!native_cache_account_current(&account, &interval, now));
    }

    #[test]
    fn n5c_plain_proof_revocation_never_contains_native_cache_continuation() {
        let interval = native_interval();
        assert!(stopped(&interval.inputs.source, false).is_none());
        let revoked = stopped(&interval.inputs.source, true).unwrap();
        assert_eq!(revoked.end, WaitEnd::Revoked);
        assert!(revoked.native_cache.is_none());
        assert!(!revoked.reset_reached());
        let proof_reset = ended_envelope(
            &interval.inputs.source,
            Some((interval.wake, interval.reset)),
            interval.wake,
            interval.reset,
        )
        .unwrap();
        assert_eq!(proof_reset.end, WaitEnd::Reset);
        assert!(proof_reset.native_cache.is_none());
    }
}

#[cfg(all(test, feature = "testkit"))]
mod publication_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    struct PublicationHolder {
        release: Option<std::sync::mpsc::SyncSender<()>>,
        worker: Option<std::thread::JoinHandle<()>>,
        released: Arc<AtomicBool>,
    }

    impl PublicationHolder {
        fn start(table: Arc<crate::bindings::BindingTable>) -> Self {
            let (entered, locked) = std::sync::mpsc::sync_channel(1);
            let (release, resume) = std::sync::mpsc::sync_channel(1);
            let released = Arc::new(AtomicBool::new(false));
            let flag = released.clone();
            let worker = std::thread::spawn(move || {
                table
                    .policies
                    .hold_wait_publication_for_test(entered, resume, &flag);
            });
            let holder = Self {
                release: Some(release),
                worker: Some(worker),
                released,
            };
            locked.recv_timeout(Duration::from_secs(5)).unwrap();
            holder
        }

        fn assert_held(&self) {
            assert!(
                !self.released.load(Ordering::Acquire),
                "outer policy wait must not block Tokio on publication"
            );
        }
    }

    impl Drop for PublicationHolder {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.try_send(());
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    async fn proven_relay() -> (tempfile::TempDir, RelayState, Inputs, CallerGuard) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r13-part-c/publication-unit-runs");
        std::fs::create_dir_all(&root).unwrap();
        let home = tempfile::tempdir_in(root).unwrap();
        let store = Arc::new(crate::accounts::AccountStore::acquire(home.path()).unwrap());
        let identity = crate::accounts::Identity::Codex {
            account_id: "synthetic-publication-account".into(),
            email: "publication@example.invalid".into(),
            user_id: None,
        };
        let account = crate::accounts::Account {
            schema: 1,
            id: identity.id(),
            provider: Provider::Codex,
            label: "Synthetic publication account".into(),
            email: "publication@example.invalid".into(),
            plan: Some("plus".into()),
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
        store
            .save(
                &account,
                &crate::accounts::Tokens {
                    access_token: "synthetic-publication-access".into(),
                    refresh_token: "synthetic-publication-refresh".into(),
                    id_token: None,
                },
                None,
            )
            .unwrap();
        let obs = Arc::new(crate::observability::Observability::new(home.path(), None).unwrap());
        let mut config = crate::config::Config::default();
        config.upstream.codex_base = "http://127.0.0.1:9".into();
        config.upstream.anthropic_base = "http://127.0.0.1:9".into();
        let relay = RelayState::new(
            store,
            &config,
            obs,
            Arc::new(crate::debug_pin::DebugPins::default()),
        )
        .unwrap();
        let key = "codex:synthetic-publication-wait";
        relay
            .bindings
            .import(serde_json::json!({
                "schema": 1,
                "bindings": [{"key": key, "account": account.id, "last_used": Utc::now()}],
            }))
            .unwrap();
        // Real origin + real native reducer, not a constructed receipt/interval.
        let origin = relay
            .accounts
            .capture_http_origin(&account.id)
            .await
            .unwrap();
        let at = Utc::now();
        let raw = serde_json::json!({"observed_at": at, "windows": [
            {"name":"5h", "used_pct":100.0, "observed_at":at,
                "reset_at":at + chrono::Duration::seconds(45)},
            {"name":"7d", "used_pct":20.0, "observed_at":at,
                "reset_at":at + chrono::Duration::days(1)}
        ]});
        relay
            .accounts
            .apply_quota_snapshot_reported_origin(
                &account.id,
                raw.clone(),
                &raw,
                None,
                Some(origin),
            )
            .await
            .unwrap();
        let operation = IssueOperation::new(Instant::now() + Duration::from_secs(120));
        let caller = operation.caller_guard();
        let inputs = Inputs {
            provider: Provider::Codex,
            key: key.into(),
            model: None,
            source: account.id,
            operation,
        };
        (home, relay, inputs, caller)
    }

    enum Exit {
        FirstReceiptWake,
        Wake,
        Deadline,
        Drain,
    }

    async fn outer_publication_busy(exit: Exit) {
        let (_home, relay, inputs, caller) = proven_relay().await;
        let mut gate = relay.hold_next_http_quota_wait_result(&inputs.key, &inputs.source);
        let mut events = relay.subscribe_http_quota_wait();
        let mut inspections = relay.subscribe_http_quota_wait_inspections();
        let mut waiting = Box::pin(wait_http_quota(
            &relay,
            inputs.provider,
            &inputs.key,
            None,
            &inputs.source,
            &inputs.operation,
            &caller,
        ));
        // Capture the actual first native receipt's immutable timer bounds.
        let bounds = tokio::select! {
            biased;
            _ = &mut waiting => panic!("outer wait ended before genuine receipt delivery"),
            bounds = gate.entered() => bounds.unwrap(),
        };
        assert_eq!(bounds.source, inputs.source);
        assert_eq!(bounds.deadline, inputs.operation.deadline());
        assert!(bounds.wake < bounds.deadline);
        assert!(bounds.reset_at > Utc::now());
        if matches!(&exit, Exit::FirstReceiptWake) {
            // Hold publication AFTER native inspection released its guards but
            // BEFORE delivery. Drive the real post-result policy comparison.
            let holder = PublicationHolder::start(relay.bindings.clone());
            gate.release();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut waiting => panic!("first receipt ended before result policy comparison"),
                    event = inspections.recv() => {
                        let event = event.unwrap();
                        assert_eq!(event.source, inputs.source);
                        if event.stage == "result_policy_check" {
                            break;
                        }
                    },
                }
            }
            holder.assert_held();
            assert!(
                events.try_recv().is_err(),
                "uncompared first receipt must not publish admission"
            );
            tokio::time::advance(bounds.wake.saturating_duration_since(Instant::now())).await;
            let std::task::Poll::Ready(Some(end)) = futures_util::poll!(&mut waiting) else {
                panic!("first receipt policy contention must retain its original wake");
            };
            assert_eq!(end.end, WaitEnd::WallResetPending);
            assert_eq!(end.source, inputs.source);
            assert!(end.native_cache.is_none());
            assert!(!end.reset_reached());
            assert!(
                events.try_recv().is_err(),
                "timer-only receipt must not publish admission"
            );
            holder.assert_held();
            drop(holder);
            return;
        }
        gate.release();
        loop {
            tokio::select! {
                biased;
                _ = &mut waiting => panic!("outer wait ended before actual admission"),
                event = events.recv() => {
                    let event = event.unwrap();
                    assert_eq!(event.source, inputs.source);
                    if event.stage == "admitted" {
                        break;
                    }
                },
            }
        }
        while inspections.try_recv().is_ok() {}
        let holder = PublicationHolder::start(relay.bindings.clone());
        // Native account event wakes the real outer engine; no direct callback
        // or helper invocation substitutes for its pre-inspection policy check.
        relay.accounts.set_refresh_attention(&inputs.source, true);
        let first = futures_util::poll!(&mut waiting);
        // A read()-instead-of-try_read() mutant blocks this very poll. The safety
        // holder eventually releases it; this marker then rejects that fallback.
        holder.assert_held();
        assert!(
            first.is_pending(),
            "busy publication must not terminate or hand off the wait"
        );
        assert!(
            inspections.try_recv().is_err(),
            "busy comparison must not start another inspection"
        );
        assert!(
            events.try_recv().is_err(),
            "busy comparison must not readmit optional waiting"
        );
        match exit {
            Exit::FirstReceiptWake => unreachable!("first receipt handled before admission"),
            Exit::Wake => {
                tokio::time::advance(bounds.wake.saturating_duration_since(Instant::now())).await;
                let end = waiting.await.unwrap();
                // Paused Tokio advances only monotonic time; original wall reset
                // remains future. Never extend wake or fabricate a reset hint.
                assert_eq!(end.end, WaitEnd::WallResetPending);
                assert_eq!(end.source, inputs.source);
                assert!(end.native_cache.is_none());
                assert!(!end.reset_reached());
                holder.assert_held();
            }
            Exit::Deadline => {
                tokio::time::advance(bounds.deadline.saturating_duration_since(Instant::now()))
                    .await;
                let end = waiting.await.unwrap();
                assert_eq!(end.end, WaitEnd::Revoked);
                assert!(end.native_cache.is_none());
                holder.assert_held();
                assert!(inputs.operation.check().is_err());
            }
            Exit::Drain => {
                let mut drain = Box::pin(relay.begin_drain());
                let first = futures_util::poll!(&mut drain);
                assert!(
                    first.is_pending(),
                    "real drain must wait for held publication"
                );
                assert!(relay.is_draining());
                let end = waiting.await.unwrap();
                assert_eq!(end.end, WaitEnd::Revoked);
                assert!(end.native_cache.is_none());
                holder.assert_held();
                drop(holder);
                drain.await.unwrap();
                return;
            }
        }
        drop(holder);
    }

    #[tokio::test(start_paused = true)]
    async fn n5c_outer_first_receipt_publication_busy_preserves_original_wake() {
        outer_publication_busy(Exit::FirstReceiptWake).await;
    }

    #[tokio::test(start_paused = true)]
    async fn n5c_outer_publication_busy_preserves_original_wake() {
        outer_publication_busy(Exit::Wake).await;
    }

    #[tokio::test(start_paused = true)]
    async fn n5c_outer_publication_busy_preserves_original_deadline() {
        outer_publication_busy(Exit::Deadline).await;
    }

    #[tokio::test(start_paused = true)]
    async fn n5c_outer_publication_busy_services_real_drain() {
        outer_publication_busy(Exit::Drain).await;
    }
}
