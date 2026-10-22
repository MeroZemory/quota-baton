//! English presentation of typed display-only estimates. No rate calculation.
//! Korean callers retain the core formatter; account/window names are never translated.
use chrono::{DateTime, Local, Utc};

use qb_core::usage_forecast::{AccountUsageForecast, ForecastBasis, ForecastOutcome};

pub fn lines(forecast: &AccountUsageForecast, credits_in_use: bool) -> Vec<String> {
    let clock = |at: DateTime<Utc>| at.with_timezone(&Local).format("%b %-d %H:%M").to_string();
    let describe = |outcome,
                    rate: Option<f64>,
                    seconds: Option<i64>,
                    at: Option<DateTime<Utc>>,
                    reset: Option<DateTime<Utc>>| {
        let reset = reset
            .map(&clock)
            .map(|s| format!(" · Resets {s}"))
            .unwrap_or_default();
        match outcome {
            ForecastOutcome::Measuring => "Measuring".to_owned(),
            ForecastOutcome::ResetPending => "Waiting for an observation after reset".to_owned(),
            ForecastOutcome::Exhausted => format!("Exhausted{reset}"),
            ForecastOutcome::NoFiniteForecast => {
                format!("{} · No finite exhaustion estimate{reset}", rate_text(rate))
            }
            ForecastOutcome::NoExhaustionBeforeReset => {
                format!("{} · No exhaustion before reset{reset}", rate_text(rate))
            }
            ForecastOutcome::Exhaustion => format!(
                "{} · Exhausts in about {}{}",
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
            Some(ForecastBasis::Recent) => "Recent rate",
            Some(ForecastBasis::Average) => "Window average",
            None => "Forecast",
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
            if window.changing { " · Changing" } else { "" }
        );
        if forecast.earliest_window.as_deref() == Some(&window.name) {
            line.push_str(" · Expected to exhaust first");
        }
        if window.truncated {
            line.push_str(" · Samples summarized");
        }
        result.push(line);
        if let Some(recent) = &window.secondary_recent {
            result.push(format!(
                "{} at the current rate: {}{}",
                window_label(window.window_seconds),
                describe(
                    recent.outcome,
                    Some(recent.pct_per_hour),
                    recent.seconds_to_exhaustion,
                    recent.exhaustion_at,
                    window.reset_at
                ),
                if recent.changing { " · Changing" } else { "" }
            ));
        }
    }
    if result.is_empty() {
        result.push("Exhaustion forecast: Measuring".into());
    }
    if credits_in_use {
        result.push("Continuing with allowed paid credits".into());
        if let Some(credit) = &forecast.credit {
            let mut line = if credit.outcome == ForecastOutcome::Exhausted {
                "Credits exhausted".into()
            } else {
                credit
                    .recent_credits_per_hour
                    .map(|r| format!("Recent credit rate: {r:.1}/h"))
                    .unwrap_or_else(|| "Credit exhaustion forecast: Measuring".into())
            };
            if let Some(at) = credit
                .exhaustion_at
                .filter(|_| credit.outcome != ForecastOutcome::Exhausted)
            {
                line.push_str(&format!(
                    " · Exhausts in about {} ({})",
                    duration(credit.seconds_to_exhaustion),
                    clock(at)
                ));
            }
            if credit.changing {
                line.push_str(" · Changing");
            }
            if credit.truncated {
                line.push_str(" · Samples summarized");
            }
            result.push(line);
        }
    }
    result
}

fn rate_text(rate: Option<f64>) -> String {
    rate.map(|r| format!("{r:.1}%/h"))
        .unwrap_or_else(|| "Measuring".into())
}

fn duration(seconds: Option<i64>) -> String {
    let minutes = seconds.unwrap_or_default().max(0).saturating_add(59) / 60;
    match minutes {
        0 => "<1m".into(),
        1..60 => format!("{minutes}m"),
        _ => format!("{}h {}m", minutes / 60, minutes % 60),
    }
}

fn window_label(seconds: i64) -> String {
    if seconds % 86_400 == 0 {
        format!("{}d", seconds / 86_400)
    } else if seconds % 3_600 == 0 {
        format!("{}h", seconds / 3_600)
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use qb_core::usage_forecast::{
        CreditUsageForecast, Forecasts, UsageProjection, WindowUsageForecast,
    };

    fn no_hangul(lines: &[String]) {
        assert!(!lines.is_empty());
        for line in lines {
            assert!(!line.chars().any(|c| matches!(c, '\u{1100}'..='\u{11ff}' | '\u{3130}'..='\u{318f}' | '\u{ac00}'..='\u{d7af}')), "{line}");
        }
    }

    #[test]
    fn typed_outcomes_rates_windows_and_credit_flags_stay_explicit() {
        let now = Utc.with_ymd_and_hms(2026, 10, 23, 0, 0, 0).unwrap();
        let outcomes = [
            ForecastOutcome::Measuring,
            ForecastOutcome::ResetPending,
            ForecastOutcome::Exhausted,
            ForecastOutcome::NoFiniteForecast,
            ForecastOutcome::NoExhaustionBeforeReset,
            ForecastOutcome::Exhaustion,
        ];
        let windows = outcomes
            .into_iter()
            .enumerate()
            .map(|(i, outcome)| WindowUsageForecast {
                name: format!("w{i}"),
                window_seconds: if i == 0 { 30 } else { 604800 },
                observed_at: now,
                used_pct: 50.,
                reset_at: Some(now + Duration::hours(4)),
                awaiting_usage: outcome == ForecastOutcome::ResetPending,
                recent_pct_per_hour: Some(99.),
                average_pct_per_hour: Some(12.5),
                recent_span_start: Some(now),
                measurement_start: Some(now),
                basis: Some(ForecastBasis::Average),
                exhaustion_at: Some(now + Duration::seconds(9300)),
                seconds_to_exhaustion: Some(9300),
                outcome,
                changing: true,
                truncated: true,
                secondary_recent: Some(UsageProjection {
                    pct_per_hour: 5.5,
                    measurement_start: now,
                    exhaustion_at: None,
                    seconds_to_exhaustion: None,
                    outcome: ForecastOutcome::NoFiniteForecast,
                    changing: true,
                }),
            })
            .collect();
        let mut forecast = AccountUsageForecast {
            as_of: now,
            observed_at: Some(now),
            windows,
            earliest_window: Some("w5".into()),
            truncated: true,
            credit: Some(CreditUsageForecast {
                observed_at: now,
                balance: 0.,
                recent_credits_per_hour: Some(2.5),
                measurement_start: Some(now),
                exhaustion_at: Some(now + Duration::hours(1)),
                seconds_to_exhaustion: Some(3600),
                outcome: ForecastOutcome::Exhausted,
                changing: true,
                truncated: true,
            }),
        };
        let output = lines(&forecast, true);
        no_hangul(&output);
        assert_eq!(output.len(), 14);
        assert!(output[0].starts_with("30s Window average: Measuring"));
        assert!(output[2].contains("Waiting for an observation after reset"));
        assert!(output[4].contains("Exhausted · Resets"));
        assert!(output[6].contains("12.5%/h · No finite exhaustion estimate"));
        assert!(output[8].contains("12.5%/h · No exhaustion before reset"));
        assert!(output[10].contains("7d Window average: 12.5%/h · Exhausts in about 2h 35m"));
        assert!(output[10].contains("Expected to exhaust first"));
        assert!(output[10].contains("Samples summarized"));
        assert_eq!(
            output[1],
            format!(
                "30s at the current rate: 5.5%/h · No finite exhaustion estimate · Resets {} · Changing",
                (now + Duration::hours(4))
                    .with_timezone(&Local)
                    .format("%b %-d %H:%M")
            )
        );
        assert_eq!(output[12], "Continuing with allowed paid credits");
        assert!(output[13].starts_with("Credits exhausted"));
        assert!(!output[13].contains("Exhausts in"));
        assert_eq!(lines(&forecast, false).len(), 12);
        forecast.windows.clear();
        forecast.credit = None;
        assert_eq!(lines(&forecast, false), ["Exhaustion forecast: Measuring"]);
        assert_eq!(duration(Some(61)), "2m");
        assert_eq!(duration(Some(0)), "<1m");
        assert_eq!(window_label(90), "90s");
    }

    #[test]
    fn english_v2_forecast_uses_observed_rates_and_korean_keeps_core_copy() {
        let now = Utc.with_ymd_and_hms(2026, 10, 23, 0, 0, 0).unwrap();
        let mut samples = Forecasts::default();
        for (minute, used) in [(0, 20.), (5, 22.), (10, 24.)] {
            let at = now + Duration::minutes(minute);
            samples.observe("a", "5h", used, Some(now + Duration::hours(3)), at);
            samples.observe_credit("a", 100. - used, at);
        }
        let at = now + Duration::minutes(10);
        let forecast = samples.view("a", at, Some(now));
        assert_eq!(forecast.windows[0].recent_pct_per_hour, Some(24.));
        assert_eq!(
            forecast.credit.as_ref().unwrap().recent_credits_per_hour,
            Some(24.)
        );
        let input = serde_json::json!({"accounts":[{"id":"a","provider":"codex","label":"사용자 {0}","user_state":"active","health":{"kind":"ready"},"source":{"kind":"oauth"},"credits_in_use":true,"usage_forecast":forecast}],"pools":{"codex":{"eligible":1}}}).to_string();
        crate::i18n::with_language(crate::i18n::UiLanguage::En, || {
            let view = crate::v2::project(&input, at.into()).unwrap();
            let row = &view.sections[1].rows[0];
            assert_eq!(row.name, "사용자 {0}");
            no_hangul(&row.forecast_lines);
            assert_eq!(row.forecast_lines.len(), 1);
            assert_eq!(
                row.forecast_lines,
                [qb_core::usage_forecast_display::summary(&forecast, true).text]
            );
            assert!(!row.forecast_warning);
            let output = &row.forecast_tooltip;
            assert!(output.contains("5h Recent rate: 24.0%/h"));
            assert!(output.contains("No exhaustion before reset"));
            assert!(output.contains("Recent credit rate: 24.0/h"));
            assert!(output.contains("Exhausts in about 3h 10m"));
            assert!(output.contains("Continuing with allowed paid credits"));
            let exhausted_at = forecast.credit.as_ref().unwrap().exhaustion_at.unwrap();
            let expected_date = exhausted_at
                .with_timezone(&Local)
                .format("%b %-d %H:%M")
                .to_string();
            assert!(output.contains(&expected_date));
            assert!(
                lines(&forecast, false)
                    .iter()
                    .all(|line| !line.to_lowercase().contains("credit"))
            );
        });
        crate::i18n::with_language(crate::i18n::UiLanguage::Ko, || {
            let view = crate::v2::project(&input, at.into()).unwrap();
            assert_eq!(
                view.sections[1].rows[0].forecast_lines,
                [qb_core::usage_forecast_display::summary(&forecast, false).text]
            );
            assert_eq!(
                view.sections[1].rows[0].forecast_tooltip,
                qb_core::usage_forecast_display::lines(&forecast, true).join("\n")
            );
        });
    }
}
