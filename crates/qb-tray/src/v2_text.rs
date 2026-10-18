//! Paint-pass scoped DirectWrite text, shared by native and WIC D2D targets.
//! Drop Text AND its Layouts at the end of each pass; do not cache in tray state.
#![cfg(target_os = "windows")]

use windows::{
    Win32::{
        Foundation::{E_FAIL, E_INVALIDARG},
        Graphics::{
            Direct2D::{
                Common::{D2D_RECT_F, D2D1_COLOR_F},
                D2D1_DRAW_TEXT_OPTIONS_NONE, ID2D1RenderTarget,
            },
            DirectWrite::*,
        },
    },
    core::{Error, PCWSTR, Result},
};
use windows_numerics::Vector2;

#[derive(Clone, Copy, Debug)]
pub enum Face {
    Text,
    Icon,
    Mono,
}

#[derive(Clone, Copy, Debug)]
pub enum Align {
    Leading,
    Trailing,
}

#[derive(Clone, Copy, Debug)]
pub enum Wrap {
    SingleLine,
    KeepAll,
}

#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub face: Face,
    /// DIPs, as are all bounds and measurements in this module.
    pub size: f32,
    /// Use DWRITE_FONT_WEIGHT_NORMAL (400) or SEMI_BOLD (600).
    pub weight: DWRITE_FONT_WEIGHT,
    pub line_height: f32,
    pub align: Align,
    pub wrap: Wrap,
}

/// One isolated factory per paint pass. No global factory or font cache.
pub struct Text {
    factory: IDWriteFactory,
    text_family: &'static str,
    icon_family: &'static str,
    mono_family: &'static str,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Metrics {
    /// Maximum untrimmed advance, including trailing spaces, of wrapped lines.
    pub natural_width: f32,
    /// Full required line-box height, including lines omitted by height bounds.
    pub required_height: f32,
    /// Visible advances / line-box height, bounded by the requested rectangle.
    pub width: f32,
    pub height: f32,
    pub total_lines: usize,
    pub visible_lines: usize,
    pub trimmed: bool,
    /// Ink exceeds a line box (e.g. an unusually small requested line height).
    /// Grow the caller's box/spacing; drawing deliberately does not clip ink.
    pub ink_overflow: bool,
}

struct Line {
    layout: IDWriteTextLayout,
    origin: Vector2,
}

/// Owns shaped layouts, never the input string. Keep only for this paint pass.
pub struct Layout {
    lines: Vec<Line>,
    pub metrics: Metrics,
}

impl Text {
    pub fn new() -> Result<Self> {
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED)? };
        let mut collection = None;
        unsafe {
            factory.GetSystemFontCollection(&mut collection, false)?;
        }
        let collection = collection.ok_or_else(|| Error::from_hresult(E_FAIL))?;
        Ok(Self {
            text_family: choose_family(&collection, &["Segoe UI Variable Text", "Segoe UI"])?,
            icon_family: choose_family(&collection, &["Segoe Fluent Icons", "Segoe MDL2 Assets"])?,
            mono_family: choose_family(&collection, &["Cascadia Mono", "Consolas"])?,
            factory,
        })
    }

    pub fn families(&self) -> (&'static str, &'static str) {
        (self.text_family, self.icon_family)
    }

    fn format(&self, style: Style) -> Result<IDWriteTextFormat> {
        let family = wide(match style.face {
            Face::Text => self.text_family,
            Face::Icon => self.icon_family,
            Face::Mono => self.mono_family,
        });
        // No custom font collection/fallback: DirectWrite's system fallback
        // resolves Hangul (normally Malgun Gothic on Korean Windows).
        unsafe {
            let format = self.factory.CreateTextFormat(
                PCWSTR(family.as_ptr()),
                None,
                style.weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                style.size,
                windows::core::w!("ko-kr"),
            )?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
            format.SetTextAlignment(match style.align {
                Align::Leading => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Trailing => DWRITE_TEXT_ALIGNMENT_TRAILING,
            })?;
            let sign = self.factory.CreateEllipsisTrimmingSign(&format)?;
            format.SetTrimming(
                &DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                },
                &sign,
            )?;
            Ok(format)
        }
    }

    fn run(
        &self,
        value: &str,
        format: &IDWriteTextFormat,
        width: f32,
        height: f32,
    ) -> Result<IDWriteTextLayout> {
        let utf16: Vec<u16> = value.encode_utf16().collect();
        let length = u32::try_from(utf16.len()).map_err(|_| Error::from_hresult(E_INVALIDARG))?;
        unsafe {
            let layout = self
                .factory
                .CreateTextLayout(&utf16, format, width, height)?;
            let typography = self.factory.CreateTypography()?;
            typography.AddFontFeature(DWRITE_FONT_FEATURE {
                nameTag: DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES,
                parameter: 1,
            })?;
            layout.SetTypography(
                &typography,
                DWRITE_TEXT_RANGE {
                    startPosition: 0,
                    length,
                },
            )?;
            Ok(layout)
        }
    }

    fn advance(&self, value: &str, format: &IDWriteTextFormat) -> Result<f32> {
        // Disable trimming explicitly: fitting always uses full shaped advances.
        let layout = self.run(value, format, 1.0, 1.0)?;
        unsafe {
            layout.SetTrimming(
                &DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_NONE,
                    delimiter: 0,
                    delimiterCount: 0,
                },
                None,
            )?;
        }
        Ok(measure(&layout)?.widthIncludingTrailingWhitespace)
    }

    /// Shape and measure without a render target. Only ASCII spaces are wrap
    /// opportunities; CRLF/CR/LF are explicit breaks. Long words stay intact
    /// in the wrapping algorithm and are ellipsized by DirectWrite when drawn.
    pub fn layout(&self, value: &str, bounds: D2D_RECT_F, style: Style) -> Result<Layout> {
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        if ![
            bounds.left,
            bounds.top,
            bounds.right,
            bounds.bottom,
            width,
            height,
            style.size,
            style.line_height,
        ]
        .iter()
        .all(|v| v.is_finite())
            || width <= 0.0
            || height < 0.0
            || style.size <= 0.0
            || style.line_height <= 0.0
        {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        let format = self.format(style)?;
        let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
        let mut strings = Vec::new();
        match style.wrap {
            Wrap::SingleLine => strings.push(normalized.replace('\n', " ")),
            Wrap::KeepAll => {
                for paragraph in normalized.split('\n') {
                    let mut line = String::new();
                    for token in paragraph.split_inclusive(' ') {
                        let candidate = format!("{line}{token}");
                        // Ignore trailing spaces for deciding a break, but retain
                        // the original spaces in the shaped/measured line.
                        if !line.trim_end_matches(' ').is_empty()
                            && self.advance(candidate.trim_end_matches(' '), &format)? > width
                        {
                            strings.push(std::mem::take(&mut line));
                        }
                        line.push_str(token);
                    }
                    strings.push(line);
                }
            }
        }
        let visible = ((height / style.line_height).floor() as usize).min(strings.len());
        let mut metrics = Metrics {
            required_height: strings.len() as f32 * style.line_height,
            height: visible as f32 * style.line_height,
            total_lines: strings.len(),
            visible_lines: visible,
            trimmed: visible < strings.len(),
            ..Metrics::default()
        };
        let mut lines = Vec::new();
        for (index, original) in strings.iter().enumerate() {
            let advance = self.advance(original, &format)?;
            metrics.natural_width = metrics.natural_width.max(advance);
            if index >= visible {
                continue;
            }
            // Signal omitted subsequent lines even when this line fits in width.
            let displayed = if index + 1 == visible && visible < strings.len() {
                format!("{}\u{2026}", original.trim_end_matches(' '))
            } else {
                original.clone()
            };
            let probe = self.run(&displayed, &format, width, style.line_height)?;
            let natural = measure(&probe)?;
            // Use the actual fallback font line height, rather than forcing a
            // Latin baseline onto Hangul. Center ink + logical line extents.
            let layout = self.run(&displayed, &format, width, natural.height)?;
            let measured = measure(&layout)?;
            let overhang = unsafe { layout.GetOverhangMetrics()? };
            let top = measured.top.min(-overhang.top);
            let bottom = (measured.top + measured.height).max(natural.height + overhang.bottom);
            let ink_height = bottom - top;
            let mut line_metrics = [DWRITE_LINE_METRICS::default()];
            let mut count = 0;
            unsafe {
                layout.GetLineMetrics(Some(&mut line_metrics), &mut count)?;
            }
            metrics.trimmed |= line_metrics[0].isTrimmed.as_bool();
            metrics.width = metrics
                .width
                .max(measured.widthIncludingTrailingWhitespace.min(width));
            metrics.ink_overflow |= ink_height > style.line_height + 0.01
                || overhang.left > 0.01
                || overhang.right > 0.01;
            lines.push(Line {
                layout,
                origin: Vector2 {
                    X: bounds.left,
                    Y: bounds.top
                        + index as f32 * style.line_height
                        + (style.line_height - ink_height) / 2.0
                        - top,
                },
            });
        }
        Ok(Layout { lines, metrics })
    }

    /// Convenience for callers that do not need to keep the shaped layout.
    pub fn draw(
        &self,
        target: &ID2D1RenderTarget,
        value: &str,
        bounds: D2D_RECT_F,
        style: Style,
        color: D2D1_COLOR_F,
    ) -> Result<Metrics> {
        let layout = self.layout(value, bounds, style)?;
        layout.draw(target, color)?;
        Ok(layout.metrics)
    }
}

impl Layout {
    /// Caller owns BeginDraw/EndDraw and must propagate EndDraw's deferred D2D
    /// errors. Caller clips must also allow for reported ink overflow.
    pub fn draw(&self, target: &ID2D1RenderTarget, color: D2D1_COLOR_F) -> Result<()> {
        let brush = unsafe { target.CreateSolidColorBrush(&color, None)? };
        for line in &self.lines {
            unsafe {
                target.DrawTextLayout(
                    line.origin,
                    &line.layout,
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
        }
        Ok(())
    }
}

fn measure(layout: &IDWriteTextLayout) -> Result<DWRITE_TEXT_METRICS> {
    let mut metrics = DWRITE_TEXT_METRICS::default();
    unsafe {
        layout.GetMetrics(&mut metrics)?;
    }
    Ok(metrics)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn choose_family(
    collection: &IDWriteFontCollection,
    candidates: &[&'static str],
) -> Result<&'static str> {
    for &candidate in candidates {
        let name = wide(candidate);
        let mut index = 0;
        let mut exists = windows::core::BOOL::default();
        unsafe {
            collection.FindFamilyName(PCWSTR(name.as_ptr()), &mut index, &mut exists)?;
        }
        if exists.as_bool() {
            return Ok(candidate);
        }
    }
    Err(Error::from_hresult(E_FAIL))
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    fn style(wrap: Wrap) -> Style {
        Style {
            face: Face::Text,
            size: 13.0,
            weight: DWRITE_FONT_WEIGHT_SEMI_BOLD,
            line_height: 24.0,
            align: Align::Leading,
            wrap,
        }
    }
    fn rect(width: f32, height: f32) -> D2D_RECT_F {
        D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: width,
            bottom: height,
        }
    }

    // Authored for coordinator integration; UNRUN in the bounded R5 sidecar.
    #[test]
    fn measured_korean_wraps_only_at_spaces_and_preserves_newlines() -> Result<()> {
        let text = Text::new()?;
        let s = style(Wrap::KeepAll);
        let format = text.format(s)?;
        let width = text.advance("계정 상태", &format)? + 1.0;
        let layout = text.layout("계정 상태 확인\n\n다음", rect(width, 240.0), s)?;
        assert_eq!(layout.metrics.total_lines, 4);
        assert_eq!(layout.metrics.required_height, 96.0);
        let long_word = text.layout("가나다라마바사아자차카타파하", rect(20.0, 24.0), s)?;
        assert_eq!(long_word.metrics.total_lines, 1);
        assert!(long_word.metrics.trimmed);
        Ok(())
    }

    #[test]
    fn measured_title_trims_and_height_bounds_omit_whole_lines() -> Result<()> {
        let text = Text::new()?;
        let title = "매우 긴 계정 이름과 요금제 이름 ".repeat(12);
        let layout = text.layout(&title, rect(90.0, 24.0), style(Wrap::SingleLine))?;
        assert!(layout.metrics.natural_width > 90.0);
        assert!(layout.metrics.trimmed);
        assert!(layout.metrics.width <= 90.0);
        let limited = text.layout(
            "첫 줄\n둘째 줄\n셋째 줄",
            rect(180.0, 49.0),
            style(Wrap::KeepAll),
        )?;
        assert_eq!(limited.metrics.total_lines, 3);
        assert_eq!(limited.metrics.visible_lines, 2);
        assert_eq!(limited.metrics.height, 48.0);
        assert!(limited.metrics.trimmed);
        let empty = text.layout("한글", rect(180.0, 0.0), style(Wrap::SingleLine))?;
        assert_eq!(empty.metrics.visible_lines, 0);
        assert!(
            text.layout("한글", rect(0.0, 24.0), style(Wrap::SingleLine))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn hangul_ink_is_centered_and_trailing_alignment_uses_real_metrics() -> Result<()> {
        let text = Text::new()?;
        let mut s = style(Wrap::SingleLine);
        s.align = Align::Trailing;
        let layout = text.layout("한글 받침 38%", rect(240.0, 24.0), s)?;
        assert_eq!(layout.lines.len(), 1);
        let line = &layout.lines[0];
        let m = measure(&line.layout)?;
        let o = unsafe { line.layout.GetOverhangMetrics()? };
        let top = line.origin.Y + m.top.min(-o.top);
        let bottom = line.origin.Y + (m.top + m.height).max(m.layoutHeight + o.bottom);
        assert!((top + bottom - 24.0).abs() < 0.1);
        if !layout.metrics.ink_overflow {
            assert!(top >= -0.01 && bottom <= 24.01);
        }
        assert!(m.left > 0.0);
        assert!((m.left + m.width - 240.0).abs() < 0.1);
        let format = text.format(s)?;
        assert!((text.advance("1111", &format)? - text.advance("8888", &format)?).abs() < 0.1);
        Ok(())
    }
}
