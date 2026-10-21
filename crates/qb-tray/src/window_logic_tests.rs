use super::*;

#[test]
fn wheel_router_blocks_both_modal_signals_and_resumes_after_dismissal() {
    use qb_tray::{
        geometry::{Region, Scene, Target, rect},
        state::{DashboardState, ScrollRegion},
    };
    for (opener, region) in [
        (Target::RangeMenu, ScrollRegion::Table),
        (Target::EventDaysMenu, ScrollRegion::Events),
    ] {
        let mut state = DashboardState::default();
        state.table_offset = 30.;
        state.events_offset = 40.;
        state.usage_offset = 50.;
        state.side_offset = 60.;
        let offset = if region == ScrollRegion::Table {
            30.
        } else {
            40.
        };
        let mut scene = Scene {
            modal: true,
            rect: rect(0., 0., 300., 200.),
            content: vec![Region {
                region,
                viewport: rect(0., 0., 300., 200.),
                content_height: 600.,
                offset,
                max_offset: 400.,
            }],
            targets: vec![],
            accessibility: vec![],
            draw: vec![],
            measurements: vec![],
            charts: vec![],
            tooltips: vec![],
            session_rows: vec![],
        };
        state.activate(&opener);
        let popup = state.popup;
        assert!(!wheel_scroll(&mut state, &scene, [10., 10.], 96.));
        assert_eq!(state.popup, popup);
        // Each independent modal signal must suffice, including transition frames.
        scene.modal = false;
        assert!(!wheel_scroll(&mut state, &scene, [10., 10.], 96.));
        assert_eq!(state.popup, popup);
        dismiss_popup(&mut state);
        scene.modal = true;
        assert!(!wheel_scroll(&mut state, &scene, [10., 10.], 96.));
        assert_eq!(
            (
                state.table_offset,
                state.events_offset,
                state.usage_offset,
                state.side_offset
            ),
            (30., 40., 50., 60.)
        );
        scene.modal = false;
        assert!(wheel_scroll(&mut state, &scene, [10., 10.], 96.));
        let expected = if region == ScrollRegion::Table {
            (126., 40., 50., 60.)
        } else {
            (30., 136., 50., 60.)
        };
        assert_eq!(
            (
                state.table_offset,
                state.events_offset,
                state.usage_offset,
                state.side_offset
            ),
            expected
        );
        assert!(state.popup.is_none());
        assert!(!wheel_scroll(&mut state, &scene, [400., 400.], 96.));
    }
}

#[test]
fn modal_keys_fence_underlay_and_navigate_only_popup_choices() {
    use qb_tray::{
        geometry::{HitTarget, Scene, Target, rect},
        state::{Command, DashboardState, Range},
    };
    for (opener, choices, expected) in [
        (
            Target::RangeMenu,
            vec![Target::Range(Range::Today), Target::Range(Range::All)],
            Command::Sessions(Range::All),
        ),
        (
            Target::EventDaysMenu,
            vec![Target::EventDays(1), Target::EventDays(7)],
            Command::Activity(7),
        ),
    ] {
        let mut state = DashboardState::default();
        state.search = "unchanged".into();
        state.table_offset = 31.;
        state.events_offset = 47.;
        state.usage_offset = 59.;
        state.activate(&opener);
        let mut scene = Scene {
            modal: true,
            rect: rect(0., 0., 300., 200.),
            content: vec![],
            targets: vec![],
            accessibility: vec![],
            draw: vec![],
            measurements: vec![],
            charts: vec![],
            tooltips: vec![],
            session_rows: vec![],
        };
        for target in [
            Target::Search,
            Target::Session("underlay".into()),
            Target::DismissPopup,
        ]
        .into_iter()
        .chain(choices.clone())
        {
            scene.targets.push(HitTarget {
                rect: scene.rect,
                target,
                enabled: true,
                tooltip: None,
            });
        }
        let mut focus = Some(Target::Search);
        // Ctrl+F's key, paging and ordinary typing never become popup actions.
        for key in [0x46, 0x21, 0x22, 0x41] {
            assert!(popup_key(&scene, &mut focus, key, false).is_none());
        }
        assert!(!text_input_allowed(&state, true, true));
        assert_eq!(state.search, "unchanged");
        assert_eq!(
            (state.table_offset, state.events_offset, state.usage_offset),
            (31., 47., 59.)
        );
        assert!(popup_key(&scene, &mut focus, 0x0d, false).is_none());
        popup_key(&scene, &mut focus, 0x28, false);
        assert_eq!(focus, Some(choices[0].clone()));
        popup_key(&scene, &mut focus, 0x26, false);
        assert_eq!(focus, Some(choices[1].clone()));
        popup_key(&scene, &mut focus, 0x09, false);
        assert_eq!(focus, Some(choices[0].clone()));
        popup_key(&scene, &mut focus, 0x09, true);
        assert_eq!(focus, Some(choices[1].clone()));
        let action = popup_key(&scene, &mut focus, 0x0d, false).unwrap();
        assert_eq!(state.activate(&action), vec![expected]);
        assert!(state.popup.is_none());
        state.activate(&opener);
        let action = popup_key(&scene, &mut focus, 0x1b, false).unwrap();
        assert_eq!(action, Target::DismissPopup);
        assert!(state.activate(&action).is_empty());
        assert!(text_input_allowed(&state, true, true));
    }
}

#[test]
fn enter_retains_detail_ticket_but_pointer_toggle_and_escape_still_close() {
    use qb_tray::{
        dashboard_model::SessionDetail,
        geometry::Target,
        state::{Command, DashboardState},
    };
    let mut state = DashboardState::default();
    let commands = open_session(&mut state, "a");
    let [Command::Detail(ticket)] = commands.as_slice() else {
        panic!("missing detail")
    };
    assert!(open_session(&mut state, "a").is_empty());
    assert!(open_session(&mut state, "a").is_empty());
    let mut detail = SessionDetail::default();
    detail.session.id = ticket.id.clone();
    assert!(state.publish_detail(ticket, detail));
    assert_eq!(state.selected.as_deref(), Some("a"));
    assert!(state.detail.is_some());
    state.activate(&Target::Session("a".into()));
    assert!(state.selected.is_none());
    open_session(&mut state, "a");
    state.activate(&Target::CloseDetail);
    assert!(state.selected.is_none());
}

#[test]
fn arrows_reveal_offscreen_rows_refresh_detail_and_invalidate_old_frame() {
    use qb_tray::{
        geometry::{Region, Scene, SessionGeometry, Target, rect},
        state::{Command, DashboardState, ScrollRegion},
    };
    let mut scene = Scene {
        modal: false,
        rect: rect(0., 0., 300., 100.),
        content: vec![Region {
            region: ScrollRegion::Table,
            viewport: rect(0., 0., 300., 100.),
            content_height: 200.,
            offset: 0.,
            max_offset: 100.,
        }],
        // No visible targets: keyboard traversal must use full row geometry.
        targets: vec![],
        accessibility: vec![],
        draw: vec![],
        measurements: vec![],
        charts: vec![],
        tooltips: vec![],
        session_rows: vec![
            SessionGeometry {
                id: "a".into(),
                rect: rect(0., 0., 300., 50.),
            },
            SessionGeometry {
                id: "b".into(),
                rect: rect(0., 150., 300., 50.),
            },
        ],
    };
    let mut state = DashboardState::default();
    let old = state.select(Some("a".into())).unwrap();
    let before = format!("{state:?}");
    assert!(navigate_session(&mut state, &scene, None, false, false).is_none());
    assert_eq!(format!("{state:?}"), before);
    assert!(!text_input_allowed(&state, true, false));
    let (focus, commands) = navigate_session(
        &mut state,
        &scene,
        Some(&Target::Session("a".into())),
        false,
        true,
    )
    .unwrap();
    assert_eq!(focus, Target::Session("b".into()));
    assert_eq!(state.table_offset, 100.);
    let [Command::Detail(ticket)] = commands.as_slice() else {
        panic!("missing detail")
    };
    assert_eq!(ticket.id, "b");
    assert!(ticket.generation > old.generation);
    let mut life = Lifecycle::new(1);
    let stamp = Stamp {
        owner: 1,
        epoch: 1,
        pixels: [300, 100],
        dpi: 96,
    };
    assert!(life.painted(stamp));
    life.invalidate();
    assert!(!life.accepts(1, 1, [300, 100], 96));
    // Simulate the rebuilt Scene after the downward reveal.
    scene.content[0].offset = 100.;
    for row in &mut scene.session_rows {
        row.rect.top -= 100.;
        row.rect.bottom -= 100.;
    }
    let (focus, _) = navigate_session(&mut state, &scene, Some(&focus), true, true).unwrap();
    assert_eq!(focus, Target::Session("a".into()));
    assert_eq!(state.table_offset, 0.);
    state.select(None);
    let (_, commands) = navigate_session(&mut state, &scene, Some(&focus), true, true).unwrap();
    assert!(commands.is_empty());
    assert!(state.selected.is_none());
    scene.modal = true;
    assert!(navigate_session(&mut state, &scene, Some(&focus), false, true).is_none());
}

#[test]
fn escape_dismisses_popup_without_closing_detail_or_changing_ticket() {
    use qb_tray::{
        geometry::Target,
        state::{DashboardState, Popup},
    };
    let mut state = DashboardState::default();
    let ticket = state.select(Some("a".into())).unwrap();
    assert!(state.activate(&Target::RangeMenu).is_empty());
    assert_eq!(state.popup, Some(Popup::Range));
    assert!(dismiss_popup(&mut state));
    assert!(state.popup.is_none());
    assert_eq!(state.selected.as_deref(), Some("a"));
    let mut detail = qb_tray::dashboard_model::SessionDetail::default();
    detail.session.id = ticket.id.clone();
    assert!(state.publish_detail(&ticket, detail));
    assert!(!dismiss_popup(&mut state));
}

#[test]
fn scene_popup_choices_close_popup_and_emit_only_the_selected_query() {
    use qb_tray::{
        geometry::Target,
        state::{Command, DashboardState, Popup, Range},
    };
    let mut state = DashboardState::default();
    assert!(state.activate(&Target::RangeMenu).is_empty());
    assert_eq!(state.popup, Some(Popup::Range));
    assert_eq!(
        state.activate(&Target::Range(Range::Today)),
        vec![Command::Sessions(Range::Today)]
    );
    assert!(state.popup.is_none());
    assert_eq!(state.range, Range::Today);
    assert!(state.activate(&Target::EventDaysMenu).is_empty());
    assert_eq!(state.popup, Some(Popup::EventDays));
    assert_eq!(
        state.activate(&Target::EventDays(7)),
        vec![Command::Activity(7)]
    );
    assert!(state.popup.is_none());
    assert_eq!(state.event_days, 7);
    state.activate(&Target::EventDaysMenu);
    assert!(dismiss_popup(&mut state));
    assert_eq!(state.event_days, 7);
}

#[test]
fn startup_requests_current_range_and_days_without_unselected_detail_or_usage() {
    use qb_tray::state::{Command, DashboardState, Range, Tab};
    let mut state = DashboardState::default();
    state.range = Range::Today;
    state.event_days = 7;
    state.tab = Tab::Events;
    assert_eq!(
        startup_commands(&mut state),
        vec![Command::Sessions(Range::Today), Command::Activity(7)]
    );
    assert_eq!(state.range, Range::Today);
    assert_eq!(state.tab, Tab::Events);
}

#[test]
fn startup_usage_and_selected_detail_use_new_ticket_without_carrying_stale_detail() {
    use qb_tray::{
        dashboard_model::SessionDetail,
        state::{Command, DashboardState, Range, Tab},
    };
    let mut state = DashboardState::default();
    state.range = Range::All;
    state.tab = Tab::Usage;
    state.event_days = 1;
    let old = state.select(Some("aaaaaaaaaaaa".into())).unwrap();
    let mut detail = SessionDetail::default();
    detail.session.id = old.id.clone();
    assert!(state.publish_detail(&old, detail.clone()));
    let commands = startup_commands(&mut state);
    assert_eq!(
        &commands[..3],
        &[
            Command::Sessions(Range::All),
            Command::Activity(1),
            Command::Usage
        ]
    );
    let Command::Detail(fresh) = &commands[3] else {
        panic!("missing fresh detail intent")
    };
    assert_eq!(fresh.id, old.id);
    assert!(fresh.generation > old.generation);
    assert!(state.detail.is_none());
    assert!(!state.publish_detail(&old, detail.clone()));
    assert!(state.publish_detail(fresh, detail));
}
fn geometry() -> Geometry {
    Geometry {
        left: 12,
        top: 40,
        width: 1320,
        height: 960,
        dpi: 96,
        maximized: false,
    }
}
#[test]
fn late_geometry_never_overwrites_user_move_or_new_owner() {
    let mut life = Lifecycle::new(4);
    assert!(life.load(3, Some(geometry())).is_none());
    life.placed(geometry(), 1, true);
    assert!(life.load(4, Some(geometry())).is_none());
    let mut next = Lifecycle::new(5);
    assert!(next.load(4, Some(geometry())).is_none());
    assert_eq!(next.load(5, Some(geometry())), Some(geometry()));
    assert!(next.load(5, Some(geometry())).is_none());
}
#[test]
fn old_frame_rejected_on_resize_invalidation_and_close() {
    let mut life = Lifecycle::new(4);
    let s = Stamp {
        owner: 4,
        epoch: 100,
        pixels: [1320, 960],
        dpi: 96,
    };
    assert!(life.painted(s));
    assert!(life.accepts(4, 100, [1320, 960], 96));
    assert!(!life.accepts(4, 100, [1980, 1440], 144));
    life.invalidate();
    assert!(!life.accepts(4, 100, [1320, 960], 96));
    life.painted(Stamp { epoch: 101, ..s });
    assert!(!life.accepts(4, 100, [1320, 960], 96));
    life.close();
    assert!(!life.painted(s));
    assert!(life.load(4, Some(geometry())).is_none());
}
#[test]
fn debounce_close_flush_and_payload_are_geometry_only() {
    let mut life = Lifecycle::new(1);
    life.placed(geometry(), 100, true);
    assert!(life.dirty(599, false).is_none());
    assert_eq!(life.dirty(600, false), Some((1, geometry())));
    assert!(life.dirty(700, true).is_none());
    let g = Geometry {
        left: -1500,
        ..geometry()
    };
    life.placed(g, 800, true);
    assert_eq!(life.dirty(801, true), Some((2, g)));
}
#[test]
fn provisional_default_cannot_overwrite_a_pending_load_even_on_close() {
    let mut life = Lifecycle::new(1);
    life.placed(geometry(), 0, false);
    assert!(life.dirty(1000, false).is_none());
    assert!(life.dirty(1000, true).is_none());
    assert!(life.load(1, None).is_none());
    assert_eq!(life.dirty(1000, false), Some((1, geometry())));
}
#[test]
fn small_work_area_negative_monitor_and_dpi_do_not_strand_window() {
    for dpi in [96, 120, 144, 192] {
        for work in [
            [-1920, 0, 0, 1080],
            [0, 0, 800, 600],
            [1920, -1080, 3840, 0],
        ] {
            let r = default_rect(work, dpi);
            assert!(r[0] >= work[0] && r[1] >= work[1] && r[2] <= work[2] && r[3] <= work[3]);
        }
    }
}
#[test]
fn interactive_resize_is_bounded_and_keeps_minimum_when_display_has_room() {
    let work = [-1920, 0, 0, 1080];
    let r = fit_rect([-2500, -50, -2400, 10], work, 96);
    assert_eq!(r, [-1920, 0, -896, 640]);
    assert_eq!(
        fit_rect([0, 0, 9000, 9000], [0, 0, 800, 600], 192),
        [0, 0, 800, 600]
    );
}
#[test]
fn utf16_ime_and_edit_selection_do_not_split_characters_or_insert_controls() {
    let mut edit = Editor::default();
    edit.insert("폴더");
    edit.utf16(0xd83d);
    edit.utf16(0xde00);
    assert_eq!(edit.text, "폴더😀");
    edit.erase(true);
    assert_eq!(edit.text, "폴더");
    edit.select_all();
    edit.composition = "검색".into();
    assert_eq!(edit.displayed(), "검색");
    assert_eq!(edit.text, "폴더");
    edit.insert("검색\r\n");
    assert_eq!(edit.text, "검색");
    edit.move_to(0, false);
    edit.erase(false);
    assert_eq!(edit.text, "색");
    edit.insert(&"x".repeat(500));
    assert_eq!(edit.text.chars().count(), 256);
}
#[test]
fn uia_search_values_respect_connection_modal_state_and_idempotence() {
    use qb_tray::{
        geometry::Target,
        state::{DashboardState, Popup},
    };
    let mut state = DashboardState::default();
    state.table_offset = 100.;
    state.events_offset = 200.;
    let before = format!("{state:?}");
    assert!(!set_search_value(
        &mut state,
        &Target::Search,
        "query".into(),
        false
    ));
    assert_eq!(format!("{state:?}"), before);
    state.popup = Some(Popup::Range);
    assert!(!set_search_value(
        &mut state,
        &Target::Search,
        "query".into(),
        true
    ));
    assert!(state.search.is_empty());
    state.popup = None;
    assert!(set_search_value(
        &mut state,
        &Target::Search,
        "query".into(),
        true
    ));
    assert_eq!(state.search, "query");
    assert_eq!(state.table_offset, 0.);
    assert_eq!(state.events_offset, 200.);
    state.table_offset = 50.;
    assert!(!set_search_value(
        &mut state,
        &Target::Search,
        "query".into(),
        true
    ));
    assert_eq!(state.table_offset, 50.);
    assert!(set_search_value(
        &mut state,
        &Target::EventSearch,
        "event".into(),
        true
    ));
    assert_eq!(state.events_offset, 0.);
    assert_eq!(state.search, "query");
}
