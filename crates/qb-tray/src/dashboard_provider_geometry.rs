//! Provider account-equivalents. Numerical inputs stay in the typed model.
use super::*;
use crate::provider_usage::{Direction, Projection, Provider};

fn en() -> bool {
    crate::i18n::language() == crate::i18n::UiLanguage::En
}
fn copy(ko: &str, english: &str) -> String {
    if en() { english } else { ko }.into()
}
fn name(p: &Projection) -> &'static str {
    match p.provider {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
    }
}
fn verdict(p: &Projection) -> String {
    let Some(v) = p.verdict else {
        return copy("비교 측정 중", "Measuring comparison");
    };
    let n = format!("{:.1}", v.tenths as f64 / 10.);
    match v.direction {
        Direction::Ahead => {
            if en() {
                format!("{n} accounts ahead of pace")
            } else {
                format!("권장선보다 {n}계정분 여유")
            }
        }
        Direction::Behind => {
            if en() {
                format!("{n} accounts behind pace")
            } else {
                format!("권장선보다 {n}계정분 부족")
            }
        }
        Direction::OnPace => copy("권장선과 비슷함", "On pace"),
    }
}
fn note(p: &Projection) -> String {
    let mut s = if en() {
        format!(
            "Current cohort · measured {}/{} · paused {} / disabled {} / other {} excluded · not weighted by plan",
            p.current.remaining.known,
            p.cohort,
            p.excluded_paused,
            p.excluded_disabled,
            p.excluded_other
        )
    } else {
        format!(
            "현재 계정 기준 · 측정 {}/{} · 일시중지 {} / 비활성 {} / 기타 {} 제외 · 요금제 가중치 없음",
            p.current.remaining.known,
            p.cohort,
            p.excluded_paused,
            p.excluded_disabled,
            p.excluded_other
        )
    };
    if p.history_truncated {
        s.push_str(&copy(" · 일부 기록", " · Partial history"));
    }
    s
}
fn quantity(n: Option<f64>) -> String {
    n.map(|n| {
        if en() {
            format!("{n:.1} accounts")
        } else {
            format!("{n:.1}계정분")
        }
    })
    .unwrap_or_else(m::unknown)
}

fn current_quantity(p: &Projection) -> String {
    match p.current.remaining.value {
        Some(n) if en() => format!("{n:.1} of {} account-equivalents left", p.cohort),
        Some(n) => format!("{}계정 중 {n:.1}계정분 남음", p.cohort),
        None if en() => format!(
            "Remaining unknown · measured {}/{} accounts",
            p.current.remaining.known, p.cohort
        ),
        None => format!(
            "남은 양 측정 중 · {}/{}계정 측정",
            p.current.remaining.known, p.cohort
        ),
    }
}

const GAP_COLUMNS: usize = 1024;

/// One panel-wide sweep. A column must fit wholly in one verified span;
/// boundaries and sign crossings are omitted instead of inventing gap area.
fn gap_columns(p: &Projection, plot: Rect) -> Vec<(Rect, Color)> {
    let w = width(plot) as f64;
    let duration = (p.to - p.from).num_milliseconds() as f64;
    if !w.is_finite() || w <= 0. || duration <= 0. || p.cohort == 0 {
        return vec![];
    }
    let columns = (w.ceil() as usize).min(GAP_COLUMNS);
    let mut result = Vec::with_capacity(columns);
    let mut span = 0;
    let now = (p.now - p.from).num_milliseconds() as f64;
    let max = p.cohort as f64;
    let y = |n: f64| plot.bottom - (n / max).clamp(0., 1.) as f32 * height(plot);
    for i in 0..columns {
        let left = duration * i as f64 / columns as f64;
        let right = duration * (i + 1) as f64 / columns as f64;
        if right > now {
            break;
        }
        while span < p.gap_spans.len()
            && (p.gap_spans[span].to - p.from).num_milliseconds() as f64 <= left
        {
            span += 1;
        }
        let Some(gap) = p.gap_spans.get(span) else {
            break;
        };
        let start = (gap.from - p.from).num_milliseconds() as f64;
        let end = (gap.to - p.from).num_milliseconds() as f64;
        if start > left || end < right || end <= start {
            continue;
        }
        let pace = |t| gap.pace_from + (gap.pace_to - gap.pace_from) * (t - start) / (end - start);
        let a = pace(left);
        let z = pace(right);
        if ![a, z, gap.remaining]
            .iter()
            .all(|n| n.is_finite() && *n >= 0. && *n <= max)
        {
            continue;
        }
        let (low, high, tone) = if gap.remaining > a && gap.remaining > z {
            (a.max(z), gap.remaining, v2::Tone::Ok)
        } else if gap.remaining < a && gap.remaining < z {
            (gap.remaining, a.min(z), v2::Tone::Warn)
        } else {
            continue;
        };
        let x0 = plot.left + (w * i as f64 / columns as f64) as f32;
        let x1 = plot.left + (w * (i + 1) as f64 / columns as f64) as f32;
        result.push((
            rect(x0, y(high), x1 - x0, (y(low) - y(high)).max(0.)),
            Color::ChipBg(tone),
        ));
    }
    result
}

fn line_height(b: &mut Builder<'_>, value: &str, w: f32, bold: bool) -> f32 {
    b.measured_height(value, w.max(1.), 12., bold)
}

pub(super) fn height_for(b: &mut Builder<'_>, p: &Projection, w: f32) -> f32 {
    let inner = (w - 24.).max(1.);
    let title = if en() {
        format!("{} · Weekly / 30-day remaining", name(p))
    } else {
        format!("{} · 주간 / 30일 남은 양", name(p))
    };
    218. + b.measured_height(&title, inner, 13., true)
        + line_height(b, &current_quantity(p), inner, false)
        + line_height(b, &verdict(p), inner, true)
        + line_height(b, &note(p), inner, false)
        + line_height(
            b,
            &copy(
                "━ 남은 계정분  ┄ 권장선  ▧ 차이",
                "━ Remaining accounts  ┄ Pace  ▧ Gap",
            ),
            inner,
            false,
        )
}

pub(super) fn panel(b: &mut Builder<'_>, v: &m::DashboardView, p: &Projection, r: Rect) {
    let visible = intersection(r, b.clip).is_some();
    let title = if en() {
        format!("{} · Weekly / 30-day remaining", name(p))
    } else {
        format!("{} · 주간 / 30일 남은 양", name(p))
    };
    let w = (width(r) - 24.).max(1.);
    let title_h = b.measured_height(&title, w, 13., true);
    let verdict = verdict(p);
    let vh = line_height(b, &verdict, w, true);
    let quantity_line = current_quantity(p);
    let qh = line_height(b, &quantity_line, w, false);
    let legend = copy(
        "━ 남은 계정분  ┄ 권장선  ▧ 차이",
        "━ Remaining accounts  ┄ Pace  ▧ Gap",
    );
    let lh = line_height(b, &legend, w, false);
    let note = note(p);
    let nh = line_height(b, &note, w, false);
    let top = r.top + 12. + title_h + 6. + vh + 6. + qh + 6. + lh + 8.;
    let max = p.cohort.max(1) as f64;
    let axis = format!("{max:.1}");
    let gutter = b.measured_width(&axis, 11., false).max(28.) + 8.;
    let plot = rect(r.left + 12. + gutter, top, (w - gutter).max(1.), 140.);
    let total = p.to.signed_duration_since(p.from).num_milliseconds().max(1) as f64;
    let x = |at: DateTime<Utc>| {
        plot.left
            + ((at - p.from).num_milliseconds() as f64 / total).clamp(0., 1.) as f32 * width(plot)
    };
    let y = |n: f64| plot.bottom - (n / max).clamp(0., 1.) as f32 * height(plot);
    if visible {
        b.fill(r, Color::Surface, 6.);
        b.stroke(r, Color::Border, 1., 6.);
        let mut text_top = r.top + 12.;
        for (text, h, size, bold, color, after) in [
            (title.clone(), title_h, 13., true, Color::Text, 6.),
            (verdict.clone(), vh, 12., true, Color::Text, 6.),
            (quantity_line, qh, 12., false, Color::Text, 6.),
            (legend, lh, 12., false, Color::Secondary, 8.),
        ] {
            b.text(
                rect(r.left + 12., text_top, w, h),
                text,
                size,
                bold,
                color,
                true,
            );
            text_top += h + after;
        }
        for n in [0., max / 2., max] {
            b.text(
                rect(r.left + 8., y(n) - 7., gutter - 4., 16.),
                format!("{n:.1}"),
                11.,
                false,
                Color::Secondary,
                false,
            );
        }
        if let Some(plot_clip) = intersection(plot, b.clip) {
            b.scene.draw.push(Draw::Clip(plot_clip));
            b.fill(
                rect(
                    x(p.now),
                    plot.top,
                    (plot.right - x(p.now)).max(0.),
                    height(plot),
                ),
                Color::Footer,
                0.,
            );
            for (area, color) in gap_columns(p, plot) {
                b.fill(area, color, 0.);
            }
            for n in [0., max / 2., max] {
                b.line(
                    [plot.left, y(n)],
                    [plot.right, y(n)],
                    Color::Border,
                    1.,
                    Some([2., 3.]),
                );
            }
            for pair in p.knots.windows(2) {
                let [a, z] = [pair[0], pair[1]];
                if let (Some(start), Some(end)) = (a.right.pace.value, z.left.pace.value) {
                    b.line(
                        [x(a.at), y(start)],
                        [x(z.at), y(end)],
                        Color::Series7d,
                        1.5,
                        Some([4., 3.]),
                    );
                }
                if a.at <= p.now
                    && z.at <= p.now
                    && let (Some(start), Some(end)) =
                        (a.right.remaining.value, z.left.remaining.value)
                {
                    b.line(
                        [x(a.at), y(start)],
                        [x(z.at), y(end)],
                        Color::BarOk,
                        2.,
                        None,
                    );
                }
            }
            for knot in &p.knots {
                if let (Some(left), Some(right)) = (knot.left.pace.value, knot.right.pace.value)
                    && (left - right).abs() > f64::EPSILON
                {
                    b.line(
                        [x(knot.at), y(left)],
                        [x(knot.at), y(right)],
                        Color::Series7d,
                        1.,
                        Some([2., 3.]),
                    );
                }
                if knot.at <= p.now
                    && let (Some(left), Some(right)) =
                        (knot.left.remaining.value, knot.right.remaining.value)
                    && (left - right).abs() > f64::EPSILON
                {
                    b.line(
                        [x(knot.at), y(left)],
                        [x(knot.at), y(right)],
                        Color::BarOk,
                        2.,
                        None,
                    );
                }
            }
            if let Some(value) = p.current.remaining.value {
                b.line(
                    [(x(p.now) - 2.).max(plot.left), y(value)],
                    [(x(p.now) + 2.).min(plot.right), y(value)],
                    Color::BarOk,
                    3.,
                    None,
                );
            }
            b.line(
                [x(p.now), plot.top],
                [x(p.now), plot.bottom],
                Color::Text,
                1.,
                None,
            );
            b.scene.draw.push(Draw::Unclip);
        }
        let now_label = copy("지금", "Now");
        let now_w = b.measured_width(&now_label, 10., true) + 2.;
        b.text(
            rect(
                (x(p.now) - now_w / 2.).clamp(plot.left, (plot.right - now_w).max(plot.left)),
                plot.top,
                now_w,
                16.,
            ),
            now_label,
            10.,
            true,
            Color::Text,
            false,
        );
        for hours in [0, 6, 12, 18, 24] {
            let at = p.from + chrono::Duration::hours(hours);
            let text = at
                .with_timezone(v.clock.now.offset())
                .format("%H:%M")
                .to_string();
            let label_w = b.measured_width(&text, 10., false) + 2.;
            let left =
                (x(at) - label_w / 2.).clamp(plot.left, (plot.right - label_w).max(plot.left));
            b.text(
                rect(left, plot.bottom + 4., label_w, 16.),
                text,
                10.,
                false,
                Color::Secondary,
                false,
            );
        }
        b.text(
            rect(r.left + 12., plot.bottom + 28., w, nh),
            note.clone(),
            12.,
            false,
            Color::Secondary,
            true,
        );
    }
    let summary = format!(
        "{title} · {verdict} · {} {} · {} {} · {note}",
        copy("남은 양", "Remaining"),
        quantity(p.current.remaining.value),
        copy("권장선", "Pace"),
        quantity(p.current.pace.value)
    );
    let id = format!("provider-usage:{}", name(p).to_lowercase());
    if let Some(hit) = intersection(plot, b.clip) {
        b.scene.targets.push(HitTarget {
            rect: hit,
            target: Target::Chart(id.clone()),
            enabled: true,
            tooltip: Some(summary.clone()),
        });
    }
    let chart_node = b.semantic(
        id.clone(),
        None,
        Role::Chart,
        summary.clone(),
        plot,
        Some(Target::Chart(id.clone())),
    );
    b.scene.accessibility[chart_node].hidden = intersection(plot, b.clip).is_none();
    // A bounded, explicitly representative table is presentation only.
    let table_id = format!("{id}:table");
    let table = b.semantic(
        table_id.clone(),
        Some(id.clone()),
        Role::Table,
        copy(
            "대표 시각 · 남은 계정분 / 권장선 · 최대 64행",
            "Representative times · remaining accounts / pace · up to 64 rows",
        ),
        plot,
        None,
    );
    b.scene.accessibility[table].hidden = true;
    let stride = p.knots.len().div_ceil(64).max(1);
    for (i, knot) in p.knots.iter().step_by(stride).take(64).enumerate() {
        let value = format!(
            "{} · {} · {}",
            v.clock.time(Some(knot.at)),
            quantity(knot.right.remaining.value),
            quantity(knot.right.pace.value)
        );
        let ix = b.semantic(
            format!("{id}:sample:{i}"),
            Some(table_id.clone()),
            Role::Text,
            value,
            plot,
            None,
        );
        b.scene.accessibility[ix].hidden = true;
    }
    let mut future_resets: Vec<_> = p
        .knots
        .iter()
        .filter(|k| {
            k.at > p.now
                && k.left
                    .pace
                    .value
                    .zip(k.right.pace.value)
                    .is_some_and(|(a, b)| (a - b).abs() > f64::EPSILON)
        })
        .map(|k| k.at)
        .collect();
    // The existing visible-time scanner consumes this field as deadlines.
    // Coverage expiry/phase disappearance needs the same wake as a reset jump.
    if let Some(at) = p.next_change.filter(|at| *at > p.now) {
        future_resets.push(at);
    }
    future_resets.sort_unstable();
    future_resets.dedup();
    b.scene.charts.push(ChartData {
        id,
        rect: plot,
        from: p.from,
        to: p.to,
        now: p.now,
        series: vec![m::Series {
            name: copy("권장선", "Pace"),
            points: vec![],
            resets: vec![],
            future_resets,
            remaining: None,
        }],
        unconfirmed: false,
        offset: *v.clock.now.offset(),
        provider_summary: Some(summary),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_usage::{
        self as p, AccountInput, AccountState, Observation, Phase, WindowInput, WindowKind,
    };

    fn projection(provider: Provider, now: DateTime<Utc>) -> Projection {
        let reset = now + chrono::Duration::hours(60);
        p::project(
            provider,
            &[AccountInput {
                id: "sample-acct".into(),
                provider,
                state: AccountState::Active,
                plan: "Sample".into(),
                window: Some(WindowInput {
                    kind: WindowKind::Weekly,
                    observations: vec![Observation {
                        at: now - chrono::Duration::minutes(2),
                        used_pct: Some(35.),
                        valid_until: Some(now + chrono::Duration::minutes(3)),
                    }],
                    phases: vec![Phase {
                        start: reset - chrono::Duration::days(7),
                        reset,
                    }],
                    resets: vec![],
                    truncated: false,
                }),
            }],
            now,
        )
        .unwrap()
    }

    #[test]
    fn provider_charts_precede_accounts_and_use_explicit_units() {
        let clock = crate::dashboard_fixtures::clock();
        for size in [[1024., 640.], [1320., 900.]] {
            let mut state = DashboardState {
                tab: Tab::Usage,
                ..Default::default()
            };
            let mut view = m::project(&crate::dashboard_fixtures::snapshot(), &mut state, &clock);
            view.provider_usage = vec![
                projection(Provider::Claude, clock.utc()),
                projection(Provider::Codex, clock.utc()),
            ];
            let scene = Scene::build(&view, &state, size, &Metrics::default());
            let providers: Vec<_> = scene
                .charts
                .iter()
                .filter(|c| c.provider_summary.is_some())
                .collect();
            assert_eq!(providers.len(), 2);
            for c in providers {
                assert_eq!((c.from, c.to), m::usage_domain(&clock));
                let hover = scene.chart_tooltip(&c.id, c.rect.left).unwrap();
                assert!(!hover.text.contains('%'));
                assert!(hover.text.contains("0.6") || hover.text.contains("0.7"));
                assert!(c.rect.left >= 0. && c.rect.right <= size[0]);
            }
            let first_account = scene
                .charts
                .iter()
                .find(|c| c.provider_summary.is_none())
                .unwrap();
            assert!(first_account.rect.top > scene.charts[0].rect.bottom);
            assert!(scene.accessibility.len() <= history::SCENE_NODE_BUDGET);
            let region = scene
                .content
                .iter()
                .find(|r| r.region == ScrollRegion::Usage)
                .unwrap();
            assert!(region.max_offset > 0.);
        }
    }

    fn span(p: &Projection, remaining: f64, a: f64, z: f64) -> p::GapSpan {
        p::GapSpan {
            from: p.from,
            to: p.to,
            remaining,
            pace_from: a,
            pace_to: z,
        }
    }

    #[test]
    fn gap_columns_bound_whole_panel_and_do_not_bridge_dense_boundaries() {
        let mut p = projection(Provider::Claude, crate::dashboard_fixtures::clock().utc());
        p.now = p.to;
        p.gap_spans = vec![span(&p, 0.9, 0.1, 0.2)];
        let plot = rect(10., 20., 4096., 140.);
        let fills = gap_columns(&p, plot);
        assert_eq!(fills.len(), GAP_COLUMNS);
        for (r, _) in &fills {
            assert!(
                r.left.is_finite()
                    && r.top.is_finite()
                    && r.right.is_finite()
                    && r.bottom.is_finite()
            );
            assert!(
                r.left >= plot.left
                    && r.right <= plot.right
                    && r.top >= plot.top
                    && r.bottom <= plot.bottom
            );
        }
        let duration = p.to - p.from;
        p.gap_spans = (0..4096)
            .map(|i| p::GapSpan {
                from: p.from + duration * i / 4096,
                to: p.from + duration * (i + 1) / 4096,
                remaining: 0.9,
                pace_from: 0.1,
                pace_to: 0.2,
            })
            .collect();
        assert!(
            gap_columns(&p, plot).is_empty(),
            "no column may bridge independent span boundaries"
        );
    }

    #[test]
    fn gap_columns_use_common_edge_gap_and_skip_sign_crossing_and_unknown() {
        let mut p = projection(Provider::Claude, crate::dashboard_fixtures::clock().utc());
        p.now = p.to;
        p.gap_spans = vec![span(&p, 0.5, 0., 1.)];
        let plot = rect(0., 0., 4., 100.);
        let fills = gap_columns(&p, plot);
        assert_eq!(fills.len(), 2);
        assert!(matches!(fills[0].1, Color::ChipBg(v2::Tone::Ok)));
        assert!(matches!(fills[1].1, Color::ChipBg(v2::Tone::Warn)));
        assert_eq!(
            (
                fills[0].0.left,
                fills[0].0.right,
                fills[0].0.top,
                fills[0].0.bottom
            ),
            (0., 1., 50., 75.)
        );
        assert_eq!(
            (
                fills[1].0.left,
                fills[1].0.right,
                fills[1].0.top,
                fills[1].0.bottom
            ),
            (3., 4., 25., 50.)
        );
        let original = span(&p, 0.9, 0.1, 0.2);
        p.gap_spans = vec![
            p::GapSpan {
                to: p.from + (p.to - p.from) * 2 / 5,
                ..original
            },
            p::GapSpan {
                from: p.from + (p.to - p.from) * 3 / 5,
                ..original
            },
        ];
        let fills = gap_columns(&p, plot);
        assert_eq!(fills.len(), 2, "unknown middle must stay unshaded");
        assert_eq!((fills[0].0.left, fills[1].0.left), (0., 3.));
        p.gap_spans[0].remaining = f64::NAN;
        assert_eq!(gap_columns(&p, plot).len(), 1);
        p.gap_spans = vec![span(&p, 0.9, 0.1, 0.2)];
        p.now = p.from + (p.to - p.from) / 2;
        let fills = gap_columns(&p, plot);
        assert_eq!(fills.len(), 2);
        assert!(
            fills.iter().all(|(r, _)| r.right <= 2.),
            "future observed gap must stay empty"
        );
    }

    fn panel_scene(p: &Projection, offscreen: bool) -> (Scene, Rect, Rect) {
        let clock = crate::dashboard_fixtures::clock();
        let mut state = DashboardState::default();
        let view = m::project(&crate::dashboard_fixtures::snapshot(), &mut state, &clock);
        let metrics = Metrics::default();
        let clip = rect(0., 0., 420., 240.);
        let mut b = Builder {
            scene: Scene {
                modal: false,
                rect: clip,
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
            clip,
            scope: "provider-test".into(),
            enabled: true,
        };
        let h = height_for(&mut b, p, 420.);
        let bounds = rect(0., if offscreen { 1000. } else { 0. }, 420., h);
        panel(&mut b, &view, p, bounds);
        (b.scene, bounds, clip)
    }

    #[test]
    fn provider_plot_clip_balanced_offscreen_retains_semantics_without_paint() {
        let p = projection(Provider::Claude, crate::dashboard_fixtures::clock().utc());
        let (scene, _, clip) = panel_scene(&p, false);
        let plot = scene.charts[0].rect;
        let expected = intersection(plot, clip).unwrap();
        let mut depth = 0;
        let mut clips = 0;
        for draw in &scene.draw {
            match draw {
                Draw::Clip(r) => {
                    assert_eq!(
                        (r.left, r.top, r.right, r.bottom),
                        (expected.left, expected.top, expected.right, expected.bottom)
                    );
                    depth += 1;
                    clips += 1;
                }
                Draw::Unclip => {
                    depth -= 1;
                    assert_eq!(depth, 0);
                }
                Draw::Line { .. }
                | Draw::Fill {
                    color: Color::ChipBg(_) | Color::Footer,
                    ..
                } => assert_eq!(depth, 1),
                Draw::Alpha(_) => panic!("provider paint must preserve incoming alpha"),
                _ => {}
            }
        }
        assert_eq!((clips, depth), (1, 0));
        let (off, _, _) = panel_scene(&p, true);
        assert!(off.draw.is_empty());
        assert!(off.targets.is_empty());
        assert_eq!(off.accessibility.len(), scene.accessibility.len());
        assert_eq!(off.charts.len(), 1);
        assert_eq!(
            off.charts[0].provider_summary,
            scene.charts[0].provider_summary
        );
    }

    #[test]
    fn provider_deadline_is_exported_to_existing_visible_time_scanner_input() {
        let now = crate::dashboard_fixtures::clock().utc();
        let mut p = projection(Provider::Claude, now);
        let deadline = now + chrono::Duration::seconds(1);
        p.next_change = Some(deadline);
        let (scene, _, _) = panel_scene(&p, false);
        assert_eq!(
            scene.charts[0].series[0].future_resets.first(),
            Some(&deadline)
        );
        p.next_change = Some(now);
        let (scene, _, _) = panel_scene(&p, false);
        assert!(!scene.charts[0].series[0].future_resets.contains(&now));
    }

    #[test]
    fn current_quantity_is_visible_measured_in_both_languages_and_unknown_explicit() {
        let mut p = projection(Provider::Claude, crate::dashboard_fixtures::clock().utc());
        for language in [crate::i18n::UiLanguage::Ko, crate::i18n::UiLanguage::En] {
            crate::i18n::with_language(language, || {
                let expected = current_quantity(&p);
                assert!(expected.contains("0.7") || expected.contains("0.6"));
                let (scene, bounds, _) = panel_scene(&p, false);
                let painted = scene
                    .draw
                    .iter()
                    .find_map(|d| match d {
                        Draw::Text { rect, measure, .. } if measure.text == expected => {
                            Some((rect, measure))
                        }
                        _ => None,
                    })
                    .expect("current quantity must be visible, not tooltip-only");
                assert!(painted.1.wrap);
                assert!(
                    ((painted.0.bottom - painted.0.top) - Metrics::default().height(painted.1))
                        .abs()
                        < 0.001
                );
                for d in &scene.draw {
                    if let Draw::Text { rect, .. } = d {
                        assert!(rect.bottom <= bounds.bottom);
                    }
                }
            });
        }
        p.current.remaining.value = None;
        p.current.remaining.known = 0;
        crate::i18n::with_language(crate::i18n::UiLanguage::En, || {
            assert_eq!(
                current_quantity(&p),
                "Remaining unknown · measured 0/1 accounts"
            );
            let (scene, _, _) = panel_scene(&p, false);
            assert!(scene.draw.iter().any(
                |d| matches!(d, Draw::Text { measure, .. } if measure.text == current_quantity(&p))
            ));
        });
    }

    #[test]
    fn unknown_provider_has_no_fabricated_total_or_percent_tooltip() {
        let clock = crate::dashboard_fixtures::clock();
        let empty = p::project(Provider::Claude, &[], clock.utc()).unwrap();
        assert!(verdict(&empty).contains("측정") || verdict(&empty).contains("Measuring"));
        assert_eq!(quantity(empty.current.remaining.value), "—");
    }
}
