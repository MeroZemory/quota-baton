//! Shared presentation of the core's display-only estimates. No rate calculation.
use chrono::{DateTime, Local, Utc};

use crate::usage_forecast::{AccountUsageForecast, ForecastBasis, ForecastOutcome};

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
}
