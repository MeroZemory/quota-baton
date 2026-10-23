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
        let forecast = account.raw.usage_forecast.as_ref().unwrap();
        let summary = qb_core::usage_forecast_display::summary(forecast, false);
        let expected = vec![summary.text];
        let detail = qb_core::usage_forecast_display::lines(
            forecast,
            account.raw.credits_in_use == Some(true),
        )
        .join("\n");
        assert_eq!(account.forecast_lines, expected);
        assert_eq!(account.forecast_tooltip, detail);
        assert_eq!(account.forecast_warning, summary.warning);
        assert_eq!(tile.forecast_tooltip, detail);
        assert_eq!(tile.forecast_warning, summary.warning);
        assert_eq!(tile.forecast_lines, expected);
        let group = view.groups.iter().find(|g| g.id == tile.id).unwrap();
        assert_eq!(group.forecast_lines, expected);
        assert_eq!(group.forecast_tooltip, detail);
        assert_eq!(group.forecast_warning, summary.warning);
    }
    let steady = view.usage.iter().find(|t| t.id == "steady").unwrap();
    let text = &steady.forecast_tooltip;
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
    assert_eq!(cold.forecast_lines, ["측정 중"]);
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
    assert_eq!(view.usage[0].forecast_lines, ["측정 중"]);
    assert_eq!(view.usage[0].forecast_tooltip, "측정 중");
    assert!(!view.usage[0].forecast_warning);
    assert!(view.usage[0].projections.is_empty());

    fn check_fit(scene: &Scene) -> (qb_tray::Rect, qb_tray::Rect, f32) {
        let tiles: Vec<_> = scene
            .draw
            .iter()
            .filter_map(|draw| match draw {
                Draw::Stroke {
                    rect,
                    color: Color::GroupBorder,
                    radius: 6.,
                    ..
                } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(tiles.len(), 1, "the legacy fixture has one account tile");
        let tile = tiles[0];
        let texts: Vec<_> = scene
            .draw
            .iter()
            .filter_map(|draw| match draw {
                Draw::Text { rect, measure, .. } if measure.text == "측정 중" && measure.wrap => {
                    Some(*rect)
                }
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 1, "one Measuring paragraph must be painted");
        let text = texts[0];
        assert!(width(text) > 0. && height(text) > 0.);
        assert!(text.left >= tile.left && text.right <= tile.right);
        assert!(text.top >= tile.top && text.bottom <= tile.bottom);
        let nodes: Vec<_> = scene
            .accessibility
            .iter()
            .filter(|node| node.key == "chart:legacy:forecast:0")
            .collect();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].role, Role::Text);
        assert_eq!(nodes[0].name, "측정 중");
        assert_eq!(nodes[0].parent.as_deref(), Some("chart:legacy"));
        assert_eq!(
            nodes[0].rect, text,
            "summary must be visible without clipping"
        );
        assert!(
            scene
                .tooltips
                .iter()
                .any(|tip| tip.rect == text && tip.text == "측정 중")
        );
        let region = scene
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Usage)
            .unwrap();
        assert_eq!(region.offset, region.max_offset);
        // The region includes the provider panels before this last account row,
        // and the native row's 12-DIP trailing gutter. No fixed total height.
        assert!(tile.top > region.viewport.top - region.offset);
        assert!(
            (region.viewport.top - region.offset + region.content_height - tile.bottom - 12.).abs()
                < 0.01
        );
        (tile, text, region.content_height)
    }

    let size = [1024., 640.];
    let probe = Scene::build(&view, &state, size, &Metrics::default());
    state.usage_offset = probe
        .content
        .iter()
        .find(|r| r.region == ScrollRegion::Usage)
        .unwrap()
        .max_offset;
    let visible = Scene::build(&view, &state, size, &Metrics::default());
    let (before_tile, before_text, before_content) = check_fit(&visible);

    // Supply a taller text measurement to prove that allocation, scrolling and
    // accessible bounds consume the same metric rather than merely painting it.
    let mut metrics = Metrics::default();
    for measure in &probe.measurements {
        if measure.wrap && measure.text == "측정 중" {
            metrics.heights.insert(measure.key(), 64.);
        }
    }
    assert!(!metrics.heights.is_empty());
    let probe = Scene::build(&view, &state, size, &metrics);
    state.usage_offset = probe
        .content
        .iter()
        .find(|r| r.region == ScrollRegion::Usage)
        .unwrap()
        .max_offset;
    let measured = Scene::build(&view, &state, size, &metrics);
    let (after_tile, after_text, after_content) = check_fit(&measured);
    assert_eq!(height(after_text), 64.);
    let extra = height(after_text) - height(before_text);
    assert!(extra > 0.);
    assert!((height(after_tile) - height(before_tile) - extra).abs() < 0.01);
    assert!((after_content - before_content - extra).abs() < 0.01);
}
