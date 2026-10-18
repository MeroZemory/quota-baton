//! Pure action/menu state used by the native tray shell.

use crate::Row;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminOperation {
    Relogin,
    RefreshTokens,
    Account(&'static str),
    Drain(bool),
    Credits(bool),
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Relogin,
    Pause,
    Resume,
    Disable,
    Enable,
    ToggleDrain,
    ToggleCredits,
    RefreshUsage,
    RefreshTokens,
    Delete,
}

impl MenuItem {
    pub const fn label(self, _row: &Row) -> &'static str {
        match self {
            Self::Relogin => "다시 로그인",
            Self::Pause => "일시정지",
            Self::Resume => "재개",
            Self::Disable => "비활성화",
            Self::Enable => "활성화",
            Self::ToggleDrain => "새 대화 받지 않기",
            Self::ToggleCredits => "유료 크레딧 사용 허용",
            Self::RefreshUsage => "사용량 새로고침",
            Self::RefreshTokens => "지금 갱신",
            Self::Delete => "계정 삭제…",
        }
    }

    pub const fn is_toggle(self) -> bool {
        matches!(self, Self::ToggleDrain | Self::ToggleCredits)
    }

    pub const fn checked(self, row: &Row) -> Option<bool> {
        match self {
            Self::ToggleDrain => Some(row.drain),
            Self::ToggleCredits => Some(row.allow_credits),
            _ => None,
        }
    }

    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::Delete)
    }
}

/// The native shell uses this table to translate a selected menu item into
/// the typed admin operation.  Keeping the mapping here also makes it
/// testable without a Windows desktop or a live core.
pub fn admin_operation(item: MenuItem, row: &Row) -> Option<AdminOperation> {
    match item {
        MenuItem::Relogin => Some(AdminOperation::Relogin),
        MenuItem::Pause => Some(AdminOperation::Account("pause")),
        MenuItem::Resume => Some(AdminOperation::Account("resume")),
        MenuItem::Disable => Some(AdminOperation::Account("disable")),
        MenuItem::Enable => Some(AdminOperation::Account("enable")),
        MenuItem::ToggleDrain => Some(AdminOperation::Drain(!row.drain)),
        MenuItem::ToggleCredits if row.provider == "Codex" => {
            Some(AdminOperation::Credits(!row.allow_credits))
        }
        MenuItem::RefreshUsage => Some(AdminOperation::Account("refresh-usage")),
        MenuItem::RefreshTokens => Some(AdminOperation::RefreshTokens),
        MenuItem::Delete => Some(AdminOperation::Delete),
        MenuItem::ToggleCredits => None,
    }
}

pub fn menu_items(row: &Row) -> Vec<MenuItem> {
    let mut items = Vec::new();
    let linked_stale = row.source == "linked" && row.health_chip.starts_with("대기");
    if !linked_stale
        && row.user_chip != "비활성"
        && (row.health_chip == "재로그인 필요" || row.health_chip.starts_with("대기"))
        && row.source != "linked"
    {
        items.push(MenuItem::Relogin);
    }
    match row.user_chip.as_str() {
        "일시정지" => items.push(MenuItem::Resume),
        "비활성" => items.push(MenuItem::Enable),
        _ => {
            items.push(MenuItem::Pause);
            items.push(MenuItem::Disable);
        }
    }
    items.push(MenuItem::ToggleDrain);
    if row.provider == "Codex" {
        items.push(MenuItem::ToggleCredits);
    }
    if matches!(row.source.as_str(), "oauth" | "adopted")
        && !row.drain
        && row.health_chip != "재로그인 필요"
    {
        items.push(MenuItem::RefreshTokens);
    }
    items.push(MenuItem::RefreshUsage);
    items.push(MenuItem::Delete);
    items
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginPhase {
    Idle,
    Pending {
        login_id: String,
        provider: String,
        auth_url: String,
        remaining_secs: u32,
    },
    Completed {
        message: String,
    },
    Failed {
        message: String,
    },
    Expired,
    Cancelled,
}

impl LoginPhase {
    pub fn tick(&mut self) {
        if let Self::Pending { remaining_secs, .. } = self {
            if *remaining_secs == 0 {
                *self = Self::Expired;
            } else {
                *remaining_secs -= 1;
            }
        }
    }

    pub fn title(&self) -> Option<String> {
        match self {
            Self::Pending { provider, .. } => Some(format!("{provider} 로그인")),
            Self::Completed { message } | Self::Failed { message } => Some(message.clone()),
            Self::Expired => Some("시간이 지났습니다".into()),
            Self::Cancelled | Self::Idle => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitChoice {
    TrayOnly,
    TrayAndProxy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastKey {
    pub account_id: String,
    pub kind: String,
}

#[derive(Debug, Default)]
pub struct ToastDebouncer {
    entries: std::collections::HashMap<ToastKey, std::time::SystemTime>,
}

impl std::hash::Hash for ToastKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.account_id.hash(state);
        self.kind.hash(state);
    }
}

impl ToastDebouncer {
    pub fn allow(&mut self, key: ToastKey, now: std::time::SystemTime) -> bool {
        let allowed = self
            .entries
            .get(&key)
            .and_then(|at| now.duration_since(*at).ok())
            .is_none_or(|elapsed| elapsed >= std::time::Duration::from_secs(30 * 60));
        if allowed {
            self.entries.insert(key, now);
        }
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Row, Tone, Usage};

    #[test]
    fn checkbox_values_follow_refreshed_rows() {
        let mut row = row();
        for item in [MenuItem::ToggleDrain, MenuItem::ToggleCredits] {
            assert!(item.is_toggle());
            assert_eq!(item.checked(&row), Some(false));
        }
        row.drain = true;
        row.allow_credits = true;
        assert_eq!(MenuItem::ToggleDrain.checked(&row), Some(true));
        assert_eq!(MenuItem::ToggleCredits.checked(&row), Some(true));
        assert!(!MenuItem::Pause.is_toggle());
        assert_eq!(MenuItem::Pause.checked(&row), None);
    }

    fn row() -> Row {
        Row {
            view_input: None,
            id: "synthetic".into(),
            provider: "Codex".into(),
            name: "a@example.com".into(),
            email: Some("a@example.com".into()),
            plan: Some("plus".into()),
            user_chip: "재로그인 필요".into(),
            health_chip: "재로그인 필요".into(),
            health_tone: Tone::Critical,
            five_hour: Usage {
                label: "5시간".into(),
                percent: None,
                percent_text: "–".into(),
                reset_text: "–".into(),
            },
            seven_day: Usage {
                label: "주간".into(),
                percent: None,
                percent_text: "–".into(),
                reset_text: "–".into(),
            },
            meter_windows: Vec::new(),
            reason_button: true,
            attention: true,
            source: "oauth".into(),
            allow_credits: false,
            bound_sessions: 0,
            pacing_until: None,
            token_expires_at: None,
            last_refresh_at: None,
            drain: false,
            session_text: None,
        }
    }

    #[test]
    fn menu_contains_owned_relogin_and_codex_controls() {
        let items = menu_items(&row());
        assert!(items.contains(&MenuItem::Relogin));
        assert!(items.contains(&MenuItem::ToggleCredits));
        assert!(items.contains(&MenuItem::Delete));
        assert_eq!(MenuItem::Relogin.label(&row()), "다시 로그인");
    }

    #[test]
    fn manual_refresh_menu_matches_refresh_ownership_and_health() {
        let mut row = row();
        row.health_chip = "준비됨".into();
        for source in ["oauth", "adopted"] {
            row.source = source.into();
            assert!(menu_items(&row).contains(&MenuItem::RefreshTokens));
            assert_eq!(
                admin_operation(MenuItem::RefreshTokens, &row),
                Some(AdminOperation::RefreshTokens)
            );
            assert_eq!(MenuItem::RefreshTokens.label(&row), "지금 갱신");
        }
        row.source = "linked".into();
        assert!(!menu_items(&row).contains(&MenuItem::RefreshTokens));
        row.source = "oauth".into();
        row.drain = true;
        assert!(!menu_items(&row).contains(&MenuItem::RefreshTokens));
        row.drain = false;
        row.health_chip = "재로그인 필요".into();
        assert!(!menu_items(&row).contains(&MenuItem::RefreshTokens));
    }

    #[test]
    fn linked_stale_has_no_relogin() {
        let mut value = row();
        value.source = "linked".into();
        value.health_chip = "대기 · 12:00".into();
        assert!(!menu_items(&value).contains(&MenuItem::Relogin));
    }

    #[test]
    fn toast_debounces_per_account_and_kind() {
        let mut debouncer = ToastDebouncer::default();
        let key = ToastKey {
            account_id: "a".into(),
            kind: "relogin".into(),
        };
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        assert!(debouncer.allow(key.clone(), now));
        assert!(!debouncer.allow(key.clone(), now + std::time::Duration::from_secs(1)));
        assert!(debouncer.allow(key, now + std::time::Duration::from_secs(1800)));
    }

    #[test]
    fn menu_items_map_to_typed_admin_operations() {
        let value = row();
        assert_eq!(
            admin_operation(MenuItem::Relogin, &value),
            Some(AdminOperation::Relogin)
        );
        assert_eq!(
            admin_operation(MenuItem::Pause, &value),
            Some(AdminOperation::Account("pause"))
        );
        assert_eq!(
            admin_operation(MenuItem::ToggleDrain, &value),
            Some(AdminOperation::Drain(true))
        );
        assert_eq!(
            admin_operation(MenuItem::ToggleCredits, &value),
            Some(AdminOperation::Credits(true))
        );
        assert_eq!(
            admin_operation(MenuItem::Delete, &value),
            Some(AdminOperation::Delete)
        );
    }
}
