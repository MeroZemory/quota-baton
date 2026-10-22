//! Shared presentation of the core's display-only estimates. No rate calculation.
use chrono::{DateTime, Local, Utc};

use crate::usage_forecast::{
    AccountUsageForecast, ForecastBasis, ForecastOutcome, WindowUsageForecast,
};

/// Compact display-only account estimate. It never authorizes routing or credit use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForecastSummary {
    pub text: String,
    pub warning: bool,
}

/// Five minutes matches the normal usage poll interval. A summary is relative
/// to the DTO's explicit `as_of`, never a hidden read of the current clock.
const SUMMARY_FRESH_SECONDS: i64 = 300;

/// Select the limiting typed window without trusting the cached earliest name.
/// Unknown/stale/reset-pending windows make the account summary unknown. Of
/// observed exhausted windows, the latest reset controls recovery; otherwise
/// choose the earliest finite exhaustion, then the earliest safe reset. Ties
/// are duration/name ordered, independent of the incoming vector order.
pub fn limiting_window(forecast: &AccountUsageForecast) -> Option<&WindowUsageForecast> {
    let fresh = |at: DateTime<Utc>| {
        let age = forecast.as_of.signed_duration_since(at);
        age >= chrono::Duration::zero() && age <= chrono::Duration::seconds(SUMMARY_FRESH_SECONDS)
    };
    if !forecast.observed_at.is_some_and(fresh) || forecast.windows.is_empty() {
        return None;
    }
    for w in &forecast.windows {
        if !fresh(w.observed_at)
            || w.awaiting_usage
            || w.window_seconds <= 0
            || !w.used_pct.is_finite()
            || !(0.0..=100.0).contains(&w.used_pct)
            || !w.reset_at.is_some_and(|at| at > forecast.as_of)
        {
            return None;
        }
        match w.outcome {
            ForecastOutcome::Exhausted if w.used_pct >= 100.0 => {}
            ForecastOutcome::Exhaustion
                if w.used_pct < 100.0
                    && w.exhaustion_at
                        .is_some_and(|at| at > forecast.as_of && Some(at) < w.reset_at) => {}
            ForecastOutcome::NoExhaustionBeforeReset if w.used_pct < 100.0 => {}
            _ => return None,
        }
    }
    let tie = |a: &&WindowUsageForecast, b: &&WindowUsageForecast| {
        a.window_seconds
            .cmp(&b.window_seconds)
            .then_with(|| a.name.cmp(&b.name))
    };
    if let Some(w) = forecast
        .windows
        .iter()
        .filter(|w| w.outcome == ForecastOutcome::Exhausted)
        .min_by(|a, b| b.reset_at.cmp(&a.reset_at).then_with(|| tie(a, b)))
    {
        return Some(w);
    }
    if let Some(w) = forecast
        .windows
        .iter()
        .filter(|w| w.outcome == ForecastOutcome::Exhaustion)
        .min_by(|a, b| {
            a.exhaustion_at
                .cmp(&b.exhaustion_at)
                .then_with(|| tie(a, b))
        })
    {
        return Some(w);
    }
    forecast
        .windows
        .iter()
        .min_by(|a, b| a.reset_at.cmp(&b.reset_at).then_with(|| tie(a, b)))
}

/// Local clock presentation shared by account/provider summaries. Date is shown
/// only when it differs from the explicit forecast date; English never uses KO.
pub fn forecast_clock(as_of: DateTime<Utc>, at: DateTime<Utc>, english: bool) -> String {
    let local = at.with_timezone(&Local);
    let today = local.date_naive() == as_of.with_timezone(&Local).date_naive();
    local
        .format(if today {
            "%H:%M"
        } else if english {
            "%b %-d %H:%M"
        } else {
            "%m/%d %H:%M"
        })
        .to_string()
}

/// Positive time to an existing estimate, including "left"/"남음". No estimate
/// is calculated from a rate. Round sub-day minutes up; at a day or more retain
/// only whole days/hours. Past/now returns None, never a future runway of zero.
pub fn time_left(as_of: DateTime<Utc>, at: DateTime<Utc>, english: bool) -> Option<String> {
    let delta = at.signed_duration_since(as_of);
    if delta <= chrono::Duration::zero() {
        return None;
    }
    let seconds = delta.num_seconds();
    Some(if delta >= chrono::Duration::days(1) {
        let hours = seconds / 3600;
        if english {
            format!("{}d {}h left", hours / 24, hours % 24)
        } else {
            format!("{}일 {}시간 남음", hours / 24, hours % 24)
        }
    } else {
        let seconds = seconds.saturating_add(i64::from(delta.subsec_nanos() > 0));
        let minutes = seconds.saturating_add(59) / 60;
        if minutes < 60 {
            if english {
                format!("{minutes}m left")
            } else {
                format!("{minutes}분 남음")
            }
        } else if english {
            format!("{}h {}m left", minutes / 60, minutes % 60)
        } else {
            format!("{}시간 {}분 남음", minutes / 60, minutes % 60)
        }
    })
}

/// D24 compact four-state presentation. Credits and secondary projections stay
/// in `lines`; provider credit continuity is a separately observed fact.
pub fn summary(forecast: &AccountUsageForecast, english: bool) -> ForecastSummary {
    let measuring = || ForecastSummary {
        text: if english { "Measuring" } else { "측정 중" }.into(),
        warning: false,
    };
    let Some(window) = limiting_window(forecast) else {
        return measuring();
    };
    let clock = |at| forecast_clock(forecast.as_of, at, english);
    let reset = window.reset_at.expect("selected future reset");
    match window.outcome {
        ForecastOutcome::Exhausted => ForecastSummary {
            text: if english {
                format!("Out until {}", clock(reset))
            } else {
                format!("{}까지 소진", clock(reset))
            },
            warning: false,
        },
        ForecastOutcome::NoExhaustionBeforeReset => ForecastSummary {
            text: if english {
                format!("Lasts until reset {}", clock(reset))
            } else {
                format!("초기화({})까지 충분", clock(reset))
            },
            warning: false,
        },
        ForecastOutcome::Exhaustion => {
            let at = window.exhaustion_at.expect("selected future exhaustion");
            let left = time_left(forecast.as_of, at, english).expect("selected positive interval");
            let label = if window.window_seconds == 7 * 86400 {
                if english {
                    "weekly".into()
                } else {
                    "주간".into()
                }
            } else if english {
                let seconds = window.window_seconds;
                if seconds % 86400 == 0 {
                    format!("{}d", seconds / 86400)
                } else if seconds % 3600 == 0 {
                    format!("{}h", seconds / 3600)
                } else if seconds % 60 == 0 {
                    format!("{}m", seconds / 60)
                } else {
                    format!("{seconds}s")
                }
            } else {
                window_label(window.window_seconds)
            };
            ForecastSummary {
                text: if english {
                    format!("{left} · runs out {} ({label})", clock(at))
                } else {
                    format!("{left} · {} 소진 ({label})", clock(at))
                },
                warning: at.signed_duration_since(forecast.as_of) <= chrono::Duration::hours(5),
            }
        }
        _ => measuring(),
    }
}

pub fn lines(forecast: &AccountUsageForecast, credits_in_use: bool) -> Vec<String> {
    let clock = |at: DateTime<Utc>| {
        let local = at.with_timezone(&Local);
        if local.date_naive() == forecast.as_of.with_timezone(&Local).date_naive() {
            local.format("%H:%M").to_string()
        } else {
            local.format("%m/%d %H:%M").to_string()
        }
    };
    let describe = |outcome,
                    rate: Option<f64>,
                    seconds: Option<i64>,
                    at: Option<DateTime<Utc>>,
                    reset: Option<DateTime<Utc>>| {
        let reset = reset
            .map(&clock)
            .map(|s| format!(" · 초기화 {s}"))
            .unwrap_or_default();
        match outcome {
            ForecastOutcome::Measuring => "측정 중".to_owned(),
            ForecastOutcome::ResetPending => "초기화 후 관측 대기".to_owned(),
            ForecastOutcome::Exhausted => format!("소진됨{reset}"),
            ForecastOutcome::NoFiniteForecast => {
                format!("{} · 소진 시각 미정{reset}", rate_text(rate))
            }
            ForecastOutcome::NoExhaustionBeforeReset => {
                format!("{} · 초기화 전 소진 없음{reset}", rate_text(rate))
            }
            ForecastOutcome::Exhaustion => format!(
                "{} · 약 {} 후 소진{}",
                rate_text(rate),
                duration(seconds),
                at.map(&clock)
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default()
            ),
        }
    };
    let mut result = Vec::new();
    for window in &forecast.windows {
        let rate = match window.basis {
            Some(ForecastBasis::Recent) => window.recent_pct_per_hour,
            Some(ForecastBasis::Average) => window.average_pct_per_hour,
            None => None,
        };
        let basis = match window.basis {
            Some(ForecastBasis::Recent) => "최근 속도",
            Some(ForecastBasis::Average) => "구간 평균",
            None => "예측",
        };
        let mut line = format!(
            "{} {basis}: {}{}",
            window_label(window.window_seconds),
            describe(
                window.outcome,
                rate,
                window.seconds_to_exhaustion,
                window.exhaustion_at,
                window.reset_at
            ),
            if window.changing {
                " · 변동 중"
            } else {
                ""
            }
        );
        if forecast.earliest_window.as_deref() == Some(&window.name) {
            line.push_str(" · 가장 먼저 소진 예상");
        }
        if window.truncated {
            line.push_str(" · 표본 요약");
        }
        result.push(line);
        if let Some(recent) = &window.secondary_recent {
            result.push(format!(
                "{} 지금 속도면: {}{}",
                window_label(window.window_seconds),
                describe(
                    recent.outcome,
                    Some(recent.pct_per_hour),
                    recent.seconds_to_exhaustion,
                    recent.exhaustion_at,
                    window.reset_at
                ),
                if recent.changing {
                    " · 변동 중"
                } else {
                    ""
                }
            ));
        }
    }
    if result.is_empty() {
        result.push("소진 예측: 측정 중".into());
    }
    if credits_in_use {
        result.push("허용된 유료 크레딧으로 계속 처리 중".into());
        if let Some(credit) = &forecast.credit {
            let mut line = if credit.outcome == ForecastOutcome::Exhausted {
                "크레딧 소진됨".into()
            } else {
                credit
                    .recent_credits_per_hour
                    .map(|r| format!("크레딧 최근 속도: {r:.1}/시간"))
                    .unwrap_or_else(|| "크레딧 소진 예측: 측정 중".into())
            };
            if let Some(at) = credit
                .exhaustion_at
                .filter(|_| credit.outcome != ForecastOutcome::Exhausted)
            {
                line.push_str(&format!(
                    " · 약 {} 후 소진 ({})",
                    duration(credit.seconds_to_exhaustion),
                    clock(at)
                ));
            }
            if credit.changing {
                line.push_str(" · 변동 중");
            }
            if credit.truncated {
                line.push_str(" · 표본 요약");
            }
            result.push(line);
        }
    }
    result
}

fn rate_text(rate: Option<f64>) -> String {
    rate.map(|r| format!("{r:.1}%/시간"))
        .unwrap_or_else(|| "측정 중".into())
}

fn duration(seconds: Option<i64>) -> String {
    let minutes = seconds.unwrap_or_default().max(0).saturating_add(59) / 60;
    match minutes {
        0 => "1분 미만".into(),
        1..60 => format!("{minutes}분"),
        _ => format!("{}시간 {}분", minutes / 60, minutes % 60),
    }
}

fn window_label(seconds: i64) -> String {
    if seconds % 86_400 == 0 {
        format!("{}일", seconds / 86_400)
    } else if seconds % 3_600 == 0 {
        format!("{}시간", seconds / 3_600)
    } else if seconds % 60 == 0 {
        format!("{}분", seconds / 60)
    } else {
        format!("{seconds}초")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_forecast::Forecasts;
    use chrono::{Duration, TimeZone};

    #[test]
    fn r13_forecast_display_exhausted_credit_and_exact_second_windows() {
        assert_eq!(window_label(30), "30초");
        assert_eq!(window_label(90), "90초");
        assert_eq!(window_label(120), "2분");
        let mut samples = Forecasts::default();
        let now = Utc::now();
        samples.observe_credit("a", 0., now);
        let text = lines(&samples.view("a", now, None), true).join("\n");
        assert!(text.contains("크레딧 소진됨"));
        assert!(!text.contains("후 소진"));
        assert!(!text.contains("크레딧 소진 예측: 측정 중"));
    }

    #[test]
    fn r13_forecast_display_shares_numbers_and_gates_paid_note() {
        let now = Utc.with_ymd_and_hms(2026, 10, 22, 0, 0, 0).unwrap();
        let reset = now + Duration::hours(3);
        let mut samples = Forecasts::default();
        for (minute, used) in [(0, 20.), (5, 22.), (10, 24.)] {
            let at = now + Duration::minutes(minute);
            samples.observe("cx-test", "5h", used, Some(reset), at);
            samples.observe_credit("cx-test", 100. - used, at);
        }
        let forecast = samples.view("cx-test", now + Duration::minutes(10), Some(now));
        let text = lines(&forecast, false).join("\n");
        assert!(text.contains("24.0%/시간"));
        assert!(text.contains("변동 중"));
        assert!(!text.contains("크레딧"));
        let paid = lines(&forecast, true).join("\n");
        assert!(paid.contains("허용된 유료 크레딧으로 계속 처리 중"));
        assert!(paid.contains("크레딧 최근 속도: 24.0/시간"));
    }

    #[test]
    fn r13_forecast_display_cold_reset_and_exhausted_labels() {
        let now = Utc::now();
        let mut samples = Forecasts::default();
        assert_eq!(
            lines(&samples.view("a", now, None), false),
            vec!["소진 예측: 측정 중"]
        );
        samples.observe("a", "7d", 100., Some(now + Duration::hours(2)), now);
        let text = lines(&samples.view("a", now, None), false).join("\n");
        assert!(text.contains("7일 구간 평균: 소진됨 · 초기화"));
        samples.observe("b", "5h", 1., Some(now + Duration::hours(2)), now);
        let text = lines(&samples.view("b", now, None), false).join("\n");
        assert!(text.contains("초기화 전 소진 없음"));
    }

    fn summary_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 23, 0, 0, 0).unwrap()
    }

    fn summary_window(name: &str, seconds: i64, outcome: ForecastOutcome) -> WindowUsageForecast {
        let now = summary_now();
        WindowUsageForecast {
            name: name.into(),
            window_seconds: seconds,
            observed_at: now,
            used_pct: if outcome == ForecastOutcome::Exhausted {
                100.
            } else {
                50.
            },
            reset_at: Some(now + Duration::hours(4)),
            awaiting_usage: false,
            recent_pct_per_hour: Some(25.),
            average_pct_per_hour: Some(20.),
            recent_span_start: Some(now - Duration::minutes(30)),
            measurement_start: Some(now - Duration::minutes(30)),
            basis: Some(ForecastBasis::Recent),
            exhaustion_at: (outcome == ForecastOutcome::Exhaustion)
                .then_some(now + Duration::hours(2)),
            seconds_to_exhaustion: (outcome == ForecastOutcome::Exhaustion).then_some(7200),
            outcome,
            changing: true,
            secondary_recent: None,
            truncated: false,
        }
    }

    fn summary_forecast(windows: Vec<WindowUsageForecast>) -> AccountUsageForecast {
        AccountUsageForecast {
            as_of: summary_now(),
            observed_at: Some(summary_now()),
            windows,
            earliest_window: None,
            credit: None,
            truncated: false,
        }
    }

    fn no_hangul(text: &str) -> bool {
        !text
            .chars()
            .any(|ch| ('\u{ac00}'..='\u{d7a3}').contains(&ch))
    }

    #[test]
    fn r162_summary_four_states_ko_en_one_line() {
        let now = summary_now();
        for english in [false, true] {
            for outcome in [
                ForecastOutcome::Exhaustion,
                ForecastOutcome::NoExhaustionBeforeReset,
                ForecastOutcome::Exhausted,
                ForecastOutcome::Measuring,
            ] {
                let forecast = summary_forecast(vec![summary_window("5h", 18000, outcome)]);
                let result = summary(&forecast, english);
                let at = forecast_clock(now, now + Duration::hours(2), english);
                let reset = forecast_clock(now, now + Duration::hours(4), english);
                let expected = match (outcome, english) {
                    (ForecastOutcome::Exhaustion, true) => {
                        format!("2h 0m left · runs out {at} (5h)")
                    }
                    (ForecastOutcome::Exhaustion, false) => {
                        format!("2시간 0분 남음 · {at} 소진 (5시간)")
                    }
                    (ForecastOutcome::NoExhaustionBeforeReset, true) => {
                        format!("Lasts until reset {reset}")
                    }
                    (ForecastOutcome::NoExhaustionBeforeReset, false) => {
                        format!("초기화({reset})까지 충분")
                    }
                    (ForecastOutcome::Exhausted, true) => format!("Out until {reset}"),
                    (ForecastOutcome::Exhausted, false) => format!("{reset}까지 소진"),
                    (_, true) => "Measuring".into(),
                    (_, false) => "측정 중".into(),
                };
                assert_eq!(result.text, expected);
                assert_eq!(result.warning, outcome == ForecastOutcome::Exhaustion);
                assert!(!result.text.contains('\n'));
                if english {
                    assert!(no_hangul(&result.text));
                }
            }
        }
    }

    #[test]
    fn r162_summary_exhausted_uses_latest_blocking_recovery_not_earliest_reset() {
        let now = summary_now();
        let mut short = summary_window("5h", 18000, ForecastOutcome::Exhausted);
        short.reset_at = Some(now + Duration::hours(1));
        let mut weekly = summary_window("7d", 604800, ForecastOutcome::Exhausted);
        weekly.reset_at = Some(now + Duration::days(3));
        for windows in [
            vec![short.clone(), weekly.clone()],
            vec![weekly.clone(), short.clone()],
        ] {
            let forecast = summary_forecast(windows);
            assert_eq!(limiting_window(&forecast).unwrap().name, "7d");
            for english in [false, true] {
                let result = summary(&forecast, english);
                assert!(result.text.contains(&forecast_clock(
                    now,
                    now + Duration::days(3),
                    english
                )));
                assert!(!result.warning);
                assert!(!result.text.contains("left"));
                assert!(!result.text.contains("남음"));
            }
        }
    }

    #[test]
    fn r162_summary_competing_finite_windows_and_ties_are_order_independent() {
        let mut short = summary_window("5h", 18000, ForecastOutcome::Exhaustion);
        let weekly = summary_window("7d", 604800, ForecastOutcome::Exhaustion);
        for windows in [
            vec![short.clone(), weekly.clone()],
            vec![weekly.clone(), short.clone()],
        ] {
            let mut forecast = summary_forecast(windows);
            forecast.earliest_window = Some("stale-name".into());
            assert_eq!(limiting_window(&forecast).unwrap().name, "5h");
        }
        short.exhaustion_at = Some(summary_now() + Duration::hours(3));
        let forecast = summary_forecast(vec![short, weekly]);
        assert_eq!(limiting_window(&forecast).unwrap().name, "7d");
        assert!(summary(&forecast, true).text.ends_with("(weekly)"));
        assert!(summary(&forecast, false).text.ends_with("(주간)"));
        let mut observed = summary_window("5h", 18000, ForecastOutcome::Exhausted);
        observed.reset_at = Some(summary_now() + Duration::hours(1));
        let mut forecast = forecast;
        forecast.windows.push(observed);
        assert_eq!(
            limiting_window(&forecast).unwrap().outcome,
            ForecastOutcome::Exhausted
        );
    }

    #[test]
    fn r162_summary_warning_exact_five_hour_boundary_and_long_runway() {
        let now = summary_now();
        for (delta, warning) in [
            (Duration::hours(5) - Duration::nanoseconds(1), true),
            (Duration::hours(5), true),
            (Duration::hours(5) + Duration::nanoseconds(1), false),
            (Duration::hours(51), false),
        ] {
            let mut window = summary_window("7d", 604800, ForecastOutcome::Exhaustion);
            window.reset_at = Some(now + Duration::days(4));
            window.exhaustion_at = Some(now + delta);
            let forecast = summary_forecast(vec![window]);
            assert_eq!(summary(&forecast, true).warning, warning);
            assert_eq!(summary(&forecast, false).warning, warning);
            if delta == Duration::hours(51) {
                assert!(
                    summary(&forecast, true)
                        .text
                        .starts_with("2d 3h left · runs out ")
                );
                assert!(
                    summary(&forecast, false)
                        .text
                        .starts_with("2일 3시간 남음 · ")
                );
            }
        }
    }

    #[test]
    fn r162_summary_measuring_for_stale_unknown_reset_pending_or_missing_rates() {
        for outcome in [
            ForecastOutcome::Measuring,
            ForecastOutcome::ResetPending,
            ForecastOutcome::NoFiniteForecast,
        ] {
            let mut window = summary_window("5h", 18000, outcome);
            window.recent_pct_per_hour = None;
            window.average_pct_per_hour = None;
            window.basis = None;
            for english in [false, true] {
                assert_eq!(
                    summary(&summary_forecast(vec![window.clone()]), english),
                    ForecastSummary {
                        text: if english { "Measuring" } else { "측정 중" }.into(),
                        warning: false
                    }
                );
            }
        }
        let mut forecast = summary_forecast(vec![summary_window(
            "5h",
            18000,
            ForecastOutcome::Exhaustion,
        )]);
        forecast.windows[0].observed_at -= Duration::seconds(301);
        assert_eq!(summary(&forecast, true).text, "Measuring");
        forecast.windows[0].observed_at = summary_now() - Duration::seconds(300);
        assert!(summary(&forecast, true).warning);
        forecast.windows[0].observed_at = summary_now() + Duration::nanoseconds(1);
        assert_eq!(summary(&forecast, true).text, "Measuring");
        forecast.windows[0].observed_at = summary_now();
        forecast.observed_at = None;
        assert_eq!(summary(&forecast, true).text, "Measuring");
        assert_eq!(summary(&summary_forecast(vec![]), false).text, "측정 중");
    }

    #[test]
    fn r162_summary_invalid_or_incomplete_competing_window_cannot_claim_safe_runway() {
        let good = summary_window("5h", 18000, ForecastOutcome::NoExhaustionBeforeReset);
        let mut variants = Vec::new();
        let base = summary_window("7d", 604800, ForecastOutcome::Exhaustion);
        let mut bad = base.clone();
        bad.reset_at = None;
        variants.push(bad);
        let mut bad = base.clone();
        bad.reset_at = Some(summary_now());
        variants.push(bad);
        let mut bad = base.clone();
        bad.exhaustion_at = bad.reset_at;
        variants.push(bad);
        let mut bad = base.clone();
        bad.exhaustion_at = None;
        variants.push(bad);
        let mut bad = base.clone();
        bad.awaiting_usage = true;
        variants.push(bad);
        let mut bad = base.clone();
        bad.window_seconds = 0;
        variants.push(bad);
        let mut bad = base.clone();
        bad.used_pct = f64::NAN;
        variants.push(bad);
        let mut bad = base.clone();
        bad.used_pct = 101.;
        variants.push(bad);
        let mut bad = base;
        bad.outcome = ForecastOutcome::Measuring;
        variants.push(bad);
        for bad in variants {
            let forecast = summary_forecast(vec![good.clone(), bad]);
            assert!(limiting_window(&forecast).is_none());
            assert_eq!(
                summary(&forecast, true),
                ForecastSummary {
                    text: "Measuring".into(),
                    warning: false
                }
            );
        }
    }

    #[test]
    fn r162_summary_safe_windows_choose_earliest_reset_without_inventing_exhaustion() {
        let short = summary_window("5h", 18000, ForecastOutcome::NoExhaustionBeforeReset);
        let mut long = summary_window("7d", 604800, ForecastOutcome::NoExhaustionBeforeReset);
        long.reset_at = Some(summary_now() + Duration::days(3));
        let forecast = summary_forecast(vec![long, short]);
        assert_eq!(limiting_window(&forecast).unwrap().name, "5h");
        assert!(
            summary(&forecast, true)
                .text
                .starts_with("Lasts until reset ")
        );
        assert!(!summary(&forecast, true).warning);
    }

    #[test]
    fn r162_summary_duration_and_local_clock_helpers_share_ko_en_contract() {
        let now = summary_now();
        for (delta, en, ko) in [
            (Duration::minutes(42), "42m left", "42분 남음"),
            (
                Duration::hours(9) + Duration::minutes(38),
                "9h 38m left",
                "9시간 38분 남음",
            ),
            (
                Duration::days(2) + Duration::hours(3) + Duration::minutes(59),
                "2d 3h left",
                "2일 3시간 남음",
            ),
        ] {
            assert_eq!(time_left(now, now + delta, true).as_deref(), Some(en));
            assert_eq!(time_left(now, now + delta, false).as_deref(), Some(ko));
        }
        assert_eq!(time_left(now, now, true), None);
        assert_eq!(time_left(now, now - Duration::seconds(1), false), None);
        assert_eq!(
            time_left(now, now + Duration::nanoseconds(1), true).as_deref(),
            Some("1m left")
        );
        assert_eq!(
            forecast_clock(now, now, true),
            now.with_timezone(&Local).format("%H:%M").to_string()
        );
        let later = now + Duration::days(2);
        assert_eq!(
            forecast_clock(now, later, true),
            later
                .with_timezone(&Local)
                .format("%b %-d %H:%M")
                .to_string()
        );
        assert_eq!(
            forecast_clock(now, later, false),
            later
                .with_timezone(&Local)
                .format("%m/%d %H:%M")
                .to_string()
        );
    }

    #[test]
    fn r162_summary_real_estimator_preserves_detailed_rates_and_paid_note() {
        let now = summary_now();
        let mut samples = Forecasts::default();
        samples.observe("a", "5h", 50., Some(now + Duration::hours(3)), now);
        let forecast = samples.view("a", now, None);
        let concise = summary(&forecast, true);
        assert!(concise.text.starts_with("2h 0m left · runs out "));
        assert!(concise.warning);
        assert!(!concise.text.contains('%'));
        assert!(!concise.text.contains("rate"));
        let detail = lines(&forecast, true).join("\n");
        assert!(detail.contains("구간 평균"));
        assert!(detail.contains("25.0%/시간"));
        assert!(detail.contains("허용된 유료 크레딧으로 계속 처리 중"));
    }
}
