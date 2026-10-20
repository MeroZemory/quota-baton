//! Synthetic DTOs modeled on dashboard-v2/mockup.html, never a real account reader.
use chrono::{DateTime, Duration, Utc};
use qb_tray::{dashboard_model::*, v2};
use serde_json::{Value, json};

pub const SELECTED: &str = "30f878000000";
pub fn clock() -> Clock {
    Clock {
        now: DateTime::parse_from_rfc3339("2026-10-21T06:25:50+09:00").unwrap(),
        day_start: at("2026-10-21T00:00:00+09:00"),
        day_end: at("2026-10-22T00:00:00+09:00"),
    }
}
fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}
fn today(s: &str) -> DateTime<Utc> {
    at(&format!("2026-10-21T{s}:00+09:00"))
}
fn window(name: &str, remaining: u32, reset: &str) -> Value {
    json!({"name":name,"used_pct":100-remaining,"reset_at":at(reset),
        "observed_at":clock().utc()})
}
fn account(id: &str, name: &str, plan: &str, active: u64, windows: Vec<Value>) -> Value {
    json!({"id":id,"provider":if id.starts_with("cl-") {"claude"} else {"codex"},
        "email":format!("{name}@example.invalid"),"label":name,"plan":plan,
        "user_state":"active","health":{"kind":"ready"},"source":"oauth",
        "active_sessions":active,"bound_sessions":active,"spread_threshold":5,
        "windows":windows,"observed_at":clock().utc()})
}
fn limited(a: &mut Value, window: &str, reset: &str) {
    a["health"] = json!({"kind":"cooling","reason":"quota","scope":"account","until":at(reset)});
    a["quota_cooling"] = json!({"window":window,"reset_at":at(reset),"started_at":today("03:42")});
}
fn accounts(state: u8) -> Vec<Value> {
    let mut a = vec![
        account(
            "cl-mero",
            "acct-alpha",
            "max_20x",
            6,
            vec![
                window("5h", 72, "2026-10-21T07:20:00+09:00"),
                window("7d", 89, "2026-10-26T21:00:00+09:00"),
            ],
        ),
        account(
            "cl-hello",
            "hello.sample-acct",
            "max_20x",
            1,
            vec![
                window("5h", 76, "2026-10-21T07:10:00+09:00"),
                window("7d", 92, "2026-10-27T01:00:00+09:00"),
            ],
        ),
        account(
            "cl-support",
            "support",
            "max_20x",
            0,
            vec![
                window("5h", 86, "2026-10-21T06:29:00+09:00"),
                window("7d", 86, "2026-10-27T09:00:00+09:00"),
            ],
        ),
        account(
            "cl-winapi",
            "sample-acct-c",
            "max_20x",
            0,
            vec![
                window("5h", 100, "2026-10-21T05:00:00+09:00"),
                window("7d", 0, "2026-10-24T12:00:00+09:00"),
            ],
        ),
        account(
            "cx-support",
            "support",
            "plus",
            2,
            vec![
                window("5h", 64, "2026-10-21T09:40:00+09:00"),
                window("7d", 65, "2026-10-27T15:33:00+09:00"),
            ],
        ),
        account("cx-hello", "hello.sample-acct", "plus", 0, vec![]),
        account(
            "cx-mero",
            "acct-alpha",
            "plus",
            0,
            vec![
                window("5h", 0, "2026-10-21T07:29:00+09:00"),
                window("7d", 77, "2026-10-27T16:50:00+09:00"),
            ],
        ),
        account(
            "cx-winapi",
            "sample-acct-c",
            "free",
            0,
            vec![window("30d", 100, "2026-11-12T00:00:00+09:00")],
        ),
    ];
    limited(&mut a[3], "7d", "2026-10-24T12:00:00+09:00");
    a[5]["health"] = json!({"kind":"needs_relogin"});
    limited(&mut a[6], "5h", "2026-10-21T07:29:00+09:00");
    a[7]["user_state"] = json!("paused");
    if state == 12 {
        let mut paid = a[4].clone();
        paid["credits_in_use"] = json!(true);
        paid["allow_credits"] = json!(true);
        paid["active_sessions"] = json!(1);
        paid["windows"][0]["used_pct"] = json!(100);
        let mut drain = a[5].clone();
        drain["health"] = json!({"kind":"ready"});
        drain["drain"] = json!(true);
        drain["active_sessions"] = json!(0);
        drain["bound_sessions"] = json!(1);
        drain["windows"] = json!([
            window("5h", 58, "2026-10-21T08:10:00+09:00"),
            window("7d", 80, "2026-10-27T11:00:00+09:00")
        ]);
        // Production's state precedence labels an active account "사용 중" before
        // drain. Retain a binding with active_sessions=0 to expose the drain chip.
        return vec![
            a[0].clone(),
            account("cl-new", "newteam", "max_5x", 0, vec![]),
            paid,
            drain,
        ];
    }
    if matches!(state, 8 | 11) {
        // Real account chips, arranged in collapsed production groups. The approved
        // reference also contains comparison artwork/tray rows: explicitly out of scope.
        a.clear();
        let definitions = [
            ("use", "ready", "", "active"),
            ("relogin", "needs_relogin", "", "active"),
            ("link", "cooling", "link_stale", "active"),
            ("store", "cooling", "store_error", "active"),
            ("payment", "cooling", "payment", "active"),
            ("permission", "cooling", "permission", "active"),
            ("credits", "ready", "", "active"),
            ("limit", "cooling", "quota", "active"),
            ("paused", "ready", "", "paused"),
            ("auth", "cooling", "auth", "active"),
            ("drain", "ready", "", "active"),
        ];
        for (name, kind, reason, user) in definitions {
            let mut row = account(
                &format!("cx-{name}"),
                name,
                "plus",
                if name == "use" { 1 } else { 0 },
                vec![window(
                    "5h",
                    if name == "limit" { 0 } else { 64 },
                    "2026-10-21T09:40:00+09:00",
                )],
            );
            row["health"] =
                json!({"kind":kind,"reason":reason,"scope":"account","until":today("09:40")});
            row["user_state"] = json!(user);
            row["bound_sessions"] = json!(1);
            if name == "credits" {
                row["credits_in_use"] = json!(true);
                row["allow_credits"] = json!(true);
            }
            if name == "drain" {
                row["drain"] = json!(true);
            }
            if name == "limit" {
                limited(&mut row, "5h", "2026-10-21T09:40:00+09:00");
            }
            a.push(row);
        }
    }
    a
}
fn sessions() -> Vec<Session> {
    let definitions = [
        (
            "7c1e90",
            "cl-mero",
            Some("qb-panel-opus"),
            "claude_sdk",
            None,
            "06:22",
            199,
            1,
        ),
        (
            "2b8d41",
            "cl-mero",
            Some("sample-proj-a"),
            "claude_cli",
            Some("claude-opus-5-5"),
            "05:33",
            2,
            33,
        ),
        (
            "30f878",
            "cl-mero",
            Some("quota-baton"),
            "claude_cli",
            Some("claude-opus-5-5"),
            "06:24",
            0,
            17,
        ),
        (
            "a41c07",
            "cl-mero",
            Some("sample-rev"),
            "claude_cli",
            Some("claude-opus-4-8"),
            "05:39",
            0,
            18,
        ),
        (
            "e9035b",
            "cl-mero",
            Some("sample-prj-opus-5.5"),
            "claude_cli",
            Some("claude-opus-5-5"),
            "05:46",
            120,
            24,
        ),
        (
            "c04a9e",
            "cl-mero",
            Some("2026-10-07-korea-game-design"),
            "claude_vscode",
            Some("claude-opus-5-5"),
            "04:49",
            240,
            90,
        ),
        (
            "5ab3c7",
            "cl-hello",
            Some("sample-prj"),
            "claude_cli",
            Some("claude-opus-5-5"),
            "05:40",
            120,
            23,
        ),
        (
            "a3f9c2",
            "cx-support",
            Some("quota-baton-impl"),
            "codex_cli",
            Some("gpt-6-astra"),
            "05:05",
            21,
            112,
        ),
        (
            "9731fb",
            "cx-support",
            None,
            "codex_exec",
            Some("gpt-6-astra"),
            "06:21",
            420,
            2,
        ),
    ];
    definitions
        .into_iter()
        .enumerate()
        .map(|(i, (id, account, label, kind, model, start, age, n))| {
            let inflight = matches!(i, 0 | 1 | 7);
            Session {
                id: format!("{id}000000"),
                provider: if account.starts_with("cl-") {
                    "claude"
                } else {
                    "codex"
                }
                .into(),
                account: account.into(),
                client: Client {
                    kind: kind.into(),
                    ..Default::default()
                },
                label: label.map(|text| Label {
                    text: text.into(),
                    source: if account.starts_with("cl-") {
                        "claude_project"
                    } else {
                        "codex_workspace"
                    }
                    .into(),
                }),
                first_seen_at: Some(today(start)),
                last_used_at: Some(clock().utc() - Duration::seconds(age)),
                active: i != 8,
                inflight: Inflight {
                    requests: Some(u64::from(inflight)),
                    oldest_started_at: inflight.then_some(clock().utc() - Duration::seconds(age)),
                    ws_open: Some(0),
                    ws_response_in_flight: false,
                },
                model: model.map(str::to_owned),
                requests_today: Some(n),
                failures_today: Some(0),
                subagents_today: Some(if i == 1 { 1 } else { 0 }),
                subagents_known: true,
                moves_known: true,
                counters_complete: true,
                outside_git: Some(i == 8),
                pacing_wait: (i == 7).then_some(PacingWait {
                    waiting: Some(1),
                    since: Some(clock().utc() - Duration::seconds(4)),
                }),
                moves_today: if matches!(i, 3..=5) {
                    vec![Move {
                        at: Some(today("03:42")),
                        from: Some("cl-winapi".into()),
                        to: Some("cl-mero".into()),
                        reason: "forced".into(),
                        next_turn_cache: Some("miss".into()),
                    }]
                } else {
                    vec![]
                },
                ..Default::default()
            }
        })
        .collect()
}
fn activity() -> Vec<ActivityEvent> {
    let rows = [
        ("06:02", "needs_relogin", "codex", "cx-hello", "", None),
        ("05:15", "quota_reached", "codex", "cx-mero", "5h", None),
        ("04:52", "quota_reached", "codex", "cx-mero", "5h", None),
        ("04:44", "quota_reached", "codex", "cx-mero", "5h", None),
        (
            "04:44",
            "overload_detected",
            "claude",
            "cl-mero",
            "",
            Some(2),
        ),
        (
            "03:42",
            "quota_reached",
            "claude",
            "cl-winapi",
            "7d",
            Some(3),
        ),
        ("03:10", "account_changed", "codex", "cx-winapi", "", None),
        ("02:20", "rate_hold", "codex", "cx-support", "", None),
    ];
    let mut events: Vec<_> = rows
        .into_iter()
        .enumerate()
        .map(
            |(i, (time, kind, provider, account, win, moved))| ActivityEvent {
                seq: (100 - i) as u64,
                ts: Some(today(time)),
                kind: kind.into(),
                provider: provider.into(),
                account: Some(account.into()),
                account_to: (account == "cl-winapi").then_some("cl-mero".into()),
                notable: true,
                data: ActivityData {
                    window: (!win.is_empty()).then_some(win.into()),
                    moved_groups: moved,
                    reset_at: if win == "5h" {
                        Some(today("07:29"))
                    } else if win == "7d" {
                        Some(at("2026-10-24T12:00:00+09:00"))
                    } else {
                        None
                    },
                    reason: Some(
                        if kind == "account_changed" {
                            "pause"
                        } else {
                            "overload"
                        }
                        .into(),
                    ),
                    count: Some(6),
                    max_wait_ms: Some(9000),
                    active_sessions: Some(7),
                    threshold: Some(5),
                    session: (kind == "rate_hold").then_some("a3f9c2000000".into()),
                    ..Default::default()
                },
            },
        )
        .collect();
    for (seq, kind, time) in [
        (80, "quota_reset", "2026-10-20T20:33:00+09:00"),
        (79, "credits_started", "2026-10-20T18:05:00+09:00"),
    ] {
        events.push(ActivityEvent {
            seq,
            ts: Some(at(time)),
            kind: kind.into(),
            provider: "codex".into(),
            account: Some("cx-support".into()),
            notable: true,
            data: ActivityData {
                window: Some("5h".into()),
                ..Default::default()
            },
        });
    }
    events
}
pub fn snapshot(state: u8) -> DashboardSnapshot {
    let accounts = accounts(state);
    let view = v2::project(
        &json!({"version":"R10-synthetic","accounts":accounts,
        "pools":{"claude":{"eligible":3},"codex":{"eligible":1}}})
        .to_string(),
        clock().utc().into(),
    )
    .unwrap();
    let mut s = DashboardSnapshot::new(view);
    s.revision = 10;
    s.sessions.now = Some(clock().utc());
    s.sessions.active_window_secs = Some(300);
    s.sessions.sessions = sessions();
    s.activity = activity();
    if state == 6 {
        // Preserve a known older session so production can show "last session".
        s.sessions.sessions.retain(|s| s.id == SELECTED);
        let old = &mut s.sessions.sessions[0];
        old.active = false;
        old.last_used_at = Some(clock().utc() - Duration::minutes(72));
    }
    if state == 12 {
        s.sessions
            .sessions
            .retain(|s| s.account == "cl-mero" || s.id == "a3f9c2000000");
        let mut blog = s.sessions.sessions.last().unwrap().clone();
        blog.id = "61b2dd000000".into();
        blog.account = "cx-hello".into();
        blog.client.kind = "codex_vscode".into();
        blog.label = Some(Label {
            text: "blog".into(),
            source: "codex_workspace".into(),
        });
        blog.inflight.requests = Some(0);
        blog.last_used_at = Some(clock().utc() - Duration::minutes(3));
        s.sessions.sessions.push(blog);
    }
    if matches!(state, 8 | 11) {
        s.sessions.sessions.clear();
        s.activity.clear();
    }
    for a in &s.accounts.raw.accounts {
        s.usage.accounts.push(AccountHistory {
            id: a.id.clone(),
            windows: a
                .windows
                .iter()
                .map(|w| HistoryWindow {
                    name: w.name.clone(),
                    points: vec![
                        (clock().day_start + Duration::minutes(10), Some(10.)),
                        (clock().day_start + Duration::hours(2), None),
                        (clock().day_start + Duration::hours(3), Some(80.)),
                        (clock().day_start + Duration::hours(4), Some(0.)),
                        (clock().utc(), w.used_pct),
                    ],
                    resets: vec![clock().day_start + Duration::hours(4)],
                    truncated: false,
                })
                .collect(),
            credits_spans: if a.id == "cx-support" {
                vec![(
                    clock().day_start + Duration::hours(3),
                    Some(clock().day_start + Duration::hours(4)),
                )]
            } else {
                vec![]
            },
            truncated: false,
        });
    }
    if state == 14 {
        s.connection = Connection {
            core_ok: false,
            stream_ok: false,
            retry_seconds: 5,
        };
    }
    s
}
pub fn detail(session: Session) -> SessionDetail {
    SessionDetail {
        session,
        original_short_id: Some("30f878".into()),
        recent_requests: vec![RequestSummary {
            ts: Some(clock().utc() - Duration::seconds(10)),
            outcome: "ok".into(),
            status: Some(200),
            ttfb_ms: Some(410),
            dur_ms: Some(2400),
            ..Default::default()
        }],
        timeline: vec![Placement {
            at: Some(today("06:24")),
            kind: "new".into(),
            account: "cl-mero".into(),
            ..Default::default()
        }],
        recent_requests_known: true,
        timeline_known: true,
    }
}
