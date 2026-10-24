//! D28 part 1: individual subscription estimates and observed recovery only.
//! No summing percentages, pooled runway, rate calculation, IO, or hidden clock.
use super::{
    AccountRow, account_cooling, actual_credits, provider_label, summary_forecast, timestamp,
};
use chrono::{DateTime, Utc};
use qb_core::usage_forecast_display::{forecast_clock, time_left};

pub(super) fn line(
    provider: &str,
    rows: &[AccountRow],
    eligible: Option<u64>,
    now: DateTime<Utc>,
    english: bool,
) -> String {
    use qb_core::{
        accounts::{Provider, UserState},
        provider_forecast::{ProviderAccount, SubscriptionOutlook},
    };
    let provider_kind = match provider {
        "claude" => Provider::Claude,
        "codex" => Provider::Codex,
        _ => return provider_label(provider).to_owned(),
    };
    let forecasts: Vec<_> = rows
        .iter()
        .map(|row| summary_forecast(&row.raw, now))
        .collect();
    let accounts: Vec<_> = rows
        .iter()
        .zip(&forecasts)
        .map(|(row, forecast)| {
            let a = &row.raw;
            ProviderAccount {
                id: &a.id,
                name: &row.name,
                user_state: if a.user_state == "active" {
                    UserState::Active
                } else {
                    UserState::Paused
                },
                drain: a.drain,
                usable: a.pool_eligible == Some(true)
                    && a.health.kind == "ready"
                    && !a.probe_pending
                    && !a.refresh_attention,
                cooling_until: if account_cooling(a) {
                    a.health.until.as_deref().and_then(timestamp)
                } else {
                    None
                },
                allow_credits: a.allow_credits,
                credits_in_use: actual_credits(a),
                credits_unlimited: a.credits.as_ref().and_then(|c| c.unlimited),
                forecast: Some(forecast),
            }
        })
        .collect();
    let summary = qb_core::provider_forecast::summarize(provider_kind, &accounts, eligible, now);
    let total = summary.configured;
    let eligible = summary.usable;
    let measured = summary.coverage.measured;
    let enabled = summary.coverage.total;
    let recovery = summary.earliest_recovery;
    let credit_first = summary.credit_continuations.first();
    let credit_count = summary.credit_continuations.len();
    let count = match (english, eligible) {
        (true, Some(n)) => format!("{n} of {total} usable"),
        (false, Some(n)) => format!("{total}개 중 {n}개 사용 가능"),
        (true, None) => format!("Usable unknown / {total} accounts"),
        (false, None) => format!("전체 {total}개 · 사용 가능 확인 전"),
    };
    let mut result = format!("{} · {count}", provider_label(provider));
    result.push_str(&if english {
        format!(" · forecast measured {measured}/{enabled} accounts")
    } else {
        format!(" · 예측 측정 {measured}/{enabled} 계정")
    });
    if let SubscriptionOutlook::Next(next) = &summary.subscription {
        // The known reset of the next account that will actually run out is
        // relevant recovery; unrelated healthy-window resets are not.
        let left = time_left(now, next.at, english).expect("core selected future exhaustion");
        let clock = forecast_clock(now, next.at, english);
        result.push_str(&if english {
            format!(
                " · next out: {} in {} ({clock})",
                next.account.name,
                left.strip_suffix(" left").unwrap_or(&left)
            )
        } else {
            format!(
                " · 다음 소진 {} {} 후 ({clock})",
                next.account.name,
                left.strip_suffix(" 남음").unwrap_or(&left)
            )
        });
    } else if summary.subscription == SubscriptionOutlook::NoExhaustionBeforeReset {
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
        let runway = credit_runway(&row.runway, now, english);
        result.push_str(&if english {
            format!(" · credits continue on {} ({runway})", row.account.name)
        } else {
            format!(" · {} 크레딧으로 계속 사용 ({runway})", row.account.name)
        });
        if credit_count > 1 {
            result.push_str(&if english {
                format!(" +{}", credit_count - 1)
            } else {
                format!(" 외 {}개", credit_count - 1)
            });
        }
    }
    if measured < enabled {
        result.push_str(&if english {
            let missing = enabled - measured;
            format!(
                " ({missing} account{} measuring)",
                if missing == 1 { "" } else { "s" }
            )
        } else {
            format!(" ({}개 계정 측정 중)", enabled - measured)
        });
    }
    result
}

fn credit_runway(
    runway: &qb_core::provider_forecast::CreditRunway,
    now: DateTime<Utc>,
    english: bool,
) -> String {
    use qb_core::{provider_forecast::CreditRunway, usage_forecast::ForecastOutcome};
    match runway {
        CreditRunway::Unknown => if english {
            "remaining time unknown"
        } else {
            "잔여시간 확인 전"
        }
        .into(),
        CreditRunway::Unlimited => if english { "unlimited" } else { "무제한" }.into(),
        CreditRunway::Forecast(forecast) => {
            if let Some(at) = forecast.exhaustion_at.filter(|at| *at > now) {
                format!(
                    "{} ({})",
                    time_left(now, at, english).expect("future credit exhaustion"),
                    forecast_clock(now, at, english)
                )
            } else if forecast.outcome == ForecastOutcome::Exhausted {
                if english { "exhausted" } else { "소진" }.into()
            } else {
                if english {
                    "no finite estimate"
                } else {
                    "유한 예측 없음"
                }
                .into()
            }
        }
    }
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
        AccountUsageForecast, ForecastBasis, ForecastOutcome, UsageProjection, WindowUsageForecast,
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
            assert!(!text.contains(if english {
                "accounts measuring"
            } else {
                "개 계정 측정 중"
            }));
            assert!(text.contains(&format!(
                "({})",
                forecast_clock(now(), now() + Duration::minutes(35), english)
            )));
            assert!(text.contains(if english { "in 35m" } else { "35분 후" }));
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
                "2 accounts measuring"
            } else {
                "2개 계정 측정 중"
            }));
            assert!(text.contains(if english { "Measuring" } else { "측정 중" }));
            assert!(!text.contains(if english {
                "next out:"
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
                let clock = forecast_clock(now(), now() + Duration::minutes(10), english);
                let expected_next = if english {
                    format!("next out: next in 10m ({clock})")
                } else {
                    format!("다음 소진 next 10분 후 ({clock})")
                };
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
            assert!(!text.contains(if english {
                "accounts measuring"
            } else {
                "개 계정 측정 중"
            }));
            assert!(!text.contains(if english {
                "credits continue"
            } else {
                "크레딧으로 계속"
            }));
            assert!(text.contains(if english { "in 10m" } else { "10분 후" }));
            let mut actual = paid.clone();
            actual["credits_in_use"] = json!(true);
            let v = project(vec![actual.clone()], Some(1), language, now());
            let text = &v.summaries[1].forecast_line;
            assert!(!text.contains(if english {
                "accounts measuring"
            } else {
                "개 계정 측정 중"
            }));
            assert!(text.contains(if english {
                "credits continue on paid"
            } else {
                "paid 크레딧으로 계속 사용"
            }));
            assert!(!text.contains(if english {
                "next out:"
            } else {
                "다음 소진 "
            }));
            assert!(!text.contains("in 10m"));
            actual.as_object_mut().unwrap().remove("usage_forecast");
            let v = project(vec![actual], Some(1), language, now());
            let text = &v.summaries[1].forecast_line;
            assert!(text.contains(if english {
                "1 account measuring"
            } else {
                "1개 계정 측정 중"
            }));
            assert!(text.contains(if english {
                "credits continue on paid"
            } else {
                "paid 크레딧으로 계속 사용"
            }));
            assert!(!text.contains(if english {
                "next out:"
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
        assert!(forward.summaries[1].forecast_line.contains("in 1h 30m"));
        assert!(
            forward.summaries[1]
                .forecast_line
                .contains("next out: a in")
        );
        let aged = project(
            vec![a],
            Some(1),
            UiLanguage::En,
            now() + Duration::minutes(6),
        );
        assert!(
            aged.summaries[1]
                .forecast_line
                .contains("1 account measuring")
        );
        assert_eq!(aged.sections[1].rows[0].forecast_lines, ["Measuring"]);
    }

    #[test]
    fn r163_exhausted_account_needs_no_rate_samples_and_counts_as_measured() {
        let mut exhausted = window("7d", None);
        exhausted.used_pct = 100.;
        exhausted.outcome = ForecastOutcome::Exhausted;
        exhausted.observed_at = now() - Duration::hours(2);
        exhausted.reset_at = Some(now() + Duration::days(2));
        let mut unknown = window("5h", None);
        unknown.outcome = ForecastOutcome::Measuring;
        let spent = account("sample-spent", vec![unknown, exhausted]);
        for language in [UiLanguage::Ko, UiLanguage::En] {
            let english = language == UiLanguage::En;
            let view = project(vec![spent.clone()], Some(0), language, now());
            let row = &view.sections[1].rows[0];
            assert!(row.forecast_lines[0].contains(if english {
                "Out until"
            } else {
                "까지 소진"
            }));
            let provider = &view.summaries[1].forecast_line;
            assert!(!provider.contains(if english {
                "account measuring"
            } else {
                "계정 측정 중"
            }));
            assert!(provider.contains(&forecast_clock(now(), now() + Duration::days(2), english)));
        }
    }

    #[test]
    fn r163_provider_recovery_uses_latest_blocker_per_account_then_earliest_account() {
        let short_reset = now() + Duration::hours(1);
        let weekly_reset = now() + Duration::days(2);
        let mut short = window("5h", None);
        short.used_pct = 100.;
        short.outcome = ForecastOutcome::Exhausted;
        short.reset_at = Some(short_reset);
        let mut weekly = window("7d", None);
        weekly.used_pct = 100.;
        weekly.outcome = ForecastOutcome::Exhausted;
        weekly.reset_at = Some(weekly_reset);
        let mut blocked = account("sample-mixed", vec![short.clone(), weekly]);
        blocked["health"] =
            json!({"kind":"cooling","scope":"account","reason":"quota","until":short_reset});
        blocked["pool_eligible"] = json!(false);

        let other_reset = now() + Duration::hours(3);
        short.reset_at = Some(other_reset);
        let mut other = account("sample-other", vec![short]);
        other["health"] =
            json!({"kind":"cooling","scope":"account","reason":"quota","until":other_reset});
        other["pool_eligible"] = json!(false);
        let longer_health = now() + Duration::days(3);
        let mut longer = blocked.clone();
        longer["health"]["until"] = json!(longer_health);

        for language in [UiLanguage::Ko, UiLanguage::En] {
            let english = language == UiLanguage::En;
            for (mut accounts, expected) in [
                (vec![blocked.clone()], weekly_reset),
                (vec![blocked.clone(), other.clone()], other_reset),
                (vec![longer.clone()], longer_health),
            ] {
                let forward = project(accounts.clone(), Some(0), language, now());
                accounts.reverse();
                let reverse = project(accounts, Some(0), language, now());
                let text = &forward.summaries[1].forecast_line;
                assert_eq!(text, &reverse.summaries[1].forecast_line);
                let expected = format!(
                    "{} {}",
                    if english {
                        "earliest recovery"
                    } else {
                        "가장 빠른 회복"
                    },
                    forecast_clock(now(), expected, english),
                );
                assert!(text.ends_with(&expected), "{text}; expected {expected}");
            }
        }
    }
    #[test]
    fn provider_coverage_always_names_measured_and_active_non_drained_total() {
        for language in [UiLanguage::En, UiLanguage::Ko] {
            let english = language == UiLanguage::En;
            for measured in 0..=2 {
                let mut a = account("a", vec![window("5h", Some(10))]);
                let mut b = account("b", vec![window("5h", Some(20))]);
                if measured < 1 {
                    a.as_object_mut().unwrap().remove("usage_forecast");
                }
                if measured < 2 {
                    b.as_object_mut().unwrap().remove("usage_forecast");
                }
                let mut paused = account("paused", vec![window("5h", Some(1))]);
                paused["user_state"] = json!("paused");
                let mut disabled = paused.clone();
                disabled["id"] = json!("disabled");
                disabled["user_state"] = json!("disabled");
                let mut drained = account("drained", vec![window("5h", Some(1))]);
                drained["drain"] = json!(true);
                let view = project(
                    vec![a, b, paused, disabled, drained],
                    Some(2),
                    language,
                    now(),
                );
                let text = &view.summaries[1].forecast_line;
                let expected = if english {
                    format!("forecast measured {measured}/2 accounts")
                } else {
                    format!("예측 측정 {measured}/2 계정")
                };
                assert_eq!(text.matches(expected.as_str()).count(), 1, "{text}");
                assert!(text.contains(if english {
                    "2 of 5 usable"
                } else {
                    "5개 중 2개 사용 가능"
                }));
            }
            let empty = line("codex", &[], None, now(), english);
            assert!(empty.contains(if english {
                "forecast measured 0/0 accounts"
            } else {
                "예측 측정 0/0 계정"
            }));
        }
    }

    #[test]
    fn first_credit_runway_precedes_other_accounts_and_ignores_input_order() {
        use qb_core::usage_forecast::CreditUsageForecast;

        let mut a = account("a", vec![window("5h", Some(10))]);
        a["allow_credits"] = json!(true);
        a["credits_in_use"] = json!(true);
        a["credits"] = json!({"unlimited":true});
        let mut b = account("b", vec![window("5h", Some(20))]);
        b["allow_credits"] = json!(true);
        b["credits_in_use"] = json!(true);
        b["usage_forecast"]["credit"] = json!(CreditUsageForecast {
            observed_at: now(),
            balance: 10.0,
            recent_credits_per_hour: Some(30.0),
            measurement_start: Some(now() - Duration::minutes(10)),
            exhaustion_at: Some(now() + Duration::minutes(20)),
            seconds_to_exhaustion: Some(1200),
            outcome: ForecastOutcome::Exhaustion,
            changing: false,
            truncated: false,
        });
        for language in [UiLanguage::En, UiLanguage::Ko] {
            let forward = project(vec![a.clone(), b.clone()], Some(2), language, now());
            let reverse = project(vec![b.clone(), a.clone()], Some(2), language, now());
            let text = &forward.summaries[1].forecast_line;
            assert_eq!(text, &reverse.summaries[1].forecast_line);
            assert!(
                text.contains(if language == UiLanguage::En {
                    "credits continue on a (unlimited) +1"
                } else {
                    "a 크레딧으로 계속 사용 (무제한) 외 1개"
                }),
                "{text}"
            );
            assert!(!text.contains("+1 (unlimited)"));
            assert!(!text.contains("외 1개 크레딧으로 계속 사용 (무제한)"));
            // The displayed monetary detail belongs to a, not b's finite ETA.
            assert!(!text.contains("20m left"));
            assert!(!text.contains("20분 남음"));
        }
    }
}
