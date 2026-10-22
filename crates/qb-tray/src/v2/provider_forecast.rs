//! D28 part 1: individual subscription estimates and observed recovery only.
//! No summing percentages, pooled runway, rate calculation, IO, or hidden clock.
use super::{
    AccountRow, account_cooling, actual_credits, provider_label, summary_forecast, timestamp,
};
use chrono::{DateTime, Utc};
use qb_core::{
    usage_forecast::ForecastOutcome,
    usage_forecast_display::{forecast_clock, limiting_window, time_left},
};

struct Next<'a> {
    at: DateTime<Utc>,
    reset: DateTime<Utc>,
    row: &'a AccountRow,
}

pub(super) fn line(
    provider: &str,
    rows: &[AccountRow],
    eligible: Option<u64>,
    now: DateTime<Utc>,
    english: bool,
) -> String {
    // Usable m includes every configured account, including cooling/disabled.
    // Forecast coverage m is enabled, non-drained accounts, including cooling
    // and unknown/relogin rows. They never silently disappear from coverage.
    let mut enabled = 0usize;
    let mut measured = 0usize;
    let mut safe_usable = 0u64;
    let mut next: Option<Next<'_>> = None;
    let mut recovery = None;
    let mut credit_first: Option<&AccountRow> = None;
    let mut credit_count = 0usize;
    for row in rows {
        let a = &row.raw;
        if a.user_state != "active" || a.drain {
            continue;
        }
        enabled += 1;
        if account_cooling(a)
            && let Some(at) = a
                .health
                .until
                .as_deref()
                .and_then(timestamp)
                .filter(|at| *at > now)
        {
            recovery = Some(recovery.map_or(at, |previous: DateTime<Utc>| previous.min(at)));
        }
        // The pool count is authoritative; per-account pool membership is
        // required to attach a future exhaustion or credit label to a name.
        let usable = a.pool_eligible == Some(true)
            && a.health.kind == "ready"
            && !a.probe_pending
            && !a.refresh_attention;
        let forecast = summary_forecast(a, now);
        let window = limiting_window(&forecast);
        // Coverage describes the measured subscription forecast even when
        // permitted credits keep that account serving after it runs out.
        measured += usize::from(window.is_some());
        if usable && actual_credits(a) && a.allow_credits {
            credit_count += 1;
            if credit_first.is_none_or(|first| (&row.name, &a.id) < (&first.name, &first.raw.id)) {
                credit_first = Some(row);
            }
            // A spent subscription window does not exhaust an account that
            // is actually continuing on permitted credits. Credit runway
            // remains separate detail, not invented subscription capacity.
            continue;
        }
        let Some(window) = window else {
            continue;
        };
        if !usable {
            continue;
        }
        if window.outcome == ForecastOutcome::NoExhaustionBeforeReset {
            safe_usable += 1;
        }
        if window.outcome != ForecastOutcome::Exhaustion {
            continue;
        }
        let (Some(at), Some(reset)) = (window.exhaustion_at, window.reset_at) else {
            continue;
        };
        if next.as_ref().is_none_or(|previous| {
            (at, &row.name, &a.id) < (previous.at, &previous.row.name, &previous.row.raw.id)
        }) {
            next = Some(Next { at, reset, row });
        }
    }
    let total = rows.len();
    let eligible = if rows.is_empty() { Some(0) } else { eligible };
    let count = match (english, eligible) {
        (true, Some(n)) => format!("{n} of {total} usable"),
        (false, Some(n)) => format!("{total}개 중 {n}개 사용 가능"),
        (true, None) => format!("Usable unknown / {total} accounts"),
        (false, None) => format!("전체 {total}개 · 사용 가능 확인 전"),
    };
    let coverage = if english {
        format!("measured {measured}/{enabled} accounts")
    } else {
        format!("측정 {measured}/{enabled} 계정")
    };
    let mut result = format!("{} · {count} · {coverage}", provider_label(provider));
    if let Some(next) = next {
        // The known reset of the next account that will actually run out is
        // relevant recovery; unrelated healthy-window resets are not.
        recovery = Some(recovery.map_or(next.reset, |at| at.min(next.reset)));
        let left = time_left(now, next.at, english).expect("core selected future exhaustion");
        let clock = forecast_clock(now, next.at, english);
        result.push_str(&if english {
            format!(" · {left} · next runs out {clock} ({})", next.row.name)
        } else {
            format!(" · {left} · 다음 소진 {clock} ({})", next.row.name)
        });
    } else if eligible.is_some_and(|n| n > 0 && n == safe_usable) {
        result.push_str(if english {
            " · no exhaustion before reset"
        } else {
            " · 초기화 전 소진 없음"
        });
    } else {
        result.push_str(if english {
            " · next exhaustion: Measuring"
        } else {
            " · 다음 소진: 측정 중"
        });
    }
    if let Some(at) = recovery {
        let clock = forecast_clock(now, at, english);
        result.push_str(&if english {
            format!(" · earliest recovery {clock}")
        } else {
            format!(" · 가장 빠른 회복 {clock}")
        });
    }
    if let Some(row) = credit_first {
        result.push_str(&match (english, credit_count) {
            (true, 1) => format!(" · credits continue on {}", row.name),
            (true, n) => format!(" · credits continue on {} +{}", row.name, n - 1),
            (false, 1) => format!(" · {} 크레딧으로 계속 사용", row.name),
            (false, n) => format!(" · {} 외 {}개 크레딧으로 계속 사용", row.name, n - 1),
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        i18n::{UiLanguage, with_language},
        v2,
    };
    use chrono::{Duration, TimeZone};
    use qb_core::usage_forecast::{
        AccountUsageForecast, ForecastBasis, UsageProjection, WindowUsageForecast,
    };
    use serde_json::{Value, json};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 23, 0, 0, 0).unwrap()
    }
    fn window(name: &str, minutes: Option<i64>) -> WindowUsageForecast {
        WindowUsageForecast {
            name: name.into(),
            window_seconds: if name == "7d" { 604800 } else { 18000 },
            observed_at: now(),
            used_pct: 50.,
            reset_at: Some(now() + Duration::days(3)),
            awaiting_usage: false,
            recent_pct_per_hour: Some(12.),
            average_pct_per_hour: Some(1.),
            recent_span_start: Some(now() - Duration::minutes(10)),
            measurement_start: Some(now() - Duration::minutes(10)),
            basis: Some(ForecastBasis::Average),
            exhaustion_at: minutes.map(|n| now() + Duration::minutes(n)),
            seconds_to_exhaustion: minutes.map(|n| n * 60),
            outcome: if minutes.is_some() {
                ForecastOutcome::Exhaustion
            } else {
                ForecastOutcome::NoExhaustionBeforeReset
            },
            changing: true,
            secondary_recent: None,
            truncated: false,
        }
    }
    fn account(name: &str, windows: Vec<WindowUsageForecast>) -> Value {
        let raw_windows: Vec<_> = windows.iter().map(|w| json!({"name":w.name,"scope":"account","used_pct":w.used_pct,"reset_at":w.reset_at})).collect();
        let forecast = AccountUsageForecast {
            as_of: now(),
            observed_at: Some(now()),
            windows,
            earliest_window: Some("untrusted-cache".into()),
            credit: None,
            truncated: false,
        };
        json!({"id":name,"label":name,"provider":"codex","user_state":"active","health":{"kind":"ready"},"source":{"kind":"oauth"},"pool_eligible":true,"windows":raw_windows,"usage_forecast":forecast})
    }
    fn project(
        accounts: Vec<Value>,
        eligible: Option<u64>,
        language: UiLanguage,
        at: DateTime<Utc>,
    ) -> v2::View {
        with_language(language, || {
            v2::project(
                &json!({"accounts":accounts,"pools":{"codex":{"eligible":eligible}}}).to_string(),
                at.into(),
            )
            .unwrap()
        })
    }

    #[test]
    fn r162_provider_competing_windows_coverage_recovery_and_both_languages() {
        let mut max = account("max", vec![window("5h", Some(90)), window("7d", Some(35))]);
        // The detailed recent-rate counterfactual cannot replace the chosen average.
        max["usage_forecast"]["windows"][1]["secondary_recent"] =
            serde_json::to_value(UsageProjection {
                pct_per_hour: 99.,
                measurement_start: now(),
                exhaustion_at: Some(now() + Duration::minutes(1)),
                seconds_to_exhaustion: Some(60),
                outcome: ForecastOutcome::Exhaustion,
                changing: true,
            })
            .unwrap();
        let mut exhausted = window("5h", None);
        exhausted.used_pct = 100.;
        exhausted.outcome = ForecastOutcome::Exhausted;
        exhausted.reset_at = Some(now() + Duration::minutes(15));
        let mut cooling = account("cooling", vec![exhausted]);
        cooling["health"] = json!({"kind":"cooling","scope":"account","reason":"quota","until":now()+Duration::minutes(15)});
        cooling["pool_eligible"] = json!(false);
        let mut disabled = account("disabled", vec![window("5h", Some(1))]);
        disabled["user_state"] = json!("disabled");
        for language in [UiLanguage::En, UiLanguage::Ko] {
            let v = project(
                vec![
                    max.clone(),
                    account("other", vec![window("5h", Some(70))]),
                    cooling.clone(),
                    disabled.clone(),
                ],
                Some(2),
                language,
                now(),
            );
            let text = &v.summaries[1].forecast_line;
            let english = language == UiLanguage::En;
            assert!(text.contains(if english {
                "2 of 4 usable"
            } else {
                "4개 중 2개 사용 가능"
            }));
            assert!(text.contains(if english {
                "measured 3/3 accounts"
            } else {
                "측정 3/3 계정"
            }));
            assert!(text.contains(&format!(
                "{} ({})",
                forecast_clock(now(), now() + Duration::minutes(35), english),
                "max"
            )));
            assert!(text.contains(if english { "35m left" } else { "35분 남음" }));
            assert!(text.contains(&forecast_clock(
                now(),
                now() + Duration::minutes(15),
                english
            )));
            assert!(!text.contains("%/"));
            let row = v.sections[1]
                .rows
                .iter()
                .find(|r| r.raw.id == "max")
                .unwrap();
            assert_eq!(row.forecast_lines.len(), 1);
            assert!(row.forecast_warning);
            assert_eq!(
                row.accessible_name
                    .matches(row.forecast_lines[0].as_str())
                    .count(),
                1
            );
            assert!(row.forecast_tooltip.contains(if english {
                "current rate"
            } else {
                "지금 속도면"
            }));
        }
    }

    #[test]
    fn r162_provider_unknown_no_accounts_and_inactive_rows_never_invent_capacity() {
        for language in [UiLanguage::En, UiLanguage::Ko] {
            let english = language == UiLanguage::En;
            assert!(
                line("claude", &[], None, now(), english).contains(if english {
                    "0 of 0 usable"
                } else {
                    "0개 중 0개 사용 가능"
                })
            );
            let mut cold = account("cold", vec![]);
            cold.as_object_mut().unwrap().remove("usage_forecast");
            let mut accounts = vec![cold];
            for state in ["paused", "disabled", "needs_relogin"] {
                let mut a = account(state, vec![window("5h", Some(1))]);
                if state == "needs_relogin" {
                    a["health"] = json!({"kind":state});
                } else {
                    a["user_state"] = json!(state);
                }
                accounts.push(a);
            }
            let v = project(accounts, None, language, now());
            let text = &v.summaries[1].forecast_line;
            assert!(text.contains(if english {
                "Usable unknown / 4 accounts"
            } else {
                "전체 4개 · 사용 가능 확인 전"
            }));
            assert!(text.contains(if english {
                "measured 0/2 accounts"
            } else {
                "측정 0/2 계정"
            }));
            assert!(text.contains(if english { "Measuring" } else { "측정 중" }));
            assert!(!text.contains(if english {
                "next runs out"
            } else {
                "다음 소진 "
            }));
            for row in v.sections[1]
                .rows
                .iter()
                .chain(&v.sections[1].disabled_rows)
            {
                assert_eq!(
                    row.forecast_lines,
                    [if english { "Measuring" } else { "측정 중" }]
                );
                assert!(!row.forecast_warning);
            }
        }
    }

    #[test]
    fn r162_provider_recovery_uses_exhausted_or_next_account_only() {
        let mut next_window = window("5h", Some(10));
        next_window.reset_at = Some(now() + Duration::hours(6));
        let next = account("next", vec![next_window]);
        let mut later_window = window("5h", Some(20));
        later_window.reset_at = Some(now() + Duration::hours(1));
        let later = account("later", vec![later_window]);
        let mut exhausted = window("5h", None);
        exhausted.used_pct = 100.;
        exhausted.outcome = ForecastOutcome::Exhausted;
        exhausted.reset_at = Some(now() + Duration::minutes(30));
        let mut cooling = account("cooling", vec![exhausted]);
        cooling["health"] = json!({"kind":"cooling","scope":"account","reason":"quota","until":now()+Duration::minutes(30)});
        cooling["pool_eligible"] = json!(false);
        for language in [UiLanguage::En, UiLanguage::Ko] {
            let english = language == UiLanguage::En;
            for with_cooling in [false, true] {
                let mut accounts = vec![next.clone(), later.clone()];
                if with_cooling {
                    accounts.push(cooling.clone());
                }
                let recovery = if with_cooling {
                    now() + Duration::minutes(30)
                } else {
                    now() + Duration::hours(6)
                };
                let expected_recovery = format!(
                    "{} {}",
                    if english {
                        "earliest recovery"
                    } else {
                        "가장 빠른 회복"
                    },
                    forecast_clock(now(), recovery, english)
                );
                let expected_next = format!(
                    "{} {} (next)",
                    if english {
                        "next runs out"
                    } else {
                        "다음 소진"
                    },
                    forecast_clock(now(), now() + Duration::minutes(10), english)
                );
                let forward = project(accounts.clone(), Some(2), language, now());
                accounts.reverse();
                let reverse = project(accounts, Some(2), language, now());
                let text = &forward.summaries[1].forecast_line;
                assert!(text.contains(&expected_next));
                assert_eq!(text.rsplit(" · ").next(), Some(expected_recovery.as_str()));
                assert_eq!(text, &reverse.summaries[1].forecast_line);
            }
        }
    }

    #[test]
    fn r162_provider_credit_permission_is_not_actual_continuation() {
        let mut paid = account("paid", vec![window("5h", Some(10))]);
        paid["allow_credits"] = json!(true);
        paid["credits"] = json!({"has_credits":true,"unlimited":true});
        for language in [UiLanguage::En, UiLanguage::Ko] {
            let english = language == UiLanguage::En;
            let v = project(vec![paid.clone()], Some(1), language, now());
            let text = &v.summaries[1].forecast_line;
            assert!(text.contains(if english {
                "measured 1/1 accounts"
            } else {
                "측정 1/1 계정"
            }));
            assert!(!text.contains(if english {
                "credits continue"
            } else {
                "크레딧으로 계속"
            }));
            assert!(text.contains(if english { "10m left" } else { "10분 남음" }));
            let mut actual = paid.clone();
            actual["credits_in_use"] = json!(true);
            let v = project(vec![actual.clone()], Some(1), language, now());
            let text = &v.summaries[1].forecast_line;
            assert!(text.contains(if english {
                "measured 1/1 accounts"
            } else {
                "측정 1/1 계정"
            }));
            assert!(text.contains(if english {
                "credits continue on paid"
            } else {
                "paid 크레딧으로 계속 사용"
            }));
            assert!(!text.contains(if english {
                "next runs out"
            } else {
                "다음 소진 "
            }));
            assert!(!text.contains("10m left"));
            actual.as_object_mut().unwrap().remove("usage_forecast");
            let v = project(vec![actual], Some(1), language, now());
            let text = &v.summaries[1].forecast_line;
            assert!(text.contains(if english {
                "measured 0/1 accounts"
            } else {
                "측정 0/1 계정"
            }));
            assert!(text.contains(if english {
                "credits continue on paid"
            } else {
                "paid 크레딧으로 계속 사용"
            }));
            assert!(!text.contains(if english {
                "next runs out"
            } else {
                "다음 소진 "
            }));
        }
    }

    #[test]
    fn r162_provider_scope_clock_and_input_order_are_respected() {
        let mut a = account("a", vec![window("5h", Some(90)), window("7d", Some(45))]);
        a["windows"][1]["scope"] = json!("model:fixture");
        let b = account("b", vec![window("5h", Some(90))]);
        let forward = project(vec![a.clone(), b.clone()], Some(2), UiLanguage::En, now());
        let reverse = project(vec![b, a.clone()], Some(2), UiLanguage::En, now());
        assert_eq!(
            forward.summaries[1].forecast_line,
            reverse.summaries[1].forecast_line
        );
        assert!(forward.summaries[1].forecast_line.contains("1h 30m left"));
        assert!(forward.summaries[1].forecast_line.contains("(a)"));
        let aged = project(
            vec![a],
            Some(1),
            UiLanguage::En,
            now() + Duration::minutes(6),
        );
        assert!(
            aged.summaries[1]
                .forecast_line
                .contains("measured 0/1 accounts")
        );
        assert_eq!(aged.sections[1].rows[0].forecast_lines, ["Measuring"]);
    }
}
