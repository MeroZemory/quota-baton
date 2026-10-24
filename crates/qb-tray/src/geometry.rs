//! Dashboard DIP layout and semantic scene. Paint/hit/UIA use these same boxes.
use crate::{Rect, dashboard_history as history, dashboard_model as m, state::*, v2};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};
#[path = "dashboard_usage_geometry.rs"]
mod usage_geometry;
use usage_geometry::usage;
#[path = "dashboard_provider_geometry.rs"]
mod provider_geometry;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    DismissPopup,
    Tab(Tab),
    Provider(ProviderFilter),
    Range(Range),
    RangeMenu,
    ToggleGrouping,
    Search,
    EventSearch,
    Sort(SortKey),
    Group(String),
    Session(String),
    SessionUrgency(String),
    SessionMoveNow(String),
    CloseDetail,
    DetailSurface,
    EventFilter(EventFilter),
    EventDays(u8),
    EventDaysMenu,
    AllEvents,
    SessionLink(String),
    AccountLink(String),
    OpenTrayAccount(String),
    OpenTrayAdd,
    Today,
    Chart(String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Tab,
    Button,
    Checkbox,
    Edit,
    ComboBox,
    Table,
    ColumnHeader,
    Row,
    Text,
    Chart,
    Group,
}
#[derive(Clone, Copy, Debug)]
pub enum Color {
    Page,
    Surface,
    Footer,
    Border,
    GroupBorder,
    Text,
    Secondary,
    Accent,
    OnAccent,
    Warn,
    Bad,
    Select,
    Track,
    BarOk,
    BarLow,
    BarCritical,
    BarPaused,
    PipOutline,
    Verdict(v2::Tone),
    Series7d,
    ChipBg(v2::Tone),
    ChipFg(v2::Tone),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    Text,
    Mono,
    Icon,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlign {
    Leading,
    Center,
}
#[derive(Clone, Debug)]
pub struct Measure {
    pub text: String,
    pub size: f32,
    pub line_height: f32,
    pub width: f32,
    pub bold: bool,
    pub wrap: bool,
    pub face: Face,
    pub align: TextAlign,
}
impl Measure {
    pub fn key(&self) -> String {
        format!(
            "{:?}:{:?}:{}:{}:{}:{}:{}:{}",
            self.face,
            self.align,
            self.size,
            self.line_height,
            self.width,
            self.bold,
            self.wrap,
            self.text
        )
    }
}
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub heights: BTreeMap<String, f32>,
    pub widths: BTreeMap<String, f32>,
}
impl Metrics {
    fn height(&self, m: &Measure) -> f32 {
        self.heights.get(&m.key()).copied().unwrap_or_else(|| {
            if m.wrap {
                ((m.text.chars().count() as f32 * m.size * 0.65 / m.width.max(1.))
                    .ceil()
                    .max(1.))
                    * m.line_height
            } else {
                m.line_height
            }
        })
    }
}
#[derive(Clone, Debug)]
pub enum Draw {
    Fill {
        rect: Rect,
        color: Color,
        radius: f32,
    },
    Stroke {
        rect: Rect,
        color: Color,
        width: f32,
        radius: f32,
    },
    Text {
        rect: Rect,
        measure: Measure,
        color: Color,
    },
    Line {
        from: [f32; 2],
        to: [f32; 2],
        color: Color,
        width: f32,
        dash: Option<[f32; 2]>,
    },
    Clip(Rect),
    Unclip,
    Alpha(f32),
}
#[derive(Clone, Debug)]
pub struct HitTarget {
    pub rect: Rect,
    pub target: Target,
    pub enabled: bool,
    pub tooltip: Option<String>,
}
#[derive(Clone, Debug)]
pub struct TooltipRegion {
    pub rect: Rect,
    pub text: String,
}
#[derive(Clone, Debug)]
pub struct AccessibleNode {
    pub key: String,
    pub parent: Option<String>,
    pub role: Role,
    pub name: String,
    pub rect: Rect,
    pub target: Option<Target>,
    pub enabled: bool,
    pub selected: bool,
    pub expanded: Option<bool>,
    pub value: Option<String>,
    pub hidden: bool,
}
#[derive(Clone, Debug)]
pub struct Region {
    pub region: ScrollRegion,
    pub viewport: Rect,
    pub content_height: f32,
    pub offset: f32,
    pub max_offset: f32,
}
#[derive(Clone, Debug)]
pub struct ChartData {
    pub id: String,
    pub rect: Rect,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub now: DateTime<Utc>,
    pub series: Vec<m::Series>,
    pub unconfirmed: bool,
    pub offset: chrono::FixedOffset,
    /// Account-equivalent plots have their own explicitly labelled summary.
    pub provider_summary: Option<String>,
}
#[derive(Clone, Debug)]
pub struct ChartTooltip {
    pub text: String,
    pub position: [f32; 2],
}
#[derive(Clone, Debug)]
pub struct SessionGeometry {
    pub id: String,
    /// Full row in client DIPs, before viewport clipping.
    pub rect: Rect,
}
#[derive(Clone, Debug)]
pub struct Scene {
    pub modal: bool,
    pub rect: Rect,
    pub content: Vec<Region>,
    pub targets: Vec<HitTarget>,
    pub accessibility: Vec<AccessibleNode>,
    pub draw: Vec<Draw>,
    pub measurements: Vec<Measure>,
    pub charts: Vec<ChartData>,
    pub tooltips: Vec<TooltipRegion>,
    pub session_rows: Vec<SessionGeometry>,
}

// A section covers an entire card, including its viewport-dependent controls
// and sticky header. Keep every parallel Scene vector in the same paint order.
#[derive(Clone, Copy, Debug, Default)]
struct SceneCursor([usize; 8]);
impl SceneCursor {
    fn at(scene: &Scene) -> Self {
        Self([
            scene.content.len(),
            scene.targets.len(),
            scene.accessibility.len(),
            scene.draw.len(),
            scene.measurements.len(),
            scene.charts.len(),
            scene.tooltips.len(),
            scene.session_rows.len(),
        ])
    }
    fn truncate(self, scene: &mut Scene) {
        scene.content.truncate(self.0[0]);
        scene.targets.truncate(self.0[1]);
        scene.accessibility.truncate(self.0[2]);
        scene.draw.truncate(self.0[3]);
        scene.measurements.truncate(self.0[4]);
        scene.charts.truncate(self.0[5]);
        scene.tooltips.truncate(self.0[6]);
        scene.session_rows.truncate(self.0[7]);
    }
    fn append_suffix(self, previous: &Scene, scene: &mut Scene) {
        scene
            .content
            .extend_from_slice(&previous.content[self.0[0]..]);
        scene
            .targets
            .extend_from_slice(&previous.targets[self.0[1]..]);
        scene
            .accessibility
            .extend_from_slice(&previous.accessibility[self.0[2]..]);
        scene.draw.extend_from_slice(&previous.draw[self.0[3]..]);
        scene
            .measurements
            .extend_from_slice(&previous.measurements[self.0[4]..]);
        scene
            .charts
            .extend_from_slice(&previous.charts[self.0[5]..]);
        scene
            .tooltips
            .extend_from_slice(&previous.tooltips[self.0[6]..]);
        scene
            .session_rows
            .extend_from_slice(&previous.session_rows[self.0[7]..]);
    }
    fn shifted(self, old_end: Self, new_end: Self) -> Self {
        Self(std::array::from_fn(|i| {
            self.0[i] - old_end.0[i] + new_end.0[i]
        }))
    }
}

#[derive(Clone, Copy, Debug)]
enum SectionKind {
    Table,
    Recent,
    Detail,
    DetailOverlay,
    Events,
    Usage,
}
impl SectionKind {
    fn region(self) -> ScrollRegion {
        match self {
            Self::Table => ScrollRegion::Table,
            Self::Recent | Self::Detail | Self::DetailOverlay => ScrollRegion::Side,
            Self::Events => ScrollRegion::Events,
            Self::Usage => ScrollRegion::Usage,
        }
    }
    fn build(self, b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, card: Rect) {
        match self {
            Self::Table => sessions(b, v, s, card),
            Self::Recent => recent(b, v, s, card),
            Self::Detail => {
                if let Some(d) = &v.detail {
                    detail(b, v, s, card, d);
                }
            }
            Self::DetailOverlay => detail_overlay(b, v, s, card),
            Self::Events => events(b, v, s, card),
            Self::Usage => usage(b, v, s, card),
        }
    }
}
#[derive(Clone, Debug)]
struct SceneSection {
    kind: SectionKind,
    card: Rect,
    clip: Rect,
    scope: String,
    start: SceneCursor,
    end: SceneCursor,
}
#[derive(Clone, Debug, Default)]
pub(crate) struct SceneSections(Vec<SceneSection>);
impl SceneSections {
    /// Read-only identity check for the conservative Table-card damage path.
    pub(crate) fn table_cards_for_damage(&self, previous: &Self) -> Option<(Rect, Rect)> {
        fn table(sections: &SceneSections) -> Option<&SceneSection> {
            let mut tables = sections
                .0
                .iter()
                .filter(|s| matches!(s.kind, SectionKind::Table));
            let table = tables.next()?;
            tables.next().is_none().then_some(table)
        }
        let old = table(previous)?;
        let new = table(self)?;
        if old.card != new.card || old.clip != new.clip || old.scope != new.scope {
            return None;
        }
        Some((old.card, new.card))
    }

    fn build(
        &mut self,
        kind: SectionKind,
        b: &mut Builder<'_>,
        v: &m::DashboardView,
        s: &DashboardState,
        card: Rect,
    ) {
        let mut section = SceneSection {
            kind,
            card,
            clip: b.clip,
            scope: b.scope.clone(),
            start: SceneCursor::at(&b.scene),
            end: SceneCursor::default(),
        };
        kind.build(b, v, s, card);
        section.end = SceneCursor::at(&b.scene);
        self.0.push(section);
    }

    /// Caller retains the same immutable view and validates non-scroll inputs.
    pub(crate) fn rebuild(
        &self,
        view: &m::DashboardView,
        state: &DashboardState,
        previous: &Scene,
        region: ScrollRegion,
        metrics: &Metrics,
    ) -> Option<(Scene, Self)> {
        if previous.modal || state.popup.is_some() {
            return None;
        }
        let index = self
            .0
            .iter()
            .position(|section| section.kind.region() == region)?;
        let section = &self.0[index];
        let mut scene = previous.clone();
        section.start.truncate(&mut scene);
        let mut b = Builder {
            scene,
            metrics,
            clip: section.clip,
            scope: section.scope.clone(),
            enabled: view.connection.core_ok,
        };
        section.kind.build(&mut b, view, state, section.card);
        if !view.connection.core_ok {
            for target in &mut b.scene.targets[section.start.0[1]..] {
                target.enabled = false;
            }
            for node in &mut b.scene.accessibility[section.start.0[2]..] {
                if node.target.is_some() {
                    node.enabled = false;
                }
            }
        }
        let end = SceneCursor::at(&b.scene);
        section.end.append_suffix(previous, &mut b.scene);
        let mut sections = self.clone();
        sections.0[index].end = end;
        for suffix in &mut sections.0[index + 1..] {
            suffix.start = suffix.start.shifted(section.end, end);
            suffix.end = suffix.end.shifted(section.end, end);
        }
        Some((b.scene, sections))
    }

    pub(crate) fn measurements(&self, region: ScrollRegion) -> Option<std::ops::Range<usize>> {
        let section = self
            .0
            .iter()
            .find(|section| section.kind.region() == region)?;
        Some(section.start.0[4]..section.end.0[4])
    }
}
pub fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect {
        left: x,
        top: y,
        right: x + w.max(0.),
        bottom: y + h.max(0.),
    }
}
pub fn width(r: Rect) -> f32 {
    (r.right - r.left).max(0.)
}
pub fn height(r: Rect) -> f32 {
    (r.bottom - r.top).max(0.)
}
pub fn contains(r: Rect, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}
fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let r = Rect {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

struct Builder<'a> {
    scene: Scene,
    metrics: &'a Metrics,
    clip: Rect,
    scope: String,
    enabled: bool,
}
impl Builder<'_> {
    fn target(
        &mut self,
        r: Rect,
        label: &str,
        target: Target,
        role: Role,
        selected: bool,
        parent: Option<String>,
    ) {
        if let Some(hit) = intersection(r, self.clip) {
            let index = self.semantic(
                format!("{}:control:{target:?}", self.scope),
                parent,
                role,
                label.into(),
                hit,
                Some(target.clone()),
            );
            self.scene.accessibility[index].selected = selected;
            self.scene.targets.push(HitTarget {
                rect: hit,
                target,
                enabled: true,
                tooltip: None,
            });
        }
    }
    fn tooltip(&mut self, r: Rect, text: String) {
        if let Some(rect) = intersection(r, self.clip) {
            self.scene.tooltips.push(TooltipRegion { rect, text });
        }
    }
    fn fill(&mut self, r: Rect, c: Color, radius: f32) {
        self.scene.draw.push(Draw::Fill {
            rect: r,
            color: c,
            radius,
        });
    }
    fn begin_control_paint(&mut self) {
        if !self.enabled {
            self.scene.draw.push(Draw::Alpha(0.5));
        }
    }
    fn end_control_paint(&mut self) {
        if !self.enabled {
            self.scene.draw.push(Draw::Alpha(1.));
        }
    }
    fn stroke(&mut self, r: Rect, c: Color, w: f32, radius: f32) {
        self.scene.draw.push(Draw::Stroke {
            rect: r,
            color: c,
            width: w,
            radius,
        });
    }
    fn line(&mut self, a: [f32; 2], b: [f32; 2], c: Color, w: f32, dash: Option<[f32; 2]>) {
        self.scene.draw.push(Draw::Line {
            from: a,
            to: b,
            color: c,
            width: w,
            dash,
        });
    }
    fn text(
        &mut self,
        r: Rect,
        value: impl Into<String>,
        size: f32,
        bold: bool,
        color: Color,
        wrap: bool,
    ) {
        self.text_face(
            r,
            value,
            size,
            bold,
            color,
            wrap,
            Face::Text,
            TextAlign::Leading,
        );
    }
    fn center_text(
        &mut self,
        r: Rect,
        value: impl Into<String>,
        size: f32,
        bold: bool,
        color: Color,
        wrap: bool,
    ) {
        self.text_face(
            r,
            value,
            size,
            bold,
            color,
            wrap,
            Face::Text,
            TextAlign::Center,
        );
    }
    // Drawing primitive keeps face/style alongside explicit bounds, as in v2.
    #[allow(clippy::too_many_arguments)]
    fn text_face(
        &mut self,
        r: Rect,
        value: impl Into<String>,
        size: f32,
        bold: bool,
        color: Color,
        wrap: bool,
        face: Face,
        align: TextAlign,
    ) {
        let line_height: f32 = if size >= 15. {
            22.
        } else if size <= 12. {
            16.
        } else {
            20.
        };
        let measure = Measure {
            text: value.into(),
            size,
            line_height: if wrap {
                line_height
            } else {
                line_height.min(height(r).max(1.))
            },
            width: width(r),
            bold,
            wrap,
            face,
            align,
        };
        self.scene.measurements.push(measure.clone());
        self.scene.draw.push(Draw::Text {
            rect: r,
            measure,
            color,
        });
    }
    fn measured_width(&mut self, text: &str, size: f32, bold: bool) -> f32 {
        self.measured_width_face(text, size, bold, Face::Text)
    }
    fn measured_width_face(&mut self, text: &str, size: f32, bold: bool, face: Face) -> f32 {
        let m = Measure {
            text: text.into(),
            size,
            line_height: 20.,
            width: 4096.,
            bold,
            wrap: false,
            face,
            align: TextAlign::Leading,
        };
        let width = self
            .metrics
            .widths
            .get(&m.key())
            .copied()
            .unwrap_or_else(|| {
                text.chars()
                    .map(|c| size * if c.is_ascii() { 0.6 } else { 1. })
                    .sum()
            });
        self.scene.measurements.push(m);
        width.ceil()
    }
    fn measured_height(&mut self, text: &str, w: f32, size: f32, bold: bool) -> f32 {
        self.measured_height_aligned(text, w, size, bold, TextAlign::Leading)
    }
    fn measured_height_aligned(
        &mut self,
        text: &str,
        w: f32,
        size: f32,
        bold: bool,
        align: TextAlign,
    ) -> f32 {
        let m = Measure {
            text: text.into(),
            size,
            line_height: if size >= 15. {
                22.
            } else if size <= 12. {
                16.
            } else {
                20.
            },
            width: w.max(1.),
            bold,
            wrap: true,
            face: Face::Text,
            align,
        };
        let h = self.metrics.height(&m);
        self.scene.measurements.push(m);
        h
    }
    fn semantic(
        &mut self,
        key: String,
        parent: Option<String>,
        role: Role,
        name: String,
        r: Rect,
        target: Option<Target>,
    ) -> usize {
        let node = AccessibleNode {
            key,
            parent,
            role,
            name,
            rect: intersection(r, self.clip).unwrap_or(rect(0., 0., 0., 0.)),
            target,
            enabled: true,
            selected: false,
            expanded: None,
            value: None,
            hidden: false,
        };
        self.scene.accessibility.push(node);
        self.scene.accessibility.len() - 1
    }
    fn control(
        &mut self,
        r: Rect,
        label: &str,
        target: Target,
        role: Role,
        selected: bool,
        tip: Option<String>,
    ) {
        self.control_radius(r, label, target, role, selected, tip, 4.);
    }
    #[allow(clippy::too_many_arguments)]
    fn control_radius(
        &mut self,
        r: Rect,
        label: &str,
        target: Target,
        role: Role,
        selected: bool,
        tip: Option<String>,
        radius: f32,
    ) {
        let accent = matches!(&target, Target::OpenTrayAccount(_) | Target::OpenTrayAdd);
        if !self.enabled {
            self.scene.draw.push(Draw::Alpha(0.5));
        }
        if let Some(hit) = intersection(r, self.clip) {
            let idx = self.semantic(
                format!("{}:control:{target:?}", self.scope),
                None,
                role,
                label.into(),
                hit,
                Some(target.clone()),
            );
            self.scene.accessibility[idx].selected = selected;
            if role == Role::ColumnHeader {
                self.scene.accessibility[idx].parent = Some("sessions:table".into());
                if label.is_empty() {
                    self.scene.accessibility[idx].name =
                        crate::i18n::static_text("상태 정렬").into();
                }
            }
            if target == Target::CloseDetail {
                self.scene.accessibility[idx].name =
                    crate::i18n::static_text("세션 상세 닫기").into();
            }
            if target == Target::Search {
                self.scene.accessibility[idx].name = crate::i18n::static_text("세션 검색").into();
            }
            if target == Target::EventSearch {
                self.scene.accessibility[idx].name = crate::i18n::static_text("기록 검색").into();
            }
            if matches!(role, Role::Edit | Role::ComboBox) {
                self.scene.accessibility[idx].value = Some(label.into());
            }
            self.scene.targets.push(HitTarget {
                rect: hit,
                target,
                enabled: true,
                tooltip: tip,
            });
        }
        self.fill(
            r,
            if accent {
                Color::Accent
            } else if selected {
                Color::Select
            } else {
                Color::Surface
            },
            radius,
        );
        self.stroke(
            r,
            if accent { Color::Accent } else { Color::Border },
            1.,
            radius,
        );
        self.text(
            rect(r.left + 8., r.top + 5., width(r) - 16., height(r) - 6.),
            label,
            13.,
            selected,
            if accent {
                Color::OnAccent
            } else if selected {
                Color::Accent
            } else {
                Color::Text
            },
            false,
        );
        if !self.enabled {
            self.scene.draw.push(Draw::Alpha(1.));
        }
    }
    fn card(&mut self, r: Rect) {
        self.fill(r, Color::Surface, 8.);
        self.stroke(r, Color::Border, 1., 8.);
    }
    fn begin_region(
        &mut self,
        region: ScrollRegion,
        viewport: Rect,
        content_height: f32,
        requested: f32,
    ) -> (Rect, f32) {
        let max = (content_height - height(viewport)).max(0.);
        let offset = if requested.is_finite() {
            requested.clamp(0., max)
        } else {
            0.
        };
        self.scene.content.push(Region {
            region,
            viewport,
            content_height,
            offset,
            max_offset: max,
        });
        let old = self.clip;
        self.clip = intersection(old, viewport).unwrap_or(rect(0., 0., 0., 0.));
        self.scene.draw.push(Draw::Clip(self.clip));
        (old, offset)
    }
    fn end_region(&mut self, old: Rect) {
        self.scene.draw.push(Draw::Unclip);
        self.clip = old;
    }
}
impl Scene {
    pub fn build(
        view: &m::DashboardView,
        state: &DashboardState,
        size: [f32; 2],
        metrics: &Metrics,
    ) -> Self {
        Self::build_sections(view, state, size, metrics).0
    }
    pub(crate) fn build_sections(
        view: &m::DashboardView,
        state: &DashboardState,
        size: [f32; 2],
        metrics: &Metrics,
    ) -> (Self, SceneSections) {
        let w = if size[0].is_finite() {
            size[0].max(1.)
        } else {
            1320.
        };
        let h = if size[1].is_finite() {
            size[1].max(1.)
        } else {
            960.
        };
        let outer = rect(0., 0., w, h);
        let mut sections = SceneSections::default();
        let mut b = Builder {
            scene: Scene {
                modal: state.popup.is_some(),
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
            metrics,
            clip: outer,
            scope: "dashboard".into(),
            enabled: view.connection.core_ok,
        };
        b.fill(outer, Color::Page, 0.);
        b.fill(rect(0., 0., w, 48.), Color::Surface, 0.);
        b.line([0., 47.5], [w, 47.5], Color::Border, 1., None);
        app_header(&mut b, view, state, w);
        let mut top = 64.;
        if !view.connection.core_ok {
            b.fill(
                rect(20., 64., w - 40., 28.),
                Color::ChipBg(v2::Tone::Bad),
                4.,
            );
            b.text(
                rect(32., 68., w - 64., 20.),
                crate::i18n::format(
                    "{0}초 후 다시 연결합니다 · {1}",
                    &[
                        (view.connection.retry_seconds).to_string(),
                        (if view.data_at.is_some() {
                            crate::i18n::format(
                                "아래는 {0}의 마지막 값입니다",
                                &[(view.clock.stamp(view.data_at)).to_string()],
                            )
                        } else {
                            crate::i18n::static_text("마지막 갱신 시각 확인 전").into()
                        })
                        .to_string(),
                    ],
                ),
                12.,
                false,
                Color::Bad,
                false,
            );
            top += 40.;
        }
        if view.connection.core_ok {
            for summary in &view.provider_summaries {
                let h = b.measured_height(&summary.forecast_line, w - 40., 12., false);
                let area = rect(20., top, w - 40., h);
                b.text(
                    area,
                    &summary.forecast_line,
                    12.,
                    false,
                    Color::Secondary,
                    true,
                );
                b.semantic(
                    format!("provider:{}:forecast", summary.provider),
                    None,
                    Role::Text,
                    summary.forecast_line.clone(),
                    area,
                    None,
                );
                top += h + 6.;
            }
            top += 6.;
        }
        if let Some(lines) = &view.move_cost_lines {
            for (i, line) in lines.iter().enumerate() {
                let area = rect(20., top + i as f32 * 22., w - 40., 22.);
                b.text(
                    area,
                    line,
                    12.,
                    i == 0,
                    if i == 0 {
                        Color::Text
                    } else {
                        Color::Secondary
                    },
                    false,
                );
                b.semantic(
                    format!("move-cost:{i}"),
                    None,
                    Role::Text,
                    line.clone(),
                    area,
                    None,
                );
            }
            top += 52.;
        }
        let body = rect(20., top, w - 40., (h - top - 20.).max(0.));
        if view.empty == m::EmptyState::NoAccounts {
            b.card(body);
            b.semantic(
                "empty:accounts:title".into(),
                None,
                Role::Text,
                crate::i18n::static_text("아직 계정이 없습니다").into(),
                rect(body.left + 24., body.top + 40., width(body) - 48., 28.),
                None,
            );
            b.semantic("empty:accounts:help".into(),None,Role::Text,crate::i18n::static_text("Claude나 Codex 계정을 추가하세요. 한 계정이 한도에 도달하면 다른 계정으로 이어서 처리합니다.").into(),rect(body.left+24.,body.top+80.,width(body)-48.,48.),None);
            b.text(
                rect(body.left + 24., body.top + 40., width(body) - 48., 28.),
                crate::i18n::static_text("아직 계정이 없습니다"),
                16.,
                true,
                Color::Text,
                false,
            );
            b.text(rect(body.left+24.,body.top+80.,width(body)-48.,48.),crate::i18n::static_text("Claude나 Codex 계정을 추가하세요. 한 계정이 한도에 도달하면 다른 계정으로 이어서 처리합니다."),13.,false,Color::Secondary,true);
            b.control(
                rect(body.left + 24., body.top + 148., 200., 32.),
                crate::i18n::static_text("트레이에서 계정 추가"),
                Target::OpenTrayAdd,
                Role::Button,
                false,
                None,
            );
        } else {
            match state.tab {
                Tab::Overview => overview(&mut b, view, state, body, &mut sections),
                Tab::Events => sections.build(SectionKind::Events, &mut b, view, state, body),
                Tab::Usage => sections.build(SectionKind::Usage, &mut b, view, state, body),
            }
        }
        if !view.connection.core_ok {
            b.scene.draw.push(Draw::Alpha(1.));
        }
        if let Some(popup) = state.popup {
            dropdown(&mut b, popup);
        }
        if !view.connection.core_ok {
            for target in &mut b.scene.targets {
                target.enabled = false;
            }
            for node in &mut b.scene.accessibility {
                if node.target.is_some() {
                    node.enabled = false;
                }
            }
        }
        (b.scene, sections)
    }
    /// Table offset that reveals this row. Native rebuilds before publishing UIA bounds.
    pub fn reveal_session(&self, id: &str) -> Option<f32> {
        let row = self.session_rows.iter().find(|row| row.id == id)?;
        let region = self
            .content
            .iter()
            .find(|region| region.region == ScrollRegion::Table)?;
        let delta = if row.rect.top < region.viewport.top {
            row.rect.top - region.viewport.top
        } else if row.rect.bottom > region.viewport.bottom {
            row.rect.bottom - region.viewport.bottom
        } else {
            0.
        };
        Some((region.offset + delta).clamp(0., region.max_offset))
    }
    pub fn hit_test(&self, x: f32, y: f32) -> Option<&Target> {
        self.targets
            .iter()
            .rev()
            .find(|h| contains(h.rect, x, y))
            .and_then(|h| h.enabled.then_some(&h.target))
    }
    pub fn tooltip_at(&self, x: f32, y: f32) -> Option<&str> {
        if !self.modal
            && let Some(t) = self.tooltips.iter().rev().find(|t| contains(t.rect, x, y))
        {
            return Some(&t.text);
        }
        self.targets
            .iter()
            .rev()
            .find(|h| h.enabled && contains(h.rect, x, y))
            .and_then(|h| h.tooltip.as_deref())
    }
    pub fn focus_order(&self) -> Vec<&Target> {
        let mut order = Vec::new();
        for h in &self.targets {
            if h.enabled
                && h.target != Target::DetailSurface
                && (!self.modal || matches!(h.target, Target::Range(_) | Target::EventDays(_)))
                && !order.contains(&&h.target)
            {
                order.push(&h.target);
            }
        }
        order
    }
    pub fn chart_tooltip(&self, account: &str, x: f32) -> Option<ChartTooltip> {
        if !x.is_finite() {
            return None;
        }
        let chart = self.charts.iter().find(|c| c.id == account)?;
        if let Some(summary) = &chart.provider_summary {
            return Some(ChartTooltip {
                text: summary.clone(),
                position: [x.clamp(chart.rect.left, chart.rect.right), chart.rect.top],
            });
        }
        let ratio = ((x - chart.rect.left) / width(chart.rect).max(1.)).clamp(0., 1.);
        let millis = chart
            .to
            .signed_duration_since(chart.from)
            .num_milliseconds();
        let at = chart.from + chrono::Duration::milliseconds((millis as f32 * ratio) as i64);
        if at > chart.now {
            return None;
        }
        let mut parts = vec![];
        for s in &chart.series {
            let mut comparisons = 0;
            let value = history::value_at(s, at, chart.unconfirmed, &mut comparisons);
            parts.push(crate::i18n::format(
                "{0} {1} 남음",
                &[
                    (s.name).to_string(),
                    (value
                        .map(|n| format!("{}%", n.round() as u8))
                        .unwrap_or_else(m::unknown))
                    .to_string(),
                ],
            ));
        }
        Some(ChartTooltip {
            text: format!(
                "{} · {}",
                crate::i18n::date_time(&at.with_timezone(&chart.offset), "%H:%M"),
                parts.join(" · ")
            ),
            position: [x.clamp(chart.rect.left, chart.rect.right), chart.rect.top],
        })
    }
}

fn app_header(b: &mut Builder<'_>, v: &m::DashboardView, state: &DashboardState, w: f32) {
    let mut x = 20.;
    b.begin_control_paint();
    for (tab, label) in [
        (Tab::Overview, crate::i18n::static_text("개요")),
        (Tab::Events, crate::i18n::static_text("기록")),
        (Tab::Usage, crate::i18n::static_text("사용량")),
    ] {
        let base_width: f32 = if tab == Tab::Events && v.todo > 0 {
            100.
        } else {
            72.
        };
        let tw = if crate::i18n::language() == crate::i18n::UiLanguage::En {
            base_width.max(b.measured_width(label, 13., state.tab == tab) + 24.)
        } else {
            base_width
        };
        let r = rect(x, 8., tw, 40.);
        b.target(
            r,
            label,
            Target::Tab(tab),
            Role::Tab,
            state.tab == tab,
            None,
        );
        b.text(
            rect(
                x + 12.,
                19.,
                if crate::i18n::language() == crate::i18n::UiLanguage::En {
                    tw - 24.
                } else {
                    48.
                },
                20.,
            ),
            label,
            13.,
            state.tab == tab,
            Color::Text,
            false,
        );
        if tab == Tab::Events && v.todo > 0 {
            b.text(
                rect(x + 48., 19., 52., 20.),
                crate::i18n::format("할 일 {0}", &[(v.todo).to_string()]),
                12.,
                true,
                Color::Bad,
                false,
            );
        }
        if state.tab == tab {
            b.fill(rect(x + 12., 45., tw - 24., 3.), Color::Accent, 0.);
        }
        x += tw;
    }
    b.end_control_paint();
    if !v.connection.core_ok {
        b.text(
            rect((w - 330.).max(x + 16.), 16., 310., 22.),
            crate::i18n::static_text("코어에 연결할 수 없습니다"),
            13.,
            true,
            Color::Bad,
            false,
        );
        return;
    }
    let counts = crate::i18n::format(
        "세션 {0}개    활성 {1}개    요청 중 {2}개",
        &[
            (v.session_count).to_string(),
            (v.active_count).to_string(),
            (v.inflight_text()).to_string(),
        ],
    );
    let cw = if w >= 1180. {
        b.measured_width(&counts, 12., false)
    } else {
        0.
    };
    let right = w - 20. - if cw > 0. { cw + 16. } else { 0. };
    if cw > 0. {
        let r = rect(w - 20. - cw, 17., cw, 20.);
        b.text(r, counts.clone(), 12., false, Color::Secondary, false);
        b.semantic("header:counts".into(), None, Role::Text, counts, r, None);
    } else {
        let index = b.semantic(
            "header:counts".into(),
            None,
            Role::Text,
            counts,
            rect(0., 0., 0., 0.),
            None,
        );
        b.scene.accessibility[index].hidden = true;
    }
    let parts: Vec<_> = v
        .provider_summaries
        .iter()
        .take(2)
        .map(|summary| {
            let provider_w = b.measured_width(&summary.provider_label, 12., true);
            let verdict_w = b.measured_width(summary.verdict.text(), 12., false);
            let todo = if summary.todo > 0 {
                crate::i18n::format(" · 할 일 {0}", &[(summary.todo).to_string()])
            } else {
                String::new()
            };
            let todo_w = if todo.is_empty() {
                0.
            } else {
                b.measured_width(&todo, 12., true)
            };
            let detail_w = if summary.detail.is_empty() {
                0.
            } else {
                b.measured_width(&summary.detail, 12., false) + 6.
            };
            (provider_w, verdict_w, todo, todo_w, detail_w)
        })
        .collect();
    let separators = parts.len().saturating_sub(1) as f32 * 25.;
    let fixed = parts
        .iter()
        .map(|(p, verdict, _, todo, _)| 16. + p + 6. + verdict + todo)
        .sum::<f32>()
        + separators;
    let mut room = (right - 300. - fixed).max(0.);
    let details: Vec<_> = parts
        .iter()
        .map(|part| {
            let width = part.4.min(room);
            room -= width;
            width
        })
        .collect();
    let mut sx = (right - fixed - details.iter().sum::<f32>()).max(300.);
    for (i, (summary, (provider_w, verdict_w, todo, todo_w, _))) in
        v.provider_summaries.iter().zip(parts).enumerate()
    {
        let left = sx;
        b.fill(rect(sx, 20., 8., 8.), Color::Verdict(summary.tone), 4.);
        sx += 16.;
        b.text(
            rect(sx, 16., provider_w, 20.),
            summary.provider_label.clone(),
            12.,
            true,
            Color::Text,
            false,
        );
        sx += provider_w + 6.;
        b.text(
            rect(sx, 16., verdict_w, 20.),
            summary.verdict.text(),
            12.,
            false,
            Color::Text,
            false,
        );
        sx += verdict_w;
        if details[i] > 0. {
            b.text(
                rect(sx + 6., 16., (details[i] - 6.).max(0.), 20.),
                summary.detail.clone(),
                12.,
                false,
                Color::Secondary,
                false,
            );
            sx += details[i];
        }
        if todo_w > 0. {
            b.text(
                rect(sx, 16., todo_w, 20.),
                todo,
                12.,
                true,
                Color::Bad,
                false,
            );
            sx += todo_w;
        }
        b.semantic(
            format!("summary:{i}"),
            None,
            Role::Text,
            summary.accessible_name.clone(),
            rect(left, 12., sx - left, 24.),
            None,
        );
        if i + 1 < v.provider_summaries.len().min(2) {
            b.line([sx + 12., 12.], [sx + 12., 36.], Color::Border, 1., None);
            sx += 25.;
        }
    }
}
fn dropdown(b: &mut Builder<'_>, popup: Popup) {
    let anchor_target = match popup {
        Popup::Range => Target::RangeMenu,
        Popup::EventDays => Target::EventDaysMenu,
    };
    let Some(anchor) = b
        .scene
        .targets
        .iter()
        .find(|h| h.target == anchor_target)
        .map(|h| h.rect)
    else {
        return;
    };
    let items: Vec<(String, Target)> = match popup {
        Popup::Range => Range::ALL
            .iter()
            .map(|range| (range.label().into(), Target::Range(*range)))
            .collect(),
        Popup::EventDays => [
            (1, crate::i18n::static_text("오늘")),
            (2, crate::i18n::static_text("최근 2일")),
            (7, crate::i18n::static_text("최근 7일")),
        ]
        .into_iter()
        .map(|(days, label)| (label.into(), Target::EventDays(days)))
        .collect(),
    };
    let h = items.len() as f32 * 32. + 8.;
    let w = 180.;
    let x = anchor.left.min((b.scene.rect.right - w - 8.).max(0.));
    let y = (anchor.bottom + 4.).min((b.scene.rect.bottom - h - 8.).max(0.));
    let r = rect(x, y, w, h);
    for hit in &mut b.scene.targets {
        hit.enabled = false;
    }
    for node in &mut b.scene.accessibility {
        if node.target.is_some() {
            node.enabled = false;
        }
    }
    b.scene.targets.push(HitTarget {
        rect: b.scene.rect,
        target: Target::DismissPopup,
        enabled: true,
        tooltip: None,
    });
    b.card(r);
    for (i, (label, target)) in items.iter().enumerate() {
        b.control(
            rect(x + 4., y + 4. + i as f32 * 32., w - 8., 32.),
            label,
            target.clone(),
            Role::Button,
            false,
            None,
        );
    }
}

fn overview(
    b: &mut Builder<'_>,
    v: &m::DashboardView,
    s: &DashboardState,
    body: Rect,
    sections: &mut SceneSections,
) {
    let narrow = width(body) < 1140.;
    let (table, side) = if narrow {
        (
            rect(
                body.left,
                body.top,
                width(body),
                (height(body) - 56.).max(0.),
            ),
            rect(body.left, body.bottom - 44., width(body), 44.),
        )
    } else {
        (
            rect(body.left, body.top, width(body) - 376., height(body)),
            rect(body.right - 360., body.top, 360., height(body)),
        )
    };
    sections.build(SectionKind::Table, b, v, s, table);
    if narrow {
        recent_strip(b, v, side);
        if v.detail.is_some() {
            let overlay = rect(
                body.right - 360.,
                body.top + 16.,
                360.,
                (height(body) - 16.).max(0.),
            );
            sections.build(SectionKind::DetailOverlay, b, v, s, overlay);
        }
    } else if v.detail.is_some() {
        sections.build(SectionKind::Detail, b, v, s, side);
    } else {
        sections.build(SectionKind::Recent, b, v, s, side);
    }
}
fn detail_overlay(b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, overlay: Rect) {
    b.scene.draw.push(Draw::Alpha(0.03));
    for spread in 1..=8 {
        let spread = spread as f32;
        b.fill(
            rect(
                overlay.left - spread,
                overlay.top + 8. - spread,
                width(overlay) + 2. * spread,
                height(overlay) + 2. * spread,
            ),
            Color::Text,
            8. + spread,
        );
    }
    b.scene.draw.push(Draw::Alpha(1.));
    if let Some(d) = &v.detail {
        detail(b, v, s, overlay, d);
    }
}
fn recent_strip(b: &mut Builder<'_>, v: &m::DashboardView, r: Rect) {
    b.card(r);
    b.text(
        rect(r.left + 16., r.top + 12., 58., 20.),
        crate::i18n::static_text("최근 일"),
        13.,
        true,
        Color::Text,
        false,
    );
    let link = rect(r.right - 130., r.top + 8., 114., 28.);
    b.target(
        link,
        crate::i18n::static_text("기록 모두 보기"),
        Target::AllEvents,
        Role::Button,
        false,
        None,
    );
    b.begin_control_paint();
    b.text(
        rect(link.left, link.top + 5., width(link), 18.),
        crate::i18n::static_text("기록 모두 보기 ›"),
        12.,
        false,
        Color::Accent,
        false,
    );
    b.end_control_paint();
    if let Some(event) = v.recent.first() {
        let tag_w = b.measured_width(&event.tag, 12., true) + 16.;
        chip(
            b,
            &event.tag,
            event.tone,
            rect(r.left + 82., r.top + 12., tag_w, 20.),
            4.,
        );
        let x = r.left + 82. + tag_w + 12.;
        b.text(
            rect(x, r.top + 13., (link.left - x - 66.).max(0.), 18.),
            format!("{} · {}", event.title, event.body),
            12.,
            false,
            Color::Text,
            false,
        );
        b.text(
            rect(link.left - 58., r.top + 13., 48., 18.),
            event.time.clone(),
            12.,
            false,
            Color::Secondary,
            false,
        );
    } else {
        b.text(
            rect(
                r.left + 82.,
                r.top + 13.,
                (link.left - r.left - 98.).max(0.),
                18.,
            ),
            v.event_empty.clone(),
            12.,
            false,
            Color::Secondary,
            false,
        );
    }
}
const EMPTY_SESSION_HELP: &str = "codex-qb나 claude-qb로 실행한 Codex·Claude가 요청을 보내면 여기에 바로 나타납니다. 그냥 codex나 claude로 실행하면 quota-baton을 거치지 않습니다.";

struct EmptyHelpRun {
    command: &'static str,
    text: &'static str,
    command_width: f32,
    text_width: f32,
}

fn empty_help_lines(b: &mut Builder<'_>, available: f32, gap: f32) -> Vec<Vec<EmptyHelpRun>> {
    let mut lines = vec![Vec::new()];
    let mut line_width = 0.;
    for token in crate::i18n::static_text(EMPTY_SESSION_HELP).split_whitespace() {
        let command = ["codex-qb", "claude-qb", "codex", "claude"]
            .into_iter()
            .find(|command| token.starts_with(*command))
            .unwrap_or("");
        let text = &token[command.len()..];
        let command_width = if command.is_empty() {
            0.
        } else {
            b.measured_width_face(command, 12., false, Face::Mono) + 14.
        };
        let text_width = if text.is_empty() {
            0.
        } else {
            b.measured_width(text, 13., false)
        };
        let run_width = command_width + text_width;
        if line_width > 0. && line_width + gap + run_width > available {
            lines.push(Vec::new());
            line_width = 0.;
        }
        if line_width > 0. {
            line_width += gap;
        }
        line_width += run_width;
        lines.last_mut().unwrap().push(EmptyHelpRun {
            command,
            text,
            command_width,
            text_width,
        });
    }
    lines
}

fn sessions(b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, card: Rect) {
    b.card(card);
    b.semantic(
        "sessions:table".into(),
        None,
        Role::Table,
        crate::i18n::static_text("세션").into(),
        card,
        None,
    );
    let y = card.top + 12.;
    let title_width = if crate::i18n::language() == crate::i18n::UiLanguage::En {
        b.measured_width(crate::i18n::static_text("세션"), 14., true) + 4.
    } else {
        42.
    };
    b.text(
        rect(card.left + 16., y + 5., title_width, 20.),
        crate::i18n::static_text("세션"),
        14.,
        true,
        Color::Text,
        false,
    );
    let start = card.left
        + if crate::i18n::language() == crate::i18n::UiLanguage::En {
            16. + title_width + 12.
        } else {
            64.
        };
    let segment = rect(start, y, 192., 28.);
    b.begin_control_paint();
    b.stroke(segment, Color::Border, 1., 4.);
    for (i, (filter, label)) in [
        (ProviderFilter::All, crate::i18n::static_text("전체")),
        (ProviderFilter::Claude, "Claude"),
        (ProviderFilter::Codex, "Codex"),
    ]
    .iter()
    .enumerate()
    {
        let r = rect(start + i as f32 * 64., y, 64., 28.);
        let selected = s.provider == *filter;
        if selected {
            b.fill(rect(r.left + 1., r.top + 1., 62., 26.), Color::Select, 0.);
        }
        if i > 0 {
            b.line([r.left, r.top], [r.left, r.bottom], Color::Border, 1., None);
        }
        let text = format!("{label} {}", v.provider_counts[i]);
        b.target(
            r,
            &text,
            Target::Provider(*filter),
            Role::Button,
            selected,
            None,
        );
        b.center_text(
            rect(r.left + 2., r.top + 5., 60., 18.),
            text,
            12.,
            false,
            if selected { Color::Accent } else { Color::Text },
            false,
        );
    }
    b.end_control_paint();
    b.control(
        rect(start + 204., y, 96., 28.),
        s.range.label(),
        Target::RangeMenu,
        Role::ComboBox,
        false,
        None,
    );
    let check = rect(start + 312., y, 134., 28.);
    b.begin_control_paint();
    b.target(
        check,
        crate::i18n::static_text("계정별로 묶기"),
        Target::ToggleGrouping,
        Role::Checkbox,
        s.grouped,
        None,
    );
    let box_rect = rect(check.left, check.top + 6., 16., 16.);
    if s.grouped {
        b.fill(box_rect, Color::Accent, 2.);
        b.text_face(
            rect(box_rect.left + 1., box_rect.top + 1., 14., 14.),
            "\u{e73e}",
            12.,
            false,
            Color::OnAccent,
            false,
            Face::Icon,
            TextAlign::Center,
        );
    } else {
        b.stroke(box_rect, Color::Secondary, 1., 2.);
    }
    b.text(
        rect(check.left + 24., y + 5., 110., 18.),
        crate::i18n::static_text("계정별로 묶기"),
        12.,
        false,
        Color::Text,
        false,
    );
    b.end_control_paint();
    let search_w = (card.right - 16. - check.right - 12.).clamp(100., 220.);
    let search = rect(card.right - search_w - 16., y, search_w, 28.);
    b.begin_control_paint();
    b.fill(search, Color::Surface, 4.);
    b.stroke(search, Color::Border, 1., 4.);
    b.target(
        search,
        crate::i18n::static_text("세션 검색"),
        Target::Search,
        Role::Edit,
        false,
        None,
    );
    b.text(
        rect(search.left + 30., y + 5., search_w - 38., 18.),
        if s.search.is_empty() {
            crate::i18n::static_text("폴더, 세션 번호, 계정 검색")
        } else {
            &s.search
        },
        12.,
        false,
        if s.search.is_empty() {
            Color::Secondary
        } else {
            Color::Text
        },
        false,
    );
    b.text_face(
        rect(search.left + 8., y + 6., 14., 16.),
        "\u{e721}",
        12.,
        false,
        Color::Secondary,
        false,
        Face::Icon,
        TextAlign::Leading,
    );
    b.end_control_paint();
    if let Some(node) = b
        .scene
        .accessibility
        .iter_mut()
        .rev()
        .find(|n| n.target == Some(Target::Search))
    {
        node.value = Some(s.search.clone());
    }
    let no_sessions = matches!(v.empty, m::EmptyState::NoSessions(_));
    let header = rect(
        card.left + 1.,
        card.top + 52.,
        width(card) - 2.,
        if no_sessions { 0. } else { 32. },
    );
    b.fill(header, Color::Footer, 0.);
    let control_width = v
        .flat
        .iter()
        .map(|row| {
            let controls = session_control_layout(b, row, 0., &v.clock)
                .last()
                .map_or(0., |(_, r, _)| r.right);
            if row
                .memo
                .starts_with(crate::i18n::static_text("이동 (Claude는 자동 재개 미지원)"))
            {
                controls + b.measured_width(crate::i18n::static_text("이동"), 12., false)
            } else {
                controls
            }
        })
        .fold(0., f32::max);
    let columns = session_columns(width(card) - 50., !s.grouped, control_width);
    let mut cx = card.left + 25.;
    if !no_sessions {
        for (key, label, cw) in &columns {
            if s.grouped {
                b.text(
                    rect(cx, header.top + 8., *cw, 20.),
                    *label,
                    12.,
                    true,
                    Color::Secondary,
                    false,
                );
            } else {
                b.control(
                    rect(cx, header.top + 2., *cw, 28.),
                    label,
                    Target::Sort(*key),
                    Role::ColumnHeader,
                    s.sort == *key,
                    Some(
                        if s.sort == *key {
                            if s.descending {
                                crate::i18n::static_text("내림차순")
                            } else {
                                crate::i18n::static_text("오름차순")
                            }
                        } else {
                            crate::i18n::static_text("정렬")
                        }
                        .into(),
                    ),
                );
            }
            cx += *cw;
        }
    }
    let footer_height = if v.counters_unknown { 52. } else { 32. };
    let foot = rect(
        card.left + 1.,
        card.bottom - footer_height,
        width(card) - 2.,
        footer_height - 1.,
    );
    b.fill(foot, Color::Footer, 0.);
    if v.counters_unknown {
        let bounds = rect(foot.left + 12., foot.top + 30., width(foot) - 24., 18.);
        let notice = crate::i18n::static_text(
            "오늘 요청 수 확인 전 · 이전 실행의 집계는 확인할 수 없습니다.",
        );
        b.text(bounds, notice, 12., false, Color::Secondary, false);
        b.semantic(
            "sessions:counters-unknown".into(),
            Some("sessions:table".into()),
            Role::Text,
            notice.into(),
            bounds,
            None,
        );
    }
    let mut fx = foot.left + 12.;
    for (kind, label) in [
        (0, crate::i18n::static_text("요청 중")),
        (1, crate::i18n::static_text("5분 안에 활동")),
        (2, crate::i18n::static_text("쉬는 중")),
    ] {
        let dot = rect(fx, foot.top + 13., 7., 7.);
        match kind {
            0 => {
                b.fill(rect(fx - 3., foot.top + 10., 13., 13.), Color::Select, 7.);
                b.fill(dot, Color::Accent, 4.);
            }
            1 => b.stroke(dot, Color::Accent, 2., 4.),
            _ => b.fill(dot, Color::Secondary, 4.),
        }
        let label_width = b.measured_width(label, 12., false);
        b.text(
            rect(fx + 13., foot.top + 8., label_width, 18.),
            label,
            12.,
            false,
            Color::Secondary,
            false,
        );
        fx += label_width + 28.;
    }
    let time = format!(
        "{} · {}{}",
        if v.data_at.is_some() {
            crate::i18n::format("{0} 기준", &[(v.clock.stamp(v.data_at)).to_string()])
        } else {
            crate::i18n::static_text("갱신 시각 확인 전").into()
        },
        if v.connection.stream_ok {
            crate::i18n::static_text("바뀌면 바로 반영")
        } else {
            crate::i18n::static_text("다시 연결 중")
        },
        if v.truncated {
            crate::i18n::static_text(" · 최근 2,000개")
        } else {
            ""
        }
    );
    let tw = b.measured_width(&time, 12., false);
    b.text(
        rect(foot.right - 12. - tw, foot.top + 8., tw, 18.),
        time,
        12.,
        false,
        if v.connection.stream_ok {
            Color::Secondary
        } else {
            Color::Warn
        },
        false,
    );
    b.text(
        rect(fx, foot.top + 8., (foot.right - 24. - tw - fx).max(0.), 18.),
        crate::i18n::static_text("활성 세션 = 요청 중 + 5분 안에 활동"),
        12.,
        false,
        Color::Secondary,
        false,
    );
    let viewport = rect(
        card.left + 1.,
        header.bottom,
        width(card) - 2.,
        (foot.top - header.bottom).max(0.),
    );
    let empty_width = (width(viewport) - 48.).clamp(1., 620.);
    let empty_title_height = if let m::EmptyState::NoSessions(title) = &v.empty {
        b.measured_height_aligned(title, empty_width, 16., true, TextAlign::Center)
    } else {
        0.
    };
    let empty_help_gap = if no_sessions {
        b.measured_width(" ", 13., false)
    } else {
        0.
    };
    let empty_help_lines = if no_sessions {
        empty_help_lines(b, empty_width, empty_help_gap)
    } else {
        Vec::new()
    };
    let empty_help_height = empty_help_lines.len() as f32 * 20.;
    let empty_last_height = if no_sessions {
        v.last_session
            .as_ref()
            .map(|last| b.measured_height_aligned(last, empty_width, 12., false, TextAlign::Center))
            .unwrap_or(0.)
    } else {
        0.
    };
    let empty_button = matches!(s.range, Range::Hour | Range::Active);
    let empty_height = 48.
        + 20.
        + empty_title_height
        + 12.
        + empty_help_height
        + 16.
        + if empty_button { 32. } else { 0. }
        + if v.last_session.is_some() {
            16. + empty_last_height
        } else {
            0.
        };
    let content = if no_sessions {
        empty_height
    } else if matches!(v.empty, m::EmptyState::NoMatch(_)) {
        120.
    } else if s.grouped {
        let mut provider = "";
        let mut total = 4.;
        for g in &v.groups {
            if provider != g.provider {
                total += if provider.is_empty() { 24. } else { 28. };
                provider = &g.provider;
            }
            total += group_header_height(b, g, width(viewport) - 22.) + 2. + 10.;
            if !g.collapsed {
                total += if g.rows.is_empty() {
                    32.
                } else {
                    g.rows
                        .iter()
                        .map(|row| session_height(b, row, width(viewport) - 24., &v.clock))
                        .sum()
                };
            }
        }
        total + 12.
    } else {
        v.flat
            .iter()
            .map(|row| session_height(b, row, width(viewport) - 22., &v.clock))
            .sum::<f32>()
            + 8.
    };
    let (old, offset) = b.begin_region(ScrollRegion::Table, viewport, content, s.table_offset);
    let mut ry = viewport.top - offset;
    match &v.empty {
        m::EmptyState::NoSessions(title) => {
            let cx = viewport.left + width(viewport) / 2.;
            ry += ((height(viewport) - empty_height) / 2.).max(0.);
            let left = cx - empty_width / 2.;
            b.fill(rect(cx - 24., ry, 48., 48.), Color::Footer, 24.);
            b.text_face(
                rect(cx - 12., ry + 12., 24., 24.),
                "\u{e8f2}",
                24.,
                false,
                Color::Secondary,
                false,
                Face::Icon,
                TextAlign::Center,
            );
            ry += 68.;
            b.semantic(
                "empty:sessions".into(),
                Some("sessions:table".into()),
                Role::Text,
                title.clone(),
                rect(left, ry, empty_width, empty_title_height),
                None,
            );
            b.center_text(
                rect(left, ry, empty_width, empty_title_height),
                title.clone(),
                16.,
                true,
                Color::Text,
                true,
            );
            ry += empty_title_height + 12.;
            b.semantic(
                "empty:sessions:help".into(),
                Some("empty:sessions".into()),
                Role::Text,
                crate::i18n::static_text(EMPTY_SESSION_HELP).into(),
                rect(left, ry, empty_width, empty_help_height),
                None,
            );
            for line in empty_help_lines {
                let line_width = line
                    .iter()
                    .map(|run| run.command_width + run.text_width)
                    .sum::<f32>()
                    + line.len().saturating_sub(1) as f32 * empty_help_gap;
                let mut x = cx - line_width / 2.;
                for run in line {
                    if !run.command.is_empty() {
                        let r = rect(x, ry, run.command_width, 20.);
                        b.fill(r, Color::Footer, 4.);
                        b.stroke(r, Color::Border, 1., 4.);
                        b.text_face(
                            rect(r.left + 7., r.top + 2., width(r) - 14., 16.),
                            run.command,
                            12.,
                            false,
                            Color::Text,
                            false,
                            Face::Mono,
                            TextAlign::Leading,
                        );
                    }
                    if !run.text.is_empty() {
                        b.text(
                            rect(x + run.command_width, ry, run.text_width, 20.),
                            run.text,
                            13.,
                            false,
                            Color::Secondary,
                            false,
                        );
                    }
                    x += run.command_width + run.text_width + empty_help_gap;
                }
                ry += 20.;
            }
            ry += 16.;
            if empty_button {
                b.control(
                    rect(cx - 75., ry, 150., 32.),
                    crate::i18n::static_text("오늘 전체 보기"),
                    Target::Today,
                    Role::Button,
                    false,
                    None,
                );
                ry += 32.;
            }
            if let Some(last) = &v.last_session {
                ry += 16.;
                b.center_text(
                    rect(left, ry, empty_width, empty_last_height),
                    last.clone(),
                    12.,
                    false,
                    Color::Secondary,
                    true,
                );
            }
        }
        m::EmptyState::NoMatch(text) => {
            let message = crate::i18n::format("“{0}”와 맞는 세션이 없습니다", &[text.to_string()]);
            let available = (width(viewport) - 80.).max(1.);
            let message_height = b
                .measured_height(&message, available, 14., false)
                .min((height(viewport) - 80.).max(20.));
            let message_rect = rect(
                viewport.left + 40.,
                viewport.top + 40.,
                available,
                message_height,
            );
            b.semantic(
                "empty:search".into(),
                Some("sessions:table".into()),
                Role::Text,
                message.clone(),
                message_rect,
                None,
            );
            b.center_text(message_rect, message, 14., false, Color::Secondary, true)
        }
        _ => {
            if s.grouped {
                let mut provider = "";
                let mut pinned = None;
                ry += 4.;
                for group in &v.groups {
                    if provider != group.provider {
                        let ph = if provider.is_empty() { 24. } else { 28. };
                        b.text(
                            rect(
                                viewport.left + 12.,
                                ry + ph - 18.,
                                width(viewport) - 24.,
                                16.,
                            ),
                            v2::provider_label(&group.provider),
                            12.,
                            true,
                            Color::Secondary,
                            false,
                        );
                        ry += ph;
                        provider = &group.provider;
                    }
                    let header_height = group_header_height(b, group, width(viewport) - 22.);
                    let rows_height = if group.collapsed {
                        0.
                    } else if group.rows.is_empty() {
                        32.
                    } else {
                        group
                            .rows
                            .iter()
                            .map(|row| session_height(b, row, width(viewport) - 24., &v.clock))
                            .sum()
                    };
                    let box_rect = rect(
                        viewport.left + 11.,
                        ry,
                        width(viewport) - 22.,
                        header_height + rows_height + 2.,
                    );
                    let description = crate::i18n::format(
                        "{0} 계정, {1}, {2}, 활성 {3}{4}, {5}",
                        &[
                            (group.name).to_string(),
                            (group.plan).to_string(),
                            (group.chip).to_string(),
                            (group.active).to_string(),
                            (if group.active >= group.threshold.unwrap_or(5).max(1) {
                                crate::i18n::static_text(" · 몰림")
                            } else {
                                ""
                            })
                            .to_string(),
                            (if group.relogin_reason.is_empty() {
                                group
                                    .windows
                                    .iter()
                                    .map(|w| format!("{} {} {}", w.label, w.remaining, w.reset))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            } else {
                                group.relogin_reason.clone()
                            })
                            .to_string(),
                        ],
                    );
                    b.semantic(
                        format!("group:{}", group.id),
                        Some("sessions:table".into()),
                        Role::Group,
                        description,
                        box_rect,
                        None,
                    );
                    b.fill(box_rect, Color::Surface, 6.);
                    b.stroke(box_rect, Color::GroupBorder, 1., 6.);
                    group_header(
                        b,
                        group,
                        rect(
                            box_rect.left + 1.,
                            ry + 1.,
                            width(box_rect) - 2.,
                            header_height,
                        ),
                        false,
                    );
                    if !group.collapsed {
                        b.line(
                            [box_rect.left, ry + header_height + 1.],
                            [box_rect.right, ry + header_height + 1.],
                            Color::Border,
                            1.,
                            None,
                        );
                    }
                    if box_rect.top < viewport.top
                        && box_rect.bottom > viewport.top
                        && !group.collapsed
                    {
                        pinned = Some((
                            group,
                            rect(
                                box_rect.left + 1.,
                                viewport.top.min(box_rect.bottom - header_height),
                                width(box_rect) - 2.,
                                header_height,
                            ),
                        ));
                    }
                    ry += header_height + 1.;
                    if !group.collapsed {
                        if group.rows.is_empty() {
                            b.text(
                                rect(box_rect.left + 32., ry + 8., width(box_rect) - 44., 16.),
                                group.no_sessions.clone(),
                                12.,
                                false,
                                Color::Secondary,
                                false,
                            );
                        } else {
                            for (index, row) in group.rows.iter().enumerate() {
                                let row_height =
                                    session_height(b, row, width(box_rect) - 2., &v.clock);
                                session(
                                    b,
                                    row,
                                    rect(box_rect.left + 1., ry, width(box_rect) - 2., row_height),
                                    &columns,
                                    Some(format!("group:{}", group.id)),
                                    &v.clock,
                                );
                                if index + 1 < group.rows.len() {
                                    b.line(
                                        [box_rect.left + 1., ry + row_height - 0.5],
                                        [box_rect.right - 1., ry + row_height - 0.5],
                                        Color::Border,
                                        1.,
                                        None,
                                    );
                                }
                                ry += row_height;
                            }
                        }
                    }
                    ry = box_rect.bottom + 10.;
                }
                if let Some((group, pinned_rect)) = pinned {
                    b.scene.draw.push(Draw::Alpha(0.02));
                    for spread in 1..=4 {
                        let spread = spread as f32;
                        b.fill(
                            rect(
                                pinned_rect.left - spread * 0.5,
                                pinned_rect.top + 2.,
                                width(pinned_rect) + spread,
                                height(pinned_rect) + spread,
                            ),
                            Color::Text,
                            6. + spread,
                        );
                    }
                    b.scene.draw.push(Draw::Alpha(1.));
                    group_header(b, group, pinned_rect, true);
                    b.stroke(pinned_rect, Color::GroupBorder, 1., 6.);
                }
            } else {
                for row in &v.flat {
                    let row_height = session_height(b, row, width(viewport) - 22., &v.clock);
                    session(
                        b,
                        row,
                        rect(viewport.left + 11., ry, width(viewport) - 22., row_height),
                        &columns,
                        None,
                        &v.clock,
                    );
                    ry += row_height;
                }
            }
        }
    }
    b.end_region(old);
}
pub fn columns(available: f32, flat: bool) -> Vec<(SortKey, &'static str, f32)> {
    let mut cols = vec![
        (SortKey::Status, "", 20.),
        (SortKey::Session, crate::i18n::static_text("세션"), 188.),
    ];
    if flat {
        cols.push((SortKey::Account, crate::i18n::static_text("계정"), 160.));
    }
    cols.extend([
        (
            SortKey::Client,
            crate::i18n::static_text("클라이언트 · 번호"),
            172.,
        ),
        (SortKey::Model, crate::i18n::static_text("모델"), 116.),
        (SortKey::Start, crate::i18n::static_text("시작"), 64.),
        (
            SortKey::LastActivity,
            crate::i18n::static_text("마지막 활동"),
            96.,
        ),
        (
            SortKey::Requests,
            crate::i18n::static_text("오늘 요청"),
            if crate::i18n::language() == crate::i18n::UiLanguage::En {
                84.
            } else {
                64.
            },
        ),
    ]);
    let used = cols.iter().map(|c| c.2).sum::<f32>();
    if available - used >= 96. {
        cols.push((
            SortKey::Memo,
            crate::i18n::static_text("메모"),
            available - used,
        ));
    } else {
        if let Some(column) = cols.iter_mut().find(|column| column.0 == SortKey::Session) {
            column.2 = (column.2 - (used - available).max(0.)).clamp(160., 188.);
        }
        for key in [SortKey::Start, SortKey::Model] {
            if cols.iter().map(|c| c.2).sum::<f32>() <= available {
                break;
            }
            cols.retain(|c| c.0 != key);
        }
        let used = cols.iter().map(|c| c.2).sum::<f32>();
        if let Some(col) = cols.iter_mut().find(|c| c.0 == SortKey::Session) {
            col.2 = (col.2 + available - used).clamp(160., 188.);
        }
        // The native minimum window never needs this fallback. Keep callers
        // below that width contained without shrinking the session id column.
        let overflow = (cols.iter().map(|c| c.2).sum::<f32>() - available).max(0.);
        if let Some(col) = cols.iter_mut().find(|c| c.0 == SortKey::Account) {
            col.2 = (col.2 - overflow).max(80.);
        }
    }
    cols
}
// Reserve the Notes cell for real actions before dropping optional data columns.
// The legacy column policy is unchanged when no session supports urgency.
fn session_columns(available: f32, flat: bool, controls: f32) -> Vec<(SortKey, &'static str, f32)> {
    if controls == 0. {
        return columns(available, flat);
    }
    let mut cols = columns(available, flat);
    cols.retain(|c| c.0 != SortKey::Memo);
    let needed = controls + 8.;
    for key in [
        SortKey::Start,
        SortKey::Model,
        SortKey::LastActivity,
        SortKey::Requests,
    ] {
        if cols.iter().map(|c| c.2).sum::<f32>() + needed <= available {
            break;
        }
        cols.retain(|c| c.0 != key);
    }
    let used = cols.iter().map(|c| c.2).sum::<f32>();
    cols.push((
        SortKey::Memo,
        crate::i18n::static_text("메모"),
        (available - used).max(needed),
    ));
    cols
}

fn group_usage_height(g: &m::AccountGroup, available: f32) -> f32 {
    let slots = ((available - 40. + 24.) / 320.).floor().max(1.) as usize;
    60. + g.windows.len().max(1).div_ceil(slots).saturating_sub(1) as f32 * 24.
}
fn group_header_height(b: &mut Builder<'_>, g: &m::AccountGroup, available: f32) -> f32 {
    group_usage_height(g, available)
        + if g.outside_used { 20. } else { 0. }
        + g.forecast_lines
            .iter()
            .map(|line| {
                b.measured_height(line, (available - 46.).max(1.), 12., g.forecast_warning) + 4.
            })
            .sum::<f32>()
}
fn chip(b: &mut Builder<'_>, text: &str, tone: v2::Tone, r: Rect, radius: f32) {
    if text.is_empty() {
        return;
    }
    b.fill(r, Color::ChipBg(tone), radius);
    b.center_text(r, text, 12., true, Color::ChipFg(tone), false);
}
fn usage_bar(b: &mut Builder<'_>, r: Rect, remaining: u8, paused: bool) {
    b.fill(r, Color::Track, 2.);
    if remaining == 0 && !paused {
        b.line(
            [r.left, (r.top + r.bottom) / 2.],
            [r.right, (r.top + r.bottom) / 2.],
            Color::ChipBg(v2::Tone::Limit),
            height(r),
            Some([4., 3.]),
        );
    } else if remaining > 0 {
        b.fill(
            rect(
                r.left,
                r.top,
                width(r) * f32::from(remaining) / 100.,
                height(r),
            ),
            if paused {
                Color::BarPaused
            } else {
                bar_color(remaining)
            },
            2.,
        );
    }
}
fn group_header(b: &mut Builder<'_>, g: &m::AccountGroup, r: Rect, sticky: bool) {
    b.fill(r, Color::Footer, 6.);
    let prior_scope = b.scope.clone();
    if sticky {
        b.scope = format!("sticky:{}", g.id);
    }
    let target = Target::Group(g.id.clone());
    b.target(
        r,
        &crate::i18n::format("{0} 계정 접기/펼치기", &[(g.name).to_string()]),
        target.clone(),
        Role::Button,
        false,
        Some(format!("group:{}", g.id)),
    );
    if let Some(node) = b
        .scene
        .accessibility
        .last_mut()
        .filter(|n| n.target.as_ref() == Some(&target))
    {
        node.expanded = Some(!g.collapsed);
    }
    b.begin_control_paint();
    b.text_face(
        rect(r.left + 12., r.top + 15., 20., 16.),
        if g.collapsed { "\u{e76c}" } else { "\u{e70d}" },
        10.,
        false,
        Color::Secondary,
        false,
        Face::Icon,
        TextAlign::Leading,
    );
    b.end_control_paint();
    let threshold = g.threshold.unwrap_or(5).max(1);
    let crowded = g.active >= threshold;
    let count_text = crate::i18n::format("활성 {0}", &[(g.active).to_string()]);
    let count_width = b.measured_width(&count_text, 12., true).max(34.);
    let crowded_width = if crowded {
        b.measured_width(crate::i18n::static_text("몰림"), 12., true) + 16.
    } else {
        0.
    };
    let button_width = if g.relogin {
        b.measured_width(
            crate::i18n::static_text("트레이에서 다시 로그인"),
            13.,
            false,
        ) + 20.
    } else {
        0.
    };
    let chip_width = if g.chip.is_empty() {
        0.
    } else {
        b.measured_width(&g.chip, 12., true) + 16.
    };
    let plan_width = b.measured_width(&g.plan, 12., false);
    let prefix = if sticky {
        format!("{} · ", v2::provider_label(&g.provider))
    } else {
        String::new()
    };
    let prefix_width = if sticky {
        b.measured_width(&prefix, 12., false) + 6.
    } else {
        0.
    };
    let name_left = r.left + 32. + prefix_width;
    let measured_name = b.measured_width(&g.name, 13., true);
    let action_gap = if g.relogin { button_width + 24. } else { 12. };
    let load_width = count_width + if crowded { crowded_width + 8. } else { 0. };
    let load_left = r.right - 12. - load_width;
    let content_right = load_left - action_gap;
    if sticky {
        b.text(
            rect(r.left + 32., r.top + 11., prefix_width, 18.),
            prefix,
            12.,
            false,
            Color::Secondary,
            false,
        );
    }
    let name_width =
        measured_name.min((content_right - name_left - plan_width - chip_width - 20.).max(0.));
    b.text(
        rect(name_left, r.top + 11., name_width, 18.),
        g.name.clone(),
        13.,
        true,
        Color::Text,
        false,
    );
    let plan_left = name_left + name_width + 6.;
    b.text(
        rect(plan_left, r.top + 12., plan_width, 16.),
        g.plan.clone(),
        12.,
        false,
        Color::Secondary,
        false,
    );
    chip(
        b,
        &g.chip,
        g.tone,
        rect(plan_left + plan_width + 8., r.top + 10., chip_width, 20.),
        10.,
    );
    if g.relogin {
        b.control(
            rect(
                load_left - button_width - 12.,
                r.top + 8.,
                button_width,
                24.,
            ),
            crate::i18n::static_text("트레이에서 다시 로그인"),
            Target::OpenTrayAccount(g.id.clone()),
            Role::Button,
            false,
            None,
        );
    }
    b.text(
        rect(load_left, r.top + 12., count_width, 16.),
        count_text.clone(),
        12.,
        true,
        Color::Text,
        false,
    );
    if crowded {
        chip(
            b,
            crate::i18n::static_text("몰림"),
            v2::Tone::Warn,
            rect(
                load_left + count_width + 8.,
                r.top + 10.,
                crowded_width,
                20.,
            ),
            12.,
        );
    }
    let load_rect = rect(load_left, r.top + 8., load_width, 24.);
    let load_description = if crowded {
        crate::i18n::format(
            "분산 기준 {0}개에 도달해 새 세션은 다른 계정을 우선합니다. 제한이 아니며 세션은 계속 받습니다.",
            &[threshold.to_string()],
        )
    } else {
        crate::i18n::format(
            "활성 {0} · 연결된 대화 {1}개",
            &[(g.active).to_string(), (g.bound).to_string()],
        )
    };
    b.semantic(
        format!("{}:load:{}", b.scope, g.id),
        Some(format!("group:{}", g.id)),
        Role::Text,
        format!(
            "{count_text}{}. {load_description}",
            if crowded {
                crate::i18n::static_text(" · 몰림")
            } else {
                ""
            }
        ),
        load_rect,
        None,
    );
    b.tooltip(load_rect, load_description);
    if !g.relogin_reason.is_empty() {
        b.text(
            rect(r.left + 32., r.top + 36., width(r) - 44., 16.),
            g.relogin_reason.clone(),
            12.,
            false,
            Color::Secondary,
            false,
        );
    } else if g.windows.is_empty() {
        b.text(
            rect(r.left + 32., r.top + 36., 112., 16.),
            crate::i18n::static_text("사용량 확인 전"),
            12.,
            false,
            Color::Secondary,
            false,
        );
        b.fill(rect(r.left + 152., r.top + 42., 64., 4.), Color::Track, 2.);
    } else {
        let slots = ((width(r) - 40. + 24.) / 320.).floor().max(1.) as usize;
        for (index, window) in g.windows.iter().enumerate() {
            let x = r.left + 32. + (index % slots) as f32 * 320.;
            let y = r.top + 36. + (index / slots) as f32 * 24.;
            b.text(
                rect(x, y, 40., 16.),
                window.label.clone(),
                12.,
                false,
                Color::Secondary,
                false,
            );
            b.text(
                rect(x + 40., y, 66., 16.),
                window.remaining.clone(),
                12.,
                true,
                Color::Text,
                false,
            );
            usage_bar(
                b,
                rect(x + 106., y + 6., 64., 4.),
                window.remaining_pct,
                g.paused,
            );
            b.text(
                rect(x + 178., y, 118., 16.),
                window.reset.clone(),
                12.,
                false,
                Color::Secondary,
                false,
            );
            b.semantic(
                if sticky {
                    format!("sticky:usage:{}:{index}", g.id)
                } else {
                    format!("usage:{}:{index}", g.id)
                },
                Some(format!("group:{}", g.id)),
                Role::Text,
                format!("{} {} · {}", window.label, window.remaining, window.reset),
                rect(x, y, 296., 16.),
                None,
            );
        }
    }
    let mut forecast_y = r.top + group_usage_height(g, width(r) + 2.) - 4.;
    if g.outside_used {
        let note = crate::i18n::static_text("qb 밖에서도 사용 중");
        let bounds = rect(r.left + 32., forecast_y, (width(r) - 44.).max(1.), 16.);
        b.text(bounds, note, 12., false, Color::Secondary, false);
        b.semantic(
            format!("{}:outside-used:{}", b.scope, g.id),
            Some(format!("group:{}", g.id)),
            Role::Text,
            note.into(),
            bounds,
            None,
        );
        forecast_y += 20.;
    }
    for (index, line) in g.forecast_lines.iter().enumerate() {
        let h = b.measured_height(line, (width(r) - 44.).max(1.), 12., g.forecast_warning);
        let bounds = rect(r.left + 32., forecast_y, (width(r) - 44.).max(1.), h);
        b.text(
            bounds,
            line,
            12.,
            g.forecast_warning,
            if g.forecast_warning {
                Color::Warn
            } else {
                Color::Secondary
            },
            true,
        );
        b.tooltip(bounds, g.forecast_tooltip.clone());
        b.semantic(
            format!("{}:forecast:{}:{index}", b.scope, g.id),
            Some(format!("group:{}", g.id)),
            Role::Text,
            line.clone(),
            bounds,
            None,
        );
        forecast_y += h + 4.;
    }
    b.scope = prior_scope;
}
fn bar_color(p: u8) -> Color {
    if p >= 30 {
        Color::BarOk
    } else if p >= 10 {
        Color::BarLow
    } else {
        Color::BarCritical
    }
}
// One measured layout is used for scroll extents, paint, pointer and UIA bounds.
#[derive(Clone, Copy)]
enum SessionControl {
    Urgency,
    Waiting,
    Move,
    Feedback,
}
fn session_control_layout(
    b: &mut Builder<'_>,
    row: &m::SessionRow,
    _available: f32,
    clock: &m::Clock,
) -> Vec<(SessionControl, Rect, String)> {
    if row.routing.is_none() && row.quota_wait.is_none() {
        return vec![];
    }
    let mut parts = vec![(
        SessionControl::Urgency,
        crate::i18n::static_text(if row.routing.is_some() {
            "긴급"
        } else {
            "긴급 설정 확인 전"
        })
        .to_string(),
    )];
    if !row.quota_wait_known {
        parts.push((
            SessionControl::Waiting,
            crate::i18n::static_text("5시간 대기 상태 확인 전").into(),
        ));
    } else if let Some(wait) = &row.quota_wait {
        parts.push((
            SessionControl::Waiting,
            crate::i18n::format("초기화 {0}까지 대기", &[clock.time(Some(wait.reset_at))]),
        ));
        parts.push((
            SessionControl::Move,
            crate::i18n::static_text("지금 옮기기").into(),
        ));
    }
    if row.urgency_pending {
        parts.push((
            SessionControl::Feedback,
            crate::i18n::static_text("긴급 설정 저장 중").into(),
        ));
    } else if row.urgency_failed {
        parts.push((
            SessionControl::Feedback,
            crate::i18n::static_text("긴급 설정 저장 실패").into(),
        ));
    }
    let mut x = 0.;
    let mut result = vec![];
    for (kind, label) in parts {
        let (size, padding) = match kind {
            SessionControl::Urgency => (13., 36.),
            SessionControl::Move => (13., 16.),
            _ => (12., 0.),
        };
        let measured = b.measured_width(&label, size, false) + padding;
        let w = match kind {
            SessionControl::Waiting => measured.min(80.),
            SessionControl::Feedback => measured.min(48.),
            _ => measured,
        };
        result.push((kind, rect(x, 3.5, w, 28.), label));
        x += w + 8.;
    }
    result
}
fn session_height(
    _b: &mut Builder<'_>,
    _row: &m::SessionRow,
    _available: f32,
    _clock: &m::Clock,
) -> f32 {
    // Pre-N5c row rhythm, including pending, waiting and disabled controls.
    35.
}

fn session_controls(b: &mut Builder<'_>, row: &m::SessionRow, row_rect: Rect, clock: &m::Clock) {
    for (kind, local, label) in session_control_layout(b, row, width(row_rect), clock) {
        let r = rect(
            row_rect.left + local.left,
            row_rect.top + local.top,
            width(local),
            height(local),
        );
        let (target, role, selected, enabled, reason) = match kind {
            SessionControl::Urgency => (
                Some(Target::SessionUrgency(row.id.clone())),
                Role::Checkbox,
                row.routing.as_ref().is_some_and(m::SessionRouting::urgent),
                row.urgency_enabled,
                if row.routing.is_none() {
                    "긴급 설정 확인 전"
                } else if row.urgency_pending {
                    "긴급 설정 저장 중"
                } else if !row.urgency_enabled {
                    "새로고침 또는 다른 긴급 설정 처리를 기다려 주세요"
                } else {
                    "5시간 한도에서 다른 계정을 찾습니다. 고정·캐시·이동 제한은 그대로 적용됩니다"
                },
            ),
            SessionControl::Move => {
                let strict = row.routing.as_ref().is_some_and(|r| r.mode == "strict");
                let urgent = row.routing.as_ref().is_some_and(m::SessionRouting::urgent);
                (
                    Some(Target::SessionMoveNow(row.id.clone())),
                    Role::Button,
                    false,
                    row.urgency_enabled && !strict && !urgent,
                    if strict {
                        "계정 고정 정책에서는 지금 옮길 수 없습니다"
                    } else if urgent {
                        "이미 긴급으로 설정되어 있습니다. 실제 이동에는 기존 제한이 적용됩니다"
                    } else if !row.urgency_enabled {
                        "새로고침 또는 다른 긴급 설정 처리를 기다려 주세요"
                    } else {
                        "세션을 긴급으로 설정합니다. 실제 계정 이동을 보장하지 않습니다"
                    },
                )
            }
            SessionControl::Waiting | SessionControl::Feedback => {
                let key = if matches!(kind, SessionControl::Waiting) {
                    "waiting"
                } else {
                    "urgency-feedback"
                };
                b.semantic(
                    format!("session:{}:{key}", row.id),
                    Some(format!("session:{}", row.id)),
                    Role::Text,
                    label.clone(),
                    r,
                    None,
                );
                b.text(
                    rect(r.left, r.top + 5., width(r), height(r) - 6.),
                    label.clone(),
                    12.,
                    false,
                    if row.urgency_failed && matches!(kind, SessionControl::Feedback) {
                        Color::Bad
                    } else {
                        Color::Secondary
                    },
                    false,
                );
                b.tooltip(r, label.clone());
                if matches!(kind, SessionControl::Feedback) && row.urgency_failed {
                    b.tooltip(
                        r,
                        crate::i18n::static_text(
                            "긴급 설정을 저장하지 못했습니다. 새로고침한 뒤 다시 시도하세요",
                        )
                        .into(),
                    );
                }
                continue;
            }
        };
        let enabled = enabled && b.enabled;
        let tip = crate::i18n::static_text(reason).to_string();
        if let (Some(hit), Some(target)) = (intersection(r, b.clip), target) {
            let idx = b.semantic(
                format!("session:{}:{target:?}", row.id),
                Some(format!("session:{}", row.id)),
                role,
                label.clone(),
                hit,
                Some(target.clone()),
            );
            b.scene.accessibility[idx].selected = selected;
            b.scene.accessibility[idx].enabled = enabled;
            if role == Role::Checkbox {
                b.scene.accessibility[idx].value = Some(
                    crate::i18n::static_text(if row.routing.is_none() {
                        "확인 전"
                    } else if selected {
                        "켜짐"
                    } else {
                        "꺼짐"
                    })
                    .into(),
                );
            }
            b.scene.targets.push(HitTarget {
                rect: hit,
                target,
                enabled,
                tooltip: Some(tip.clone()),
            });
        }
        b.tooltip(r, tip);
        if !enabled {
            b.scene.draw.push(Draw::Alpha(0.5));
        }
        b.fill(r, Color::Surface, 4.);
        b.stroke(r, Color::Border, 1., 4.);
        let padding = if role == Role::Checkbox {
            let check = rect(r.left + 6., r.top + 7., 14., 14.);
            if selected {
                b.fill(check, Color::Accent, 2.);
                b.text_face(
                    check,
                    "\u{e73e}",
                    12.,
                    false,
                    Color::OnAccent,
                    false,
                    Face::Icon,
                    TextAlign::Center,
                );
            } else {
                b.stroke(check, Color::Secondary, 1., 2.);
                if row.routing.is_none() {
                    b.line(
                        [check.left + 3., check.top + 7.],
                        [check.right - 3., check.top + 7.],
                        Color::Secondary,
                        1.,
                        None,
                    );
                }
            }
            28.
        } else {
            8.
        };
        b.text(
            rect(
                r.left + padding,
                r.top + 5.,
                (width(r) - padding - 8.).max(1.),
                height(r) - 6.,
            ),
            label,
            13.,
            false,
            Color::Text,
            true,
        );
        if !enabled {
            b.scene.draw.push(Draw::Alpha(1.));
        }
    }
}
fn session(
    b: &mut Builder<'_>,
    row: &m::SessionRow,
    r: Rect,
    columns: &[(SortKey, &str, f32)],
    parent: Option<String>,
    clock: &m::Clock,
) {
    b.scene.session_rows.push(SessionGeometry {
        id: row.id.clone(),
        rect: r,
    });
    b.tooltip(rect(r.left + 12., r.top, 16., 35.), row.dot.clone());
    b.tooltip(
        rect(r.left + 28., r.top, 268., 35.),
        row.label_source.clone(),
    );
    let key = format!("session:{}", row.id);
    if row.selected {
        b.fill(r, Color::Select, 0.);
        b.fill(rect(r.left, r.top, 3., height(r)), Color::Accent, 0.);
    }
    let idx = b.semantic(
        key.clone(),
        parent.or_else(|| Some("sessions:table".into())),
        Role::Row,
        crate::i18n::format(
            "{0}, {1} · {2}, 계정 {3} · {4}{5}, 모델 {6}, {7}, 오늘 요청 {8}",
            &[
                (row.label).to_string(),
                (row.client).to_string(),
                (row.short_id).to_string(),
                (row.account).to_string(),
                (row.account_provider).to_string(),
                (row.account_chip
                    .as_ref()
                    .map(|chip| format!(" · {}", chip.text))
                    .unwrap_or_default())
                .to_string(),
                (row.model).to_string(),
                (row.dot).to_string(),
                (row.requests).to_string(),
            ],
        ),
        r,
        Some(Target::Session(row.id.clone())),
    );
    b.scene.accessibility[idx].selected = row.selected;
    if let Some(hit) = intersection(r, b.clip) {
        b.scene.targets.push(HitTarget {
            rect: hit,
            target: Target::Session(row.id.clone()),
            enabled: true,
            tooltip: Some(row.label_source.clone()),
        });
    }
    let mut x = r.left + 12.;
    for (key, _, cw) in columns {
        let cr = rect(x, r.top + 7., *cw, 20.);
        match key {
            SortKey::Status => {
                let dot = rect(x + 2., r.top + 13., 8., 8.);
                if !row.inflight_known {
                    b.text(cr, "—", 13., false, Color::Secondary, false);
                } else if row.inflight {
                    b.fill(rect(x - 1., r.top + 10., 14., 14.), Color::Select, 7.);
                    b.fill(dot, Color::Accent, 4.);
                } else if row.active {
                    b.stroke(dot, Color::Accent, 2., 4.);
                } else {
                    b.fill(dot, Color::Secondary, 4.);
                }
            }
            SortKey::Session => {
                b.text(
                    cr,
                    row.label.clone(),
                    13.,
                    row.label_source
                        .contains(crate::i18n::static_text("사용자"))
                        || !row.label.contains(crate::i18n::static_text(" 세션 ")),
                    Color::Text,
                    false,
                );
            }
            SortKey::Client => {
                let id_text = format!(" · {}", row.short_id);
                let id_width = b.measured_width(&id_text, 12., false);
                let client_width = b
                    .measured_width(&row.client, 12., false)
                    .min((*cw - id_width - 8.).max(0.));
                b.text(
                    rect(cr.left, cr.top, client_width, 20.),
                    row.client.clone(),
                    12.,
                    false,
                    Color::Secondary,
                    false,
                );
                b.text(
                    rect(cr.left + client_width, cr.top, id_width, 20.),
                    id_text,
                    12.,
                    false,
                    Color::Secondary,
                    false,
                );
            }
            SortKey::Account => {
                let chip_width = row.account_chip.as_ref().map_or(0., |chip| {
                    (b.measured_width(&chip.text, 12., true) + 12.).min(*cw - 40.)
                });
                let text_right = cr.right - if chip_width > 0. { chip_width + 6. } else { 0. };
                let provider = format!(" · {}", row.account_provider);
                let provider_width = b
                    .measured_width(&provider, 12., false)
                    .min((text_right - cr.left).max(0.));
                b.text(
                    rect(
                        cr.left,
                        cr.top,
                        (text_right - cr.left - provider_width).max(0.),
                        20.,
                    ),
                    row.account.clone(),
                    12.,
                    false,
                    Color::Text,
                    false,
                );
                b.text(
                    rect(text_right - provider_width, cr.top, provider_width, 20.),
                    provider,
                    12.,
                    false,
                    Color::Secondary,
                    false,
                );
                if let Some(status) = &row.account_chip {
                    chip(
                        b,
                        &status.text,
                        status.tone,
                        rect(cr.right - chip_width, r.top + 9.5, chip_width, 16.),
                        8.,
                    );
                }
            }
            SortKey::Memo if row.routing.is_some() || row.quota_wait.is_some() => {
                let cell = rect(cr.left, r.top, *cw, 35.);
                if !row.memo.is_empty() {
                    // Specific action explanations take precedence over the cell.
                    b.tooltip(cell, row.memo.clone());
                }
                session_controls(b, row, cell, clock);
                let used = session_control_layout(b, row, *cw, clock)
                    .last()
                    .map_or(0., |(_, r, _)| r.right + 8.);
                let compact = row
                    .memo
                    .starts_with(crate::i18n::static_text("이동 (Claude는 자동 재개 미지원)"));
                let minimum_width = if compact {
                    b.measured_width(crate::i18n::static_text("이동"), 12., false)
                } else {
                    40.
                };
                if *cw - used >= minimum_width {
                    let bounds = rect(cr.left + used, cr.top, *cw - used, 20.);
                    let text = if compact {
                        crate::i18n::static_text("이동")
                    } else {
                        &row.memo
                    };
                    b.text(
                        bounds,
                        text,
                        12.,
                        false,
                        if row.memo.contains(crate::i18n::static_text("크레딧")) {
                            Color::Warn
                        } else {
                            Color::Text
                        },
                        false,
                    );
                    if !row.memo.is_empty() {
                        b.semantic(
                            format!("session:{}:memo", row.id),
                            Some(format!("session:{}", row.id)),
                            Role::Text,
                            row.memo.clone(),
                            bounds,
                            None,
                        );
                    }
                }
            }
            _ => {
                let text = match key {
                    SortKey::Model => &row.model,
                    SortKey::Start => &row.start,
                    SortKey::LastActivity => &row.activity,
                    SortKey::Requests => &row.requests,
                    _ => &row.memo,
                };
                b.text(
                    cr,
                    text.clone(),
                    if *key == SortKey::Memo { 12. } else { 13. },
                    false,
                    if *key == SortKey::Memo
                        && row.memo.contains(crate::i18n::static_text("크레딧"))
                    {
                        Color::Warn
                    } else {
                        Color::Text
                    },
                    false,
                );
            }
        }
        x += *cw;
    }
}
fn recent(b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, card: Rect) {
    b.card(card);
    b.text(
        rect(card.left + 16., card.top + 14., 100., 22.),
        crate::i18n::static_text("최근 일"),
        14.,
        true,
        Color::Text,
        false,
    );
    b.control(
        rect(card.right - 140., card.top + 8., 128., 30.),
        crate::i18n::static_text("기록 모두 보기 ›"),
        Target::AllEvents,
        Role::Button,
        false,
        None,
    );
    let viewport = rect(
        card.left + 1.,
        card.top + 48.,
        width(card) - 2.,
        height(card) - 49.,
    );
    let heights: Vec<_> = v
        .recent
        .iter()
        .map(|e| {
            88. + b
                .measured_height(&e.body, width(card) - 32., 12., false)
                .min(64.)
                + if e.relogin { 32. } else { 0. }
        })
        .collect();
    let (old, offset) = b.begin_region(
        ScrollRegion::Side,
        viewport,
        heights.iter().sum(),
        s.side_offset,
    );
    let mut y = viewport.top - offset;
    if v.recent.is_empty() {
        b.text(
            rect(viewport.left + 16., y + 20., width(viewport) - 32., 48.),
            v.event_empty.clone(),
            13.,
            false,
            Color::Secondary,
            true,
        );
    }
    for (e, h) in v.recent.iter().zip(heights) {
        event_card(b, e, rect(viewport.left, y, width(viewport), h), true);
        y += h;
    }
    b.end_region(old);
}
fn event_card(b: &mut Builder<'_>, e: &m::EventRow, r: Rect, recent: bool) {
    let old_scope = std::mem::replace(&mut b.scope, format!("event:{}", e.seq));
    let x = r.left + 16.;
    let tag_width = b.measured_width(&e.tag, 12., true) + 16.;
    chip(b, &e.tag, e.tone, rect(x, r.top + 12., tag_width, 20.), 4.);
    b.text(
        rect(r.right - 96., r.top + 14., 80., 18.),
        e.time.clone(),
        12.,
        false,
        Color::Secondary,
        false,
    );
    b.text(
        rect(x, r.top + 40., width(r) - 32., 24.),
        e.title.clone(),
        13.,
        true,
        Color::Text,
        false,
    );
    let bodyh = if recent {
        (height(r) - 88. - if e.relogin { 32. } else { 0. }).max(16.)
    } else {
        (height(r) - 76.).max(20.)
    };
    b.text(
        rect(x, r.top + 66., width(r) - 32., bodyh),
        e.body.clone(),
        12.,
        false,
        Color::Secondary,
        true,
    );
    b.semantic(
        format!("event:{}", e.seq),
        None,
        Role::Text,
        format!("{} {} {} {}", e.time, e.tag, e.title, e.body),
        r,
        None,
    );
    if e.relogin
        && let Some(account) = &e.account
    {
        b.control(
            rect(x, r.bottom - 36., 200., 28.),
            crate::i18n::static_text("트레이에서 다시 로그인"),
            Target::OpenTrayAccount(account.clone()),
            Role::Button,
            false,
            None,
        );
    }
    b.line(
        [r.left, r.bottom - 0.5],
        [r.right, r.bottom - 0.5],
        Color::Border,
        1.,
        None,
    );
    b.scope = old_scope;
}
fn detail(
    b: &mut Builder<'_>,
    v: &m::DashboardView,
    s: &DashboardState,
    card: Rect,
    d: &m::DetailView,
) {
    b.card(card);
    b.scene.targets.push(HitTarget {
        rect: card,
        target: Target::DetailSurface,
        enabled: true,
        tooltip: None,
    });
    let header = if d.raw_prefix.is_some() { 146. } else { 124. };
    b.text(
        rect(card.left + 16., card.top + 16., width(card) - 64., 28.),
        d.title.clone(),
        16.,
        true,
        Color::Text,
        false,
    );
    b.control(
        rect(card.right - 42., card.top + 10., 30., 28.),
        "×",
        Target::CloseDetail,
        Role::Button,
        false,
        Some(crate::i18n::static_text("세션 상세 닫기").into()),
    );
    b.text(
        rect(card.left + 16., card.top + 50., width(card) - 32., 20.),
        d.subtitle.clone(),
        12.,
        false,
        Color::Secondary,
        false,
    );
    if let Some(prefix) = &d.raw_prefix {
        b.text(
            rect(card.left + 16., card.top + 72., width(card) - 32., 20.),
            crate::i18n::format("원래 세션 번호 앞 8자: {0}", &[prefix.to_string()]),
            12.,
            false,
            Color::Secondary,
            false,
        );
    }
    let chip_width = b.measured_width(&d.status, 12., true) + 16.;
    let status_y = card.top + header - 36.;
    chip(
        b,
        &d.status,
        d.status_tone,
        rect(
            card.left + 16.,
            status_y,
            chip_width.min(width(card) - 32.),
            20.,
        ),
        10.,
    );
    b.text(
        rect(
            card.left + 24. + chip_width,
            status_y + 2.,
            (width(card) - 40. - chip_width).max(0.),
            18.,
        ),
        d.account_status.clone(),
        12.,
        false,
        Color::Secondary,
        false,
    );
    let viewport = rect(
        card.left + 1.,
        card.top + header,
        width(card) - 2.,
        height(card) - header - 1.,
    );
    let facts: Vec<_> = d
        .facts
        .iter()
        .map(|(_, value)| {
            b.measured_height(value, width(card) - 140., 13., false)
                .max(24.)
        })
        .collect();
    let lines: Vec<_> = d
        .timeline
        .iter()
        .chain(&d.requests)
        .map(|t| b.measured_height(t, width(card) - 40., 12., false) + 12.)
        .collect();
    let content = facts.iter().sum::<f32>()
        + facts.len() as f32 * 4.
        + lines.iter().sum::<f32>()
        + 172.
        + if d.expired { 52. } else { 0. };
    let (old, offset) = b.begin_region(ScrollRegion::Side, viewport, content, s.side_offset);
    let mut y = viewport.top - offset + 8.;
    if d.expired {
        b.text(
            rect(viewport.left + 16., y, width(viewport) - 32., 44.),
            crate::i18n::static_text("이 세션은 더 이상 연결되어 있지 않습니다"),
            13.,
            true,
            Color::Secondary,
            true,
        );
        y += 52.;
    }
    for ((key, value), h) in d.facts.iter().zip(facts) {
        b.text(
            rect(viewport.left + 16., y, 96., h),
            key.clone(),
            12.,
            false,
            Color::Secondary,
            false,
        );
        b.text(
            rect(viewport.left + 116., y, width(viewport) - 132., h),
            value.clone(),
            13.,
            false,
            Color::Text,
            true,
        );
        b.semantic(
            format!("detail:{}:fact:{}", d.id, detail_fact_key(key)),
            None,
            Role::Text,
            format!("{key}: {value}"),
            rect(viewport.left + 16., y, width(viewport) - 32., h),
            None,
        );
        y += h + 4.;
    }
    let mut index = 0;
    for (source_title, items) in [("계정 이력", &d.timeline), ("최근 요청", &d.requests)] {
        let title = crate::i18n::static_text(source_title);
        b.text(
            rect(viewport.left + 16., y + 8., width(viewport) - 32., 24.),
            title,
            14.,
            true,
            Color::Text,
            false,
        );
        y += 40.;
        for (i, text) in items.iter().enumerate() {
            let h = lines[index];
            index += 1;
            if title == crate::i18n::static_text("계정 이력") {
                b.fill(
                    rect(viewport.left + 17., y + 5., 6., 6.),
                    if text.contains(crate::i18n::static_text("(현재)")) {
                        Color::Accent
                    } else {
                        Color::Secondary
                    },
                    3.,
                );
            }
            b.text(
                rect(viewport.left + 28., y, width(viewport) - 44., h),
                text.clone(),
                12.,
                false,
                if text.contains(crate::i18n::static_text("실패")) {
                    Color::Bad
                } else {
                    Color::Text
                },
                true,
            );
            b.semantic(
                format!("detail:{}:{source_title}:{i}", d.id),
                None,
                Role::Text,
                text.clone(),
                rect(viewport.left + 16., y, width(viewport) - 32., h),
                None,
            );
            y += h;
        }
    }
    b.text(
        rect(viewport.left + 16., y + 12., width(viewport) - 32., 64.),
        crate::i18n::static_text(
            "대화 내용은 보지도 기록하지도 않습니다. 세션 번호는 ledger의 session 값과 같습니다.",
        ),
        12.,
        false,
        Color::Secondary,
        true,
    );
    let _ = v;
    b.end_region(old);
}
fn event_refs(e: &m::EventRow) -> Vec<(Target, String)> {
    let mut refs = Vec::new();
    if let Some(id) = &e.account {
        refs.push((
            Target::AccountLink(id.clone()),
            crate::i18n::format(
                "계정 {0} ›",
                &[(e.account_label
                    .as_deref()
                    .unwrap_or(crate::i18n::static_text("이름 없음")))
                .to_string()],
            ),
        ));
    }
    if let Some(id) = &e.session {
        refs.push((
            Target::SessionLink(id.clone()),
            crate::i18n::format(
                "세션 {0} ›",
                &[(e.session_label
                    .as_deref()
                    .unwrap_or_else(|| id.get(..6).unwrap_or(id)))
                .to_string()],
            ),
        ));
    }
    refs
}
fn event_refs_width(b: &mut Builder<'_>, e: &m::EventRow) -> f32 {
    let refs = event_refs(e);
    refs.iter()
        .map(|(_, label)| b.measured_width(label, 12., false).min(240.))
        .sum::<f32>()
        + refs.len().saturating_sub(1) as f32 * 16.
}
fn events(b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, card: Rect) {
    b.card(card);
    let mut x = card.left + 12.;
    let mut toolbar_y = card.top + 12.;
    for (filter, count) in &v.event_counts {
        let label = format!("{} {count}", filter.label());
        let w = (b.measured_width(&label, 13., true) + 24.).max(80.);
        if x + w > card.right - 12. && x > card.left + 12. {
            x = card.left + 12.;
            toolbar_y += 36.;
        }
        b.control_radius(
            rect(x, toolbar_y, w, 28.),
            &label,
            Target::EventFilter(*filter),
            Role::Button,
            s.event_filter == *filter,
            None,
            14.,
        );
        x += w + 6.;
    }
    if x + 4. + 104. + 12. + 200. > card.right - 12. {
        x = card.left + 12. - 4.;
        toolbar_y += 36.;
    }
    b.control(
        rect(x + 4., toolbar_y, 104., 28.),
        match s.event_days {
            1 => crate::i18n::static_text("오늘"),
            2 => crate::i18n::static_text("최근 2일"),
            _ => crate::i18n::static_text("최근 7일"),
        },
        Target::EventDaysMenu,
        Role::ComboBox,
        false,
        None,
    );
    x += 118.;
    let sw = (card.right - x - 12.).clamp(60., 200.);
    b.control(
        rect(card.right - sw - 12., toolbar_y, sw, 28.),
        if s.event_search.is_empty() {
            crate::i18n::static_text("기록 검색")
        } else {
            &s.event_search
        },
        Target::EventSearch,
        Role::Edit,
        false,
        None,
    );
    let toolbar_height = toolbar_y - card.top + 40.;
    let banner = if v.activity_truncated { 28. } else { 0. };
    if v.activity_truncated {
        let r = rect(
            card.left + 12.,
            card.top + toolbar_height + 2.,
            width(card) - 24.,
            24.,
        );
        b.text(
            r,
            crate::i18n::static_text("보관 한도로 일부 기록만 표시합니다."),
            12.,
            false,
            Color::Warn,
            false,
        );
        b.semantic(
            "events:truncated".into(),
            None,
            Role::Text,
            crate::i18n::static_text("보관 한도로 일부 기록만 표시합니다.").into(),
            r,
            None,
        );
    }
    let viewport = rect(
        card.left + 1.,
        card.top + toolbar_height + banner,
        width(card) - 2.,
        height(card) - toolbar_height - 1. - banner,
    );
    if let Some(node) = b
        .scene
        .accessibility
        .iter_mut()
        .rev()
        .find(|n| n.target == Some(Target::EventSearch))
    {
        node.value = Some(s.event_search.clone());
    }
    let heights: Vec<_> = v
        .events
        .iter()
        .map(|e| {
            let middle =
                (width(viewport) - 32. - 56. - 92. - 36. - event_refs_width(b, e)).max(100.);
            (b.measured_height(&e.body, middle, 12., false)
                + 50.
                + if e.relogin { 36. } else { 0. })
            .max(70.)
        })
        .collect();
    let mut previous = "";
    let mut content = 0.;
    for (e, h) in v.events.iter().zip(&heights) {
        if previous != e.day {
            content += 32.;
            previous = &e.day;
        }
        content += h;
    }
    let (old, offset) = b.begin_region(ScrollRegion::Events, viewport, content, s.events_offset);
    let mut y = viewport.top - offset;
    let mut day = "";
    if v.events.is_empty() {
        b.text(
            rect(viewport.left + 24., y + 40., width(viewport) - 48., 48.),
            v.event_empty.clone(),
            13.,
            false,
            Color::Secondary,
            true,
        );
    }
    for (e, h) in v.events.iter().zip(heights) {
        let old_scope = std::mem::replace(&mut b.scope, format!("event:{}", e.seq));
        if day != e.day {
            b.fill(
                rect(viewport.left, y, width(viewport), 32.),
                Color::Footer,
                0.,
            );
            b.text(
                rect(viewport.left + 16., y + 8., width(viewport) - 32., 20.),
                e.day.clone(),
                12.,
                true,
                Color::Secondary,
                false,
            );
            y += 32.;
            day = &e.day;
        }
        let time_x = viewport.left + 16.;
        b.text(
            rect(time_x, y + 12., 64., 20.),
            e.time.clone(),
            13.,
            false,
            Color::Text,
            false,
        );
        let tag_width = b.measured_width(&e.tag, 12., true) + 16.;
        let tag = rect(time_x + 68., y + 12., tag_width, 20.);
        chip(b, &e.tag, e.tone, tag, 4.);
        let tx = time_x + 56. + 12. + 92. + 12.;
        let refs_width = event_refs_width(b, e);
        let middle = (viewport.right - 16. - refs_width - 12. - tx).max(100.);
        b.text(
            rect(tx, y + 10., middle, 24.),
            e.title.clone(),
            13.,
            true,
            Color::Text,
            false,
        );
        b.text(
            rect(
                tx,
                y + 38.,
                middle,
                (h - 50. - if e.relogin { 36. } else { 0. }).max(16.),
            ),
            e.body.clone(),
            12.,
            false,
            Color::Secondary,
            true,
        );
        let mut refs = viewport.right - 16. - refs_width;
        for (target, label) in event_refs(e) {
            let width = b.measured_width(&label, 12., false).min(240.);
            let r = rect(refs, y + 10., width, 24.);
            b.target(r, &label, target, Role::Button, false, None);
            b.text(
                rect(r.left, r.top + 2., width, 20.),
                label,
                12.,
                false,
                Color::Accent,
                false,
            );
            refs += width + 16.;
        }
        if e.relogin
            && let Some(id) = &e.account
        {
            b.control(
                rect(tx, y + h - 34., 200., 28.),
                crate::i18n::static_text("트레이에서 다시 로그인"),
                Target::OpenTrayAccount(id.clone()),
                Role::Button,
                false,
                None,
            );
        }
        b.semantic(
            format!("event:{}", e.seq),
            None,
            Role::Text,
            format!("{} {} {} {}", e.time, e.tag, e.title, e.body),
            rect(viewport.left, y, width(viewport), h),
            None,
        );
        b.line(
            [viewport.left, y + h - 0.5],
            [viewport.right, y + h - 0.5],
            Color::Border,
            1.,
            None,
        );
        y += h;
        b.scope = old_scope;
    }
    b.end_region(old);
}
#[cfg(test)]
#[path = "dashboard_geometry_tests.rs"]
mod tests;

// Domain-limited mapping of model-owned fact labels to their incumbent UIA keys.
// Never use this for caller-supplied account/project/session names.
fn detail_fact_key(label: &str) -> &str {
    for ko in [
        "계정",
        "남은 양",
        "모델",
        "시작",
        "마지막 활동",
        "연결",
        "하위 에이전트",
        "오늘 요청",
        "기록 범위",
        "속도 조절 대기",
        "이름 출처",
        "최근 요청 범위",
        "계정 이력 범위",
    ] {
        if label == ko || label == crate::i18n::static_text(ko) {
            return ko;
        }
    }
    label
}

#[cfg(test)]
mod r16_stable_key_tests {
    use super::*;
    #[test]
    fn translated_detail_facts_keep_existing_scheduler_keys() {
        crate::i18n::with_language(crate::i18n::UiLanguage::En, || {
            assert_eq!(detail_fact_key("Started"), "시작");
            assert_eq!(detail_fact_key("Last activity"), "마지막 활동");
            assert_eq!(detail_fact_key("Rate-limit wait"), "속도 조절 대기");
        });
    }
}

#[cfg(test)]
mod n10_outside_usage_tests {
    use super::*;

    #[test]
    fn n10_dashboard_outside_note_has_separate_paint_and_read_only_semantics() {
        for language in [crate::i18n::UiLanguage::Ko, crate::i18n::UiLanguage::En] {
            crate::i18n::with_language(language, || {
                let clock = crate::dashboard_fixtures::clock();
                let mut wire = serde_json::json!({"accounts":[{
                    "id":"synthetic-outside", "provider":"codex", "label":"Synthetic",
                    "plan":"plus", "user_state":"active", "health":{"kind":"ready"},
                    "credits_in_use":true, "allow_credits":true, "source":{"kind":"oauth"}
                }]});
                let mut state = DashboardState {
                    grouped: true,
                    range: Range::All,
                    ..Default::default()
                };
                let mut baseline = crate::dashboard_fixtures::snapshot();
                baseline.accounts = v2::project(&wire.to_string(), clock.utc().into()).unwrap();
                baseline.sessions.sessions.truncate(1);
                baseline.sessions.sessions[0].account = "synthetic-outside".into();
                baseline.sessions.sessions[0].provider = "codex".into();
                let before = m::project(&baseline, &mut state, &clock);
                wire["accounts"][0]["outside_used"] = true.into();
                let mut marked = baseline.clone();
                marked.accounts = v2::project(&wire.to_string(), clock.utc().into()).unwrap();
                let after = m::project(&marked, &mut state, &clock);
                let before_group = &before.groups[0];
                let after_group = &after.groups[0];
                assert!(!before_group.outside_used && after_group.outside_used);
                assert_eq!(before_group.plan, after_group.plan);
                assert_eq!(before_group.chip, after_group.chip);
                assert_eq!(before_group.name, after_group.name);
                assert_eq!(before_group.relogin_reason, after_group.relogin_reason);
                let note = crate::i18n::static_text("qb 밖에서도 사용 중");
                for size in [[1024., 640.], [1320., 900.]] {
                    let plain = Scene::build(&before, &state, size, &Metrics::default());
                    assert!(
                        !plain.draw.iter().any(
                            |d| matches!(d, Draw::Text { measure, .. } if measure.text == note)
                        )
                    );
                    let scene = Scene::build(&after, &state, size, &Metrics::default());
                    let node = scene.accessibility.iter().find(|n| n.name == note).unwrap();
                    assert_eq!(node.role, Role::Text);
                    assert!(node.target.is_none());
                    assert!(
                        scene
                            .draw
                            .iter()
                            .any(|d| matches!(d, Draw::Text { rect, measure, .. }
                        if measure.text == note && *rect == node.rect))
                    );
                    assert!(width(node.rect) > 200. && height(node.rect) == 16.);
                }
            });
        }
    }
}

#[cfg(test)]
mod n5d_claude_note_tests {
    use super::*;

    #[test]
    fn english_grouped_claude_note_fits_the_captured_862_dip_row() {
        crate::i18n::with_language(crate::i18n::UiLanguage::En, || {
            let clock = crate::dashboard_fixtures::clock();
            let mut snapshot = crate::dashboard_fixtures::snapshot();
            snapshot.accounts = v2::project(
                &serde_json::json!({"accounts": [
                    {"id":"cl-a", "provider":"claude", "label":"Synthetic A",
                     "user_state":"active", "health":{"kind":"ready"}, "source":{"kind":"oauth"}},
                    {"id":"cl-b", "provider":"claude", "label":"Synthetic B",
                     "user_state":"active", "health":{"kind":"ready"}, "source":{"kind":"oauth"}}
                ]})
                .to_string(),
                clock.utc().into(),
            )
            .unwrap();
            snapshot.sessions.sessions.truncate(1);
            let session = &mut snapshot.sessions.sessions[0];
            session.routing = Some(m::SessionRouting {
                action_id: "a".repeat(64),
                on_5h_limit: "wait".into(),
                mode: "recover".into(),
            });
            session.quota_wait = None;
            session.quota_wait_known = true;
            let id = session.id.clone();
            let mut state = DashboardState {
                grouped: true,
                range: Range::All,
                ..Default::default()
            };
            let view = m::project(&snapshot, &mut state, &clock);
            let memo = &view.flat[0].memo;
            assert!(memo.starts_with("Move (Claude cannot resume automatically)"));
            let mut metrics = Metrics::default();
            // Captured Urgent is 76 DIP: 40 measured text + 36 control padding.
            for (text, size, measured) in [("Urgent", 13., 40.), ("Move", 12., 30.)] {
                metrics.widths.insert(
                    Measure {
                        text: text.into(),
                        size,
                        line_height: 20.,
                        width: 4096.,
                        bold: false,
                        wrap: false,
                        face: Face::Text,
                        align: TextAlign::Leading,
                    }
                    .key(),
                    measured,
                );
            }
            for (size, row_width) in [([1304., 889.], 862.), ([1320., 900.], 878.)] {
                let scene = Scene::build(&view, &state, size, &metrics);
                let row = scene.session_rows.iter().find(|row| row.id == id).unwrap();
                assert_eq!(width(row.rect), row_width);
                assert_eq!(height(row.rect), 35.);
                let urgent = scene
                    .targets
                    .iter()
                    .find(|hit| hit.target == Target::SessionUrgency(id.clone()))
                    .unwrap();
                assert_eq!(width(urgent.rect), 76.);
                let node = scene
                    .accessibility
                    .iter()
                    .find(|node| node.key == format!("session:{id}:memo"))
                    .expect("visible compact Notes");
                assert_eq!(node.name, *memo);
                assert_eq!(node.role, Role::Text);
                assert!(node.target.is_none());
                assert!(width(node.rect) >= 30.);
                assert!(scene.draw.iter().any(|draw| matches!(draw,
                    Draw::Text { rect, measure, .. }
                    if *rect == node.rect && measure.text == "Move" && !measure.wrap)));
                assert!(
                    scene.tooltips.iter().any(|tip| tip.text == *memo
                        && contains(tip.rect, node.rect.left, node.rect.top))
                );
                if size[0] == 1320. {
                    assert!(
                        width(node.rect) < 40.,
                        "measured copy fits below the old guard"
                    );
                }
            }
        });
    }

    #[test]
    fn claude_note_paints_compact_copy_with_full_read_only_semantics_and_tooltip() {
        for (language, compact) in [
            (crate::i18n::UiLanguage::Ko, "이동"),
            (crate::i18n::UiLanguage::En, "Move"),
        ] {
            crate::i18n::with_language(language, || {
                let clock = crate::dashboard_fixtures::clock();
                let mut snapshot = crate::dashboard_fixtures::snapshot();
                snapshot.sessions.sessions.truncate(1);
                let session = &mut snapshot.sessions.sessions[0];
                session.provider = "claude".into();
                session.routing = Some(m::SessionRouting {
                    action_id: "a".repeat(64),
                    on_5h_limit: "wait".into(),
                    mode: "recover".into(),
                });
                session.quota_wait = None;
                session.quota_wait_known = true;
                let id = session.id.clone();
                let mut state = DashboardState {
                    grouped: false,
                    range: Range::All,
                    ..Default::default()
                };
                let view = m::project(&snapshot, &mut state, &clock);
                let memo = &view.flat[0].memo;
                assert!(
                    memo.starts_with(crate::i18n::static_text("이동 (Claude는 자동 재개 미지원)"))
                );
                let scene = Scene::build(&view, &state, [1320., 900.], &Metrics::default());
                let key = format!("session:{id}:memo");
                let nodes: Vec<_> = scene
                    .accessibility
                    .iter()
                    .filter(|node| node.key == key)
                    .collect();
                assert_eq!(nodes.len(), 1);
                let node = nodes[0];
                assert_eq!(
                    node.parent.as_deref(),
                    Some(format!("session:{id}").as_str())
                );
                assert_eq!(node.role, Role::Text);
                assert_eq!(&node.name, memo);
                assert!(node.target.is_none() && !node.hidden);
                assert!(width(node.rect) >= 40. && height(node.rect) == 20.);
                assert!(scene.draw.iter().any(|draw| matches!(draw,
                    Draw::Text { rect, measure, .. }
                    if *rect == node.rect && measure.text == compact && !measure.wrap)));
                assert!(!scene.draw.iter().any(|draw| matches!(draw,
                    Draw::Text { rect, measure, .. }
                    if *rect == node.rect && measure.text == *memo)));
                assert!(scene.tooltips.iter().any(|tip| tip.text == *memo
                    && contains(tip.rect, node.rect.left, node.rect.top)
                    && node.rect.right <= tip.rect.right
                    && node.rect.bottom <= tip.rect.bottom));
                let row = scene.session_rows.iter().find(|row| row.id == id).unwrap();
                assert_eq!(height(row.rect), 35.);

                let mut ordinary = view.clone();
                ordinary.flat[0].memo = "standard memo".into();
                let baseline = Scene::build(&ordinary, &state, [1320., 900.], &Metrics::default());
                assert!(baseline.draw.iter().any(|draw| matches!(draw,
                    Draw::Text { rect, measure, .. }
                    if *rect == node.rect && measure.text == "standard memo")));
                assert_eq!(scene.focus_order(), baseline.focus_order());
                let hits = |scene: &Scene| {
                    scene
                        .targets
                        .iter()
                        .map(|hit| {
                            (
                                hit.target.clone(),
                                hit.rect,
                                hit.enabled,
                                hit.tooltip.clone(),
                            )
                        })
                        .collect::<Vec<_>>()
                };
                assert_eq!(hits(&scene), hits(&baseline));
                let baseline_row = baseline
                    .session_rows
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap();
                assert_eq!(row.rect, baseline_row.rect);

                ordinary.flat[0].memo.clear();
                let empty = Scene::build(&ordinary, &state, [1320., 900.], &Metrics::default());
                assert!(!empty.accessibility.iter().any(|node| node.key == key));
            });
        }
    }
}

#[cfg(test)]
mod n5c_session_control_tests {
    use super::*;
    fn fixture(mode: &str) -> (m::DashboardSnapshot, DashboardState, m::Clock, String) {
        let mut snapshot = crate::dashboard_fixtures::snapshot();
        snapshot.sessions.sessions.truncate(1);
        let row = &mut snapshot.sessions.sessions[0];
        row.routing = Some(m::SessionRouting {
            action_id: "a".repeat(64),
            on_5h_limit: "wait".into(),
            mode: mode.into(),
        });
        row.quota_wait = Some(m::QuotaWait {
            reset_at: crate::dashboard_fixtures::clock().utc() + chrono::Duration::minutes(5),
            waiting: 1,
        });
        row.quota_wait_known = true;
        let id = row.id.clone();
        (
            snapshot,
            DashboardState {
                grouped: false,
                range: Range::All,
                ..Default::default()
            },
            crate::dashboard_fixtures::clock(),
            id,
        )
    }
    #[test]
    fn n5c_session_actions_share_whole_painted_hit_and_uia_rects_in_both_languages() {
        for lang in [crate::i18n::UiLanguage::Ko, crate::i18n::UiLanguage::En] {
            crate::i18n::with_language(lang, || {
                for size in [[1024., 640.], [1320., 900.]] {
                    let (snapshot, mut state, clock, id) = fixture("recover");
                    let view = m::project(&snapshot, &mut state, &clock);
                    let scene = Scene::build(&view, &state, size, &Metrics::default());
                    for (target, role, label) in [
                        (
                            Target::SessionUrgency(id.clone()),
                            Role::Checkbox,
                            crate::i18n::static_text("긴급"),
                        ),
                        (
                            Target::SessionMoveNow(id.clone()),
                            Role::Button,
                            crate::i18n::static_text("지금 옮기기"),
                        ),
                    ] {
                        let hit = scene
                            .targets
                            .iter()
                            .find(|h| h.target == target)
                            .expect("visible action target");
                        let node = scene
                            .accessibility
                            .iter()
                            .find(|n| n.target.as_ref() == Some(&target))
                            .unwrap();
                        assert!(hit.enabled && node.enabled);
                        assert_eq!(hit.rect, node.rect);
                        assert_eq!(node.role, role);
                        assert_eq!(node.name, label);
                        assert!(width(hit.rect) > 16. && height(hit.rect) >= 28.);
                        assert!(
                            scene
                                .draw
                                .iter()
                                .any(|d| matches!(d,Draw::Stroke{rect,..} if *rect==hit.rect)),
                            "painted border and hit/UIA use the same whole box"
                        );
                        for [x, y] in [
                            [hit.rect.left + 1., hit.rect.top + 1.],
                            [hit.rect.right - 1., hit.rect.bottom - 1.],
                        ] {
                            assert_eq!(
                                scene.hit_test(x, y),
                                Some(&target),
                                "row selection must not swallow action edges"
                            );
                        }
                        assert!(scene.focus_order().contains(&&target));
                    }
                    let command = state.activate(&Target::SessionMoveNow(id));
                    assert_eq!(
                        command,
                        vec![Command::SetSessionUrgent {
                            id: "a".repeat(64),
                            urgent: true
                        }]
                    );
                    assert!(state.selected.is_none());
                }
            });
        }
    }
    #[test]
    fn r13_ui_urgency_keeps_pre_n5c_35_dip_rows_and_inline_targets() {
        for lang in [crate::i18n::UiLanguage::Ko, crate::i18n::UiLanguage::En] {
            crate::i18n::with_language(lang, || {
                for size in [[1024., 640.], [1320., 900.]] {
                    for grouped in [false, true] {
                        for waiting in [false, true] {
                            let (mut snapshot, mut state, clock, id) = fixture("recover");
                            state.grouped = grouped;
                            if !waiting {
                                snapshot.sessions.sessions[0].quota_wait = None;
                            }
                            for pending in [false, true] {
                                if pending {
                                    state.apply_urgency_update(
                                        crate::state::UrgencyUpdate::Pending { id: "a".repeat(64) },
                                    );
                                }
                                let view = m::project(&snapshot, &mut state, &clock);
                                let scene = Scene::build(&view, &state, size, &Metrics::default());
                                let row = scene.session_rows.iter().find(|r| r.id == id).unwrap();
                                assert_eq!(
                                    height(row.rect),
                                    35.,
                                    "pre-N5c row height: {lang:?} {size:?}"
                                );
                                let mut controls = Vec::new();
                                for target in [
                                    Target::SessionUrgency(id.clone()),
                                    Target::SessionMoveNow(id.clone()),
                                ] {
                                    if !waiting && matches!(target, Target::SessionMoveNow(_)) {
                                        continue;
                                    }
                                    let node = scene
                                        .accessibility
                                        .iter()
                                        .find(|n| n.target.as_ref() == Some(&target))
                                        .unwrap();
                                    assert!(
                                        node.rect.left >= row.rect.left
                                            && node.rect.right <= row.rect.right,
                                        "inline control outside row: {:?} {:?}",
                                        node.rect,
                                        row.rect
                                    );
                                    assert!(
                                        node.rect.top >= row.rect.top
                                            && node.rect.bottom <= row.rect.bottom
                                    );
                                    assert_eq!(
                                        height(node.rect),
                                        28.,
                                        "same full hit target height"
                                    );
                                    let hit =
                                        scene.targets.iter().find(|h| h.target == target).unwrap();
                                    assert_eq!(hit.rect, node.rect);
                                    controls.push(node.rect);
                                }
                                assert!(controls.windows(2).all(|w| w[0].right <= w[1].left));
                                for draw in &scene.draw {
                                    if let Draw::Text { rect, measure, .. } = draw
                                        && rect.top >= row.rect.top
                                        && rect.bottom <= row.rect.bottom
                                        && ![
                                            crate::i18n::static_text("긴급"),
                                            crate::i18n::static_text("지금 옮기기"),
                                            "\u{e73e}",
                                        ]
                                        .contains(&measure.text.as_str())
                                    {
                                        assert!(
                                            controls
                                                .iter()
                                                .all(|c| rect.right <= c.left
                                                    || rect.left >= c.right),
                                            "text overlaps action: {}",
                                            measure.text
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }
    }
    #[test]
    fn n5c_strict_and_pending_controls_block_row_fallthrough_and_expose_safe_tooltips() {
        let (snapshot, mut state, clock, id) = fixture("strict");
        let view = m::project(&snapshot, &mut state, &clock);
        let scene = Scene::build(&view, &state, [1024., 640.], &Metrics::default());
        let move_target = Target::SessionMoveNow(id.clone());
        let hit = scene
            .targets
            .iter()
            .find(|h| h.target == move_target)
            .unwrap();
        assert!(!hit.enabled);
        assert!(
            scene
                .hit_test(hit.rect.left + 1., hit.rect.top + 1.)
                .is_none()
        );
        assert_eq!(
            scene.tooltip_at(hit.rect.left + 1., hit.rect.top + 1.),
            Some(crate::i18n::static_text(
                "계정 고정 정책에서는 지금 옮길 수 없습니다"
            ))
        );
        assert!(state.activate(&move_target).is_empty());
        assert_eq!(
            state.activate(&Target::SessionUrgency(id.clone())),
            vec![Command::SetSessionUrgent {
                id: "a".repeat(64),
                urgent: true
            }]
        );
        let view = m::project(&snapshot, &mut state, &clock);
        let scene = Scene::build(&view, &state, [1024., 640.], &Metrics::default());
        assert!(
            scene
                .targets
                .iter()
                .filter(|h| matches!(
                    h.target,
                    Target::SessionUrgency(_) | Target::SessionMoveNow(_)
                ))
                .all(|h| !h.enabled)
        );
        let check = scene
            .accessibility
            .iter()
            .find(|n| n.target == Some(Target::SessionUrgency(id.clone())))
            .unwrap();
        assert!(
            !check.selected,
            "pending does not optimistically check the preference"
        );
    }
}
