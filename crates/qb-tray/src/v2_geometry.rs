//! Shared tray-v2 DIP geometry, independent of rendering, Win32, and backend calls.
//! Integration expects sibling `crate::v2` and the incumbent `crate::Rect`.
//! All public boxes are scene coordinates; list boxes may extend outside `list`.
//! Draw list content clipped to `list`; targets are already clipped identically.
use crate::{Rect, v2};
use std::collections::{BTreeMap, BTreeSet};

pub const WIDTH: f32 = 360.;
pub const FOOTER: f32 = 48.;
pub const QUIT_DETAILS: [&str; 2] = [
    "프록시는 계속 실행됩니다. CLI는 그대로 작동합니다.",
    "quota-baton을 거치는 CLI 요청이 멈춥니다.",
];

#[derive(Debug, Clone, Default)]
pub struct FrozenSection {
    pub rows: Vec<String>,
    pub disabled_rows: Vec<String>,
}
/// Capture/update this in the caller at open or when appending newly seen IDs.
/// Preserve these vectors for the open lifetime, including original membership.
pub fn capture_order(view: &v2::View) -> BTreeMap<String, FrozenSection> {
    view.sections
        .iter()
        .map(|s| {
            (
                s.provider.clone(),
                FrozenSection {
                    rows: s.rows.iter().map(|r| r.raw.id.clone()).collect(),
                    disabled_rows: s.disabled_rows.iter().map(|r| r.raw.id.clone()).collect(),
                },
            )
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Presentation {
    /// Absent provider = model recommendation; Some(false) overrides auto-expand.
    pub expanded: BTreeMap<String, bool>,
    pub frozen: BTreeMap<String, FrozenSection>,
    pub offset: f32,
    pub overlay: Overlay,
    pub core_ok: bool,
    pub pending_ids: BTreeSet<String>,
    /// Actual text advance in DIPs, keyed by `text_key`. Missing keys use an
    /// explicit deterministic estimate; native callers should supply font metrics.
    pub text_widths: BTreeMap<String, f32>,
    /// Actual DirectWrite keep-all line count for each footer-menu subtitle.
    pub subtitle_lines: BTreeMap<String, usize>,
    /// Dialog body line count after caller's keep-all wrapping. Default is 3.
    pub credits_body_lines: usize,
    /// Optional scroll within a popup when its contents exceed the scene cap.
    pub popup_offset: f32,
}
impl Default for Presentation {
    fn default() -> Self {
        Self {
            expanded: BTreeMap::new(),
            frozen: BTreeMap::new(),
            offset: 0.,
            overlay: Overlay::None,
            core_ok: true,
            pending_ids: BTreeSet::new(),
            text_widths: BTreeMap::new(),
            subtitle_lines: BTreeMap::new(),
            credits_body_lines: 3,
            popup_offset: 0.,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    None,
    RowMenu(String),
    Settings,
    Quit,
    Add,
    TakeoverProviders,
    TakeoverCard {
        body: String,
        confirming: bool,
        ready: bool,
        replace: bool,
    },
    CreditsConfirm(String),
    DeleteConfirm(String),
    /// Replaces the list; provider identifies the caller-owned login flow.
    Login(String),
}
/// Measurement cache keys distinguish font size/weight. Values exclude padding.
pub fn text_key(text: &str, size: u8, bold: bool) -> String {
    format!("{size}:{}:{text}", if bold { "b" } else { "r" })
}
fn advance(p: &Presentation, text: &str, size: u8, bold: bool) -> f32 {
    p.text_widths
        .get(&text_key(text, size, bold))
        .copied()
        .filter(|x| x.is_finite() && *x >= 0.)
        .unwrap_or_else(|| {
            text.chars()
                .map(|c| {
                    if c.is_whitespace() {
                        0.33
                    } else if c.is_ascii() {
                        0.55
                    } else {
                        1.0
                    }
                })
                .sum::<f32>()
                * f32::from(size)
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterAction {
    Add,
    Settings,
    Quit,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    RefreshUsage,
    Relogin,
    RefreshToken,
    ToggleCredits,
    ToggleDrain,
    Pause,
    Resume,
    Disable,
    Enable,
    Delete,
    OpenDashboard,
    OpenConfig,
    OpenLogs,
    AddClaude,
    AddCodex,
    Takeover,
    TakeoverCodex,
    TakeoverClaude,
    QuitTray,
    QuitAll,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Row(String),
    Inline {
        id: String,
        action: v2::ActionKind,
    },
    Disclosure(String),
    Footer(FooterAction),
    AddProvider(String),
    Menu {
        id: Option<String>,
        action: MenuAction,
    },
    TakeoverNext,
    TakeoverCommit,
    ConfirmCredits(String),
    ConfirmDelete(String),
    CancelDialog,
    LoginCopy,
    LoginCancel,
    /// Modal surface/scrim absorbs clicks, preventing activation underneath.
    Blocked,
    /// Caller closes the menu, without forwarding the same click underneath.
    DismissOverlay,
}
#[derive(Debug, Clone)]
pub struct HitBox {
    pub rect: Rect,
    pub target: Target,
    pub enabled: bool,
}
#[derive(Debug, Clone)]
pub struct ButtonBox {
    pub visual: Rect,
    pub hit: Rect,
    pub label: String,
    pub target: Target,
    pub enabled: bool,
}
#[derive(Debug, Clone)]
pub struct HeaderBox {
    pub rect: Rect,
    pub title: Rect,
    pub summaries: Vec<Rect>,
    pub banner: Option<Rect>,
    pub note: Option<Rect>,
    pub divider: Rect,
}
#[derive(Debug, Clone)]
pub struct FooterBox {
    pub rect: Rect,
    pub buttons: Vec<ButtonBox>,
    pub stale_space: Rect,
}
#[derive(Debug, Clone)]
pub struct RowBox {
    pub id: String,
    pub provider: String,
    pub disabled_slot: bool,
    pub rect: Rect,
    pub hover: Rect,
    pub line1: Rect,
    pub more: Rect,
    pub name: Rect,
    pub plan: Option<Rect>,
    pub chip: Option<Rect>,
    pub usage: Option<Rect>,
    pub reset: Option<Rect>,
    pub bar: Option<Rect>,
    pub bar_fill: Option<Rect>,
    pub credits: Option<Rect>,
    pub reason: Option<Rect>,
    pub button: Option<ButtonBox>,
}
#[derive(Debug, Clone)]
pub struct SectionBox {
    pub provider: String,
    pub rect: Rect,
    pub label: Rect,
    pub rows: Vec<RowBox>,
    pub disabled_rows: Vec<RowBox>,
    pub dividers: Vec<Rect>,
    pub disclosure: Option<Rect>,
    pub hint: Option<Rect>,
    pub expanded: bool,
    pub empty_message: Option<Rect>,
    pub add_button: Option<ButtonBox>,
}
#[derive(Debug, Clone)]
pub struct EmptyBox {
    pub title: Rect,
    pub body: Rect,
    pub buttons: Vec<ButtonBox>,
    pub cli: Rect,
}
/// Pure old-SPEC9 boxes. Caller supplies title/status/countdown/button wording,
/// URL readiness, retry intent, and progress-ring graphics.
#[derive(Debug, Clone)]
pub struct LoginBox {
    pub provider: String,
    pub rect: Rect,
    pub title: Rect,
    pub status: Rect,
    pub ring: Rect,
    pub countdown: Rect,
    pub buttons: Vec<ButtonBox>,
}
#[derive(Debug, Clone)]
pub struct InfoLine {
    pub rect: Rect,
    pub text: String,
    pub emphasized: bool,
}
#[derive(Debug, Clone)]
pub struct MenuItemBox {
    pub rect: Rect,
    pub lead: Option<Rect>,
    pub label: Rect,
    pub subtitle: Option<Rect>,
    pub text: String,
    pub detail: Option<String>,
    pub checked: Option<bool>,
    pub target: Target,
    pub enabled: bool,
}
#[derive(Debug, Clone)]
pub struct PopupBox {
    pub rect: Rect,
    pub clip: Rect,
    pub content_height: f32,
    pub offset: f32,
    pub info: Vec<InfoLine>,
    pub items: Vec<MenuItemBox>,
    pub separators: Vec<Rect>,
}
#[derive(Debug, Clone)]
pub struct DialogBox {
    pub takeover_body: Option<String>,
    pub body_offset: f32,
    pub body_height: f32,
    pub scrim: Rect,
    pub card: Rect,
    /// Desired card height before the work-area cap clips the body viewport.
    pub content_height: f32,
    pub clip: Rect,
    pub title: Rect,
    pub body: Rect,
    pub divider: Rect,
    pub button_bar: Rect,
    pub buttons: Vec<ButtonBox>,
}
#[derive(Debug, Clone)]
pub struct Scene {
    pub rect: Rect,
    pub header: HeaderBox,
    pub list: Rect,
    pub footer: FooterBox,
    pub content_height: f32,
    pub offset: f32,
    pub max_offset: f32,
    pub sections: Vec<SectionBox>,
    pub empty: Option<EmptyBox>,
    pub login: Option<LoginBox>,
    pub popup: Option<PopupBox>,
    pub dialog: Option<DialogBox>,
    pub targets: Vec<HitBox>,
    pub core_ok: bool,
    /// Returned order includes newly appended IDs. Caller may retain it to ensure
    /// those IDs also stay stable on subsequent updates during this opening.
    pub resolved_order: BTreeMap<String, FrozenSection>,
}

impl Scene {
    pub fn build(view: &v2::View, p: Presentation, max_height: f32) -> Self {
        let is_login = matches!(p.overlay, Overlay::Login(_));
        let header = header(view, p.core_ok);
        let mut sections = Vec::new();
        let mut y = 0.;
        let mut resolved_order = BTreeMap::new();
        for section in &view.sections {
            let (main, disabled) = ordered(section, p.frozen.get(&section.provider));
            resolved_order.insert(
                section.provider.clone(),
                FrozenSection {
                    rows: main.iter().map(|r| r.raw.id.clone()).collect(),
                    disabled_rows: disabled.iter().map(|r| r.raw.id.clone()).collect(),
                },
            );
            let start = y;
            let label = r(0., y, WIDTH, y + 32.);
            y += 32.;
            let mut dividers = Vec::new();
            let mut rows = Vec::new();
            for (index, row) in main.iter().enumerate() {
                if index > 0 {
                    dividers.push(r(16., y, 344., y + 1.));
                    y += 1.;
                }
                let box_ = row_box(row, false, y, &p);
                y = box_.rect.bottom;
                rows.push(box_);
            }
            let empty_message = if main.is_empty() && disabled.is_empty() {
                Some(r(16., y + 14., 250., y + 30.))
            } else {
                None
            };
            let add_button = empty_message.map(|_| {
                button(
                    r(260., y + 10., 344., y + 34.),
                    "계정 추가",
                    Target::AddProvider(section.provider.clone()),
                    p.core_ok,
                )
            });
            if empty_message.is_some() {
                y += 44.;
            }
            let expanded = p
                .expanded
                .get(&section.provider)
                .copied()
                .unwrap_or(section.auto_expand_recommended);
            let disclosure = if disabled.is_empty() {
                None
            } else {
                dividers.push(r(16., y, 344., y + 1.));
                y += 1.;
                let rect = r(4., y, 356., y + 32.);
                y += 32.;
                Some(rect)
            };
            let hint = if disclosure.is_some() && expanded && section.auto_expand_recommended {
                let rect = r(16., y, 344., y + 20.);
                y += 20.;
                Some(rect)
            } else {
                None
            };
            let mut disabled_rows = Vec::new();
            if expanded {
                for row in disabled {
                    let box_ = row_box(row, true, y, &p);
                    y = box_.rect.bottom;
                    disabled_rows.push(box_);
                }
            }
            sections.push(SectionBox {
                provider: section.provider.clone(),
                rect: r(0., start, WIDTH, y),
                label,
                rows,
                disabled_rows,
                dividers,
                disclosure,
                hint,
                expanded,
                empty_message,
                add_button,
            });
        }
        // 41 header + 209 empty body + 48 footer = reference 298.
        let content_height = if is_login {
            176. + 8.
        } else if view.empty {
            209.
        } else {
            y + 8.
        };
        let natural = header.rect.bottom + content_height + FOOTER;
        let cap = finite(max_height, natural).max(0.);
        let mut height = natural.min(cap);
        let minimum_overlay = overlay_min_height(view, &p);
        height = height.max(minimum_overlay.min(cap));
        let footer_top = (height - FOOTER).max(0.);
        let list_top = header.rect.bottom.min(footer_top);
        let list = r(0., list_top, WIDTH, footer_top);
        let max_offset = (content_height - h(list)).max(0.);
        let offset = if is_login {
            0.
        } else {
            finite(p.offset, 0.).clamp(0., max_offset)
        };
        let dy = list.top - offset;
        for section in &mut sections {
            shift_section(section, dy);
        }
        let footer = footer(height, &p);
        let empty = (view.empty && !is_login).then(|| empty_box(dy, p.core_ok));
        let login = if let Overlay::Login(provider) = &p.overlay {
            sections.clear();
            Some(login_box(provider, list.top))
        } else {
            None
        };
        let rect = r(0., 0., WIDTH, height);
        let mut scene = Self {
            rect,
            header,
            list,
            footer,
            content_height,
            offset,
            max_offset,
            sections,
            empty,
            login,
            popup: None,
            dialog: None,
            targets: vec![],
            core_ok: p.core_ok,
            resolved_order,
        };
        scene.build_base_targets();
        match &p.overlay {
            Overlay::TakeoverCard {
                body,
                confirming,
                ready,
                replace,
            } => {
                let mut d = dialog_box(rect, "r6-takeover", false, &p);
                d.takeover_body = Some(body.clone());
                d.body_height = p.credits_body_lines as f32 * 20.;
                d.body_offset = p
                    .popup_offset
                    .clamp(0., (d.body_height - h(d.body)).max(0.));
                d.buttons[0].label = if *confirming {
                    if *replace {
                        "교체하고 가져오기"
                    } else {
                        "가져오기"
                    }
                } else {
                    "경고 확인"
                }
                .into();
                d.buttons[0].target = if *confirming {
                    Target::TakeoverCommit
                } else {
                    Target::TakeoverNext
                };
                d.buttons[0].enabled = p.core_ok && *ready;
                scene.targets = vec![HitBox {
                    rect,
                    target: Target::Blocked,
                    enabled: true,
                }];
                for b in &d.buttons {
                    push_target(
                        &mut scene.targets,
                        b.hit,
                        d.clip,
                        b.target.clone(),
                        b.enabled,
                    );
                }
                scene.dialog = Some(d);
            }
            Overlay::None | Overlay::Login(_) => {}
            Overlay::CreditsConfirm(id) | Overlay::DeleteConfirm(id)
                if find_row(view, id).is_some() =>
            {
                let delete = matches!(p.overlay, Overlay::DeleteConfirm(_));
                let dialog = dialog_box(rect, id, delete, &p);
                scene.targets = vec![HitBox {
                    rect,
                    target: Target::Blocked,
                    enabled: true,
                }];
                for b in &dialog.buttons {
                    push_target(
                        &mut scene.targets,
                        b.hit,
                        dialog.clip,
                        b.target.clone(),
                        b.enabled,
                    );
                }
                scene.dialog = Some(dialog);
            }
            Overlay::CreditsConfirm(_) | Overlay::DeleteConfirm(_) => {}
            _ => {
                if let Some(popup) = build_popup(view, &scene, &p) {
                    // Topmost-first is encoded as reverse traversal of this shared list.
                    scene.targets = vec![HitBox {
                        rect,
                        target: Target::DismissOverlay,
                        enabled: true,
                    }];
                    scene.targets.push(HitBox {
                        rect: popup.rect,
                        target: Target::Blocked,
                        enabled: true,
                    });
                    for item in &popup.items {
                        push_target(
                            &mut scene.targets,
                            item.rect,
                            popup.clip,
                            item.target.clone(),
                            item.enabled,
                        );
                    }
                    scene.popup = Some(popup);
                }
            }
        }
        scene
    }
    /// Reverse order gives buttons precedence over row backgrounds. Disabled
    /// topmost targets absorb the hit instead of falling through to another action.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<&Target> {
        self.targets
            .iter()
            .rev()
            .find(|t| contains(t.rect, x, y))
            .and_then(|t| t.enabled.then_some(&t.target))
    }
    pub fn row(&self, id: &str) -> Option<&RowBox> {
        self.sections
            .iter()
            .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
            .find(|r| r.id == id)
    }
    fn build_base_targets(&mut self) {
        if let Some(login) = &self.login {
            for b in &login.buttons {
                push_target(
                    &mut self.targets,
                    b.hit,
                    self.list,
                    b.target.clone(),
                    b.enabled,
                );
            }
        }
        for s in &self.sections {
            for row in s.rows.iter().chain(&s.disabled_rows) {
                push_target(
                    &mut self.targets,
                    row.hover,
                    self.list,
                    Target::Row(row.id.clone()),
                    self.core_ok,
                );
                if let Some(b) = &row.button {
                    push_target(
                        &mut self.targets,
                        b.hit,
                        self.list,
                        b.target.clone(),
                        b.enabled,
                    );
                }
            }
            if let Some(rect) = s.disclosure {
                push_target(
                    &mut self.targets,
                    rect,
                    self.list,
                    Target::Disclosure(s.provider.clone()),
                    true,
                );
            }
            if let Some(b) = &s.add_button {
                push_target(
                    &mut self.targets,
                    b.hit,
                    self.list,
                    b.target.clone(),
                    b.enabled,
                );
            }
        }
        if let Some(empty) = &self.empty {
            for b in &empty.buttons {
                push_target(
                    &mut self.targets,
                    b.hit,
                    self.list,
                    b.target.clone(),
                    b.enabled,
                );
            }
        }
        for b in &self.footer.buttons {
            push_target(
                &mut self.targets,
                b.hit,
                self.rect,
                b.target.clone(),
                b.enabled,
            );
        }
    }
}

fn header(view: &v2::View, core_ok: bool) -> HeaderBox {
    let mut y = 40.;
    let mut summaries = vec![];
    let mut banner = None;
    let mut note = None;
    if !core_ok {
        banner = Some(r(16., y, 344., y + 28.));
        y += 28.;
        note = Some(r(16., y, 344., y + 20.));
        y += 20.;
        y += 8.;
    } else if !view.empty {
        for _ in 0..2 {
            summaries.push(r(16., y, 344., y + 28.));
            y += 28.;
        }
        if view.note.is_some() {
            note = Some(r(32., y, 344., y + 20.));
            y += 20.;
        }
        y += 8.;
    }
    HeaderBox {
        rect: r(0., 0., WIDTH, y + 1.),
        title: r(16., 12., 344., 32.),
        summaries,
        banner,
        note,
        divider: r(0., y, WIDTH, y + 1.),
    }
}
fn footer(height: f32, p: &Presentation) -> FooterBox {
    let top = (height - FOOTER).max(0.);
    let by = top + 8.;
    let add_width = 36. + advance(p, "계정 추가", 13, false);
    let settings_width = 36. + advance(p, "설정", 13, false);
    let quit_width = 36. + advance(p, "종료", 13, false);
    let quit = r(352. - quit_width, by, 352., by + 32.);
    let settings = r(
        quit.left - 2. - settings_width,
        by,
        quit.left - 2.,
        by + 32.,
    );
    let add = r(8., by, 8. + add_width, by + 32.);
    FooterBox {
        rect: r(0., top, WIDTH, height),
        buttons: vec![
            button(
                add,
                "계정 추가",
                Target::Footer(FooterAction::Add),
                p.core_ok && !matches!(p.overlay, Overlay::Login(_)),
            ),
            button(
                settings,
                "설정",
                Target::Footer(FooterAction::Settings),
                true,
            ),
            button(quit, "종료", Target::Footer(FooterAction::Quit), true),
        ],
        stale_space: r(add.right + 4., by, settings.left - 4., by + 32.),
    }
}
fn login_box(provider: &str, y: f32) -> LoginBox {
    LoginBox {
        provider: provider.into(),
        rect: r(0., y, WIDTH, y + 176.),
        title: r(16., y + 16., 344., y + 36.),
        status: r(42., y + 48., 344., y + 88.),
        ring: r(16., y + 50., 32., y + 66.),
        countdown: r(42., y + 92., 344., y + 108.),
        // Both are geometry targets even while core is disconnected. Native
        // owns URL readiness (Copy/Retry) and an in-flight login's cancellation.
        buttons: vec![
            button(
                r(16., y + 124., 176., y + 156.),
                "주소 복사",
                Target::LoginCopy,
                true,
            ),
            button(
                r(184., y + 124., 344., y + 156.),
                "취소",
                Target::LoginCancel,
                true,
            ),
        ],
    }
}
fn empty_box(y: f32, enabled: bool) -> EmptyBox {
    EmptyBox {
        title: r(16., y + 20., 344., y + 40.),
        body: r(16., y + 48., 344., y + 88.),
        buttons: vec![
            button(
                r(16., y + 104., 176., y + 136.),
                "Claude 계정 추가",
                Target::AddProvider("claude".into()),
                enabled,
            ),
            button(
                r(184., y + 104., 344., y + 136.),
                "Codex 계정 추가",
                Target::AddProvider("codex".into()),
                enabled,
            ),
        ],
        cli: r(16., y + 152., 344., y + 184.),
    }
}
fn ordered<'a>(
    section: &'a v2::Section,
    frozen: Option<&FrozenSection>,
) -> (Vec<&'a v2::AccountRow>, Vec<&'a v2::AccountRow>) {
    let all: Vec<_> = section.rows.iter().chain(&section.disabled_rows).collect();
    let Some(frozen) = frozen else {
        return (
            section.rows.iter().collect(),
            section.disabled_rows.iter().collect(),
        );
    };
    let mut seen = BTreeSet::new();
    let mut main = Vec::new();
    let mut disabled = Vec::new();
    for (ids, out) in [
        (&frozen.rows, &mut main),
        (&frozen.disabled_rows, &mut disabled),
    ] {
        for id in ids {
            if seen.insert(id.clone())
                && let Some(row) = all.iter().find(|r| r.raw.id == *id)
            {
                out.push(*row);
            }
        }
    }
    for row in all {
        if seen.insert(row.raw.id.clone()) {
            if row.raw.user_state == "disabled" {
                disabled.push(row)
            } else {
                main.push(row)
            }
        }
    }
    (main, disabled)
}

pub fn row_height(row: &v2::AccountRow, disabled_slot: bool) -> f32 {
    if disabled_slot {
        return 44. + if row.reason.is_some() { 24. } else { 0. };
    }
    let action = row.action.as_ref().map(|a| a.kind);
    44. + if row.usage.is_some() { 30. } else { 0. }
        + if row.credits.is_some() {
            6. + if matches!(
                action,
                Some(v2::ActionKind::AllowCredits | v2::ActionKind::DisableCredits)
            ) {
                24.
            } else {
                16.
            }
        } else {
            0.
        }
        + if row.reason.is_some() {
            8. + if matches!(
                action,
                Some(v2::ActionKind::Relogin | v2::ActionKind::OpenLogs)
            ) {
                24.
            } else {
                16.
            }
        } else {
            0.
        }
}
fn row_box(row: &v2::AccountRow, disabled_slot: bool, y: f32, p: &Presentation) -> RowBox {
    let enabled = p.core_ok && !p.pending_ids.contains(&row.raw.id);
    let height = row_height(row, disabled_slot);
    let rect = r(0., y, WIDTH, y + height);
    let line1 = r(16., y + 10., 352., y + 34.);
    let more = r(324., line1.top, 352., line1.bottom);
    let mut right = 320.;
    let mut inline = None;
    if disabled_slot && row.raw.user_state == "disabled" {
        let width = 20. + advance(p, "켜기", 12, row.action.as_ref().is_some_and(|a| a.accent));
        inline = Some(button(
            r(right - width, line1.top, right, line1.bottom),
            "켜기",
            Target::Inline {
                id: row.raw.id.clone(),
                action: v2::ActionKind::Enable,
            },
            enabled,
        ));
        right -= width + 8.;
    }
    let chip = if disabled_slot {
        None
    } else {
        row.chip.as_ref().map(|c| {
            let width = 16. + advance(p, &c.text, 12, true);
            let rect = r(
                (right - width).max(16.),
                line1.top + 2.,
                right,
                line1.bottom - 2.,
            );
            right = rect.left - 8.;
            rect
        })
    };
    let plan_width = row.plan.as_deref().map(|s| advance(p, s, 12, false));
    let name_width = advance(p, &row.name, 13, !disabled_slot)
        .min((right - 16. - plan_width.map(|w| w + 6.).unwrap_or(0.)).max(0.));
    let name = r(16., line1.top + 3., 16. + name_width, line1.top + 21.);
    let plan = plan_width.map(|width| {
        r(
            name.right + 6.,
            line1.top + 4.,
            name.right + 6. + width,
            line1.top + 20.,
        )
    });
    let mut next = line1.bottom;
    let mut usage = None;
    let mut reset = None;
    let mut bar = None;
    let mut bar_fill = None;
    let mut credits = None;
    let mut reason = None;
    if !disabled_slot {
        if let Some(u) = &row.usage {
            next += 6.;
            let reset_width = u
                .binding
                .as_ref()
                .and_then(|w| w.reset_text.as_deref())
                .map(|s| advance(p, s, 12, false))
                .unwrap_or(0.);
            usage = Some(r(
                16.,
                next,
                if reset_width > 0. {
                    344. - reset_width - 8.
                } else {
                    344.
                },
                next + 16.,
            ));
            reset = (reset_width > 0.).then(|| r(344. - reset_width, next, 344., next + 16.));
            next += 20.;
            bar = Some(r(16., next, 344., next + 4.));
            bar_fill = u
                .binding
                .as_ref()
                .and_then(|w| w.remaining)
                .filter(|n| *n > 0)
                .map(|n| {
                    r(
                        16.,
                        next,
                        16. + (328. * f32::from(n) / 100.).round().max(4.),
                        next + 4.,
                    )
                });
            next += 4.;
        }
        let action = row.action.as_ref();
        if row.credits.is_some() {
            next += 6.;
            let has_button = action.is_some_and(|a| {
                matches!(
                    a.kind,
                    v2::ActionKind::AllowCredits | v2::ActionKind::DisableCredits
                )
            });
            let height = if has_button { 24. } else { 16. };
            let mut right = 344.;
            if has_button {
                let a = action.unwrap();
                let width = 20. + advance(p, a.label, 12, a.accent);
                inline = Some(button(
                    r(344. - width, next, 344., next + 24.),
                    a.label,
                    Target::Inline {
                        id: row.raw.id.clone(),
                        action: a.kind,
                    },
                    enabled,
                ));
                right -= width + 12.;
            }
            credits = Some(r(
                16.,
                next + (height - 16.) / 2.,
                right,
                next + (height + 16.) / 2.,
            ));
            next += height;
        }
        if row.reason.is_some() {
            next += 8.;
            let has_button = action.is_some_and(|a| {
                matches!(a.kind, v2::ActionKind::Relogin | v2::ActionKind::OpenLogs)
            });
            let height = if has_button { 24. } else { 16. };
            let mut right = 344.;
            if has_button {
                let a = action.unwrap();
                let width = 20. + advance(p, a.label, 12, a.accent);
                inline = Some(button(
                    r(344. - width, next, 344., next + 24.),
                    a.label,
                    Target::Inline {
                        id: row.raw.id.clone(),
                        action: a.kind,
                    },
                    enabled,
                ));
                right -= width + 12.;
            }
            reason = Some(r(
                16.,
                next + (height - 16.) / 2.,
                right,
                next + (height + 16.) / 2.,
            ));
        }
    }
    if disabled_slot && row.reason.is_some() {
        reason = Some(r(16., line1.bottom + 8., 344., line1.bottom + 24.));
    }
    RowBox {
        id: row.raw.id.clone(),
        provider: row.raw.provider.clone(),
        disabled_slot,
        rect,
        hover: r(4., y, 356., y + height),
        line1,
        more,
        name,
        plan,
        chip,
        usage,
        reset,
        bar,
        bar_fill,
        credits,
        reason,
        button: inline,
    }
}

#[derive(Clone)]
enum Part {
    Info(String, bool, f32),
    Gap(f32),
    Separator,
    Item(String, Option<String>, Option<bool>, bool, Target, bool),
}
fn find_row<'a>(view: &'a v2::View, id: &str) -> Option<&'a v2::AccountRow> {
    view.sections
        .iter()
        .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
        .find(|r| r.raw.id == id)
}
fn row_parts(row: &v2::AccountRow, core_ok: bool) -> Vec<Part> {
    let mut parts = vec![
        Part::Gap(8.),
        Part::Info(row.menu.full_name.clone(), true, 18.),
        Part::Info(row.menu.identity_line.clone(), false, 16.),
    ];
    if let Some(s) = &row.menu.refresh_warning {
        parts.push(Part::Info(s.clone(), false, 16.));
    }
    parts.push(Part::Gap(6.));
    for s in &row.menu.window_lines {
        parts.push(Part::Info(s.clone(), false, 16.));
    }
    if let Some(s) = &row.menu.model_cooling_line {
        parts.push(Part::Info(s.clone(), false, 16.));
    }
    if let Some(s) = &row.menu.credits_line {
        parts.push(Part::Info(s.clone(), false, 16.));
    }
    parts.push(Part::Gap(6.));
    parts.push(Part::Info(row.menu.sessions_line.clone(), false, 16.));
    parts.push(Part::Gap(8.));
    parts.push(Part::Separator);
    let mut groups: Vec<Vec<(MenuAction, &str, Option<bool>)>> = vec![vec![], vec![], vec![]];
    if row.menu.show_refresh_usage {
        groups[0].push((MenuAction::RefreshUsage, "사용량 새로고침", None));
    }
    if row.menu.show_relogin {
        groups[0].push((MenuAction::Relogin, "다시 로그인", None));
    }
    if row.menu.show_refresh_token {
        groups[0].push((MenuAction::RefreshToken, "토큰 지금 갱신", None));
    }
    if row.raw.provider == "codex" {
        groups[1].push((
            MenuAction::ToggleCredits,
            "유료 크레딧 사용 허용",
            Some(row.raw.allow_credits),
        ));
    }
    if matches!(row.raw.user_state.as_str(), "active" | "paused") {
        groups[1].push((
            MenuAction::ToggleDrain,
            "새 대화 받지 않기",
            Some(row.raw.drain),
        ));
    }
    if row.raw.user_state == "active" {
        groups[2].push((MenuAction::Pause, "일시정지", None));
    }
    if row.raw.user_state == "paused" {
        groups[2].push((MenuAction::Resume, "재개", None));
    }
    groups[2].push(if row.raw.user_state == "disabled" {
        (MenuAction::Enable, "켜기", None)
    } else {
        (MenuAction::Disable, "끄기", None)
    });
    groups[2].push((MenuAction::Delete, "계정 삭제…", None));
    let mut first = true;
    for group in groups.into_iter().filter(|g| !g.is_empty()) {
        if !first {
            parts.push(Part::Separator);
        }
        first = false;
        for (action, label, checked) in group {
            parts.push(Part::Item(
                label.into(),
                None,
                checked,
                true,
                Target::Menu {
                    id: Some(row.raw.id.clone()),
                    action,
                },
                core_ok,
            ));
        }
    }
    parts
}
fn menu_parts(view: &v2::View, p: &Presentation) -> Option<(f32, Vec<Part>)> {
    let item = |label: &str, action: MenuAction, enabled: bool| {
        Part::Item(
            label.into(),
            None,
            None,
            true,
            Target::Menu { id: None, action },
            enabled,
        )
    };
    Some(match &p.overlay {
        Overlay::RowMenu(id) => (
            264.,
            row_parts(
                find_row(view, id)?,
                p.core_ok && !p.pending_ids.contains(id),
            ),
        ),
        Overlay::Settings => (
            240.,
            vec![
                item("대시보드 열기", MenuAction::OpenDashboard, true),
                item("설정 파일 열기", MenuAction::OpenConfig, true),
                item("로그 폴더 열기", MenuAction::OpenLogs, true),
                Part::Separator,
                Part::Gap(8.),
                Part::Info(
                    format!(
                        "quota-baton {} · {}",
                        view.raw.version,
                        if p.core_ok {
                            "코어 실행 중"
                        } else {
                            "코어 연결 끊김"
                        }
                    ),
                    false,
                    16.,
                ),
                Part::Info("설정 파일: config.toml".into(), false, 16.),
                Part::Gap(8.),
            ],
        ),
        Overlay::Add => (
            300.,
            vec![
                item("Claude 계정 로그인", MenuAction::AddClaude, p.core_ok),
                item("Codex 계정 로그인", MenuAction::AddCodex, p.core_ok),
                item("다른 앱에서 계정 가져오기", MenuAction::Takeover, p.core_ok),
            ],
        ),
        Overlay::TakeoverProviders => (
            300.,
            vec![
                item(
                    "Codex 폴더에서 가져오기",
                    MenuAction::TakeoverCodex,
                    p.core_ok,
                ),
                item(
                    "Claude 폴더에서 가져오기",
                    MenuAction::TakeoverClaude,
                    p.core_ok,
                ),
            ],
        ),
        Overlay::Quit => (
            300.,
            vec![
                Part::Item(
                    "트레이만 닫기".into(),
                    Some(QUIT_DETAILS[0].into()),
                    None,
                    false,
                    Target::Menu {
                        id: None,
                        action: MenuAction::QuitTray,
                    },
                    true,
                ),
                Part::Item(
                    "트레이와 프록시 종료".into(),
                    Some(QUIT_DETAILS[1].into()),
                    None,
                    false,
                    Target::Menu {
                        id: None,
                        action: MenuAction::QuitAll,
                    },
                    true,
                ),
            ],
        ),
        _ => return None,
    })
}
fn subtitle_height(detail: &str, p: &Presentation) -> f32 {
    16. * p
        .subtitle_lines
        .get(detail)
        .copied()
        .unwrap_or(1)
        .clamp(1, 8) as f32
}
fn parts_height(parts: &[Part], p: &Presentation) -> f32 {
    8. + parts
        .iter()
        .map(|part| match part {
            Part::Info(_, _, height) | Part::Gap(height) => *height,
            Part::Separator => 9.,
            Part::Item(_, subtitle, _, _, _, _) => {
                if let Some(detail) = subtitle {
                    36. + subtitle_height(detail, p)
                } else {
                    32.
                }
            }
        })
        .sum::<f32>()
}
fn overlay_min_height(view: &v2::View, p: &Presentation) -> f32 {
    match &p.overlay {
        Overlay::TakeoverCard { .. } => 149. + p.credits_body_lines as f32 * 20.,
        Overlay::DeleteConfirm(id) if find_row(view, id).is_some() => 193. + 16.,
        Overlay::CreditsConfirm(id) if find_row(view, id).is_some() => {
            credits_dialog_height(p) + 16.
        }
        _ => menu_parts(view, p)
            .map(|(_, parts)| parts_height(&parts, p) + 8. + FOOTER + 12.)
            .unwrap_or(0.),
    }
}
fn build_popup(view: &v2::View, scene: &Scene, p: &Presentation) -> Option<PopupBox> {
    let (width, parts) = menu_parts(view, p)?;
    let content_height = parts_height(&parts, p);
    let available = (scene.footer.rect.top - 8.).max(0.);
    if available <= 0. {
        return None;
    }
    let height = content_height.min(available);
    let (right, top) = match &p.overlay {
        Overlay::RowMenu(id) => {
            let row = scene.row(id)?;
            let below = row.more.bottom + 4.;
            let above = row.more.top - 4. - height;
            let top = if below + height <= scene.footer.rect.top {
                below
            } else {
                above
            };
            (
                row.more.right,
                top.max(8.).min((scene.footer.rect.top - height).max(8.)),
            )
        }
        Overlay::Settings | Overlay::Quit | Overlay::Add | Overlay::TakeoverProviders => {
            let kind = match p.overlay {
                Overlay::Settings => FooterAction::Settings,
                Overlay::Quit => FooterAction::Quit,
                _ => FooterAction::Add,
            };
            let anchor = scene
                .footer
                .buttons
                .iter()
                .find(|b| b.target == Target::Footer(kind.clone()))?;
            let right = if kind == FooterAction::Add {
                (anchor.visual.left + width).min(352.)
            } else {
                anchor.visual.right.min(352.)
            };
            (right, (anchor.visual.top - 4. - height).max(8.))
        }
        _ => return None,
    };
    let rect = r(
        (right - width).max(8.),
        top,
        right.max(width + 8.),
        top + height,
    );
    let clip = r(
        rect.left + 1.,
        rect.top + 4.,
        rect.right - 1.,
        rect.bottom - 4.,
    );
    let offset = finite(p.popup_offset, 0.).clamp(0., (content_height - height).max(0.));
    let mut y = rect.top + 4. - offset;
    let mut info = vec![];
    let mut items = vec![];
    let mut separators = vec![];
    for part in parts {
        match part {
            Part::Info(text, emphasized, height) => {
                info.push(InfoLine {
                    rect: r(rect.left + 17., y, rect.right - 17., y + height),
                    text,
                    emphasized,
                });
                y += height;
            }
            Part::Gap(height) => y += height,
            Part::Separator => {
                separators.push(r(rect.left + 1., y + 4., rect.right - 1., y + 5.));
                y += 9.;
            }
            Part::Item(text, detail, checked, lead, target, enabled) => {
                let height = detail
                    .as_deref()
                    .map_or(32., |detail| 36. + subtitle_height(detail, p));
                let item = r(rect.left + 5., y, rect.right - 5., y + height);
                let lead_rect = lead.then(|| {
                    r(
                        item.left + 12.,
                        y + (height - 16.) / 2.,
                        item.left + 28.,
                        y + (height + 16.) / 2.,
                    )
                });
                let text_left = item.left + if lead { 40. } else { 12. };
                let ty = y + if detail.is_some() { 8. } else { 7. };
                items.push(MenuItemBox {
                    rect: item,
                    lead: lead_rect,
                    label: r(text_left, ty, item.right - 12., ty + 18.),
                    subtitle: detail.as_ref().map(|detail| {
                        r(
                            text_left,
                            ty + 20.,
                            item.right - 12.,
                            ty + 20. + subtitle_height(detail, p),
                        )
                    }),
                    text,
                    detail,
                    checked,
                    target,
                    enabled,
                });
                y += height;
            }
        }
    }
    Some(PopupBox {
        rect,
        clip,
        content_height,
        offset,
        info,
        items,
        separators,
    })
}
fn credits_dialog_height(p: &Presentation) -> f32 {
    20. + 20. + 8. + 20. * p.credits_body_lines.clamp(1, 12) as f32 + 20. + 1. + 64.
}
fn dialog_box(scene: Rect, id: &str, delete: bool, p: &Presentation) -> DialogBox {
    let content_height = if matches!(p.overlay, Overlay::TakeoverCard { .. }) {
        // Body starts at +48, ends 20 above the 65-DIP button bar.
        133. + p.credits_body_lines as f32 * 20.
    } else if delete {
        193.
    } else {
        credits_dialog_height(p)
    };
    let height = content_height.min((h(scene) - 16.).max(0.));
    let top = ((h(scene) - height) / 2.).round().max(8.);
    let card = r(16., top, 344., top + height);
    let bar = r(16., card.bottom - 65., 344., card.bottom - 1.);
    let by = bar.top + 16.;
    let confirm = if delete {
        Target::ConfirmDelete(id.into())
    } else {
        Target::ConfirmCredits(id.into())
    };
    DialogBox {
        takeover_body: None,
        body_offset: 0.,
        body_height: 0.,
        scrim: scene,
        card,
        content_height,
        clip: intersection(card, scene).unwrap_or(r(0., 0., 0., 0.)),
        title: r(36., top + 20., 324., top + 40.),
        body: r(36., top + 48., 324., (bar.top - 20.).max(top + 48.)),
        divider: r(16., bar.top, 344., bar.top + 1.),
        button_bar: bar,
        buttons: vec![
            button(
                r(36., by, 176., by + 32.),
                if delete { "삭제" } else { "크레딧 사용" },
                confirm,
                p.core_ok && !p.pending_ids.contains(id),
            ),
            button(
                r(184., by, 324., by + 32.),
                "취소",
                Target::CancelDialog,
                true,
            ),
        ],
    }
}

fn button(visual: Rect, label: &str, target: Target, enabled: bool) -> ButtonBox {
    let center = (visual.top + visual.bottom) / 2.;
    ButtonBox {
        visual,
        hit: r(visual.left, center - 16., visual.right, center + 16.),
        label: label.into(),
        target,
        enabled,
    }
}
fn r(left: f32, top: f32, right: f32, bottom: f32) -> Rect {
    Rect {
        left,
        top,
        right,
        bottom,
    }
}
pub fn h(rect: Rect) -> f32 {
    (rect.bottom - rect.top).max(0.)
}

#[cfg(test)]
mod r6_layout_tests {
    use super::*;
    #[test]
    fn r6_body_fits_exactly_or_scrolls_to_its_last_line() {
        for lines in [1, 12, 60] {
            for cap in [240., 480., 800.] {
                let scene = Scene::build(
                    &view(),
                    Presentation {
                        overlay: Overlay::TakeoverCard {
                            body: "synthetic".into(),
                            confirming: true,
                            ready: true,
                            replace: false,
                        },
                        credits_body_lines: lines,
                        popup_offset: 100_000.,
                        ..Default::default()
                    },
                    cap,
                );
                let d = scene.dialog.unwrap();
                assert_eq!(h(d.card), (133. + lines as f32 * 20.).min(cap - 16.));
                assert!(h(d.body) <= d.body_height);
                assert!((d.body_offset + h(d.body) - d.body_height).abs() < 0.1);
                if 133. + lines as f32 * 20. <= cap - 16. {
                    assert_eq!(d.body_offset, 0.);
                    assert_eq!(h(d.body), d.body_height);
                } else {
                    assert!(d.body_offset > 0.);
                }
            }
        }
    }
    fn view() -> v2::View {
        v2::project("{\"accounts\":[]}", std::time::SystemTime::UNIX_EPOCH).unwrap()
    }
    #[test]
    fn takeover_is_third_and_whole_item_clickable() {
        let scene = Scene::build(
            &view(),
            Presentation {
                overlay: Overlay::Add,
                ..Default::default()
            },
            600.,
        );
        let popup = scene.popup.as_ref().unwrap();
        assert_eq!(popup.items.len(), 3);
        let item = &popup.items[2];
        assert_eq!(item.text, "다른 앱에서 계정 가져오기");
        for x in [item.rect.left + 1., item.rect.right - 1.] {
            for y in [item.rect.top + 1., item.rect.bottom - 1.] {
                assert_eq!(scene.hit_test(x, y), Some(&item.target));
            }
        }
        let providers = Scene::build(
            &view(),
            Presentation {
                overlay: Overlay::TakeoverProviders,
                ..Default::default()
            },
            600.,
        );
        let popup = providers.popup.as_ref().unwrap();
        assert_eq!(
            popup
                .items
                .iter()
                .map(|i| i.text.as_str())
                .collect::<Vec<_>>(),
            ["Codex 폴더에서 가져오기", "Claude 폴더에서 가져오기"]
        );
        for item in &popup.items {
            for scale in [1., 1.5, 2.] {
                for x in [item.rect.left + 1., item.rect.right - 1.] {
                    for y in [item.rect.top + 1., item.rect.bottom - 1.] {
                        // Native mouse coordinates are physical; geometry is DIP.
                        let px = (x * scale).round();
                        let py = (y * scale).round();
                        assert_eq!(
                            providers.hit_test(px / scale, py / scale),
                            Some(&item.target)
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn long_inspect_and_replace_confirmation_keep_buttons_contained_and_underlay_blocked() {
        for cap in [240., 480., 800.] {
            for confirming in [false, true] {
                for ready in [false, true] {
                    let scene = Scene::build(
                        &view(),
                        Presentation {
                            overlay: Overlay::TakeoverCard {
                                body: "합성 긴 경로\n".repeat(60),
                                confirming,
                                ready,
                                replace: true,
                            },
                            credits_body_lines: 60,
                            popup_offset: 100_000.,
                            ..Default::default()
                        },
                        cap,
                    );
                    let d = scene.dialog.as_ref().unwrap();
                    assert!(contains(scene.rect, d.card.left, d.card.top));
                    assert!(d.card.bottom <= scene.rect.bottom);
                    assert!(d.body.bottom <= d.button_bar.top);
                    assert!(d.body_offset > 0.);
                    assert!(d.body_offset + h(d.body) <= d.body_height + 0.1);
                    assert_eq!(
                        d.buttons[0].label,
                        if confirming {
                            "교체하고 가져오기"
                        } else {
                            "경고 확인"
                        }
                    );
                    assert_eq!(d.buttons[0].enabled, ready);
                    for b in &d.buttons {
                        assert!(b.hit.left >= d.card.left && b.hit.right <= d.card.right);
                        assert!(b.hit.top >= d.card.top && b.hit.bottom <= d.card.bottom);
                        let hit = scene.hit_test(
                            (b.hit.left + b.hit.right) / 2.,
                            (b.hit.top + b.hit.bottom) / 2.,
                        );
                        assert_eq!(hit, if b.enabled { Some(&b.target) } else { None });
                    }
                    assert_eq!(
                        scene.hit_test(1., scene.rect.bottom - 1.),
                        Some(&Target::Blocked)
                    );
                }
            }
        }
    }
}
pub fn contains(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}
pub fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let rect = r(
        a.left.max(b.left),
        a.top.max(b.top),
        a.right.min(b.right),
        a.bottom.min(b.bottom),
    );
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}
fn finite(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}
fn push_target(out: &mut Vec<HitBox>, rect: Rect, clip: Rect, target: Target, enabled: bool) {
    if let Some(rect) = intersection(rect, clip) {
        out.push(HitBox {
            rect,
            target,
            enabled,
        });
    }
}
fn shift(rect: &mut Rect, dy: f32) {
    rect.top += dy;
    rect.bottom += dy;
}
fn shift_button(b: &mut ButtonBox, dy: f32) {
    shift(&mut b.visual, dy);
    shift(&mut b.hit, dy);
}
fn shift_row(row: &mut RowBox, dy: f32) {
    for rect in [
        &mut row.rect,
        &mut row.hover,
        &mut row.line1,
        &mut row.more,
        &mut row.name,
    ] {
        shift(rect, dy);
    }
    for rect in [
        &mut row.plan,
        &mut row.chip,
        &mut row.usage,
        &mut row.reset,
        &mut row.bar,
        &mut row.bar_fill,
        &mut row.credits,
        &mut row.reason,
    ]
    .into_iter()
    .flatten()
    {
        shift(rect, dy);
    }
    if let Some(b) = &mut row.button {
        shift_button(b, dy);
    }
}
fn shift_section(section: &mut SectionBox, dy: f32) {
    shift(&mut section.rect, dy);
    shift(&mut section.label, dy);
    for rect in &mut section.dividers {
        shift(rect, dy);
    }
    for rect in [
        &mut section.disclosure,
        &mut section.hint,
        &mut section.empty_message,
    ]
    .into_iter()
    .flatten()
    {
        shift(rect, dy);
    }
    for row in section.rows.iter_mut().chain(&mut section.disabled_rows) {
        shift_row(row, dy);
    }
    if let Some(b) = &mut section.add_button {
        shift_button(b, dy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::time::{Duration, UNIX_EPOCH};

    fn account(id: &str) -> Value {
        json!({"id":id,"provider":"codex","email":format!("{id}@example.invalid"),"user_state":"active","health":{"kind":"ready"},"pool_eligible":true,"source":{"kind":"oauth"}})
    }
    fn view(accounts: Vec<Value>, eligible: u64) -> v2::View {
        v2::project(
            &json!({"version":"0.1.0","accounts":accounts,"pools":{"codex":{"eligible":eligible}}})
                .to_string(),
            UNIX_EPOCH + Duration::from_secs(1_792_324_500),
        )
        .unwrap()
    }
    fn ready_view() -> v2::View {
        view(vec![account("a")], 1)
    }
    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.left >= outer.left
            && inner.top >= outer.top
            && inner.right <= outer.right
            && inner.bottom <= outer.bottom
    }
    fn center(rect: Rect) -> (f32, f32) {
        ((rect.left + rect.right) / 2., (rect.top + rect.bottom) / 2.)
    }
    fn relogin() -> Value {
        let mut a = account("a");
        a["health"] = json!({"kind":"needs_relogin"});
        a["pool_eligible"] = json!(false);
        a
    }
    fn disabled(id: &str) -> Value {
        let mut a = account(id);
        a["user_state"] = json!("disabled");
        a["pool_eligible"] = json!(false);
        a
    }
    fn credits() -> Value {
        let mut a = account("a");
        a["quota_credit_releasable"] = json!(true);
        a["health"] = json!({"kind":"cooling","reason":"quota","scope":"account","until":"2099-01-01T00:00:00Z"});
        a["credits"] = json!({"has_credits":true,"balance":19395.23});
        a["pool_eligible"] = json!(false);
        a
    }

    #[test]
    fn header_and_empty_reference_dimensions() {
        let empty = view(vec![], 0);
        let scene = Scene::build(&empty, Presentation::default(), 1000.);
        assert_eq!(scene.rect, r(0., 0., 360., 298.));
        assert_eq!(h(scene.header.rect), 41.);
        assert_eq!(h(scene.footer.rect), 48.);
        let empty = scene.empty.as_ref().unwrap();
        assert_eq!(
            empty.buttons[0].visual.right - empty.buttons[0].visual.left,
            160.
        );
        assert_eq!(
            empty.buttons[1].visual.left - empty.buttons[0].visual.right,
            8.
        );
        for b in &empty.buttons {
            assert!(inside(b.visual, scene.list));
            let (x, y) = center(b.visual);
            assert_eq!(scene.hit_test(x, y), Some(&b.target));
        }
        let scene = Scene::build(&ready_view(), Presentation::default(), 1000.);
        assert_eq!(h(scene.header.rect), 105.);
        assert_eq!(scene.header.summaries.len(), 2);
        let scene = Scene::build(&view(vec![relogin()], 0), Presentation::default(), 1000.);
        assert_eq!(h(scene.header.rect), 125.);
        let scene = Scene::build(
            &ready_view(),
            Presentation {
                core_ok: false,
                ..Presentation::default()
            },
            1000.,
        );
        assert_eq!(h(scene.header.rect), 97.);
        assert!(scene.header.banner.is_some());
        assert!(scene.header.summaries.is_empty());
    }
    #[test]
    fn row_formula_and_visual_coordinates() {
        let v = ready_view();
        let scene = Scene::build(&v, Presentation::default(), 1000.);
        let row = scene.row("a").unwrap();
        assert_eq!(h(row.rect), 74.);
        assert_eq!(row.hover.left, 4.);
        assert_eq!(row.hover.right, 356.);
        assert_eq!(row.more.left, 324.);
        assert_eq!(row.more.right, 352.);
        assert_eq!(h(row.line1), 24.);
        assert_eq!(row.line1.top - row.rect.top, 10.);
        assert_eq!(h(row.usage.unwrap()), 16.);
        assert_eq!(h(row.bar.unwrap()), 4.);
        assert_eq!(row.bar.unwrap().top - row.usage.unwrap().bottom, 4.);
        let v = view(vec![relogin()], 0);
        let scene = Scene::build(&v, Presentation::default(), 1000.);
        let row = scene.row("a").unwrap();
        assert_eq!(h(row.rect), 76.);
        assert!(row.usage.is_none());
        assert_eq!(h(row.reason.unwrap()), 16.);
        assert_eq!(h(row.button.as_ref().unwrap().visual), 24.);
        let v = view(vec![credits()], 0);
        let scene = Scene::build(&v, Presentation::default(), 1000.);
        let row = scene.row("a").unwrap();
        assert_eq!(h(row.rect), 104.);
        assert_eq!(h(row.button.as_ref().unwrap().visual), 24.);
        assert!(row.credits.is_some());
    }
    #[test]
    fn bar_fill_is_remaining_minimum_and_no_zero_fill() {
        for (used, width) in [(62., 125.), (99., 4.), (100., 0.)] {
            let mut a = account("a");
            a["windows"] = json!([{"name":"7d","used_pct":used,"reset_at":"2099-01-01T00:00:00Z"}]);
            let v = view(vec![a], 1);
            let scene = Scene::build(&v, Presentation::default(), 1000.);
            let row = scene.row("a").unwrap();
            assert_eq!(row.bar_fill.map(|r| r.right - r.left).unwrap_or(0.), width);
            if let Some(fill) = row.bar_fill {
                assert!(inside(fill, row.bar.unwrap()));
            }
        }
    }
    #[test]
    fn whole_row_and_button_share_exact_targets_with_button_precedence() {
        let v = view(vec![relogin()], 0);
        let scene = Scene::build(&v, Presentation::default(), 1000.);
        let row = scene.row("a").unwrap();
        let b = row.button.as_ref().unwrap();
        let (x, y) = center(b.visual);
        assert_eq!(scene.hit_test(x, y), Some(&b.target));
        assert_eq!(h(b.hit), 32.);
        assert_eq!(b.hit.left, b.visual.left);
        assert_eq!(b.hit.right, b.visual.right);
        assert!(
            scene
                .targets
                .iter()
                .any(|t| t.rect == b.hit && t.target == b.target)
        );
        let (x, y) = center(row.more);
        assert_eq!(scene.hit_test(x, y), Some(&Target::Row("a".into())));
        assert_eq!(
            scene.hit_test(5., row.rect.top + 2.),
            Some(&Target::Row("a".into()))
        );
        assert_eq!(scene.hit_test(3., row.rect.top + 2.), None);
    }
    #[test]
    fn cap_scroll_only_list_and_clip_hitboxes() {
        let v = view((0..12).map(|i| account(&format!("a{i:02}"))).collect(), 12);
        let a = Scene::build(&v, Presentation::default(), 400.);
        let b = Scene::build(
            &v,
            Presentation {
                offset: 87.,
                ..Presentation::default()
            },
            400.,
        );
        assert_eq!(a.rect, b.rect);
        assert_eq!(a.header.rect, b.header.rect);
        assert_eq!(a.footer.rect, b.footer.rect);
        assert_eq!(a.list, b.list);
        assert_eq!(a.rect.bottom, 400.);
        assert_eq!(
            a.row("a00").unwrap().rect.top - b.row("a00").unwrap().rect.top,
            87.
        );
        for target in &b.targets {
            if matches!(
                target.target,
                Target::Row(_)
                    | Target::Inline { .. }
                    | Target::Disclosure(_)
                    | Target::AddProvider(_)
            ) {
                assert!(inside(target.rect, b.list));
            }
        }
        assert_eq!(b.hit_test(8., b.list.top - 1.), None);
        let last = Scene::build(
            &v,
            Presentation {
                offset: f32::MAX,
                ..Presentation::default()
            },
            400.,
        );
        assert_eq!(last.offset, last.max_offset);
        assert!(last.row("a11").unwrap().rect.bottom <= last.list.bottom);
    }
    #[test]
    fn section_disclosure_hint_disabled44_and_manual_override() {
        let v = view(vec![disabled("off")], 0);
        let scene = Scene::build(&v, Presentation::default(), 1000.);
        let s = &scene.sections[1];
        assert_eq!(h(s.label), 32.);
        assert_eq!(h(s.disclosure.unwrap()), 32.);
        assert_eq!(h(s.hint.unwrap()), 20.);
        assert_eq!(h(scene.row("off").unwrap().rect), 44.);
        let mut p = Presentation::default();
        p.expanded.insert("codex".into(), false);
        let closed = Scene::build(&v, p, 1000.);
        let s = &closed.sections[1];
        assert!(!s.expanded);
        assert!(s.disabled_rows.is_empty());
        assert!(s.hint.is_none());
        assert!(s.disclosure.is_some());
        let (x, y) = center(s.disclosure.unwrap());
        assert_eq!(
            closed.hit_test(x, y),
            Some(&Target::Disclosure("codex".into()))
        );
    }
    #[test]
    fn frozen_order_and_new_ids_append_without_membership_movement() {
        let opened = view(vec![account("b"), account("a"), disabled("off")], 2);
        let mut p = Presentation {
            frozen: capture_order(&opened),
            ..Default::default()
        };
        p.expanded.insert("codex".into(), true);
        let mut a = account("a");
        a["label"] = json!("z");
        let changed = view(vec![a, account("b"), account("off"), account("00-new")], 4);
        let scene = Scene::build(&changed, p.clone(), 1000.);
        let section = &scene.sections[1];
        assert_eq!(
            section
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "00-new"]
        );
        let off = scene.row("off").unwrap();
        assert!(off.disabled_slot);
        assert_eq!(h(off.rect), 44.);
        assert!(off.button.is_none());
        assert!(off.usage.is_none());
        p.frozen = scene.resolved_order.clone();
        let mut new = account("00-new");
        new["label"] = json!("zz");
        let changed = view(
            vec![
                account("b"),
                account("a"),
                account("off"),
                new,
                account("01-next"),
            ],
            5,
        );
        let scene = Scene::build(&changed, p, 1000.);
        assert_eq!(
            scene.sections[1]
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "00-new", "01-next"]
        );
    }
    #[test]
    fn popup_contains_all_items_and_grows_scene_within_cap() {
        let v = ready_view();
        let base = Scene::build(&v, Presentation::default(), 1000.);
        let scene = Scene::build(
            &v,
            Presentation {
                overlay: Overlay::RowMenu("a".into()),
                ..Presentation::default()
            },
            1000.,
        );
        let popup = scene.popup.as_ref().unwrap();
        assert_eq!(popup.rect.right - popup.rect.left, 264.);
        assert!(scene.rect.bottom > base.rect.bottom);
        assert!(inside(popup.rect, scene.rect));
        assert!(popup.rect.top >= 8.);
        assert!(popup.rect.bottom <= scene.footer.rect.top);
        for item in &popup.items {
            assert!(inside(item.rect, popup.clip));
            assert!(inside(item.label, item.rect));
            if let Some(lead) = item.lead {
                assert!(inside(lead, item.rect));
                let (x, y) = center(lead);
                assert_eq!(scene.hit_test(x, y), Some(&item.target));
            }
            let (x, y) = center(item.label);
            assert_eq!(scene.hit_test(x, y), Some(&item.target));
            assert_eq!(item.rect.left, popup.rect.left + 5.);
            assert_eq!(item.rect.right, popup.rect.right - 5.);
        }
        assert!(popup.info.iter().all(|line| inside(line.rect, popup.clip)));
    }
    #[test]
    fn popup_cap_scroll_clip_and_no_clickthrough() {
        let v = ready_view();
        let scene = Scene::build(
            &v,
            Presentation {
                overlay: Overlay::RowMenu("a".into()),
                ..Presentation::default()
            },
            300.,
        );
        let popup = scene.popup.as_ref().unwrap();
        assert_eq!(scene.rect.bottom, 300.);
        assert!(inside(popup.rect, scene.rect));
        assert!(popup.content_height > h(popup.rect));
        for t in &scene.targets {
            if matches!(t.target, Target::Menu { .. }) {
                assert!(inside(t.rect, popup.clip));
            }
        }
        assert_eq!(scene.hit_test(1., 1.), Some(&Target::DismissOverlay));
        assert_eq!(
            scene.hit_test(popup.rect.left + 2., popup.rect.top + 2.),
            Some(&Target::Blocked)
        );
        let scene = Scene::build(
            &v,
            Presentation {
                overlay: Overlay::RowMenu("a".into()),
                popup_offset: f32::MAX,
                ..Presentation::default()
            },
            300.,
        );
        let popup = scene.popup.as_ref().unwrap();
        let last = popup.items.last().unwrap();
        assert!(inside(last.rect, popup.clip));
        let (x, y) = center(last.rect);
        assert_eq!(scene.hit_test(x, y), Some(&last.target));
    }
    #[test]
    fn failed_enable_has_a_contained_reason_line_without_losing_the_button() {
        let mut v = view(vec![disabled("off")], 0);
        let row = v
            .sections
            .iter_mut()
            .flat_map(|section| &mut section.disabled_rows)
            .next()
            .unwrap();
        row.reason = Some("실패했습니다. 다시 시도하세요.".into());
        let scene = Scene::build(&v, Presentation::default(), 1000.);
        let row = scene.row("off").unwrap();
        assert_eq!(h(row.rect), 68.);
        assert!(inside(row.reason.unwrap(), row.rect));
        assert!(inside(row.button.as_ref().unwrap().visual, row.rect));
        assert!(row.reason.unwrap().top > row.line1.bottom);
    }
    #[test]
    fn quit_subtitles_grow_from_measured_keep_all_lines() {
        let v = ready_view();
        let mut p = Presentation {
            overlay: Overlay::Quit,
            ..Default::default()
        };
        p.subtitle_lines.insert(QUIT_DETAILS[0].into(), 2);
        let scene = Scene::build(&v, p, 1000.);
        let popup = scene.popup.unwrap();
        assert_eq!(h(popup.items[0].rect), 68.);
        assert_eq!(h(popup.items[0].subtitle.unwrap()), 32.);
        assert_eq!(h(popup.items[1].rect), 52.);
        for item in popup.items {
            assert!(inside(item.subtitle.unwrap(), item.rect));
        }
    }
    #[test]
    fn footer_popups_widths_item_sizes_and_anchors() {
        let v = ready_view();
        for (overlay, width, count, height) in [
            (Overlay::Settings, 240., 3, 32.),
            (Overlay::Quit, 300., 2, 52.),
            (Overlay::Add, 300., 3, 32.),
        ] {
            let scene = Scene::build(
                &v,
                Presentation {
                    overlay,
                    ..Presentation::default()
                },
                1000.,
            );
            let popup = scene.popup.as_ref().unwrap();
            assert_eq!(popup.rect.right - popup.rect.left, width);
            assert_eq!(popup.items.len(), count);
            for item in &popup.items {
                assert_eq!(h(item.rect), height);
                assert!(inside(item.rect, popup.clip));
                let (x, y) = center(item.rect);
                assert_eq!(scene.hit_test(x, y), Some(&item.target));
            }
            assert!(popup.rect.bottom <= scene.footer.buttons[0].visual.top - 4.);
            assert!(popup.rect.left >= 8.);
            assert!(popup.rect.right <= 352.);
        }
    }
    #[test]
    fn dialogs_cover_footer_and_only_buttons_activate() {
        let v = ready_view();
        for (overlay, target) in [
            (
                Overlay::DeleteConfirm("a".into()),
                Target::ConfirmDelete("a".into()),
            ),
            (
                Overlay::CreditsConfirm("a".into()),
                Target::ConfirmCredits("a".into()),
            ),
        ] {
            let scene = Scene::build(
                &v,
                Presentation {
                    overlay,
                    ..Presentation::default()
                },
                1000.,
            );
            let dialog = scene.dialog.as_ref().unwrap();
            assert_eq!(h(dialog.card), 193.);
            assert_eq!(dialog.card.left, 16.);
            assert_eq!(dialog.card.right, 344.);
            assert!(inside(dialog.card, scene.rect));
            assert_eq!(
                scene.hit_test(20., scene.footer.rect.top + 12.),
                Some(&Target::Blocked)
            );
            let (x, y) = center(dialog.buttons[0].visual);
            assert_eq!(scene.hit_test(x, y), Some(&target));
            let (x, y) = center(dialog.buttons[1].visual);
            assert_eq!(scene.hit_test(x, y), Some(&Target::CancelDialog));
            for b in &dialog.buttons {
                assert_eq!(h(b.visual), 32.);
                assert_eq!(b.visual.right - b.visual.left, 140.);
                assert!(inside(b.visual, dialog.card));
            }
        }
    }
    #[test]
    fn disconnected_and_pending_actions_absorb_hits() {
        let v = view(vec![relogin()], 0);
        let scene = Scene::build(
            &v,
            Presentation {
                core_ok: false,
                ..Presentation::default()
            },
            1000.,
        );
        let row = scene.row("a").unwrap();
        let b = row.button.as_ref().unwrap();
        let (x, y) = center(b.visual);
        assert_eq!(scene.hit_test(x, y), None);
        let settings = &scene.footer.buttons[1];
        let (x, y) = center(settings.visual);
        assert_eq!(
            scene.hit_test(x, y),
            Some(&Target::Footer(FooterAction::Settings))
        );
        let mut p = Presentation::default();
        p.pending_ids.insert("a".into());
        let scene = Scene::build(&v, p, 1000.);
        let row = scene.row("a").unwrap();
        let b = row.button.as_ref().unwrap();
        let (x, y) = center(b.visual);
        assert_eq!(scene.hit_test(x, y), None);
        assert_eq!(
            scene.hit_test(8., row.rect.top + 1.),
            Some(&Target::Row("a".into()))
        );
    }
    #[test]
    fn measured_width_is_shared_by_visual_and_hit() {
        let v = view(vec![relogin()], 0);
        let mut p = Presentation::default();
        p.text_widths
            .insert(text_key("다시 로그인", 12, true), 101.);
        let scene = Scene::build(&v, p, 1000.);
        let b = scene.row("a").unwrap().button.as_ref().unwrap();
        assert_eq!(b.visual.right - b.visual.left, 121.);
        assert_eq!(b.hit.left, b.visual.left);
        assert_eq!(b.hit.right, b.visual.right);
    }
    #[test]
    fn stale_overlay_id_has_no_targets_and_list_has_no_extra_more_target() {
        let v = ready_view();
        let scene = Scene::build(
            &v,
            Presentation {
                overlay: Overlay::RowMenu("missing".into()),
                ..Presentation::default()
            },
            1000.,
        );
        assert!(scene.popup.is_none());
        assert!(
            scene
                .targets
                .iter()
                .all(|t| !matches!(t.target, Target::Menu { .. } | Target::DismissOverlay))
        );
        assert_eq!(
            scene
                .targets
                .iter()
                .filter(|t| t.target == Target::Row("a".into()))
                .count(),
            1
        );
    }

    #[test]
    fn capped_dialog_keeps_card_and_buttons_inside_scene() {
        let v = ready_view();
        let scene = Scene::build(
            &v,
            Presentation {
                overlay: Overlay::CreditsConfirm("a".into()),
                credits_body_lines: 12,
                ..Presentation::default()
            },
            280.,
        );
        let dialog = scene.dialog.as_ref().unwrap();
        assert!(inside(dialog.card, scene.rect));
        assert!(dialog.content_height > h(dialog.card));
        for b in &dialog.buttons {
            assert!(inside(b.visual, dialog.card));
            let (x, y) = center(b.visual);
            assert_eq!(scene.hit_test(x, y), Some(&b.target));
        }
    }

    #[test]
    fn login_replaces_list_with_spec9_boxes_and_fixed_footer() {
        let v = ready_view();
        let scene = Scene::build(
            &v,
            Presentation {
                overlay: Overlay::Login("codex".into()),
                offset: 500.,
                ..Presentation::default()
            },
            1000.,
        );
        let login = scene.login.as_ref().unwrap();
        assert_eq!(login.provider, "codex");
        assert_eq!(scene.content_height, 184.);
        assert_eq!(
            scene.rect.bottom,
            scene.header.rect.bottom + 176. + 8. + 48.
        );
        assert_eq!(scene.offset, 0.);
        assert_eq!(scene.list.bottom, scene.footer.rect.top);
        assert_eq!(h(scene.footer.rect), 48.);
        assert_eq!(h(login.rect), 176.);
        assert!(inside(login.rect, scene.list));
        assert_eq!(login.title.top - login.rect.top, 16.);
        assert_eq!(h(login.title), 20.);
        assert_eq!(login.status.top - login.title.top, 32.);
        assert_eq!(h(login.status), 40.);
        assert_eq!(login.status.left, 42.);
        assert_eq!(
            login.ring,
            r(16., login.status.top + 2., 32., login.status.top + 18.)
        );
        assert_eq!(login.countdown.top - login.status.bottom, 4.);
        assert_eq!(h(login.countdown), 16.);
        assert_eq!(login.buttons[0].visual.top - login.countdown.bottom, 16.);
        assert_eq!(
            login.buttons[1].visual.left - login.buttons[0].visual.right,
            8.
        );
        assert_eq!(login.rect.bottom - login.buttons[0].visual.bottom, 20.);
        assert!(scene.sections.is_empty() && scene.empty.is_none());
        assert!(!scene.targets.iter().any(|t| matches!(
            t.target,
            Target::Row(_) | Target::Inline { .. } | Target::Disclosure(_) | Target::AddProvider(_)
        )));
        for b in &login.buttons {
            assert_eq!(b.visual.right - b.visual.left, 160.);
            assert_eq!(h(b.hit), 32.);
            assert!(inside(b.visual, login.rect));
            let (x, y) = center(b.visual);
            assert_eq!(scene.hit_test(x, y), Some(&b.target));
        }
        let add = &scene.footer.buttons[0];
        let (x, y) = center(add.visual);
        assert!(!add.enabled);
        assert_eq!(scene.hit_test(x, y), None);
    }

    #[test]
    fn disconnected_login_keeps_owned_cancel_and_empty_state_hidden() {
        let scene = Scene::build(
            &view(vec![], 0),
            Presentation {
                overlay: Overlay::Login("claude".into()),
                core_ok: false,
                ..Presentation::default()
            },
            1000.,
        );
        let login = scene.login.as_ref().unwrap();
        assert!(scene.header.banner.is_some());
        assert!(scene.empty.is_none());
        let cancel = &login.buttons[1];
        let (x, y) = center(cancel.visual);
        assert!(cancel.enabled);
        assert_eq!(scene.hit_test(x, y), Some(&Target::LoginCancel));
        let settings = &scene.footer.buttons[1];
        let (x, y) = center(settings.visual);
        assert_eq!(
            scene.hit_test(x, y),
            Some(&Target::Footer(FooterAction::Settings))
        );
        assert_eq!(scene.list.bottom, scene.footer.rect.top);
        assert!(inside(login.rect, scene.list));
    }
}
