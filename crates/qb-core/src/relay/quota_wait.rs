//! Revocable proof-backed HTTP waiting, never send authority.
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
    Revoked,
    WallResetPending,
}

pub(super) struct WaitHint {
    pub(super) source: String,
    pub(super) end: WaitEnd,
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
    let deadline = operation.deadline();
    let mut account_events = relay.accounts.subscribe();
    // Infer the crate-visible receipt returned through the runtime API; the
    // private wait_owner module need not be exposed to this relay child.
    let mut receipt = None;
    // Once set, this envelope is immutable even after replacement proof tickets.
    let mut envelope: Option<(Instant, DateTime<Utc>)> = None;

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
        let accounts = relay.accounts.clone();
        let bindings = relay.bindings.clone();
        let work_inputs = inputs.clone();
        let previous = receipt.take();
        let task = tokio::task::spawn_blocking(move || {
            if !caller_live(
                &work_inputs.operation,
                work_inputs.operation.deadline(),
                false,
            ) {
                return Ok(None);
            }
            match previous {
                None => accounts.current_http_quota_wait(&bindings, work_inputs.request()),
                Some(old) => {
                    accounts.renew_current_http_quota_wait(&bindings, work_inputs.request(), old)
                }
            }
        });
        tokio::pin!(task);
        let inspected = tokio::select! {
            biased;
            _ = relay.wait_for_drain_start() => return stopped(source, envelope.is_some()),
            _ = tokio::time::sleep_until(deadline) => return stopped(source, envelope.is_some()),
            result = &mut task => result,
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
                return envelope.map(|_| WaitHint {
                    source: inputs.source.clone(),
                    end: WaitEnd::Revoked,
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
            });
        }
        receipt = Some(current);
        #[cfg(feature = "testkit")]
        if first {
            relay.observe_http_quota_wait("admitted", source);
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
                            return Some(WaitHint { source: inputs.source.clone(), end: WaitEnd::Revoked });
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
}
