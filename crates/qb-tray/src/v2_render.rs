//! Shared HWND/WIC tray-v2 painting over the exact input/hit-test scene.
use crate::{
    Rect, v2,
    v2_colors::{Palette, Rgb},
    v2_geometry as g,
    v2_text::{Align, Face, Style, Text, Wrap},
};
use std::{collections::BTreeSet, time::SystemTime};
use windows::{
    Win32::Graphics::{
        Direct2D::{
            Common::{D2D_RECT_F, D2D1_COLOR_F},
            D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ELLIPSE, D2D1_ROUNDED_RECT, ID2D1RenderTarget,
        },
        DirectWrite::{DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD},
    },
    core::Result,
};

pub struct Renderer {
    text: Text,
    palette: Palette,
}
pub struct LoginStatus {
    pub message: String,
    pub countdown: Option<String>,
    pub copy_ready: bool,
    pub retry: bool,
}
pub struct Tooltip {
    pub rect: Rect,
    pub text: String,
}
pub struct RenderOptions<'a> {
    pub version: &'a str,
    pub now: SystemTime,
    pub hover: Option<&'a g::Target>,
    pub pressed: Option<&'a g::Target>,
    pub focus: Option<&'a g::Target>,
    pub core_note: Option<&'a str>,
    pub last_success: Option<SystemTime>,
    pub errors: &'a BTreeSet<String>,
    pub login: Option<&'a LoginStatus>,
    pub tooltip: Option<&'a Tooltip>,
    pub draw_window_border: bool,
}

fn bounds(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}
fn rgb(c: Rgb, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.0 as f32 / 255.,
        g: c.1 as f32 / 255.,
        b: c.2 as f32 / 255.,
        a: alpha,
    }
}
fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}
fn style(size: f32, bold: bool, line_height: f32) -> Style {
    Style {
        face: Face::Text,
        size,
        weight: if bold {
            DWRITE_FONT_WEIGHT_SEMI_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        },
        line_height,
        align: Align::Leading,
        wrap: Wrap::SingleLine,
    }
}
fn account<'a>(view: &'a v2::View, id: &str) -> Option<&'a v2::AccountRow> {
    view.sections
        .iter()
        .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
        .find(|r| r.raw.id == id)
}

impl Renderer {
    /// Release the old target domain before creating its replacement.
    pub fn clear_target_resources(&self) {
        self.text.clear_target_resources();
    }

    pub(crate) fn dashboard_text(&self) -> &Text {
        &self.text
    }
    pub(crate) fn dashboard_palette(&self) -> Palette {
        self.palette
    }
    pub(crate) fn dashboard_line(
        &self,
        target: &ID2D1RenderTarget,
        from: [f32; 2],
        to: [f32; 2],
        color: Rgb,
        width: f32,
        alpha: f32,
    ) -> Result<()> {
        let brush = self.text.solid_brush(target, rgb(color, alpha))?;
        unsafe {
            target.DrawLine(
                windows_numerics::Vector2 {
                    X: from[0],
                    Y: from[1],
                },
                windows_numerics::Vector2 { X: to[0], Y: to[1] },
                &brush,
                width,
                None,
            );
        }
        Ok(())
    }

    pub fn new(dark: bool) -> Result<Self> {
        Ok(Self {
            text: Text::new()?,
            palette: Palette::new(dark),
        })
    }
    fn width(&self, text: &str, size: u8, bold: bool) -> Result<f32> {
        Ok(self
            .text
            .layout(
                text,
                bounds(r(0., 0., 65536., 32.)),
                style(size as f32, bold, 32.),
            )?
            .metrics
            .natural_width)
    }
    // Keep R5's Korean keep-all layout; split only overlong tokens such as paths
    // using the same DirectWrite font metrics. This keeps every identity byte
    // readable through vertical scrolling instead of silently ellipsizing it.
    fn takeover_wrap(&self, text: &str) -> Result<String> {
        let mut out = String::new();
        for (index, paragraph) in text.split('\n').enumerate() {
            if index > 0 {
                out.push('\n');
            }
            for token in paragraph.split_inclusive(' ') {
                if self.width(token, 13, false)? <= 288. {
                    out.push_str(token);
                    continue;
                }
                let mut chunk = String::new();
                for c in token.chars() {
                    let candidate = format!("{chunk}{c}");
                    if !chunk.is_empty() && self.width(&candidate, 13, false)? > 288. {
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
    pub fn measure(&self, view: &v2::View, p: &mut g::Presentation) -> Result<()> {
        if let g::Overlay::TakeoverCard { body, .. } = &p.overlay {
            let mut s = style(13., false, 20.);
            s.wrap = Wrap::KeepAll;
            p.credits_body_lines = self
                .text
                .layout(
                    &self.takeover_wrap(body)?,
                    bounds(r(0., 0., 288., 65536.)),
                    s,
                )?
                .metrics
                .total_lines
                .max(1);
        }
        for detail in g::QUIT_DETAILS {
            let mut text_style = style(12., false, 16.);
            text_style.wrap = Wrap::KeepAll;
            let lines = self
                .text
                .layout(detail, bounds(r(0., 0., 266., 4096.)), text_style)?
                .metrics
                .total_lines;
            p.subtitle_lines.insert(detail.into(), lines);
        }
        for row in view
            .sections
            .iter()
            .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
        {
            let mut labels = vec![row.name.as_str()];
            if let Some(plan) = &row.plan {
                labels.push(plan);
            }
            if let Some(chip) = &row.chip {
                labels.push(&chip.text);
            }
            if let Some(action) = &row.action {
                labels.push(action.label);
            }
            if let Some(usage) = &row.usage {
                if usage.lines.is_empty() {
                    labels.push(&usage.text);
                } else {
                    for line in &usage.lines {
                        labels.push(&line.window.label);
                        if let Some(reset) = line.window.reset_text.as_deref() {
                            labels.push(reset);
                        }
                    }
                }
                for line in &usage.lines {
                    if let Some(remaining) = line.window.remaining {
                        let text = format!("{remaining}% 남음");
                        for (size, bold) in [(12, true), (13, true)] {
                            p.text_widths.insert(
                                g::text_key(&text, size, bold),
                                self.width(&text, size, bold)?,
                            );
                        }
                    }
                }
            }
            for text in labels {
                for (size, bold) in [(12, false), (12, true), (13, false), (13, true)] {
                    p.text_widths
                        .insert(g::text_key(text, size, bold), self.width(text, size, bold)?);
                }
            }
            if let Some(confirm) = &row.menu.credits_enable_confirmation {
                let mut s = style(13., false, 20.);
                s.wrap = Wrap::KeepAll;
                let count = self
                    .text
                    .layout(&confirm.body, bounds(r(0., 0., 288., 4096.)), s)?
                    .metrics
                    .total_lines;
                if matches!(&p.overlay,g::Overlay::CreditsConfirm(id) if id==&row.raw.id) {
                    p.credits_body_lines = count;
                }
            }
        }
        for text in [
            "켜기",
            "다시 로그인",
            "크레딧으로 계속",
            "크레딧 사용 끄기",
            "로그 열기",
            "계정 추가",
        ] {
            for bold in [false, true] {
                p.text_widths
                    .insert(g::text_key(text, 12, bold), self.width(text, 12, bold)?);
            }
        }
        Ok(())
    }
    pub(crate) fn fill(
        &self,
        target: &ID2D1RenderTarget,
        rect: Rect,
        color: Rgb,
        alpha: f32,
        radius: f32,
    ) -> Result<()> {
        let brush = self.text.solid_brush(target, rgb(color, alpha))?;
        unsafe {
            target.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: bounds(rect),
                    radiusX: radius,
                    radiusY: radius,
                },
                &brush,
            )
        };
        Ok(())
    }
    pub(crate) fn stroke(
        &self,
        target: &ID2D1RenderTarget,
        rect: Rect,
        color: Rgb,
        width: f32,
        radius: f32,
        alpha: f32,
    ) -> Result<()> {
        let brush = self.text.solid_brush(target, rgb(color, alpha))?;
        unsafe {
            target.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: bounds(rect),
                    radiusX: radius,
                    radiusY: radius,
                },
                &brush,
                width,
                None,
            )
        };
        Ok(())
    }
    pub(crate) fn text(
        &self,
        target: &ID2D1RenderTarget,
        rect: Rect,
        text: &str,
        s: Style,
        color: Rgb,
        alpha: f32,
    ) -> Result<()> {
        if text.is_empty() || rect.right <= rect.left || rect.bottom <= rect.top {
            return Ok(());
        }
        self.text
            .draw(target, text, bounds(rect), s, rgb(color, alpha))?;
        Ok(())
    }
    fn icon(
        &self,
        target: &ID2D1RenderTarget,
        rect: Rect,
        text: &str,
        size: f32,
        color: Rgb,
        alpha: f32,
    ) -> Result<()> {
        let mut s = style(size, false, rect.bottom - rect.top);
        s.face = Face::Icon;
        self.text(target, rect, text, s, color, alpha)
    }
    fn clip<T>(
        &self,
        target: &ID2D1RenderTarget,
        rect: Rect,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        unsafe { target.PushAxisAlignedClip(&bounds(rect), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) };
        let result = f();
        unsafe { target.PopAxisAlignedClip() };
        result
    }
    fn overlay(
        &self,
        target: &ID2D1RenderTarget,
        rect: Rect,
        radius: f32,
        pressed: bool,
    ) -> Result<()> {
        self.fill(
            target,
            rect,
            if self.palette.dark {
                Rgb(255, 255, 255)
            } else {
                Rgb(0, 0, 0)
            },
            if pressed {
                if self.palette.dark { 0.04 } else { 0.03 }
            } else if self.palette.dark {
                0.06
            } else {
                0.045
            },
            radius,
        )
    }
    fn button(
        &self,
        target: &ID2D1RenderTarget,
        b: &g::ButtonBox,
        accent: bool,
        small: bool,
        o: &RenderOptions<'_>,
    ) -> Result<()> {
        let p = self.palette;
        let alpha = if !b.enabled {
            0.5
        } else if accent && o.hover == Some(&b.target) {
            0.9
        } else {
            1.
        };
        let accent_color = if matches!(&b.target, g::Target::ConfirmDelete(_)) {
            p.bad
        } else {
            p.accent
        };
        self.fill(
            target,
            b.visual,
            if accent { accent_color } else { p.button },
            alpha,
            4.,
        )?;
        if !accent {
            self.stroke(target, b.visual, p.button_border, 1., 4., alpha)?;
        }
        if b.enabled && ((!accent && o.hover == Some(&b.target)) || o.pressed == Some(&b.target)) {
            self.overlay(target, b.visual, 4., o.pressed == Some(&b.target))?;
        }
        let font = if small { 12. } else { 13. };
        let bold = accent && small;
        let width = self.width(&b.label, font as u8, bold)?;
        let glyph = match &b.target {
            g::Target::AddProvider(_) if !small => Some("\u{e710}"),
            g::Target::LoginCopy if b.label == "주소 복사" => Some("\u{e8c8}"),
            _ => None,
        };
        let total_width = width + if glyph.is_some() { 22. } else { 0. };
        let content_x =
            b.visual.left + ((b.visual.right - b.visual.left - total_width) / 2.).max(4.);
        if let Some(glyph) = glyph {
            self.icon(
                target,
                r(
                    content_x,
                    b.visual.top + (b.visual.bottom - b.visual.top - 14.) / 2.,
                    14.,
                    14.,
                ),
                glyph,
                14.,
                if accent { p.on_accent } else { p.text },
                alpha,
            )?;
        }
        let text_x = content_x + if glyph.is_some() { 22. } else { 0. };
        let text_rect = r(
            text_x,
            b.visual.top,
            width.min((b.visual.right - text_x - 4.).max(0.)),
            b.visual.bottom - b.visual.top,
        );
        self.text(
            target,
            text_rect,
            &b.label,
            style(font, bold, b.visual.bottom - b.visual.top),
            if accent { p.on_accent } else { p.text },
            alpha,
        )
    }
    pub fn tooltip(&self, text: &str, anchor: Rect, scene: Rect) -> Result<Tooltip> {
        let width = (scene.right - scene.left - 16.).clamp(32., 288.);
        let mut s = style(12., false, 16.);
        s.wrap = Wrap::KeepAll;
        let measured = self
            .text
            .layout(text, bounds(r(0., 0., width - 20., 4096.)), s)?
            .metrics;
        let height =
            (measured.required_height + 16.).min((scene.bottom - scene.top - 16.).max(16.));
        let top = if anchor.bottom + 14. + height <= scene.bottom - 8. {
            anchor.bottom + 14.
        } else {
            (anchor.top - 14. - height).max(scene.top + 8.)
        };
        let left = anchor.left.clamp(
            scene.left + 8.,
            (scene.right - 8. - width).max(scene.left + 8.),
        );
        Ok(Tooltip {
            rect: r(left, top, width, height),
            text: text.into(),
        })
    }
    pub fn paint(
        &self,
        target: &ID2D1RenderTarget,
        view: &v2::View,
        scene: &g::Scene,
        o: &RenderOptions<'_>,
    ) -> Result<()> {
        unsafe { target.BeginDraw() };
        let result = self.clip(target, scene.rect, || {
            self.paint_inner(target, view, scene, o)
        });
        let ended = unsafe { target.EndDraw(None, None) };
        result.and(ended)
    }
    fn paint_inner(
        &self,
        target: &ID2D1RenderTarget,
        view: &v2::View,
        scene: &g::Scene,
        o: &RenderOptions<'_>,
    ) -> Result<()> {
        self.fill(target, scene.rect, self.palette.surface, 1., 8.)?;
        self.header(target, view, scene, o)?;
        self.clip(target, scene.list, || self.list(target, view, scene, o))?;
        self.footer(target, scene, o)?;
        if let Some(popup) = &scene.popup {
            self.popup(target, view, popup, o)?;
        }
        if let Some(dialog) = &scene.dialog {
            self.dialog(target, view, scene, dialog, o)?;
        }
        if let Some(tip) = o.tooltip {
            self.popup_surface(target, tip.rect, 6.)?;
            self.clip(target, tip.rect, || self.tooltip_text(target, tip))?;
        }
        if let Some(focus) = o.focus
            && let Some(hit) = scene
                .targets
                .iter()
                .rev()
                .find(|h| &h.target == focus && h.enabled)
        {
            let b = hit.rect;
            let outer = r(
                b.left - 3.,
                b.top - 3.,
                b.right - b.left + 6.,
                b.bottom - b.top + 6.,
            );
            self.stroke(target, outer, self.palette.focus_outer, 2., 8., 1.)?;
            self.stroke(
                target,
                r(
                    b.left - 1.,
                    b.top - 1.,
                    b.right - b.left + 2.,
                    b.bottom - b.top + 2.,
                ),
                self.palette.focus_inner,
                1.,
                6.,
                1.,
            )?;
        }
        if o.draw_window_border {
            self.stroke(
                target,
                r(0.5, 0.5, 359., (scene.rect.bottom - 1.).max(0.)),
                self.palette.border,
                1.,
                8.,
                1.,
            )?;
        }
        Ok(())
    }
    fn header(
        &self,
        target: &ID2D1RenderTarget,
        view: &v2::View,
        scene: &g::Scene,
        o: &RenderOptions<'_>,
    ) -> Result<()> {
        let p = self.palette;
        self.text(
            target,
            scene.header.title,
            "quota-baton",
            style(14., true, 20.),
            p.text,
            1.,
        )?;
        if let Some(b) = scene.header.banner {
            self.text(
                target,
                b,
                "코어에 연결할 수 없습니다",
                style(13., false, 28.),
                p.bad,
                1.,
            )?;
        } else {
            for (summary, box_) in view.summaries.iter().zip(&scene.header.summaries) {
                let y = box_.top + 5.;
                let dot = p.dots[match summary.tone {
                    v2::Tone::Ok => 0,
                    v2::Tone::Warn => 1,
                    v2::Tone::Bad => 2,
                    _ => 3,
                }];
                let brush = self.text.solid_brush(target, rgb(dot, 1.))?;
                unsafe {
                    target.FillEllipse(
                        &D2D1_ELLIPSE {
                            point: windows_numerics::Vector2 {
                                X: 20.,
                                Y: box_.top + 14.,
                            },
                            radiusX: 4.,
                            radiusY: 4.,
                        },
                        &brush,
                    )
                };
                let name_width = self.width(&summary.provider_label, 13, true)?;
                self.text(
                    target,
                    r(32., y, name_width, 18.),
                    &summary.provider_label,
                    style(13., true, 18.),
                    p.text,
                    1.,
                )?;
                let x = 32. + name_width + 6.;
                let verdict = summary.verdict.text();
                let verdict_width = self.width(verdict, 13, false)?;
                self.text(
                    target,
                    r(x, y, verdict_width, 18.),
                    verdict,
                    style(13., false, 18.),
                    p.text,
                    1.,
                )?;
                let detail_left = x + verdict_width + 12.;
                let todo = if summary.todo > 0 {
                    format!("할 일 {}", summary.todo)
                } else {
                    String::new()
                };
                let todo_width = if todo.is_empty() {
                    0.
                } else {
                    self.width(&todo, 12, true)?
                };
                let separator_width = if todo.is_empty() {
                    0.
                } else {
                    self.width(" · ", 12, false)?
                };
                let detail_right = 344. - todo_width - separator_width;
                let mut s = style(12., false, 18.);
                s.align = Align::Trailing;
                self.text(
                    target,
                    r(detail_left, y, (detail_right - detail_left).max(0.), 18.),
                    &summary.detail,
                    s,
                    p.secondary,
                    1.,
                )?;
                if !todo.is_empty() {
                    self.text(
                        target,
                        r(detail_right, y, separator_width, 18.),
                        " · ",
                        style(12., false, 18.),
                        p.secondary,
                        1.,
                    )?;
                    self.text(
                        target,
                        r(344. - todo_width, y, todo_width, 18.),
                        &todo,
                        style(12., true, 18.),
                        p.bad,
                        1.,
                    )?;
                }
            }
        }
        if let Some(b) = scene.header.note {
            self.text(
                target,
                b,
                if scene.core_ok {
                    view.note.as_deref().unwrap_or("")
                } else {
                    o.core_note.unwrap_or("코어 상태를 읽지 못했습니다")
                },
                style(12., false, 16.),
                p.secondary,
                1.,
            )?;
        }
        self.fill(target, scene.header.divider, p.divider, 1., 0.)
    }
    fn list(
        &self,
        target: &ID2D1RenderTarget,
        view: &v2::View,
        scene: &g::Scene,
        o: &RenderOptions<'_>,
    ) -> Result<()> {
        let p = self.palette;
        let alpha = if scene.core_ok { 1. } else { 0.5 };
        if let Some(login) = &scene.login {
            return self.login(target, login, o);
        }
        if let Some(empty) = &scene.empty {
            self.text(
                target,
                empty.title,
                "아직 계정이 없습니다",
                style(15., true, 20.),
                p.text,
                1.,
            )?;
            let mut s = style(13., false, 20.);
            s.wrap = Wrap::KeepAll;
            self.text(target,empty.body,"Claude나 Codex 계정을 추가하세요. 한 계정이 한도에 도달하면 다른 계정으로 이어서 처리합니다.",s,p.secondary,1.)?;
            for button in &empty.buttons {
                self.button(target, button, true, false, o)?;
            }
            self.text(
                target,
                r(empty.cli.left, empty.cli.top, 328., 16.),
                "CPA 계정을 가져오려면 명령줄에서",
                style(12., false, 16.),
                p.secondary,
                1.,
            )?;
            let mut mono = style(12., false, 16.);
            mono.face = Face::Mono;
            self.text(
                target,
                r(empty.cli.left, empty.cli.top + 16., 328., 16.),
                "quota-baton accounts import cpa",
                mono,
                p.text,
                1.,
            )?;
            return Ok(());
        }
        for section in &scene.sections {
            let model = view
                .sections
                .iter()
                .find(|s| s.provider == section.provider);
            self.text(
                target,
                r(16., section.label.top + 12., 328., 16.),
                v2::provider_label(&section.provider),
                style(12., true, 16.),
                p.secondary,
                alpha,
            )?;
            for divider in &section.dividers {
                self.fill(target, *divider, p.divider, alpha, 0.)?;
            }
            for b in section.rows.iter().chain(&section.disabled_rows) {
                if let Some(row) = account(view, &b.id) {
                    self.row(target, row, b, o, alpha)?;
                }
            }
            if let Some(b) = section.disclosure {
                let target_ = g::Target::Disclosure(section.provider.clone());
                if o.hover == Some(&target_) || o.pressed == Some(&target_) {
                    self.overlay(target, b, 6., o.pressed == Some(&target_))?;
                }
                let text = model
                    .and_then(|s| s.disclosure.as_deref())
                    .unwrap_or("꺼진 계정");
                let width = self.width(text, 12, false)?;
                self.text(
                    target,
                    r(16., b.top + 8., width, 16.),
                    text,
                    style(12., false, 16.),
                    p.secondary,
                    alpha,
                )?;
                self.icon(
                    target,
                    r(16. + width + 6., b.top + 11., 10., 10.),
                    if section.expanded {
                        "\u{e70d}"
                    } else {
                        "\u{e76c}"
                    },
                    10.,
                    p.secondary,
                    alpha,
                )?;
            }
            if let Some(b) = section.hint {
                self.text(
                    target,
                    b,
                    "켜면 사용량을 확인한 뒤 바로 이어서 처리합니다.",
                    style(12., false, 16.),
                    p.secondary,
                    alpha,
                )?;
            }
            if let Some(b) = section.empty_message {
                self.text(
                    target,
                    b,
                    model
                        .and_then(|s| s.empty_message.as_deref())
                        .unwrap_or("계정이 없습니다"),
                    style(12., false, 16.),
                    p.secondary,
                    alpha,
                )?;
            }
            if let Some(button) = &section.add_button {
                self.button(target, button, false, true, o)?;
            }
        }
        if scene.max_offset > 0. {
            let extent = scene.list.bottom - scene.list.top;
            let height = (extent * extent / scene.content_height.max(extent))
                .max(24.)
                .min(extent);
            let top = scene.list.top + (extent - height) * scene.offset / scene.max_offset;
            self.fill(target, r(354., top, 3., height), p.secondary, 0.45, 1.5)?;
        }
        Ok(())
    }
    fn row(
        &self,
        target: &ID2D1RenderTarget,
        row: &v2::AccountRow,
        b: &g::RowBox,
        o: &RenderOptions<'_>,
        alpha: f32,
    ) -> Result<()> {
        let p = self.palette;
        let row_target = g::Target::Row(row.raw.id.clone());
        if o.hover == Some(&row_target) || o.pressed == Some(&row_target) {
            self.overlay(target, b.hover, 6., o.pressed == Some(&row_target))?;
        }
        let disabled = b.disabled_slot && row.raw.user_state == "disabled";
        self.text(
            target,
            b.name,
            &row.name,
            style(13., !disabled, 18.),
            if disabled { p.secondary } else { p.text },
            alpha,
        )?;
        if let (Some(plan), Some(rect)) = (&row.plan, b.plan) {
            self.text(
                target,
                rect,
                plan,
                style(12., false, 16.),
                p.secondary,
                alpha,
            )?;
        }
        if let (Some(chip), Some(rect)) = (&row.chip, b.chip) {
            let (bg, fg) = p.chip(chip.tone);
            self.fill(target, rect, bg, alpha, 10.)?;
            self.text(
                target,
                r(rect.left + 8., rect.top, rect.right - rect.left - 16., 20.),
                &chip.text,
                style(12., true, 20.),
                fg,
                alpha,
            )?;
        }
        self.icon(target, b.more, "\u{e712}", 16., p.secondary, alpha)?;
        if let (Some(usage), Some(rect)) = (&row.usage, b.usage) {
            if usage.lines.is_empty() {
                self.text(
                    target,
                    rect,
                    "사용량 확인 전",
                    style(12., false, 16.),
                    p.secondary,
                    alpha,
                )?;
            } else {
                for (line, boxes) in usage.lines.iter().zip(&b.usage_lines) {
                    let label_width = self.width(&line.window.label, 12, false)?;
                    self.text(
                        target,
                        r(
                            boxes.line.left,
                            boxes.line.top,
                            label_width.min(boxes.line.right - boxes.line.left),
                            16.,
                        ),
                        &line.window.label,
                        style(12., false, 16.),
                        p.secondary,
                        alpha,
                    )?;
                    let rest = line
                        .window
                        .remaining
                        .map_or_else(|| "확인 전".into(), |n| format!("{n}% 남음"));
                    self.text(
                        target,
                        r(
                            boxes.line.left + label_width + 4.,
                            boxes.line.top,
                            (boxes.line.right - boxes.line.left - label_width - 4.).max(0.),
                            16.,
                        ),
                        &rest,
                        style(12., true, 16.),
                        p.text,
                        alpha,
                    )?;
                    if let Some(reset) = &line.window.reset_text
                        && let Some(reset_rect) = boxes.reset
                    {
                        let mut s = style(12., false, 16.);
                        s.align = Align::Trailing;
                        self.text(target, reset_rect, reset, s, p.secondary, alpha)?;
                    }
                    self.fill(target, boxes.bar, p.track, alpha, 2.)?;
                    if line.bar_tone == v2::BarTone::Limit {
                        let mut x = boxes.bar.left;
                        while x < boxes.bar.right {
                            self.fill(
                                target,
                                Rect {
                                    left: x,
                                    right: (x + 4.).min(boxes.bar.right),
                                    ..boxes.bar
                                },
                                p.chip(v2::Tone::Limit).0,
                                alpha,
                                0.,
                            )?;
                            x += 7.;
                        }
                    }
                    if let Some(fill) = boxes.fill {
                        let color = match line.bar_tone {
                            v2::BarTone::Ok => p.bars[0],
                            v2::BarTone::Low => p.bars[1],
                            v2::BarTone::Critical => p.bars[2],
                            v2::BarTone::Paused => p.bars[3],
                            v2::BarTone::Limit => p.chip(v2::Tone::Limit).0,
                        };
                        self.fill(target, fill, color, alpha, 2.)?;
                    }
                }
            }
        }
        if let (Some(credits), Some(rect)) = (&row.credits, b.credits) {
            self.text(
                target,
                rect,
                &credits.text,
                style(12., false, 16.),
                p.tone(credits.tone),
                alpha,
            )?;
        }
        if let (Some(reason), Some(rect)) = (&row.reason, b.reason) {
            self.text(
                target,
                rect,
                reason,
                style(12., false, 16.),
                if o.errors.contains(&row.raw.id) {
                    p.bad
                } else {
                    p.secondary
                },
                alpha,
            )?;
        }
        if let Some(button) = &b.button {
            let accent = row.action.as_ref().is_some_and(|a| a.accent);
            self.button(target, button, accent, true, o)?;
        }
        Ok(())
    }
    fn footer(
        &self,
        target: &ID2D1RenderTarget,
        scene: &g::Scene,
        o: &RenderOptions<'_>,
    ) -> Result<()> {
        let p = self.palette;
        self.fill(target, scene.footer.rect, p.footer, 1., 0.)?;
        self.fill(
            target,
            r(0., scene.footer.rect.top, 360., 1.),
            p.divider,
            1.,
            0.,
        )?;
        for b in &scene.footer.buttons {
            let opened = match &b.target {
                g::Target::Footer(g::FooterAction::Settings) => scene
                    .popup
                    .as_ref()
                    .is_some_and(|x| x.rect.right - x.rect.left == 240.),
                g::Target::Footer(g::FooterAction::Quit) => scene
                    .popup
                    .as_ref()
                    .is_some_and(|x| x.rect.right - x.rect.left == 300.),
                _ => false,
            };
            if opened || o.hover == Some(&b.target) || o.pressed == Some(&b.target) {
                self.overlay(target, b.visual, 4., o.pressed == Some(&b.target))?;
            }
            let glyph = match b.target {
                g::Target::Footer(g::FooterAction::Add) => "\u{e710}",
                g::Target::Footer(g::FooterAction::Settings) => "\u{e713}",
                _ => "\u{e7e8}",
            };
            let alpha = if b.enabled { 1. } else { 0.5 };
            self.icon(
                target,
                r(b.visual.left + 8., b.visual.top + 9., 14., 14.),
                glyph,
                14.,
                p.text,
                alpha,
            )?;
            self.text(
                target,
                r(
                    b.visual.left + 28.,
                    b.visual.top + 7.,
                    b.visual.right - b.visual.left - 36.,
                    18.,
                ),
                &b.label,
                style(13., false, 18.),
                p.text,
                alpha,
            )?;
        }
        if let Some(text) = o.last_success.and_then(|at| v2::stale_notice(at, o.now)) {
            let mut s = style(12., false, 16.);
            s.align = Align::Trailing;
            self.text(target, scene.footer.stale_space, &text, s, p.secondary, 1.)?;
        }
        Ok(())
    }
    fn popup_surface(&self, target: &ID2D1RenderTarget, rect: Rect, radius: f32) -> Result<()> {
        let opacity = if self.palette.dark { 0.055 } else { 0.022 };
        for spread in (1..=6).rev() {
            let s = spread as f32;
            self.fill(
                target,
                r(
                    rect.left - s,
                    rect.top + 4. - s,
                    rect.right - rect.left + 2. * s,
                    rect.bottom - rect.top + 2. * s,
                ),
                Rgb(0, 0, 0),
                opacity,
                radius + s,
            )?;
        }
        self.fill(target, rect, self.palette.popup, 1., radius)?;
        self.stroke(target, rect, self.palette.popup_border, 1., radius, 1.)
    }
    fn popup(
        &self,
        target: &ID2D1RenderTarget,
        view: &v2::View,
        popup: &g::PopupBox,
        o: &RenderOptions<'_>,
    ) -> Result<()> {
        let p = self.palette;
        self.popup_surface(target, popup.rect, 8.)?;
        let row = popup.items.iter().find_map(|item| {
            let g::Target::Menu { id: Some(id), .. } = &item.target else {
                return None;
            };
            account(view, id)
        });
        self.clip(target, popup.clip, || {
            for line in &popup.info {
                if let Some(row) = row {
                    if let Some(index) = row
                        .menu
                        .window_lines
                        .iter()
                        .position(|text| text == &line.text)
                        && let Some(window) = row
                            .windows
                            .iter()
                            .filter(|w| w.account_scoped || w.remaining.is_some())
                            .nth(index)
                    {
                        let value = window
                            .remaining
                            .map_or_else(|| "사용량 확인 전".into(), |n| format!("{n}% 남음"));
                        let rest = window
                            .reset_text
                            .as_deref()
                            .map_or(String::new(), |s| format!(" · {s}"))
                            + if window.model_limited {
                                " · 사용 제한"
                            } else {
                                ""
                            };
                        self.value_line(
                            target,
                            line.rect,
                            &window.label,
                            &value,
                            &rest,
                            window.remaining.is_some(),
                        )?;
                        continue;
                    }
                    if row.menu.credits_line.as_ref() == Some(&line.text) {
                        self.value_line(
                            target,
                            line.rect,
                            "크레딧",
                            line.text.strip_prefix("크레딧 ").unwrap_or("확인 전"),
                            "",
                            true,
                        )?;
                        continue;
                    }
                }
                self.text(
                    target,
                    line.rect,
                    &line.text,
                    style(
                        if line.emphasized { 13. } else { 12. },
                        line.emphasized,
                        if line.emphasized { 18. } else { 16. },
                    ),
                    if line.emphasized {
                        p.text
                    } else if row
                        .is_some_and(|r| r.menu.refresh_warning.as_ref() == Some(&line.text))
                    {
                        p.bad
                    } else {
                        p.secondary
                    },
                    1.,
                )?;
            }
            for separator in &popup.separators {
                self.fill(target, *separator, p.popup_border, 1., 0.)?;
            }
            for item in &popup.items {
                if item.enabled
                    && (o.hover == Some(&item.target)
                        || o.focus == Some(&item.target)
                        || o.pressed == Some(&item.target))
                {
                    self.overlay(target, item.rect, 4., o.pressed == Some(&item.target))?;
                }
                let alpha = if item.enabled { 1. } else { 0.5 };
                if let Some(lead) = item.lead {
                    if let Some(checked) = item.checked {
                        let box_ = r(lead.left, lead.top, 16., 16.);
                        if checked {
                            self.fill(target, box_, p.accent, alpha, 3.)?;
                            self.icon(target, lead, "\u{e73e}", 12., p.on_accent, alpha)?;
                        } else {
                            self.stroke(target, box_, p.check_border, 1., 3., alpha)?;
                        }
                    } else if let g::Target::Menu { action, .. } = &item.target {
                        self.icon(target, lead, menu_glyph(action), 16., p.secondary, alpha)?;
                    }
                }
                self.text(
                    target,
                    item.label,
                    &item.text,
                    style(13., false, 18.),
                    p.text,
                    alpha,
                )?;
                if let (Some(detail), Some(rect)) = (&item.detail, item.subtitle) {
                    let mut s = style(12., false, 16.);
                    s.wrap = Wrap::KeepAll;
                    self.text(target, rect, detail, s, p.secondary, alpha)?;
                }
            }
            Ok(())
        })
    }
    fn value_line(
        &self,
        target: &ID2D1RenderTarget,
        rect: Rect,
        label: &str,
        value: &str,
        rest: &str,
        bold: bool,
    ) -> Result<()> {
        let p = self.palette;
        self.text(
            target,
            r(rect.left, rect.top, 44., 16.),
            label,
            style(12., false, 16.),
            p.secondary,
            1.,
        )?;
        let width = self
            .width(value, 12, bold)?
            .min((rect.right - rect.left - 44.).max(0.));
        self.text(
            target,
            r(rect.left + 44., rect.top, width, 16.),
            value,
            style(12., bold, 16.),
            if bold { p.text } else { p.secondary },
            1.,
        )?;
        self.text(
            target,
            r(
                rect.left + 44. + width,
                rect.top,
                (rect.right - rect.left - 44. - width).max(0.),
                16.,
            ),
            rest,
            style(12., false, 16.),
            p.secondary,
            1.,
        )
    }
    fn tooltip_text(&self, target: &ID2D1RenderTarget, tip: &Tooltip) -> Result<()> {
        let mut y = tip.rect.top + 8.;
        let mut s = style(12., false, 16.);
        s.wrap = Wrap::KeepAll;
        for paragraph in tip.text.split('\n') {
            let remaining = tip.rect.bottom - 8. - y;
            if remaining < 16. {
                break;
            }
            let mut layout = self.text.layout(
                paragraph,
                bounds(r(
                    tip.rect.left + 10.,
                    y,
                    tip.rect.right - tip.rect.left - 20.,
                    remaining,
                )),
                s,
            )?;
            let mut emphasis = Vec::new();
            if let Some((label, _)) = paragraph.split_once(" 사용량:") {
                emphasis.push(format!("{label} 사용량"));
            }
            for (end, _) in paragraph.match_indices("% 남음") {
                let start = paragraph[..end]
                    .char_indices()
                    .rev()
                    .find(|(_, c)| c.is_whitespace())
                    .map_or(0, |(i, c)| i + c.len_utf8());
                emphasis.push(paragraph[start..end + "% 남음".len()].to_owned());
            }
            if let Some(amount) = paragraph
                .strip_prefix("크레딧 잔액 ")
                .and_then(|s| s.split_whitespace().next())
            {
                emphasis.push(amount.into());
            }
            let refs = emphasis.iter().map(String::as_str).collect::<Vec<_>>();
            layout.emphasize(&refs)?;
            layout.draw(
                target,
                rgb(
                    if paragraph.ends_with("에 초기화됩니다.") || paragraph.starts_with("막대는 ")
                    {
                        self.palette.secondary
                    } else {
                        self.palette.text
                    },
                    1.,
                ),
            )?;
            y += layout.metrics.required_height;
        }
        Ok(())
    }
    fn dialog(
        &self,
        target: &ID2D1RenderTarget,
        view: &v2::View,
        _scene: &g::Scene,
        d: &g::DialogBox,
        o: &RenderOptions<'_>,
    ) -> Result<()> {
        let p = self.palette;
        self.fill(
            target,
            d.scrim,
            Rgb(0, 0, 0),
            if p.dark { 0.35 } else { 0.2 },
            0.,
        )?;
        self.popup_surface(target, d.card, 8.)?;
        let credit_id = d.buttons.iter().find_map(|b| {
            if let g::Target::ConfirmCredits(id) = &b.target {
                Some(id.as_str())
            } else {
                None
            }
        });
        let delete_id = d.buttons.iter().find_map(|b| {
            if let g::Target::ConfirmDelete(id) = &b.target {
                Some(id.as_str())
            } else {
                None
            }
        });
        let credit = credit_id
            .and_then(|id| account(view, id))
            .and_then(|row| row.menu.credits_enable_confirmation.as_ref());
        let title = if d.takeover_body.is_some() {
            if d.buttons[0].target == g::Target::TakeoverCommit {
                "가져오기 확인"
            } else {
                "다른 앱에서 계정 가져오기"
            }
        } else {
            credit.map_or("계정 삭제", |c| c.title)
        };
        self.text(target, d.title, title, style(14., true, 20.), p.text, 1.)?;
        self.clip(target, d.body, || {
            if let Some(body) = &d.takeover_body {
                let mut s = style(13., false, 20.);
                s.wrap = Wrap::KeepAll;
                self.text(
                    target,
                    r(
                        d.body.left,
                        d.body.top - d.body_offset,
                        d.body.right - d.body.left,
                        d.body_height.max(20.),
                    ),
                    &self.takeover_wrap(body)?,
                    s,
                    p.text,
                    1.,
                )?;
            } else if let Some(credit) = credit {
                let mut s = style(13., false, 20.);
                s.wrap = Wrap::KeepAll;
                self.text(target, d.body, &credit.body, s, p.text, 1.)?;
            } else if let Some(row) = delete_id.and_then(|id| account(view, id)) {
                self.text(
                    target,
                    r(d.body.left, d.body.top, d.body.right - d.body.left, 20.),
                    row.raw.email.as_deref().unwrap_or(&row.name),
                    style(13., true, 20.),
                    p.text,
                    1.,
                )?;
                self.text(
                    target,
                    r(
                        d.body.left,
                        d.body.top + 20.,
                        d.body.right - d.body.left,
                        20.,
                    ),
                    "계정을 quota-baton에서 삭제할까요?",
                    style(13., false, 20.),
                    p.text,
                    1.,
                )?;
                self.text(
                    target,
                    r(
                        d.body.left,
                        d.body.top + 44.,
                        d.body.right - d.body.left,
                        16.,
                    ),
                    "제공자 쪽 로그인은 취소되지 않습니다.",
                    style(12., false, 16.),
                    p.secondary,
                    1.,
                )?;
            }
            Ok(())
        })?;
        self.fill(target, d.button_bar, p.footer, 1., 0.)?;
        self.fill(target, d.divider, p.divider, 1., 0.)?;
        for b in &d.buttons {
            self.button(
                target,
                b,
                !matches!(b.target, g::Target::CancelDialog),
                false,
                o,
            )?;
        }
        Ok(())
    }
    fn login(
        &self,
        target: &ID2D1RenderTarget,
        b: &g::LoginBox,
        o: &RenderOptions<'_>,
    ) -> Result<()> {
        let p = self.palette;
        let title = format!("{} 로그인", v2::provider_label(&b.provider));
        self.text(target, b.title, &title, style(15., true, 20.), p.text, 1.)?;
        let message = o.login.map_or(
            "브라우저에서 로그인을 마치면 자동으로 완료됩니다.",
            |l| l.message.as_str(),
        );
        let mut s = style(13., false, 20.);
        s.wrap = Wrap::KeepAll;
        self.text(target, b.status, message, s, p.text, 1.)?;
        if let Some(countdown) = o.login.and_then(|l| l.countdown.as_deref()) {
            self.text(
                target,
                b.countdown,
                countdown,
                style(12., false, 16.),
                p.secondary,
                1.,
            )?;
        }
        let center = windows_numerics::Vector2 {
            X: (b.ring.left + b.ring.right) / 2.,
            Y: (b.ring.top + b.ring.bottom) / 2.,
        };
        let track = self.text.solid_brush(target, rgb(p.track, 1.))?;
        let accent = self.text.solid_brush(target, rgb(p.accent, 1.))?;
        unsafe {
            target.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: 7.,
                    radiusY: 7.,
                },
                &track,
                2.,
                None,
            )
        };
        let angle = o
            .now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            .fract() as f32
            * std::f32::consts::TAU;
        for segment in 0..24 {
            let point = |i: f32| {
                let angle = angle + i * std::f32::consts::FRAC_PI_2 / 24.;
                windows_numerics::Vector2 {
                    X: center.X + 7. * angle.cos(),
                    Y: center.Y + 7. * angle.sin(),
                }
            };
            unsafe {
                target.DrawLine(
                    point(segment as f32),
                    point(segment as f32 + 1.),
                    &accent,
                    2.,
                    None,
                )
            };
        }
        for original in &b.buttons {
            let mut button = original.clone();
            if matches!(button.target, g::Target::LoginCopy) {
                button.enabled = o.login.is_some_and(|l| l.copy_ready || l.retry);
                if o.login.is_some_and(|l| l.retry) {
                    button.label = "다시 시도".into();
                }
            }
            self.button(target, &button, false, false, o)?;
        }
        Ok(())
    }
}

fn menu_glyph(action: &g::MenuAction) -> &'static str {
    match action {
        g::MenuAction::RefreshUsage => "\u{e72c}",
        g::MenuAction::Relogin => "\u{e8d7}",
        g::MenuAction::RefreshToken => "\u{e895}",
        g::MenuAction::Pause => "\u{e769}",
        g::MenuAction::Resume => "\u{e768}",
        g::MenuAction::Disable => "\u{e7e8}",
        g::MenuAction::Enable => "\u{e8fb}",
        g::MenuAction::Delete => "\u{e74d}",
        g::MenuAction::OpenDashboard => "\u{e9d9}",
        g::MenuAction::OpenConfig => "\u{e8a5}",
        g::MenuAction::OpenLogs => "\u{e838}",
        g::MenuAction::AddClaude | g::MenuAction::AddCodex => "\u{e710}",
        _ => "",
    }
}

#[cfg(test)]
mod r6_render_tests {
    use super::*;

    #[test]
    fn r6_long_paths_measure_the_same_full_body_that_scrolls() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let view = v2::project("{\"accounts\":[]}", std::time::SystemTime::UNIX_EPOCH).unwrap();
        for path in [
            format!("C:/{}", "long-directory/".repeat(50)),
            format!("C:/{}", "긴한글폴더/".repeat(50)),
        ] {
            let body = format!(
                "원본 경로: {path}\n마지막 갱신: 알 수 없음\n원래 앱 실행 중: 감지되지 않음"
            );
            let wrapped = renderer.takeover_wrap(&body)?;
            // Presentation line breaks must preserve every path character.
            assert_eq!(wrapped.replace('\n', ""), body.replace('\n', ""));
            let mut s = style(13., false, 20.);
            s.wrap = Wrap::KeepAll;
            let measured = renderer
                .text
                .layout(&wrapped, bounds(r(0., 0., 288., 65536.)), s)?;
            assert!(!measured.metrics.trimmed);
            assert!(measured.metrics.total_lines > 10);
            let mut p = g::Presentation {
                overlay: g::Overlay::TakeoverCard {
                    body,
                    confirming: true,
                    ready: true,
                    replace: false,
                },
                popup_offset: 100_000.,
                ..Default::default()
            };
            renderer.measure(&view, &mut p)?;
            assert_eq!(p.credits_body_lines, measured.metrics.total_lines);
            for cap in [240., 480., 800.] {
                let scene = g::Scene::build(&view, p.clone(), cap);
                let dialog = scene.dialog.unwrap();
                assert_eq!(dialog.body_height, measured.metrics.required_height);
                assert!((dialog.body_offset + g::h(dialog.body) - dialog.body_height).abs() < 0.1);
                assert!(
                    dialog
                        .buttons
                        .iter()
                        .all(|b| b.hit.bottom <= dialog.card.bottom)
                );
            }
        }
        Ok(())
    }
}
