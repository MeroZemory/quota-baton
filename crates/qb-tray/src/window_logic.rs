//! Pure native coordinator rules. No HWND, disk, clock reads or serialization.
use crate::dashboard_persistence::Geometry;

/// Shared WM_MOUSEWHEEL router. Modal rejection precedes region selection.
pub fn wheel_scroll(
    state: &mut qb_tray::state::DashboardState,
    scene: &qb_tray::geometry::Scene,
    point: [f32; 2],
    delta: f32,
) -> bool {
    if state.popup.is_some() || scene.modal {
        return false;
    }
    let Some(region) = scene
        .content
        .iter()
        .rev()
        .find(|region| qb_tray::geometry::contains(region.viewport, point[0], point[1]))
    else {
        return false;
    };
    state.scroll(region.region, region.offset + delta, region.max_offset);
    true
}

/// Navigate the full laid-out table, including rows outside its viewport.
/// The caller must invalidate/repaint before publishing new accessibility bounds.
pub fn navigate_session(
    state: &mut qb_tray::state::DashboardState,
    scene: &qb_tray::geometry::Scene,
    focus: Option<&qb_tray::geometry::Target>,
    up: bool,
    connected: bool,
) -> Option<(qb_tray::geometry::Target, Vec<qb_tray::state::Command>)> {
    use qb_tray::{
        geometry::Target,
        state::{Command, ScrollRegion},
    };
    if !connected || scene.modal || scene.session_rows.is_empty() {
        return None;
    }
    let at = match focus {
        Some(Target::Session(id)) => scene.session_rows.iter().position(|row| &row.id == id),
        _ => None,
    };
    let next = if up {
        at.unwrap_or(1).saturating_sub(1)
    } else {
        at.map_or(0, |n| (n + 1).min(scene.session_rows.len() - 1))
    };
    let id = &scene.session_rows[next].id;
    let offset = scene.reveal_session(id)?;
    let region = scene
        .content
        .iter()
        .find(|r| r.region == ScrollRegion::Table)?;
    state.scroll(ScrollRegion::Table, offset, region.max_offset);
    let commands = if state
        .selected
        .as_ref()
        .is_some_and(|selected| selected != id)
    {
        state
            .select(Some(id.clone()))
            .map(Command::Detail)
            .into_iter()
            .collect()
    } else {
        vec![]
    };
    Some((Target::Session(id.clone()), commands))
}

pub fn dismiss_popup(state: &mut qb_tray::state::DashboardState) -> bool {
    state.popup.take().is_some()
}

/// Modal input never falls through to editor, paging or session navigation.
pub fn popup_key(
    scene: &qb_tray::geometry::Scene,
    focus: &mut Option<qb_tray::geometry::Target>,
    key: usize,
    shift: bool,
) -> Option<qb_tray::geometry::Target> {
    use qb_tray::geometry::Target;
    if !scene.modal {
        return None;
    }
    if key == 0x1b {
        return Some(Target::DismissPopup);
    }
    let order = scene.focus_order();
    if order.is_empty() {
        return None;
    }
    let at = focus
        .as_ref()
        .and_then(|t| order.iter().position(|v| *v == t));
    match key {
        0x09 | 0x26 | 0x28 => {
            let backwards = key == 0x26 || (key == 0x09 && shift);
            let next = match (at, backwards) {
                (Some(0), true) | (None, true) => order.len() - 1,
                (Some(i), true) => i - 1,
                (Some(i), false) => (i + 1) % order.len(),
                (None, false) => 0,
            };
            *focus = Some(order[next].clone());
            None
        }
        0x0d | 0x20 => at.map(|i| order[i].clone()),
        _ => None,
    }
}

pub fn text_input_allowed(
    state: &qb_tray::state::DashboardState,
    editing: bool,
    connected: bool,
) -> bool {
    connected && state.popup.is_none() && editing
}

pub fn set_search_value(
    state: &mut qb_tray::state::DashboardState,
    target: &qb_tray::geometry::Target,
    value: String,
    connected: bool,
) -> bool {
    use qb_tray::geometry::Target;
    if !connected || state.popup.is_some() {
        return false;
    }
    match target {
        Target::Search if state.search != value => {
            state.search = value;
            state.table_offset = 0.;
            state.table_freeze.frozen = false;
        }
        Target::EventSearch if state.event_search != value => {
            state.event_search = value;
            state.events_offset = 0.;
        }
        _ => return false,
    }
    true
}

/// Enter opens a session; pointer activation continues to use state.activate's toggle.
pub fn open_session(
    state: &mut qb_tray::state::DashboardState,
    id: &str,
) -> Vec<qb_tray::state::Command> {
    if state.selected.as_deref() == Some(id) {
        return vec![];
    }
    state
        .select(Some(id.to_owned()))
        .map(qb_tray::state::Command::Detail)
        .into_iter()
        .collect()
}

/// Reopened UI preferences are the query intent, not coordinator defaults.
/// Regenerate the selection ticket so an earlier detail reply cannot satisfy it.
pub fn startup_commands(
    state: &mut qb_tray::state::DashboardState,
) -> Vec<qb_tray::state::Command> {
    use qb_tray::state::{Command, Tab};
    let mut commands = vec![
        Command::Sessions(state.range),
        Command::Activity(state.event_days),
    ];
    if state.tab == Tab::Usage {
        commands.push(Command::Usage);
    }
    if let Some(id) = state.selected.clone()
        && let Some(ticket) = state.select(Some(id))
    {
        commands.push(Command::Detail(ticket));
    }
    commands
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub owner: u64,
    pub epoch: usize,
    pub pixels: [u32; 2],
    pub dpi: u32,
}

pub struct Lifecycle {
    pub owner: u64,
    pub frame: Option<Stamp>,
    alive: bool,
    load_pending: bool,
    user_placed: bool,
    geometry: Option<Geometry>,
    dirty_at: Option<u64>,
    sequence: u64,
}
impl Lifecycle {
    pub fn new(owner: u64) -> Self {
        Self {
            owner,
            frame: None,
            alive: true,
            load_pending: true,
            user_placed: false,
            geometry: None,
            dirty_at: None,
            sequence: 0,
        }
    }
    pub fn invalidate(&mut self) {
        self.frame = None;
    }
    pub fn painted(&mut self, stamp: Stamp) -> bool {
        if !self.alive || stamp.owner != self.owner || stamp.epoch == 0 {
            return false;
        }
        self.frame = Some(stamp);
        true
    }
    pub fn accepts(&self, owner: u64, epoch: usize, pixels: [u32; 2], dpi: u32) -> bool {
        self.alive
            && self.frame
                == Some(Stamp {
                    owner,
                    epoch,
                    pixels,
                    dpi,
                })
    }
    pub fn load(&mut self, owner: u64, saved: Option<Geometry>) -> Option<Geometry> {
        if !self.alive || owner != self.owner || !self.load_pending {
            return None;
        }
        self.load_pending = false;
        if self.user_placed {
            return None;
        }
        saved.filter(|g| g.valid())
    }
    pub fn placed(&mut self, geometry: Geometry, now: u64, user: bool) {
        if !self.alive || !geometry.valid() {
            return;
        }
        self.user_placed |= user;
        if self.geometry != Some(geometry) {
            self.geometry = Some(geometry);
            self.dirty_at = Some(now);
        }
    }
    pub fn dirty(&mut self, now: u64, force: bool) -> Option<(u64, Geometry)> {
        // A slow worker load must not lose the previous saved rectangle to the
        // provisional default, even if the user closes before the read returns.
        if self.load_pending && !self.user_placed {
            return None;
        }
        let at = self.dirty_at?;
        if !force && now.saturating_sub(at) < 500 {
            return None;
        }
        self.sequence = self.sequence.checked_add(1)?;
        self.dirty_at = None;
        Some((self.sequence, self.geometry?))
    }
    pub fn close(&mut self) {
        self.alive = false;
        self.frame = None;
        self.load_pending = false;
    }
}

pub fn default_rect(work: [i32; 4], dpi: u32) -> [i32; 4] {
    let saved = Geometry {
        left: work[0],
        top: work[1],
        width: 1320,
        height: 960,
        dpi: 96,
        maximized: false,
    };
    let mut r = saved.restore(work, dpi).unwrap_or(work);
    let width = r[2] - r[0];
    let height = r[3] - r[1];
    r[0] = work[0] + ((work[2] - work[0]) - width) / 2;
    r[1] = work[1] + ((work[3] - work[1]) - height) / 2;
    [r[0], r[1], r[0] + width, r[1] + height]
}

/// Bound an interactive outer rectangle to its selected monitor. Unlike a
/// default/restore this allows the full work area, with a softened minimum for
/// small displays. All arithmetic is widened before accepting OS coordinates.
pub fn fit_rect(rect: [i32; 4], work: [i32; 4], dpi: u32) -> [i32; 4] {
    if work[2] <= work[0] || work[3] <= work[1] {
        return [0, 0, 1024, 640];
    }
    let ww = (i64::from(work[2]) - i64::from(work[0])).max(1);
    let wh = (i64::from(work[3]) - i64::from(work[1])).max(1);
    let dpi = i64::from(dpi.clamp(48, 768));
    let width = (i64::from(rect[2]) - i64::from(rect[0])).clamp((1024 * dpi / 96).min(ww), ww);
    let height = (i64::from(rect[3]) - i64::from(rect[1])).clamp((640 * dpi / 96).min(wh), wh);
    let left = i64::from(rect[0]).clamp(i64::from(work[0]), i64::from(work[2]) - width);
    let top = i64::from(rect[1]).clamp(i64::from(work[1]), i64::from(work[3]) - height);
    [
        left as i32,
        top as i32,
        (left + width) as i32,
        (top + height) as i32,
    ]
}

/// Character-index editor. UTF-16 arrival is decoded separately; no byte slicing.
#[derive(Default)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    pub anchor: usize,
    pub composition: String,
    high: Option<u16>,
}
impl Editor {
    pub fn reset(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = text.chars().count();
        self.anchor = self.cursor;
        self.composition.clear();
        self.high = None;
    }
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.chars().count();
    }
    fn range(&self) -> std::ops::Range<usize> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor)
    }
    pub fn insert(&mut self, value: &str) {
        let mut chars: Vec<char> = self.text.chars().collect();
        let r = self.range();
        let clean: Vec<char> = value
            .chars()
            .filter(|c| !c.is_control())
            .take(256)
            .collect();
        let count = clean
            .len()
            .min(256usize.saturating_sub(chars.len() - (r.end - r.start)));
        chars.splice(r.clone(), clean.into_iter().take(count));
        self.cursor = r.start + count;
        self.anchor = self.cursor;
        self.text = chars.into_iter().collect();
        self.composition.clear();
    }
    pub fn utf16(&mut self, unit: u16) {
        if (0xd800..=0xdbff).contains(&unit) {
            self.high = Some(unit);
            return;
        }
        let value = if (0xdc00..=0xdfff).contains(&unit) {
            let Some(high) = self.high.take() else {
                return;
            };
            char::from_u32(
                0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00),
            )
        } else {
            self.high = None;
            char::from_u32(u32::from(unit))
        };
        if let Some(c) = value {
            self.insert(&c.to_string());
        }
    }
    pub fn move_to(&mut self, position: usize, extend: bool) {
        self.cursor = position.min(self.text.chars().count());
        if !extend {
            self.anchor = self.cursor;
        }
    }
    pub fn erase(&mut self, backward: bool) {
        if self.cursor == self.anchor {
            if backward {
                self.anchor = self.cursor.saturating_sub(1);
            } else {
                self.anchor = (self.cursor + 1).min(self.text.chars().count());
            }
        }
        self.insert("");
    }
    pub fn displayed(&self) -> String {
        let mut chars: Vec<char> = self.text.chars().collect();
        chars.splice(
            self.range(),
            self.composition
                .chars()
                .filter(|c| !c.is_control())
                .take(64),
        );
        chars.into_iter().collect()
    }
}

#[cfg(test)]
#[path = "window_logic_tests.rs"]
mod tests;
