//! DirectWrite text shared by native and WIC D2D targets.
//! Layouts last one paint pass. The native text engine lasts one visible session
//! and is released on hide; WIC uses one engine per render.
#![cfg(target_os = "windows")]

#[cfg(any(debug_assertions, feature = "testkit"))]
use std::{cell::Cell, time::Instant};
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
    Center,
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

/// An isolated factory owned by its renderer, never a global shared font cache.
pub struct Text {
    factory: IDWriteFactory,
    text_family: &'static str,
    icon_family: &'static str,
    mono_family: &'static str,
    #[cfg(any(debug_assertions, feature = "testkit"))]
    phases: TextPhaseCounters,
}

/// Per-engine elapsed wall durations since reset, not process CPU time.
/// Creation counts include failed COM attempts.
/// Plain release builds expose the same API and return zero totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TextPhaseStats {
    /// Elapsed wall time in layout(), including shaping, metrics and wrapping.
    pub text_measure_us: u64,
    pub layout_creations: u64,
    pub format_creations: u64,
}

#[cfg(any(debug_assertions, feature = "testkit"))]
#[derive(Default)]
struct TextPhaseCounters {
    enabled: Cell<bool>,
    text_measure_ns: Cell<u64>,
    layout_creations: Cell<u64>,
    format_creations: Cell<u64>,
}

#[cfg(any(debug_assertions, feature = "testkit"))]
pub(crate) fn phase_elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

/// Includes early-error paths without reading the clock when disabled.
#[cfg(any(debug_assertions, feature = "testkit"))]
pub(crate) struct PhaseTimer<'a> {
    started: Option<Instant>,
    total: &'a Cell<u64>,
}

#[cfg(any(debug_assertions, feature = "testkit"))]
impl<'a> PhaseTimer<'a> {
    pub(crate) fn new(enabled: bool, total: &'a Cell<u64>) -> Self {
        Self {
            started: enabled.then(Instant::now),
            total,
        }
    }
}

#[cfg(any(debug_assertions, feature = "testkit"))]
impl Drop for PhaseTimer<'_> {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            self.total
                .set(self.total.get().saturating_add(phase_elapsed_ns(started)));
        }
    }
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
    text: String,
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
            #[cfg(any(debug_assertions, feature = "testkit"))]
            phases: TextPhaseCounters::default(),
        })
    }

    /// Default off. Disabling retains totals; reset does not change this flag.
    pub fn set_phase_stats_enabled(&self, enabled: bool) {
        #[cfg(any(debug_assertions, feature = "testkit"))]
        self.phases.enabled.set(enabled);
        #[cfg(not(any(debug_assertions, feature = "testkit")))]
        let _ = enabled;
    }

    pub fn reset_phase_stats(&self) {
        #[cfg(any(debug_assertions, feature = "testkit"))]
        {
            self.phases.text_measure_ns.set(0);
            self.phases.layout_creations.set(0);
            self.phases.format_creations.set(0);
        }
    }

    pub fn phase_stats(&self) -> TextPhaseStats {
        #[cfg(any(debug_assertions, feature = "testkit"))]
        {
            TextPhaseStats {
                text_measure_us: self.phases.text_measure_ns.get() / 1_000,
                layout_creations: self.phases.layout_creations.get(),
                format_creations: self.phases.format_creations.get(),
            }
        }
        #[cfg(not(any(debug_assertions, feature = "testkit")))]
        TextPhaseStats::default()
    }

    #[cfg(any(debug_assertions, feature = "testkit"))]
    pub(crate) fn phase_measure_ns(&self) -> u64 {
        self.phases.text_measure_ns.get()
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
            #[cfg(any(debug_assertions, feature = "testkit"))]
            if self.phases.enabled.get() {
                self.phases
                    .format_creations
                    .set(self.phases.format_creations.get().saturating_add(1));
            }
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
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
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
            #[cfg(any(debug_assertions, feature = "testkit"))]
            if self.phases.enabled.get() {
                self.phases
                    .layout_creations
                    .set(self.phases.layout_creations.get().saturating_add(1));
            }
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
        #[cfg(any(debug_assertions, feature = "testkit"))]
        let _measurement = PhaseTimer::new(self.phases.enabled.get(), &self.phases.text_measure_ns);
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
                text: displayed,
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
    /// Emphasize shaped values using UTF-16 ranges, including after Korean wraps.
    pub fn emphasize(&mut self, values: &[&str]) -> Result<()> {
        for line in &self.lines {
            for value in values.iter().filter(|value| !value.is_empty()) {
                for (start, _) in line.text.match_indices(*value) {
                    let start = u32::try_from(line.text[..start].encode_utf16().count())
                        .map_err(|_| Error::from_hresult(E_INVALIDARG))?;
                    let length = u32::try_from(value.encode_utf16().count())
                        .map_err(|_| Error::from_hresult(E_INVALIDARG))?;
                    unsafe {
                        line.layout.SetFontWeight(
                            DWRITE_FONT_WEIGHT_SEMI_BOLD,
                            DWRITE_TEXT_RANGE {
                                startPosition: start,
                                length,
                            },
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
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

    #[cfg(any(debug_assertions, feature = "testkit"))]
    #[test]
    fn r1210_phase_stats_count_actual_calls_and_preserve_enable_reset_semantics() -> Result<()> {
        let text = Text::new()?;
        text.layout("disabled", rect(240., 24.), style(Wrap::SingleLine))?;
        assert_eq!(text.phase_stats(), TextPhaseStats::default());

        text.set_phase_stats_enabled(true);
        for value in ["enabled before reset", "enabled after reset"] {
            text.reset_phase_stats();
            assert_eq!(text.phase_stats(), TextPhaseStats::default());
            text.layout(value, rect(240., 24.), style(Wrap::SingleLine))?;
            let stats = text.phase_stats();
            assert_eq!(stats.format_creations, 1);
            // Natural advance, line-height probe, and final shaped line.
            assert_eq!(stats.layout_creations, 3);
        }

        let before = text.phase_stats();
        text.set_phase_stats_enabled(false);
        text.layout("disabled again", rect(240., 24.), style(Wrap::SingleLine))?;
        assert_eq!(text.phase_stats(), before);
        text.reset_phase_stats();
        text.layout(
            "reset while disabled",
            rect(240., 24.),
            style(Wrap::SingleLine),
        )?;
        assert_eq!(text.phase_stats(), TextPhaseStats::default());

        text.set_phase_stats_enabled(true);
        assert!(
            text.layout("invalid", rect(0., 24.), style(Wrap::SingleLine))
                .is_err()
        );
        assert_eq!(text.phase_stats().format_creations, 0);
        assert_eq!(text.phase_stats().layout_creations, 0);
        Ok(())
    }

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
    fn r7_center_alignment_keeps_full_width_and_centers_each_shaped_line() -> Result<()> {
        let text = Text::new()?;
        let bounds = rect(320., 200.);
        let mut s = style(Wrap::KeepAll);
        let leading = text.layout("한글 빈 상태 안내\ncodex-qb claude-qb", bounds, s)?;
        s.align = Align::Center;
        let centered = text.layout("한글 빈 상태 안내\ncodex-qb claude-qb", bounds, s)?;
        assert_eq!(leading.metrics.total_lines, centered.metrics.total_lines);
        assert!((leading.metrics.natural_width - centered.metrics.natural_width).abs() < 0.1);
        assert!(!centered.metrics.trimmed);
        for line in &centered.lines {
            let metrics = measure(&line.layout)?;
            assert!(
                (metrics.left + metrics.widthIncludingTrailingWhitespace / 2. - 160.).abs() < 1.
            );
        }
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
    #[test]
    fn emphasis_ranges_use_utf16_after_korean_and_surrogate_pairs() -> Result<()> {
        let text = Text::new()?;
        let base = Style {
            weight: DWRITE_FONT_WEIGHT_NORMAL,
            ..style(Wrap::SingleLine)
        };
        let mut layout = text.layout("계정 😀 38% 남음", rect(400., 24.), base)?;
        layout.emphasize(&["38% 남음"])?;
        let mut weight = DWRITE_FONT_WEIGHT_NORMAL;
        unsafe {
            layout.lines[0].layout.GetFontWeight(0, &mut weight, None)?;
        }
        assert_eq!(weight, DWRITE_FONT_WEIGHT_NORMAL);
        let offset = "계정 😀 ".encode_utf16().count() as u32;
        unsafe {
            layout.lines[0]
                .layout
                .GetFontWeight(offset, &mut weight, None)?;
        }
        assert_eq!(weight, DWRITE_FONT_WEIGHT_SEMI_BOLD);
        Ok(())
    }
}
