//! Private view/cancellation policy; no I/O or window calls.
#[derive(Default)]
pub(super) struct ViewOwnership {
    owner: usize,
    value: Option<String>,
}
impl std::ops::Deref for ViewOwnership {
    type Target = Option<String>;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}
impl ViewOwnership {
    pub(super) fn owner(&self) -> usize {
        self.owner
    }
    // Every UI transition (even to the same text) invalidates earlier work.
    pub(super) fn set(&mut self, value: Option<String>) -> usize {
        self.owner = self.owner.checked_add(1).expect("view owner exhausted");
        self.value = value;
        self.owner
    }
    pub(super) fn begin(&mut self) -> usize {
        self.set(self.value.clone())
    }
    pub(super) fn begin_menu(&mut self, id: &str, index: usize, toggle: bool) -> Option<usize> {
        if self
            .value
            .as_deref()
            .is_some_and(|v| v.ends_with(":pending"))
        {
            return None;
        }
        Some(self.set(toggle.then(|| format!("menu:{id}:{index}:pending"))))
    }
    pub(super) fn publish(&mut self, owner: usize, value: Option<String>) -> bool {
        if self.owner != owner {
            return false;
        }
        self.value = value;
        true
    }
    pub(super) fn completion(&mut self, owner: usize, error: Option<String>) -> usize {
        if self.owner == owner
            && let Some(error) = error
        {
            return self.set(Some(error)); // invalidates previously queued successes
        }
        owner
    }
    pub(super) fn consume(&mut self, owner: usize, preserve_error: bool) {
        if self.owner == owner && !preserve_error {
            let menu = self
                .value
                .as_deref()
                .and_then(|v| v.strip_suffix(":pending"))
                .map(str::to_owned);
            self.set(menu);
        }
    }
    pub(super) fn take(&mut self) -> Option<String> {
        let value = self.value.clone();
        self.set(None);
        value
    }
}

// Keep the sole ID until the urgent lane accepts ownership. No dialog restore.
pub(super) fn admit_cancellation(
    id: &mut Option<String>,
    submit: impl FnOnce(String) -> bool,
) -> bool {
    let Some(pending) = id.as_ref() else {
        return true;
    };
    if !submit(pending.clone()) {
        return false;
    }
    *id = None;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkbox_refresh_keeps_menu_and_command_closes_it() {
        let mut view = ViewOwnership::default();
        view.set(Some("menu:codex:3".into()));
        let owner = view.begin_menu("codex", 3, true).unwrap();
        assert_eq!(view.as_deref(), Some("menu:codex:3:pending"));
        assert!(view.begin_menu("codex", 3, true).is_none());
        view.completion(owner, None);
        view.consume(owner, false);
        assert_eq!(view.as_deref(), Some("menu:codex:3"));
        let owner = view.begin_menu("codex", 0, false).unwrap();
        assert!(view.is_none());
        view.consume(owner, false);
        assert!(view.is_none());
    }
    #[test]
    fn dismissed_checkbox_completion_cannot_reopen_menu() {
        let mut view = ViewOwnership::default();
        let owner = view.begin_menu("codex", 3, true).unwrap();
        view.set(Some("login:claude".into()));
        view.consume(owner, false);
        assert_eq!(view.as_deref(), Some("login:claude"));
    }
    #[test]
    fn action_a_finishing_after_new_login_b_cannot_clear_or_publish_over_b() {
        let mut view = ViewOwnership::default();
        let a = view.set(Some("menu:A".into()));
        let b = view.set(Some("login:B".into()));
        assert!(!view.publish(a, Some("login:A:completed".into())));
        view.consume(a, false);
        assert_eq!(view.as_deref(), Some("login:B"));
        assert_eq!(view.owner(), b);
    }
    #[test]
    fn queued_login_a_success_cannot_clear_b_or_a_new_safe_error() {
        let mut view = ViewOwnership::default();
        let a = view.set(Some("login:A".into()));
        // A posts success, then B starts before UI dispatch consumes A.
        let queued_a = view.completion(a, None);
        let b = view.set(Some("login:B".into()));
        view.consume(queued_a, false);
        assert_eq!(view.as_deref(), Some("login:B"));
        let error = view.completion(b, Some("error:safe-refresh-error".into()));
        view.consume(queued_a, false);
        view.consume(b, false);
        view.consume(error, true);
        assert_eq!(view.as_deref(), Some("error:safe-refresh-error"));
        // Even an older failure cannot overwrite the newer error.
        view.completion(a, Some("error:old".into()));
        assert_eq!(view.as_deref(), Some("error:safe-refresh-error"));
    }
    #[test]
    fn owned_success_clears_once_and_new_ui_transition_invalidates_publication() {
        let mut view = ViewOwnership::default();
        let a = view.set(Some("login:A".into()));
        assert!(view.publish(a, Some("login:A:completed".into())));
        view.consume(a, false);
        assert!(view.is_none());
        assert!(!view.publish(a, Some("login:A".into())));
    }
    #[test]
    fn rejected_cancel_retains_id_blocks_replacement_and_retries_without_transfer() {
        let mut id = Some("A".into());
        let mut view = ViewOwnership::default();
        let a = view.set(Some("login:A".into()));
        let can_start_b = admit_cancellation(&mut id, |sent| {
            assert_eq!(sent, "A");
            false
        });
        assert!(!can_start_b);
        view.set(Some("error:worker-unavailable".into()));
        assert_eq!(id.as_deref(), Some("A"));
        assert!(!view.publish(a, Some("login:A".into())));
        view.consume(a, false);
        assert_eq!(view.as_deref(), Some("error:worker-unavailable"));
        assert!(admit_cancellation(&mut id, |sent| sent == "A"));
        assert!(id.is_none());
        view.set(Some("login:B".into()));
        id = Some("B".into());
        assert_eq!(id.as_deref(), Some("B"));
        assert!(!view.publish(a, Some("login:A".into())));
    }
}
