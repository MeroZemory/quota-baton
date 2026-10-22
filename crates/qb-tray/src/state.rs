//! Pure dashboard preferences and navigation. No native calls or I/O.
use crate::{
    dashboard_model::{EventRow, SessionDetail},
    geometry::Target,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tab {
    #[default]
    Overview,
    Events,
    Usage,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProviderFilter {
    #[default]
    All,
    Claude,
    Codex,
}
impl ProviderFilter {
    pub fn matches(self, p: &str) -> bool {
        match self {
            Self::All => true,
            Self::Claude => p == "claude",
            Self::Codex => p == "codex",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Range {
    Active,
    #[default]
    Hour,
    Today,
    All,
}
impl Range {
    pub fn empty_title(self) -> &'static str {
        match self {
            Self::Active => crate::i18n::static_text("현재 활성 세션이 없습니다"),
            Self::Hour => {
                crate::i18n::static_text("최근 1시간 동안 quota-baton을 거친 세션이 없습니다")
            }
            Self::Today => crate::i18n::static_text("오늘 quota-baton을 거친 세션이 없습니다"),
            Self::All => crate::i18n::static_text("연결된 세션이 없습니다"),
        }
    }
    pub const ALL: [Self; 4] = [Self::Active, Self::Hour, Self::Today, Self::All];
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => crate::i18n::static_text("활성만"),
            Self::Hour => crate::i18n::static_text("최근 1시간"),
            Self::Today => crate::i18n::static_text("오늘"),
            Self::All => crate::i18n::static_text("연결된 전체"),
        }
    }
    pub fn query(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Hour => "hour",
            Self::Today => "today",
            Self::All => "all",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum SortKey {
    Status,
    Session,
    Client,
    Account,
    Model,
    Start,
    #[default]
    LastActivity,
    Requests,
    Memo,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventFilter {
    #[default]
    Notable,
    Tasks,
    Moves,
    Pacing,
    Credits,
    Tokens,
    All,
}
impl EventFilter {
    pub const ALL: [Self; 7] = [
        Self::Notable,
        Self::Tasks,
        Self::Moves,
        Self::Pacing,
        Self::Credits,
        Self::Tokens,
        Self::All,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Notable => crate::i18n::static_text("중요한 일"),
            Self::Tasks => crate::i18n::static_text("할 일"),
            Self::Moves => crate::i18n::static_text("한도·계정 전환"),
            Self::Pacing => crate::i18n::static_text("속도 조절"),
            Self::Credits => crate::i18n::static_text("크레딧"),
            Self::Tokens => crate::i18n::static_text("토큰·로그인"),
            Self::All => crate::i18n::static_text("모든 일"),
        }
    }
    pub fn matches(self, e: &EventRow) -> bool {
        match self {
            Self::All => true,
            Self::Notable => e.notable,
            Self::Tokens => matches!(e.kind.as_str(), "token_refresh" | "linked_reload" | "login"),
            _ => e.category == self,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScrollRegion {
    Table,
    Side,
    Events,
    Usage,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Popup {
    Range,
    EventDays,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailTicket {
    pub id: String,
    pub generation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Sessions(Range),
    Detail(DetailTicket),
    Activity(u8),
    Usage,
    OpenTrayAccount(String),
    OpenTrayAdd,
    FocusSearch,
    FocusEventSearch,
}
#[derive(Clone, Debug, Default)]
pub struct Freeze {
    pub frozen: bool,
    pub order: Vec<String>,
    last_activity: u64,
}
impl Freeze {
    pub fn apply<T>(&mut self, rows: &mut [T], id: impl Fn(&T) -> String) {
        if self.frozen {
            let ranks: BTreeMap<_, _> = self
                .order
                .iter()
                .enumerate()
                .map(|(i, id)| (id.clone(), i))
                .collect();
            rows.sort_by_key(|r| ranks.get(&id(r)).copied().unwrap_or(usize::MAX));
        }
        self.order = rows.iter().map(id).collect();
    }
    fn pointer(&mut self, inside: bool, now: u64) {
        self.frozen = inside;
        self.last_activity = now;
    }
    fn idle(&mut self, now: u64) {
        if now.saturating_sub(self.last_activity) >= 5000 {
            self.frozen = false;
        }
    }
}
#[derive(Clone, Debug)]
pub struct DashboardState {
    pub popup: Option<Popup>,
    pub tab: Tab,
    pub provider: ProviderFilter,
    pub range: Range,
    pub grouped: bool,
    pub search: String,
    pub account_filter: Option<String>,
    pub sort: SortKey,
    pub descending: bool,
    pub collapsed: BTreeSet<String>,
    pub selected: Option<String>,
    pub table_offset: f32,
    pub side_offset: f32,
    pub events_offset: f32,
    pub usage_offset: f32,
    pub event_filter: EventFilter,
    pub event_days: u8,
    pub event_search: String,
    pub detail: Option<SessionDetail>,
    pub detail_expired: bool,
    pub table_freeze: Freeze,
    pub recent_freeze: Freeze,
    pub group_orders: BTreeMap<String, Freeze>,
    pub group_headers: Freeze,
    pub(crate) generation: u64,
}
impl Default for DashboardState {
    fn default() -> Self {
        Self {
            popup: None,
            tab: Default::default(),
            provider: Default::default(),
            range: Default::default(),
            grouped: true,
            search: String::new(),
            account_filter: None,
            sort: Default::default(),
            descending: true,
            collapsed: Default::default(),
            selected: None,
            table_offset: 0.,
            side_offset: 0.,
            events_offset: 0.,
            usage_offset: 0.,
            event_filter: Default::default(),
            event_days: 2,
            event_search: String::new(),
            detail: None,
            detail_expired: false,
            table_freeze: Default::default(),
            recent_freeze: Default::default(),
            group_orders: Default::default(),
            group_headers: Default::default(),
            generation: 0,
        }
    }
}
impl DashboardState {
    pub fn select(&mut self, id: Option<String>) -> Option<DetailTicket> {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("dashboard detail generation exhausted");
        self.selected = id;
        self.detail = None;
        self.detail_expired = false;
        self.side_offset = 0.;
        self.selected.as_ref().map(|id| DetailTicket {
            id: id.clone(),
            generation: self.generation,
        })
    }
    pub fn publish_detail(&mut self, ticket: &DetailTicket, detail: SessionDetail) -> bool {
        if self.generation != ticket.generation
            || self.selected.as_deref() != Some(&ticket.id)
            || detail.session.id != ticket.id
        {
            return false;
        }
        self.detail = Some(detail);
        self.detail_expired = false;
        true
    }
    pub fn detail_removed(&mut self, id: &str) {
        if self.selected.as_deref() == Some(id) {
            self.detail_expired = true;
        }
    }
    pub fn pointer(&mut self, region: ScrollRegion, inside: bool, now: u64) {
        match region {
            ScrollRegion::Table => self.table_freeze.pointer(inside, now),
            ScrollRegion::Side => self.recent_freeze.pointer(inside, now),
            _ => {}
        }
    }
    pub fn pointer_activity(&mut self, region: ScrollRegion, now: u64) {
        self.pointer(region, true, now);
    }
    pub fn release_idle(&mut self, now: u64) {
        self.table_freeze.idle(now);
        self.recent_freeze.idle(now);
    }
    pub fn scroll(&mut self, region: ScrollRegion, offset: f32, max: f32) {
        let value = if offset.is_finite() {
            offset.clamp(0., max.max(0.))
        } else {
            0.
        };
        match region {
            ScrollRegion::Table => self.table_offset = value,
            ScrollRegion::Side => self.side_offset = value,
            ScrollRegion::Events => self.events_offset = value,
            ScrollRegion::Usage => self.usage_offset = value,
        }
    }
    pub fn activate(&mut self, target: &Target) -> Vec<Command> {
        match target {
            Target::Tab(tab) => {
                self.popup = None;
                self.tab = *tab;
                match tab {
                    Tab::Usage => vec![Command::Usage],
                    Tab::Events => vec![Command::Activity(self.event_days)],
                    Tab::Overview => vec![],
                }
            }
            Target::Provider(provider) => {
                self.provider = *provider;
                self.account_filter = None;
                self.table_offset = 0.;
                self.table_freeze.frozen = false;
                vec![]
            }
            Target::Range(range) => {
                self.popup = None;
                self.range = *range;
                self.table_offset = 0.;
                self.table_freeze.frozen = false;
                vec![Command::Sessions(*range)]
            }
            Target::ToggleGrouping => {
                self.grouped = !self.grouped;
                self.table_offset = 0.;
                self.table_freeze.frozen = false;
                vec![]
            }
            Target::Search => vec![Command::FocusSearch],
            Target::EventSearch => vec![Command::FocusEventSearch],
            Target::Sort(key) => {
                if !self.grouped {
                    if self.sort == *key {
                        self.descending = !self.descending;
                    } else {
                        self.sort = *key;
                        self.descending = false;
                    }
                    self.table_freeze.frozen = false;
                }
                vec![]
            }
            Target::Group(id) => {
                if !self.collapsed.remove(id) {
                    self.collapsed.insert(id.clone());
                }
                vec![]
            }
            Target::Session(id) => {
                let selected = if self.selected.as_deref() == Some(id) {
                    None
                } else {
                    Some(id.clone())
                };
                self.select(selected)
                    .map(Command::Detail)
                    .into_iter()
                    .collect()
            }
            Target::SessionLink(id) => {
                self.tab = Tab::Overview;
                self.provider = ProviderFilter::All;
                self.range = Range::All;
                self.search.clear();
                self.account_filter = None;
                let mut commands = vec![Command::Sessions(Range::All)];
                if let Some(t) = self.select(Some(id.clone())) {
                    commands.push(Command::Detail(t));
                }
                commands
            }
            Target::AccountLink(id) => {
                self.tab = Tab::Overview;
                self.provider = ProviderFilter::All;
                self.search.clear();
                self.account_filter = Some(id.clone());
                self.table_offset = 0.;
                vec![Command::Sessions(self.range)]
            }
            Target::CloseDetail => {
                self.select(None);
                vec![]
            }
            Target::DetailSurface => vec![],
            Target::EventFilter(filter) => {
                self.event_filter = *filter;
                self.events_offset = 0.;
                vec![]
            }
            Target::EventDays(days) => {
                self.popup = None;
                self.event_days = (*days).clamp(1, 7);
                self.events_offset = 0.;
                vec![Command::Activity(self.event_days)]
            }
            Target::AllEvents => {
                self.tab = Tab::Events;
                self.event_filter = EventFilter::Notable;
                self.event_search.clear();
                self.events_offset = 0.;
                vec![Command::Activity(self.event_days)]
            }
            Target::OpenTrayAccount(id) => vec![Command::OpenTrayAccount(id.clone())],
            Target::OpenTrayAdd => vec![Command::OpenTrayAdd],
            Target::Today => {
                self.range = Range::Today;
                self.table_offset = 0.;
                vec![Command::Sessions(Range::Today)]
            }
            Target::RangeMenu => {
                self.popup = Some(Popup::Range);
                vec![]
            }
            Target::EventDaysMenu => {
                self.popup = Some(Popup::EventDays);
                vec![]
            }
            Target::DismissPopup => {
                self.popup = None;
                vec![]
            }
            Target::Chart(_) => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn r7_detail_tickets_never_publish_over_new_selection_or_close() {
        let mut state = DashboardState::default();
        let a = state.select(Some("aaaaaaaaaaaa".into())).unwrap();
        let b = state.select(Some("bbbbbbbbbbbb".into())).unwrap();
        let mut d = SessionDetail::default();
        d.session.id = a.id.clone();
        assert!(!state.publish_detail(&a, d.clone()));
        d.session.id = b.id.clone();
        assert!(state.publish_detail(&b, d.clone()));
        state.select(None);
        assert!(!state.publish_detail(&b, d));
    }
    #[test]
    fn r7_freeze_updates_values_appends_new_ids_then_releases_after_idle() {
        let mut f = Freeze::default();
        let mut rows = vec![("a", 1), ("b", 2)];
        f.apply(&mut rows, |r| r.0.into());
        f.pointer(true, 100);
        let mut rows = vec![("b", 20), ("c", 3), ("a", 10)];
        f.apply(&mut rows, |r| r.0.into());
        assert_eq!(rows, vec![("a", 10), ("b", 20), ("c", 3)]);
        f.idle(5099);
        assert!(f.frozen);
        f.idle(5100);
        assert!(!f.frozen);
    }
    #[test]
    fn r7_read_only_commands_and_default_filters() {
        let mut s = DashboardState::default();
        assert_eq!(s.range, Range::Hour);
        assert!(s.grouped);
        assert_eq!(s.event_days, 2);
        assert!(matches!(
            s.activate(&Target::OpenTrayAdd).as_slice(),
            [Command::OpenTrayAdd]
        ));
        assert_eq!(
            s.activate(&Target::Today),
            vec![Command::Sessions(Range::Today)]
        );
    }
}
