//! Model membership and display facts; no fetching or placement authority.
use super::*;

#[cfg(test)]
mod tests;

/// Projection only; never persist its reducer output as an observed account.
pub(crate) struct ModelQuotaStatus {
    pub window: Option<String>,
    /// Operational cooldown deadline, which may include a local fallback.
    pub reset_at: DateTime<Utc>,
    /// Observed provider reset only; absence must remain null in notices.
    pub observed_reset_at: Option<DateTime<Utc>>,
    pub model_limited: bool,
}

impl AccountRuntime {
    /// Unmanaged Codex traffic historically admitted unknown catalogs. Managed
    /// launches retain their separate, strict catalog validation at the caller.
    pub(crate) fn model_entitled_accounts(
        &self,
        accounts: &[Account],
        provider: Provider,
        model: Option<&str>,
    ) -> BTreeSet<String> {
        self.model_entitled_accounts_with_incumbent(accounts, provider, model, None)
    }

    /// Drain excludes new assignments and catalog publication, not an already
    /// accepted conversation. Native admission still validates that incumbent.
    pub(crate) fn model_entitled_accounts_with_incumbent(
        &self,
        accounts: &[Account],
        provider: Provider,
        model: Option<&str>,
        incumbent: Option<&str>,
    ) -> BTreeSet<String> {
        let Some(model) = model else {
            return accounts
                .iter()
                .filter(|account| account.provider == provider)
                .map(|account| account.id.clone())
                .collect();
        };
        let ids: Vec<_> = accounts
            .iter()
            .filter(|account| {
                account.provider == provider
                    && (crate::relay::catalog_assignable(account)
                        || (incumbent == Some(account.id.as_str())
                            && account.user_state == UserState::Active
                            && account.drain
                            && !(provider == Provider::Claude
                                && account.plan.as_deref() == Some("free"))
                            && !matches!(
                                account.health,
                                Health::NeedsRelogin { .. } | Health::PlanWithoutClaudeCode
                            )))
            })
            .map(|account| account.id.clone())
            .collect();
        let Ok(entitled) = self.catalogs.entitled_accounts(&ids, model, true) else {
            return BTreeSet::new();
        };
        if provider != Provider::Claude {
            return entitled;
        }
        for id in &ids {
            let Ok(view) = self.catalogs.view(id) else {
                return BTreeSet::new();
            };
            // Check the raw known membership before denial overrides. If a model
            // exists but its only supporting account was rejected, that is not
            // evidence that the provider's catalog lags a newly released model.
            if view
                .observation
                .last_known()
                .is_some_and(|known| known.supported().contains(model))
            {
                return entitled;
            }
        }
        ids.into_iter()
            .filter_map(|id| match self.catalogs.is_model_denied(&id, model) {
                Ok(false) => Some(Ok(id)),
                Ok(true) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap_or_default()
    }

    /// Display-only recovery time, never selection authority. The existing
    /// reducer evaluates cached windows for the exact requested model; native
    /// request holds and volatile holds retain their own deadlines.
    pub(crate) fn model_limited_until(
        &self,
        account: &Account,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let effective = self.model_quota_projection(account, model, now);
        self.model_limited_until_projected(account, &effective, model, now)
    }

    fn model_limited_until_projected(
        &self,
        account: &Account,
        effective: &Account,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let hold = |health: &Health| match health {
            Health::Cooling { until, scope, .. }
                if *until > now
                    && (scope == "account"
                        || scope
                            .strip_prefix("model:")
                            .is_some_and(|m| model == Some(m))) =>
            {
                Some(*until)
            }
            _ => None,
        };
        let volatile = self
            .volatile
            .lock()
            .ok()
            .and_then(|entries| entries.get(&account.id).and_then(hold));
        hold(&account.health)
            .into_iter()
            .chain(hold(&effective.health))
            .chain(volatile)
            .chain(account.payment_until.filter(|until| *until > now))
            .max()
    }

    pub(crate) fn model_quota_status(
        &self,
        account: &Account,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<ModelQuotaStatus> {
        let effective = self.model_quota_projection(account, model, now);
        let quota_scope = |health: &Health| match health {
            Health::Cooling {
                reason: HealthReason::Quota,
                until,
                scope,
            } if *until > now
                && (scope == "account"
                    || scope
                        .strip_prefix("model:")
                        .is_some_and(|m| model == Some(m))) =>
            {
                Some(scope.starts_with("model:"))
            }
            _ => None,
        };
        let projected_scope = quota_scope(&effective.health);
        let volatile_scope = if projected_scope.is_some() || effective.eligible_at(model, now) {
            self.volatile
                .lock()
                .ok()
                .and_then(|entries| entries.get(&account.id).and_then(quota_scope))
        } else {
            None
        };
        if projected_scope.is_none() && volatile_scope.is_none() {
            return None;
        }
        let evidence = projected_scope
            .and_then(|_| effective.quota_cooling.as_ref())
            .filter(|quota| quota.reset_at > now && quota.started_at <= now);
        // The existing observation fence checks account/health/deadline, but
        // not model scope. Never lend another model's native reset to this one.
        let native_observed =
            quota_scope(&account.health).and_then(|_| self.observed_quota_reset(account));
        Some(ModelQuotaStatus {
            window: evidence.map(|quota| quota.window.clone()),
            reset_at: self.model_limited_until_projected(account, &effective, model, now)?,
            observed_reset_at: native_observed
                .into_iter()
                .chain(evidence.map(|quota| quota.reset_at))
                .max(),
            model_limited: projected_scope == Some(true) || volatile_scope == Some(true),
        })
    }

    fn model_quota_projection(
        &self,
        account: &Account,
        model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Account {
        let mut effective = account.clone();
        // Match selection's reconstruction only when native health permits this
        // model. An explicit native hold must never be erased by a cached poll.
        if account.eligible_at(model, now)
            && let Some(mut snapshot) =
                self.selection_quota(account, self.obs.quota_snapshot(&account.id))
        {
            snapshot["signals"] = Value::Null;
            effective.health = Health::Ready;
            effective.quota_cooling = None;
            effective.apply_health_signal(
                crate::accounts::HealthSignal::UsageSnapshot {
                    snapshot,
                    model: model.map(str::to_owned),
                    projected_allowance: 0.0,
                },
                now,
            );
        }
        effective
    }
}
