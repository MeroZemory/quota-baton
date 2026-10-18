//! Grounded notification context. No placement, token reload or network I/O.
use super::*;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str()?.parse().ok()
}

pub(crate) fn limiting_window(
    account: &Account,
    snapshot: &Value,
) -> (Option<String>, Option<DateTime<Utc>>) {
    let now = Utc::now();
    if let Some(evidence) = &account.quota_cooling
        && evidence.reset_at > now
    {
        return (Some(evidence.window.clone()), Some(evidence.reset_at));
    }
    let active = snapshot["signals"]["active_limit"]
        .as_str()
        .or(snapshot["signals"]["representative_claim"].as_str());
    let matches = |name: &str| {
        active.is_some_and(|a| {
            a == name
                || matches!(
                    (a, name),
                    ("primary" | "five_hour", "5h")
                        | ("secondary" | "seven_day", "7d")
                        | ("seven_day_opus", "7d_oi")
                )
        })
    };
    let window = snapshot["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| timestamp(&w["reset_at"]).is_none_or(|r| r > now))
        .filter(|w| {
            timestamp(&w["observed_at"])
                .is_some_and(|t| t <= now && now.signed_duration_since(t).num_seconds() <= 300)
        })
        .filter(|w| {
            matches(w["name"].as_str().unwrap_or_default())
                || w["used_pct"].as_f64().is_some_and(|p| p >= 100.0)
                || w["status"] == "rejected"
        })
        .max_by_key(|w| {
            (
                matches(w["name"].as_str().unwrap_or_default()),
                timestamp(&w["reset_at"]),
            )
        });
    (
        window.and_then(|w| w["name"].as_str()).map(str::to_owned),
        window.and_then(|w| timestamp(&w["reset_at"])),
    )
}

pub(crate) fn model_quota_applies(account: &Account, model: Option<&str>) -> bool {
    matches!(&account.health, Health::Cooling { reason: HealthReason::Quota, until, scope }
        if *until > Utc::now() && scope.strip_prefix("model:").is_some_and(|blocked| model == Some(blocked)))
}

pub(crate) fn quota_applies(account: &Account, model: Option<&str>) -> bool {
    matches!(&account.health, Health::Cooling { reason: HealthReason::Quota, until, scope }
        if *until > Utc::now() && (scope == "account" || scope.strip_prefix("model:").is_some_and(|blocked| model == Some(blocked))))
}

/// Latest real blocking window per account; callers take the provider minimum.
pub(crate) fn limiting_reset(
    account: &Account,
    snapshot: &Value,
    model: Option<&str>,
    observed: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    if !quota_applies(account, model) {
        return None;
    }
    let now = Utc::now();
    let scoped = model_quota_applies(account, model);
    let windows = snapshot["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| {
            w["name"]
                .as_str()
                .is_some_and(|name| name.contains('_') == scoped)
        })
        .filter(|w| {
            w["used_pct"].as_f64().is_some_and(|used| used >= 100.0) || w["status"] == "rejected"
        })
        .filter(|w| {
            timestamp(&w["observed_at"])
                .is_some_and(|t| t <= now && now.signed_duration_since(t).num_seconds() <= 300)
        })
        .filter_map(|w| timestamp(&w["reset_at"]));
    windows
        .chain(limiting_window(account, snapshot).1)
        .chain(observed)
        .filter(|reset| *reset > now)
        .max()
}

impl RelayState {
    /// Called after C7 resolved a quota failure and actual placement returned.
    /// A missing selection is exhaustion only if the provider truly has no room.
    pub(super) async fn notice_quota(
        &self,
        id: &str,
        selected: Option<&Account>,
        model: Option<&str>,
    ) {
        let store = self.store.clone();
        let Ok(Ok((accounts, credential_ids))) = tokio::task::spawn_blocking(move || {
            let accounts = store.list()?;
            let ids: BTreeSet<_> = accounts
                .iter()
                .filter(|a| store.tokens(a).is_ok_and(|t| !t.access_token.is_empty()))
                .map(|a| a.id.clone())
                .collect();
            Ok::<_, crate::accounts::StoreError>((accounts, ids))
        })
        .await
        else {
            return;
        };
        let Some(account) = accounts.iter().find(|a| a.id == id) else {
            return;
        };
        let now = Utc::now();
        if account.user_state != UserState::Active
            || account.drain
            || !quota_applies(account, model)
        {
            return;
        }
        let eligible = |a: &Account| {
            a.provider == account.provider
                && a.eligible_at(model, now)
                && credential_ids.contains(&a.id)
                && !self.accounts.probing(&a.id)
                && !self.accounts.blocked_in_memory(&a.id)
                && !a.auth_probe_pending
                && a.auth_failures == 0
        };
        // Identity comes only from the production selection, including refreshed
        // pacing, reconnect exclusions, tried accounts, pins and reservations.
        let next = selected.and_then(|selected| {
            accounts
                .iter()
                .find(|a| a.id == selected.id && a.id != id && eligible(a))
        });
        if next.is_none() && accounts.iter().any(eligible) {
            return;
        }
        let snapshot = self.observability.quota_snapshot(id).unwrap_or_default();
        let (window, mut reset_at) = limiting_window(account, &snapshot);
        reset_at = reset_at.or_else(|| self.accounts.observed_quota_reset(account));
        let mut earliest_account = None;
        let mut model_limited = model_quota_applies(account, model);
        if next.is_none() {
            let earliest = accounts
                .iter()
                .filter(|a| {
                    a.provider == account.provider && a.user_state == UserState::Active && !a.drain
                })
                .filter_map(|a| {
                    let snapshot = self.observability.quota_snapshot(&a.id).unwrap_or_default();
                    limiting_reset(a, &snapshot, model, self.accounts.observed_quota_reset(a))
                        .map(|reset| (a.id.clone(), reset))
                })
                .min_by_key(|(_, reset)| *reset);
            reset_at = earliest.as_ref().map(|(_, reset)| *reset);
            earliest_account = earliest.map(|(id, _)| id);
            model_limited |= accounts.iter().any(|a| {
                a.provider == account.provider
                    && a.user_state == UserState::Active
                    && !a.drain
                    && model_quota_applies(a, model)
            });
        }
        self.accounts.set_quota_notice(
            account,
            json!({"action":if next.is_some(){"failover"}else{"exhausted"},
            "window":window,"reset_at":reset_at,"next_account":next.map(|a| &a.id),
            "earliest_account":earliest_account,"observed_at":now,"model_limited":model_limited}),
            model,
        );
    }
}
