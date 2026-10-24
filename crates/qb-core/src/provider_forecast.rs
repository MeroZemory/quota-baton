//! D28 display-only provider facts, reusing D24 account estimates.
//! No IO, clock reads, rate estimation, routing, percentage sums or pooled runway.
use chrono::{DateTime, Utc};

use crate::{
    accounts::{Provider, UserState},
    usage_forecast::{AccountUsageForecast, CreditUsageForecast, ForecastOutcome},
    usage_forecast_display::limiting_window,
};

/// A projection of existing account facts, not a new wire or live-account schema.
/// Supply one provider's unique configured accounts, including inactive accounts.
/// `forecast` must contain only visible account-scoped windows (the existing
/// summary_forecast filtering); suppress it for auth/relogin/store/link errors.
#[derive(Clone, Copy, Debug)]
pub struct ProviderAccount<'a> {
    pub id: &'a str,
    /// Existing display name; used only for stable name/id ordering, never formatted.
    pub name: &'a str,
    pub user_state: UserState,
    pub drain: bool,
    /// Existing pool_eligible == true AND ready AND !probe_pending AND
    /// !refresh_attention. The provider pool count remains authoritative.
    pub usable: bool,
    /// Only an account-scoped cooling deadline, not route/model cooling.
    pub cooling_until: Option<DateTime<Utc>>,
    pub allow_credits: bool,
    /// Actual observed paid serving; permission/entitlement alone is insufficient.
    pub credits_in_use: bool,
    /// Preserve unknown independently of an explicit unlimited entitlement.
    pub credits_unlimited: Option<bool>,
    pub forecast: Option<&'a AccountUsageForecast>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRef {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextExhaustion {
    pub account: AccountRef,
    pub at: DateTime<Utc>,
    pub reset_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionOutlook {
    Unknown,
    NoExhaustionBeforeReset,
    Next(NextExhaustion),
}

#[derive(Clone, Debug, PartialEq)]
pub enum CreditRunway {
    Unknown,
    Unlimited,
    /// Existing monetary forecast, never subscription capacity or a pooled ETA.
    /// NoFiniteForecast remains that outcome; it does not mean unlimited.
    Forecast(CreditUsageForecast),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreditContinuation {
    pub account: AccountRef,
    pub runway: CreditRunway,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeasuredCoverage {
    /// Includes observed exhausted facts even without rate samples.
    pub measured: usize,
    /// Active, non-drained accounts, including cooling and unknown/relogin.
    pub total: usize,
}

/// Intentionally no numeric variant. Calibration and a real 24 h backtest with
/// justified error bounds are prerequisites for any future pooled estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PooledRunway {
    Unavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderSummary {
    pub provider: Provider,
    pub as_of: DateTime<Utc>,
    pub configured: usize,
    pub usable: Option<u64>,
    pub coverage: MeasuredCoverage,
    pub subscription: SubscriptionOutlook,
    pub earliest_recovery: Option<DateTime<Utc>>,
    /// Stable display-name/id order; callers can show the first name and count.
    pub credit_continuations: Vec<CreditContinuation>,
    pub pooled_runway: PooledRunway,
}

fn account_ref(account: &ProviderAccount<'_>) -> AccountRef {
    AccountRef {
        id: account.id.into(),
        name: account.name.into(),
    }
}

fn credit_runway(
    account: &ProviderAccount<'_>,
    forecast: Option<&AccountUsageForecast>,
    now: DateTime<Utc>,
) -> CreditRunway {
    if account.credits_unlimited == Some(true) {
        return CreditRunway::Unlimited;
    }
    // Match D24's five-minute observation freshness without calculating a rate.
    let credit = forecast.and_then(|f| f.credit.as_ref()).filter(|c| {
        let age = now.signed_duration_since(c.observed_at);
        age >= chrono::Duration::zero()
            && age <= chrono::Duration::seconds(300)
            && c.balance.is_finite()
            && c.balance >= 0.0
            && match c.outcome {
                ForecastOutcome::Exhaustion => {
                    c.balance > 0.0 && c.exhaustion_at.is_some_and(|at| at > now)
                }
                ForecastOutcome::Exhausted => c.balance == 0.0,
                ForecastOutcome::NoFiniteForecast => c.balance > 0.0,
                _ => false,
            }
    });
    credit
        .cloned()
        .map(CreditRunway::Forecast)
        .unwrap_or(CreditRunway::Unknown)
}

/// Aggregate a single provider at the caller's explicit time. Observed quota
/// includes outside use already: do not subtract it or substitute ledger rates.
/// The input pool count is authoritative; never infer it from forecast coverage.
pub fn summarize(
    provider: Provider,
    accounts: &[ProviderAccount<'_>],
    eligible: Option<u64>,
    now: DateTime<Utc>,
) -> ProviderSummary {
    let eligible = if accounts.is_empty() {
        Some(0)
    } else {
        eligible
    };
    let mut result = ProviderSummary {
        provider,
        as_of: now,
        configured: accounts.len(),
        usable: eligible,
        coverage: MeasuredCoverage {
            measured: 0,
            total: 0,
        },
        subscription: SubscriptionOutlook::Unknown,
        earliest_recovery: None,
        credit_continuations: Vec::new(),
        pooled_runway: PooledRunway::Unavailable,
    };
    let mut next: Option<NextExhaustion> = None;
    let mut safe_usable = 0u64;
    for account in accounts {
        if account.user_state != UserState::Active || account.drain {
            continue;
        }
        result.coverage.total += 1;
        // Reject a future DTO, but age all other estimates at the caller's time.
        let forecast = account.forecast.filter(|f| f.as_of <= now).map(|f| {
            let mut f = f.clone();
            f.as_of = now;
            f
        });
        let window = forecast.as_ref().and_then(limiting_window);
        result.coverage.measured += usize::from(window.is_some());
        let mut recovery = account.cooling_until.filter(|at| *at > now);
        if let Some(at) = window
            .filter(|w| w.outcome == ForecastOutcome::Exhausted)
            .and_then(|w| w.reset_at)
        {
            recovery = Some(recovery.map_or(at, |previous| previous.max(at)));
        }
        if let Some(at) = recovery {
            result.earliest_recovery = Some(
                result
                    .earliest_recovery
                    .map_or(at, |previous| previous.min(at)),
            );
        }
        if provider == Provider::Codex
            && account.usable
            && account.credits_in_use
            && account.allow_credits
        {
            result.credit_continuations.push(CreditContinuation {
                account: account_ref(account),
                runway: credit_runway(account, forecast.as_ref(), now),
            });
            continue;
        }
        let Some(window) = window.filter(|_| account.usable) else {
            continue;
        };
        if window.outcome == ForecastOutcome::NoExhaustionBeforeReset {
            safe_usable += 1;
        }
        if window.outcome != ForecastOutcome::Exhaustion {
            continue;
        }
        let (Some(at), Some(reset_at)) = (window.exhaustion_at, window.reset_at) else {
            continue;
        };
        if next.as_ref().is_none_or(|previous| {
            (at, account.name, account.id)
                < (
                    previous.at,
                    previous.account.name.as_str(),
                    previous.account.id.as_str(),
                )
        }) {
            next = Some(NextExhaustion {
                account: account_ref(account),
                at,
                reset_at,
            });
        }
    }
    result.subscription = if let Some(next) = next {
        // Only the selected next account's reset joins actual blocker recovery.
        result.earliest_recovery = Some(
            result
                .earliest_recovery
                .map_or(next.reset_at, |at| at.min(next.reset_at)),
        );
        SubscriptionOutlook::Next(next)
    } else if eligible.is_some_and(|n| n > 0 && n == safe_usable) {
        SubscriptionOutlook::NoExhaustionBeforeReset
    } else {
        SubscriptionOutlook::Unknown
    };
    result
        .credit_continuations
        .sort_by(|a, b| (&a.account.name, &a.account.id).cmp(&(&b.account.name, &b.account.id)));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_forecast::{ForecastBasis, UsageProjection, WindowUsageForecast};
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }
    fn at(minutes: i64) -> DateTime<Utc> {
        now() + Duration::minutes(minutes)
    }
    fn window(
        seconds: i64,
        outcome: ForecastOutcome,
        exhausted_in: i64,
        reset_in: i64,
    ) -> WindowUsageForecast {
        WindowUsageForecast {
            name: format!("{seconds}s"),
            window_seconds: seconds,
            observed_at: now(),
            used_pct: if outcome == ForecastOutcome::Exhausted {
                100.0
            } else {
                50.0
            },
            reset_at: Some(at(reset_in)),
            awaiting_usage: false,
            recent_pct_per_hour: None,
            average_pct_per_hour: None,
            recent_span_start: None,
            measurement_start: None,
            basis: Some(ForecastBasis::Average),
            exhaustion_at: (outcome == ForecastOutcome::Exhaustion).then(|| at(exhausted_in)),
            seconds_to_exhaustion: None,
            outcome,
            changing: false,
            secondary_recent: None,
            truncated: false,
        }
    }
    fn forecast(windows: Vec<WindowUsageForecast>) -> AccountUsageForecast {
        AccountUsageForecast {
            as_of: now(),
            observed_at: Some(now()),
            windows,
            earliest_window: Some("ignored-cache".into()),
            credit: None,
            truncated: false,
        }
    }
    fn account<'a>(id: &'a str, f: Option<&'a AccountUsageForecast>) -> ProviderAccount<'a> {
        ProviderAccount {
            id,
            name: id,
            user_state: UserState::Active,
            drain: false,
            usable: true,
            cooling_until: None,
            allow_credits: false,
            credits_in_use: false,
            credits_unlimited: None,
            forecast: f,
        }
    }
    fn summary(accounts: &[ProviderAccount<'_>], eligible: Option<u64>) -> ProviderSummary {
        summarize(Provider::Codex, accounts, eligible, now())
    }

    #[test]
    fn empty_unknown_and_authoritative_counts() {
        let empty = summary(&[], None);
        assert_eq!(empty.usable, Some(0));
        assert_eq!(empty.subscription, SubscriptionOutlook::Unknown);
        let rows = [account("sample-acct", None)];
        assert_eq!(summary(&rows, None).usable, None);
        assert_eq!(summary(&rows, Some(0)).usable, Some(0));
        assert_eq!(summary(&rows, Some(7)).usable, Some(7));
        assert_eq!(
            summary(&rows, None).coverage,
            MeasuredCoverage {
                measured: 0,
                total: 1
            }
        );
    }

    #[test]
    fn partial_coverage_includes_cooling_and_unknown_but_not_inactive_or_drained() {
        let safe = forecast(vec![window(
            18000,
            ForecastOutcome::NoExhaustionBeforeReset,
            0,
            120,
        )]);
        let mut rows = vec![
            account("sample-acct-a", Some(&safe)),
            account("sample-acct-b", None),
            account("sample-acct-c", None),
            account("sample-acct-d", Some(&safe)),
            account("sample-acct-e", Some(&safe)),
            account("sample-acct-f", Some(&safe)),
        ];
        rows[1].usable = false;
        rows[1].cooling_until = Some(at(20));
        rows[2].usable = false; // Unknown/relogin adapter suppresses forecast.
        rows[3].user_state = UserState::Paused;
        rows[4].user_state = UserState::Disabled;
        rows[5].drain = true;
        let result = summary(&rows, Some(1));
        assert_eq!(result.configured, 6);
        assert_eq!(
            result.coverage,
            MeasuredCoverage {
                measured: 1,
                total: 3
            }
        );
        assert_eq!(
            result.subscription,
            SubscriptionOutlook::NoExhaustionBeforeReset
        );
        assert_eq!(result.earliest_recovery, Some(at(20)));
        assert_eq!(result.pooled_runway, PooledRunway::Unavailable);
    }

    #[test]
    fn exhausted_facts_need_no_rates_latest_blocker_then_earliest_account() {
        let mut spent = window(604800, ForecastOutcome::Exhausted, 0, 2880);
        spent.observed_at = at(-120);
        let f = forecast(vec![
            window(18000, ForecastOutcome::Exhausted, 0, 60),
            spent,
            window(3600, ForecastOutcome::Measuring, 0, 90),
        ]);
        let other = forecast(vec![window(18000, ForecastOutcome::Exhausted, 0, 180)]);
        let mut a = account("sample-acct-a", Some(&f));
        a.usable = false;
        a.cooling_until = Some(at(30));
        assert_eq!(summary(&[a], Some(0)).earliest_recovery, Some(at(2880)));
        a.cooling_until = Some(at(4320));
        assert_eq!(summary(&[a], Some(0)).earliest_recovery, Some(at(4320)));
        let mut b = account("sample-acct-b", Some(&other));
        b.usable = false;
        let result = summary(&[a, b], Some(0));
        assert_eq!(
            result.coverage,
            MeasuredCoverage {
                measured: 2,
                total: 2
            }
        );
        assert_eq!(result.earliest_recovery, Some(at(180)));
        assert_eq!(result, summary(&[b, a], Some(0)));
    }

    #[test]
    fn chosen_average_and_next_reset_not_secondary_or_unrelated_reset() {
        let mut weekly = window(604800, ForecastOutcome::Exhaustion, 35, 600);
        weekly.secondary_recent = Some(UsageProjection {
            pct_per_hour: 99.0,
            measurement_start: at(-10),
            exhaustion_at: Some(at(1)),
            seconds_to_exhaustion: Some(60),
            outcome: ForecastOutcome::Exhaustion,
            changing: false,
        });
        let f = forecast(vec![
            window(18000, ForecastOutcome::Exhaustion, 90, 300),
            weekly,
        ]);
        let later = forecast(vec![window(18000, ForecastOutcome::Exhaustion, 45, 60)]);
        let rows = [
            account("sample-acct-a", Some(&f)),
            account("sample-acct-b", Some(&later)),
        ];
        let result = summary(&rows, Some(2));
        assert_eq!(
            result.subscription,
            SubscriptionOutlook::Next(NextExhaustion {
                account: account_ref(&rows[0]),
                at: at(35),
                reset_at: at(600)
            })
        );
        assert_eq!(result.earliest_recovery, Some(at(600)));
        assert_eq!(result, summary(&[rows[1], rows[0]], Some(2)));
    }

    #[test]
    fn ties_use_name_then_id_independent_of_input_order() {
        let f = forecast(vec![window(18000, ForecastOutcome::Exhaustion, 10, 60)]);
        let mut a = account("sample-acct-a", Some(&f));
        a.name = "sample-acct";
        let mut b = account("sample-acct-b", Some(&f));
        b.name = "sample-acct";
        let result = summary(&[b, a], Some(2));
        assert_eq!(result, summary(&[a, b], Some(2)));
        let SubscriptionOutlook::Next(next) = result.subscription else {
            panic!("expected next");
        };
        assert_eq!(next.account.id, "sample-acct-a");
    }

    #[test]
    fn stale_future_reset_pending_and_invalid_windows_remain_unknown() {
        let f = forecast(vec![window(18000, ForecastOutcome::Exhaustion, 10, 60)]);
        let aged = summarize(
            Provider::Codex,
            &[account("sample-acct", Some(&f))],
            Some(1),
            at(6),
        );
        assert_eq!(aged.coverage.measured, 0);
        for variant in 0..6 {
            let mut bad = f.clone();
            match variant {
                0 => bad.as_of = at(1),
                1 => bad.windows[0].observed_at = at(1),
                2 => bad.windows[0].awaiting_usage = true,
                3 => bad.windows[0].reset_at = Some(now()),
                4 => bad.windows[0].used_pct = f64::NAN,
                _ => bad.windows[0].outcome = ForecastOutcome::ResetPending,
            }
            let result = summary(&[account("sample-acct", Some(&bad))], Some(1));
            assert_eq!(result.coverage.measured, 0);
            assert_eq!(result.subscription, SubscriptionOutlook::Unknown);
            assert_eq!(result.earliest_recovery, None);
        }
    }

    #[test]
    fn credit_permission_is_not_continuation_and_continuation_keeps_coverage() {
        let f = forecast(vec![window(18000, ForecastOutcome::Exhaustion, 10, 60)]);
        let mut a = account("sample-acct", Some(&f));
        a.allow_credits = true;
        a.credits_unlimited = Some(true);
        assert!(summary(&[a], Some(1)).credit_continuations.is_empty());
        a.credits_in_use = true;
        let result = summary(&[a], Some(1));
        assert_eq!(result.coverage.measured, 1);
        assert_eq!(result.subscription, SubscriptionOutlook::Unknown);
        assert_eq!(
            result.credit_continuations[0].runway,
            CreditRunway::Unlimited
        );
        a.forecast = None;
        a.credits_unlimited = None;
        let result = summary(&[a], Some(1));
        assert_eq!(
            result.coverage,
            MeasuredCoverage {
                measured: 0,
                total: 1
            }
        );
        assert_eq!(result.credit_continuations[0].runway, CreditRunway::Unknown);
        assert!(
            summarize(Provider::Claude, &[a], Some(1), now())
                .credit_continuations
                .is_empty()
        );
        for variant in 0..3 {
            let mut gated = a;
            match variant {
                0 => gated.allow_credits = false,
                1 => gated.usable = false,
                _ => gated.drain = true,
            }
            assert!(summary(&[gated], Some(1)).credit_continuations.is_empty());
        }
    }

    #[test]
    fn credit_runway_reuses_monetary_estimate_never_infers_unlimited() {
        let mut f = forecast(vec![]);
        f.credit = Some(CreditUsageForecast {
            observed_at: now(),
            balance: 10.0,
            recent_credits_per_hour: Some(0.0),
            measurement_start: Some(at(-10)),
            exhaustion_at: None,
            seconds_to_exhaustion: None,
            outcome: ForecastOutcome::NoFiniteForecast,
            changing: false,
            truncated: false,
        });
        let check = |f: &AccountUsageForecast| {
            let mut a = account("sample-acct", Some(f));
            a.allow_credits = true;
            a.credits_in_use = true;
            summary(&[a], Some(1)).credit_continuations.remove(0).runway
        };
        assert_eq!(check(&f), CreditRunway::Forecast(f.credit.clone().unwrap()));
        f.credit.as_mut().unwrap().outcome = ForecastOutcome::Exhaustion;
        f.credit.as_mut().unwrap().exhaustion_at = Some(at(20));
        assert_eq!(check(&f), CreditRunway::Forecast(f.credit.clone().unwrap()));
        f.credit.as_mut().unwrap().observed_at = at(-6);
        assert_eq!(check(&f), CreditRunway::Unknown);
        f.credit.as_mut().unwrap().observed_at = at(1);
        assert_eq!(check(&f), CreditRunway::Unknown);
    }

    #[test]
    fn observed_quota_including_outside_use_needs_no_ledger_or_plan_sum() {
        use crate::usage_forecast::Forecasts;
        let mut estimates = Forecasts::default();
        // Quota polls include outside use; no request/token ledger is supplied.
        for (minute, pct) in [(-10, 40.0), (-5, 45.0), (0, 50.0)] {
            estimates.observe("sample-acct", "5h", pct, Some(at(180)), at(minute));
        }
        let f = estimates.view("sample-acct", now(), None);
        let result = summary(&[account("sample-acct", Some(&f))], Some(1));
        assert_eq!(result.coverage.measured, 1);
        assert!(matches!(result.subscription, SubscriptionOutlook::Next(_)));
        assert_eq!(result.pooled_runway, PooledRunway::Unavailable);
    }
}
