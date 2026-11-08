//! Native login lifecycle, pure and portable. Cancellation retains the worker lease
//! until the exact core session is terminal; a dismissed prompt never returns.
use crate::client::login::{LoginFailure, LoginMethod};
use qb_proto::admin_login::{LoginStart, LoginState};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Chooser,
    Starting,
    Pending,
    Cancelling,
    Completed,
    Failed,
    Expired,
    Cancelled,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Web,
    Device,
    Open,
    CopyLink,
    CopyCode,
    Retry,
    Change,
    Cancel,
}
#[derive(Clone, PartialEq, Eq)]
pub struct Card {
    pub title: String,
    pub body: String,
    pub code: Option<String>,
    pub actions: Vec<(Action, String, bool)>,
}
// Deliberately no Debug: code/URL are ephemeral, never diagnostic content.
pub struct Flow {
    pub phase: Phase,
    pub method: Option<LoginMethod>,
    pub origin: Option<String>,
    pub visible: bool,
    pub leased: bool,
    pub cancel_requested: bool,
    pub choose_after_cancel: bool,
    pub id: Option<String>,
    pub prompt: Option<LoginStart>,
    pub deadline: Option<Duration>,
    pub copied: bool,
    pub browser_failed: bool,
    pub status_uncertain: bool,
    pub failure: Option<LoginFailure>,
    pub generation: u64,
    opened: bool,
    completion_reported: bool,
}
impl Default for Flow {
    fn default() -> Self {
        Self {
            phase: Phase::Chooser,
            method: None,
            origin: None,
            visible: false,
            leased: false,
            cancel_requested: false,
            choose_after_cancel: false,
            id: None,
            prompt: None,
            deadline: None,
            copied: false,
            browser_failed: false,
            status_uncertain: false,
            failure: None,
            generation: 0,
            opened: false,
            completion_reported: false,
        }
    }
}
impl Flow {
    pub fn chooser(&mut self, origin: Option<String>) {
        if self.leased {
            self.cancel(true);
            return;
        }
        // Completion racing dismissal must remain truthful when the user reopens.
        if self.phase == Phase::Completed {
            self.visible = true;
            return;
        }
        // An ambiguous start has no safely addressable session; never issue another.
        if self.phase == Phase::Unknown {
            self.visible = true;
            return;
        }
        *self = Self {
            generation: self.generation.saturating_add(1),
            origin,
            visible: true,
            ..Self::default()
        };
    }
    pub fn begin(&mut self, method: LoginMethod) -> Option<u64> {
        if self.leased
            || !self.visible
            || !matches!(
                self.phase,
                Phase::Chooser | Phase::Failed | Phase::Expired | Phase::Cancelled
            )
        {
            return None;
        }
        self.generation = self.generation.checked_add(1)?;
        self.method = Some(method);
        self.phase = Phase::Starting;
        self.leased = true;
        self.cancel_requested = false;
        self.choose_after_cancel = false;
        self.prompt = None;
        self.id = None;
        self.deadline = None;
        self.copied = false;
        self.browser_failed = false;
        self.status_uncertain = false;
        self.failure = None;
        self.opened = false;
        self.completion_reported = false;
        Some(self.generation)
    }
    pub fn started(&mut self, generation: u64, start: LoginStart, now: Duration) -> bool {
        if self.generation != generation || !self.leased {
            return false;
        }
        self.id = Some(start.login_id.clone());
        if self.cancel_requested || !self.visible {
            self.phase = Phase::Cancelling;
            return false;
        }
        self.deadline = match self.method {
            Some(LoginMethod::Device) => start
                .device
                .as_ref()
                .and_then(|d| now.checked_add(Duration::from_secs(d.expires_in))),
            Some(LoginMethod::Web) => now.checked_add(Duration::from_secs(600)),
            None => None,
        };
        self.prompt = Some(start);
        self.phase = Phase::Pending;
        true
    }
    pub fn invalid_started(&mut self, generation: u64, id: String) {
        if generation != self.generation || !self.leased {
            return;
        }
        self.id = Some(id);
        self.prompt = None;
        self.deadline = None;
        self.cancel_requested = true;
        self.phase = Phase::Cancelling;
    }
    pub fn start_failed(&mut self, generation: u64, failure: LoginFailure) {
        if generation != self.generation {
            return;
        }
        self.prompt = None;
        self.deadline = None;
        self.leased = false;
        self.failure = Some(failure);
        self.phase = match failure {
            LoginFailure::Busy | LoginFailure::Unknown => Phase::Unknown,
            _ => Phase::Failed,
        };
    }
    pub fn cancel(&mut self, chooser: bool) {
        self.prompt = None;
        self.deadline = None;
        self.copied = false;
        self.choose_after_cancel = chooser;
        self.visible = chooser;
        if self.leased {
            self.cancel_requested = true;
            self.phase = Phase::Cancelling;
        } else if self.phase != Phase::Unknown {
            self.phase = if chooser {
                Phase::Chooser
            } else {
                Phase::Cancelled
            };
        }
    }
    pub fn expired(&self, now: Duration) -> bool {
        self.deadline.is_some_and(|at| now >= at)
    }
    pub fn interactive(&self, now: Duration) -> bool {
        self.visible && self.phase == Phase::Pending && !self.cancel_requested && !self.expired(now)
    }
    pub fn auto_open(&mut self, now: Duration) -> Option<String> {
        if self.method != Some(LoginMethod::Web) || self.opened || !self.interactive(now) {
            return None;
        }
        self.opened = true;
        self.prompt.as_ref().map(|p| p.auth_url.clone())
    }
    pub fn terminal(&mut self, generation: u64, status: LoginState) -> bool {
        if generation != self.generation || status == LoginState::Pending {
            return false;
        }
        self.leased = false;
        self.prompt = None;
        self.deadline = None;
        self.id = None;
        self.copied = false;
        let completed = status == LoginState::Completed && !self.completion_reported;
        if completed {
            self.completion_reported = true;
            self.phase = Phase::Completed;
        } else if self.choose_after_cancel {
            self.phase = Phase::Chooser;
            self.method = None;
        } else {
            self.phase = match status {
                LoginState::Expired => Phase::Expired,
                LoginState::Failed => Phase::Failed,
                LoginState::Cancelled => Phase::Cancelled,
                LoginState::Completed => Phase::Completed,
                LoginState::Pending => unreachable!(),
            };
        }
        self.cancel_requested = false;
        self.choose_after_cancel = false;
        completed
    }
    pub fn value(&self, action: Action, now: Duration) -> Option<String> {
        if !self.interactive(now) {
            return None;
        }
        let prompt = self.prompt.as_ref()?;
        match action {
            Action::Open | Action::CopyLink => Some(prompt.auth_url.clone()),
            Action::CopyCode => prompt.device.as_ref().map(|d| d.user_code.clone()),
            _ => None,
        }
    }
    pub fn expire(&mut self, now: Duration) -> bool {
        if self.phase != Phase::Pending || !self.expired(now) {
            return false;
        }
        self.phase = Phase::Expired;
        self.prompt = None;
        self.deadline = None;
        self.cancel_requested = true;
        self.copied = false;
        true
    }
    pub fn card(&self, now: Duration) -> Card {
        use crate::i18n::static_text as t;
        let mut card = Card {
            title: t("Codex 계정 추가").into(),
            body: String::new(),
            code: None,
            actions: vec![],
        };
        match self.phase {
            Phase::Chooser => {
                card.body = format!("{}\n{}\n\n{}\n{}", t("웹 브라우저로 로그인"), t("이 PC의 브라우저에서 로그인합니다."), t("디바이스 코드로 로그인"), t("표시된 코드를 브라우저에서 입력합니다. 다른 기기에서도 가능합니다."));
                card.actions.extend([(Action::Web, t("웹 브라우저로 로그인").into(), true), (Action::Device, t("디바이스 코드로 로그인").into(), true)]);
            }
            Phase::Starting => card.body = t("로그인 요청 중…").into(),
            Phase::Cancelling => card.body = t("이전 로그인 종료를 확인하는 중…").into(),
            Phase::Completed => card.body = t("로그인이 완료되었습니다.").into(),
            Phase::Unknown => card.body = t("로그인 요청 결과를 확인하지 못했습니다. 새 요청을 만들 수 없습니다. 코어 연결과 기존 로그인 요청이 종료되었는지 확인해 주세요.").into(),
            Phase::Pending if !self.expired(now) => {
                let device = self.prompt.as_ref().and_then(|p| p.device.as_ref());
                card.body = t(if device.is_some() { "브라우저에서 코드를 입력해 주세요." } else { "브라우저에서 로그인을 완료해 주세요." }).into();
                if let Some(device) = device {
                    card.code = Some(device.user_code.clone());
                    card.body.push_str(&format!("\n{}\n{}", device.verification_url, crate::i18n::format("남은 시간 {0}초", &[self.deadline.unwrap_or(now).saturating_sub(now).as_secs().to_string()])));
                    card.actions.push((Action::CopyCode, t(if self.copied { "코드 복사됨" } else { "코드 복사" }).into(), true));
                }
                if self.status_uncertain { card.body.push_str(&format!("\n{}", t("연결을 확인하며 기존 로그인 상태를 다시 확인하고 있습니다. 새 요청은 만들지 않습니다."))); }
                if self.browser_failed { card.body.push_str(&format!("\n{}", t("브라우저를 열지 못했습니다. 주소를 복사하거나 다시 열어 주세요."))); }
                card.actions.push((Action::Open, t(if device.is_some() { "인증 페이지 열기" } else { "브라우저 다시 열기" }).into(), true));
                if device.is_none() { card.actions.push((Action::CopyLink, t("주소 복사").into(), true)); }
            }
            Phase::Pending | Phase::Expired | Phase::Failed | Phase::Cancelled => {
                card.body = t(if (self.phase == Phase::Expired || self.expired(now)) && self.method == Some(LoginMethod::Device) { "로그인 시간이 만료되었습니다. 새 코드를 명시적으로 요청해 주세요." }
                    else if self.phase == Phase::Expired || self.expired(now) { "로그인 시간이 만료되었습니다. 다시 시도하거나 로그인 방법을 변경해 주세요." }
                    else if self.phase == Phase::Cancelled { "로그인이 취소되었습니다." }
                    else if self.failure == Some(LoginFailure::RateLimited) { "로그인 요청이 너무 많습니다. 잠시 후 직접 다시 시도하거나 웹 로그인을 선택해 주세요." }
                    else { "로그인 요청을 처리하지 못했습니다. 디바이스 로그인이 허용되어 있는지 Security 설정이나 워크스페이스 관리자에게 확인하세요." }).into();
                card.actions.push((Action::Retry, t(if self.method == Some(LoginMethod::Device) { "새 코드 요청" } else { "다시 시도" }).into(), !self.leased));
                if self.method == Some(LoginMethod::Device) && self.phase == Phase::Failed { card.actions.push((Action::Web, t("웹 브라우저로 로그인").into(), !self.leased)); }
            }
        }
        if self.phase != Phase::Chooser {
            card.actions.push((
                Action::Change,
                t("로그인 방법 변경").into(),
                self.phase != Phase::Unknown,
            ));
        }
        card.actions.push((
            Action::Cancel,
            t(if self.phase == Phase::Completed {
                "닫기"
            } else {
                "취소"
            })
            .into(),
            true,
        ));
        card
    }
}

impl std::fmt::Debug for Card {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginCard")
            .field("actions", &self.actions)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qb_proto::admin_login::DevicePrompt;
    fn start(method: LoginMethod) -> LoginStart {
        LoginStart {
            login_id: "synthetic-session".into(),
            auth_url: if method == LoginMethod::Device {
                "https://auth.openai.com/codex/device"
            } else {
                "https://auth.openai.com/oauth/authorize?state=synthetic"
            }
            .into(),
            device: (method == LoginMethod::Device).then(|| DevicePrompt {
                verification_url: "https://auth.openai.com/codex/device".into(),
                user_code: "SYNTHETIC-CODE".into(),
                expires_in: 83,
            }),
        }
    }
    fn now() -> Duration {
        Duration::from_millis(40_500)
    }
    fn pending(method: LoginMethod) -> (Flow, u64) {
        let mut f = Flow::default();
        f.chooser(Some("existing-account".into()));
        let generation = f.begin(method).unwrap();
        assert!(f.started(generation, start(method), now()));
        (f, generation)
    }
    #[test]
    fn chooser_has_zero_start_authority_until_explicit_choice_and_double_click_admits_one() {
        let mut f = Flow::default();
        f.chooser(None);
        assert!(!f.leased && f.id.is_none() && f.prompt.is_none());
        assert!(f.auto_open(now()).is_none());
        assert!(f.begin(LoginMethod::Device).is_some());
        assert!(f.begin(LoginMethod::Device).is_none());
        assert!(f.begin(LoginMethod::Web).is_none());
    }
    #[test]
    fn device_uses_exact_receipt_lifetime_copy_open_leave_code_and_deadline_unchanged() {
        let (mut f, _) = pending(LoginMethod::Device);
        let deadline = now() + Duration::from_secs(83);
        assert_eq!(f.deadline, Some(deadline));
        assert!(f.auto_open(now()).is_none());
        for _ in 0..4 {
            assert_eq!(
                f.value(Action::CopyCode, now()).as_deref(),
                Some("SYNTHETIC-CODE")
            );
            assert_eq!(
                f.value(Action::Open, now()).as_deref(),
                Some("https://auth.openai.com/codex/device")
            );
        }
        f.copied = true;
        assert_eq!(f.card(now()).code.as_deref(), Some("SYNTHETIC-CODE"));
        assert_eq!(f.deadline, Some(deadline));
        assert!(f.interactive(deadline - Duration::from_millis(1)));
        assert!(!f.interactive(deadline));
        assert!(f.value(Action::CopyCode, deadline).is_none());
        assert!(f.expire(deadline));
        assert!(f.prompt.is_none());
        assert!(f.leased && f.cancel_requested);
        assert!(f.begin(LoginMethod::Device).is_none());
    }
    #[test]
    fn web_opens_once_and_shell_failure_keeps_copy_and_reopen_without_new_start() {
        let (mut f, _) = pending(LoginMethod::Web);
        assert!(f.auto_open(now()).is_some());
        assert!(f.auto_open(now()).is_none());
        assert_eq!(f.deadline, Some(now() + Duration::from_secs(600)));
        f.browser_failed = true;
        assert!(f.value(Action::CopyLink, now()).is_some());
        assert!(f.value(Action::Open, now()).is_some());
        assert!(f.card(now()).code.is_none());
        assert!(f.leased);
    }
    #[test]
    fn cancel_escape_dismiss_and_method_switch_clear_prompt_but_hold_lease_until_terminal() {
        for _route in ["cancel", "escape", "dismiss", "hide"] {
            let (mut f, generation) = pending(LoginMethod::Device);
            f.cancel(false);
            assert!(f.prompt.is_none() && f.deadline.is_none());
            assert!(f.begin(LoginMethod::Web).is_none());
            assert!(f.auto_open(now()).is_none());
            f.chooser(Some("other".into()));
            assert!(f.begin(LoginMethod::Web).is_none());
            assert!(!f.terminal(generation, LoginState::Pending));
            assert!(f.leased);
            assert!(!f.terminal(generation, LoginState::Cancelled));
            assert!(f.phase == Phase::Chooser);
            assert!(f.begin(LoginMethod::Web).is_some());
        }
    }
    #[test]
    fn late_start_after_cancel_retains_exact_id_for_cancel_without_publishing_payload() {
        let mut f = Flow::default();
        f.chooser(None);
        let generation = f.begin(LoginMethod::Device).unwrap();
        f.cancel(true);
        assert!(!f.started(generation, start(LoginMethod::Device), now()));
        assert_eq!(f.id.as_deref(), Some("synthetic-session"));
        assert!(f.prompt.is_none());
        assert!(f.cancel_requested && f.leased && f.auto_open(now()).is_none());
        assert!(f.begin(LoginMethod::Web).is_none());
        f.terminal(generation, LoginState::Cancelled);
        assert!(f.begin(LoginMethod::Web).is_some());
    }
    #[test]
    fn malformed_known_id_is_reconciled_and_completed_cancel_race_reports_once() {
        let mut f = Flow::default();
        f.chooser(Some("existing-account".into()));
        let generation = f.begin(LoginMethod::Device).unwrap();
        f.invalid_started(generation, "malformed-known-session".into());
        assert!(f.leased && f.cancel_requested && f.prompt.is_none());
        f.cancel(true);
        assert!(f.terminal(generation, LoginState::Completed));
        assert!(!f.terminal(generation, LoginState::Completed));
        assert!(f.phase == Phase::Completed);
        assert_eq!(f.origin.as_deref(), Some("existing-account"));
        assert!(f.begin(LoginMethod::Web).is_none());
    }
    #[test]
    fn unknown_no_id_and_busy_latch_survive_dismiss_and_reopen_without_new_session() {
        for failure in [LoginFailure::Unknown, LoginFailure::Busy] {
            let mut f = Flow::default();
            f.chooser(None);
            let generation = f.begin(LoginMethod::Device).unwrap();
            f.start_failed(generation, failure);
            f.cancel(false);
            f.chooser(None);
            assert!(f.phase == Phase::Unknown && f.prompt.is_none());
            assert!(f.begin(LoginMethod::Device).is_none() && f.begin(LoginMethod::Web).is_none());
            assert!(
                f.card(now())
                    .actions
                    .iter()
                    .all(|(action, _, enabled)| !enabled || *action == Action::Cancel)
            );
        }
    }
    #[test]
    fn expiry_and_finite_failure_require_explicit_retry_or_web_alternative() {
        let (mut f, generation) = pending(LoginMethod::Device);
        f.terminal(generation, LoginState::Expired);
        assert!(!f.leased && f.prompt.is_none());
        assert!(
            f.card(now())
                .actions
                .iter()
                .any(|(a, _, enabled)| *a == Action::Retry && *enabled)
        );
        let generation = f.begin(LoginMethod::Device).unwrap();
        f.start_failed(generation, LoginFailure::RateLimited);
        assert!(!f.leased && f.id.is_none());
        assert!(
            f.card(now())
                .actions
                .iter()
                .any(|(a, _, enabled)| *a == Action::Web && *enabled)
        );
    }
    #[test]
    fn stale_generation_cannot_publish_prompt_and_sensitive_debug_is_redacted() {
        let (mut f, generation) = pending(LoginMethod::Device);
        f.cancel(true);
        f.terminal(generation, LoginState::Cancelled);
        let next = f.begin(LoginMethod::Web).unwrap();
        assert!(!f.started(generation, start(LoginMethod::Device), now()));
        f.started(next, start(LoginMethod::Web), now());
        let debug = format!("{:?}", f.card(now()));
        assert!(!debug.contains("SYNTHETIC-CODE") && !debug.contains("auth.openai.com"));
    }
}
