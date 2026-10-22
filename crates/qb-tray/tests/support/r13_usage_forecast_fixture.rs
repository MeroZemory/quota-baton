//! Typed synthetic forecast payloads shared by pure layout and owned-HWND tests.
use chrono::{DateTime, Duration, Utc};
use qb_core::usage_forecast::{
    AccountUsageForecast, CreditUsageForecast, ForecastBasis, ForecastOutcome, UsageProjection,
    WindowUsageForecast,
};
use qb_tray::{dashboard_model as m, v2};
use serde_json::json;

pub fn forecast(now: DateTime<Utc>, cold: bool) -> AccountUsageForecast {
    let start = now - Duration::minutes(30);
    let mut result = AccountUsageForecast {
        as_of: now,
        observed_at: Some(now),
        windows: vec![],
        earliest_window: None,
        credit: None,
        truncated: false,
    };
    if cold {
        return result;
    }
    result.windows = vec![
        WindowUsageForecast {
            name: "5h".into(),
            window_seconds: 18_000,
            observed_at: now,
            awaiting_usage: false,
            used_pct: 70.,
            reset_at: Some(now + Duration::hours(4)),
            recent_pct_per_hour: Some(12.),
            average_pct_per_hour: Some(70.),
            recent_span_start: Some(start),
            measurement_start: Some(start),
            basis: Some(ForecastBasis::Recent),
            exhaustion_at: Some(now + Duration::minutes(150)),
            seconds_to_exhaustion: Some(9_000),
            outcome: ForecastOutcome::Exhaustion,
            changing: true,
            secondary_recent: None,
            truncated: false,
        },
        WindowUsageForecast {
            name: "7d".into(),
            window_seconds: 604_800,
            observed_at: now,
            awaiting_usage: false,
            used_pct: 40.,
            reset_at: Some(now + Duration::hours(128)),
            recent_pct_per_hour: Some(20.),
            average_pct_per_hour: Some(1.),
            recent_span_start: Some(start),
            measurement_start: Some(now - Duration::hours(40)),
            basis: Some(ForecastBasis::Average),
            exhaustion_at: Some(now + Duration::hours(60)),
            seconds_to_exhaustion: Some(216_000),
            outcome: ForecastOutcome::Exhaustion,
            changing: true,
            secondary_recent: Some(UsageProjection {
                pct_per_hour: 20.,
                measurement_start: start,
                exhaustion_at: Some(now + Duration::hours(3)),
                seconds_to_exhaustion: Some(10_800),
                outcome: ForecastOutcome::Exhaustion,
                changing: true,
            }),
            truncated: false,
        },
    ];
    result.earliest_window = Some("5h".into());
    result.credit = Some(CreditUsageForecast {
        observed_at: now,
        balance: 80.,
        recent_credits_per_hour: Some(8.),
        measurement_start: Some(start),
        exhaustion_at: Some(now + Duration::hours(10)),
        seconds_to_exhaustion: Some(36_000),
        outcome: ForecastOutcome::Exhaustion,
        changing: true,
        truncated: false,
    });
    result
}

pub fn fixture() -> (m::Clock, m::DashboardSnapshot) {
    let now = DateTime::parse_from_rfc3339("2026-10-22T23:40:00+09:00").unwrap();
    let start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(*now.offset())
        .unwrap()
        .to_utc();
    let clock = m::Clock {
        now,
        day_start: start,
        day_end: start + Duration::days(1),
    };
    let at = clock.utc();
    let payload = json!({"version":"R13-forecast-synthetic", "accounts":[
        {"id":"steady","provider":"codex","label":"합성 사용량 예측","plan":"plus",
         "user_state":"active","health":{"kind":"ready"},"active_sessions":2,"bound_sessions":2,
         "credits_in_use":true,"allow_credits":true,
         "windows":[{"name":"5h","used_pct":70.,"reset_at":at+Duration::hours(4)},
                    {"name":"7d","used_pct":40.,"reset_at":at+Duration::hours(128)}],
         "usage_forecast":forecast(at, false)},
        {"id":"cold","provider":"codex","label":"합성 초기 측정","plan":"free",
         "user_state":"active","health":{"kind":"ready"},"active_sessions":0,
         "windows":[{"name":"5h","used_pct":10.,"reset_at":at+Duration::hours(3)}],
         "usage_forecast":forecast(at, true)}
    ]});
    let mut snapshot =
        m::DashboardSnapshot::new(v2::project(&payload.to_string(), at.into()).unwrap());
    snapshot.sessions = serde_json::from_value(json!({"sessions":[
        {"id":"0123456789ab","provider":"codex","account":"steady","last_used_at":at,
         "first_seen_at":at-Duration::hours(1),"active":true,"client":{"kind":"codex"}}
    ]}))
    .unwrap();
    snapshot.usage.accounts = vec![m::AccountHistory {
        id: "steady".into(),
        windows: vec![
            m::HistoryWindow {
                name: "5h".into(),
                points: vec![
                    (at - Duration::minutes(30), Some(64.)),
                    (at - Duration::minutes(15), None),
                    (at, Some(70.)),
                ],
                ..Default::default()
            },
            m::HistoryWindow {
                name: "7d".into(),
                points: vec![(at - Duration::minutes(30), Some(30.)), (at, Some(40.))],
                ..Default::default()
            },
        ],
        ..Default::default()
    }];
    (clock, snapshot)
}
