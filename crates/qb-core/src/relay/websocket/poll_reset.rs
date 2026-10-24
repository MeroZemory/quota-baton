//! One bounded C9 operation for an original physical WS quota rejection.
use super::{pump::QuotaCandidateSnapshot, ws_attempt::HandoffLiveness};
use crate::{
    bindings::HttpWaitRequest,
    issue_operation::{CallerGuard, IssueOperation},
    launch_tokens::LaunchOwnerScope,
    relay::{RelayState, native_reset::PendingPublication},
};
use std::sync::Arc;
use tokio::time::Instant;

// Inputs are captured with the physically forwarded create, not read from
// mutable pump model/session state after the poll. None always preserves the
// original quota error and the established far-wait move fallback.
pub(super) struct PendingWsPublication {
    publication: PendingPublication,
    liveness: HandoffLiveness,
    operation: IssueOperation,
    caller: CallerGuard,
    owner: LaunchOwnerScope,
}
impl PendingWsPublication {
    // The caller serializes the final local client-write handoff. This physical
    // veto cannot create authority; dropped publication cancels its record.
    pub(super) fn keep_if_current(self) -> bool {
        let Self {
            publication,
            liveness,
            operation,
            caller,
            owner,
        } = self;
        if !operation.can_reply() || !owner.live() {
            return false;
        }
        liveness
            .with_live(|| {
                publication.keep();
                caller.disarm();
            })
            .is_some()
    }
}

pub(super) async fn register(
    relay: Arc<RelayState>,
    snapshot: QuotaCandidateSnapshot,
    deadline: Instant,
) -> Option<PendingWsPublication> {
    let dispatch = snapshot.dispatch;
    let account = dispatch.account;
    let root = dispatch.key?;
    let model = dispatch.model?;
    let pin = dispatch.pin?;
    let original_owner = dispatch.owner?;
    let candidate = snapshot.candidate;
    let liveness = candidate.handoff_liveness();
    let operation = IssueOperation::new(deadline);
    let caller = operation.caller_guard();
    if relay.is_draining() || !operation.can_reply() {
        return None;
    }
    let scope = pin.deferred_owner_scope()?;
    if !scope.same_owner(&original_owner) || !original_owner.live() || !scope.matches_pin(&pin) {
        return None;
    }
    // One C9 operation; its existing 401 retry is internal. No forced polling
    // or placement-budget bypass. Partial/healthy/weekly-only has no receipt.
    let (_, receipt) = tokio::select! {
        biased;
        _ = relay.wait_for_drain_start() => return None,
        _ = tokio::time::sleep_until(deadline) => return None,
        result = relay.quota.poll_ws_quota(&account) => result.ok()?,
    };
    let receipt = receipt?;
    let accounts = relay.accounts.clone();
    let bindings = relay.bindings.clone();
    let task_operation = operation.clone();
    let task_accounts = accounts.clone();
    let task_owner = original_owner.clone();
    let task = tokio::task::spawn_blocking(move || {
        // Exact native receipt and actual write witness are rechecked under
        // the existing Store/runtime/binding/policy gates. Cancellation of this
        // async caller revokes operation; the blocking result owns rollback.
        task_accounts
            .register_ws_deferred_reset(
                &bindings,
                HttpWaitRequest {
                    provider: crate::accounts::Provider::Codex,
                    key: Some(&root),
                    model: Some(&model),
                    expected_source: Some(&account),
                    operation: &task_operation,
                },
                &receipt,
                &pin,
                &task_owner,
                candidate,
            )
            .ok()
            .flatten()
            .map(|(id, scope)| PendingPublication::new(task_accounts.clone(), scope, id))
    });
    let publication = tokio::select! {
        biased;
        _ = relay.wait_for_drain_start() => return None,
        _ = tokio::time::sleep_until(deadline) => return None,
        result = task => result.ok()??,
    };
    if relay.is_draining() || !scope.live() || !operation.can_reply() {
        return None;
    }
    Some(PendingWsPublication {
        publication,
        liveness,
        operation,
        caller,
        owner: original_owner,
    })
}

#[cfg(test)]
#[path = "poll_reset_tests.rs"]
mod tests;
