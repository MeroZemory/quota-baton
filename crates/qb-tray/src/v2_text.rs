//! DirectWrite text shared by native and WIC D2D targets.
//! Mutable layouts are independent; plain drawing and metrics use bounded caches.
//! The native engine lasts one visible session; WIC uses one engine per render.
#![cfg(target_os = "windows")]

#[cfg(any(debug_assertions, feature = "testkit"))]
use std::{cell::Cell, time::Instant};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Face {
    Text,
    Icon,
    Mono,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Align {
    Leading,
    Center,
    Trailing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

const FORMAT_CACHE_CAPACITY: usize = 64;
const LAYOUT_CACHE_CAPACITY: usize = 1_024;
const LAYOUT_CACHE_LINE_LIMIT: usize = 2_048;
const LAYOUT_CACHE_TEXT_LIMIT: usize = 1024 * 1024;
const LAYOUT_CACHE_INPUT_LIMIT: usize = 16 * 1024;
const LAYOUT_CACHE_ENTRY_LINE_LIMIT: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct StyleKey {
    face: Face,
    size: u32,
    weight: i32,
    line_height: u32,
    align: Align,
    wrap: Wrap,
}

impl From<Style> for StyleKey {
    fn from(style: Style) -> Self {
        Self {
            face: style.face,
            size: style.size.to_bits(),
            weight: style.weight.0,
            line_height: style.line_height.to_bits(),
            align: style.align,
            wrap: style.wrap,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LayoutKey {
    text: String,
    style: StyleKey,
    width: u32,
    height: u32,
}

impl LayoutKey {
    fn new(value: &str, width: f32, height: f32, style: Style) -> Self {
        Self {
            text: value.to_owned(),
            style: style.into(),
            width: width.to_bits(),
            height: height.to_bits(),
        }
    }
}

struct Cached<T> {
    value: T,
    used: u64,
}

#[derive(Default)]
struct TextCache {
    formats: BTreeMap<StyleKey, Cached<IDWriteTextFormat>>,
    // Only draw/measure can obtain these shared, immutable layouts.
    layouts: BTreeMap<LayoutKey, Cached<Rc<Layout>>>,
    clock: u64,
    line_count: usize,
    text_bytes: usize,
}

impl TextCache {
    fn tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    fn format(&mut self, key: &StyleKey) -> Option<IDWriteTextFormat> {
        let used = self.tick();
        self.formats.get_mut(key).map(|entry| {
            entry.used = used;
            entry.value.clone()
        })
    }

    fn retain_format(&mut self, key: StyleKey, format: IDWriteTextFormat) {
        self.formats.remove(&key);
        if self.formats.len() >= FORMAT_CACHE_CAPACITY {
            let oldest = self
                .formats
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| *key)
                .expect("nonempty format cache");
            self.formats.remove(&oldest);
        }
        let used = self.tick();
        self.formats.insert(
            key,
            Cached {
                value: format,
                used,
            },
        );
    }

    fn layout(&mut self, key: &LayoutKey) -> Option<Rc<Layout>> {
        let used = self.tick();
        self.layouts.get_mut(key).map(|entry| {
            entry.used = used;
            Rc::clone(&entry.value)
        })
    }

    fn remove_layout(&mut self, key: &LayoutKey) {
        if let Some((key, entry)) = self.layouts.remove_entry(key) {
            self.line_count -= entry.value.lines.len();
            self.text_bytes -= retained_text_bytes(&key, &entry.value);
        }
    }

    fn retain_layout(&mut self, key: LayoutKey, layout: Rc<Layout>) {
        let lines = layout.lines.len();
        let bytes = retained_text_bytes(&key, &layout);
        if key.text.len() > LAYOUT_CACHE_INPUT_LIMIT
            || lines > LAYOUT_CACHE_ENTRY_LINE_LIMIT
            || bytes > LAYOUT_CACHE_TEXT_LIMIT
        {
            return;
        }
        self.remove_layout(&key);
        while self.layouts.len() >= LAYOUT_CACHE_CAPACITY
            || self.line_count + lines > LAYOUT_CACHE_LINE_LIMIT
            || self.text_bytes + bytes > LAYOUT_CACHE_TEXT_LIMIT
        {
            let oldest = self
                .layouts
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
                .expect("nonempty layout cache");
            self.remove_layout(&oldest);
        }
        let used = self.tick();
        self.line_count += lines;
        self.text_bytes += bytes;
        self.layouts.insert(
            key,
            Cached {
                value: layout,
                used,
            },
        );
    }
}

fn retained_text_bytes(key: &LayoutKey, layout: &Layout) -> usize {
    layout
        .lines
        .iter()
        .fold(key.text.capacity(), |bytes, line| {
            bytes.saturating_add(line.text.capacity())
        })
}

/// An isolated factory owned by its renderer, never a global shared font cache.
pub struct Text {
    factory: IDWriteFactory,
    text_family: &'static str,
    icon_family: &'static str,
    mono_family: &'static str,
    cache: RefCell<TextCache>,
    #[cfg(any(debug_assertions, feature = "testkit"))]
    phases: TextPhaseCounters,
}

/// Per-engine elapsed wall durations since reset, not process CPU time.
/// Creation counts include failed COM attempts.
/// Plain release builds expose the same API and return zero totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TextPhaseStats {
    /// Elapsed wall time in layout/measure/draw preparation, including cache lookup.
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
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

/// Independently mutable shaped layouts returned by Text::layout.
/// Shared plain layouts stay private to Text's draw/measure cache.
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
            cache: RefCell::new(TextCache::default()),
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
        let key = style.into();
        if let Some(format) = self.cache.borrow_mut().format(&key) {
            return Ok(format);
        }
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
            self.cache.borrow_mut().retain_format(key, format.clone());
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
        self.build_layout(value, bounds, style)
    }

    fn build_layout(&self, value: &str, bounds: D2D_RECT_F, style: Style) -> Result<Layout> {
        let (width, height) = dimensions(bounds, style)?;
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

    // Build at a relative origin. Never expose shared COM layouts to emphasize.
    fn cached_layout(&self, value: &str, bounds: D2D_RECT_F, style: Style) -> Result<Rc<Layout>> {
        #[cfg(any(debug_assertions, feature = "testkit"))]
        let _measurement = PhaseTimer::new(self.phases.enabled.get(), &self.phases.text_measure_ns);
        let (width, height) = dimensions(bounds, style)?;
        let relative = D2D_RECT_F {
            left: 0.,
            top: 0.,
            right: width,
            bottom: height,
        };
        // Oversized input still renders normally without retaining a key copy.
        if value.len() > LAYOUT_CACHE_INPUT_LIMIT {
            return self.build_layout(value, relative, style).map(Rc::new);
        }
        let key = LayoutKey::new(value, width, height, style);
        if let Some(layout) = self.cache.borrow_mut().layout(&key) {
            return Ok(layout);
        }
        // The lookup borrow is gone before shaping consults the format cache.
        let layout = Rc::new(self.build_layout(value, relative, style)?);
        self.cache
            .borrow_mut()
            .retain_layout(key, Rc::clone(&layout));
        Ok(layout)
    }

    /// Cached plain metrics without exposing mutable shared DirectWrite objects.
    pub fn measure(&self, value: &str, bounds: D2D_RECT_F, style: Style) -> Result<Metrics> {
        Ok(self.cached_layout(value, bounds, style)?.metrics)
    }

    /// Draw a cached plain layout at the current bounds' origin.
    pub fn draw(
        &self,
        target: &ID2D1RenderTarget,
        value: &str,
        bounds: D2D_RECT_F,
        style: Style,
        color: D2D1_COLOR_F,
    ) -> Result<Metrics> {
        let layout = self.cached_layout(value, bounds, style)?;
        layout.draw_at(
            target,
            color,
            Vector2 {
                X: bounds.left,
                Y: bounds.top,
            },
        )?;
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
        self.draw_at(target, color, Vector2 { X: 0., Y: 0. })
    }

    fn draw_at(
        &self,
        target: &ID2D1RenderTarget,
        color: D2D1_COLOR_F,
        origin: Vector2,
    ) -> Result<()> {
        let brush = unsafe { target.CreateSolidColorBrush(&color, None)? };
        for line in &self.lines {
            unsafe {
                target.DrawTextLayout(
                    line.at(origin),
                    &line.layout,
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
        }
        Ok(())
    }
}

impl Line {
    fn at(&self, origin: Vector2) -> Vector2 {
        Vector2 {
            X: self.origin.X + origin.X,
            Y: self.origin.Y + origin.Y,
        }
    }
}

fn dimensions(bounds: D2D_RECT_F, style: Style) -> Result<(f32, f32)> {
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
    Ok((width, height))
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
            // Profiling reset preserves the format warmed while disabled.
            assert_eq!(stats.format_creations, 0);
            // Natural advance, line-height probe, and final shaped line.
            assert_eq!(stats.layout_creations, 3);
        }

        text.reset_phase_stats();
        text.layout(
            "new style",
            rect(240., 24.),
            Style {
                size: 14.,
                ..style(Wrap::SingleLine)
            },
        )?;
        assert_eq!(text.phase_stats().format_creations, 1);
        assert_eq!(text.phase_stats().layout_creations, 3);

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

    #[test]
    fn r1210_plain_cache_reuses_shaping_and_translates_hangul_origins() -> Result<()> {
        let text = Text::new()?;
        text.set_phase_stats_enabled(true);
        let value = "계정 상태 😀 38% 남음\n다음 줄";
        for align in [Align::Leading, Align::Center, Align::Trailing] {
            let s = Style {
                align,
                ..style(Wrap::KeepAll)
            };
            let initial = D2D_RECT_F {
                left: 24.,
                top: 18.,
                right: 264.,
                bottom: 66.,
            };
            let cached = text.cached_layout(value, initial, s)?;
            let fresh = text.layout(value, initial, s)?;
            assert_eq!(cached.metrics, fresh.metrics);
            for origin in [Vector2 { X: 47., Y: 91. }, Vector2 { X: -24., Y: -96. }] {
                let shifted = D2D_RECT_F {
                    left: origin.X,
                    top: origin.Y,
                    right: origin.X + 240.,
                    bottom: origin.Y + 48.,
                };
                text.reset_phase_stats();
                let hit = text.cached_layout(value, shifted, s)?;
                assert!(Rc::ptr_eq(&cached, &hit));
                assert_eq!(text.measure(value, shifted, s)?, cached.metrics);
                assert_eq!(text.phase_stats().format_creations, 0);
                assert_eq!(text.phase_stats().layout_creations, 0);
                let fresh = text.layout(value, shifted, s)?;
                assert_eq!(hit.metrics, fresh.metrics);
                for (line, fresh) in hit.lines.iter().zip(&fresh.lines) {
                    let actual = line.at(origin);
                    assert!((actual.X - fresh.origin.X).abs() < 0.001);
                    assert!((actual.Y - fresh.origin.Y).abs() < 0.001);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn r1210_plain_cache_keys_all_style_fields_and_both_dimensions() -> Result<()> {
        let text = Text::new()?;
        let value = "계정 상태\n다음 줄\n셋째 줄";
        let s = style(Wrap::KeepAll);
        let bounds = rect(240., 48.);
        let base = text.cached_layout(value, bounds, s)?;
        for variant in [
            Style {
                face: Face::Mono,
                ..s
            },
            Style {
                face: Face::Icon,
                ..s
            },
            Style { size: 14., ..s },
            Style {
                weight: DWRITE_FONT_WEIGHT_NORMAL,
                ..s
            },
            Style {
                line_height: 25.,
                ..s
            },
            Style {
                align: Align::Center,
                ..s
            },
            Style {
                align: Align::Trailing,
                ..s
            },
            Style {
                wrap: Wrap::SingleLine,
                ..s
            },
        ] {
            let changed = text.cached_layout(value, bounds, variant)?;
            assert!(!Rc::ptr_eq(&base, &changed));
            assert_eq!(
                changed.metrics,
                text.layout(value, bounds, variant)?.metrics
            );
        }
        for bounds in [rect(241., 48.), rect(240., 24.), rect(240., 0.)] {
            let changed = text.cached_layout(value, bounds, s)?;
            assert!(!Rc::ptr_eq(&base, &changed));
            assert_eq!(changed.metrics, text.layout(value, bounds, s)?.metrics);
        }
        assert_eq!(base.metrics.visible_lines, 2);
        assert!(base.metrics.trimmed);
        assert_eq!(text.measure(value, rect(240., 0.), s)?.visible_lines, 0);
        let changed = text.cached_layout("different", bounds, s)?;
        assert!(!Rc::ptr_eq(&base, &changed));
        let entries = text.cache.borrow().layouts.len();
        for invalid in [
            rect(0., 48.),
            rect(240., -1.),
            D2D_RECT_F {
                left: f32::NAN,
                ..bounds
            },
        ] {
            assert!(text.measure(value, invalid, s).is_err());
        }
        assert!(
            text.measure(value, bounds, Style { size: 0., ..s })
                .is_err()
        );
        assert!(
            text.measure(
                value,
                bounds,
                Style {
                    line_height: f32::INFINITY,
                    ..s
                }
            )
            .is_err()
        );
        assert_eq!(text.cache.borrow().layouts.len(), entries);
        Ok(())
    }

    #[test]
    fn r1210_mutable_emphasis_cannot_change_cached_or_other_private_layouts() -> Result<()> {
        let text = Text::new()?;
        let value = "계정 😀 38% 남음";
        let s = Style {
            weight: DWRITE_FONT_WEIGHT_NORMAL,
            ..style(Wrap::SingleLine)
        };
        let bounds = rect(400., 24.);
        let cached = text.cached_layout(value, bounds, s)?;
        let mut private = text.layout(value, bounds, s)?;
        private.emphasize(&["38% 남음"])?;
        private.emphasize(&["계정"])?;
        let fresh = text.layout(value, bounds, s)?;
        let offset = "계정 😀 ".encode_utf16().count() as u32;
        for position in [0, offset] {
            for (layout, expected) in [
                (&*cached, DWRITE_FONT_WEIGHT_NORMAL),
                (&fresh, DWRITE_FONT_WEIGHT_NORMAL),
                (&private, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            ] {
                let mut actual = DWRITE_FONT_WEIGHT_NORMAL;
                unsafe {
                    layout.lines[0]
                        .layout
                        .GetFontWeight(position, &mut actual, None)?;
                }
                assert_eq!(actual, expected);
            }
        }
        assert!(Rc::ptr_eq(&cached, &text.cached_layout(value, bounds, s)?));
        Ok(())
    }

    #[test]
    fn r1210_cache_retention_bounds_entries_lines_bytes_and_formats() -> Result<()> {
        let text = Text::new()?;
        let s = style(Wrap::SingleLine);
        for index in 0..=FORMAT_CACHE_CAPACITY {
            text.format(Style {
                size: 10. + index as f32,
                ..s
            })?;
        }
        assert_eq!(text.cache.borrow().formats.len(), FORMAT_CACHE_CAPACITY);
        let single = Rc::new(text.layout("x", rect(240., 24.), s)?);
        let key = |value: &str| LayoutKey::new(value, 240., 24., s);
        // Exercise retention policy without shaping thousands of fixture lines.
        let mut cache = TextCache::default();
        for index in 0..=LAYOUT_CACHE_CAPACITY {
            cache.retain_layout(key(&format!("entry-{index}")), Rc::clone(&single));
        }
        assert_eq!(cache.layouts.len(), LAYOUT_CACHE_CAPACITY);
        assert!(cache.layout(&key("entry-0")).is_none());
        assert!(cache.layout(&key("entry-1")).is_some());
        cache.retain_layout(key("extra"), Rc::clone(&single));
        assert!(cache.layout(&key("entry-1")).is_some());
        assert!(cache.layout(&key("entry-2")).is_none());

        let body = (0..32).map(|_| "계정").collect::<Vec<_>>().join("\n");
        let tall = Rc::new(text.layout(&body, rect(240., 32. * 24.), style(Wrap::KeepAll))?);
        assert_eq!(tall.lines.len(), 32);
        let mut cache = TextCache::default();
        for index in 0..=LAYOUT_CACHE_LINE_LIMIT / tall.lines.len() {
            cache.retain_layout(key(&format!("line-{index}")), Rc::clone(&tall));
        }
        assert_eq!(cache.line_count, LAYOUT_CACHE_LINE_LIMIT);
        assert!(cache.layout(&key("line-0")).is_none());

        let mut cache = TextCache::default();
        for index in 0..=LAYOUT_CACHE_TEXT_LIMIT / LAYOUT_CACHE_INPUT_LIMIT {
            let value = format!("{index:04}{}", "x".repeat(LAYOUT_CACHE_INPUT_LIMIT - 4));
            cache.retain_layout(key(&value), Rc::clone(&single));
        }
        assert!(cache.layouts.len() < LAYOUT_CACHE_TEXT_LIMIT / LAYOUT_CACHE_INPUT_LIMIT);
        assert!(cache.text_bytes <= LAYOUT_CACHE_TEXT_LIMIT);
        assert_eq!(cache.line_count, cache.layouts.len());
        assert_eq!(
            cache.text_bytes,
            cache
                .layouts
                .iter()
                .map(|(key, entry)| retained_text_bytes(key, &entry.value))
                .sum::<usize>()
        );

        let oversized = "x".repeat(LAYOUT_CACHE_INPUT_LIMIT + 1);
        let before = text.cache.borrow().layouts.len();
        assert_eq!(
            text.measure(&oversized, rect(240., 24.), s)?,
            text.layout(&oversized, rect(240., 24.), s)?.metrics
        );
        assert_eq!(text.cache.borrow().layouts.len(), before);
        let many = (0..=LAYOUT_CACHE_ENTRY_LINE_LIMIT)
            .map(|_| "계정")
            .collect::<Vec<_>>()
            .join("\n");
        let bounds = rect(240., 24. * (LAYOUT_CACHE_ENTRY_LINE_LIMIT + 1) as f32);
        assert_eq!(
            text.measure(&many, bounds, style(Wrap::KeepAll))?,
            text.layout(&many, bounds, style(Wrap::KeepAll))?.metrics
        );
        assert_eq!(text.cache.borrow().layouts.len(), before);
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
