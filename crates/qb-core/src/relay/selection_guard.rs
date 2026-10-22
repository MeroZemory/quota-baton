use std::{future::Future, pin::Pin};

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::{
    sync::{Notify, broadcast, futures::Notified},
    time::Instant,
};

use crate::{accounts::Health, bindings::Candidate, issue_operation::IssueOperation};

// Called under BindingTable::mutations. Cancellation/expiry can still win while
// external admission runs; after admit wins, required publication may finish.
pub(super) fn admit(operation: &IssueOperation, checks: impl FnOnce() -> bool) -> bool {
    operation.check().is_ok() && checks() && operation.admit().is_ok()
}

pub(super) struct Changes<'a> {
    policy: Pin<Box<Notified<'a>>>,
    accounts: broadcast::Receiver<Value>,
}

impl<'a> Changes<'a> {
    pub(super) fn new(policy: &'a Notify, accounts: broadcast::Receiver<Value>) -> Self {
        let mut policy = Box::pin(policy.notified());
        policy.as_mut().enable();
        Self { policy, accounts }
    }

    // true ends the hold; false re-evaluates eligibility and the breaker.
    pub(super) async fn wait(
        &mut self,
        account: Option<&str>,
        wake: Instant,
        deadline: Instant,
        drain: impl Future<Output = ()>,
    ) -> bool {
        tokio::pin!(drain);
        loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => return true,
                _ = &mut drain => return true,
                _ = &mut self.policy => return false,
                event = self.accounts.recv() => match event {
                    Ok(event) if matches!(event["event"].as_str(), Some("account_changed" | "quota_updated"))
                        && account.is_none_or(|id| event["data"]["id"].as_str() == Some(id)) => return false,
                    Ok(_) => {},
                    Err(_) => return false, // lost evidence requires a fresh snapshot
                },
                _ = tokio::time::sleep_until(wake) => return false,
            }
        }
    }
}

pub(super) fn recovery_deadline(
    mut until: DateTime<Utc>,
    candidates: &[Candidate],
    account: Option<&str>,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    // These times only cause re-selection; they never supply usability evidence
    // or authorize a move. Expired facts must not cause a zero-delay retry loop.
    let mut consider = |at: DateTime<Utc>| {
        if at > now {
            until = until.min(at);
        }
    };
    for c in candidates
        .iter()
        .filter(|c| account.is_none_or(|id| c.account.id == id))
    {
        if let Health::Cooling { until, .. } = &c.account.health {
            consider(*until);
        }
        if let Some(until) = c.account.payment_until {
            consider(until);
        }
        if let Some(quota) = &c.account.quota_cooling {
            consider(quota.reset_at);
        }
        if let Some(windows) = c.quota.as_ref().and_then(|q| q["windows"].as_array()) {
            for window in windows {
                if window["used_pct"]
                    .as_f64()
                    .is_some_and(|used| used >= 100.0)
                    && let Some(reset) = window["reset_at"].as_str()
                    && let Ok(reset) = DateTime::parse_from_rfc3339(reset)
                {
                    consider(reset.with_timezone(&Utc));
                }
            }
        }
    }
    until
}

#[cfg(test)]
mod tests;
