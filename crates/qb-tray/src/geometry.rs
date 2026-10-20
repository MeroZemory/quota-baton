//! Dashboard DIP layout and semantic scene. Paint/hit/UIA use these same boxes.
use crate::{Rect, dashboard_history as history, dashboard_model as m, state::*, v2};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
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
    CloseDetail,
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
}
#[derive(Clone, Copy, Debug)]
pub enum Color {
    Page,
    Surface,
    Footer,
    Border,
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
}
impl Builder<'_> {
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
        let measure = Measure {
            text: value.into(),
            size,
            line_height: if size >= 15. {
                22.
            } else if size <= 12. {
                16.
            } else {
                20.
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
        let m = Measure {
            text: text.into(),
            size,
            line_height: 20.,
            width: 4096.,
            bold,
            wrap: false,
            face: Face::Text,
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
        let accent = matches!(&target, Target::OpenTrayAccount(_) | Target::OpenTrayAdd);
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
                    self.scene.accessibility[idx].name = "상태 정렬".into();
                }
            }
            if target == Target::CloseDetail {
                self.scene.accessibility[idx].name = "세션 상세 닫기".into();
            }
            if target == Target::Search {
                self.scene.accessibility[idx].name = "세션 검색".into();
            }
            if target == Target::EventSearch {
                self.scene.accessibility[idx].name = "기록 검색".into();
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
            4.,
        );
        self.stroke(
            r,
            if accent { Color::Accent } else { Color::Border },
            1.,
            4.,
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
        };
        b.fill(outer, Color::Page, 0.);
        b.fill(rect(0., 0., w, 48.), Color::Surface, 0.);
        b.line([0., 47.5], [w, 47.5], Color::Border, 1., None);
        for (i, (tab, label)) in [
            (Tab::Overview, "개요"),
            (Tab::Events, "기록"),
            (Tab::Usage, "사용량"),
        ]
        .iter()
        .enumerate()
        {
            let r = rect(20. + i as f32 * 72., 12., 68., 36.);
            b.control(
                r,
                label,
                Target::Tab(*tab),
                Role::Tab,
                state.tab == *tab,
                None,
            );
            if state.tab == *tab {
                b.fill(rect(r.left + 12., 45., 44., 3.), Color::Accent, 0.);
            }
        }
        if view.todo > 0 {
            b.text(
                rect(242., 15., 88., 20.),
                format!("할 일 {}", view.todo),
                12.,
                true,
                Color::Bad,
                false,
            );
        }
        if view.connection.core_ok {
            let right = w - 20.;
            b.semantic(
                "header:counts".into(),
                None,
                Role::Text,
                format!(
                    "세션 {}개, 활성 {}개, 요청 중 {}개{}",
                    view.session_count,
                    view.active_count,
                    view.inflight_text(),
                    if view.truncated {
                        " · 일부 세션만 표시"
                    } else {
                        ""
                    }
                ),
                rect(right - 380., 16., 380., 20.),
                None,
            );
            b.text(
                rect(right - 380., 16., 380., 20.),
                format!(
                    "세션 {}개    활성 {}개    요청 중 {}개",
                    view.session_count,
                    view.active_count,
                    view.inflight_text()
                ),
                12.,
                false,
                Color::Secondary,
                false,
            );
            let summary_space = (right - 400. - 330.).max(0.);
            for (i, s) in view.summaries.iter().take(2).enumerate() {
                let sw = summary_space / 2.;
                let r = rect(330. + i as f32 * sw, 14., (sw - 12.).max(0.), 24.);
                b.text(r, s.clone(), 12., false, Color::Text, false);
                b.semantic(format!("summary:{i}"), None, Role::Text, s.clone(), r, None);
            }
        } else {
            b.text(
                rect(w - 330., 15., 310., 22.),
                "코어에 연결할 수 없습니다",
                13.,
                true,
                Color::Bad,
                false,
            );
        }
        let mut top = 64.;
        if !view.connection.core_ok {
            b.fill(rect(20., 64., w - 40., 28.), Color::Select, 4.);
            b.text(
                rect(32., 68., w - 64., 20.),
                format!("{}초 후 다시 연결합니다", view.connection.retry_seconds),
                12.,
                false,
                Color::Secondary,
                false,
            );
            top += 40.;
            b.scene.draw.push(Draw::Alpha(0.5));
        }
        let body = rect(20., top, w - 40., (h - top - 20.).max(0.));
        if view.empty == m::EmptyState::NoAccounts {
            b.card(body);
            b.semantic(
                "empty:accounts:title".into(),
                None,
                Role::Text,
                "아직 계정이 없습니다".into(),
                rect(body.left + 24., body.top + 40., width(body) - 48., 28.),
                None,
            );
            b.semantic("empty:accounts:help".into(),None,Role::Text,"Claude나 Codex 계정을 추가하세요. 한 계정이 한도에 도달하면 다른 계정으로 이어서 처리합니다.".into(),rect(body.left+24.,body.top+80.,width(body)-48.,48.),None);
            b.text(
                rect(body.left + 24., body.top + 40., width(body) - 48., 28.),
                "아직 계정이 없습니다",
                16.,
                true,
                Color::Text,
                false,
            );
            b.text(rect(body.left+24.,body.top+80.,width(body)-48.,48.),"Claude나 Codex 계정을 추가하세요. 한 계정이 한도에 도달하면 다른 계정으로 이어서 처리합니다.",13.,false,Color::Secondary,true);
            b.control(
                rect(body.left + 24., body.top + 148., 200., 32.),
                "트레이에서 계정 추가",
                Target::OpenTrayAdd,
                Role::Button,
                false,
                None,
            );
        } else {
            match state.tab {
                Tab::Overview => overview(&mut b, view, state, body),
                Tab::Events => events(&mut b, view, state, body),
                Tab::Usage => usage(&mut b, view, state, body),
            }
        }
        if !view.connection.core_ok {
            b.scene.draw.push(Draw::Alpha(1.));
        }
        if let Some(popup) = state.popup {
            dropdown(&mut b, popup);
        }
        b.scene
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
            parts.push(format!(
                "{} {} 남음",
                s.name,
                value
                    .map(|n| format!("{}%", n.round() as u8))
                    .unwrap_or_else(m::unknown)
            ));
        }
        Some(ChartTooltip {
            text: format!(
                "{} · {}",
                at.with_timezone(&chart.offset).format("%H:%M"),
                parts.join(" · ")
            ),
            position: [x.clamp(chart.rect.left, chart.rect.right), chart.rect.top],
        })
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
        Popup::EventDays => [(1, "오늘"), (2, "최근 2일"), (7, "최근 7일")]
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

fn overview(b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, body: Rect) {
    let narrow = width(body) < 1140.;
    let (table, side) = if narrow {
        let th = (height(body) * 0.6).min(height(body) - 156.).max(120.);
        (
            rect(body.left, body.top, width(body), th),
            rect(
                body.left,
                body.top + th + 16.,
                width(body),
                height(body) - th - 16.,
            ),
        )
    } else {
        (
            rect(body.left, body.top, width(body) - 376., height(body)),
            rect(body.right - 360., body.top, 360., height(body)),
        )
    };
    sessions(b, v, s, table);
    if let Some(d) = &v.detail {
        detail(b, v, s, side, d);
    } else {
        recent(b, v, s, side);
    }
}
fn sessions(b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, card: Rect) {
    b.card(card);
    b.semantic(
        "sessions:table".into(),
        None,
        Role::Table,
        "세션".into(),
        card,
        None,
    );
    let mut x = card.left + 12.;
    let y = card.top + 8.;
    b.text(
        rect(x, y + 6., 42., 22.),
        "세션",
        14.,
        true,
        Color::Text,
        false,
    );
    x += 48.;
    for (i, (filter, label)) in [
        (ProviderFilter::All, "전체"),
        (ProviderFilter::Claude, "Claude"),
        (ProviderFilter::Codex, "Codex"),
    ]
    .iter()
    .enumerate()
    {
        let text = format!("{label} {}", v.provider_counts[i]);
        b.control(
            rect(x, y, 76., 28.),
            &text,
            Target::Provider(*filter),
            Role::Button,
            s.provider == *filter,
            None,
        );
        x += 78.;
    }
    x += 8.;
    b.control(
        rect(x, y, 106., 28.),
        s.range.label(),
        Target::RangeMenu,
        Role::ComboBox,
        false,
        None,
    );
    x += 116.;
    b.control(
        rect(x, y, 136., 28.),
        if s.grouped {
            "☑ 계정별로 묶기"
        } else {
            "☐ 계정별로 묶기"
        },
        Target::ToggleGrouping,
        Role::Checkbox,
        s.grouped,
        None,
    );
    let search_w = (card.right - 12. - (x + 148.)).clamp(64., 200.);
    b.control(
        rect(card.right - search_w - 12., y, search_w, 28.),
        if s.search.is_empty() {
            "검색"
        } else {
            &s.search
        },
        Target::Search,
        Role::Edit,
        false,
        None,
    );
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
        card.top + 48.,
        width(card) - 2.,
        if no_sessions { 0. } else { 32. },
    );
    b.fill(header, Color::Footer, 0.);
    let columns = columns(width(card) - 28., !s.grouped);
    let mut cx = card.left + 12.;
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
                                "내림차순"
                            } else {
                                "오름차순"
                            }
                        } else {
                            "정렬"
                        }
                        .into(),
                    ),
                );
            }
            cx += *cw;
        }
    }
    let foot = rect(card.left + 1., card.bottom - 32., width(card) - 2., 31.);
    b.fill(foot, Color::Footer, 0.);
    b.text(rect(foot.left+12.,foot.top+8.,width(foot)-24.,18.),format!("● 요청 중  ◯ 5분 안에 활동  ● 쉬는 중 · 활성 세션 = 요청 중 + 5분 안에 활동 · {} 기준 · {}{}",v.clock.now.format("%H:%M:%S"),if v.connection.stream_ok{"바뀌면 바로 반영"}else{"다시 연결 중"},if v.truncated{" · 최근 2,000개까지 표시"}else{""}),12.,false,if v.connection.stream_ok{Color::Secondary}else{Color::Warn},false);
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
    let explanation = "codex-qb나 claude-qb로 실행한 Codex·Claude가 요청을 보내면 여기에 바로 나타납니다. 그냥 codex나 claude로 실행하면 quota-baton을 거치지 않습니다.";
    let empty_help_height = if no_sessions {
        b.measured_height_aligned(explanation, empty_width, 13., false, TextAlign::Center)
    } else {
        0.
    };
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
        + 24.
        + if empty_button { 44. } else { 0. }
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
        let mut total = 0.;
        for g in &v.groups {
            if provider != g.provider {
                total += 26.;
                provider = &g.provider;
            }
            total += 34.;
            if !g.collapsed {
                total += if g.rows.is_empty() {
                    30.
                } else {
                    g.rows.len() as f32 * 34.
                };
            }
        }
        total
    } else {
        v.flat.len() as f32 * 34.
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
            b.center_text(
                rect(left, ry, empty_width, empty_help_height),
                explanation,
                13.,
                false,
                Color::Secondary,
                true,
            );
            b.semantic(
                "empty:sessions:help".into(),
                Some("empty:sessions".into()),
                Role::Text,
                explanation.into(),
                rect(left, ry, empty_width, empty_help_height),
                None,
            );
            ry += empty_help_height + 16.;
            for (i, command) in ["codex-qb", "claude-qb"].iter().enumerate() {
                let r = rect(cx - 88. + i as f32 * 92., ry, 84., 24.);
                b.fill(r, Color::Footer, 4.);
                b.text_face(
                    rect(r.left + 6., r.top + 3., width(r) - 12., 20.),
                    *command,
                    12.,
                    false,
                    Color::Text,
                    false,
                    Face::Mono,
                    TextAlign::Center,
                );
            }
            ry += 24.;
            if empty_button {
                ry += 12.;
                b.control(
                    rect(cx - 75., ry, 150., 32.),
                    "오늘 전체 보기",
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
            b.semantic(
                "empty:search".into(),
                Some("sessions:table".into()),
                Role::Text,
                format!("“{text}”와 맞는 세션이 없습니다"),
                rect(viewport.left + 24., ry + 32., width(viewport) - 48., 40.),
                None,
            );
            b.text(
                rect(viewport.left + 24., ry + 32., width(viewport) - 48., 40.),
                format!("“{text}”와 맞는 세션이 없습니다"),
                14.,
                false,
                Color::Secondary,
                true,
            )
        }
        _ => {
            if s.grouped {
                let mut provider = "";
                for group in &v.groups {
                    if provider != group.provider {
                        b.text(
                            rect(viewport.left + 12., ry + 5., width(viewport) - 24., 20.),
                            v2::provider_label(&group.provider),
                            12.,
                            true,
                            Color::Secondary,
                            false,
                        );
                        ry += 26.;
                        provider = &group.provider;
                    }
                    let header_height =
                        (34. + group.windows.len().saturating_sub(1) as f32 * 26.).max(34.);
                    group_header(
                        b,
                        group,
                        rect(viewport.left, ry, width(viewport), header_height),
                    );
                    ry += header_height;
                    if !group.collapsed {
                        if group.rows.is_empty() {
                            b.text(
                                rect(viewport.left + 44., ry + 7., width(viewport) - 56., 20.),
                                group.no_sessions.clone(),
                                12.,
                                false,
                                Color::Secondary,
                                false,
                            );
                            ry += 30.;
                        } else {
                            for row in &group.rows {
                                session(
                                    b,
                                    row,
                                    rect(viewport.left, ry, width(viewport), 34.),
                                    &columns,
                                );
                                ry += 34.;
                            }
                        }
                    }
                }
            } else {
                for row in &v.flat {
                    session(
                        b,
                        row,
                        rect(viewport.left, ry, width(viewport), 34.),
                        &columns,
                    );
                    ry += 34.;
                }
            }
        }
    }
    b.end_region(old);
}
pub fn columns(available: f32, flat: bool) -> Vec<(SortKey, &'static str, f32)> {
    let mut cols = vec![(SortKey::Status, "", 16.), (SortKey::Session, "세션", 268.)];
    if flat {
        cols.push((SortKey::Account, "계정", 160.));
    }
    cols.extend([
        (SortKey::Model, "모델", 136.),
        (SortKey::Start, "시작", 76.),
        (SortKey::LastActivity, "마지막 활동", 112.),
        (SortKey::Requests, "오늘 요청", 80.),
    ]);
    let used = cols.iter().map(|c| c.2).sum::<f32>();
    if available - used >= 96. {
        cols.push((SortKey::Memo, "메모", available - used));
    } else {
        for key in [SortKey::Start, SortKey::Model] {
            if cols.iter().map(|c| c.2).sum::<f32>() <= available {
                break;
            }
            cols.retain(|c| c.0 != key);
        }
    }
    cols
}
fn group_header(b: &mut Builder<'_>, g: &m::AccountGroup, r: Rect) {
    b.fill(r, Color::Footer, 0.);
    let label = format!(
        "{} {} · {} · {}",
        if g.collapsed { "›" } else { "⌄" },
        g.name,
        g.plan,
        g.chip
    );
    b.control(
        rect(r.left + 8., r.top + 1., (width(r) - 500.).max(184.), 32.),
        &label,
        Target::Group(g.id.clone()),
        Role::Button,
        false,
        None,
    );
    if let Some(node) = b
        .scene
        .accessibility
        .last_mut()
        .filter(|n| n.target.as_ref() == Some(&Target::Group(g.id.clone())))
    {
        node.expanded = Some(!g.collapsed);
        node.parent = Some("sessions:table".into());
    }
    let lx = (r.right - 490.).max(r.left + 210.);
    let threshold = g
        .threshold
        .map(|n| n.to_string())
        .unwrap_or_else(m::unknown);
    let load = format!(
        "활성 세션 {} / {}{}",
        g.active,
        threshold,
        if g.threshold.is_some_and(|t| g.active > t) {
            " · 기준 넘음"
        } else {
            ""
        }
    );
    b.text(
        rect(lx, r.top + 7., 210., 20.),
        load.clone(),
        12.,
        true,
        Color::Text,
        false,
    );
    let pips = g.threshold.unwrap_or(0).max(g.active).min(20);
    for i in 0..pips {
        let pr = rect(lx + i as f32 * 10., r.top + 26., 8., 5.);
        if i < g.active {
            b.fill(
                pr,
                if g.threshold.is_some_and(|t| i >= t) {
                    Color::Accent
                } else {
                    Color::BarOk
                },
                2.,
            );
        } else {
            b.stroke(pr, Color::Accent, 1., 2.);
        }
    }
    let tip = format!(
        "활성 세션 {}개 / 기준 {}개. 기준을 넘으면 새 세션은 여유 있는 다른 계정에 먼저 배정합니다. 연결된 세션 {}개.",
        g.active, threshold, g.bound
    );
    b.tooltip(rect(lx, r.top, 210., 34.), tip.clone());
    b.semantic(
        format!("load:{}", g.id),
        None,
        Role::Text,
        tip,
        rect(lx, r.top, 210., 34.),
        None,
    );
    let right = rect(r.right - 270., r.top, 258., 34.);
    if g.relogin {
        b.control(
            rect(right.left, right.top + 2., width(right), 30.),
            "트레이에서 다시 로그인",
            Target::OpenTrayAccount(g.id.clone()),
            Role::Button,
            false,
            None,
        );
    } else if g.windows.is_empty() {
        b.text(
            rect(right.left, right.top + 3., width(right), 18.),
            g.remaining.clone(),
            12.,
            true,
            Color::Text,
            false,
        );
        b.text(
            rect(right.left, right.top + 19., width(right) - 60., 16.),
            g.reset.clone(),
            12.,
            false,
            Color::Secondary,
            false,
        );
        if let Some(p) = g.remaining_pct {
            let bar = rect(right.right - 48., right.top + 24., 48., 4.);
            b.fill(bar, Color::Track, 2.);
            b.fill(
                rect(bar.left, bar.top, 48. * p as f32 / 100., 4.),
                bar_color(p),
                2.,
            );
        }
    } else {
        for (index, window) in g.windows.iter().enumerate() {
            let top = right.top + 2. + index as f32 * 26.;
            let reset_width = b.measured_width(&window.reset, 12., false);
            let label = format!("{} {}", window.label, window.remaining);
            b.text(
                rect(
                    right.left,
                    top,
                    (width(right) - reset_width - 8.).max(0.),
                    16.,
                ),
                label.clone(),
                12.,
                true,
                Color::Text,
                false,
            );
            if !window.reset.is_empty() {
                b.text(
                    rect(right.right - reset_width, top, reset_width, 16.),
                    window.reset.clone(),
                    12.,
                    false,
                    Color::Secondary,
                    false,
                );
            }
            let bar = rect(right.left, top + 19., width(right), 4.);
            b.fill(bar, Color::Track, 2.);
            b.fill(
                rect(
                    bar.left,
                    bar.top,
                    width(bar) * f32::from(window.remaining_pct) / 100.,
                    4.,
                ),
                bar_color(window.remaining_pct),
                2.,
            );
            b.semantic(
                format!("usage:{}:{index}", g.id),
                Some("sessions:table".into()),
                Role::Text,
                format!("{label} · {}", window.reset),
                rect(right.left, top, width(right), 23.),
                None,
            );
        }
    }
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
fn session(b: &mut Builder<'_>, row: &m::SessionRow, r: Rect, columns: &[(SortKey, &str, f32)]) {
    b.scene.session_rows.push(SessionGeometry {
        id: row.id.clone(),
        rect: r,
    });
    b.tooltip(rect(r.left + 12., r.top, 16., height(r)), row.dot.clone());
    b.tooltip(
        rect(r.left + 28., r.top, 268., height(r)),
        row.label_source.clone(),
    );
    let key = format!("session:{}", row.id);
    if row.selected {
        b.fill(r, Color::Select, 0.);
        b.fill(rect(r.left, r.top, 3., height(r)), Color::Accent, 0.);
    }
    let idx = b.semantic(
        key.clone(),
        Some("sessions:table".into()),
        Role::Row,
        format!(
            "{}, {} · {}, 계정 {}, 모델 {}, {}, 오늘 요청 {}",
            row.label, row.client, row.short_id, row.account, row.model, row.dot, row.requests
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
                    rect(cr.left, cr.top, (*cw - 110.).max(80.), 20.),
                    row.label.clone(),
                    13.,
                    true,
                    Color::Text,
                    false,
                );
                b.text(
                    rect(cr.left + (*cw - 106.), cr.top, 106., 20.),
                    format!("{} · {}", row.client, row.short_id),
                    12.,
                    false,
                    Color::Secondary,
                    false,
                );
            }
            _ => {
                let text = match key {
                    SortKey::Account => &row.account,
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
                    if *key == SortKey::Memo && row.memo.contains("크레딧") {
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
    b.line(
        [r.left, r.bottom - 0.5],
        [r.right, r.bottom - 0.5],
        Color::Border,
        1.,
        None,
    );
}
fn recent(b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, card: Rect) {
    b.card(card);
    b.text(
        rect(card.left + 16., card.top + 14., 100., 22.),
        "최근 일",
        14.,
        true,
        Color::Text,
        false,
    );
    b.control(
        rect(card.right - 140., card.top + 8., 128., 30.),
        "기록 모두 보기 ›",
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
    let tag = rect(x, r.top + 12., 96., 20.);
    b.fill(tag, Color::ChipBg(e.tone), 4.);
    b.text(
        rect(tag.left + 6., tag.top + 2., 84., 18.),
        e.tag.clone(),
        11.,
        true,
        Color::ChipFg(e.tone),
        false,
    );
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
            "트레이에서 다시 로그인",
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
        Some("세션 상세 닫기".into()),
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
            format!("원래 세션 번호 앞 8자: {prefix}"),
            12.,
            false,
            Color::Secondary,
            false,
        );
    }
    b.text(
        rect(
            card.left + 16.,
            card.top + header - 44.,
            width(card) - 32.,
            40.,
        ),
        d.status.clone(),
        12.,
        false,
        Color::Accent,
        true,
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
            "이 세션은 더 이상 연결되어 있지 않습니다",
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
            format!("detail:{}:fact:{key}", d.id),
            None,
            Role::Text,
            format!("{key}: {value}"),
            rect(viewport.left + 16., y, width(viewport) - 32., h),
            None,
        );
        y += h + 4.;
    }
    let mut index = 0;
    for (title, items) in [("계정 이력", &d.timeline), ("최근 요청", &d.requests)] {
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
            if title == "계정 이력" {
                b.fill(
                    rect(viewport.left + 17., y + 5., 6., 6.),
                    if text.contains("(현재)") {
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
                if text.contains("실패") {
                    Color::Bad
                } else {
                    Color::Text
                },
                true,
            );
            b.semantic(
                format!("detail:{}:{title}:{i}", d.id),
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
        "대화 내용은 보지도 기록하지도 않습니다. 세션 번호는 ledger의 session 값과 같습니다.",
        12.,
        false,
        Color::Secondary,
        true,
    );
    let _ = v;
    b.end_region(old);
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
        b.control(
            rect(x, toolbar_y, w, 28.),
            &label,
            Target::EventFilter(*filter),
            Role::Button,
            s.event_filter == *filter,
            None,
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
            1 => "오늘",
            2 => "최근 2일",
            _ => "최근 7일",
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
            "기록 검색"
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
            "보관 한도로 일부 기록만 표시합니다.",
            12.,
            false,
            Color::Warn,
            false,
        );
        b.semantic(
            "events:truncated".into(),
            None,
            Role::Text,
            "보관 한도로 일부 기록만 표시합니다.".into(),
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
    let middle = (width(card) - 32. - 64. - 96. - 260. - 36.).max(100.);
    let heights: Vec<_> = v
        .events
        .iter()
        .map(|e| {
            (b.measured_height(&e.body, middle, 12., false)
                + 52.
                + if e.relogin { 36. } else { 0. })
            .max(92.)
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
        let tag = rect(time_x + 76., y + 12., 96., 20.);
        b.fill(tag, Color::ChipBg(e.tone), 4.);
        b.text(
            rect(tag.left + 6., tag.top + 2., 84., 18.),
            e.tag.clone(),
            11.,
            true,
            Color::ChipFg(e.tone),
            false,
        );
        let tx = tag.right + 12.;
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
                (h - 52. - if e.relogin { 36. } else { 0. }).max(16.),
            ),
            e.body.clone(),
            12.,
            false,
            Color::Secondary,
            true,
        );
        let refs = viewport.right - 276.;
        if let Some(id) = &e.session {
            b.control(
                rect(refs, y + 8., 260., 28.),
                &format!("세션 {} ›", e.session_label.as_deref().unwrap_or(&id[..6])),
                Target::SessionLink(id.clone()),
                Role::Button,
                false,
                None,
            );
        }
        if let Some(id) = &e.account {
            b.control(
                rect(refs, y + 42., 260., 28.),
                "계정 보기 ›",
                Target::AccountLink(id.clone()),
                Role::Button,
                false,
                None,
            );
        }
        if e.relogin
            && let Some(id) = &e.account
        {
            b.control(
                rect(tx, y + h - 34., 200., 28.),
                "트레이에서 다시 로그인",
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
fn usage(b: &mut Builder<'_>, v: &m::DashboardView, s: &DashboardState, card: Rect) {
    b.card(card);
    b.text(
        rect(card.left + 16., card.top + 14., 120., 24.),
        "오늘 사용량",
        14.,
        true,
        Color::Text,
        false,
    );
    b.text(
        rect(card.left + 144., card.top + 16., 300., 20.),
        format!(
            "남은 양 · 00:00–24:00 · {}",
            v.clock.day_label(v.clock.utc())
        ),
        12.,
        false,
        Color::Secondary,
        false,
    );
    b.text(
        rect(card.right - 520., card.top + 16., 504., 20.),
        "━ 5시간 남은 양  ┄ 주간 남은 양  ⋮ 초기화  ▧ 크레딧으로 처리",
        12.,
        false,
        Color::Secondary,
        false,
    );
    let viewport = rect(
        card.left + 12.,
        card.top + 52.,
        width(card) - 24.,
        height(card) - 64.,
    );
    let columns = if width(card) < 1140. { 2 } else { 3 };
    let tw = (width(viewport) - (columns - 1) as f32 * 12.) / columns as f32;
    let rows = v.usage.len().div_ceil(columns);
    let content = rows as f32 * 292.;
    let (old, offset) = b.begin_region(ScrollRegion::Usage, viewport, content, s.usage_offset);
    let available_nodes = history::SCENE_NODE_BUDGET.saturating_sub(b.scene.accessibility.len());
    for (i, tile) in v.usage.iter().enumerate() {
        let r = rect(
            viewport.left + (i % columns) as f32 * (tw + 12.),
            viewport.top + (i / columns) as f32 * 292. - offset,
            tw,
            280.,
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
    b.card(r);
    b.text(
        rect(r.left + 12., r.top + 10., 70., 18.),
        v2::provider_label(&t.provider),
        12.,
        false,
        Color::Secondary,
        false,
    );
    b.text(
        rect(r.left + 12., r.top + 32., width(r) - 130., 24.),
        format!("{} · {}", t.name, t.plan),
        13.,
        true,
        Color::Text,
        false,
    );
    b.text(
        rect(r.right - 110., r.top + 34., 98., 20.),
        t.chip.clone(),
        12.,
        true,
        Color::ChipFg(t.tone),
        false,
    );
    b.text(
        rect(r.left + 12., r.top + 64., width(r) - 24., 20.),
        format!(
            "지금 {}",
            if t.series.is_empty() {
                "사용량 확인 전".into()
            } else {
                t.series
                    .iter()
                    .map(|s| {
                        format!(
                            "{} {} 남음",
                            s.name,
                            s.remaining
                                .map(|p| format!("{p}%"))
                                .unwrap_or_else(m::unknown)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
            }
        ),
        12.,
        false,
        Color::Text,
        false,
    );
    let future_labels: BTreeSet<_> = t
        .series
        .iter()
        .flat_map(|series| {
            series
                .future_resets
                .iter()
                .map(|at| v.clock.time(Some(*at)))
        })
        .collect();
    let annotation_space = if future_labels.is_empty() { 0. } else { 20. };
    let plot = rect(
        r.left + 40.,
        r.top + 102. + annotation_space,
        width(r) - 64.,
        130. - annotation_space,
    );
    if !future_labels.is_empty() {
        b.text(
            rect(plot.left, r.top + 86., width(plot), 16.),
            format!(
                "{} 초기화",
                future_labels.into_iter().collect::<Vec<_>>().join(" · ")
            ),
            11.,
            false,
            Color::Secondary,
            false,
        );
    }
    let from = v.clock.day_start;
    let to = v.clock.day_end;
    let total = to.signed_duration_since(from).num_milliseconds().max(1) as f64;
    let x = |at: DateTime<Utc>| {
        plot.left
            + (at.signed_duration_since(from).num_milliseconds() as f64 / total).clamp(0., 1.)
                as f32
                * width(plot)
    };
    let y = |pct: f64| plot.bottom - (pct.clamp(0., 100.) as f32 / 100.) * height(plot);
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
    b.text(
        rect(
            (nowx - 12.).clamp(plot.left, plot.right - 26.),
            plot.top - 18.,
            28.,
            18.,
        ),
        "지금",
        11.,
        true,
        Color::Text,
        false,
    );
    let mut end_labels: Vec<Rect> = Vec::new();
    for (series_index, series) in t.series.iter().enumerate() {
        let color = if series.name == "주간" || series.name == "7d" || series_index > 0 {
            Color::Series7d
        } else {
            Color::BarOk
        };
        let dash = if series.name == "주간" || series.name == "7d" || series_index > 0 {
            Some([5., 3.])
        } else {
            None
        };
        let mut previous: Option<(DateTime<Utc>, f64)> = None;
        let resets: BTreeSet<_> = series.resets.iter().copied().collect();
        for (at, pct) in &series.points {
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
        if let Some((at, Some(pct))) = series.points.last() {
            let left = (x(*at) - 32.).clamp(plot.left, plot.right - 40.);
            let preferred = (y(*pct) - 18.).clamp(plot.top + 2., plot.bottom - 18.);
            let end_rect = std::iter::once(preferred)
                .chain(
                    (0..=((height(plot) - 20.) / 20.) as usize)
                        .map(|row| plot.top + 2. + row as f32 * 20.),
                )
                .map(|top| rect(left, top, 40., 18.))
                .find(|r| {
                    end_labels
                        .iter()
                        .all(|old| intersection(*old, *r).is_none())
                })
                .unwrap_or(rect(left, preferred, 40., 18.));
            end_labels.push(end_rect);
            b.text(
                end_rect,
                format!("{}%", pct.round() as u8),
                11.,
                series_index == 0,
                color,
                false,
            );
        }
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
        b.text(
            rect(r.left + 12., r.bottom - 24., width(r) - 24., 18.),
            note,
            12.,
            false,
            Color::Bad,
            false,
        );
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
}

#[cfg(test)]
#[path = "dashboard_geometry_tests.rs"]
mod tests;
