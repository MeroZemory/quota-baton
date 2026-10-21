//! Synthetic inputs only. No filesystem, process, transport, or native UI calls.
use crate::{dashboard_model::*, v2};
use chrono::{DateTime, Utc};
use serde_json::json;
pub fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}
pub fn clock() -> Clock {
    Clock {
        now: DateTime::parse_from_rfc3339("2026-10-19T15:42:08+09:00").unwrap(),
        day_start: at("2026-10-18T15:00:00Z"),
        day_end: at("2026-10-19T15:00:00Z"),
    }
}
pub fn snapshot() -> DashboardSnapshot {
    let now = clock();
    let accounts = json!({"version":"synthetic-r7","pools":{"claude":{"eligible":1},"codex":{"eligible":2}},"accounts":[
        {"id":"cl-a","provider":"claude","email":"hello@example.invalid","plan":"max_5x","user_state":"active","health":{"kind":"ready"},"source":"oauth","active_sessions":3,"bound_sessions":4,"spread_threshold":5,"windows":[{"name":"5h","used_pct":66,"reset_at":"2026-10-19T09:20:00Z"},{"name":"7d","used_pct":12}]},
        {"id":"cl-b","provider":"claude","email":"second@example.invalid","plan":"pro","user_state":"active","health":{"kind":"ready"},"source":"oauth","active_sessions":1,"bound_sessions":2,"spread_threshold":5,"windows":[{"name":"7d","used_pct":42}]},
        {"id":"cl-login","provider":"claude","email":"support@example.invalid","user_state":"active","health":{"kind":"needs_relogin"},"source":"oauth","bound_sessions":1},
        {"id":"cl-quota","provider":"claude","email":"quota@example.invalid","user_state":"active","health":{"kind":"cooling","reason":"quota","until":"2026-10-19T08:10:00Z"},"source":"oauth","windows":[{"name":"5h","used_pct":100}]},
        {"id":"cx-a","provider":"codex","email":"mero@example.invalid","plan":"plus","user_state":"active","health":{"kind":"ready"},"source":"takeover","active_sessions":4,"bound_sessions":4,"spread_threshold":5,"windows":[{"name":"5h","used_pct":44},{"name":"7d","used_pct":59}]},
        {"id":"cx-b","provider":"codex","email":"inside@example.invalid","plan":"plus","user_state":"active","health":{"kind":"ready"},"source":"oauth","active_sessions":2,"bound_sessions":2,"spread_threshold":5,"windows":[{"name":"5h","used_pct":28}]},
        {"id":"cx-disabled","provider":"codex","email":"off@example.invalid","user_state":"disabled","health":{"kind":"ready"},"source":"oauth"}
    ]});
    let tray = v2::project(&accounts.to_string(), now.utc().into()).unwrap();
    let mut data = DashboardSnapshot::new(tray);
    data.data_at = Some(now.utc());
    for i in 0..12u64 {
        let provider = if i < 6 { "claude" } else { "codex" };
        let account = if i < 4 {
            "cl-a"
        } else if i < 6 {
            "cl-b"
        } else if i < 10 {
            "cx-a"
        } else {
            "cx-b"
        };
        data.sessions.sessions.push(Session {
            id: format!("{i:012x}"),
            provider: provider.into(),
            account: account.into(),
            client: Client {
                kind: if provider == "claude" {
                    "claude_cli"
                } else {
                    "codex_cli"
                }
                .into(),
                product: None,
                version: Some("synthetic".into()),
            },
            label: Some(Label {
                text: format!("project-{i}"),
                source: if provider == "claude" {
                    "claude_project"
                } else {
                    "codex_workspace"
                }
                .into(),
            }),
            first_seen_at: Some(now.utc() - chrono::Duration::hours(4)),
            last_used_at: Some(now.utc() - chrono::Duration::seconds(i as i64 * 12)),
            active: i < 10,
            inflight: Inflight {
                requests: Some(if i < 4 { 1 } else { 0 }),
                oldest_started_at: (i < 4).then_some(now.utc() - chrono::Duration::seconds(42)),
                ws_open: Some(0),
                ws_response_in_flight: false,
            },
            requests_today: Some(100 + i),
            counters_complete: true,
            subagents_known: true,
            moves_known: true,
            failures_today: Some(0),
            subagents_today: Some(0),
            ..Default::default()
        });
    }
    let kinds = [
        "quota_reached",
        "credits_started",
        "credits_stopped",
        "quota_reset",
        "needs_relogin",
        "link_stale",
        "payment",
        "permission",
        "store_error",
        "move",
        "overload_detected",
        "rate_hold",
        "burst",
        "token_refresh",
        "linked_reload",
        "login",
        "account_changed",
    ];
    data.activity = kinds
        .iter()
        .enumerate()
        .map(|(i, kind)| ActivityEvent {
            seq: i as u64 + 1,
            ts: Some(now.utc() - chrono::Duration::minutes(i as i64)),
            kind: (*kind).into(),
            provider: "claude".into(),
            account: Some(
                if *kind == "needs_relogin" {
                    "cl-login"
                } else {
                    "cl-a"
                }
                .into(),
            ),
            account_to: Some("cl-b".into()),
            notable: *kind != "login",
            data: ActivityData {
                window: Some("5h".into()),
                reset_at: Some(now.utc() + chrono::Duration::hours(1)),
                count: Some(4),
                max_wait_ms: Some(31000),
                moved_groups: Some(2),
                reason: Some("overload".into()),
                session: Some("000000000001".into()),
                label: Some("project-1".into()),
                active_sessions: Some(7),
                threshold: Some(5),
                ok: Some(false),
                ..Default::default()
            },
        })
        .collect();
    data.usage.accounts.push(AccountHistory {
        id: "cx-a".into(),
        windows: vec![
            HistoryWindow {
                name: "5h".into(),
                points: vec![
                    (now.day_start + chrono::Duration::hours(1), Some(10.)),
                    (now.day_start + chrono::Duration::hours(6), Some(90.)),
                    (now.day_start + chrono::Duration::hours(7), Some(0.)),
                    (now.utc(), Some(44.)),
                ],
                resets: vec![now.day_start + chrono::Duration::hours(7)],
                truncated: false,
            },
            HistoryWindow {
                name: "7d".into(),
                points: vec![
                    (now.day_start + chrono::Duration::hours(2), Some(50.)),
                    (now.utc(), Some(59.)),
                ],
                resets: vec![],
                truncated: false,
            },
        ],
        credits_spans: vec![(
            now.day_start + chrono::Duration::hours(6),
            Some(now.day_start + chrono::Duration::hours(7)),
        )],
        truncated: false,
    });
    data
}
