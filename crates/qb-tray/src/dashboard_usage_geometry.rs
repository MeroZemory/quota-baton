//! Dashboard usage tiles over the shared measured scene.
use super::*;

pub(super) fn usage(b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, card: Rect) {
    b.card(card);
    b.text(
        rect(card.left + 16., card.top + 14., 120., 24.),
        "오늘 사용량",
        14.,
        true,
        Color::Text,
        false,
    );
    let summary = format!(
        "남은 양 · 00:00–24:00 · {}",
        v.clock.day_label(v.clock.utc())
    );
    let summary_width = b.measured_width(&summary, 12., false) + 4.;
    b.text(
        rect(card.left + 144., card.top + 16., summary_width, 20.),
        summary,
        12.,
        false,
        Color::Secondary,
        false,
    );
    let legend = "━ 5시간 (Free는 30일)  ┄ 주간  ⋮ 초기화  ▧ 크레딧으로 처리";
    let legend_width = b.measured_width(legend, 12., false) + 4.;
    let legend_left = card.right - 16. - legend_width;
    let legend_wrap = legend_left < card.left + 144. + summary_width + 16.;
    b.text(
        rect(
            legend_left,
            card.top + if legend_wrap { 40. } else { 16. },
            legend_width,
            20.,
        ),
        legend,
        12.,
        false,
        Color::Secondary,
        false,
    );
    let header_extra = if legend_wrap { 24. } else { 0. };
    let viewport = rect(
        card.left + 12.,
        card.top + 52. + header_extra,
        width(card) - 24.,
        height(card) - 64. - header_extra,
    );
    let columns = if width(card) < 1140. { 2 } else { 3 };
    let tw = (width(viewport) - (columns - 1) as f32 * 12.) / columns as f32;
    let rows = v.usage.len().div_ceil(columns);
    let content = rows as f32 * 300.;
    let (old, offset) = b.begin_region(ScrollRegion::Usage, viewport, content, s.usage_offset);
    let available_nodes = history::SCENE_NODE_BUDGET.saturating_sub(b.scene.accessibility.len());
    for (i, tile) in v.usage.iter().enumerate() {
        let r = rect(
            viewport.left + (i % columns) as f32 * (tw + 12.),
            viewport.top + (i / columns) as f32 * 300. - offset,
            tw,
            288.,
        );
        let table = history::project(
            tile,
            history::row_limit(tile.series.len(), v.usage.len(), available_nodes),
        );
        let draw_start = b.scene.draw.len();
        usage_tile(b, v, tile, r, &table);
        // Retain bounded offscreen semantics, but do not paint invisible history lines.
        if intersection(r, b.clip).is_none() {
            b.scene.draw.truncate(draw_start);
        }
    }
    if v.usage.is_empty() {
        b.text(
            rect(
                viewport.left + 24.,
                viewport.top + 24.,
                width(viewport) - 48.,
                40.,
            ),
            "사용량 확인 전",
            14.,
            false,
            Color::Secondary,
            false,
        );
    }
    b.end_region(old);
}
fn usage_tile(
    b: &mut Builder<'_>,
    v: &m::DashboardView,
    t: &m::UsageTile,
    r: Rect,
    table: &history::Table,
) {
    let tile_clip = intersection(r, b.clip);
    if let Some(clip) = tile_clip {
        b.scene.draw.push(Draw::Clip(clip));
    }
    b.fill(r, Color::Surface, 6.);
    b.fill(rect(r.left, r.top, width(r), 60.), Color::Footer, 6.);
    b.fill(rect(r.left, r.top + 30., width(r), 30.), Color::Footer, 0.);
    b.stroke(r, Color::GroupBorder, 1., 6.);
    b.line(
        [r.left, r.top + 60.],
        [r.right, r.top + 60.],
        Color::Border,
        1.,
        None,
    );
    let provider = v2::provider_label(&t.provider);
    let provider_width = b.measured_width(provider, 12., false);
    let plan_width = b.measured_width(&t.plan, 12., false);
    let chip_width = if t.chip.is_empty() {
        0.
    } else {
        b.measured_width(&t.chip, 12., true) + 16.
    };
    let name_left = r.left + 12. + provider_width + 6.;
    let content_right = r.right - 12. - chip_width - if chip_width > 0. { 8. } else { 0. };
    let name_width = b
        .measured_width(&t.name, 13., true)
        .min((content_right - name_left - plan_width - 6.).max(0.));
    b.text(
        rect(r.left + 12., r.top + 12., provider_width, 18.),
        provider,
        12.,
        false,
        Color::Secondary,
        false,
    );
    b.text(
        rect(name_left, r.top + 10., name_width, 24.),
        &t.name,
        13.,
        true,
        Color::Text,
        false,
    );
    b.text(
        rect(name_left + name_width + 6., r.top + 12., plan_width, 18.),
        &t.plan,
        12.,
        false,
        Color::Secondary,
        false,
    );
    chip(
        b,
        &t.chip,
        t.tone,
        rect(r.right - 12. - chip_width, r.top + 10., chip_width, 20.),
        10.,
    );
    // Windows arrive shortest first from the model, independently of observed history.
    if t.unconfirmed {
        let since = t
            .last_observed
            .map(|at| format!("{} 이후 확인하지 못함 · ", v.clock.time(Some(at))))
            .unwrap_or_default();
        b.text(
            rect(r.left + 12., r.top + 36., width(r) - 24., 16.),
            format!("{since}다시 로그인 필요"),
            12.,
            false,
            Color::Bad,
            false,
        );
    } else if t.windows.is_empty() {
        b.text(
            rect(r.left + 12., r.top + 36., width(r) - 24., 16.),
            "사용량 확인 전",
            12.,
            false,
            Color::Secondary,
            false,
        );
    } else {
        let mut left = r.left + 12.;
        for window in &t.windows {
            let label = format!("{} ", window.label);
            let label_width = b.measured_width(&label, 11., false);
            let value_width = b.measured_width(&window.remaining, 11., true);
            let reset = format!(" · {}", window.reset);
            let reset_width = b.measured_width(&reset, 11., false);
            b.text(
                rect(left, r.top + 36., label_width, 16.),
                label,
                11.,
                false,
                Color::Secondary,
                false,
            );
            left += label_width;
            b.text(
                rect(left, r.top + 36., value_width, 16.),
                &window.remaining,
                11.,
                true,
                Color::Text,
                false,
            );
            left += value_width;
            b.text(
                rect(left, r.top + 36., reset_width, 16.),
                reset,
                11.,
                false,
                Color::Secondary,
                false,
            );
            left += reset_width + 16.;
        }
    }
    let future_resets: BTreeSet<_> = t
        .series
        .iter()
        .flat_map(|series| series.future_resets.iter().copied())
        .collect();
    let annotation_space = if future_resets.is_empty() { 0. } else { 20. };
    let notice_space = if t.truncated || !table.complete {
        20.
    } else {
        0.
    };
    let mut plot = rect(
        r.left + 40.,
        r.top + 86. + annotation_space,
        width(r) - 64.,
        170. - annotation_space - notice_space,
    );
    let from = v.clock.day_start;
    let to = v.clock.day_end;
    let total = to.signed_duration_since(from).num_milliseconds().max(1) as f64;
    // Keep close reset captions in separate rows while retaining their own x anchor.
    let mut caption_rights: Vec<f32> = Vec::new();
    let mut captions = Vec::new();
    let mut overflow = Vec::new();
    for at in future_resets {
        let label = format!("{} 초기화", v.clock.time(Some(at)));
        let w = (b.measured_width(&label, 11., false) + 4.).min(width(plot));
        let marker = plot.left
            + (at.signed_duration_since(from).num_milliseconds() as f64 / total).clamp(0., 1.)
                as f32
                * width(plot);
        let left = (marker - w / 2.).clamp(plot.left, plot.right - w);
        let lane = caption_rights
            .iter()
            .position(|right| *right + 4. <= left)
            .unwrap_or(caption_rights.len());
        if lane >= 2 {
            overflow.push(label);
            continue;
        }
        if lane == caption_rights.len() {
            caption_rights.push(left + w);
        } else {
            caption_rights[lane] = left + w;
        }
        captions.push((label, left, w, lane));
    }
    plot.top += caption_rights.len().saturating_sub(1) as f32 * 20.;
    if !overflow.is_empty() {
        // One bounded overflow row; all dashed markers remain in the plot.
        plot.top += 20.;
        let note = format!("초기화 {}개 더", overflow.len());
        let bounds = rect(plot.left, r.top + 104., width(plot), 16.);
        b.text(bounds, note.clone(), 11., false, Color::Secondary, false);
        b.semantic(
            format!("usage:{}:reset-overflow", t.id),
            None,
            Role::Text,
            format!("{note}: {}", overflow.join(" · ")),
            bounds,
            None,
        );
    }
    for (label, left, w, lane) in captions {
        b.text(
            rect(left, r.top + 64. + lane as f32 * 20., w, 16.),
            label,
            11.,
            false,
            Color::Secondary,
            false,
        );
    }
    let x = |at: DateTime<Utc>| {
        plot.left
            + (at.signed_duration_since(from).num_milliseconds() as f64 / total).clamp(0., 1.)
                as f32
                * width(plot)
    };
    let y = |pct: f64| plot.bottom - (pct.clamp(0., 100.) as f32 / 100.) * height(plot);
    if t.unconfirmed
        && let Some(last) = t.last_observed.filter(|last| *last < v.clock.utc())
    {
        hatch(
            b,
            rect(x(last), plot.top, x(v.clock.utc()) - x(last), height(plot)),
        );
    }
    for (start, end) in &t.credits_spans {
        b.fill(
            rect(x(*start), plot.top, x(*end) - x(*start), height(plot)),
            Color::ChipBg(v2::Tone::Warn),
            0.,
        );
    }
    for p in [0, 50, 100] {
        let yy = y(p as f64);
        b.line([plot.left, yy], [plot.right, yy], Color::Border, 1., None);
        b.text(
            rect(r.left + 4., yy - 7., 32., 16.),
            format!("{p}%"),
            11.,
            false,
            Color::Secondary,
            false,
        );
    }
    for hour in [0, 6, 12, 18, 24] {
        let xx = plot.left + width(plot) * hour as f32 / 24.;
        b.text(
            rect(xx - 10., plot.bottom + 8., 36., 16.),
            format!("{hour}:00"),
            11.,
            false,
            Color::Secondary,
            false,
        );
    }
    let nowx = x(v.clock.utc());
    b.line([nowx, plot.top], [nowx, plot.bottom], Color::Text, 1., None);
    // Reserve a separate caption row below future-reset text and above the plot.
    let now_width = b.measured_width("지금", 11., true) + 4.;
    b.text(
        rect(
            (nowx - now_width / 2.).clamp(plot.left, plot.right - now_width),
            plot.top - 22.,
            now_width,
            16.,
        ),
        "지금",
        11.,
        true,
        Color::Text,
        false,
    );
    // No line or label may extrapolate into the unconfirmed span.
    let observed = |at: DateTime<Utc>| {
        at >= from
            && at <= v.clock.utc()
            && (!t.unconfirmed || t.last_observed.is_some_and(|last| at <= last))
    };
    let has_observation = t.series.iter().any(|series| {
        series
            .points
            .iter()
            .any(|(at, pct)| observed(*at) && pct.is_some())
    });
    let mut endpoints = Vec::new();
    for (series_index, series) in t.series.iter().enumerate() {
        let weekly = series.name == "주간" || series.name == "7d";
        let color = if weekly {
            Color::Series7d
        } else if t.paused {
            Color::BarPaused
        } else {
            Color::BarOk
        };
        let dash = if weekly { Some([5., 3.]) } else { None };
        let mut previous: Option<(DateTime<Utc>, f64)> = None;
        let resets: BTreeSet<_> = series.resets.iter().copied().collect();
        for (at, pct) in &series.points {
            if !observed(*at) {
                continue;
            }
            if let Some(pct) = pct {
                if let Some((pa, pp)) = previous {
                    if resets.contains(at) {
                        b.line([x(pa), y(pp)], [x(*at), y(pp)], color, 2., dash);
                        b.line([x(*at), y(pp)], [x(*at), y(*pct)], color, 2., dash);
                    } else {
                        b.line([x(pa), y(pp)], [x(*at), y(*pct)], color, 2., dash);
                    }
                }
                previous = Some((*at, *pct));
            } else {
                previous = None;
            }
        }
        for reset in &series.resets {
            b.line(
                [x(*reset), plot.top],
                [x(*reset), plot.bottom],
                Color::Secondary,
                1.,
                Some([1., 3.]),
            );
        }
        for reset in &series.future_resets {
            b.line(
                [x(*reset), plot.top],
                [x(*reset), plot.bottom],
                Color::Secondary,
                1.,
                Some([1., 3.]),
            );
        }
        if let Some((at, Some(pct))) = series.points.iter().rev().find(|(at, _)| observed(*at))
            && !t.unconfirmed
        {
            endpoints.push((
                x(*at),
                y(*pct),
                pct.round().clamp(0., 100.) as u8,
                color,
                series_index == 0,
            ));
        }
    }
    let mut labels: Vec<(f32, f32, u8, Color, bool)> = Vec::new();
    for endpoint in endpoints {
        if let Some(old) = labels.iter_mut().find(|old| old.2 == endpoint.2) {
            old.0 = old.0.max(endpoint.0);
            old.3 = Color::Text;
        } else {
            labels.push(endpoint);
        }
    }
    labels.sort_by(|a, b| a.1.total_cmp(&b.1));
    let preferred: Vec<_> = labels.iter().map(|label| label.1 - 6.).collect();
    let positions = end_label_positions(&preferred, plot);
    for ((xx, _, pct, color, bold), yy) in labels.into_iter().zip(positions) {
        let text = format!("{pct}%");
        let w = b.measured_width(&text, 11., bold) + 4.;
        backed_text(
            b,
            rect((xx + 6.).clamp(plot.left, plot.right - w), yy, w, 16.),
            &text,
            bold,
            color,
        );
    }
    // Keep the specified right-of-now anchors even late in the day.
    if let Some(clip) = intersection(plot, b.clip) {
        b.scene.draw.push(Draw::Clip(clip));
        if !has_observation {
            let text = "오늘은 아직 관측한 값이 없습니다";
            let w = b.measured_width(text, 12., false) + 4.;
            b.fill(rect(nowx + 12., y(50.) - 8., w, 16.), Color::Surface, 0.);
            b.text(
                rect(nowx + 12., y(50.) - 8., w, 16.),
                text,
                12.,
                false,
                Color::Secondary,
                false,
            );
        }
        if t.unconfirmed {
            let text = t
                .last_observed
                .map(|at| format!("{} 이후 확인 못 함", v.clock.time(Some(at))))
                .unwrap_or_else(|| "이후 확인 못 함".into());
            let w = b.measured_width(&text, 11., false) + 4.;
            backed_text(
                b,
                rect(nowx + 8., plot.top + 2., w, 16.),
                &text,
                false,
                Color::Bad,
            );
        }
        b.scene.draw.push(Draw::Unclip);
    }
    if t.unconfirmed || t.truncated || !table.complete {
        let note = if table.rows.len() < table.total_rows {
            format!(
                "접근성 표: 대표 {}/{}행 · 일부만 표시{}",
                table.rows.len(),
                table.total_rows,
                if t.unconfirmed {
                    " · 이후 확인 못 함"
                } else {
                    ""
                }
            )
        } else {
            match (t.unconfirmed, t.truncated) {
                (true, true) => "이후 확인 못 함 · 일부 관측만 표시",
                (true, false) => "이후 확인 못 함",
                _ => "일부 관측만 표시",
            }
            .into()
        };
        b.semantic(
            format!("chart:{}:notice", t.id),
            None,
            Role::Text,
            note.clone(),
            rect(r.left + 12., r.bottom - 24., width(r) - 24., 18.),
            None,
        );
        if t.truncated || !table.complete {
            b.text(
                rect(r.left + 12., r.bottom - 20., width(r) - 24., 18.),
                note,
                11.,
                false,
                Color::Secondary,
                false,
            );
        }
    }
    if let Some(hit) = intersection(plot, b.clip) {
        b.scene.targets.push(HitTarget {
            rect: hit,
            target: Target::Chart(t.id.clone()),
            enabled: true,
            tooltip: None,
        });
    }
    let chartkey = format!("chart:{}", t.id);
    b.semantic(
        chartkey.clone(),
        None,
        Role::Chart,
        format!("{} 오늘 남은 양", t.name),
        plot,
        Some(Target::Chart(t.id.clone())),
    );
    let tablekey = format!("{chartkey}:table");
    let index = b.semantic(
        tablekey.clone(),
        Some(chartkey.clone()),
        Role::Table,
        if table.complete {
            "시각과 남은 양".into()
        } else {
            format!(
                "시각과 남은 양 · 일부 기록 · 대표 {}/{}행",
                table.rows.len(),
                table.total_rows
            )
        },
        plot,
        None,
    );
    b.scene.accessibility[index].hidden = true;
    b.scene.accessibility[index].value = Some(
        if table.complete {
            "complete"
        } else {
            "incomplete"
        }
        .into(),
    );
    let headers = std::iter::once("시각".to_string())
        .chain(t.series.iter().map(|s| format!("{} 남은 양", s.name)));
    for (i, name) in headers.enumerate() {
        let idx = b.semantic(
            format!("{tablekey}:header:{i}"),
            Some(tablekey.clone()),
            Role::ColumnHeader,
            name,
            plot,
            None,
        );
        b.scene.accessibility[idx].hidden = true;
    }
    for row in &table.rows {
        let at = row.at;
        let values: Vec<_> = t
            .series
            .iter()
            .zip(&row.values)
            .map(|(series, value)| {
                format!(
                    "{} {} 남음",
                    series.name,
                    value
                        .map(|n| format!("{}%", n.round() as u8))
                        .unwrap_or_else(m::unknown)
                )
            })
            .collect();
        let rowkey = format!(
            "{tablekey}:{}:{}",
            at.timestamp(),
            at.timestamp_subsec_nanos()
        );
        let idx = b.semantic(
            rowkey.clone(),
            Some(tablekey.clone()),
            Role::Row,
            format!("{} · {}", v.clock.stamp(Some(at)), values.join(" · ")),
            plot,
            None,
        );
        b.scene.accessibility[idx].hidden = true;
        for (i, value) in std::iter::once(v.clock.stamp(Some(at)))
            .chain(values)
            .enumerate()
        {
            let idx = b.semantic(
                format!("{rowkey}:cell:{i}"),
                Some(rowkey.clone()),
                Role::Text,
                value,
                plot,
                None,
            );
            b.scene.accessibility[idx].hidden = true;
        }
    }
    b.scene.charts.push(ChartData {
        id: t.id.clone(),
        rect: plot,
        from,
        to,
        now: v.clock.utc(),
        series: t.series.clone(),
        unconfirmed: t.unconfirmed,
        offset: *v.clock.now.offset(),
    });
    if tile_clip.is_some() {
        b.scene.draw.push(Draw::Unclip);
    }
}

// §7: symmetric separation projected into the plot's feasible bounds.
fn end_label_positions(preferred: &[f32], plot: Rect) -> Vec<f32> {
    // Keep at least §7's 14 DIP separation and room for each 16 DIP backing.
    const GAP: f32 = 16.;
    let lower = plot.top + 6.;
    let upper = plot.bottom - 18.;
    if preferred.is_empty() || upper < lower {
        return Vec::new();
    }
    // Omit excess labels rather than overlap backings or leave the plot.
    let capacity = (((upper - lower) / GAP).floor() as usize).saturating_add(1);
    let preferred = &preferred[..preferred.len().min(capacity)];
    let mut positions = preferred.to_vec();
    let upper_mean = upper - (positions.len() - 1) as f32 * GAP;
    // Pool colliding neighbours around their original mean (also handles >2 windows).
    let mut blocks: Vec<(usize, usize, f32)> = Vec::new();
    for (i, &y) in preferred.iter().enumerate() {
        blocks.push((i, 1, y - i as f32 * GAP));
        while blocks.len() > 1 {
            let n = blocks.len();
            if blocks[n - 2].2 <= blocks[n - 1].2 {
                break;
            }
            let (start_b, count_b, mean_b) = blocks.pop().unwrap();
            let (start_a, count_a, mean_a) = blocks.pop().unwrap();
            debug_assert_eq!(start_a + count_a, start_b);
            let count = count_a + count_b;
            blocks.push((
                start_a,
                count,
                (mean_a * count_a as f32 + mean_b * count_b as f32) / count as f32,
            ));
        }
    }
    for (start, count, mean) in blocks {
        // A common interval preserves monotone means, hence GAP separation.
        let mean = mean.clamp(lower, upper_mean);
        for (i, y) in positions.iter_mut().enumerate().skip(start).take(count) {
            *y = mean + i as f32 * GAP;
        }
    }
    positions
}
fn backed_text(b: &mut Builder<'_>, r: Rect, text: &str, bold: bool, color: Color) {
    b.fill(r, Color::Surface, 0.);
    b.text(r, text, 11., bold, color, false);
}
fn hatch(b: &mut Builder<'_>, band: Rect) {
    if width(band) <= 0. {
        return;
    }
    // Analytically clipped 45-degree segments, six DIP of perpendicular pitch.
    let mut offset: f32 = 0.;
    while offset < width(band) + height(band) {
        let start_x = offset.min(width(band));
        let start_y = (offset - width(band)).max(0.);
        let end_y = offset.min(height(band));
        let end_x = (offset - height(band)).max(0.);
        b.line(
            [band.left + start_x, band.top + start_y],
            [band.left + end_x, band.top + end_y],
            Color::Track,
            1.,
            None,
        );
        offset += 6. * std::f32::consts::SQRT_2;
    }
}

#[cfg(test)]
#[path = "dashboard_usage_v2_tests.rs"]
mod tests;
