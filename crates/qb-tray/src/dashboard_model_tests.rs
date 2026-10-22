use super::*;
use crate::dashboard_fixtures as fixtures;
use fixtures::*;

#[test]
fn r10_retained_data_timestamp_does_not_follow_later_offline_or_polling_clocks() {
    let mut data = snapshot();
    data.data_at = None; // Exercise the explicitly unknown publication time first.
    let mut state = DashboardState {
        range: Range::All,
        ..Default::default()
    };
    let initial = clock();
    assert_eq!(project(&data, &mut state, &initial).data_at, None);
    data.data_at = Some(initial.utc());
    let retained_sessions = data.sessions.sessions.len();
    for core_ok in [false, true] {
        data.connection.core_ok = core_ok;
        data.connection.stream_ok = false;
        for seconds in [30, 90] {
            let mut later = initial.clone();
            later.now += chrono::Duration::seconds(seconds);
            let view = project(&data, &mut state, &later);
            assert_eq!(view.clock.utc(), later.utc());
            assert_eq!(view.data_at, Some(initial.utc()));
            assert_ne!(view.data_at, Some(view.clock.utc()));
            assert_eq!(view.session_count, retained_sessions);
            assert_eq!(view.connection.core_ok, core_ok);
            assert!(!view.connection.stream_ok);
        }
    }
}

#[test]
fn r10_flat_account_status_uses_the_shared_chip_but_omits_serving_chip() {
    let data = snapshot();
    let session = &data.sessions.sessions[0];
    for (health, expected) in [
        (serde_json::json!({"kind":"ready"}), None),
        (
            serde_json::json!({"kind":"needs_relogin"}),
            Some(v2::Tone::Bad),
        ),
        (
            serde_json::json!({"kind":"cooling","reason":"quota","scope":"account","until":"2026-10-19T09:20:00Z"}),
            Some(v2::Tone::Limit),
        ),
    ] {
        let accounts = v2::project(
            &serde_json::json!({"accounts":[{"id":session.account,"provider":"claude",
                "email":"synthetic@example.invalid","user_state":"active","source":"oauth",
                "active_sessions":1,"health":health}]})
            .to_string(),
            clock().utc().into(),
        )
        .unwrap();
        let row = session_row(session, &accounts, &clock(), None);
        assert_eq!(row.account_provider, "Claude");
        assert_eq!(row.account_chip.as_ref().map(|c| c.tone), expected);
    }
}

#[test]
fn r7_unknown_inflight_never_projects_idle_or_exact_zero() {
    let mut data = snapshot();
    let mut session:Session=serde_json::from_value(serde_json::json!({"id":"abcdef123456","provider":"codex","account":"cx-a","active":false,"inflight":{"requests":0,"ws_open":0},"inflight_known":false})).unwrap();
    let c = clock();
    let row = session_row(&session, &data.accounts, &c, None);
    assert!(!row.inflight_known);
    assert!(row.dot.starts_with('—') && row.dot.contains("확인 전"));
    assert!(!row.dot.contains("쉬는 중"));
    assert!(row.activity.contains("확인 전"));
    let detail = detail_view(
        &SessionDetail {
            session: session.clone(),
            ..Default::default()
        },
        &data.accounts,
        &c,
        false,
    );
    assert!(
        detail
            .facts
            .iter()
            .any(|(key, value)| key == "연결" && value.starts_with('—'))
    );
    data.sessions.sessions = vec![session.clone()];
    let mut state = DashboardState {
        range: Range::All,
        ..Default::default()
    };
    let view = project(&data, &mut state, &c);
    assert_eq!(view.inflight_count, 0);
    assert!(!view.inflight_count_known);
    assert_eq!(view.inflight_text(), "—");
    let scene = crate::geometry::Scene::build(
        &view,
        &state,
        [1024., 640.],
        &crate::geometry::Metrics::default(),
    );
    assert!(
        scene
            .accessibility
            .iter()
            .any(|node| node.key == "header:counts" && node.name.contains("요청 중 —개"))
    );
    session.inflight.requests = Some(1);
    session.inflight_known = true;
    data.sessions.sessions.push(session);
    assert_eq!(project(&data, &mut state, &c).inflight_text(), "≥1");
    let legacy: Session =
        serde_json::from_value(serde_json::json!({"id":"abcdef123456","inflight":{"requests":0}}))
            .unwrap();
    assert!(legacy.inflight_known);
    assert!(Session::default().inflight_known);
    data.sessions.sessions = vec![legacy];
    assert_eq!(project(&data, &mut state, &c).inflight_text(), "0");
}
#[test]
fn r7_provider_selection_clears_event_account_filter_and_restores_other_groups() {
    let data = snapshot();
    let c = clock();
    let mut state = DashboardState::default();
    state.activate(&crate::geometry::Target::AccountLink("cl-a".into()));
    let filtered = project(&data, &mut state, &c);
    assert!(filtered.groups.iter().all(|g| g.id == "cl-a"));
    state.activate(&crate::geometry::Target::Provider(ProviderFilter::Codex));
    assert_eq!(state.account_filter, None);
    let codex = project(&data, &mut state, &c);
    assert!(!codex.flat.is_empty());
    assert!(codex.groups.iter().all(|g| g.provider == "codex"));
    state.activate(&crate::geometry::Target::Provider(ProviderFilter::All));
    let all = project(&data, &mut state, &c);
    assert!(all.groups.iter().any(|g| g.id == "cl-a"));
    assert!(all.groups.iter().any(|g| g.provider == "codex"));
}
#[test]
fn r7_core_completeness_false_zero_is_unknown_true_zero_is_known() {
    let data = snapshot();
    let c = clock();
    let mut s:Session=serde_json::from_value(serde_json::json!({"id":"abcdef123456","provider":"codex","account":"cx-a","requests_today":0,"failures_today":0,"subagents_today":0,"rate_wait_today":{"count":0,"total_ms":0},"counters_complete":false,"subagents_known":false,"moves_known":false})).unwrap();
    assert_eq!(session_row(&s, &data.accounts, &c, None).requests, "—");
    assert_eq!(
        session_row(&s, &data.accounts, &c, None).request_count,
        None
    );
    let detail = SessionDetail {
        session: s.clone(),
        ..Default::default()
    };
    let v = detail_view(&detail, &data.accounts, &c, false);
    assert!(!v.facts.iter().any(|(key, _)| key == "오늘 요청"));
    assert!(!v.facts.iter().any(|(key, _)| key == "하위 에이전트"));
    assert!(v.facts.iter().any(|(key, _)| key == "계정 이력 범위"));
    assert!(v.facts.iter().any(|(key, _)| key == "최근 요청 범위"));
    assert!(
        session_row(&s, &data.accounts, &c, None)
            .memo
            .contains("이력 일부")
    );
    s.requests_today = Some(7);
    assert_eq!(session_row(&s, &data.accounts, &c, None).requests, "—");
    s.requests_today = Some(0);
    s.counters_complete = true;
    s.subagents_known = true;
    assert_eq!(session_row(&s, &data.accounts, &c, None).requests, "0");
    let d = SessionDetail {
        session: s,
        recent_requests_known: true,
        timeline_known: true,
        ..Default::default()
    };
    let v = detail_view(&d, &data.accounts, &c, false);
    assert!(
        !v.facts
            .iter()
            .any(|(key, _)| key == "최근 요청 범위" || key == "계정 이력 범위")
    );
    assert!(
        v.facts
            .iter()
            .any(|(key, value)| key == "오늘 요청" && value == "0건 · 실패 0건")
    );
    assert!(
        v.facts
            .iter()
            .any(|(key, value)| key == "하위 에이전트" && value == "없음")
    );
}

#[test]
fn r7_core_candidate_wire_null_clients_prefix_counters_activity_and_history() {
    let list:SessionsResponse=serde_json::from_value(serde_json::json!({"now":"2026-10-19T06:42:08Z","active_window_secs":300,"sessions":[{"id":"abcdef123456","provider":"codex","client":{"kind":"unknown","product":null,"version":null},"original_short_id":"NOT-LIST","counters_complete":false,"requests_today":2,"truncated":true}],"truncated":true})).unwrap();
    assert_eq!(list.sessions[0].client.product, None);
    assert!(!format!("{:?}", list.sessions).contains("NOT-LIST"));
    let detail:SessionDetail=serde_json::from_value(serde_json::json!({"id":"abcdef123456","original_short_id":"12345678","recent_requests":[{"outcome":"relayed_rate429","status":429}]})).unwrap();
    assert_eq!(detail.original_short_id.as_deref(), Some("12345678"));
    let activity:ActivityResponse=serde_json::from_value(serde_json::json!({"events":[{"seq":1,"kind":"token_refresh","data":{"active":7,"recovered":true,"credits_used":3.9,"label":"FORBIDDEN-WIRE-LABEL","labels":["FORBIDDEN-WIRE-LABEL"],"outcome":"ok"}}],"next_seq":1,"truncated":false})).unwrap();
    assert_eq!(activity.events[0].data.active_sessions, Some(7));
    assert!(activity.events[0].data.after_failure);
    assert!(!format!("{activity:?}").contains("FORBIDDEN"));
    let history:UsageHistory=serde_json::from_value(serde_json::json!({"accounts":[{"id":"cx-a","windows":[{"name":"43200m","points":[["2026-10-19T06:00:00Z",null]],"resets":[],"truncated":true}],"credits_spans":[["2026-10-19T05:00:00Z",null]],"truncated":true}],"truncated":true})).unwrap();
    assert!(history.truncated);
    assert_eq!(history.accounts[0].windows[0].points[0].1, None);
}

#[test]
fn r7_defaults_counts_grouping_tray_order_and_unknowns() {
    let data = snapshot();
    let mut state = DashboardState::default();
    let view = project(&data, &mut state, &clock());
    assert_eq!(view.session_count, 12);
    assert_eq!(view.active_count, 10);
    assert_eq!(view.inflight_count, 4);
    assert_eq!(view.provider_counts, [12, 6, 6]);
    assert!(
        view.groups
            .iter()
            .position(|g| g.provider == "claude")
            .unwrap()
            < view
                .groups
                .iter()
                .position(|g| g.provider == "codex")
                .unwrap()
    );
    assert!(!view.groups.iter().any(|g| g.id == "cx-disabled"));
    assert!(
        view.groups
            .iter()
            .find(|g| g.id == "cl-login")
            .unwrap()
            .relogin
    );
    assert!(view.flat.iter().all(|r| r.model == "—"));
    assert!(view.flat[0].activity.contains("0:42"));
}
#[test]
fn r7_range_boundaries_provider_search_and_group_empty_reason() {
    let mut data = snapshot();
    let now = clock();
    data.sessions.sessions[0].inflight = Default::default();
    data.sessions.sessions[0].last_used_at = Some(now.utc() - chrono::Duration::hours(1));
    data.sessions.sessions[1].inflight = Default::default();
    data.sessions.sessions[1].last_used_at =
        Some(now.utc() - chrono::Duration::hours(1) - chrono::Duration::seconds(1));
    let mut state = DashboardState::default();
    let v = project(&data, &mut state, &now);
    assert!(v.flat.iter().any(|r| r.id == "000000000000"));
    assert!(!v.flat.iter().any(|r| r.id == "000000000001"));
    state.provider = ProviderFilter::Codex;
    state.search = "PROJECT-8".into();
    let v = project(&data, &mut state, &now);
    assert_eq!(v.flat.len(), 1);
    assert_eq!(v.flat[0].id, "000000000008");
    state.search.clear();
    state.provider = ProviderFilter::All;
    state.range = Range::Active;
    assert_eq!(project(&data, &mut state, &now).session_count, 10);
    state.range = Range::All;
    assert_eq!(project(&data, &mut state, &now).session_count, 12);
    let g = project(&data, &mut state, &now)
        .groups
        .into_iter()
        .find(|g| g.id == "cl-login")
        .unwrap();
    assert!(g.no_sessions.contains("새 세션을 배정하지 않습니다"));
}
#[test]
fn r7_all_flat_columns_sort_and_frozen_rows_update_without_jump() {
    let mut data = snapshot();
    let mut state = DashboardState {
        grouped: false,
        ..Default::default()
    };
    for key in [
        SortKey::Status,
        SortKey::Session,
        SortKey::Account,
        SortKey::Model,
        SortKey::Start,
        SortKey::LastActivity,
        SortKey::Requests,
        SortKey::Memo,
    ] {
        state.sort = key;
        state.descending = false;
        let v = project(&data, &mut state, &clock());
        for pair in v.flat.windows(2) {
            assert!(!compare_rows(&pair[0], &pair[1], key, false).is_gt());
        }
        state.descending = true;
        let v = project(&data, &mut state, &clock());
        for pair in v.flat.windows(2) {
            assert!(!compare_rows(&pair[0], &pair[1], key, true).is_gt());
        }
    }
    state.sort = SortKey::Requests;
    state.descending = true;
    let before = project(&data, &mut state, &clock())
        .flat
        .iter()
        .map(|r| r.id.clone())
        .collect::<Vec<_>>();
    state.pointer(ScrollRegion::Table, true, 100);
    data.sessions.sessions[0].requests_today = Some(9000);
    let after = project(&data, &mut state, &clock());
    assert_eq!(
        after.flat.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        before
    );
    assert!(
        after
            .flat
            .iter()
            .find(|r| r.id == "000000000000")
            .unwrap()
            .requests
            .contains("9,000")
    );
    state.release_idle(5100);
    assert_eq!(
        project(&data, &mut state, &clock()).flat[0].id,
        "000000000000"
    );
}
#[test]
fn r7_activity_same_seq_updates_first_placement_hidden_and_filter_counts() {
    let mut data = snapshot();
    let mut state = DashboardState {
        event_filter: EventFilter::All,
        ..Default::default()
    };
    let old = data
        .activity
        .iter()
        .find(|e| e.kind == "rate_hold")
        .unwrap()
        .clone();
    let mut update = old.clone();
    update.data.count = Some(8);
    update.data.relayed = Some(2);
    upsert_activity(&mut data.activity, update);
    assert_eq!(data.activity.iter().filter(|e| e.seq == old.seq).count(), 1);
    let view = project(&data, &mut state, &clock());
    let rate = view.events.iter().find(|e| e.seq == old.seq).unwrap();
    assert!(rate.body.contains("8건"));
    assert!(rate.body.contains("2건"));
    assert_eq!(rate.tag, "속도 조절");
    assert_eq!(rate.tone, v2::Tone::Off);
    assert!(
        view.event_counts
            .iter()
            .find(|(f, _)| *f == EventFilter::Tasks)
            .unwrap()
            .1
            >= 5
    );
    let mut first = data
        .activity
        .iter()
        .find(|e| e.kind == "move")
        .unwrap()
        .clone();
    first.seq = 100;
    first.data.reason = Some("new".into());
    upsert_activity(&mut data.activity, first);
    assert!(
        !project(&data, &mut state, &clock())
            .events
            .iter()
            .any(|e| e.seq == 100)
    );
    state.event_filter = EventFilter::Credits;
    assert!(
        project(&data, &mut state, &clock())
            .events
            .iter()
            .all(|e| e.category == EventFilter::Credits)
    );
}
#[test]
fn r7_all_event_wording_variants_keep_semantic_tags_and_no_raw_errors() {
    let data = snapshot();
    let c = clock();
    for e in &data.activity {
        let row = event_row(e, &data.accounts, &c).unwrap();
        assert!(!row.title.is_empty());
        assert!(!row.body.is_empty());
    }
    let base = data.activity.iter().find(|e| e.kind == "move").unwrap();
    for reason in ["overload", "forced", "cold_rebalance", "reset_hold_timeout"] {
        let mut e = base.clone();
        e.data.reason = Some(reason.into());
        let row = event_row(&e, &data.accounts, &c).unwrap();
        assert_eq!(row.tag, "계정 전환");
        assert_eq!(row.tone, v2::Tone::Use);
    }
    let mut quota = data.activity[0].clone();
    quota.data.no_eligible = true;
    quota.data.actionable = false;
    assert_eq!(event_row(&quota, &data.accounts, &c).unwrap().tag, "정보");
    quota.data.actionable = true;
    assert_eq!(event_row(&quota, &data.accounts, &c).unwrap().tag, "할 일");
    for action in [
        "pause",
        "resume",
        "drain",
        "allow_credits",
        "enable",
        "disable",
    ] {
        let mut e = base.clone();
        e.kind = "account_changed".into();
        e.data.action = Some(action.into());
        assert_eq!(event_row(&e, &data.accounts, &c).unwrap().tag, "설정");
    }
}
#[test]
fn r7_exact_decimal_credit_delta_floor_omits_unknown_and_never_negative() {
    let start = serde_json::from_str::<serde_json::Number>("10000000000000.23").unwrap();
    let stop = serde_json::from_str::<serde_json::Number>("9999999999999.24").unwrap();
    assert_eq!(decimal_spent(&start, &stop), Some("0".into()));
    assert_eq!(decimal_spent(&stop, &start), Some("0".into()));
    assert_eq!(
        decimal_spent(
            &serde_json::from_str("10.99").unwrap(),
            &serde_json::from_str("8.01").unwrap()
        ),
        Some("2".into())
    );
}
#[test]
fn r7_detail_raw_eight_only_request_outcomes_and_removed_binding() {
    let data = snapshot();
    let mut state = DashboardState::default();
    let session = data.sessions.sessions[0].clone();
    let ticket = state.select(Some(session.id.clone())).unwrap();
    let mut detail = SessionDetail {
        session,
        original_short_id: Some("raw-eight-NEVER-REST".into()),
        ..Default::default()
    };
    for outcome in [
        "ok",
        "rate_hold_ok",
        "waiting",
        "inflight",
        "relayed_rate_429",
        "upstream_error",
        "client_drop",
    ] {
        detail.recent_requests.push(RequestSummary {
            outcome: outcome.into(),
            ts: Some(clock().utc()),
            status: Some(500),
            rate_retries: Some(2),
            ..Default::default()
        });
    }
    assert!(state.publish_detail(&ticket, detail));
    let view = project(&data, &mut state, &clock());
    assert_eq!(
        view.detail.as_ref().unwrap().raw_prefix.as_deref(),
        Some("raw-eigh")
    );
    let rows = format!("{:?}", view.flat);
    assert!(!rows.contains("raw-eigh"));
    state.detail_removed(&ticket.id);
    assert!(project(&data, &mut state, &clock()).detail.unwrap().expired);
    assert!(
        view.detail
            .unwrap()
            .requests
            .iter()
            .any(|r| r.contains("속도 제한 오류를 그대로 전달"))
    );
}
#[test]
fn r7_labels_use_last_segment_and_unknown_fields_are_not_kept() {
    let session:Session=serde_json::from_value(serde_json::json!({"id":"abcdef123456","provider":"codex","label":{"text":"C:/private/work/folder\u{202e}","source":"launcher"},"tokens":"NEVER","raw_session_id":"FULL-NEVER"})).unwrap();
    assert_eq!(label(&session), "folder");
    assert!(!format!("{session:?}").contains("NEVER"));
    assert_eq!(hash("raw-session-id"), None);
}
#[test]
fn r7_usage_is_remaining_observed_only_with_reset_and_unknown_gaps() {
    let mut data = snapshot();
    let c = clock();
    let history = &mut data.usage.accounts[0];
    history.windows[0]
        .points
        .push((c.day_start - chrono::Duration::minutes(1), Some(80.)));
    history.windows[0]
        .points
        .push((c.day_start + chrono::Duration::minutes(30), None));
    history.windows.push(HistoryWindow {
        name: "7d_opus".into(),
        ..Default::default()
    });
    let tiles = usage_tiles(&data, &c);
    let tile = tiles.iter().find(|t| t.id == "cx-a").unwrap();
    assert!(!tile.series.iter().any(|s| s.name.contains("opus")));
    let series = &tile.series[0];
    assert!(
        series
            .points
            .iter()
            .all(|(t, _)| *t >= usage_domain(&c).0 && *t <= c.utc())
    );
    assert!(series.points.iter().any(|(_, p)| *p == Some(90.)));
    assert_eq!(series.points[0].1, Some(20.));
    assert!(series.points.iter().any(|(_, p)| p.is_none()));
    assert_eq!(tile.credits_spans.len(), 1);
    assert!(!tiles.iter().any(|t| t.id == "cx-disabled"));
}

#[test]
fn r13_usage_midnight_keeps_previous_day_observations_but_today_filters_do_not() {
    use chrono::Duration;
    let mut c = clock();
    c.now = DateTime::parse_from_rfc3339("2026-10-20T00:20:45+09:00").unwrap();
    c.day_start = at("2026-10-20T00:00:00+09:00");
    c.day_end = c.day_start + Duration::days(1);
    let (from, to) = usage_domain(&c);
    assert_eq!(from, at("2026-10-19T05:20:00+09:00"));
    assert_eq!(to, at("2026-10-20T05:20:00+09:00"));
    let yesterday = at("2026-10-19T23:50:00+09:00");
    let gap = yesterday - Duration::minutes(10);
    let mut data = snapshot();
    data.usage.accounts[0].windows = vec![HistoryWindow {
        name: "5h".into(),
        points: vec![
            (from - Duration::seconds(1), Some(99.)),
            (from, Some(10.)),
            (gap, None),
            (yesterday, Some(30.)),
            (c.utc(), Some(40.)),
            (c.utc() + Duration::seconds(1), Some(50.)),
        ],
        resets: vec![from - Duration::seconds(1), from, yesterday, to],
        ..Default::default()
    }];
    data.usage.accounts[0].credits_spans = vec![(from - Duration::hours(1), None)];
    data.activity = [yesterday, c.day_start]
        .into_iter()
        .enumerate()
        .map(|(i, ts)| {
            let mut e = r10_quota(i as u64 + 1, 0);
            e.ts = Some(ts);
            e
        })
        .collect();
    data.sessions.sessions.truncate(2);
    for (session, ts) in data
        .sessions
        .sessions
        .iter_mut()
        .zip([yesterday, c.day_start])
    {
        session.last_used_at = Some(ts);
        session.inflight = Default::default();
    }
    let view = project(
        &data,
        &mut DashboardState {
            range: Range::Today,
            event_days: 1,
            ..Default::default()
        },
        &c,
    );
    let tile = view.usage.iter().find(|t| t.id == "cx-a").unwrap();
    assert_eq!(
        tile.series[0].points,
        vec![
            (from, Some(90.)),
            (gap, None),
            (yesterday, Some(70.)),
            (c.utc(), Some(60.))
        ]
    );
    assert_eq!(tile.series[0].resets, vec![from, yesterday]);
    assert_eq!(tile.credits_spans, vec![(from, c.utc())]);
    assert_eq!(view.flat.len(), 1);
    assert_eq!(view.flat[0].id, "000000000001");
    assert_eq!(view.events.len(), 1);
    assert_eq!(view.events[0].seq, 2);
}

#[test]
fn r13_usage_domain_is_minute_anchored_and_recovers_from_clock_reversal() {
    use chrono::Duration;
    let mut c = clock();
    let initial = usage_domain(&c);
    for ms in [0, 500, 51_999] {
        c.now = clock().now + Duration::milliseconds(ms);
        assert_eq!(usage_domain(&c), initial);
    }
    c.now = clock().now + Duration::seconds(52);
    assert_eq!(usage_domain(&c).0, initial.0 + Duration::minutes(1));
    c.now = clock().now - Duration::seconds(9);
    assert_eq!(usage_domain(&c).0, initial.0 - Duration::minutes(1));
    assert_eq!(usage_domain(&c).1 - usage_domain(&c).0, Duration::hours(24));
}

#[test]
fn r13_usage_empty_history_keeps_all_real_windows_and_resets_beyond_domain() {
    use chrono::Duration;
    let c = clock();
    let (_, to) = usage_domain(&c);
    let data = r10_accounts(serde_json::json!([
        {"id":"free","provider":"codex","user_state":"active","health":{"kind":"ready"},"windows":[
            {"name":"5h","used_pct":20,"reset_at":to},
            {"name":"7d","used_pct":30,"reset_at":to + Duration::seconds(1)},
            {"name":"43200m","used_pct":40,"reset_at":to + Duration::days(20)}
        ]}
    ]));
    let tiles = usage_tiles(&data, &c);
    let tile = &tiles[0];
    assert_eq!(tile.windows.len(), 3);
    assert_eq!(tile.series.len(), 3);
    assert!(
        tile.series
            .iter()
            .all(|s| s.points.is_empty() && s.resets.is_empty())
    );
    assert_eq!(
        tile.series.iter().map(|s| s.remaining).collect::<Vec<_>>(),
        vec![Some(80), Some(70), Some(60)]
    );
    assert_eq!(
        tile.series
            .iter()
            .flat_map(|s| s.future_resets.clone())
            .collect::<Vec<_>>(),
        vec![to, to + Duration::seconds(1), to + Duration::days(20)]
    );
    assert!(!tile.series.iter().any(|s| s.name == "0m"));
}
#[test]
fn r84_dashboard_group_header_keeps_each_usage_window_and_binding_chip() {
    let data = snapshot();
    let c = clock();
    let view = project(&data, &mut DashboardState::default(), &c);
    let group = view.groups.iter().find(|g| g.id == "cl-a").unwrap();
    assert_eq!(
        group
            .windows
            .iter()
            .map(|w| w.label.as_str())
            .collect::<Vec<_>>(),
        ["5시간", "주간"]
    );
    assert_eq!(group.windows[0].remaining, "34% 남음");
    assert!(group.windows[0].reset.contains("초기화"));
    assert_eq!(group.windows[1].remaining, "88% 남음");
    assert_eq!(group.chip, "사용 중");
    let scene = crate::geometry::Scene::build(
        &view,
        &DashboardState::default(),
        [1280., 900.],
        &crate::geometry::Metrics::default(),
    );
    for (index, window) in group.windows.iter().enumerate() {
        let node = scene
            .accessibility
            .iter()
            .find(|n| n.key == format!("usage:cl-a:{index}"))
            .unwrap();
        assert!(node.name.contains(&window.label));
        assert!(node.name.contains(&window.remaining));
        assert!(node.name.contains(&window.reset));
    }
}
#[test]
fn r84_dashboard_window_uses_authoritative_cooling_reset() {
    let c = clock();
    let accounts = v2::project(
        &serde_json::json!({"accounts":[{
            "id":"quota","provider":"claude","user_state":"active",
            "health":{"kind":"cooling","reason":"quota","scope":"account"},
            "quota_cooling":{"window":"5h","reset_at":"2026-10-20T01:00:00Z"},
            "windows":[
                {"name":"5h","used_pct":100,"reset_at":"2026-10-19T09:20:00Z"},
                {"name":"7d","used_pct":12,"reset_at":"2026-10-26T01:00:00Z"}
            ]
        }]})
        .to_string(),
        c.utc().into(),
    )
    .unwrap();
    let expected = accounts.sections[0].rows[0]
        .usage
        .as_ref()
        .unwrap()
        .binding
        .as_ref()
        .unwrap()
        .reset_text
        .clone()
        .unwrap();
    let data = DashboardSnapshot::new(accounts);
    let view = project(&data, &mut DashboardState::default(), &c);
    assert_eq!(view.groups[0].windows[0].reset, expected);
    assert_eq!(view.groups[0].windows[1].remaining, "88% 남음");
    assert_ne!(view.groups[0].windows[1].reset, expected);
}
#[test]
fn r7_empty_states_are_distinct_and_day_boundary_explicit() {
    let mut data = snapshot();
    let mut state = DashboardState {
        search: "no match".into(),
        ..Default::default()
    };
    assert!(matches!(
        project(&data, &mut state, &clock()).empty,
        EmptyState::NoMatch(_)
    ));
    state.search.clear();
    data.sessions.sessions.clear();
    assert!(matches!(
        project(&data, &mut state, &clock()).empty,
        EmptyState::NoSessions(_)
    ));
    let accounts = v2::project("{\"accounts\":[]}", clock().utc().into()).unwrap();
    let data = DashboardSnapshot::new(accounts);
    assert_eq!(
        project(&data, &mut state, &clock()).empty,
        EmptyState::NoAccounts
    );
    let mut c = clock();
    c.day_end = c.day_start + chrono::Duration::hours(23);
    assert_eq!(c.day_end.signed_duration_since(c.day_start).num_hours(), 23);
}

fn r10_quota(seq: u64, minutes_ago: i64) -> ActivityEvent {
    ActivityEvent {
        seq,
        ts: Some(clock().utc() - chrono::Duration::minutes(minutes_ago)),
        kind: "quota_reached".into(),
        provider: "claude".into(),
        account: Some("cl-a".into()),
        notable: true,
        data: ActivityData {
            window: Some("5h".into()),
            reset_at: Some(clock().utc() + chrono::Duration::hours(1)),
            moved_groups: Some(0),
            ..Default::default()
        },
        ..Default::default()
    }
}
fn r10_fold(events: &[ActivityEvent]) -> Vec<EventRow> {
    folded_events(
        &events.iter().collect::<Vec<_>>(),
        &snapshot().accounts,
        &clock(),
    )
}
fn r10_accounts(accounts: serde_json::Value) -> DashboardSnapshot {
    DashboardSnapshot::new(
        v2::project(
            &serde_json::json!({"accounts":accounts}).to_string(),
            clock().utc().into(),
        )
        .unwrap(),
    )
}
fn r10_fact<'a>(detail: &'a DetailView, key: &str) -> &'a str {
    &detail.facts.iter().find(|(k, _)| k == key).unwrap().1
}

#[test]
fn r10_limit_tag_names_known_windows_and_never_leaves_empty_title_slot() {
    let accounts = snapshot().accounts;
    for (window, label) in [("5h", "5시간"), ("7d", "주간"), ("43200m", "30일")] {
        let mut e = r10_quota(1, 1);
        e.data.window = Some(window.into());
        for moves in [0, 2] {
            e.data.moved_groups = Some(moves);
            let row = event_row(&e, &accounts, &clock()).unwrap();
            assert_eq!(row.tag, "한도 도달");
            assert_eq!(row.tone, v2::Tone::Limit);
            assert!(row.title.ends_with(&format!(" · {label} 한도 도달")));
        }
    }
    for window in [None, Some("".into()), Some("  ".into())] {
        let mut e = r10_quota(1, 1);
        e.data.window = window;
        let row = event_row(&e, &accounts, &clock()).unwrap();
        assert!(row.title.ends_with(" · 한도 도달"));
        assert!(!row.title.contains("·  "));
        assert_eq!(row.title.matches('·').count(), 1);
    }
    let mut e = r10_quota(1, 1);
    e.data.no_eligible = true;
    assert_eq!(event_row(&e, &accounts, &clock()).unwrap().tag, "정보");
    e.data.actionable = true;
    assert_eq!(
        event_row(&e, &accounts, &clock()).unwrap().tone,
        v2::Tone::Bad
    );
}

#[test]
fn r10_repeats_keep_newest_and_show_three_local_times_then_overflow() {
    let events: Vec<_> = (0..6).map(|i| r10_quota(i + 1, 6 - i as i64)).collect();
    let rows = r10_fold(&events);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].seq, 6);
    let times: Vec<_> = [2, 3, 4]
        .iter()
        .map(|m| clock().time(Some(clock().utc() - chrono::Duration::minutes(*m))))
        .collect();
    assert!(
        rows[0]
            .body
            .ends_with(&format!("같은 알림 5번 더 · {} 외 2번", times.join(", ")))
    );
    assert!(!rows[0].body.contains(&clock().time(events[0].ts)));
}

#[test]
fn r10_folding_keeps_distinct_cycles_windows_moves_and_no_eligible_alerts() {
    let old = r10_quota(1, 2);
    for case in 0..15 {
        let mut new = r10_quota(2, 1);
        match case {
            0 => new.provider = "codex".into(),
            1 => new.provider.clear(),
            2 => new.account = Some("cl-b".into()),
            3 => new.account = None,
            4 => new.account = Some("".into()),
            5 => new.data.window = Some("7d".into()),
            6 => new.data.window = None,
            7 => new.data.window = Some(" ".into()),
            8 => new.data.reset_at = new.data.reset_at.map(|t| t + chrono::Duration::seconds(1)),
            9 => new.data.reset_at = None,
            10 => new.data.moved_groups = Some(1),
            11 => new.data.moved_groups = None,
            12 => new.data.no_eligible = true,
            13 => new.kind = "move".into(),
            _ => new.data.actionable = true, // Actionability alone is not a folding veto.
        }
        let rows = r10_fold(&[old.clone(), new.clone()]);
        assert_eq!(
            rows.len(),
            if [6, 7, 11, 14].contains(&case) { 1 } else { 2 },
            "case {case}"
        );
        // Every veto applies equally to the older member of the pair.
        let mut changed_old = new;
        changed_old.seq = 1;
        changed_old.ts = old.ts;
        assert_eq!(
            r10_fold(&[changed_old, r10_quota(2, 1)]).len(),
            if [6, 7, 11, 14].contains(&case) { 1 } else { 2 },
            "older case {case}"
        );
    }
    for kind in ["move", "quota_reset", "credits_started"] {
        let mut a = old.clone();
        a.kind = kind.into();
        let mut b = a.clone();
        b.seq += 1;
        assert_eq!(r10_fold(&[a, b]).len(), 2);
    }
}

#[test]
fn r10_resets_break_all_windows_only_for_same_provider_account_including_ties() {
    for same_time in [false, true] {
        let mut old = r10_quota(1, 3);
        let mut reset = r10_quota(2, 2);
        reset.kind = "quota_reset".into();
        reset.data.window = Some("7d".into());
        let new = r10_quota(3, 1);
        if same_time {
            old.ts = new.ts;
            reset.ts = new.ts;
        }
        assert_eq!(
            r10_fold(&[new.clone(), old.clone(), reset.clone()]).len(),
            3
        );
        reset.account = Some("cl-b".into());
        assert_eq!(
            r10_fold(&[old.clone(), reset.clone(), new.clone()]).len(),
            2
        );
        reset.account = old.account.clone();
        reset.provider = "codex".into();
        assert_eq!(
            r10_fold(&[old.clone(), reset.clone(), new.clone()]).len(),
            2
        );
        reset.provider = old.provider.clone();
        reset.ts = Some(clock().utc());
        reset.seq = 4;
        if !same_time {
            assert_eq!(
                r10_fold(&[old.clone(), reset.clone(), new.clone()]).len(),
                2
            );
        }
        reset.ts = None;
        assert_eq!(r10_fold(&[old, reset, new]).len(), 2); // Unknown reset is a barrier, not a dated row.
    }
}

#[test]
fn r10_feed_and_tab_fold_before_limits_and_keep_nonnotable_reset_barriers() {
    let mut data = snapshot();
    data.activity = (0..6).map(|i| r10_quota(i + 1, 6 - i as i64)).collect();
    let view = project(&data, &mut DashboardState::default(), &clock());
    assert_eq!(view.events.len(), 1);
    assert_eq!(view.recent.len(), 1);
    assert_eq!(view.events[0].seq, view.recent[0].seq);
    assert_eq!(view.events[0].body, view.recent[0].body);
    let mut reset = r10_quota(20, 3);
    reset.kind = "quota_reset".into();
    reset.notable = false;
    data.activity.push(reset);
    let view = project(&data, &mut DashboardState::default(), &clock());
    assert_eq!(view.recent.len(), 2);
    assert_eq!(
        view.events
            .iter()
            .filter(|e| e.kind == "quota_reached")
            .count(),
        2
    );
    let mut future = r10_quota(21, -1);
    future.data.moved_groups = Some(2);
    let mut unknown = r10_quota(22, 1);
    unknown.ts = None;
    data.activity.extend([future, unknown]);
    let view = project(&data, &mut DashboardState::default(), &clock());
    assert!(!view.events.iter().chain(&view.recent).any(|e| e.seq >= 21));
}

#[test]
fn r10_headers_detail_and_tiles_share_all_windows_including_exhausted_and_unknown() {
    let data = r10_accounts(serde_json::json!([
        {"id":"free","provider":"codex","user_state":"active","health":{"kind":"ready"},"windows":[{"name":"43200m","used_pct":100},{"name":"7d_oi","used_pct":50}]},
        {"id":"unknown","provider":"claude","user_state":"active","health":{"kind":"ready"},"windows":[{"name":"5h"}]}
    ]));
    let view = project(&data, &mut DashboardState::default(), &clock());
    for group in &view.groups {
        let tile = view.usage.iter().find(|t| t.id == group.id).unwrap();
        assert_eq!(
            tile.windows
                .iter()
                .map(|w| (&w.label, w.remaining_pct))
                .collect::<Vec<_>>(),
            group
                .windows
                .iter()
                .map(|w| (&w.label, w.remaining_pct))
                .collect::<Vec<_>>()
        );
        let detail = detail_view(
            &SessionDetail {
                session: Session {
                    account: group.id.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
            &data.accounts,
            &clock(),
            false,
        );
        if group.id == "free" {
            assert_eq!(group.windows.len(), 1);
            assert_eq!(r10_fact(&detail, "남은 양"), "30일 0%");
        } else {
            assert!(group.windows.is_empty());
            assert_eq!(r10_fact(&detail, "남은 양"), "사용량 확인 전");
        }
    }
    let data = snapshot();
    let detail = detail_view(
        &SessionDetail {
            session: data.sessions.sessions[0].clone(),
            ..Default::default()
        },
        &data.accounts,
        &clock(),
        false,
    );
    assert_eq!(r10_fact(&detail, "남은 양"), "5시간 34% · 주간 88%");
    assert!(!r10_fact(&detail, "계정").contains('%'));
}

#[test]
fn r10_relogin_stale_windows_have_observation_time_and_paused_metadata() {
    let data = r10_accounts(serde_json::json!([
        {"id":"login","provider":"claude","user_state":"active","health":{"kind":"needs_relogin"},"observed_at":"2026-10-19T05:00:00Z","windows":[{"name":"5h","used_pct":10},{"name":"7d","used_pct":20}]},
        {"id":"paused","provider":"codex","user_state":"paused","health":{"kind":"ready"},"windows":[{"name":"5h","used_pct":100}]}
    ]));
    let view = project(&data, &mut DashboardState::default(), &clock());
    let login = view.groups.iter().find(|g| g.id == "login").unwrap();
    assert!(!login.relogin_reason.is_empty());
    assert!(!login.paused);
    let tile = view.usage.iter().find(|t| t.id == "login").unwrap();
    assert!(tile.unconfirmed);
    assert_eq!(tile.last_observed, Some(at("2026-10-19T05:00:00Z")));
    let detail = detail_view(
        &SessionDetail {
            session: Session {
                account: "login".into(),
                ..Default::default()
            },
            ..Default::default()
        },
        &data.accounts,
        &clock(),
        false,
    );
    assert_eq!(
        r10_fact(&detail, "남은 양"),
        "확인 불가 · 14:00 기준 5시간 90% · 주간 80%"
    );
    let paused = view.groups.iter().find(|g| g.id == "paused").unwrap();
    assert!(paused.paused);
    assert_eq!(paused.windows[0].remaining_pct, 0);
    assert!(view.usage.iter().find(|t| t.id == "paused").unwrap().paused);
    assert!(paused.no_sessions.ends_with("새 세션을 배정하지 않습니다"));
}

#[test]
fn r10_usage_observation_metadata_ignores_future_unknown_and_sorts_windows() {
    let mut data = snapshot();
    let history = data
        .usage
        .accounts
        .iter_mut()
        .find(|h| h.id == "cx-a")
        .unwrap();
    history.windows.reverse();
    history.windows[0]
        .points
        .push((clock().utc() + chrono::Duration::seconds(1), Some(20.)));
    history.windows[0].points.push((clock().utc(), None));
    let tiles = usage_tiles(&data, &clock());
    let tile = tiles.iter().find(|t| t.id == "cx-a").unwrap();
    assert_eq!(tile.series[0].name, "5시간");
    assert_eq!(tile.series[1].name, "주간");
    assert_eq!(tile.last_observed, Some(clock().utc()));
    assert_eq!(tile.series[1].points.last().unwrap().1, None);
    let unknown = r10_accounts(serde_json::json!([
        {"id":"bad","provider":"codex","user_state":"active","health":{"kind":"needs_relogin"},"observed_at":"not-a-time"},
        {"id":"future","provider":"codex","user_state":"active","health":{"kind":"ready"},"observed_at":"2030-01-01T00:00:00Z"}
    ]));
    assert!(
        usage_tiles(&unknown, &clock())
            .iter()
            .all(|t| t.last_observed.is_none())
    );
}

#[test]
fn r10_detail_status_chip_separates_request_activity_from_account_assignment() {
    let data = snapshot();
    let mut detail = SessionDetail {
        session: data.sessions.sessions[0].clone(),
        ..Default::default()
    };
    let view = detail_view(&detail, &data.accounts, &clock(), false);
    assert_eq!(view.status, "요청 중 0:42");
    assert_eq!(view.status_tone, v2::Tone::Use);
    assert!(view.account_status.ends_with("에서 처리 중"));
    detail.session.inflight = Inflight::default();
    for (known, active, text) in [
        (true, true, "5분 안에 활동"),
        (true, false, "쉬는 중"),
        (false, false, "요청 상태 확인 전"),
    ] {
        detail.session.inflight_known = known;
        detail.session.active = active;
        let view = detail_view(&detail, &data.accounts, &clock(), false);
        assert_eq!(view.status, text);
        assert_eq!(view.status_tone, v2::Tone::Off);
        assert!(view.account_status.ends_with("에 배정됨"));
    }
}

#[test]
fn r10_final_failures_survive_failover_and_today_counts_respect_boundaries_and_partial_data() {
    let data = snapshot();
    let mut detail = SessionDetail {
        session: data.sessions.sessions[0].clone(),
        ..Default::default()
    };
    for result in [
        "upstream_error",
        "network_error",
        "relayed_rate429",
        "relayed_rate_429",
        "client_drop",
        "ok",
        "rate_hold_ok",
        "inflight",
    ] {
        detail.recent_requests.push(RequestSummary {
            ts: Some(clock().utc()),
            outcome: result.into(),
            failover: Some(Failover {
                from: "cl-a".into(),
                to: "cl-b".into(),
            }),
            ..Default::default()
        });
    }
    let view = detail_view(&detail, &data.accounts, &clock(), false);
    assert_eq!(
        view.requests.iter().filter(|r| r.contains("실패")).count(),
        4
    );
    assert_eq!(r10_fact(&view, "오늘 요청"), "100건 · 실패 4건");
    for time in [
        None,
        Some(clock().day_start - chrono::Duration::seconds(1)),
        Some(clock().utc() + chrono::Duration::seconds(1)),
    ] {
        detail.recent_requests.push(RequestSummary {
            ts: time,
            outcome: "upstream_error".into(),
            ..Default::default()
        });
    }
    assert_eq!(
        r10_fact(
            &detail_view(&detail, &data.accounts, &clock(), false),
            "오늘 요청"
        ),
        "100건 · 실패 4건"
    );
    detail.session.failures_today = Some(9);
    assert_eq!(
        r10_fact(
            &detail_view(&detail, &data.accounts, &clock(), false),
            "오늘 요청"
        ),
        "100건 · 실패 9건"
    );
    detail.session.counters_complete = false;
    assert_eq!(
        r10_fact(
            &detail_view(&detail, &data.accounts, &clock(), false),
            "오늘 요청"
        ),
        "실패 최소 9건"
    );
    detail.session.counters_complete = true;
    detail.session.failures_today = None;
    assert_eq!(
        r10_fact(
            &detail_view(&detail, &data.accounts, &clock(), false),
            "오늘 요청"
        ),
        "100건 · 실패 최소 4건"
    );
}

#[test]
fn r10_empty_group_ready_reason_and_client_sort_are_explicit() {
    let data = r10_accounts(serde_json::json!([
        {"id":"cl-a","provider":"claude","user_state":"active","health":{"kind":"ready"}},
        {"id":"cl-login","provider":"claude","user_state":"active","health":{"kind":"needs_relogin"}},
        {"id":"cl-quota","provider":"claude","user_state":"active","health":{"kind":"cooling","reason":"quota","scope":"account"},"windows":[{"name":"5h","used_pct":100}]}
    ]));
    let view = project(&data, &mut DashboardState::default(), &clock());
    assert_eq!(view.session_count, 0);
    assert_eq!(view.active_count, 0);
    assert_eq!(view.inflight_count, 0);
    assert!(
        view.groups
            .iter()
            .find(|g| g.id == "cl-a")
            .unwrap()
            .no_sessions
            .ends_with("새 세션 배정 가능")
    );
    assert!(
        view.groups
            .iter()
            .find(|g| g.id == "cl-login")
            .unwrap()
            .no_sessions
            .ends_with("새 세션을 배정하지 않습니다")
    );
    assert!(
        view.groups
            .iter()
            .find(|g| g.id == "cl-quota")
            .unwrap()
            .no_sessions
            .ends_with("새 세션을 배정하지 않습니다")
    );
    let original = snapshot();
    let mut a = session_row(
        &original.sessions.sessions[0],
        &original.accounts,
        &clock(),
        None,
    );
    let mut b = a.clone();
    a.client = "Zulu".into();
    b.client = "alpha".into();
    assert!(compare_rows(&a, &b, SortKey::Client, false).is_gt());
    assert!(compare_rows(&a, &b, SortKey::Client, true).is_lt());
    a.client = "ALPHA".into();
    a.short_id = "000001".into();
    b.short_id = "000002".into();
    assert!(compare_rows(&a, &b, SortKey::Client, false).is_lt());
}

#[test]
fn r10_unknown_event_times_do_not_fold_and_reset_outside_pair_does_not_split() {
    let old = r10_quota(1, 3);
    let new = r10_quota(3, 1);
    for minutes in [0, 4] {
        let mut reset = r10_quota(2, minutes);
        reset.kind = "quota_reset".into();
        let rows = r10_fold(&[old.clone(), reset, new.clone()]);
        assert_eq!(rows.iter().filter(|r| r.kind == "quota_reached").count(), 1);
    }
    let mut unknown = new.clone();
    unknown.ts = None;
    let rows = r10_fold(&[old, unknown]);
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].body.contains("같은 알림"));
    let mut data = snapshot();
    data.activity = vec![r10_quota(1, 2), new];
    data.activity[1].notable = false;
    let view = project(&data, &mut DashboardState::default(), &clock());
    assert_eq!(view.recent.len(), 1);
    assert_eq!(view.recent[0].seq, 1);
}

#[test]
fn r10_relogin_unknown_observation_time_never_claims_a_known_timestamp() {
    let data = r10_accounts(serde_json::json!([
        {"id":"login","provider":"claude","user_state":"active","health":{"kind":"needs_relogin"},"windows":[{"name":"5h","used_pct":10}]}
    ]));
    let detail = detail_view(
        &SessionDetail {
            session: Session {
                account: "login".into(),
                ..Default::default()
            },
            ..Default::default()
        },
        &data.accounts,
        &clock(),
        false,
    );
    assert_eq!(
        r10_fact(&detail, "남은 양"),
        "확인 불가 · 기준 시각 확인 전 5시간 90%"
    );
    assert_eq!(usage_tiles(&data, &clock())[0].last_observed, None);
}

#[test]
fn r101_legacy_unknown_client_maps_only_known_codex_product_spellings() {
    let mut s = Session::default();
    s.client.kind = "unknown".into();
    for (product, expected) in [
        ("codex-tui", "Codex CLI"),
        ("codex_cli_rs", "Codex CLI"),
        ("codex_vscode", "Codex VS Code"),
        ("codex_exec", "Codex exec"),
        ("Codex Desktop", "Codex 앱"),
        ("codex_desktop", "Codex 앱"),
        ("my-codex-client", "my-codex-client"),
        ("codex-tui-extra", "codex-tui-extra"),
    ] {
        s.client.product = Some(product.into());
        assert_eq!(client(&s), expected);
    }
    s.client.kind = "codex_vscode".into();
    s.client.product = Some("codex-tui".into());
    assert_eq!(client(&s), "Codex VS Code", "known kind retains precedence");
}

#[test]
fn r101_legacy_duplicates_use_only_unique_supplied_window_evidence() {
    for window in [None, Some(""), Some("5h")] {
        let mut old = r10_quota(1, 47);
        old.data.window = window.map(str::to_owned);
        old.data.moved_groups = None;
        let mut new = old.clone();
        new.seq = 3;
        new.ts = Some(clock().utc() - chrono::Duration::minutes(1));
        let rows = r10_fold(&[old.clone(), new.clone()]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq, 3);
        assert!(rows[0].body.contains("같은 알림 1번 더"));
        let mut other = new.clone();
        other.data.window = Some("7d".into());
        assert_eq!(
            r10_fold(&[old.clone(), other]).len(),
            if window == Some("5h") { 2 } else { 1 }
        );
        new.data.reset_at = new.data.reset_at.map(|at| at + chrono::Duration::hours(1));
        assert_eq!(r10_fold(&[old, new]).len(), 2);
    }
}

#[test]
fn r101_mixed_legacy_new_windows_fold_only_when_unambiguous() {
    let mut legacy = r10_quota(1, 47);
    legacy.data.window = None;
    legacy.data.moved_groups = None;
    let current = r10_quota(2, 1);
    let rows = r10_fold(&[legacy.clone(), current.clone()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].seq, 2);
    assert!(rows[0].body.contains("같은 알림 1번 더"));
    // Known evidence may be older: matching must not depend on input order.
    let mut newer_legacy = legacy.clone();
    newer_legacy.seq = 3;
    newer_legacy.ts = Some(clock().utc());
    assert_eq!(r10_fold(&[current.clone(), newer_legacy.clone()]).len(), 1);
    let mut weekly = current.clone();
    weekly.seq = 4;
    weekly.data.window = Some("7d".into());
    let rows = r10_fold(&[
        legacy.clone(),
        current.clone(),
        weekly.clone(),
        newer_legacy,
    ]);
    assert_eq!(
        rows.len(),
        3,
        "unknown folds only with unknown when two known windows exist"
    );
    assert!(rows.iter().any(|r| r.seq == 2));
    assert!(rows.iter().any(|r| r.seq == 4));
    assert!(
        rows.iter()
            .find(|r| r.seq == 3)
            .unwrap()
            .title
            .ends_with(" · 한도 도달")
    );
    // Another cycle's known value cannot supply the missing window.
    weekly.data.reset_at = weekly.data.reset_at.map(|t| t + chrono::Duration::hours(1));
    assert_eq!(r10_fold(&[legacy, current, weekly]).len(), 2);
}

#[test]
fn r101_folded_legacy_alert_keeps_supplied_window_and_newest_identity_in_both_views() {
    for window in [None, Some(""), Some("   ")] {
        let mut data = snapshot();
        let mut older_legacy = r10_quota(1, 47);
        older_legacy.data.window = None;
        older_legacy.data.moved_groups = None;
        let known = r10_quota(2, 2);
        let mut newest = r10_quota(3, 1);
        newest.data.window = window.map(str::to_owned);
        newest.data.moved_groups = None;
        let expected_title = event_row(&known, &data.accounts, &clock()).unwrap().title;
        let newest_row = event_row(&newest, &data.accounts, &clock()).unwrap();
        let expected_body = format!(
            "{}\n같은 알림 2번 더 · {}, {}",
            newest_row.body,
            clock().time(known.ts),
            clock().time(older_legacy.ts)
        );
        data.activity = vec![older_legacy, known, newest.clone()];
        let view = project(&data, &mut DashboardState::default(), &clock());
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.recent.len(), 1);
        for row in view.events.iter().chain(&view.recent) {
            assert_eq!(row.title, expected_title);
            assert_eq!(row.seq, newest.seq);
            assert_eq!(row.at, newest.ts);
            assert_eq!(row.day, newest_row.day);
            assert_eq!(row.time, newest_row.time);
            assert_eq!(row.body, expected_body);
        }
        assert_eq!(data.activity[2].data.window.as_deref(), window);
    }
}

#[test]
fn r101_folded_legacy_title_does_not_borrow_a_window_across_a_barrier() {
    for kind in ["move", "quota_reset", "quota_reached"] {
        let mut data = snapshot();
        let known = r10_quota(1, 3);
        let mut barrier = r10_quota(2, 2);
        barrier.kind = kind.into();
        barrier.data.moved_groups = Some(1);
        barrier.notable = false;
        let mut legacy = r10_quota(3, 1);
        legacy.data.window = None;
        legacy.data.moved_groups = None;
        let mut newest = legacy.clone();
        newest.seq = 4;
        newest.ts = Some(clock().utc());
        let expected_title = event_row(&newest, &data.accounts, &clock()).unwrap().title;
        data.activity = vec![known, barrier, legacy, newest];
        let view = project(&data, &mut DashboardState::default(), &clock());
        for rows in [&view.events, &view.recent] {
            let retained = rows.iter().find(|row| row.seq == 4).unwrap();
            assert_eq!(retained.title, expected_title);
            assert!(retained.body.contains("같은 알림 1번 더"));
            assert!(
                rows.iter()
                    .find(|row| row.seq == 1)
                    .unwrap()
                    .title
                    .ends_with(" · 5시간 한도 도달")
            );
        }
    }
}

#[test]
fn r101_moves_and_positive_move_counts_break_legacy_folding_in_both_accounts() {
    for kind in ["move", "quota_reached", "quota_reset"] {
        for destination in [false, true] {
            for dated in [false, true] {
                let old = r10_quota(1, 3);
                let mut barrier = r10_quota(2, 2);
                barrier.kind = kind.into();
                barrier.data.moved_groups = Some(1);
                barrier.notable = false;
                if destination {
                    barrier.account_to = barrier.account.clone();
                    barrier.account = Some("cl-b".into());
                }
                if !dated {
                    barrier.ts = None;
                }
                let new = r10_quota(3, 1);
                let rows = r10_fold(&[old.clone(), barrier.clone(), new.clone()]);
                assert_eq!(rows.iter().filter(|r| r.seq == 1 || r.seq == 3).count(), 2);
                let mut data = snapshot();
                data.activity = vec![old, barrier, new];
                let view = project(&data, &mut DashboardState::default(), &clock());
                assert_eq!(
                    view.recent
                        .iter()
                        .filter(|r| r.seq == 1 || r.seq == 3)
                        .count(),
                    2
                );
            }
        }
    }
}

#[test]
fn r101_titles_and_detail_subtitle_have_no_empty_parts() {
    for kind in ["quota_reached", "quota_reset", "credits_stopped"] {
        for window in [None, Some(""), Some("   "), Some("5h")] {
            let mut e = r10_quota(1, 1);
            e.kind = kind.into();
            e.data.window = window.map(str::to_owned);
            let row = event_row(&e, &snapshot().accounts, &clock()).unwrap();
            assert!(!row.title.is_empty());
            assert!(!row.title.contains("  "));
            assert!(row.title.split('·').all(|part| !part.trim().is_empty()));
        }
    }
    let d = SessionDetail::default();
    let view = detail_view(&d, &snapshot().accounts, &clock(), false);
    assert!(!view.subtitle.contains("  "));
}

#[test]
fn r101_unknown_totals_hide_stats_but_keep_failure_lower_bound_and_active_wait() {
    let mut d = SessionDetail::default();
    d.session.counters_complete = false;
    d.session.failures_today = Some(2);
    d.session.pacing_wait = Some(
        serde_json::from_value(serde_json::json!({
            "waiting": 1, "since": clock().utc() - chrono::Duration::seconds(7)
        }))
        .unwrap(),
    );
    let v = detail_view(&d, &snapshot().accounts, &clock(), false);
    assert_eq!(r10_fact(&v, "오늘 요청"), "실패 최소 2건");
    assert_eq!(r10_fact(&v, "속도 조절 대기"), "지금 7초째");
    assert!(
        !v.facts
            .iter()
            .any(|(_, value)| value.contains("—번") || value.contains("—초"))
    );
    d.session.pacing_wait.as_mut().unwrap().since = None;
    let v = detail_view(&d, &snapshot().accounts, &clock(), false);
    assert_eq!(r10_fact(&v, "속도 조절 대기"), "지금 대기 중");
    d.session.failures_today = None;
    d.recent_requests.push(RequestSummary {
        ts: Some(clock().utc()),
        outcome: "upstream_error".into(),
        ..Default::default()
    });
    let v = detail_view(&d, &snapshot().accounts, &clock(), false);
    assert_eq!(r10_fact(&v, "오늘 요청"), "실패 최소 1건");
    d.recent_requests.clear();
    d.session.pacing_wait = None;
    let v = detail_view(&d, &snapshot().accounts, &clock(), false);
    assert!(
        !v.facts
            .iter()
            .any(|(key, _)| key == "오늘 요청" || key == "속도 조절 대기")
    );
    assert!(
        !session_row(&d.session, &snapshot().accounts, &clock(), None)
            .memo
            .contains("오늘 요청 수 확인 전")
    );
}

#[test]
fn r11_model_choice_wire_reasons_preserve_initial_timeline_and_explicit_move_wording() {
    let data = snapshot();
    let c = clock();
    let detail: SessionDetail = serde_json::from_value(serde_json::json!({
        "id":"abcdef123456", "provider":"codex", "account":"cx-b",
        "timeline":[
            {"at":c.utc()-chrono::Duration::minutes(2), "kind":"placed",
             "account":"cx-a", "from":null, "reason":"launch_model", "next_turn_cache":null},
            {"at":c.utc()-chrono::Duration::minutes(1), "kind":"move",
             "account":"cx-b", "from":"cx-a", "reason":"model_choice", "next_turn_cache":null}
        ]
    }))
    .unwrap();
    let view = detail_view(&detail, &data.accounts, &c, false);
    assert_eq!(view.timeline.len(), 2);
    assert!(view.timeline[0].contains("시작 모델에 맞춰 처음 배정"));
    assert!(!view.timeline[0].contains("캐시"));
    assert!(view.timeline[1].contains("선택한 모델에 맞춰 계정 전환"));
    assert!(view.timeline.iter().all(|line| !line.contains("사용 불가")));

    let activity: ActivityResponse = serde_json::from_value(serde_json::json!({
        "events":[{"seq":1, "ts":c.utc(), "kind":"move", "provider":"codex",
            "account":"cx-a", "account_to":"cx-b", "notable":true,
            "data":{"session":"abcdef123456", "reason":"model_choice"}}],
        "next_seq":1, "truncated":false
    }))
    .unwrap();
    let event = &activity.events[0];
    let row = event_row(event, &data.accounts, &c).unwrap();
    assert_eq!(row.tag, "계정 전환");
    assert_eq!(row.category, EventFilter::Moves);
    assert!(row.body.contains("선택한 모델"));
    assert!(!row.body.contains("쓸 수 없"));
    assert!(!row.body.contains("사용 불가"));
    assert!(row.notable);
    for initial in ["new", "launch_model"] {
        let mut first = event.clone();
        first.data.reason = Some(initial.into());
        assert!(event_row(&first, &data.accounts, &c).is_none());
    }
}
