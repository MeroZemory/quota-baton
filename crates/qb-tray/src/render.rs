//! Shared v2 Direct2D/DirectWrite renderer adapter; no HWND, GDI, I/O or clock.
use crate::{
    dashboard_model::DashboardView,
    geometry::*,
    state::DashboardState,
    v2_colors::{Palette, Rgb},
    v2_render, v2_text,
};
use windows::{
    Win32::Graphics::{
        Direct2D::{Common::D2D_RECT_F, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, ID2D1RenderTarget},
        DirectWrite::{DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD},
    },
    core::Result,
};

pub struct Renderer {
    shared: v2_render::Renderer,
}
#[derive(Default)]
pub struct Interaction {
    pub hover: Option<Target>,
    pub pressed: Option<Target>,
    pub focus: Option<Target>,
    pub keyboard_focus: bool,
    pub pointer: Option<[f32; 2]>,
    pub tooltip_ready: bool,
}
fn bounds(r: crate::Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}
fn style(m: &Measure) -> v2_text::Style {
    v2_text::Style {
        face: match m.face {
            Face::Text => v2_text::Face::Text,
            Face::Mono => v2_text::Face::Mono,
            Face::Icon => v2_text::Face::Icon,
        },
        size: m.size,
        weight: if m.bold {
            DWRITE_FONT_WEIGHT_SEMI_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        },
        line_height: m.line_height,
        align: match m.align {
            TextAlign::Leading => v2_text::Align::Leading,
            TextAlign::Center => v2_text::Align::Center,
        },
        wrap: if m.wrap {
            v2_text::Wrap::KeepAll
        } else {
            v2_text::Wrap::SingleLine
        },
    }
}
pub fn color(p: Palette, c: Color) -> Rgb {
    match c {
        Color::Page => Rgb::hex(if p.dark { 0x1b1b1b } else { 0xf3f3f3 }),
        Color::Surface => p.surface,
        Color::Footer => p.footer,
        Color::Border => Rgb::hex(if p.dark { 0x333333 } else { 0xe3e3e3 }),
        Color::GroupBorder => Rgb::hex(if p.dark { 0x3d3d3d } else { 0xd6d9dd }),
        Color::Text => p.text,
        Color::Secondary => p.secondary,
        Color::Accent => p.accent,
        Color::OnAccent => p.on_accent,
        Color::Warn => p.warning,
        Color::Bad => p.bad,
        Color::Select => p.chips[0].0,
        Color::Track => p.track,
        Color::BarOk => p.bars[0],
        Color::BarLow => p.bars[1],
        Color::BarCritical => p.bars[2],
        Color::BarPaused => p.bars[3],
        Color::PipOutline => Rgb::hex(0x9db8d6),
        Color::Verdict(tone) => p.tone(tone),
        Color::Series7d => Rgb::hex(if p.dark { 0xb5b5b5 } else { 0x50555b }),
        Color::ChipBg(tone) => p.chip(tone).0,
        Color::ChipFg(tone) => p.chip(tone).1,
    }
}
impl Renderer {
    pub fn new(dark: bool) -> Result<Self> {
        Ok(Self {
            shared: v2_render::Renderer::new(dark)?,
        })
    }
    fn wrapped(&self, measure: &Measure) -> Result<String> {
        if !measure.wrap {
            return Ok(measure.text.clone());
        }
        let mut out = String::new();
        let mut probe = style(measure);
        probe.wrap = v2_text::Wrap::SingleLine;
        for (i, line) in measure.text.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for token in line.split_inclusive(' ') {
                let advance = self
                    .shared
                    .dashboard_text()
                    .layout(
                        token,
                        D2D_RECT_F {
                            left: 0.,
                            top: 0.,
                            right: 65536.,
                            bottom: measure.line_height,
                        },
                        probe,
                    )?
                    .metrics
                    .natural_width;
                if advance <= measure.width {
                    out.push_str(token);
                    continue;
                }
                let mut chunk = String::new();
                for c in token.chars() {
                    let next = format!("{chunk}{c}");
                    let width = self
                        .shared
                        .dashboard_text()
                        .layout(
                            &next,
                            D2D_RECT_F {
                                left: 0.,
                                top: 0.,
                                right: 65536.,
                                bottom: measure.line_height,
                            },
                            probe,
                        )?
                        .metrics
                        .natural_width;
                    if !chunk.is_empty() && width > measure.width {
                        out.push_str(&chunk);
                        out.push('\n');
                        chunk.clear();
                    }
                    chunk.push(c);
                }
                out.push_str(&chunk);
            }
        }
        Ok(out)
    }
    pub fn scene(
        &self,
        view: &DashboardView,
        state: &DashboardState,
        size: [f32; 2],
    ) -> Result<Scene> {
        let mut metrics = Metrics::default();
        // Probe collects exact width/font requests, then build with measured line
        // heights. Second probe covers content newly revealed by offset clamping.
        for _ in 0..3 {
            let scene = Scene::build(view, state, size, &metrics);
            let mut added = false;
            for measure in &scene.measurements {
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    metrics.heights.entry(measure.key())
                {
                    let layout = self.shared.dashboard_text().layout(
                        &self.wrapped(measure)?,
                        D2D_RECT_F {
                            left: 0.,
                            top: 0.,
                            right: measure.width.max(1.),
                            bottom: 65536.,
                        },
                        style(measure),
                    )?;
                    entry.insert(layout.metrics.required_height);
                    metrics
                        .widths
                        .insert(measure.key(), layout.metrics.natural_width);
                    added = true;
                }
            }
            if !added {
                return Ok(scene);
            }
        }
        Ok(Scene::build(view, state, size, &metrics))
    }
    pub fn paint(
        &self,
        target: &ID2D1RenderTarget,
        scene: &Scene,
        interaction: &Interaction,
    ) -> Result<()> {
        let p = self.shared.dashboard_palette();
        let mut clips = 0usize;
        let mut alpha = 1.;
        unsafe {
            target.BeginDraw();
        }
        let result = (|| -> Result<()> {
            unsafe {
                target.PushAxisAlignedClip(&bounds(scene.rect), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            }
            clips += 1;
            for draw in &scene.draw {
                match draw {
                    Draw::Fill {
                        rect,
                        color: c,
                        radius,
                    } => self
                        .shared
                        .fill(target, *rect, color(p, *c), alpha, *radius)?,
                    Draw::Stroke {
                        rect,
                        color: c,
                        width,
                        radius,
                    } => self
                        .shared
                        .stroke(target, *rect, color(p, *c), *width, *radius, alpha)?,
                    Draw::Text {
                        rect,
                        measure,
                        color: c,
                    } => self.shared.text(
                        target,
                        *rect,
                        &self.wrapped(measure)?,
                        style(measure),
                        color(p, *c),
                        alpha,
                    )?,
                    Draw::Line {
                        from,
                        to,
                        color: c,
                        width,
                        dash,
                    } => self.line(target, *from, *to, color(p, *c), *width, *dash, alpha)?,
                    Draw::Clip(rect) => {
                        unsafe {
                            target.PushAxisAlignedClip(
                                &bounds(*rect),
                                D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                            );
                        }
                        clips += 1;
                    }
                    Draw::Unclip => {
                        if clips > 1 {
                            unsafe {
                                target.PopAxisAlignedClip();
                            }
                            clips -= 1;
                        }
                    }
                    Draw::Alpha(value) => alpha = *value,
                }
            }
            for hit in &scene.targets {
                if !hit.enabled || hit.target == Target::DetailSurface {
                    continue;
                }
                if interaction.hover.as_ref() == Some(&hit.target)
                    || interaction.pressed.as_ref() == Some(&hit.target)
                {
                    self.shared.fill(
                        target,
                        hit.rect,
                        if p.dark {
                            Rgb::hex(0xffffff)
                        } else {
                            Rgb::hex(0x000000)
                        },
                        if interaction.pressed.as_ref() == Some(&hit.target) {
                            0.06
                        } else {
                            0.03
                        },
                        4.,
                    )?;
                }
                if interaction.keyboard_focus && interaction.focus.as_ref() == Some(&hit.target) {
                    self.shared
                        .stroke(target, hit.rect, p.focus_outer, 2., 4., 1.)?;
                    let inner = rect(
                        hit.rect.left + 1.,
                        hit.rect.top + 1.,
                        (width(hit.rect) - 2.).max(0.),
                        (height(hit.rect) - 2.).max(0.),
                    );
                    self.shared
                        .stroke(target, inner, p.focus_inner, 1., 3., 1.)?;
                }
            }
            if let (Some(Target::Chart(id)), Some(pointer)) =
                (&interaction.hover, interaction.pointer)
                && let Some(tip) = scene.chart_tooltip(id, pointer[0])
                && let Some(chart) = scene.charts.iter().find(|c| &c.id == id)
            {
                self.line(
                    target,
                    [tip.position[0], chart.rect.top],
                    [tip.position[0], chart.rect.bottom],
                    p.secondary,
                    1.,
                    Some([1., 3.]),
                    1.,
                )?;
                let w = 320f32.min(width(scene.rect) - 16.).max(1.);
                let x = (pointer[0] + 10.).clamp(
                    scene.rect.left + 8.,
                    (scene.rect.right - w - 8.).max(scene.rect.left + 8.),
                );
                let y = (pointer[1] - 44.).clamp(
                    scene.rect.top + 8.,
                    (scene.rect.bottom - 48.).max(scene.rect.top + 8.),
                );
                let r = rect(x, y, w, 40.);
                self.shared.fill(target, r, p.popup, 1., 6.)?;
                self.shared.stroke(target, r, p.popup_border, 1., 6., 1.)?;
                let m = Measure {
                    text: tip.text,
                    size: 12.,
                    line_height: 16.,
                    width: w - 16.,
                    bold: false,
                    wrap: true,
                    face: Face::Text,
                    align: TextAlign::Leading,
                };
                self.shared.text(
                    target,
                    rect(x + 8., y + 4., w - 16., 32.),
                    &m.text,
                    style(&m),
                    p.text,
                    1.,
                )?;
            }
            if interaction.tooltip_ready
                && let Some(pointer) = interaction.pointer
                && let Some(text) = scene.tooltip_at(pointer[0], pointer[1])
            {
                self.tooltip(target, scene, text, pointer)?;
            }
            Ok(())
        })();
        while clips > 0 {
            unsafe {
                target.PopAxisAlignedClip();
            }
            clips -= 1;
        }
        let ended = unsafe { target.EndDraw(None, None) };
        result.and(ended)
    }
    fn tooltip(
        &self,
        target: &ID2D1RenderTarget,
        scene: &Scene,
        text: &str,
        pointer: [f32; 2],
    ) -> Result<()> {
        let p = self.shared.dashboard_palette();
        let w = 288f32.min(width(scene.rect) - 32.).max(1.);
        let measure = Measure {
            text: text.into(),
            size: 12.,
            line_height: 16.,
            width: w,
            bold: false,
            wrap: true,
            face: Face::Text,
            align: TextAlign::Leading,
        };
        let h = self
            .shared
            .dashboard_text()
            .layout(
                text,
                D2D_RECT_F {
                    left: 0.,
                    top: 0.,
                    right: w,
                    bottom: 4096.,
                },
                style(&measure),
            )?
            .metrics
            .required_height
            + 16.;
        let x = (pointer[0] + 12.).clamp(
            scene.rect.left + 8.,
            (scene.rect.right - w - 28.).max(scene.rect.left + 8.),
        );
        let y = if pointer[1] + 14. + h < scene.rect.bottom - 8. {
            pointer[1] + 14.
        } else {
            (pointer[1] - h - 14.).max(scene.rect.top + 8.)
        };
        let r = rect(x, y, w + 20., h);
        self.shared.fill(target, r, p.popup, 1., 6.)?;
        self.shared.stroke(target, r, p.popup_border, 1., 6., 1.)?;
        self.shared.text(
            target,
            rect(x + 10., y + 8., w, h - 16.),
            text,
            style(&measure),
            p.text,
            1.,
        )
    }
    // D2D drawing primitive: explicit endpoints and stroke parameters avoid hidden state.
    #[allow(clippy::too_many_arguments)]
    fn line(
        &self,
        target: &ID2D1RenderTarget,
        from: [f32; 2],
        to: [f32; 2],
        c: Rgb,
        width: f32,
        dash: Option<[f32; 2]>,
        alpha: f32,
    ) -> Result<()> {
        if let Some([on, off]) = dash {
            let dx = to[0] - from[0];
            let dy = to[1] - from[1];
            let length = (dx * dx + dy * dy).sqrt();
            if length <= 0. {
                return Ok(());
            }
            let stride = (on + off).max(1.);
            let count = (length / stride).ceil().min(8192.) as usize;
            for i in 0..count {
                let a = i as f32 * stride;
                let z = (a + on.max(1.)).min(length);
                if a >= length {
                    break;
                }
                self.shared.dashboard_line(
                    target,
                    [from[0] + dx * a / length, from[1] + dy * a / length],
                    [from[0] + dx * z / length, from[1] + dy * z / length],
                    c,
                    width,
                    alpha,
                )?;
            }
            Ok(())
        } else {
            self.shared
                .dashboard_line(target, from, to, c, width, alpha)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn r10_relogin_action_keeps_its_complete_directwrite_label_at_boundary_widths() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let mut state = DashboardState::default();
        let mut view = crate::dashboard_model::project(
            &crate::dashboard_fixtures::snapshot(),
            &mut state,
            &crate::dashboard_fixtures::clock(),
        );
        view.groups.truncate(1);
        view.groups[0].name = "아주 긴 합성 계정 이름을 가진 재로그인 대상".into();
        view.groups[0].relogin = true;
        view.groups[0].relogin_reason = "다시 로그인 필요".into();
        for width in [1024., 1179., 1180., 1320.] {
            let scene = renderer.scene(&view, &state, [width, 960.])?;
            let button = scene
                .targets
                .iter()
                .find(|t| t.target == Target::OpenTrayAccount(view.groups[0].id.clone()))
                .unwrap()
                .rect;
            let (rect, measure) = scene
                .draw
                .iter()
                .find_map(|draw| match draw {
                    Draw::Text { rect, measure, .. }
                        if measure.text == "트레이에서 다시 로그인"
                            && rect.left >= button.left
                            && rect.right <= button.right
                            && rect.top >= button.top
                            && rect.bottom <= button.bottom =>
                    {
                        Some((*rect, measure))
                    }
                    _ => None,
                })
                .unwrap();
            let layout = renderer.shared.dashboard_text().layout(
                &measure.text,
                bounds(rect),
                style(measure),
            )?;
            assert_eq!(layout.metrics.visible_lines, 1);
            assert!(
                !layout.metrics.trimmed,
                "relogin action must not truncate at {width} DIP"
            );
        }
        Ok(())
    }
    #[test]
    fn r10_single_line_account_names_fit_their_actual_directwrite_line_boxes() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let mut state = DashboardState::default();
        let mut view = crate::dashboard_model::project(
            &crate::dashboard_fixtures::snapshot(),
            &mut state,
            &crate::dashboard_fixtures::clock(),
        );
        for name in ["hello.sample-acct", "합성 계정"] {
            view.groups[0].name = name.into();
            let scene = renderer.scene(&view, &state, [1320., 960.])?;
            let (rect, measure) = scene
                .draw
                .iter()
                .find_map(|draw| match draw {
                    Draw::Text { rect, measure, .. } if measure.text == name => {
                        Some((*rect, measure))
                    }
                    _ => None,
                })
                .expect("actual account header draw");
            let layout =
                renderer
                    .shared
                    .dashboard_text()
                    .layout(name, bounds(rect), style(measure))?;
            assert_eq!(layout.metrics.visible_lines, 1, "header name must be drawn");
            assert!(
                !layout.metrics.ink_overflow,
                "Korean strokes must fit the line box"
            );
        }
        Ok(())
    }
    #[test]
    fn r7_directwrite_wrap_preserves_long_tokens_and_measured_height() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let measure = Measure {
            text: "verylongunbrokenfoldername0123456789".repeat(4),
            size: 13.,
            line_height: 18.,
            width: 120.,
            bold: false,
            wrap: true,
            face: Face::Text,
            align: TextAlign::Leading,
        };
        let wrapped = renderer.wrapped(&measure)?;
        assert_eq!(wrapped.replace('\n', ""), measure.text);
        assert!(wrapped.contains('\n'));
        let layout = renderer.shared.dashboard_text().layout(
            &wrapped,
            D2D_RECT_F {
                left: 0.,
                top: 0.,
                right: measure.width,
                bottom: 65536.,
            },
            style(&measure),
        )?;
        assert!(layout.metrics.required_height > measure.line_height);
        Ok(())
    }
    #[test]
    fn r7_approved_theme_tokens_and_noncolor_series_identity() {
        let light = Palette::new(false);
        let dark = Palette::new(true);
        assert_eq!(color(light, Color::Page), Rgb::hex(0xf3f3f3));
        assert_eq!(color(dark, Color::Page), Rgb::hex(0x1b1b1b));
        assert_eq!(color(light, Color::BarOk), light.bars[0]);
        assert_ne!(color(light, Color::Series7d), color(light, Color::BarOk));
    }
}
