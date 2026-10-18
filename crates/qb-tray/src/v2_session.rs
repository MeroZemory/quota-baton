//! Pure open-lifetime state. Native callbacks own the I/O and overlay ownership.
use super::v2;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub id: String,
    pub provider: String,
    pub disabled_on_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub id: String,
    pub nonce: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnTo {
    List,
    Menu(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditsDialog {
    pub id: String,
    pub return_to: ReturnTo,
}

#[derive(Default)]
pub struct Session {
    pub open: bool,
    pub slots: Vec<Slot>,
    /// Only manual choices live across close/open; auto recommendations do not.
    pub manual_expansion: BTreeMap<String, bool>,
    pub expansion: BTreeMap<String, bool>,
    pub credits_dialog: Option<CreditsDialog>,
    pending: BTreeMap<String, u64>,
    errors: BTreeMap<String, Duration>,
    next_nonce: u64,
}

fn slots(view: &v2::View) -> Vec<Slot> {
    view.sections
        .iter()
        .flat_map(|section| {
            section
                .rows
                .iter()
                .map(|row| (row, false))
                .chain(section.disabled_rows.iter().map(|row| (row, true)))
                .map(|(row, disabled)| Slot {
                    id: row.raw.id.clone(),
                    provider: section.provider.clone(),
                    disabled_on_open: disabled,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

impl Session {
    pub fn show(&mut self, view: &v2::View) {
        self.open = true;
        self.slots = slots(view);
        self.expansion = view
            .sections
            .iter()
            .map(|section| {
                (
                    section.provider.clone(),
                    self.manual_expansion
                        .get(&section.provider)
                        .copied()
                        .unwrap_or(section.auto_expand_recommended),
                )
            })
            .collect();
        self.credits_dialog = None;
    }
    pub fn hide(&mut self) {
        self.open = false;
        self.credits_dialog = None;
    }
    pub fn update(&mut self, view: &v2::View) {
        if !self.open {
            return;
        }
        let current = slots(view);
        let ids: BTreeSet<_> = current.iter().map(|slot| slot.id.clone()).collect();
        self.slots.retain(|slot| ids.contains(slot.id.as_str()));
        for slot in current {
            if !self.slots.iter().any(|old| old.id == slot.id) {
                self.slots.push(slot);
            }
        }
        if self
            .credits_dialog
            .as_ref()
            .is_some_and(|dialog| !ids.contains(dialog.id.as_str()))
        {
            self.credits_dialog = None;
        }
        self.pending.retain(|id, _| ids.contains(id.as_str()));
        self.errors.retain(|id, _| ids.contains(id.as_str()));
    }
    pub fn set_expanded(&mut self, provider: &str, expanded: bool) {
        self.manual_expansion.insert(provider.into(), expanded);
        self.expansion.insert(provider.into(), expanded);
    }
    pub fn begin(&mut self, id: &str) -> Option<Ticket> {
        if self.pending.contains_key(id) {
            return None;
        }
        self.next_nonce = self.next_nonce.checked_add(1)?;
        let ticket = Ticket {
            id: id.into(),
            nonce: self.next_nonce,
        };
        self.pending.insert(ticket.id.clone(), ticket.nonce);
        self.errors.remove(id);
        Some(ticket)
    }
    pub fn complete(&mut self, ticket: &Ticket, failed: bool, now: Duration) -> bool {
        if self.pending.get(&ticket.id) != Some(&ticket.nonce) {
            return false;
        }
        self.pending.remove(&ticket.id);
        if failed {
            self.errors.insert(
                ticket.id.clone(),
                now.saturating_add(Duration::from_secs(10)),
            );
        }
        true
    }
    pub fn pending(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }
    pub fn pending_ids(&self) -> BTreeSet<String> {
        self.pending.keys().cloned().collect()
    }
    pub fn error(&self, id: &str, now: Duration) -> Option<&'static str> {
        self.errors
            .get(id)
            .is_some_and(|until| *until > now)
            .then_some("요청을 처리하지 못했습니다")
    }
    /// Opening a confirmation never yields a mutation; no remembered consent.
    pub fn ask_credits(&mut self, view: &v2::View, id: &str, return_to: ReturnTo) -> bool {
        let current = view.raw.accounts.iter().find(|a| a.id == id);
        if self.pending(id) || !current.is_some_and(|a| a.provider == "codex" && !a.allow_credits) {
            return false;
        }
        self.credits_dialog = Some(CreditsDialog {
            id: id.into(),
            return_to,
        });
        true
    }
    pub fn cancel_credits(&mut self) -> Option<CreditsDialog> {
        self.credits_dialog.take()
    }
    /// Caller PATCHes true only for this explicit confirm result, then refreshes
    /// the checked state before completing its ticket. Off bypasses this method.
    pub fn confirm_credits(&mut self, view: &v2::View) -> Option<(CreditsDialog, Ticket)> {
        let dialog = self.credits_dialog.take()?;
        if !view
            .raw
            .accounts
            .iter()
            .any(|a| a.id == dialog.id && a.provider == "codex" && !a.allow_credits)
        {
            return None;
        }
        let ticket = self.begin(&dialog.id)?;
        Some((dialog, ticket))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view(allow: bool, disabled: bool) -> v2::View {
        v2::project(&serde_json::json!({"pools":{"codex":{"eligible":0}},"accounts":[{"id":"synthetic-codex","provider":"codex","email":"sample@example.invalid","user_state":if disabled {"disabled"} else {"active"},"allow_credits":allow,"health":{"kind":"cooling","reason":"quota","scope":"account","until":"2099-01-01T00:00:00Z"}}]}).to_string(), std::time::SystemTime::now()).unwrap()
    }
    #[test]
    fn every_enable_needs_explicit_confirm_cancel_mutates_nothing_and_off_needs_no_consent() {
        let mut s = Session::default();
        let v = view(false, false);
        s.show(&v);
        assert!(s.ask_credits(&v, "synthetic-codex", ReturnTo::List));
        assert!(s.cancel_credits().is_some());
        assert!(!s.pending("synthetic-codex"));
        assert!(s.confirm_credits(&v).is_none());
        for _ in 0..2 {
            assert!(s.ask_credits(
                &v,
                "synthetic-codex",
                ReturnTo::Menu("menu:synthetic-codex:0".into())
            ));
            let (_, ticket) = s.confirm_credits(&v).unwrap();
            assert!(!s.ask_credits(&v, "synthetic-codex", ReturnTo::List));
            assert!(s.complete(&ticket, false, Duration::ZERO));
            assert!(s.confirm_credits(&v).is_none());
        }
        assert!(!s.ask_credits(&view(true, false), "synthetic-codex", ReturnTo::List));
        let off = s.begin("synthetic-codex").unwrap();
        assert!(s.credits_dialog.is_none());
        assert!(s.complete(&off, false, Duration::ZERO));
    }
    #[test]
    fn update_keeps_disabled_slot_and_manual_expansion_until_next_open() {
        let mut s = Session::default();
        s.show(&view(false, true));
        assert!(s.slots[0].disabled_on_open);
        s.set_expanded("codex", false);
        s.update(&view(false, false));
        assert!(s.slots[0].disabled_on_open);
        assert!(!s.expansion["codex"]);
        s.hide();
        s.show(&view(false, false));
        assert!(!s.slots[0].disabled_on_open);
        assert!(!s.expansion["codex"]);
    }
    #[test]
    fn stale_completion_does_not_clear_a_new_ticket_and_failure_expires_after_ten_seconds() {
        let mut s = Session::default();
        let first = s.begin("id").unwrap();
        assert!(s.complete(&first, false, Duration::ZERO));
        let next = s.begin("id").unwrap();
        assert!(!s.complete(&first, true, Duration::ZERO));
        assert!(s.pending("id"));
        assert!(s.complete(&next, true, Duration::from_secs(5)));
        assert!(s.error("id", Duration::from_secs(14)).is_some());
        assert!(s.error("id", Duration::from_secs(15)).is_none());
    }
}
