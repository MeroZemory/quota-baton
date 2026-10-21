use super::*;
use crate::dashboard_fixtures as fixtures;
use chrono::Duration;

fn fixture() -> (m::DashboardView, m::UsageTile) {
    let mut state = DashboardState::default();
    let v = m::project(&fixtures::snapshot(), &mut state, &fixtures::clock());
    let mut t = v.usage[0].clone();
    t.chip = "한도 도달".into();
    t.tone = v2::Tone::Limit;
    t.unconfirmed = false;
    t.truncated = false;
    t.paused = false;
    t.credits_spans.clear();
    t.last_observed = Some(v.clock.utc());
    t.windows = vec![
        m::AccountUsageWindow {
            label: "5시간".into(),
            remaining: "64% 남음".into(),
            reset: "09:40 초기화".into(),
            remaining_pct: 64,
        },
        m::AccountUsageWindow {
            label: "주간".into(),
            remaining: "65% 남음".into(),
            reset: "10/27 15:33 초기화".into(),
            remaining_pct: 65,
        },
    ];
    t.series = [("5시간", 64.), ("주간", 65.)]
        .into_iter()
        .map(|(name, pct)| m::Series {
            name: name.into(),
            points: vec![
                (v.clock.day_start + Duration::hours(1), Some(100.)),
                (v.clock.utc(), Some(pct)),
            ],
            resets: vec![],
            future_resets: vec![],
            remaining: Some(pct as u8),
        })
        .collect();
    (v, t)
}
fn scene(v: &m::DashboardView, t: &m::UsageTile) -> Scene {
    scene_at_width(v, t, 406.)
}
fn scene_at_width(v: &m::DashboardView, t: &m::UsageTile, tile_width: f32) -> Scene {
    let metrics = Metrics::default();
    let outer = rect(0., 0., tile_width, 288.);
    let mut b = Builder {
        scene: Scene {
            modal: false,
            rect: outer,
            content: vec![],
            targets: vec![],
            accessibility: vec![],
            draw: vec![],
            measurements: vec![],
            charts: vec![],
            tooltips: vec![],
            session_rows: vec![],
        },
        metrics: &metrics,
        clip: outer,
        scope: "usage-test".into(),
        enabled: true,
    };
    usage_tile(
        &mut b,
        v,
        t,
        outer,
        &history::project(t, history::CHART_ROW_BUDGET),
        &mut BTreeMap::new(),
    );
    b.scene
}
fn text<'a>(s: &'a Scene, value: &str) -> (usize, Rect, &'a Color) {
    s.draw
        .iter()
        .enumerate()
        .find_map(|(i, d)| match d {
            Draw::Text {
                rect,
                measure,
                color,
            } if measure.text == value => Some((i, *rect, color)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing text {value}"))
}
fn backed(s: &Scene, index: usize, bounds: Rect) {
    assert!(
        matches!(s.draw[index - 1], Draw::Fill { rect, color: Color::Surface, .. } if rect == bounds)
    );
}
fn endpoint_text<'a>(s: &'a Scene, value: &str) -> (usize, Rect, &'a Color) {
    s.draw.iter().enumerate().find_map(|(i, d)| match d {
        Draw::Text { rect, measure, color } if measure.text == value && i > 0
            && matches!(s.draw[i - 1], Draw::Fill { rect: backing, color: Color::Surface, .. } if backing == *rect)
            => Some((i, *rect, color)),
        _ => None,
    }).unwrap_or_else(|| panic!("missing backed endpoint {value}"))
}
type SeriesLine<'a> = (&'a [f32; 2], &'a [f32; 2], &'a Color, &'a Option<[f32; 2]>);
fn series_lines(s: &Scene) -> Vec<SeriesLine<'_>> {
    s.draw
        .iter()
        .filter_map(|d| match d {
            Draw::Line {
                from,
                to,
                color,
                width,
                dash,
            } if *width == 2. => Some((from, to, color, dash)),
            _ => None,
        })
        .collect()
}

#[test]
fn r10_usage_header_has_sixty_dip_divider_all_windows_and_limit_chip() {
    let (v, t) = fixture();
    let s = scene(&v, &t);
    assert!(s.draw.iter().any(|d| matches!(d,
        Draw::Line { from, to, .. } if from == &[0., 60.] && to == &[406., 60.])));
    assert!(s.draw.iter().any(|d| matches!(d,
        Draw::Stroke { color: Color::GroupBorder, radius, .. } if *radius == 6.)));
    let (chip_index, chip_rect, chip_color) = text(&s, "한도 도달");
    assert!(matches!(chip_color, Color::ChipFg(v2::Tone::Limit)));
    assert!(matches!(s.draw[chip_index - 1], Draw::Fill {
        rect, color: Color::ChipBg(v2::Tone::Limit), .. } if rect == chip_rect));
    for value in [
        "5시간 ",
        "64% 남음",
        " · 09:40 초기화",
        "주간 ",
        "65% 남음",
        " · 10/27 15:33 초기화",
    ] {
        let (_, r, _) = text(&s, value);
        assert_eq!(r.top, 36.);
    }
    let (_, first_reset, _) = text(&s, " · 09:40 초기화");
    let (_, second_label, _) = text(&s, "주간 ");
    assert_eq!(second_label.left - first_reset.right, 16.);
}

#[test]
fn r10_usage_end_label_positions_handle_empty_tiny_and_over_capacity_plots() {
    let plot = rect(0., 100., 100., 40.);
    assert!(end_label_positions(&[], plot).is_empty());
    assert!(end_label_positions(&[100.], rect(0., 100., 100., 23.)).is_empty());
    assert_eq!(
        end_label_positions(&[100., 100., 100.], plot),
        vec![106., 122.]
    );
    assert_eq!(
        end_label_positions(&[100., 100.], rect(0., 100., 100., 24.)),
        vec![106.]
    );
}

#[test]
fn r10_usage_end_labels_separate_symmetrically_clamp_and_have_backings() {
    let (v, mut t) = fixture();
    for (a, b) in [(64., 65.), (0., 1.), (99., 100.), (100., 0.), (0., 100.)] {
        t.series[0].points.last_mut().unwrap().1 = Some(a);
        t.series[1].points.last_mut().unwrap().1 = Some(b);
        let s = scene(&v, &t);
        let plot = s.charts[0].rect;
        let (ai, ar, _) = endpoint_text(&s, &format!("{a}%"));
        let (bi, br, _) = endpoint_text(&s, &format!("{b}%"));
        assert!((ar.top - br.top).abs() >= 14.);
        assert!(intersection(ar, br).is_none());
        for (i, r) in [(ai, ar), (bi, br)] {
            assert!(
                r.top >= plot.top + 6. && r.bottom <= plot.bottom - 2.,
                "endpoint pair {a}/{b}: label {r:?} outside strict plot inset {plot:?}"
            );
            assert!(r.left >= plot.left && r.right <= plot.right);
            backed(&s, i, r);
        }
        if a == 64. {
            let expected_mid = plot.bottom - height(plot) * 0.645 - 6.;
            assert!(((ar.top + br.top) / 2. - expected_mid).abs() < 0.01);
            let now_x = plot.left
                + width(plot)
                    * v.clock
                        .utc()
                        .signed_duration_since(v.clock.day_start)
                        .num_milliseconds() as f32
                    / v.clock
                        .day_end
                        .signed_duration_since(v.clock.day_start)
                        .num_milliseconds() as f32;
            assert!((ar.left - now_x - 6.).abs() < 0.01);
        }
    }
}

#[test]
fn r10_usage_now_caption_and_edge_endpoints_have_separate_bounded_rows() {
    let (v, mut t) = fixture();
    for tile_width in [406., 476.] {
        for future_reset in [false, true] {
            for (a, b) in [
                (100., 100.),
                (0., 0.),
                (99., 100.),
                (0., 1.),
                (64., 65.),
                (100., 0.),
                (0., 100.),
            ] {
                for (series, pct) in t.series.iter_mut().zip([a, b]) {
                    series.points.last_mut().unwrap().1 = Some(pct);
                    series.future_resets = if future_reset {
                        vec![v.clock.utc() + Duration::hours(1)]
                    } else {
                        vec![]
                    };
                }
                let s = scene_at_width(&v, &t, tile_width);
                let plot = s.charts[0].rect;
                let (_, now, _) = text(&s, "지금");
                assert!(now.top >= 60. && now.bottom <= plot.top - 6.);
                assert!(now.left >= plot.left && now.right <= plot.right);
                if future_reset {
                    let reset = format!(
                        "{} 초기화",
                        v.clock.time(Some(v.clock.utc() + Duration::hours(1)))
                    );
                    assert!(intersection(now, text(&s, &reset).1).is_none());
                }
                for pct in [a, b] {
                    let (i, label, _) = endpoint_text(&s, &format!("{pct}%"));
                    assert!(intersection(now, label).is_none());
                    assert!(label.top >= plot.top + 6. && label.bottom <= plot.bottom - 2.);
                    assert!(label.left >= plot.left && label.right <= plot.right);
                    backed(&s, i, label);
                }
            }
        }
    }
}

#[test]
fn r10_usage_full_legend_is_measured_contained_and_clear_of_header_and_tiles() {
    let (mut v, t) = fixture();
    v.usage = vec![t];
    let state = DashboardState {
        tab: Tab::Usage,
        ..Default::default()
    };
    let legend = "━ 5시간 (Free는 30일)  ┄ 주간  ⋮ 초기화  ▧ 크레딧으로 처리";
    let summary = format!(
        "남은 양 · 00:00–24:00 · {}",
        v.clock.day_label(v.clock.utc())
    );
    for size in [[1320., 960.], [1024., 640.]] {
        for measured_legend_width in [500., 600.] {
            let mut metrics = Metrics::default();
            let measure = Measure {
                text: legend.into(),
                size: 12.,
                line_height: 20.,
                width: 4096.,
                bold: false,
                wrap: false,
                face: Face::Text,
                align: TextAlign::Leading,
            };
            metrics.widths.insert(measure.key(), measured_legend_width);
            metrics.widths.insert(
                Measure {
                    text: summary.clone(),
                    ..measure
                }
                .key(),
                340.,
            );
            let s = Scene::build(&v, &state, size, &metrics);
            let (_, bounds, _) = text(&s, legend);
            assert_eq!(width(bounds), measured_legend_width + 4.);
            assert!(bounds.left >= 36. && bounds.right <= size[0] - 36.);
            for value in ["오늘 사용량", summary.as_str()] {
                assert!(intersection(bounds, text(&s, value).1).is_none());
            }
            assert!(
                s.targets
                    .iter()
                    .all(|target| intersection(bounds, target.rect).is_none())
            );
            assert!(s.draw.iter().all(|d| match d {
                Draw::Stroke {
                    rect,
                    color: Color::GroupBorder,
                    radius,
                    ..
                } if *radius == 6. => bounds.bottom + 8. <= rect.top,
                _ => true,
            }));
            assert_eq!(bounds.top, if size[0] == 1024. { 104. } else { 80. });
        }
    }
}

#[test]
fn r10_usage_equal_values_emit_one_neutral_endpoint_label() {
    let (v, mut t) = fixture();
    for series in &mut t.series {
        series.points.last_mut().unwrap().1 = Some(86.);
    }
    let s = scene(&v, &t);
    assert_eq!(
        s.draw
            .iter()
            .filter(|d| matches!(d, Draw::Text { measure, .. } if measure.text == "86%"))
            .count(),
        1
    );
    let (i, r, color) = text(&s, "86%");
    assert!(matches!(color, Color::Text));
    backed(&s, i, r);
}

#[test]
fn r10_usage_no_observation_copy_is_backed_at_midpoint_right_of_now() {
    let (v, mut t) = fixture();
    for series in &mut t.series {
        series.points.clear();
    }
    let s = scene(&v, &t);
    assert!(series_lines(&s).is_empty());
    let plot = s.charts[0].rect;
    let now_x = s
        .draw
        .iter()
        .find_map(|d| match d {
            Draw::Line {
                from,
                to,
                color: Color::Text,
                ..
            } if from[1] == plot.top && to[1] == plot.bottom => Some(from[0]),
            _ => None,
        })
        .unwrap();
    let (i, r, _) = text(&s, "오늘은 아직 관측한 값이 없습니다");
    assert_eq!(r.left, now_x + 12.);
    assert_eq!((r.top + r.bottom) / 2., (plot.top + plot.bottom) / 2.);
    backed(&s, i, r);
}

#[test]
fn r10_usage_relogin_stops_at_observation_and_hatches_only_the_unknown_span() {
    let (v, mut t) = fixture();
    let last = v.clock.utc() - Duration::hours(2);
    t.unconfirmed = true;
    t.last_observed = Some(last);
    for series in &mut t.series {
        series.points[1].0 = last;
        // A stray future point must never be painted as observed history.
        series.points.push((v.clock.utc(), Some(20.)));
    }
    let s = scene(&v, &t);
    let plot = s.charts[0].rect;
    let x = |at: DateTime<Utc>| {
        plot.left
            + width(plot)
                * at.signed_duration_since(v.clock.day_start)
                    .num_milliseconds() as f32
                / v.clock
                    .day_end
                    .signed_duration_since(v.clock.day_start)
                    .num_milliseconds() as f32
    };
    for (from, to, _, _) in series_lines(&s) {
        assert!(from[0] <= x(last) + 0.01 && to[0] <= x(last) + 0.01);
    }
    let hatch: Vec<_> = s
        .draw
        .iter()
        .filter_map(|d| match d {
            Draw::Line {
                from,
                to,
                color: Color::Track,
                dash: None,
                ..
            } => Some((from, to)),
            _ => None,
        })
        .collect();
    assert!(hatch.len() > 2);
    for (from, to) in &hatch {
        for p in [from, to] {
            assert!(
                p[0] >= x(last) - 0.01
                    && p[0] <= x(v.clock.utc()) + 0.01
                    && p[1] >= plot.top
                    && p[1] <= plot.bottom
            );
        }
        assert!(((from[0] - to[0]).abs() - (from[1] - to[1]).abs()).abs() < 0.01);
    }
    assert!((hatch[1].0[0] - hatch[0].0[0] - 6. * std::f32::consts::SQRT_2).abs() < 0.01);
    assert!(
        hatch
            .iter()
            .any(|(from, _)| (from[1] - plot.bottom).abs() < 6. * std::f32::consts::SQRT_2)
    );
    assert!(
        !s.draw.iter().any(|d| matches!(d, Draw::Text { measure, .. }
        if measure.text == "64%" || measure.text == "65%" || measure.text == "20%"))
    );
    let stamp = v.clock.time(Some(last));
    assert!(matches!(
        text(
            &s,
            &format!("{stamp} 이후 확인하지 못함 · 다시 로그인 필요")
        )
        .2,
        Color::Bad
    ));
    let (i, r, color) = text(&s, &format!("{stamp} 이후 확인 못 함"));
    assert!(matches!(color, Color::Bad));
    assert!((r.left - x(v.clock.utc()) - 8.).abs() < 0.01);
    backed(&s, i, r);
}

#[test]
fn r10_usage_paused_shortest_is_solid_gray_and_weekly_retains_dashes() {
    let (v, mut t) = fixture();
    let s = scene(&v, &t);
    let lines = series_lines(&s);
    assert!(
        lines
            .iter()
            .any(|(_, _, c, d)| matches!(c, Color::BarOk) && d.is_none())
    );
    assert!(
        lines
            .iter()
            .any(|(_, _, c, d)| matches!(c, Color::Series7d) && **d == Some([5., 3.]))
    );
    t.paused = true;
    let paused = scene(&v, &t);
    let lines = series_lines(&paused);
    assert!(!lines.is_empty());
    assert!(
        lines
            .iter()
            .any(|(_, _, c, d)| matches!(c, Color::BarPaused) && d.is_none())
    );
    assert!(
        lines
            .iter()
            .any(|(_, _, c, d)| matches!(c, Color::Series7d) && **d == Some([5., 3.]))
    );
    assert!(lines.iter().all(|(_, _, c, d)| {
        (matches!(c, Color::BarPaused) && d.is_none())
            || (matches!(c, Color::Series7d) && **d == Some([5., 3.]))
    }));
}

#[test]
fn r10_usage_unknown_sample_breaks_line_without_invented_endpoint() {
    let (v, mut t) = fixture();
    t.series.truncate(1);
    t.series[0]
        .points
        .insert(1, (v.clock.day_start + Duration::hours(2), None));
    let s = scene(&v, &t);
    assert!(series_lines(&s).is_empty());
    t.series[0].points.last_mut().unwrap().1 = None;
    let s = scene(&v, &t);
    assert!(
        !s.draw
            .iter()
            .any(|d| matches!(d, Draw::Text { measure, .. } if measure.text == "64%"))
    );
}

#[test]
fn r10_usage_future_annotation_and_bounded_accessible_history_survive() {
    let (v, mut t) = fixture();
    let future = v.clock.utc() + Duration::hours(1);
    t.series[0].future_resets.push(future);
    t.series[0].points = (0..2000)
        .map(|i| {
            (
                v.clock.day_start + Duration::seconds(i),
                Some((i % 100) as f64),
            )
        })
        .collect();
    let s = scene(&v, &t);
    text(&s, &format!("{} 초기화", v.clock.time(Some(future))));
    let plot = s.charts[0].rect;
    assert!(s.draw.iter().any(
        |d| matches!(d, Draw::Line { from, to, dash: Some([1., 3.]), .. }
        if from[0] == to[0] && from[1] == plot.top && to[1] == plot.bottom)
    ));
    assert!(s.accessibility.len() <= history::SCENE_NODE_BUDGET);
    assert!(
        s.accessibility
            .iter()
            .filter(|n| n.role == Role::Row)
            .count()
            <= history::CHART_ROW_BUDGET
    );
    assert!(
        s.accessibility
            .iter()
            .any(|n| n.role == Role::Table && n.hidden && n.value.as_deref() == Some("incomplete"))
    );
    assert!(
        s.accessibility
            .iter()
            .any(|n| n.role == Role::ColumnHeader && n.name == "주간 남은 양")
    );
    assert_eq!(s.charts[0].series[0].points.len(), 2000);
}

#[test]
fn r10_usage_tiles_share_row_height_at_wide_and_narrow_widths() {
    let (mut v, tile) = fixture();
    v.usage = vec![tile.clone(); 4];
    for (i, t) in v.usage.iter_mut().enumerate() {
        t.id = format!("tile-{i}");
    }
    v.usage[1].unconfirmed = true;
    v.usage[2].series.clear();
    let state = DashboardState {
        tab: Tab::Usage,
        ..Default::default()
    };
    for size in [[1320., 960.], [1024., 640.]] {
        let s = Scene::build(&v, &state, size, &Metrics::default());
        let tiles: Vec<_> = s
            .draw
            .iter()
            .filter_map(|d| match d {
                Draw::Stroke {
                    rect,
                    color: Color::GroupBorder,
                    radius,
                    ..
                } if *radius == 6. => Some(rect),
                _ => None,
            })
            .collect();
        assert!(tiles.len() >= 2);
        assert!(tiles.iter().all(|r| height(**r) == 288.));
        assert_eq!(tiles[0].top, tiles[1].top);
        assert_eq!(s.charts.len(), 4);
        assert!(s.accessibility.len() <= history::SCENE_NODE_BUDGET);
    }
}

#[test]
fn r101_crowded_resets_bound_captions_keep_markers_and_expose_overflow() {
    let (v, mut t) = fixture();
    for at_end in [false, true] {
        let start = if at_end {
            v.clock.day_end - Duration::minutes(12)
        } else {
            v.clock.utc() + Duration::hours(1)
        };
        let resets: Vec<_> = (0..40).map(|i| start + Duration::minutes(i)).collect();
        t.series[0].future_resets = resets.clone();
        t.series[1].future_resets.clear();
        for tile_width in [406., 476.] {
            let s = scene_at_width(&v, &t, tile_width);
            let plot = s.charts[0].rect;
            assert!(
                height(plot) >= 90.,
                "crowded captions must not consume the plot"
            );
            assert!(plot.top < plot.bottom && plot.bottom <= s.rect.bottom);
            let overflow = s
                .accessibility
                .iter()
                .find(|n| n.key.ends_with(":reset-overflow"))
                .unwrap();
            assert!(overflow.rect.bottom <= plot.top - 22.);
            assert!(
                overflow
                    .name
                    .contains(&v.clock.time(Some(*resets.last().unwrap())))
            );
            let captions: Vec<_> = s
                .draw
                .iter()
                .filter_map(|d| match d {
                    Draw::Text { rect, measure, .. }
                        if measure.text.ends_with(" 초기화")
                            && !measure.text.starts_with(" · ") =>
                    {
                        Some(*rect)
                    }
                    _ => None,
                })
                .collect();
            assert!(captions.len() <= 2);
            for r in captions {
                assert!(r.bottom <= plot.top - 22. && r.left >= plot.left && r.right <= plot.right);
            }
            let markers: Vec<_> = s
                .draw
                .iter()
                .filter_map(|d| match d {
                    Draw::Line {
                        from,
                        to,
                        dash: Some(dash),
                        ..
                    } if *dash == [1., 3.] && from[1] == plot.top && to[1] == plot.bottom => {
                        Some(from[0])
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(markers.len(), resets.len());
            for (marker, at) in markers.iter().zip(&resets) {
                let fraction = (at
                    .signed_duration_since(v.clock.day_start)
                    .num_milliseconds() as f64
                    / v.clock
                        .day_end
                        .signed_duration_since(v.clock.day_start)
                        .num_milliseconds() as f64)
                    .clamp(0., 1.);
                let expected = plot.left + fraction as f32 * width(plot);
                assert!((*marker - expected).abs() < 0.01);
            }
            if at_end {
                assert_eq!(*markers.last().unwrap(), plot.right);
            }
        }
    }
}

#[test]
fn r101_each_reset_caption_tracks_its_own_dashed_marker() {
    let (v, mut t) = fixture();
    for close in [false, true] {
        let resets = [
            v.clock.day_start + Duration::hours(1),
            v.clock.day_start + Duration::hours(23),
        ];
        let resets = if close {
            [resets[0], resets[0] + Duration::minutes(1)]
        } else {
            resets
        };
        for (series, at) in t.series.iter_mut().zip(resets) {
            series.future_resets = vec![at];
        }
        for tile_width in [406., 476.] {
            let s = scene_at_width(&v, &t, tile_width);
            let plot = s.charts[0].rect;
            let first_label = text(&s, &format!("{} 초기화", v.clock.time(Some(resets[0])))).1;
            let second_label = text(&s, &format!("{} 초기화", v.clock.time(Some(resets[1])))).1;
            assert!(intersection(first_label, second_label).is_none());
            for at in resets {
                let (_, label, _) = text(&s, &format!("{} 초기화", v.clock.time(Some(at))));
                let fraction = at
                    .signed_duration_since(v.clock.day_start)
                    .num_milliseconds() as f32
                    / v.clock
                        .day_end
                        .signed_duration_since(v.clock.day_start)
                        .num_milliseconds() as f32;
                let marker = plot.left + width(plot) * fraction;
                assert!(label.left <= marker && marker <= label.right);
                assert!(label.left >= plot.left && label.right <= plot.right);
                assert!(s.draw.iter().any(|draw| matches!(draw,
                Draw::Line { from, to, dash: Some(_), .. }
                    if (from[0] - marker).abs() < 0.01 && from[0] == to[0]
                    && from[1] == plot.top && to[1] == plot.bottom)));
            }
        }
    }
}
