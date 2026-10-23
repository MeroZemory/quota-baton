//! Pure dashboard preferences and navigation. No native calls or I/O.
use crate::{
    dashboard_model::{EventRow, Session, SessionDetail, SessionRouting, valid_routing_action_id},
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
    SetSessionUrgent { id: String, urgent: bool },
}
/// One serial mutation lane; acknowledgements do not assert that an account moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrgencyUpdate {
    Pending { id: String },
    Finished { id: String, failed: bool },
}
#[derive(Clone, Debug)]
pub(crate) struct SessionAction {
    routing: SessionRouting,
    waiting: bool,
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
    pub urgency_update: Option<UrgencyUpdate>,
    pub(crate) session_actions: BTreeMap<String, SessionAction>,
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
            urgency_update: None,
            session_actions: BTreeMap::new(),
        }
    }
}
impl DashboardState {
    pub fn apply_urgency_update(&mut self, update: UrgencyUpdate) {
        let id = match &update {
            UrgencyUpdate::Pending { id } | UrgencyUpdate::Finished { id, .. } => id,
        };
        if !valid_routing_action_id(id) {
            return;
        }
        match &update {
            UrgencyUpdate::Pending { .. } => {
                if matches!(&self.urgency_update, Some(UrgencyUpdate::Pending { id: current }) if current != id)
                {
                    return;
                }
                self.urgency_update = Some(update);
            }
            UrgencyUpdate::Finished { failed, .. } => {
                if !matches!(&self.urgency_update, Some(UrgencyUpdate::Pending { id: current }) if current == id)
                {
                    return;
                }
                self.urgency_update = if *failed { Some(update) } else { None };
            }
        }
    }
    pub(crate) fn refresh_session_actions(&mut self, sessions: &[Session], available: bool) {
        self.session_actions.clear();
        // Bounded like the policy store. Duplicated display IDs are ambiguous.
        if !available || sessions.len() > 20_000 {
            return;
        }
        let mut seen = BTreeSet::new();
        for session in sessions {
            if !seen.insert(session.id.as_str()) {
                self.session_actions.remove(&session.id);
                continue;
            }
            if session.id.len() != 12
                || !session
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                continue;
            }
            if let Some(routing) = session.routing.as_ref().filter(|r| r.usable()) {
                self.session_actions.insert(
                    session.id.clone(),
                    SessionAction {
                        routing: routing.clone(),
                        waiting: session.quota_wait_known
                            && session.quota_wait.as_ref().is_some_and(|w| w.waiting > 0),
                    },
                );
            }
        }
    }
    pub(crate) fn session_urgency_enabled(&self, row_id: &str) -> bool {
        !matches!(self.urgency_update, Some(UrgencyUpdate::Pending { .. }))
            && self.session_actions.contains_key(row_id)
    }
    fn session_urgent_command(&mut self, row_id: &str, move_now: bool) -> Vec<Command> {
        if !self.session_urgency_enabled(row_id) {
            return vec![];
        }
        let Some(action) = self.session_actions.get(row_id) else {
            return vec![];
        };
        if move_now
            && (!action.waiting || action.routing.mode == "strict" || action.routing.urgent())
        {
            return vec![];
        }
        let urgent = move_now || !action.routing.urgent();
        let id = action.routing.action_id.clone();
        self.urgency_update = Some(UrgencyUpdate::Pending { id: id.clone() });
        vec![Command::SetSessionUrgent { id, urgent }]
    }
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
            Target::SessionUrgency(id) => self.session_urgent_command(id, false),
            Target::SessionMoveNow(id) => self.session_urgent_command(id, true),
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

#[cfg(test)]
mod n5c_urgency_tests {
    use super::*;
    use crate::dashboard_model::{QuotaWait, SessionsResponse};
    fn session(row: &str, action: &str, mode: &str, urgent: bool, waiting: bool) -> Session {
        serde_json::from_value(serde_json::json!({
            "id":row, "provider":"codex", "routing":{
                "action_id":action,"mode":mode,"on_5h_limit":if urgent {"move"} else {"wait"}},
            "quota_wait_known":true,
            "quota_wait":if waiting {serde_json::json!({"reset_at":"2026-10-23T12:00:00Z","waiting":1})} else {serde_json::Value::Null}
        })).unwrap()
    }
    #[test]
    fn n5c_urgency_uses_exact_current_action_and_explicit_desired_state_once() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let mut rows = vec![
            session("111111111111", &a, "recover", false, true),
            session("222222222222", &b, "rebalance", false, true),
        ];
        let mut state = DashboardState::default();
        state.select(Some("222222222222".into()));
        state.refresh_session_actions(&rows, true);
        assert_eq!(
            state.activate(&Target::SessionUrgency(rows[0].id.clone())),
            vec![Command::SetSessionUrgent {
                id: a.clone(),
                urgent: true
            }]
        );
        assert_eq!(state.selected.as_deref(), Some("222222222222"));
        assert!(
            state
                .activate(&Target::SessionMoveNow(rows[1].id.clone()))
                .is_empty()
        );
        assert!(
            state
                .activate(&Target::SessionUrgency(rows[0].id.clone()))
                .is_empty()
        );
        state.apply_urgency_update(UrgencyUpdate::Finished {
            id: b,
            failed: false,
        });
        assert!(
            !state.session_urgency_enabled(&rows[0].id),
            "unrelated completion must not unlock the lane"
        );
        state.apply_urgency_update(UrgencyUpdate::Finished {
            id: a.clone(),
            failed: false,
        });
        assert!(
            !rows[0].routing.as_ref().unwrap().urgent(),
            "acknowledgement never rewrites DTO policy"
        );
        rows[0].routing.as_mut().unwrap().on_5h_limit = "move".into();
        state.refresh_session_actions(&rows, true);
        assert_eq!(
            state.activate(&Target::SessionUrgency(rows[0].id.clone())),
            vec![Command::SetSessionUrgent {
                id: a,
                urgent: false
            }]
        );
    }
    #[test]
    fn n5c_urgency_legacy_unknown_offline_and_removed_rows_have_no_authority() {
        let valid = session("111111111111", &"a".repeat(64), "recover", false, true);
        for routing in [
            None,
            Some(SessionRouting {
                action_id: valid.id.clone(),
                on_5h_limit: "wait".into(),
                mode: "recover".into(),
            }),
            Some(SessionRouting {
                action_id: "A".repeat(64),
                on_5h_limit: "wait".into(),
                mode: "recover".into(),
            }),
            Some(SessionRouting {
                action_id: "a".repeat(64),
                on_5h_limit: "future".into(),
                mode: "recover".into(),
            }),
            Some(SessionRouting {
                action_id: "a".repeat(64),
                on_5h_limit: "wait".into(),
                mode: "future".into(),
            }),
        ] {
            let mut row = valid.clone();
            row.routing = routing;
            let mut state = DashboardState::default();
            state.refresh_session_actions(&[row.clone()], true);
            assert!(
                state
                    .activate(&Target::SessionUrgency(row.id.clone()))
                    .is_empty()
            );
            assert!(state.activate(&Target::SessionMoveNow(row.id)).is_empty());
        }
        let mut state = DashboardState::default();
        state.refresh_session_actions(std::slice::from_ref(&valid), true);
        assert!(state.session_urgency_enabled(&valid.id));
        assert!(
            state
                .activate(&Target::SessionUrgency("a".repeat(64)))
                .is_empty(),
            "action ID is not a display-row lookup"
        );
        state.refresh_session_actions(std::slice::from_ref(&valid), false);
        assert!(
            state
                .activate(&Target::SessionUrgency(valid.id.clone()))
                .is_empty()
        );
        state.refresh_session_actions(&[], true);
        assert!(
            state
                .activate(&Target::SessionUrgency(valid.id.clone()))
                .is_empty()
        );
        state.refresh_session_actions(&[valid.clone(), valid.clone()], true);
        assert!(
            state.activate(&Target::SessionUrgency(valid.id)).is_empty(),
            "duplicate display ID cannot choose authority"
        );
        let legacy: SessionsResponse =
            serde_json::from_value(serde_json::json!({"sessions":[{"id":"111111111111"}]}))
                .unwrap();
        assert!(legacy.sessions[0].routing.is_none());
    }
    #[test]
    fn n5c_move_now_requires_real_wait_and_respects_strict_while_toggle_remains_available() {
        let id = "a".repeat(64);
        let mut row = session("111111111111", &id, "strict", false, true);
        let mut state = DashboardState::default();
        state.refresh_session_actions(std::slice::from_ref(&row), true);
        assert!(
            state
                .activate(&Target::SessionMoveNow(row.id.clone()))
                .is_empty()
        );
        assert_eq!(
            state.activate(&Target::SessionUrgency(row.id.clone())),
            vec![Command::SetSessionUrgent {
                id: id.clone(),
                urgent: true
            }]
        );
        state.apply_urgency_update(UrgencyUpdate::Finished {
            id: id.clone(),
            failed: true,
        });
        assert!(matches!(
            state.urgency_update,
            Some(UrgencyUpdate::Finished { failed: true, .. })
        ));
        assert_eq!(row.routing.as_ref().unwrap().on_5h_limit, "wait");
        row.routing.as_mut().unwrap().mode = "recover".into();
        row.quota_wait = None;
        state.refresh_session_actions(std::slice::from_ref(&row), true);
        assert!(
            state
                .activate(&Target::SessionMoveNow(row.id.clone()))
                .is_empty()
        );
        row.quota_wait = Some(QuotaWait {
            reset_at: "2026-10-23T12:00:00Z".parse().unwrap(),
            waiting: 0,
        });
        state.refresh_session_actions(std::slice::from_ref(&row), true);
        assert!(
            state
                .activate(&Target::SessionMoveNow(row.id.clone()))
                .is_empty()
        );
        row.quota_wait.as_mut().unwrap().waiting = 1;
        row.quota_wait_known = false;
        state.refresh_session_actions(std::slice::from_ref(&row), true);
        assert!(
            state
                .activate(&Target::SessionMoveNow(row.id.clone()))
                .is_empty(),
            "unknown coverage cannot authorize the waiting-only action"
        );
        row.quota_wait_known = true;
        state.refresh_session_actions(std::slice::from_ref(&row), true);
        assert_eq!(
            state.activate(&Target::SessionMoveNow(row.id.clone())),
            vec![Command::SetSessionUrgent { id, urgent: true }]
        );
    }
}
