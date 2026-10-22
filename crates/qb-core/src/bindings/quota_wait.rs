//! Conservative D26 attribution, independent of selection and wait budgets.
//! C7's <=1440-minute ordering slot is not proof of a literal five-hour limit.

use crate::accounts::{Account, Health, HealthReason, Provider, UserState};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::collections::BTreeSet;

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str()?.parse().ok()
}

/// Only a currently applicable, observed, exclusively 5h quota hold qualifies.
/// Unknown/partial evidence is not permission to wait. This never changes C5.
/// This result is classification, NOT causal authority: the runtime must also
/// validate an origin captured before the actual send and current invalidation
/// generation. Neither observed_at nor persisted QuotaCooling can replace it.
/// The caller supplies a bounded, complete, unmerged normalized raw report.
pub(crate) fn short_quota_reset(
    account: &Account,
    quota: Option<&Value>,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if account.user_state != UserState::Active
        || account.drain
        || account.auth_probe_pending
        || account.auth_failures > 0
        || account.payment_until.is_some_and(|until| until > now)
    {
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
    if *until <= now
        || !(scope == "account"
            || scope
                .strip_prefix("model:")
                .is_some_and(|blocked| model == Some(blocked)))
    {
        return None;
    }
    let evidence = account.quota_cooling.as_ref()?;
    // C5 merges deadlines independently from window evidence. Never shorten a
    // later authoritative hold, or turn an unlabelled reset into a 5h one.
    if evidence.window != "5h" || evidence.reset_at != *until || evidence.started_at > now {
        return None;
    }
    let quota = quota?;
    if quota["signals"]["serving_from_credits"] == true
        || quota["signals"]["active_limit"] == "credits"
        || (!crate::credits::serving_ended(quota)
            && quota["paid_serving_observed_at"].as_str().is_some())
        || (account.allow_credits
            && account.quota_credit_releasable
            && crate::credits::entitled(quota))
    {
        return None;
    }
    let observed = timestamp(&quota["observed_at"])?;
    if observed > now
        || now.signed_duration_since(observed) > Duration::seconds(300)
        || observed < evidence.started_at
    {
        return None;
    }
    // These named claims explicitly mean 5h. Slot aliases do not: normalization
    // does not retain which physical slot supplied a duration-named window.
    for key in ["active_limit", "representative_claim"] {
        if !quota["signals"][key].is_null()
            && !matches!(quota["signals"][key].as_str(), Some("5h" | "five_hour"))
        {
            return None;
        }
    }
    let windows = quota["windows"].as_array()?;
    let mut names = BTreeSet::new();
    let mut account_windows = 0;
    let mut short = None;
    let mut weekly_present = false;
    for window in windows {
        let name = window["name"].as_str()?;
        if !names.insert(name)
            || matches!(name, "primary" | "secondary")
            || timestamp(&window["observed_at"])? != observed
        {
            return None;
        }
        let used = window["used_pct"].as_f64()?;
        if !used.is_finite() || used < 0.0 {
            return None;
        }
        if !window["status"].is_null()
            && !matches!(
                window["status"].as_str(),
                Some("allowed" | "allowed_warning" | "rejected" | "true" | "false")
            )
        {
            return None;
        }
        let reset = timestamp(&window["reset_at"])?;
        if reset <= now {
            return None;
        }
        // Normalized observations carry durations in their names. Extra raw
        // duration fields must not contradict the sole accepted short name.
        for (field, expected) in [
            ("window_minutes", 300),
            ("limit_window_seconds", 18_000),
            ("window_seconds", 18_000),
        ] {
            if !window[field].is_null()
                && (name != "5h" || window[field].as_u64() != Some(expected))
            {
                return None;
            }
        }
        let exhausted = used >= 100.0 || window["status"] == "rejected";
        if !super::placement_account_window(window) {
            // A model sub-bucket may be applicable; never reinterpret its name
            // as an account duration or silently discard a possible blocker.
            if !name.contains('_') || exhausted {
                return None;
            }
            continue;
        }
        account_windows += 1;
        weekly_present |= name == "7d";
        if name == "5h" && super::account_window(account.provider, window) == Some(false) {
            if !exhausted || reset != evidence.reset_at {
                return None;
            }
            short = Some(reset);
        } else if exhausted {
            // Includes 7d, Free 43200m, and every nonstandard short slot.
            // A missing/earlier long reset must never hide simultaneous limits.
            return None;
        }
    }
    // A partial one-window observation cannot establish absence of a second
    // provider bucket. Claude's account pair is named; Codex durations vary.
    if account_windows < 2 || (account.provider == Provider::Claude && !weekly_present) {
        return None;
    }
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::QuotaCooling;
    use serde_json::json;

    fn fixture() -> (Account, Value, DateTime<Utc>) {
        let now = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
        let reset = now + Duration::minutes(1);
        let mut account = crate::accounts::health::tests::fixture();
        account.health = Health::Cooling {
            reason: HealthReason::Quota,
            until: reset,
            scope: "account".into(),
        };
        account.quota_cooling = Some(QuotaCooling {
            window: "5h".into(),
            reset_at: reset,
            started_at: now - Duration::seconds(1),
        });
        let quota = json!({"observed_at":now,"windows":[
            {"name":"5h","used_pct":100.,"observed_at":now,"reset_at":reset},
            {"name":"7d","used_pct":20.,"observed_at":now,"reset_at":now+Duration::days(1)}
        ]});
        (account, quota, now)
    }

    #[test]
    fn r13_quota_wait_explicit_5h_for_both_providers() {
        let (mut account, quota, now) = fixture();
        for provider in [Provider::Codex, Provider::Claude] {
            account.provider = provider;
            assert_eq!(
                short_quota_reset(&account, Some(&quota), None, now),
                Some(now + Duration::minutes(1))
            );
        }
    }

    #[test]
    fn r13_quota_wait_weekly_and_simultaneous_limits_never_short() {
        let (account, mut quota, now) = fixture();
        quota["windows"][1]["used_pct"] = json!(100.);
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
        // Even a weekly reset before the 5h deadline is still another blocker.
        quota["windows"][1]["reset_at"] = json!(now + Duration::seconds(30));
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
        quota["windows"][0]["used_pct"] = json!(10.);
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
    }

    #[test]
    fn r13_quota_wait_free_30d_cannot_be_the_short_limit() {
        let (mut account, mut quota, now) = fixture();
        account.plan = Some("free".into());
        quota["windows"][1]["name"] = json!("43200m");
        assert!(short_quota_reset(&account, Some(&quota), None, now).is_some());
        quota["windows"][1]["used_pct"] = json!(100.);
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
    }

    #[test]
    fn r13_quota_wait_alias_and_nonstandard_short_slot_are_not_5h() {
        let (account, quota, now) = fixture();
        for name in ["primary", "secondary", "observed", "60m", "1440m", "300m"] {
            let mut candidate = quota.clone();
            candidate["windows"][0]["name"] = json!(name);
            assert_eq!(
                short_quota_reset(&account, Some(&candidate), None, now),
                None,
                "{name}"
            );
        }
        let mut candidate = quota;
        candidate["signals"]["active_limit"] = json!("primary");
        assert_eq!(
            short_quota_reset(&account, Some(&candidate), None, now),
            None
        );
    }

    #[test]
    fn r13_quota_wait_model_scope_requires_exact_current_model() {
        let (mut account, quota, now) = fixture();
        if let Health::Cooling { scope, .. } = &mut account.health {
            *scope = "model:synthetic".into();
        }
        assert!(short_quota_reset(&account, Some(&quota), Some("synthetic"), now).is_some());
        assert_eq!(
            short_quota_reset(&account, Some(&quota), Some("other"), now),
            None
        );
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
    }

    #[test]
    fn r13_quota_wait_exhausted_subbucket_never_hidden() {
        let (account, mut quota, now) = fixture();
        quota["windows"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"7d_oi",
            "used_pct":100.,"reset_at":now+Duration::hours(2),"observed_at":now}));
        assert_eq!(
            short_quota_reset(&account, Some(&quota), Some("synthetic"), now),
            None
        );
    }

    #[test]
    fn r13_quota_wait_preserves_later_c5_deadline() {
        let (mut account, quota, now) = fixture();
        if let Health::Cooling { until, .. } = &mut account.health {
            *until = now + Duration::days(1);
        }
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
    }

    #[test]
    fn r13_quota_wait_expired_inactive_and_nonquota_hold_rejected() {
        let (account, quota, now) = fixture();
        assert_eq!(
            short_quota_reset(&account, Some(&quota), None, now + Duration::minutes(1)),
            None
        );
        let mut inactive = account.clone();
        inactive.user_state = UserState::Paused;
        assert_eq!(short_quota_reset(&inactive, Some(&quota), None, now), None);
        let mut other = account;
        if let Health::Cooling { reason, .. } = &mut other.health {
            *reason = HealthReason::Rate;
        }
        assert_eq!(short_quota_reset(&other, Some(&quota), None, now), None);
    }

    #[test]
    fn r13_quota_wait_stale_partial_and_duplicate_evidence_rejected() {
        let (account, quota, now) = fixture();
        for (field, value) in [
            ("observed_at", json!(now - Duration::seconds(301))),
            ("observed_at", json!(now + Duration::seconds(1))),
        ] {
            let mut candidate = quota.clone();
            candidate[field] = value;
            assert_eq!(
                short_quota_reset(&account, Some(&candidate), None, now),
                None
            );
        }
        let mut partial = quota.clone();
        partial["windows"].as_array_mut().unwrap().pop();
        assert_eq!(short_quota_reset(&account, Some(&partial), None, now), None);
        let mut mixed = quota.clone();
        mixed["windows"][1]["observed_at"] = json!(now - Duration::seconds(1));
        assert_eq!(short_quota_reset(&account, Some(&mixed), None, now), None);
        let mut duplicate = quota.clone();
        duplicate["windows"]
            .as_array_mut()
            .unwrap()
            .push(quota["windows"][0].clone());
        assert_eq!(
            short_quota_reset(&account, Some(&duplicate), None, now),
            None
        );
        assert_eq!(short_quota_reset(&account, None, None, now), None);
    }

    #[test]
    fn r13_quota_wait_duration_contradictions_and_credits_serving_rejected() {
        let (account, quota, now) = fixture();
        let mut contradictory = quota.clone();
        contradictory["windows"][0]["window_minutes"] = json!(10080);
        assert_eq!(
            short_quota_reset(&account, Some(&contradictory), None, now),
            None
        );
        for signals in [
            json!({"serving_from_credits":true}),
            json!({"active_limit":"credits"}),
        ] {
            let mut paid = quota.clone();
            paid["signals"] = signals;
            assert_eq!(short_quota_reset(&account, Some(&paid), None, now), None);
        }
    }

    #[test]
    fn r13_quota_wait_rejected_weekly_even_below_100_and_without_reset() {
        let (account, mut quota, now) = fixture();
        quota["windows"][1]["status"] = json!("rejected");
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
        quota["windows"][1]["reset_at"] = Value::Null;
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
    }

    #[test]
    fn r13_quota_wait_ungrounded_or_mismatching_causing_record_rejected() {
        let (account, quota, now) = fixture();
        for name in ["primary", "observed", "7d"] {
            let mut changed = account.clone();
            changed.quota_cooling.as_mut().unwrap().window = name.into();
            assert_eq!(short_quota_reset(&changed, Some(&quota), None, now), None);
        }
        let mut changed = account.clone();
        changed.quota_cooling = None;
        assert_eq!(short_quota_reset(&changed, Some(&quota), None, now), None);
        let mut changed = quota;
        changed["windows"][0]["reset_at"] = json!(now + Duration::seconds(30));
        assert_eq!(short_quota_reset(&account, Some(&changed), None, now), None);
    }

    #[test]
    fn r13_quota_wait_credit_entitlement_does_not_invent_a_paid_hold() {
        let (mut account, mut quota, now) = fixture();
        account.allow_credits = true;
        quota["credits"] = json!({"has_credits":true});
        assert!(short_quota_reset(&account, Some(&quota), None, now).is_some());
        account.quota_credit_releasable = true;
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
    }

    #[test]
    fn r13_quota_wait_freshness_boundary_is_not_a_causal_fence() {
        let (mut account, mut quota, now) = fixture();
        account.quota_cooling.as_mut().unwrap().started_at = now - Duration::seconds(302);
        let observed = now - Duration::seconds(300);
        quota["observed_at"] = json!(observed);
        for window in quota["windows"].as_array_mut().unwrap() {
            window["observed_at"] = json!(observed);
        }
        assert!(short_quota_reset(&account, Some(&quota), None, now).is_some());
        let stale = now - Duration::seconds(301);
        quota["observed_at"] = json!(stale);
        for window in quota["windows"].as_array_mut().unwrap() {
            window["observed_at"] = json!(stale);
        }
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
        // Even the accepted boundary above still needs a live registry origin.
    }

    #[test]
    fn r13_quota_wait_delayed_apply_is_conservatively_unavailable() {
        let (mut account, mut quota, now) = fixture();
        account.quota_cooling.as_mut().unwrap().started_at = now;
        let earlier = now - Duration::nanoseconds(1);
        quota["observed_at"] = json!(earlier);
        for window in quota["windows"].as_array_mut().unwrap() {
            window["observed_at"] = json!(earlier);
        }
        // Retain the donor's rejection filter. Do not backdate either timestamp
        // to make a causally valid but delayed report pass this narrow slice.
        assert_eq!(short_quota_reset(&account, Some(&quota), None, now), None);
    }
}
