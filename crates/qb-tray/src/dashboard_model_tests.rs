use super::*;
use crate::dashboard_fixtures as fixtures;
use fixtures::*;

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
    assert!(
        v.facts
            .iter()
            .any(|(key, value)| key == "오늘 요청" && value.starts_with('—'))
    );
    assert!(
        v.facts
            .iter()
            .any(|(key, value)| key == "하위 에이전트" && value == "—")
    );
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
            .all(|(t, _)| *t >= c.day_start && *t <= c.utc())
    );
    assert!(series.points.iter().any(|(_, p)| *p == Some(90.)));
    assert_eq!(series.points[0].1, None);
    assert_eq!(tile.credits_spans.len(), 1);
    assert!(!tiles.iter().any(|t| t.id == "cx-disabled"));
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
