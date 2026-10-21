//! Deterministic large dashboard input. No account store, profile, or HTTP.
use chrono::{DateTime, Duration, Utc};
use qb_tray::{dashboard_model as model, v2};
use serde_json::{Value, json};

pub const ACCOUNTS: usize = 30;
pub const SESSIONS: usize = 40;
pub const POINTS_PER_WINDOW: usize = 2017;

pub fn clock() -> model::Clock {
    model::Clock {
        now: DateTime::parse_from_rfc3339("2026-10-21T12:00:00+09:00").unwrap(),
        day_start: DateTime::parse_from_rfc3339("2026-10-21T00:00:00+09:00")
            .unwrap()
            .with_timezone(&Utc),
        day_end: DateTime::parse_from_rfc3339("2026-10-22T00:00:00+09:00")
            .unwrap()
            .with_timezone(&Utc),
    }
}

pub fn input() -> Value {
    let now = clock().utc();
    let ids: Vec<_> = (0..ACCOUNTS)
        .map(|i| format!("{}-perf-{i:02}", if i % 2 == 0 { "cx" } else { "cl" }))
        .collect();
    let accounts: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let active = 1 + usize::from(i < SESSIONS - ACCOUNTS);
            json!({"id":id,"provider":if i%2==0 {"codex"} else {"claude"},
            "label":format!("합성 성능 계정 {i:02} · representative account label"),
            "email":format!("perf-{i:02}@example.invalid"),"plan":"plus",
            "user_state":"active","source":{"kind":"oauth"},"health":{"kind":"ready"},
            "pool_eligible":true,"eligible_for_new_session":true,
            "active_sessions":active,"bound_sessions":active,"spread_threshold":5,
            "observed_at":now,"windows":[
                {"name":"5h","used_pct":35+i,"reset_at":now+Duration::hours(3),"observed_at":now},
                {"name":"7d","used_pct":20+i,"reset_at":now+Duration::days(2),"observed_at":now}]})
        })
        .collect();
    let sessions: Vec<_> = (0..SESSIONS).map(|i| {
        let account = i % ACCOUNTS;
        json!({"id":format!("{i:012x}"),"account":ids[account],
            "provider":if account.is_multiple_of(2) {"codex"} else {"claude"},
            "client":{"kind":if account.is_multiple_of(2) {"codex_cli"} else {"claude_cli"}},
            "label":{"text":format!("합성 session {i:02} · dashboard performance"),"source":"codex_workspace"},
            "first_seen_at":now-Duration::hours(2),"bound_since":now-Duration::hours(1),
            "last_used_at":now-Duration::seconds(20),"active":true,
            "model":if account.is_multiple_of(2) {"synthetic-codex"} else {"synthetic-claude"},
            "inflight":{"requests":0,"ws_open":0},"inflight_known":true,
            "requests_today":100+i,"failures_today":0,"subagents_today":2,
            "subagents_known":true,"moves_known":true,"counters_complete":true})
    }).collect();
    let histories: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let windows: Vec<_> = ["5h", "7d"]
                .iter()
                .map(|name| {
                    let points: Vec<_> = (0..POINTS_PER_WINDOW)
                        .map(|n| {
                            let at =
                                now - Duration::minutes(((POINTS_PER_WINDOW - 1 - n) * 5) as i64);
                            let value = if n % 127 == 0 {
                                Value::Null
                            } else {
                                json!((n + i) % 101)
                            };
                            json!([at, value])
                        })
                        .collect();
                    json!({"name":name,"points":points,"resets":(0..7)
                .map(|d|now-Duration::days(d)).collect::<Vec<_>>(),"truncated":false})
                })
                .collect();
            json!({"id":id,"windows":windows,"credits_spans":if i%4==0 {
            vec![(now-Duration::hours(4),Some(now-Duration::hours(3)))]
        }else{vec![]},"truncated":false})
        })
        .collect();
    let activity: Vec<_> = (0..240)
        .map(|i| {
            json!({"seq":i+1,
        "ts":now-Duration::minutes(i),"kind":"account_changed",
        "provider":if i%2==0 {"codex"} else {"claude"},
        "account":ids[i as usize%ACCOUNTS],"notable":true,
        "data":{"reason":"synthetic performance fixture"}})
        })
        .collect();
    json!({"version":"r1210-perf-v1","accounts":{"accounts":accounts,
        "pools":{"codex":{"eligible":15},"claude":{"eligible":15}}},
        "sessions":{"now":now,"active_window_secs":300,"sessions":sessions,"truncated":false},
        "usage":{"accounts":histories,"truncated":false},"activity":activity})
}

pub fn snapshot(value: &Value) -> model::DashboardSnapshot {
    let mut snapshot = model::DashboardSnapshot::new(
        v2::project(&value["accounts"].to_string(), clock().utc().into()).unwrap(),
    );
    snapshot.sessions = serde_json::from_value(value["sessions"].clone()).unwrap();
    snapshot.usage = serde_json::from_value(value["usage"].clone()).unwrap();
    snapshot.activity = serde_json::from_value(value["activity"].clone()).unwrap();
    snapshot.data_at = Some(clock().utc());
    snapshot.revision = 1;
    assert_eq!(snapshot.accounts.raw.accounts.len(), ACCOUNTS);
    assert_eq!(snapshot.sessions.sessions.len(), SESSIONS);
    assert_eq!(snapshot.usage.accounts.len(), ACCOUNTS);
    assert!(snapshot.usage.accounts.iter().all(|a| {
        a.windows.len() == 2
            && a.windows
                .iter()
                .all(|w| w.points.len() == POINTS_PER_WINDOW)
    }));
    snapshot
}

pub fn manifest(bytes: &[u8]) -> Value {
    // Stable input identity without adding a hashing dependency to qb-tray.
    let hash = bytes.iter().fold(0xcbf29ce484222325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    json!({"generator":"r1210-perf-v1","accounts":ACCOUNTS,"sessions":SESSIONS,
        "windows_per_account":2,"points_per_window":POINTS_PER_WINDOW,
        "total_points":ACCOUNTS*2*POINTS_PER_WINDOW,"history_days":7,"sample_minutes":5,
        "unknown_every":127,"activity_events":240,"input_bytes":bytes.len(),
        "input_fnv1a64":format!("{hash:016x}"),"input_file":"fixture.json"})
}
