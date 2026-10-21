//! Shared v2 Direct2D/DirectWrite renderer adapter; no HWND, GDI or I/O.
//! Optional debug/testkit phase profiling is off by default.
#[cfg(any(debug_assertions, feature = "testkit"))]
use crate::v2_text::{PhaseTimer, phase_elapsed_ns};
use crate::{
    dashboard_model::DashboardView,
    geometry::*,
    state::{
        DashboardState, EventFilter, Popup, ProviderFilter, Range, ScrollRegion, SortKey, Tab,
    },
    v2_colors::{Palette, Rgb},
    v2_render, v2_text,
};
#[cfg(any(debug_assertions, feature = "testkit"))]
use std::{cell::Cell, time::Instant};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::{Deref, DerefMut},
    sync::Arc,
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
    paint_text_proofs: RefCell<PaintTextProofs>,
    scene_metrics: RefCell<SceneMetricCache>,
    #[cfg(test)]
    scene_builds: std::cell::Cell<usize>,
    #[cfg(any(debug_assertions, feature = "testkit"))]
    phases: RendererPhaseCounters,
}

/// A completed scene plus section spans and measurements for the same immutable
/// projection. The Host keeps this value until geometry or data is invalidated.
#[derive(Clone, Debug)]
pub struct CachedScene {
    scene: Scene,
    view: Arc<DashboardView>,
    inputs: ScrollInputs,
    sections: SceneSections,
    metrics: Metrics,
    rebuilt_region: Option<ScrollRegion>,
}
impl CachedScene {
    fn new(
        scene: Scene,
        view: &Arc<DashboardView>,
        inputs: ScrollInputs,
        sections: SceneSections,
        mut metrics: Metrics,
        rebuilt_region: Option<ScrollRegion>,
    ) -> Self {
        // Bound even unusual floating-point measurement variants by this frame's
        // working set, rather than retaining a history of earlier scroll keys.
        if metrics.heights.len() > scene.measurements.len()
            || metrics.widths.len() > scene.measurements.len()
        {
            let keys: std::collections::BTreeSet<_> =
                scene.measurements.iter().map(Measure::key).collect();
            metrics.heights.retain(|key, _| keys.contains(key));
            metrics.widths.retain(|key, _| keys.contains(key));
        }
        Self {
            scene,
            view: view.clone(),
            inputs,
            sections,
            metrics,
            rebuilt_region,
        }
    }
    pub fn as_scene(&self) -> &Scene {
        &self.scene
    }
    /// None means the entire scene was rebuilt, including a safe fallback.
    pub fn rebuilt_region(&self) -> Option<ScrollRegion> {
        self.rebuilt_region
    }

    /// Damage available before rebuilding a Table scroll. This is only a
    /// proposed invalidation; validate the completed rebuild with
    /// `scroll_damage_from` before relying on retained pixels during painting.
    pub fn table_scroll_damage(&self) -> Option<crate::Rect> {
        if self.modal || self.inputs.popup.is_some() {
            return None;
        }
        let mut tables = self
            .content
            .iter()
            .filter(|r| r.region == ScrollRegion::Table);
        tables.next()?;
        if tables.next().is_some() {
            return None;
        }
        let (old, new) = self.sections.table_cards_for_damage(&self.sections)?;
        for r in [old, new, self.rect] {
            if ![r.left, r.top, r.right, r.bottom]
                .iter()
                .all(|n| n.is_finite())
                || r.right <= r.left
                || r.bottom <= r.top
            {
                return None;
            }
        }
        crate::v2_geometry::intersection(
            crate::Rect {
                left: old.left.min(new.left) - 4.,
                top: old.top.min(new.top) - 4.,
                right: old.right.max(new.right) + 4.,
                bottom: old.bottom.max(new.bottom) + 4.,
            },
            self.rect,
        )
    }

    /// Conservative damage for a Table-only rebuild of the same projection.
    /// The Host must separately validate retained target pixels/metrics and union
    /// this with all pending damage. Replay the entire composed scene in the clip.
    pub fn scroll_damage_from(&self, previous: &Self) -> Option<crate::Rect> {
        if self.rebuilt_region != Some(ScrollRegion::Table)
            || !Arc::ptr_eq(&self.view, &previous.view)
            || self.inputs != previous.inputs
            || self.inputs.popup.is_some()
            || self.modal
            || previous.modal
            || self.rect != previous.rect
            || self.content.len() != previous.content.len()
        {
            return None;
        }
        // A caller may supply an older completed frame. Only Table's offset
        // may differ; another region's damage cannot be certified by this card.
        let mut tables = 0;
        for (new, old) in self.content.iter().zip(&previous.content) {
            if new.region != old.region
                || new.viewport != old.viewport
                || new.content_height != old.content_height
                || new.max_offset != old.max_offset
            {
                return None;
            }
            if new.region == ScrollRegion::Table {
                tables += 1;
            } else if new.offset != old.offset {
                return None;
            }
        }
        if tables != 1 {
            return None;
        }
        self.sections.table_cards_for_damage(&previous.sections)?;
        self.table_scroll_damage()
    }
}
impl Deref for CachedScene {
    type Target = Scene;
    fn deref(&self) -> &Self::Target {
        &self.scene
    }
}
impl DerefMut for CachedScene {
    // The Host may add and remove temporary editor decorations while painting.
    // Permanent scene changes must use scene_shared rather than mutate this cache.
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.scene
    }
}
#[derive(Clone, Debug, PartialEq)]
struct ScrollInputs {
    size: [f32; 2],
    tab: Tab,
    popup: Option<Popup>,
    provider: ProviderFilter,
    range: Range,
    grouped: bool,
    search: String,
    sort: SortKey,
    descending: bool,
    event_filter: EventFilter,
    event_days: u8,
    event_search: String,
}
impl ScrollInputs {
    fn new(state: &DashboardState, size: [f32; 2]) -> Self {
        Self {
            size,
            tab: state.tab,
            popup: state.popup,
            provider: state.provider,
            range: state.range,
            grouped: state.grouped,
            search: state.search.clone(),
            sort: state.sort,
            descending: state.descending,
            event_filter: state.event_filter,
            event_days: state.event_days,
            event_search: state.event_search.clone(),
        }
    }
}
fn scroll_offset(state: &DashboardState, region: ScrollRegion) -> f32 {
    match region {
        ScrollRegion::Table => state.table_offset,
        ScrollRegion::Side => state.side_offset,
        ScrollRegion::Events => state.events_offset,
        ScrollRegion::Usage => state.usage_offset,
    }
}

/// Per-renderer elapsed wall durations since reset, not process CPU time.
/// Includes this renderer's text engine.
/// Add separately owned editor Text stats at the Host boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct PhaseStats {
    /// Scene/section rebuild passes, excluding text measurement between passes.
    pub scene_geometry_us: u64,
    pub text_measure_us: u64,
    /// BeginDraw through clip cleanup, excluding measured text layout work.
    pub draw_us: u64,
    /// EndDraw elapsed wall duration; may include waits/vsync. Not display completion.
    pub present_us: u64,
    pub layout_creations: u64,
    pub format_creations: u64,
}

#[cfg(any(debug_assertions, feature = "testkit"))]
#[derive(Default)]
struct RendererPhaseCounters {
    enabled: Cell<bool>,
    scene_geometry_ns: Cell<u64>,
    draw_ns: Cell<u64>,
    present_ns: Cell<u64>,
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

// Unknown bounds always draw. Text can overhang its requested box, so only a
// measured, contained layout may use that box for culling (see paint_clipped).
#[derive(Clone, Copy, Debug)]
enum PaintBounds {
    Unknown,
    Empty,
    Rect(crate::Rect),
}
impl PaintBounds {
    fn expanded(r: crate::Rect, margin: [f32; 2]) -> Self {
        let expanded = crate::Rect {
            left: r.left - margin[0],
            top: r.top - margin[1],
            right: r.right + margin[0],
            bottom: r.bottom + margin[1],
        };
        if ![r.left, r.top, r.right, r.bottom, margin[0], margin[1]]
            .iter()
            .all(|v| v.is_finite())
            || r.right < r.left
            || r.bottom < r.top
            || ![expanded.left, expanded.top, expanded.right, expanded.bottom]
                .iter()
                .all(|v| v.is_finite())
        {
            Self::Unknown
        } else {
            Self::Rect(expanded)
        }
    }
    fn intersect(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, _) | (_, Self::Empty) => Self::Empty,
            (Self::Unknown, other) | (other, Self::Unknown) => other,
            (Self::Rect(a), Self::Rect(b)) => {
                crate::v2_geometry::intersection(a, b).map_or(Self::Empty, Self::Rect)
            }
        }
    }
}

struct PaintClip {
    stack: Vec<PaintBounds>,
    aa: [f32; 2],
}
impl PaintClip {
    fn new(outer: crate::Rect, dpi: [f32; 2]) -> Self {
        // One physical pixel on EACH side of the intersection, also covering
        // DirectWrite's 0.01 DIP ink-containment tolerance. Invalid DPI disables
        // geometric culling instead of guessing a smaller antialias footprint.
        let aa = dpi.map(|v| {
            if v.is_finite() && v > 0. {
                96. / v + 0.01
            } else {
                f32::INFINITY
            }
        });
        Self {
            stack: vec![Self::clip_bounds(outer, aa)],
            aa,
        }
    }
    fn clip_bounds(r: crate::Rect, aa: [f32; 2]) -> PaintBounds {
        if [r.left, r.top, r.right, r.bottom]
            .iter()
            .all(|v| v.is_finite())
            && (r.right == r.left || r.bottom == r.top)
        {
            PaintBounds::Empty
        } else {
            PaintBounds::expanded(r, aa)
        }
    }
    fn push(&mut self, r: crate::Rect) {
        let next = self.current().intersect(Self::clip_bounds(r, self.aa));
        self.stack.push(next);
    }
    fn pop(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
    }
    fn current(&self) -> PaintBounds {
        *self.stack.last().expect("outer paint clip")
    }
    fn visible(&self, r: crate::Rect, outset: f32) -> bool {
        if !outset.is_finite() || outset < 0. {
            return true;
        }
        !matches!(
            self.current()
                .intersect(PaintBounds::expanded(r, self.aa.map(|aa| aa + outset))),
            PaintBounds::Empty
        )
    }
    fn keeps(&self, draw: &Draw) -> bool {
        match draw {
            Draw::Fill { rect, .. } => self.visible(*rect, 0.),
            Draw::Stroke { rect, width, .. } => self.visible(*rect, *width / 2.),
            Draw::Line {
                from, to, width, ..
            } => {
                if !from.iter().chain(to).all(|v| v.is_finite()) {
                    return true;
                }
                self.visible(
                    crate::Rect {
                        left: from[0].min(to[0]),
                        top: from[1].min(to[1]),
                        right: from[0].max(to[0]),
                        bottom: from[1].max(to[1]),
                    },
                    *width / 2.,
                )
            }
            Draw::Text { .. } => !matches!(self.current(), PaintBounds::Empty),
            // Always execute state transitions, even inside an empty clip.
            Draw::Clip(_) | Draw::Unclip | Draw::Alpha(_) => true,
        }
    }
}

const PAINT_TEXT_PROOF_CAPACITY: usize = 4_096;
const PAINT_TEXT_PROOF_BYTES: usize = 1024 * 1024;
#[derive(Default)]
struct PaintTextProofs {
    contained: BTreeSet<String>,
    bytes: usize,
}
impl PaintTextProofs {
    fn key(measure: &Measure, r: crate::Rect) -> String {
        format!(
            "{}:{}:{}",
            width(r).to_bits(),
            height(r).to_bits(),
            measure.key()
        )
    }
    fn remember(&mut self, key: String) {
        if key.len() > 16 * 1024 || self.contained.contains(&key) {
            return;
        }
        while self.contained.len() >= PAINT_TEXT_PROOF_CAPACITY
            || self.bytes + key.len() > PAINT_TEXT_PROOF_BYTES
        {
            let Some(old) = self.contained.pop_first() else {
                break;
            };
            self.bytes -= old.len();
        }
        self.bytes += key.len();
        self.contained.insert(key);
    }
}

// Keep measurements, never scene geometry, across full builds. The Text engine's
// fonts live for this Renderer; Measure::key includes every measurement input.
// FIFO eviction only causes a normal probe miss. Count BOTH retained key copies.
const SCENE_METRIC_CAPACITY: usize = 4_096;
const SCENE_METRIC_KEY_BYTES: usize = 2 * 1024 * 1024;
#[derive(Default)]
struct SceneMetricCache {
    entries: BTreeMap<String, [f32; 2]>,
    order: VecDeque<String>,
    bytes: usize,
}
impl SceneMetricCache {
    fn snapshot(&self) -> Metrics {
        let mut metrics = Metrics::default();
        for (key, [height, width]) in &self.entries {
            metrics.heights.insert(key.clone(), *height);
            metrics.widths.insert(key.clone(), *width);
        }
        metrics
    }
    fn remember(&mut self, requests: &[Measure], metrics: &Metrics) {
        for request in requests {
            let key = request.key();
            if key.len() > 16 * 1024 || self.entries.contains_key(&key) {
                continue;
            }
            let (Some(&height), Some(&width)) =
                (metrics.heights.get(&key), metrics.widths.get(&key))
            else {
                continue;
            };
            if !height.is_finite() || !width.is_finite() {
                continue;
            }
            let bytes = key.len() * 2;
            while self.entries.len() >= SCENE_METRIC_CAPACITY
                || self.bytes + bytes > SCENE_METRIC_KEY_BYTES
            {
                let old = self.order.pop_front().expect("retained metric key");
                self.entries.remove(&old);
                self.bytes -= old.len() * 2;
            }
            self.bytes += bytes;
            self.order.push_back(key.clone());
            self.entries.insert(key, [height, width]);
        }
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
            paint_text_proofs: RefCell::default(),
            scene_metrics: RefCell::default(),
            #[cfg(test)]
            scene_builds: std::cell::Cell::new(0),
            #[cfg(any(debug_assertions, feature = "testkit"))]
            phases: RendererPhaseCounters::default(),
        })
    }

    /// Default off. Plain release builds ignore enabling and report zeros.
    pub fn set_phase_stats_enabled(&self, enabled: bool) {
        #[cfg(any(debug_assertions, feature = "testkit"))]
        self.phases.enabled.set(enabled);
        self.shared
            .dashboard_text()
            .set_phase_stats_enabled(enabled);
    }

    /// Clear totals while preserving whether profiling is enabled.
    pub fn reset_phase_stats(&self) {
        #[cfg(any(debug_assertions, feature = "testkit"))]
        {
            self.phases.scene_geometry_ns.set(0);
            self.phases.draw_ns.set(0);
            self.phases.present_ns.set(0);
        }
        self.shared.dashboard_text().reset_phase_stats();
    }

    pub fn phase_stats(&self) -> PhaseStats {
        #[cfg(any(debug_assertions, feature = "testkit"))]
        {
            let text = self.shared.dashboard_text().phase_stats();
            PhaseStats {
                scene_geometry_us: self.phases.scene_geometry_ns.get() / 1_000,
                text_measure_us: text.text_measure_us,
                draw_us: self.phases.draw_ns.get() / 1_000,
                present_us: self.phases.present_ns.get() / 1_000,
                layout_creations: text.layout_creations,
                format_creations: text.format_creations,
            }
        }
        #[cfg(not(any(debug_assertions, feature = "testkit")))]
        PhaseStats::default()
    }

    fn build_scene(
        &self,
        view: &DashboardView,
        state: &DashboardState,
        size: [f32; 2],
        metrics: &Metrics,
    ) -> (Scene, SceneSections) {
        #[cfg(test)]
        self.scene_builds.set(self.scene_builds.get() + 1);
        #[cfg(any(debug_assertions, feature = "testkit"))]
        let _geometry = PhaseTimer::new(self.phases.enabled.get(), &self.phases.scene_geometry_ns);
        Scene::build_sections(view, state, size, metrics)
    }

    fn rebuild_section(
        &self,
        previous: &CachedScene,
        state: &DashboardState,
        region: ScrollRegion,
        metrics: &Metrics,
    ) -> Option<(Scene, SceneSections)> {
        #[cfg(any(debug_assertions, feature = "testkit"))]
        let _geometry = PhaseTimer::new(self.phases.enabled.get(), &self.phases.scene_geometry_ns);
        previous
            .sections
            .rebuild(&previous.view, state, &previous.scene, region, metrics)
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
                    .measure(
                        token,
                        D2D_RECT_F {
                            left: 0.,
                            top: 0.,
                            right: 65536.,
                            bottom: measure.line_height,
                        },
                        probe,
                    )?
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
                        .measure(
                            &next,
                            D2D_RECT_F {
                                left: 0.,
                                top: 0.,
                                right: 65536.,
                                bottom: measure.line_height,
                            },
                            probe,
                        )?
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
        self.scene_parts(view, state, size)
            .map(|(scene, _, _)| scene)
    }

    fn scene_parts(
        &self,
        view: &DashboardView,
        state: &DashboardState,
        size: [f32; 2],
    ) -> Result<(Scene, SceneSections, Metrics)> {
        let mut metrics = self.scene_metrics.borrow().snapshot();
        // Probe collects exact width/font requests, then build with measured line
        // heights. Second probe covers content newly revealed by offset clamping.
        for _ in 0..3 {
            let (scene, sections) = self.build_scene(view, state, size, &metrics);
            if !self.measure_missing(&scene.measurements, &mut metrics)? {
                self.scene_metrics
                    .borrow_mut()
                    .remember(&scene.measurements, &metrics);
                return Ok((scene, sections, metrics));
            }
        }
        let (scene, sections) = self.build_scene(view, state, size, &metrics);
        self.scene_metrics
            .borrow_mut()
            .remember(&scene.measurements, &metrics);
        Ok((scene, sections, metrics))
    }

    fn measure_missing(&self, requests: &[Measure], metrics: &mut Metrics) -> Result<bool> {
        let mut added = false;
        for measure in requests {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                metrics.heights.entry(measure.key())
            {
                let measured = self.shared.dashboard_text().measure(
                    &self.wrapped(measure)?,
                    D2D_RECT_F {
                        left: 0.,
                        top: 0.,
                        right: measure.width.max(1.),
                        bottom: 65536.,
                    },
                    style(measure),
                )?;
                entry.insert(measured.required_height);
                metrics.widths.insert(measure.key(), measured.natural_width);
                added = true;
            }
        }
        Ok(added)
    }

    pub fn scene_shared(
        &self,
        view: &Arc<DashboardView>,
        state: &DashboardState,
        size: [f32; 2],
    ) -> Result<CachedScene> {
        let (scene, sections, metrics) = self.scene_parts(view, state, size)?;
        Ok(CachedScene::new(
            scene,
            view,
            ScrollInputs::new(state, size),
            sections,
            metrics,
            None,
        ))
    }

    /// Regenerate one complete scrolling card, including sticky headers and
    /// newly visible hit/UIA nodes. Reuse requires the same retained projection,
    /// structural inputs and all other region offsets; otherwise build fully.
    pub fn scene_scroll(
        &self,
        view: &Arc<DashboardView>,
        state: &DashboardState,
        size: [f32; 2],
        previous: &CachedScene,
        region: ScrollRegion,
    ) -> Result<CachedScene> {
        if !Arc::ptr_eq(view, &previous.view)
            || previous.inputs != ScrollInputs::new(state, size)
            || previous.scene.content.iter().any(|other| {
                other.region != region && scroll_offset(state, other.region) != other.offset
            })
        {
            return self.scene_shared(view, state, size);
        }
        let mut metrics = previous.metrics.clone();
        for _ in 0..3 {
            let Some((scene, sections)) = self.rebuild_section(previous, state, region, &metrics)
            else {
                return self.scene_shared(view, state, size);
            };
            let requests = sections.measurements(region).expect("rebuilt section");
            if !self.measure_missing(&scene.measurements[requests], &mut metrics)? {
                return Ok(CachedScene::new(
                    scene,
                    view,
                    previous.inputs.clone(),
                    sections,
                    metrics,
                    Some(region),
                ));
            }
        }
        let Some((scene, sections)) = self.rebuild_section(previous, state, region, &metrics)
        else {
            return self.scene_shared(view, state, size);
        };
        Ok(CachedScene::new(
            scene,
            view,
            previous.inputs.clone(),
            sections,
            metrics,
            Some(region),
        ))
    }
    pub fn paint(
        &self,
        target: &ID2D1RenderTarget,
        scene: &Scene,
        interaction: &Interaction,
    ) -> Result<()> {
        self.paint_clipped(target, scene, interaction, None)
    }

    /// Paint the full scene through an optional dirty rectangle in scene-space DIPs.
    pub fn paint_clipped(
        &self,
        target: &ID2D1RenderTarget,
        scene: &Scene,
        interaction: &Interaction,
        clip: Option<crate::Rect>,
    ) -> Result<()> {
        #[cfg(any(debug_assertions, feature = "testkit"))]
        let draw_started = self.phases.enabled.get().then(Instant::now);
        #[cfg(any(debug_assertions, feature = "testkit"))]
        let text_before = self.shared.dashboard_text().phase_measure_ns();
        let p = self.shared.dashboard_palette();
        let outer_clip = clip.map_or(scene.rect, |clip| {
            crate::v2_geometry::intersection(scene.rect, clip)
                .unwrap_or_else(|| rect(scene.rect.left, scene.rect.top, 0., 0.))
        });
        let (mut dpi_x, mut dpi_y) = (96., 96.);
        unsafe { target.GetDpi(&mut dpi_x, &mut dpi_y) };
        let mut paint_clip = PaintClip::new(outer_clip, [dpi_x, dpi_y]);
        let mut clips = 0usize;
        let mut alpha = 1.;
        unsafe {
            target.BeginDraw();
        }
        let result = (|| -> Result<()> {
            unsafe {
                target.PushAxisAlignedClip(&bounds(outer_clip), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            }
            clips += 1;
            for draw in &scene.draw {
                if !paint_clip.keeps(draw) {
                    continue;
                }
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
                    } => {
                        let outside = width(*rect) > 0.
                            && height(*rect) > 0.
                            && !paint_clip.visible(*rect, 0.);
                        let proof = outside.then(|| PaintTextProofs::key(measure, *rect));
                        if proof.as_ref().is_some_and(|key| {
                            self.paint_text_proofs.borrow().contained.contains(key)
                        }) {
                            continue;
                        }
                        let wrapped = self.wrapped(measure)?;
                        if let Some(key) = proof {
                            let metrics = self.shared.dashboard_text().measure(
                                &wrapped,
                                bounds(*rect),
                                style(measure),
                            )?;
                            if !metrics.ink_overflow {
                                self.paint_text_proofs.borrow_mut().remember(key);
                                continue;
                            }
                        }
                        self.shared.text(
                            target,
                            *rect,
                            &wrapped,
                            style(measure),
                            color(p, *c),
                            alpha,
                        )?;
                    }
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
                        paint_clip.push(*rect);
                    }
                    Draw::Unclip => {
                        if clips > 1 {
                            unsafe {
                                target.PopAxisAlignedClip();
                            }
                            clips -= 1;
                            paint_clip.pop();
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
        #[cfg(any(debug_assertions, feature = "testkit"))]
        if let Some(started) = draw_started {
            let text_ns = self
                .shared
                .dashboard_text()
                .phase_measure_ns()
                .saturating_sub(text_before);
            let draw_ns = phase_elapsed_ns(started).saturating_sub(text_ns);
            self.phases
                .draw_ns
                .set(self.phases.draw_ns.get().saturating_add(draw_ns));
        }
        #[cfg(any(debug_assertions, feature = "testkit"))]
        let present = PhaseTimer::new(self.phases.enabled.get(), &self.phases.present_ns);
        let ended = unsafe { target.EndDraw(None, None) };
        #[cfg(any(debug_assertions, feature = "testkit"))]
        drop(present);
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
            .measure(
                text,
                D2D_RECT_F {
                    left: 0.,
                    top: 0.,
                    right: w,
                    bottom: 4096.,
                },
                style(&measure),
            )?
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

#[cfg(test)]
mod region_scroll_tests {
    use super::*;
    use crate::{dashboard_fixtures as fixtures, dashboard_model as model};

    fn fixture() -> (Arc<DashboardView>, DashboardState) {
        let mut state = DashboardState::default();
        let mut view = model::project(&fixtures::snapshot(), &mut state, &fixtures::clock());
        let row = view
            .groups
            .iter()
            .flat_map(|g| &g.rows)
            .next()
            .unwrap()
            .clone();
        let group = view.groups.iter_mut().find(|g| !g.rows.is_empty()).unwrap();
        group.rows = (0..32)
            .map(|i| {
                let mut row = row.clone();
                row.id = format!("scroll-session-{i}");
                row
            })
            .collect();
        group.collapsed = false;
        group.relogin = true;
        group.relogin_reason = "합성 다시 로그인 필요".into();
        view.flat = view.groups.iter().flat_map(|g| g.rows.clone()).collect();
        let events = view.events.clone();
        assert!(!events.is_empty());
        view.events = (0..24)
            .map(|i| {
                let mut event = events[i % events.len()].clone();
                event.seq = 1000 + i as u64;
                event.body = "합성 기록 본문 ".repeat(12);
                event
            })
            .collect();
        view.recent = view.events.iter().take(8).cloned().collect();
        let tile = view
            .usage
            .iter()
            .find(|t| t.series.iter().any(|s| !s.points.is_empty()))
            .expect("synthetic history fixture")
            .clone();
        view.usage = (0..12)
            .map(|i| {
                let mut tile = tile.clone();
                tile.id = format!("scroll-account-{i}");
                tile
            })
            .collect();
        (Arc::new(view), state)
    }

    fn normalize(state: &mut DashboardState, scene: &Scene) {
        for region in &scene.content {
            state.scroll(region.region, region.offset, region.max_offset);
        }
    }

    fn set_offset(state: &mut DashboardState, region: ScrollRegion, offset: f32) {
        match region {
            ScrollRegion::Table => state.table_offset = offset,
            ScrollRegion::Side => state.side_offset = offset,
            ScrollRegion::Events => state.events_offset = offset,
            ScrollRegion::Usage => state.usage_offset = offset,
        }
    }

    fn equivalent(actual: &Scene, expected: &Scene) {
        assert_eq!(actual.rect, expected.rect);
        assert_eq!(actual.modal, expected.modal);
        // Compare every public vector, including ordering and measurements;
        // matching pixels alone would miss stale hit/UIA/history metadata.
        macro_rules! same {
            ($($field:ident),+ $(,)?) => {$(
                assert_eq!(format!("{:?}", actual.$field), format!("{:?}", expected.$field),
                    "{} differs from full scene", stringify!($field));
            )+};
        }
        same!(
            content,
            targets,
            accessibility,
            draw,
            measurements,
            charts,
            tooltips,
            session_rows
        );
    }

    fn scroll(
        renderer: &Renderer,
        view: &Arc<DashboardView>,
        state: &mut DashboardState,
        size: [f32; 2],
        previous: &CachedScene,
        region: ScrollRegion,
        offset: f32,
    ) -> Result<CachedScene> {
        set_offset(state, region, offset);
        let next = renderer.scene_scroll(view, state, size, previous, region)?;
        assert_eq!(next.rebuilt_region(), Some(region));
        equivalent(&next, &renderer.scene(view, state, size)?);
        assert!(next.metrics.heights.len() <= next.measurements.len());
        assert!(next.metrics.widths.len() <= next.measurements.len());
        normalize(state, &next);
        Ok(next)
    }

    #[test]
    fn r1210_region_scroll_sticky_reveal_and_alternating_cards_match_full() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let (view, mut state) = fixture();
        let size = [1320., 640.];
        let mut scene = renderer.scene_shared(&view, &state, size)?;
        normalize(&mut state, &scene);
        let viewport = scene
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Table)
            .unwrap()
            .viewport;
        let hidden = scene
            .session_rows
            .iter()
            .find(|r| r.rect.top >= viewport.bottom)
            .unwrap()
            .id
            .clone();
        let key = format!("session:{hidden}");
        assert_eq!(
            height(
                scene
                    .accessibility
                    .iter()
                    .find(|n| n.key == key)
                    .unwrap()
                    .rect
            ),
            0.
        );
        assert!(
            !scene
                .targets
                .iter()
                .any(|t| t.target == Target::Session(hidden.clone()))
        );
        let reveal = scene.reveal_session(&hidden).unwrap();
        scene = scroll(
            &renderer,
            &view,
            &mut state,
            size,
            &scene,
            ScrollRegion::Table,
            96.,
        )?;
        assert!(
            scene
                .accessibility
                .iter()
                .any(|n| n.key.starts_with("sticky:"))
        );
        scene = scroll(
            &renderer,
            &view,
            &mut state,
            size,
            &scene,
            ScrollRegion::Table,
            reveal,
        )?;
        let node = scene.accessibility.iter().find(|n| n.key == key).unwrap();
        assert_eq!(height(node.rect), 35.);
        let target = Target::Session(hidden);
        assert!(
            scene
                .targets
                .iter()
                .any(|t| t.target == target && t.rect == node.rect)
        );
        assert_eq!(
            scene.hit_test(
                (node.rect.left + node.rect.right) / 2.,
                (node.rect.top + node.rect.bottom) / 2.
            ),
            Some(&target)
        );
        // Changing Table's sticky controls changes vector lengths before Side.
        // Alternate regions to exercise the shifted suffix spans, not just one rebuild.
        for (region, offset) in [
            (ScrollRegion::Side, 96.),
            (ScrollRegion::Table, 1e6),
            (ScrollRegion::Side, 1e6),
            (ScrollRegion::Table, -5.),
            (ScrollRegion::Side, 0.),
            (ScrollRegion::Table, 0.),
        ] {
            scene = scroll(&renderer, &view, &mut state, size, &scene, region, offset)?;
        }
        assert_eq!(state.table_offset, 0.);
        assert_eq!(state.side_offset, 0.);
        Ok(())
    }

    #[test]
    fn r1210_region_scroll_detail_and_narrow_overlay_match_full() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let (view, state) = fixture();
        let mut detailed = (*view).clone();
        detailed.detail = Some(model::DetailView {
            id: "synthetic-detail".into(),
            title: "합성 상세".into(),
            subtitle: "지역 스크롤 회귀".into(),
            raw_prefix: Some("12345678".into()),
            status: "준비".into(),
            status_tone: detailed.groups[0].tone,
            account_status: "합성 계정".into(),
            facts: (0..24)
                .map(|i| (format!("항목 {i}"), "합성 상세 값 ".repeat(8)))
                .collect(),
            timeline: vec!["합성 타임라인".into(); 8],
            requests: vec!["합성 요청".into(); 8],
            expired: false,
        });
        let view = Arc::new(detailed);
        for size in [[1320., 640.], [1024., 640.]] {
            let mut state = state.clone();
            let mut scene = renderer.scene_shared(&view, &state, size)?;
            normalize(&mut state, &scene);
            assert!(
                scene
                    .content
                    .iter()
                    .any(|r| r.region == ScrollRegion::Side && r.max_offset > 0.)
            );
            for offset in [96., 193.25, 1e6, -1., 0.] {
                scene = scroll(
                    &renderer,
                    &view,
                    &mut state,
                    size,
                    &scene,
                    ScrollRegion::Side,
                    offset,
                )?;
                assert!(
                    scene
                        .targets
                        .iter()
                        .any(|t| t.target == Target::CloseDetail)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn r1210_region_scroll_events_usage_and_disabled_controls_match_full() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let (view, initial) = fixture();
        for core_ok in [true, false] {
            let mut v = (*view).clone();
            v.connection.core_ok = core_ok;
            let view = Arc::new(v);
            for (tab, region) in [
                (Tab::Events, ScrollRegion::Events),
                (Tab::Usage, ScrollRegion::Usage),
            ] {
                for size in [[1320., 640.], [1024., 640.]] {
                    let mut state = initial.clone();
                    state.tab = tab;
                    let mut scene = renderer.scene_shared(&view, &state, size)?;
                    normalize(&mut state, &scene);
                    assert!(
                        scene
                            .content
                            .iter()
                            .any(|r| r.region == region && r.max_offset > 0.)
                    );
                    for offset in [96., 173.25, 1e6, 1e6, -1., 0.] {
                        scene = scroll(&renderer, &view, &mut state, size, &scene, region, offset)?;
                        if !core_ok {
                            assert!(scene.targets.iter().all(|t| !t.enabled));
                            assert!(
                                scene
                                    .accessibility
                                    .iter()
                                    .filter(|n| n.target.is_some())
                                    .all(|n| !n.enabled)
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn r1210_region_scroll_structural_and_unsupported_inputs_fall_back() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let (view, mut state) = fixture();
        let size = [1320., 640.];
        let previous = renderer.scene_shared(&view, &state, size)?;
        normalize(&mut state, &previous);
        let fallback =
            |view: &Arc<DashboardView>, state: &DashboardState, size, region| -> Result<()> {
                let next = renderer.scene_scroll(view, state, size, &previous, region)?;
                assert_eq!(next.rebuilt_region(), None);
                equivalent(&next, &renderer.scene(view, state, size)?);
                Ok(())
            };
        fallback(&view, &state, [1024., 640.], ScrollRegion::Table)?;
        fallback(&view, &state, size, ScrollRegion::Usage)?; // no Usage section on Overview
        let mut changed = state.clone();
        changed.popup = Some(Popup::Range);
        fallback(&view, &changed, size, ScrollRegion::Table)?;
        changed = state.clone();
        changed.grouped = !changed.grouped;
        fallback(&view, &changed, size, ScrollRegion::Table)?;
        changed = state.clone();
        changed.tab = Tab::Events;
        fallback(&view, &changed, size, ScrollRegion::Events)?;
        changed = state.clone();
        changed.side_offset = 96.;
        fallback(&view, &changed, size, ScrollRegion::Table)?; // two regions changed
        let replacement = Arc::new((*view).clone());
        fallback(&replacement, &state, size, ScrollRegion::Table)?;
        // Retaining the projection makes identity safe, rather than comparing a
        // possibly reused address or relying only on a data revision number.
        let mut retained = view.clone();
        assert!(Arc::get_mut(&mut retained).is_none());
        Ok(())
    }

    #[test]
    fn r1210_region_scroll_metric_retention_is_bounded_by_current_working_set() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let (view, mut state) = fixture();
        let size = [1320., 640.];
        let mut scene = renderer.scene_shared(&view, &state, size)?;
        normalize(&mut state, &scene);
        for i in 0..32 {
            let region = if i % 2 == 0 {
                ScrollRegion::Table
            } else {
                ScrollRegion::Side
            };
            let max = scene
                .content
                .iter()
                .find(|r| r.region == region)
                .unwrap()
                .max_offset;
            let offset = (i as f32 * 73.125) % max.max(1.);
            scene = scroll(&renderer, &view, &mut state, size, &scene, region, offset)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod paint_culling_tests {
    use super::*;

    fn fill(r: crate::Rect) -> Draw {
        Draw::Fill {
            rect: r,
            color: Color::Text,
            radius: 0.,
        }
    }

    #[test]
    fn r1210_paint_culling_keeps_stroke_line_and_antialias_edges() {
        let clip = PaintClip::new(rect(0., 0., 100., 100.), [96., 96.]);
        assert!(!clip.keeps(&fill(rect(-20., 10., 2., 20.))));
        assert!(clip.keeps(&fill(rect(-2., 10., 0.5, 20.))));
        assert!(!clip.keeps(&fill(rect(-4., 10., 1., 20.))));
        let low_dpi = PaintClip::new(rect(0., 0., 100., 100.), [48., 48.]);
        assert!(low_dpi.keeps(&fill(rect(-4., 10., 1., 20.))));
        assert!(clip.keeps(&Draw::Stroke {
            rect: rect(-6., 10., 2., 20.),
            color: Color::Text,
            width: 10.,
            radius: 0.,
        }));
        for (from, to, width, expected) in [
            ([-4., 10.], [-4., 90.], 8., true),
            ([-10., 10.], [-10., 90.], 2., false),
            ([-10., 50.], [110., 50.], 1., true),
            ([110., 110.], [-10., -10.], 1., true),
            ([50., 50.], [50., 50.], 1., true),
        ] {
            assert_eq!(
                clip.keeps(&Draw::Line {
                    from,
                    to,
                    color: Color::Text,
                    width,
                    dash: Some([1., 3.]),
                }),
                expected
            );
        }
    }

    #[test]
    fn r1210_paint_culling_nested_empty_clips_preserve_state_and_restore_parent() {
        let mut clip = PaintClip::new(rect(0., 0., 100., 100.), [96., 96.]);
        clip.push(rect(10., 10., 30., 30.));
        assert!(clip.keeps(&fill(rect(20., 20., 2., 2.))));
        assert!(!clip.keeps(&fill(rect(80., 80., 2., 2.))));
        clip.push(rect(200., 200., 10., 10.));
        assert!(!clip.keeps(&fill(rect(20., 20., 2., 2.))));
        for state in [
            Draw::Alpha(0.25),
            Draw::Clip(rect(0., 0., 5., 5.)),
            Draw::Unclip,
        ] {
            assert!(clip.keeps(&state));
        }
        clip.pop();
        assert!(clip.keeps(&fill(rect(20., 20., 2., 2.))));
        assert!(!clip.keeps(&fill(rect(80., 80., 2., 2.))));
        clip.pop();
        clip.pop(); // An unmatched Unclip cannot remove the outer dirty clip.
        assert!(clip.keeps(&fill(rect(80., 80., 2., 2.))));
        assert!(!clip.keeps(&fill(rect(180., 80., 2., 2.))));
    }

    #[test]
    fn r1210_paint_culling_unknown_geometry_and_unproven_text_draw_conservatively() {
        let mut clip = PaintClip::new(rect(0., 0., 100., 100.), [96., 96.]);
        assert!(clip.keeps(&fill(rect(f32::NAN, 10., 2., 2.))));
        assert!(
            PaintClip::new(rect(0., 0., 100., 100.), [0., f32::NAN])
                .keeps(&fill(rect(200., 200., 2., 2.)))
        );
        let text = Draw::Text {
            rect: rect(200., 200., 20., 20.),
            measure: Measure {
                text: "가".into(),
                size: 24.,
                line_height: 1.,
                width: 20.,
                bold: false,
                wrap: false,
                face: Face::Text,
                align: TextAlign::Leading,
            },
            color: Color::Text,
        };
        // Requested text boxes do not bound unmeasured fallback-font ink.
        assert!(clip.keeps(&text));
        clip.push(rect(200., 200., 2., 2.));
        assert!(!clip.keeps(&text));
        assert!(clip.keeps(&Draw::Alpha(1.)));
    }

    #[test]
    fn r1210_paint_culling_text_proofs_include_dimensions_and_stay_bounded() {
        let m = Measure {
            text: "synthetic".into(),
            size: 12.,
            line_height: 20.,
            width: 40.,
            bold: false,
            wrap: false,
            face: Face::Text,
            align: TextAlign::Leading,
        };
        let key = PaintTextProofs::key(&m, rect(0., 0., 40., 20.));
        assert_eq!(key, PaintTextProofs::key(&m, rect(100., 200., 40., 20.)));
        assert_ne!(key, PaintTextProofs::key(&m, rect(0., 0., 40., 1.)));
        assert_ne!(key, PaintTextProofs::key(&m, rect(0., 0., 41., 20.)));
        let mut changed = m.clone();
        changed.wrap = true;
        assert_ne!(key, PaintTextProofs::key(&changed, rect(0., 0., 40., 20.)));
        let mut proofs = PaintTextProofs::default();
        for i in 0..PAINT_TEXT_PROOF_CAPACITY + 8 {
            proofs.remember(format!("{i:08}"));
        }
        assert_eq!(proofs.contained.len(), PAINT_TEXT_PROOF_CAPACITY);
        for i in 0..PAINT_TEXT_PROOF_CAPACITY {
            proofs.remember(format!("{i:08}{}", "x".repeat(512)));
        }
        assert!(proofs.bytes <= PAINT_TEXT_PROOF_BYTES);
        assert!(proofs.contained.len() <= PAINT_TEXT_PROOF_CAPACITY);
        assert_eq!(
            proofs.bytes,
            proofs.contained.iter().map(String::len).sum::<usize>()
        );
    }
}

#[cfg(test)]
mod scene_metric_tests {
    use super::*;
    use crate::{dashboard_fixtures as fixtures, dashboard_model as model};

    #[test]
    fn r1210_scene_metrics_warm_tabs_build_once_and_match_fresh_scenes() -> Result<()> {
        let renderer = Renderer::new(false)?;
        assert!(renderer.scene_metrics.borrow().entries.is_empty());
        let mut state = DashboardState::default();
        let view = model::project(&fixtures::snapshot(), &mut state, &fixtures::clock());
        let size = [1320., 640.];
        for tab in [Tab::Overview, Tab::Events, Tab::Usage] {
            state.tab = tab;
            renderer.scene(&view, &state, size)?;
        }
        for tab in [Tab::Overview, Tab::Events, Tab::Usage, Tab::Overview] {
            state.tab = tab;
            renderer.scene_builds.set(0);
            let warm = renderer.scene(&view, &state, size)?;
            assert_eq!(renderer.scene_builds.get(), 1, "warm {tab:?}");
            let fresh = Renderer::new(false)?;
            let cold = fresh.scene(&view, &state, size)?;
            assert!(fresh.scene_builds.get() > 1, "cold {tab:?} must probe");
            assert_eq!(format!("{warm:?}"), format!("{cold:?}"), "{tab:?}");
        }
        Ok(())
    }

    #[test]
    fn r1210_scene_metrics_changed_source_width_and_popup_match_fresh_scenes() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let mut state = DashboardState::default();
        let mut view = model::project(&fixtures::snapshot(), &mut state, &fixtures::clock());
        renderer.scene(&view, &state, [1320., 640.])?;
        view.groups[0].name.push_str(" 합성 이름 변경");
        view.events[0].body = "변경된 합성 본문 ".repeat(40);
        view.usage[0].name.push_str(" 합성 사용량 이름 변경");
        for (tab, popup, size) in [
            (Tab::Overview, Some(Popup::Range), [1024., 720.]),
            (Tab::Events, None, [1024., 720.]),
            (Tab::Usage, None, [1320., 720.]),
        ] {
            state.tab = tab;
            state.popup = popup;
            let next = renderer.scene(&view, &state, size)?;
            let cold = Renderer::new(false)?.scene(&view, &state, size)?;
            assert_eq!(format!("{next:?}"), format!("{cold:?}"), "{tab:?}");
            renderer.scene_builds.set(0);
            let warm = renderer.scene(&view, &state, size)?;
            assert_eq!(renderer.scene_builds.get(), 1, "repeated {tab:?}");
            assert_eq!(format!("{warm:?}"), format!("{cold:?}"), "{tab:?}");
        }
        Ok(())
    }

    #[test]
    fn r1210_scene_metrics_cache_requires_pairs_and_bounds_keys_and_bytes() {
        let request = Measure {
            text: "synthetic".into(),
            size: 12.,
            line_height: 20.,
            width: 40.,
            bold: false,
            wrap: false,
            face: Face::Text,
            align: TextAlign::Leading,
        };
        let mut cache = SceneMetricCache::default();
        let mut metrics = Metrics::default();
        metrics.heights.insert(request.key(), 20.);
        cache.remember(std::slice::from_ref(&request), &metrics);
        assert!(cache.entries.is_empty());
        metrics.widths.insert(request.key(), 31.);
        cache.remember(std::slice::from_ref(&request), &metrics);
        let original = request.key();
        assert_eq!(cache.snapshot().heights[&original], 20.);
        assert_eq!(cache.snapshot().widths[&original], 31.);
        let mut requests = Vec::new();
        for i in 0..SCENE_METRIC_CAPACITY + 1 {
            let mut r = request.clone();
            r.text = format!("metric-{i}");
            metrics.heights.insert(r.key(), i as f32);
            metrics.widths.insert(r.key(), (i + 1) as f32);
            requests.push(r);
        }
        cache.remember(&requests, &metrics);
        assert_eq!(cache.entries.len(), SCENE_METRIC_CAPACITY);
        assert!(!cache.entries.contains_key(&original));
        for r in &mut requests {
            r.text.push_str(&"x".repeat(512));
            metrics.heights.insert(r.key(), 20.);
            metrics.widths.insert(r.key(), 31.);
        }
        let mut oversized = request.clone();
        oversized.text = "x".repeat(16 * 1024);
        metrics.heights.insert(oversized.key(), 20.);
        metrics.widths.insert(oversized.key(), 31.);
        requests.push(oversized.clone());
        cache.remember(&requests, &metrics);
        assert!(!cache.entries.contains_key(&oversized.key()));
        assert!(cache.entries.len() <= SCENE_METRIC_CAPACITY);
        assert!(cache.bytes <= SCENE_METRIC_KEY_BYTES);
        assert_eq!(
            cache.bytes,
            cache.entries.keys().map(|k| k.len() * 2).sum::<usize>()
        );
        assert_eq!(cache.order.len(), cache.entries.len());
        let snapshot = cache.snapshot();
        assert!(snapshot.heights.keys().eq(snapshot.widths.keys()));
        for key in snapshot.heights.keys() {
            assert_eq!(
                cache.entries[key],
                [snapshot.heights[key], snapshot.widths[key]]
            );
        }
    }
}

#[cfg(test)]
mod scroll_damage_tests {
    use super::*;
    use crate::{dashboard_fixtures as fixtures, dashboard_model as model};
    use windows::Win32::{
        Graphics::{
            Direct2D::{
                Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT},
                D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1CreateFactory, ID2D1Factory,
            },
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Imaging::{
                CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmap,
                IWICImagingFactory, WICBitmapCacheOnLoad,
            },
        },
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
    };

    struct Com;
    impl Com {
        fn enter() -> Result<Self> {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
            Ok(Self)
        }
    }
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    // Same WIC/Direct2D setup as tests/support/r10_wic.rs, but keep the actual
    // target between paints and inspect pixels in memory. No HWND or file I/O.
    struct Surface {
        target: ID2D1RenderTarget,
        bitmap: IWICBitmap,
        width: u32,
        height: u32,
        dpi: f32,
    }
    impl Surface {
        fn new(size: [f32; 2], dpi: f32) -> Result<Self> {
            unsafe {
                let wic: IWICImagingFactory =
                    CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
                let width = (size[0] * dpi / 96.).round() as u32;
                let height = (size[1] * dpi / 96.).round() as u32;
                let bitmap = wic.CreateBitmap(
                    width,
                    height,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapCacheOnLoad,
                )?;
                let factory: ID2D1Factory =
                    D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
                let target = factory.CreateWicBitmapRenderTarget(
                    &bitmap,
                    &D2D1_RENDER_TARGET_PROPERTIES {
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                        },
                        dpiX: dpi,
                        dpiY: dpi,
                        ..Default::default()
                    },
                )?;
                Ok(Self {
                    target,
                    bitmap,
                    width,
                    height,
                    dpi,
                })
            }
        }
        fn pixels(&self) -> Result<Vec<u8>> {
            let mut pixels = vec![0; (self.width * self.height * 4) as usize];
            unsafe {
                self.bitmap
                    .CopyPixels(std::ptr::null(), self.width * 4, &mut pixels)?
            };
            Ok(pixels)
        }
        // Model an outward-rounded Win32 physical update RECT, converted back
        // to client DIPs. Fractional clip edges must not blend retained pixels.
        fn physical_clip(&self, r: crate::Rect) -> crate::Rect {
            let scale = self.dpi / 96.;
            crate::Rect {
                left: (r.left * scale).floor() / scale,
                top: (r.top * scale).floor() / scale,
                right: (r.right * scale).ceil() / scale,
                bottom: (r.bottom * scale).ceil() / scale,
            }
        }
    }

    fn fixture(detail: bool) -> (Arc<DashboardView>, DashboardState) {
        let mut state = DashboardState::default();
        let mut view = model::project(&fixtures::snapshot(), &mut state, &fixtures::clock());
        let group = view.groups.iter_mut().find(|g| !g.rows.is_empty()).unwrap();
        let row = group.rows[0].clone();
        group.rows = (0..32)
            .map(|i| {
                let mut row = row.clone();
                row.id = format!("damage-session-{i}");
                row
            })
            .collect();
        group.collapsed = false;
        group.relogin = true;
        group.relogin_reason = "합성 다시 로그인 필요".into();
        view.flat = view.groups.iter().flat_map(|g| g.rows.clone()).collect();
        if detail {
            view.detail = Some(model::DetailView {
                id: "damage-detail".into(),
                title: "합성 상세".into(),
                subtitle: "겹치는 상세 카드".into(),
                raw_prefix: Some("12345678".into()),
                status: "준비".into(),
                status_tone: view.groups[0].tone,
                account_status: "합성 계정".into(),
                facts: vec![("합성 항목".into(), "합성 상세 값 ".repeat(8)); 16],
                timeline: vec!["합성 타임라인".into(); 8],
                requests: vec!["합성 요청".into(); 8],
                expired: false,
            });
        }
        (Arc::new(view), state)
    }

    fn normalize(state: &mut DashboardState, scene: &Scene) {
        for region in &scene.content {
            state.scroll(region.region, region.offset, region.max_offset);
        }
    }

    fn union(a: crate::Rect, b: crate::Rect) -> crate::Rect {
        crate::Rect {
            left: a.left.min(b.left),
            top: a.top.min(b.top),
            right: a.right.max(b.right),
            bottom: a.bottom.max(b.bottom),
        }
    }

    fn equal_pixels(actual: &[u8], expected: &[u8], width: u32) {
        assert_eq!(actual.len(), expected.len());
        let mismatches = actual
            .chunks_exact(4)
            .zip(expected.chunks_exact(4))
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        assert!(
            mismatches.is_empty(),
            "{} differing pixels, first at {:?}",
            mismatches.len(),
            mismatches
                .first()
                .map(|i| (i % width as usize, i / width as usize))
        );
    }

    fn full_pixels(
        view: &Arc<DashboardView>,
        state: &DashboardState,
        size: [f32; 2],
        dpi: f32,
        dark: bool,
        interaction: &Interaction,
    ) -> Result<Vec<u8>> {
        let fresh = Renderer::new(dark)?;
        let scene = fresh.scene(view, state, size)?;
        let surface = Surface::new(size, dpi)?;
        fresh.paint(&surface.target, &scene, interaction)?;
        surface.pixels()
    }

    #[test]
    fn r1210_scroll_damage_wic_sticky_and_newly_revealed_match_full() -> Result<()> {
        let _com = Com::enter()?;
        for (dark, dpi) in [(false, 96.), (true, 192.)] {
            let renderer = Renderer::new(dark)?;
            let (view, mut state) = fixture(false);
            let size = [1320., 640.];
            let mut previous = renderer.scene_shared(&view, &state, size)?;
            normalize(&mut state, &previous);
            let viewport = previous
                .content
                .iter()
                .find(|r| r.region == ScrollRegion::Table)
                .unwrap()
                .viewport;
            let hidden = previous
                .session_rows
                .iter()
                .find(|r| r.rect.top >= viewport.bottom)
                .unwrap()
                .id
                .clone();
            assert!(
                !previous
                    .targets
                    .iter()
                    .any(|t| t.target == Target::Session(hidden.clone()))
            );
            let reveal = previous.reveal_session(&hidden).unwrap();
            let surface = Surface::new(size, dpi)?;
            renderer.paint(&surface.target, &previous, &Interaction::default())?;
            for offset in [96., reveal] {
                let old_pixels = surface.pixels()?;
                state.table_offset = offset;
                let next =
                    renderer.scene_scroll(&view, &state, size, &previous, ScrollRegion::Table)?;
                let damage = next
                    .scroll_damage_from(&previous)
                    .expect("Table card damage");
                // The card includes its static toolbar and footer, not only rows.
                assert!(damage.top < viewport.top && damage.bottom > viewport.bottom);
                assert!(
                    next.accessibility
                        .iter()
                        .any(|n| n.key.starts_with("sticky:"))
                );
                let expected =
                    full_pixels(&view, &state, size, dpi, dark, &Interaction::default())?;
                assert!(
                    old_pixels != expected,
                    "scroll must actually change retained pixels"
                );
                renderer.paint_clipped(
                    &surface.target,
                    &next,
                    &Interaction::default(),
                    Some(surface.physical_clip(damage)),
                )?;
                equal_pixels(&surface.pixels()?, &expected, surface.width);
                normalize(&mut state, &next);
                previous = next;
            }
            assert!(
                previous
                    .targets
                    .iter()
                    .any(|t| t.target == Target::Session(hidden.clone()))
            );
        }
        Ok(())
    }

    #[test]
    fn r1210_scroll_damage_wic_narrow_overlapping_suffix_matches_full() -> Result<()> {
        let _com = Com::enter()?;
        for (dark, dpi) in [(false, 96.), (true, 192.)] {
            let renderer = Renderer::new(dark)?;
            let (view, mut state) = fixture(true);
            let size = [1024., 640.];
            let previous = renderer.scene_shared(&view, &state, size)?;
            normalize(&mut state, &previous);
            let surface = Surface::new(size, dpi)?;
            renderer.paint(&surface.target, &previous, &Interaction::default())?;
            let old_pixels = surface.pixels()?;
            state.table_offset = 96.;
            let next =
                renderer.scene_scroll(&view, &state, size, &previous, ScrollRegion::Table)?;
            let damage = next
                .scroll_damage_from(&previous)
                .expect("Table card damage");
            let overlay = next
                .targets
                .iter()
                .find(|t| t.target == Target::DetailSurface)
                .unwrap();
            assert!(crate::v2_geometry::intersection(overlay.rect, damage).is_some());
            assert!(
                next.draw
                    .iter()
                    .any(|d| matches!(d, Draw::Alpha(a) if *a == 0.03))
            );
            let expected = full_pixels(&view, &state, size, dpi, dark, &Interaction::default())?;
            assert!(old_pixels != expected);
            // Whole composed scene, including the unchanged overlay and shadow,
            // must replay after the Table background under the same dirty clip.
            renderer.paint_clipped(
                &surface.target,
                &next,
                &Interaction::default(),
                Some(surface.physical_clip(damage)),
            )?;
            equal_pixels(&surface.pixels()?, &expected, surface.width);
        }
        Ok(())
    }

    #[test]
    fn r1210_scroll_damage_wic_coalesced_outside_rect_matches_full() -> Result<()> {
        let _com = Com::enter()?;
        let (view, mut state) = fixture(false);
        let size = [1320., 640.];
        let renderer = Renderer::new(false)?;
        let previous = renderer.scene_shared(&view, &state, size)?;
        normalize(&mut state, &previous);
        let old_interaction = Interaction {
            hover: Some(Target::AllEvents),
            ..Default::default()
        };
        let surface = Surface::new(size, 144.)?;
        let card_only = Surface::new(size, 144.)?;
        renderer.paint(&surface.target, &previous, &old_interaction)?;
        renderer.paint(&card_only.target, &previous, &old_interaction)?;
        state.table_offset = 96.;
        let next = renderer.scene_scroll(&view, &state, size, &previous, ScrollRegion::Table)?;
        let damage = next
            .scroll_damage_from(&previous)
            .expect("Table card damage");
        let outside = previous
            .targets
            .iter()
            .find(|t| t.target == Target::AllEvents)
            .unwrap()
            .rect;
        assert!(
            outside.left > damage.right,
            "pending hover damage is outside Table card"
        );
        let pending = crate::Rect {
            left: outside.left - 4.,
            top: outside.top - 4.,
            right: outside.right + 4.,
            bottom: outside.bottom + 4.,
        };
        let expected = full_pixels(&view, &state, size, 144., false, &Interaction::default())?;
        renderer.paint_clipped(
            &card_only.target,
            &next,
            &Interaction::default(),
            Some(card_only.physical_clip(damage)),
        )?;
        assert!(
            card_only.pixels()? != expected,
            "card-only repaint must leave the old Side hover"
        );
        renderer.paint_clipped(
            &surface.target,
            &next,
            &Interaction::default(),
            Some(surface.physical_clip(union(damage, pending))),
        )?;
        equal_pixels(&surface.pixels()?, &expected, surface.width);
        Ok(())
    }

    #[test]
    fn r1210_scroll_damage_rejects_non_table_and_mismatched_frames() -> Result<()> {
        let renderer = Renderer::new(false)?;
        let (view, mut state) = fixture(false);
        let size = [1320., 640.];
        let previous = renderer.scene_shared(&view, &state, size)?;
        normalize(&mut state, &previous);
        state.table_offset = 96.;
        let next = renderer.scene_scroll(&view, &state, size, &previous, ScrollRegion::Table)?;
        assert!(next.scroll_damage_from(&previous).is_some());
        assert_eq!(
            next.scroll_damage_from(&previous),
            previous.table_scroll_damage()
        );
        assert!(previous.scroll_damage_from(&previous).is_none());
        for region in [
            ScrollRegion::Side,
            ScrollRegion::Events,
            ScrollRegion::Usage,
        ] {
            let mut changed = next.clone();
            changed.rebuilt_region = Some(region);
            assert!(changed.scroll_damage_from(&previous).is_none());
        }
        let mut changed = next.clone();
        changed.view = Arc::new((*view).clone());
        assert!(changed.scroll_damage_from(&previous).is_none());
        changed = next.clone();
        changed.inputs.size[0] += 1.;
        assert!(changed.scroll_damage_from(&previous).is_none());
        changed = next.clone();
        changed.inputs.popup = Some(Popup::Range);
        assert!(changed.scroll_damage_from(&previous).is_none());
        assert!(changed.table_scroll_damage().is_none());
        changed = next.clone();
        changed.scene.modal = true;
        assert!(changed.scroll_damage_from(&previous).is_none());
        assert!(changed.table_scroll_damage().is_none());
        changed = next.clone();
        changed.scene.rect.right -= 1.;
        assert!(changed.scroll_damage_from(&previous).is_none());
        changed = next.clone();
        changed
            .content
            .iter_mut()
            .find(|r| r.region == ScrollRegion::Side)
            .unwrap()
            .offset += 1.;
        assert!(changed.scroll_damage_from(&previous).is_none());
        changed = next.clone();
        changed
            .content
            .iter_mut()
            .find(|r| r.region == ScrollRegion::Table)
            .unwrap()
            .viewport
            .top += 1.;
        assert!(changed.scroll_damage_from(&previous).is_none());
        // The old-frame proposal is clipped to client bounds; it is not
        // permission to reuse pixels after an actual target/size change.
        changed = previous.clone();
        changed.scene.rect = rect(30., 100., 50., 80.);
        assert_eq!(changed.table_scroll_damage(), Some(changed.scene.rect));
        changed.scene.rect.right = f32::NAN;
        assert!(changed.table_scroll_damage().is_none());
        Ok(())
    }
}
