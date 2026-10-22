//! Pure DTO/model/measurement regressions; no native window or account profile.
// DashboardState has private generation fields, so integration tests use public fields.
#![allow(clippy::field_reassign_with_default)]
#[path = "support/r13_usage_forecast_fixture.rs"]
mod fixture;

use chrono::Duration;
use qb_core::usage_forecast::{ForecastBasis, ForecastOutcome};
use qb_tray::{
    dashboard_model as m,
    geometry::{Color, Draw, Metrics, Role, Scene, Target, height, width},
    state::{DashboardState, ScrollRegion, Tab},
    v2,
};

fn row_mut(snapshot: &mut m::DashboardSnapshot) -> &mut v2::AccountRow {
    snapshot
        .accounts
        .sections
        .iter_mut()
        .flat_map(|s| &mut s.rows)
        .find(|a| a.raw.id == "steady")
        .unwrap()
}

#[test]
fn r13_forecast_shared_card_text_cold_steady_weekly_changing_and_credits() {
    let (clock, snapshot) = fixture::fixture();
    let view = m::project(&snapshot, &mut DashboardState::default(), &clock);
    for tile in &view.usage {
        let account = snapshot
            .accounts
            .sections
            .iter()
            .flat_map(|s| &s.rows)
            .find(|a| a.raw.id == tile.id)
            .unwrap();
        let expected = qb_core::usage_forecast_display::lines(
            account.raw.usage_forecast.as_ref().unwrap(),
            account.raw.credits_in_use == Some(true),
        );
        assert_eq!(account.forecast_lines, expected);
        assert_eq!(tile.forecast_lines, expected);
        assert_eq!(
            view.groups
                .iter()
                .find(|g| g.id == tile.id)
                .unwrap()
                .forecast_lines,
            expected
        );
    }
    let steady = view.usage.iter().find(|t| t.id == "steady").unwrap();
    let text = steady.forecast_lines.join("\n");
    for fragment in [
        "최근 속도",
        "12.0%/시간",
        "구간 평균",
        "1.0%/시간",
        "지금 속도면",
        "20.0%/시간",
        "변동 중",
        "크레딧",
        "가장 먼저 소진 예상",
    ] {
        assert!(text.contains(fragment), "missing {fragment}: {text}");
    }
    let cold = view.usage.iter().find(|t| t.id == "cold").unwrap();
    assert_eq!(cold.forecast_lines, ["소진 예측: 측정 중"]);
    assert!(cold.projections.is_empty());
}

#[test]
fn r13_forecast_projection_uses_core_basis_and_clips_exhaustion_reset_and_edge() {
    let (clock, mut snapshot) = fixture::fixture();
    let mut state = DashboardState::default();
    let view = m::project(&snapshot, &mut state, &clock);
    let tile = view.usage.iter().find(|t| t.id == "steady").unwrap();
    let short = tile.projections.iter().find(|s| s.name == "5시간").unwrap();
    assert_eq!(
        short.points,
        [
            (clock.utc(), 30.),
            (clock.utc() + Duration::minutes(150), 0.)
        ]
    );
    let weekly = tile.projections.iter().find(|s| s.name == "주간").unwrap();
    assert_eq!(
        weekly.points,
        [(clock.utc(), 60.), (clock.utc() + Duration::hours(5), 55.)]
    );

    let window = &mut row_mut(&mut snapshot)
        .raw
        .usage_forecast
        .as_mut()
        .unwrap()
        .windows[0];
    window.basis = Some(ForecastBasis::Average);
    window.average_pct_per_hour = Some(4.);
    window.outcome = ForecastOutcome::NoExhaustionBeforeReset;
    window.exhaustion_at = None;
    window.reset_at = Some(clock.utc() + Duration::minutes(30));
    let view = m::project(&snapshot, &mut state, &clock);
    assert_eq!(
        view.usage
            .iter()
            .find(|t| t.id == "steady")
            .unwrap()
            .projections[0]
            .points,
        [
            (clock.utc(), 30.),
            (clock.utc() + Duration::minutes(30), 28.)
        ]
    );
}

#[test]
fn r13_forecast_future_lines_leave_observed_gaps_tooltips_and_uia_table_untouched() {
    let (clock, snapshot) = fixture::fixture();
    let mut state = DashboardState::default();
    state.tab = Tab::Usage;
    let view = m::project(&snapshot, &mut state, &clock);
    let scene = Scene::build(&view, &state, [1024., 640.], &Metrics::default());
    let chart = scene.charts.iter().find(|c| c.id == "steady").unwrap();
    assert!(chart.series[0].points.iter().any(|(_, p)| p.is_none()));
    assert!(
        chart
            .series
            .iter()
            .flat_map(|s| &s.points)
            .all(|(at, _)| *at <= clock.utc())
    );
    assert!(scene.chart_tooltip("steady", chart.rect.right).is_none());
    let now_x = chart.rect.left + width(chart.rect) * 19. / 24.;
    let dashed: Vec<_> = scene
        .draw
        .iter()
        .filter_map(|d| match d {
            Draw::Line {
                from,
                to,
                dash: Some([3., 3.]),
                ..
            } if from[0] >= now_x - 0.01 => Some((from, to)),
            _ => None,
        })
        .collect();
    assert_eq!(dashed.len(), 2);
    for (from, to) in dashed {
        assert!(from[0] >= chart.rect.left && to[0] <= chart.rect.right);
        for y in [from[1], to[1]] {
            assert!(y >= chart.rect.top && y <= chart.rect.bottom);
        }
    }
    let history = qb_tray::dashboard_history::project(
        view.usage.iter().find(|t| t.id == "steady").unwrap(),
        100,
    );
    assert!(history.rows.iter().all(|r| r.at <= clock.utc()));
    assert!(
        scene
            .accessibility
            .iter()
            .any(|n| n.role == Role::Text && n.key == "chart:steady:forecast:0")
    );
    let mut later = clock.clone();
    later.now += Duration::milliseconds(500);
    let next = m::project(&snapshot, &mut state, &later);
    assert_eq!(view.usage[0].projections, next.usage[0].projections);
    assert_eq!(view.usage[0].forecast_lines, next.usage[0].forecast_lines);
}

#[test]
fn r13_forecast_cached_clock_advances_value_without_changing_selected_slope() {
    let (clock, snapshot) = fixture::fixture();
    let mut later = clock.clone();
    later.now += Duration::minutes(10);
    let view = m::project(&snapshot, &mut DashboardState::default(), &later);
    let tile = view.usage.iter().find(|t| t.id == "steady").unwrap();
    let short = tile.projections.iter().find(|s| s.name == "5시간").unwrap();
    assert_eq!(
        short.points,
        [
            (later.utc(), 28.),
            (clock.utc() + Duration::minutes(150), 0.)
        ]
    );
    for (name, rate) in [("5시간", 12.), ("주간", 1.)] {
        let points = tile
            .projections
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .points;
        let hours = (points[1].0 - points[0].0).num_microseconds().unwrap() as f64 / 3_600_000_000.;
        assert!(((points[0].1 - points[1].1) / hours - rate).abs() < 1e-9);
        assert!(points[0].0 >= m::usage_domain(&later).1 - Duration::hours(5));
    }
    let mut micro = snapshot.clone();
    let as_of = clock.utc() + Duration::microseconds(25);
    row_mut(&mut micro)
        .raw
        .usage_forecast
        .as_mut()
        .unwrap()
        .as_of = as_of;
    let mut micro_clock = clock.clone();
    micro_clock.now += Duration::microseconds(50);
    let view = m::project(&micro, &mut DashboardState::default(), &micro_clock);
    assert!(
        view.usage
            .iter()
            .find(|t| t.id == "steady")
            .unwrap()
            .projections
            .iter()
            .all(|s| s.points[0].0 == as_of && s.points[0].1.is_finite())
    );
}

#[test]
fn r13_forecast_measured_wrapping_expands_tiles_and_normal_and_sticky_headers() {
    let (clock, mut snapshot) = fixture::fixture();
    // Keep the steady group scrollable even in the taller, wider layout.
    let session = snapshot.sessions.sessions[0].clone();
    snapshot.sessions.sessions = (0..24)
        .map(|index| {
            let mut row = session.clone();
            row.id = format!("{index:012x}");
            row
        })
        .collect();
    for size in [[1024., 640.], [1320., 960.]] {
        for tab in [Tab::Overview, Tab::Usage] {
            let mut state = DashboardState::default();
            state.tab = tab;
            let view = m::project(&snapshot, &mut state, &clock);
            let probe = Scene::build(&view, &state, size, &Metrics::default());
            let lines = &view
                .usage
                .iter()
                .find(|t| t.id == "steady")
                .unwrap()
                .forecast_lines;
            let mut metrics = Metrics::default();
            for measure in &probe.measurements {
                if measure.wrap && lines.contains(&measure.text) {
                    metrics.heights.insert(measure.key(), 64.);
                }
            }
            let measured = Scene::build(&view, &state, size, &metrics);
            let region = if tab == Tab::Usage {
                ScrollRegion::Usage
            } else {
                ScrollRegion::Table
            };
            let before = probe.content.iter().find(|r| r.region == region).unwrap();
            let after = measured
                .content
                .iter()
                .find(|r| r.region == region)
                .unwrap();
            assert!(after.content_height > before.content_height);
            let header = measured
                .targets
                .iter()
                .find(|t| t.target == Target::Group("steady".into()));
            let header_bounds = header.and_then(|target| {
                measured.draw.iter().find_map(|d| match d {
                    Draw::Fill {
                        rect,
                        color: Color::Footer,
                        radius: 6.,
                    } if rect.left == target.rect.left && rect.top == target.rect.top => {
                        Some(*rect)
                    }
                    _ => None,
                })
            });
            let forecast_rects: Vec<_> = measured
                .draw
                .iter()
                .filter_map(|d| match d {
                    Draw::Text { rect, measure, .. } if lines.contains(&measure.text) => {
                        assert!(measure.wrap);
                        assert!(height(*rect) >= 64.);
                        if let Some(header) = header_bounds {
                            assert!(rect.bottom <= header.bottom);
                        }
                        Some(*rect)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(forecast_rects.len(), lines.len());
            assert!(forecast_rects.windows(2).all(|r| r[0].bottom <= r[1].top));
            if tab == Tab::Overview {
                assert!(
                    after.max_offset >= 48.,
                    "fixture must permit sticky scrolling"
                );
                state.table_offset = 48.;
                let scrolled = Scene::build(&view, &state, size, &metrics);
                let scroll = scrolled
                    .content
                    .iter()
                    .find(|r| r.region == ScrollRegion::Table)
                    .unwrap();
                assert_eq!(scroll.offset, 48.);
                let sticky_control = scrolled
                    .accessibility
                    .iter()
                    .find(|n| {
                        n.key.starts_with("sticky:steady:control:")
                            && n.target == Some(Target::Group("steady".into()))
                    })
                    .unwrap();
                let sticky = scrolled
                    .draw
                    .iter()
                    .find_map(|d| match d {
                        Draw::Fill {
                            rect,
                            color: Color::Footer,
                            radius: 6.,
                        } if rect.left == sticky_control.rect.left
                            && rect.top == sticky_control.rect.top =>
                        {
                            Some(*rect)
                        }
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(height(sticky), height(header_bounds.unwrap()));
                assert!(
                    scrolled
                        .accessibility
                        .iter()
                        .any(|n| n.key.starts_with("sticky:steady:forecast:"))
                );
                for node in scrolled
                    .accessibility
                    .iter()
                    .filter(|n| n.key.starts_with("sticky:steady:forecast:"))
                {
                    assert!(node.rect.left >= sticky.left);
                    assert!(node.rect.top >= sticky.top);
                    assert!(node.rect.right <= sticky.right);
                    assert!(node.rect.bottom <= sticky.bottom);
                }
            }
        }
    }
}

#[test]
fn r13_forecast_absent_payload_keeps_incumbent_geometry_and_unconfirmed_has_no_projection() {
    let (clock, mut snapshot) = fixture::fixture();
    row_mut(&mut snapshot).raw.health.kind = "needs_relogin".into();
    let view = m::project(&snapshot, &mut DashboardState::default(), &clock);
    assert!(
        view.usage
            .iter()
            .find(|t| t.id == "steady")
            .unwrap()
            .projections
            .is_empty()
    );
    let legacy = v2::project(
        r#"{"accounts":[{"id":"legacy","provider":"codex","user_state":"active"}]}"#,
        clock.utc().into(),
    )
    .unwrap();
    assert!(legacy.raw.accounts[0].usage_forecast.is_none());
    let mut state = DashboardState::default();
    state.tab = Tab::Usage;
    let view = m::project(&m::DashboardSnapshot::new(legacy), &mut state, &clock);
    assert!(view.usage[0].forecast_lines.is_empty());
    assert!(view.usage[0].projections.is_empty());
    let scene = Scene::build(&view, &state, [1024., 640.], &Metrics::default());
    assert_eq!(
        scene
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Usage)
            .unwrap()
            .content_height,
        300.
    );
    assert!(
        !scene
            .accessibility
            .iter()
            .any(|n| n.key.contains(":forecast:"))
    );
}
