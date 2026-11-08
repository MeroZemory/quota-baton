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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseState {
    Waiting,
    Ready,
    Unresolved,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerPress {
    pub action: Action,
    pub generation: u64,
}
impl PointerPress {
    pub fn matches(self, action: Action, generation: u64) -> bool {
        self.action == action && self.generation == generation
    }
}
/// Unlike the ordinary action drain, exact login cleanup has no five-second abort.
/// The HTTP start itself is bounded; a known session keeps reconciling to terminal.
pub async fn retain_cleanup(mut leased: impl FnMut() -> bool) {
    while leased() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// Deliberately no Debug: code/URL are ephemeral, never diagnostic content.
pub struct Flow {
    pub close_requested: bool,
    pub phase: Phase,
    pub method: Option<LoginMethod>,
    pub origin: Option<String>,
    pub completed_account: Option<String>,
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
            close_requested: false,
            phase: Phase::Chooser,
            method: None,
            origin: None,
            completed_account: None,
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
    pub fn request_close(&mut self) {
        if self.close_requested {
            return;
        }
        self.cancel(false);
        self.close_requested = true;
    }
    pub fn close_state(&self) -> CloseState {
        if self.leased {
            CloseState::Waiting
        } else if self.phase == Phase::Unknown {
            CloseState::Unresolved
        } else {
            CloseState::Ready
        }
    }
    pub fn abandon_close(&mut self) {
        self.close_requested = false;
        self.visible = true;
    }
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
        if self.close_requested
            || self.leased
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
        self.completed_account = None;
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
            LoginFailure::Unknown => Phase::Unknown,
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
    pub fn terminal_with_account(
        &mut self,
        generation: u64,
        status: LoginState,
        account: Option<String>,
    ) -> bool {
        let completed = self.terminal(generation, status);
        if completed {
            self.completed_account = account;
        }
        completed
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
            Phase::Completed => card.body = t(match (self.origin.as_deref(), self.completed_account.as_deref()) {
                (Some(selected), Some(completed)) if selected != completed => "다른 계정의 로그인이 완료되었습니다. 선택한 계정의 재로그인은 확인되지 않았습니다.",
                (Some(_), None) => "로그인은 완료되었지만 선택한 계정의 결과는 확인하지 못했습니다. 계정 목록을 확인해 주세요.",
                _ => "로그인이 완료되었습니다.",
            }).into(),
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
    fn unknown_no_id_latch_survives_dismiss_and_reopen_without_new_session() {
        let mut f = Flow::default();
        f.chooser(None);
        let generation = f.begin(LoginMethod::Device).unwrap();
        f.start_failed(generation, LoginFailure::Unknown);
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
    #[test]
    fn definite_callback_busy_allows_explicit_device_choice_without_automatic_start() {
        let mut f = Flow::default();
        f.chooser(None);
        let generation = f.begin(LoginMethod::Web).unwrap();
        f.start_failed(generation, LoginFailure::Busy);
        assert!(f.phase == Phase::Failed && !f.leased && f.id.is_none());
        assert!(
            f.card(now())
                .actions
                .iter()
                .any(|(action, _, enabled)| *action == Action::Change && *enabled)
        );
        f.cancel(true);
        assert!(f.phase == Phase::Chooser && !f.leased);
        assert!(f.begin(LoginMethod::Device).is_some());
    }
    #[test]
    fn held_pointer_action_cannot_transfer_between_login_generations_or_actions() {
        let (mut f, generation_a) = pending(LoginMethod::Device);
        let press = PointerPress {
            action: Action::CopyCode,
            generation: generation_a,
        };
        assert!(press.matches(Action::CopyCode, generation_a));
        assert!(!press.matches(Action::Open, generation_a));
        f.cancel(true);
        f.terminal(generation_a, LoginState::Cancelled);
        let generation_b = f.begin(LoginMethod::Device).unwrap();
        f.started(generation_b, start(LoginMethod::Device), now());
        assert!(f.interactive(now()));
        assert!(!press.matches(Action::CopyCode, generation_b));
        assert!(
            PointerPress {
                action: Action::CopyCode,
                generation: generation_b
            }
            .matches(Action::CopyCode, generation_b)
        );
    }
    #[tokio::test(start_paused = true)]
    async fn close_retains_delayed_start_past_five_seconds_then_cancels_exact_late_id() {
        use std::sync::{Arc, Mutex};
        let mut f = Flow::default();
        f.chooser(None);
        let generation = f.begin(LoginMethod::Device).unwrap();
        let flow = Arc::new(Mutex::new(f));
        let events = Arc::new(Mutex::new(Vec::new()));
        let job_flow = flow.clone();
        let job_events = events.clone();
        let job = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            {
                let mut f = job_flow.lock().unwrap();
                assert!(!f.started(generation, start(LoginMethod::Device), now()));
                assert!(f.prompt.is_none() && f.cancel_requested);
                job_events.lock().unwrap().push(("cancel", f.id.clone()));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            job_events
                .lock()
                .unwrap()
                .push(("status", Some("synthetic-session".into())));
            assert!(
                !job_flow
                    .lock()
                    .unwrap()
                    .terminal(generation, LoginState::Cancelled)
            );
        });
        flow.lock().unwrap().request_close();
        let wait_flow = flow.clone();
        let drain = tokio::spawn(async move {
            retain_cleanup(|| wait_flow.lock().unwrap().leased).await;
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(!drain.is_finished());
        assert_eq!(flow.lock().unwrap().close_state(), CloseState::Waiting);
        assert!(events.lock().unwrap().is_empty());
        job.await.unwrap();
        drain.await.unwrap();
        assert_eq!(flow.lock().unwrap().close_state(), CloseState::Ready);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                ("cancel", Some("synthetic-session".into())),
                ("status", Some("synthetic-session".into()))
            ]
        );
    }
    #[tokio::test(start_paused = true)]
    async fn close_known_pending_retains_cleanup_until_authoritative_completed_race() {
        use std::sync::{Arc, Mutex};
        let (f, generation) = pending(LoginMethod::Device);
        let flow = Arc::new(Mutex::new(f));
        flow.lock().unwrap().request_close();
        assert_eq!(
            flow.lock().unwrap().id.as_deref(),
            Some("synthetic-session")
        );
        let job_flow = flow.clone();
        let job = tokio::spawn(async move {
            assert!(job_flow.lock().unwrap().cancel_requested);
            // Cancel's void acknowledgement and a Pending status cannot release cleanup.
            assert!(
                !job_flow
                    .lock()
                    .unwrap()
                    .terminal(generation, LoginState::Pending)
            );
            tokio::time::sleep(Duration::from_secs(8)).await;
            assert!(
                job_flow
                    .lock()
                    .unwrap()
                    .terminal(generation, LoginState::Completed)
            );
        });
        let wait_flow = flow.clone();
        let drain = tokio::spawn(async move {
            retain_cleanup(|| wait_flow.lock().unwrap().leased).await;
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(!drain.is_finished());
        assert_eq!(flow.lock().unwrap().close_state(), CloseState::Waiting);
        job.await.unwrap();
        drain.await.unwrap();
        let f = flow.lock().unwrap();
        assert_eq!(f.close_state(), CloseState::Ready);
        assert!(f.phase == Phase::Completed && f.prompt.is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn close_bounded_no_id_start_reports_unresolved_instead_of_cancelled() {
        use std::sync::{Arc, Mutex};
        let mut f = Flow::default();
        f.chooser(None);
        let generation = f.begin(LoginMethod::Web).unwrap();
        f.request_close();
        let flow = Arc::new(Mutex::new(f));
        let job_flow = flow.clone();
        let job = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            job_flow
                .lock()
                .unwrap()
                .start_failed(generation, LoginFailure::Unknown);
        });
        let wait_flow = flow.clone();
        let drain = tokio::spawn(async move {
            retain_cleanup(|| wait_flow.lock().unwrap().leased).await;
        });
        job.await.unwrap();
        drain.await.unwrap();
        let mut f = flow.lock().unwrap();
        assert_eq!(f.close_state(), CloseState::Unresolved);
        assert!(f.phase == Phase::Unknown);
        f.abandon_close();
        assert!(f.visible && !f.close_requested && f.begin(LoginMethod::Device).is_none());
    }
}

#[cfg(test)]
mod completion_identity_tests {
    use super::*;
    #[test]
    fn selected_same_different_and_absent_completion_ids_never_conflate_relogin() {
        for language in [crate::i18n::UiLanguage::Ko, crate::i18n::UiLanguage::En] {
            crate::i18n::with_language(language, || {
                for (actual, expected) in [
                    (Some("selected"), "로그인이 완료되었습니다."),
                    (
                        Some("different"),
                        "다른 계정의 로그인이 완료되었습니다. 선택한 계정의 재로그인은 확인되지 않았습니다.",
                    ),
                    (
                        None,
                        "로그인은 완료되었지만 선택한 계정의 결과는 확인하지 못했습니다. 계정 목록을 확인해 주세요.",
                    ),
                ] {
                    let mut flow = Flow::default();
                    flow.chooser(Some("selected".into()));
                    let generation = flow.begin(LoginMethod::Device).unwrap();
                    flow.cancel(false);
                    assert!(flow.terminal_with_account(
                        generation,
                        LoginState::Completed,
                        actual.map(str::to_owned)
                    ));
                    assert_eq!(flow.origin.as_deref(), Some("selected"));
                    assert_eq!(flow.completed_account.as_deref(), actual);
                    let card = flow.card(Duration::ZERO);
                    assert_eq!(card.body, crate::i18n::static_text(expected));
                    assert!(card.code.is_none());
                    // Result facts survive without a refreshed row lookup. A
                    // duplicate terminal event cannot replace the committed ID.
                    assert!(!flow.terminal_with_account(
                        generation,
                        LoginState::Completed,
                        Some("wrong-late".into())
                    ));
                    assert_eq!(flow.completed_account.as_deref(), actual);
                    // Explicit Change method is distinct from simply reopening
                    // the truthful completed result after a close/cancel race.
                    flow.cancel(true);
                    let _ = flow.begin(LoginMethod::Web).unwrap();
                    assert!(flow.completed_account.is_none());
                }
                let mut add = Flow::default();
                add.chooser(None);
                let g = add.begin(LoginMethod::Device).unwrap();
                assert!(add.terminal_with_account(g, LoginState::Completed, Some("new".into())));
                assert_eq!(
                    add.card(Duration::ZERO).body,
                    crate::i18n::static_text("로그인이 완료되었습니다.")
                );
            });
        }
    }
}
