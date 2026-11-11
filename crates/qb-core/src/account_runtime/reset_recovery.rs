//! A confirmed Claude redemption may clear its exact native window cause.
//! Authority remains the pre-POST wait origin, accepted under the Store writer.
use super::*;

pub(crate) struct ResetQuotaObservation {
    pub(crate) expected: Account,
    pub(crate) cleared: BTreeSet<String>,
    /// Canonical quota snapshot, published by the incumbent Claude observer.
    pub(crate) snapshot: Value,
    /// Actual fresh GET facts, before any cache merge.
    pub(crate) reported: Value,
    pub(crate) wait_origin: Option<ReportOrigin>,
    pub(crate) outside_origin: Option<outside_owner::PollOrigin>,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) complete: bool,
}

pub(super) struct ResetRecovery {
    expected: Account,
    cleared: BTreeSet<String>,
    started_at: DateTime<Utc>,
    complete: bool,
}

impl ResetRecovery {
    pub(super) fn can_recover(
        &self,
        current: &Account,
        reported: &Value,
        now: DateTime<Utc>,
    ) -> bool {
        if !self.complete
            || current != &self.expected
            || current.provider != Provider::Claude
            || !matches!(current.user_state, UserState::Active | UserState::Paused)
            || current.plan.as_deref() == Some("free")
            || current.payment_until.is_some_and(|until| until > now)
            || !matches!(&current.health, Health::Cooling { reason: HealthReason::Quota, until, .. } if *until > now)
            || self.started_at > now
        {
            return false;
        }
        let Some(cause) = &current.quota_cooling else {
            return false;
        };
        if !matches!(cause.window.as_str(), "5h" | "7d" | "7d_oi" | "7d_sonnet")
            || !self.cleared.contains(&cause.window)
            || self.started_at <= cause.started_at
        {
            return false;
        }
        let Some(windows) = reported["windows"].as_array() else {
            return false;
        };
        let mut causing = windows
            .iter()
            .filter(|window| window["name"] == cause.window);
        let Some(window) = causing.next() else {
            return false;
        };
        if causing.next().is_some() {
            return false;
        }
        let observed = window["observed_at"]
            .as_str()
            .and_then(|s| s.parse::<DateTime<Utc>>().ok());
        observed.is_some_and(|at| {
            at > cause.started_at
                && at > self.started_at
                && at <= now
                && now.signed_duration_since(at) < Duration::seconds(300)
        }) && window["used_pct"]
            .as_f64()
            .is_some_and(|used| used.is_finite() && (0.0..100.0).contains(&used))
            && window["status"] != "rejected"
    }
}

impl AccountRuntime {
    pub(crate) async fn apply_reset_quota_snapshot(
        &self,
        observation: ResetQuotaObservation,
    ) -> Result<Account, ()> {
        let ResetQuotaObservation {
            expected,
            cleared,
            snapshot,
            reported,
            wait_origin,
            outside_origin,
            started_at,
            complete,
        } = observation;
        let id = expected.id.clone();
        self.apply_quota_snapshot_reported_inner(
            &id,
            snapshot,
            &reported,
            None,
            (wait_origin, wait_owner::ReportPublication::Immediate),
            Some(outside_owner::PollApplication {
                origin: outside_origin,
            }),
            None,
            Some(ResetRecovery {
                expected,
                cleared,
                started_at,
                complete,
            }),
        )
        .await
        .map(|(account, _, _, applied)| {
            if applied && complete {
                self.observe_polled_placement_capacity(&account, &reported, started_at);
            }
            account
        })
    }
}

#[cfg(test)]
mod tests;
