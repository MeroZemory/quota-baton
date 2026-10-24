//! Owned OAuth login. This module never opens a browser or logs callback URLs.
mod callback;
pub(crate) mod claude_profile;
mod device;
mod exchange;
mod manual;
mod parameters;

use crate::account_runtime::AccountRuntime;
use crate::accounts::{AccountStore, Provider};
use crate::observability::Observability;
use parameters::Parameters;
pub use qb_proto::admin_login::{DevicePrompt, LoginStart, LoginState, LoginStatus};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum LoginError {
    #[error(
        "로그인 콜백 포트 {port}를 사용할 수 없습니다. 다른 로그인 프로그램을 종료한 뒤 다시 시도해 주세요."
    )]
    CallbackPortBusy { port: u16 },
    #[error(
        "로그인 콜백 포트 {port}에 연결할 수 없습니다. 루프백 네트워크와 접근 권한을 확인해 주세요."
    )]
    CallbackBindFailed { port: u16 },
    #[error("로그인 요청을 찾을 수 없습니다.")]
    NotFound,
    #[error("이 로그인 요청은 수동 입력을 받지 않습니다.")]
    NotManual,
    #[error(
        "Device-code login is unavailable. Enable it in ChatGPT Security settings (personal account), or ask your workspace admin to allow it. You can use paste-redirect login instead."
    )]
    DeviceUnavailable,
    #[error(
        "Device-code login was rate limited. No new code was requested. You can use paste-redirect login instead."
    )]
    DeviceRateLimited,
    #[error("이미 로그인 입력을 제출했습니다. 로그인을 다시 시작해 주세요.")]
    AlreadySubmitted,
    #[error("로그인 요청이 만료되었습니다. 로그인을 다시 시작해 주세요.")]
    Expired,
    #[error("로그인 요청이 취소되었습니다.")]
    Cancelled,
    #[error(
        "CPA에 연결된 계정입니다. 별도 토큰 충돌을 막기 위해 로그인을 거절했습니다. 연결을 바꾸려면 replace_linked를 명시해 주세요."
    )]
    LinkedConflict,
    #[error("{}", crate::accounts::login::IDENTITY_CONFLICT_REASON)]
    IdentityConflict,
    #[error("로그인 상태가 일치하지 않습니다. 로그인을 다시 시작해 주세요.")]
    StateMismatch,
    #[error("로그인 콜백이 올바르지 않습니다.")]
    InvalidCallback,
    #[error("인증 서버와 통신하지 못했습니다. 로그인을 다시 시도해 주세요.")]
    ExchangeFailed,
    #[error("인증 서버의 계정 정보를 확인하지 못했습니다.")]
    InvalidIdentity,
    #[error("계정 정보를 저장하지 못했습니다.")]
    StoreFailed,
    #[error("테스트 주소는 관리 포트를 제외한 숫자 루프백 주소여야 합니다.")]
    InvalidTestEndpoint,
}

struct Session {
    status: watch::Sender<LoginStatus>,
    cancel: watch::Sender<bool>,
    finished_at: Mutex<Option<tokio::time::Instant>>,
    manual: Option<manual::Submission>,
}

enum LoginInput {
    Callback(manual::Input),
    Device(device::Authorization),
}

const TERMINAL_RETENTION: Duration = Duration::from_secs(600);

fn prune_sessions(sessions: &mut HashMap<String, Arc<Session>>) {
    let now = tokio::time::Instant::now();
    sessions.retain(|_, session| {
        session
            .finished_at
            .lock()
            .unwrap()
            .is_none_or(|finished| now.duration_since(finished) < TERMINAL_RETENTION)
    });
}

/// Dropping the last service handle cancels all pending logins.
#[derive(Clone)]
pub struct LoginService {
    inner: Arc<Inner>,
}
struct Inner {
    store: Arc<AccountStore>,
    client: reqwest::Client,
    obs: Arc<Observability>,
    runtime: Mutex<Option<Arc<AccountRuntime>>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    cancel_generation: AtomicU64,
    #[cfg(all(feature = "testkit", debug_assertions))]
    test: Option<LoginTestConfig>,
    #[cfg(all(feature = "testkit", debug_assertions))]
    device_test: Option<DeviceTestConfig>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        for session in self.sessions.get_mut().unwrap().values() {
            let _ = session.cancel.send(true);
        }
    }
}

impl LoginService {
    pub fn new(store: Arc<AccountStore>, client: reqwest::Client, obs: Arc<Observability>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                client,
                obs,
                runtime: Mutex::new(None),
                sessions: Mutex::new(HashMap::new()),
                cancel_generation: AtomicU64::new(0),
                #[cfg(all(feature = "testkit", debug_assertions))]
                test: None,
                #[cfg(all(feature = "testkit", debug_assertions))]
                device_test: None,
            }),
        }
    }

    /// Attach the live account state to clear refresh warnings after a saved login.
    /// The hook is optional for standalone/offline login services.
    pub fn with_runtime(self, runtime: Arc<AccountRuntime>) -> Self {
        *self.inner.runtime.lock().unwrap() = Some(runtime);
        self
    }

    pub async fn start(
        &self,
        provider: Provider,
        replace_linked: bool,
    ) -> Result<LoginStart, LoginError> {
        self.start_mode(provider, replace_linked, false).await
    }

    /// No callback bind: Claude paste-code; Codex device authorization.
    pub async fn start_manual(
        &self,
        provider: Provider,
        replace_linked: bool,
    ) -> Result<LoginStart, LoginError> {
        match provider {
            Provider::Claude => self.start_mode(provider, replace_linked, true).await,
            Provider::Codex => {
                let params = self.parameters(provider)?;
                self.start_device_parameters(params, replace_linked).await
            }
        }
    }

    /// Explicit Codex fallback; never requests another device code.
    pub async fn start_paste_redirect(
        &self,
        replace_linked: bool,
    ) -> Result<LoginStart, LoginError> {
        self.start_mode(Provider::Codex, replace_linked, true).await
    }

    /// Submit once; poll `status` for exchange/persistence completion. Even an
    /// invalid submission consumes the attempt. The caller owns/wipes `input`;
    /// all secret-bearing copies owned by this service are zeroized.
    pub async fn submit_manual(&self, login_id: &str, input: &str) -> Result<(), LoginError> {
        let session = self.session(login_id)?;
        session
            .manual
            .as_ref()
            .ok_or(LoginError::NotManual)?
            .submit(input, &session.status, &session.cancel)
    }

    async fn start_mode(
        &self,
        provider: Provider,
        replace_linked: bool,
        manual: bool,
    ) -> Result<LoginStart, LoginError> {
        prune_sessions(&mut self.inner.sessions.lock().unwrap());
        let params = self.parameters(provider)?;
        self.start_parameters(params, replace_linked, manual).await
    }

    fn parameters(&self, provider: Provider) -> Result<Parameters, LoginError> {
        let params = Parameters::new(provider);
        #[cfg(all(feature = "testkit", debug_assertions))]
        let params = {
            let mut params = params;
            if let Some(test) = &self.inner.test {
                params.apply_test(test)?;
            }
            if let Some(test) = &self.inner.device_test {
                params.device_code = test.usercode_url.clone();
                params.device_token = test.poll_url.clone();
                params.device_interval = Some(test.poll_interval);
                params.device_lifetime = test.lifetime;
            }
            params
        };
        Ok(params)
    }

    async fn start_device_parameters(
        &self,
        params: Parameters,
        replace_linked: bool,
    ) -> Result<LoginStart, LoginError> {
        // No task/session is published before this single request finishes.
        // Dropping start cancels the actual request, without a hidden poller.
        let generation = self.inner.cancel_generation.load(Ordering::Acquire);
        let authorization = device::request(&self.inner.client, &params).await?;
        self.start_prepared(
            params,
            replace_linked,
            true,
            Some((authorization, generation)),
        )
        .await
    }

    async fn start_parameters(
        &self,
        params: Parameters,
        replace_linked: bool,
        manual: bool,
    ) -> Result<LoginStart, LoginError> {
        self.start_prepared(params, replace_linked, manual, None)
            .await
    }

    async fn start_prepared(
        &self,
        mut params: Parameters,
        replace_linked: bool,
        manual: bool,
        device: Option<(device::Authorization, u64)>,
    ) -> Result<LoginStart, LoginError> {
        let generation = device.as_ref().map(|(_, generation)| *generation);
        let device = device.map(|(authorization, _)| authorization);
        prune_sessions(&mut self.inner.sessions.lock().unwrap());
        let provider = params.provider;
        if device.is_some() {
            params.use_device_redirect();
        } else if manual {
            params.use_manual_redirect();
        }
        let login_id = parameters::random_secret();
        let listeners = if manual {
            None
        } else {
            Some(match callback::Listeners::bind(params.port).await {
                Ok(listeners) => listeners,
                Err(error) => {
                    self.inner
                        .obs
                        .record_login(provider, &login_id, "failed", None);
                    return Err(error);
                }
            })
        };
        if let Some(listeners) = &listeners {
            params.port = listeners.port();
        }
        let state = zeroize::Zeroizing::new(parameters::random_secret());
        let verifier = zeroize::Zeroizing::new(parameters::random_secret());
        let prompt = device.as_ref().map(device::Authorization::prompt);
        let auth_url = match &prompt {
            Some(prompt) => prompt.verification_url.clone(),
            None => params.auth_url(&state, &verifier),
        };
        let pending = LoginStatus {
            status: LoginState::Pending,
            account_id: None,
            error: None,
        };
        let (status, _) = watch::channel(pending);
        let (cancel, mut cancelled) = watch::channel(false);
        let expires = device.as_ref().map_or_else(
            || tokio::time::Instant::now() + Duration::from_secs(600),
            |authorization| authorization.expires,
        );
        let (manual, input) = match (device, listeners) {
            (Some(authorization), _) => (None, LoginInput::Device(authorization)),
            (_, Some(listeners)) => (
                None,
                LoginInput::Callback(manual::Input::Browser(listeners)),
            ),
            (_, None) => {
                let (submission, receiver) = manual::Submission::new(&params, &state, expires);
                (
                    Some(submission),
                    LoginInput::Callback(manual::Input::Manual(receiver)),
                )
            }
        };
        let session = Arc::new(Session {
            status,
            cancel,
            finished_at: Mutex::new(None),
            manual,
        });
        {
            let mut sessions = self.inner.sessions.lock().unwrap();
            // Serialize initial-device publication against cancel_all. An
            // initial HTTP request cannot publish a new poller after shutdown.
            if generation.is_some_and(|generation| {
                generation != self.inner.cancel_generation.load(Ordering::Acquire)
            }) {
                return Err(LoginError::Cancelled);
            }
            sessions.insert(login_id.clone(), session.clone());
        }
        let store = self.inner.store.clone();
        let client = self.inner.client.clone();
        let obs = self.inner.obs.clone();
        let runtime = self.inner.runtime.lock().unwrap().clone();
        let task_id = login_id.clone();
        obs.record_login(provider, &login_id, "started", None);
        // The task deliberately does not hold Inner: dropping the service cancels it.
        tokio::spawn(async move {
            let deadline = async {
                if tokio::time::Instant::now() < expires {
                    tokio::time::sleep_until(expires).await;
                }
            };
            tokio::pin!(deadline);
            let outcome = {
                let flow = async {
                    let (code, verifier) = match input {
                        LoginInput::Callback(input) => {
                            (input.code(&params, &state).await?, verifier)
                        }
                        LoginInput::Device(authorization) => {
                            device::poll(&client, &params, authorization).await?
                        }
                    };
                    let (account, tokens) = tokio::time::timeout(
                        Duration::from_secs(30),
                        exchange::exchange(&client, &params, &code, &verifier, &state),
                    )
                    .await
                    .map_err(|_| LoginError::ExchangeFailed)??;
                    // No await between persistence and completion: cancel cannot report
                    // success after a committed account has been silently discarded.
                    let saved =
                        crate::accounts::login::save(&store, &account, &tokens, replace_linked)
                            .map_err(|error| match error {
                                crate::accounts::login::LoginStoreError::LinkedConflict => {
                                    LoginError::LinkedConflict
                                }
                                crate::accounts::login::LoginStoreError::IdentityConflict => {
                                    LoginError::IdentityConflict
                                }
                                _ => LoginError::StoreFailed,
                            })?;
                    if let Some(runtime) = &runtime {
                        runtime.set_refresh_attention(&saved.account.id, false);
                        runtime.clear_memory_health(&saved.account);
                    }
                    if saved.replaced_linked {
                        obs.record_login(
                            provider,
                            &task_id,
                            "replaced_linked",
                            Some(&saved.account.id),
                        );
                    }
                    Ok::<_, LoginError>(saved.account.id)
                };
                tokio::pin!(flow);
                tokio::select! {
                    biased;
                    _ = cancelled.changed() => LoginStatus { status: LoginState::Cancelled, account_id: None, error: None },
                    _ = &mut deadline => LoginStatus { status: LoginState::Expired, account_id: None, error: None },
                    result = &mut flow => match result {
                        Ok(id) => LoginStatus { status: LoginState::Completed, account_id: Some(id), error: None },
                        Err(LoginError::Expired) => LoginStatus { status: LoginState::Expired, account_id: None, error: None },
                        Err(error) => LoginStatus { status: LoginState::Failed, account_id: None, error: Some(error.to_string()) },
                    },
                }
            }; // Drop flow and all sockets before publishing a terminal status.
            obs.record_login(
                provider,
                &task_id,
                outcome.status.as_str(),
                outcome.account_id.as_deref(),
            );
            *session.finished_at.lock().unwrap() = Some(tokio::time::Instant::now());
            session.status.send_replace(outcome);
        });
        Ok(LoginStart {
            login_id,
            auth_url,
            device: prompt,
        })
    }

    pub async fn status(&self, login_id: &str) -> Result<LoginStatus, LoginError> {
        let session = self.session(login_id)?;
        let status = session.status.borrow().clone();
        Ok(status)
    }

    /// Returns only after the task has dropped the callback listeners.
    pub async fn cancel(&self, login_id: &str) -> Result<(), LoginError> {
        let session = self.session(login_id)?;
        let mut status = session.status.subscribe();
        if status.borrow().status == LoginState::Pending {
            let _ = session.cancel.send(true);
            while status.borrow().status == LoginState::Pending {
                if status.changed().await.is_err() {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Await listener release before dropping the credential lease on shutdown.
    pub async fn cancel_all(&self) {
        let sessions: Vec<_> = {
            let sessions = self.inner.sessions.lock().unwrap();
            self.inner.cancel_generation.fetch_add(1, Ordering::AcqRel);
            sessions.values().cloned().collect()
        };
        for session in &sessions {
            let _ = session.cancel.send(true);
        }
        for session in sessions {
            let mut status = session.status.subscribe();
            while status.borrow().status == LoginState::Pending {
                if status.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    fn session(&self, id: &str) -> Result<Arc<Session>, LoginError> {
        let mut sessions = self.inner.sessions.lock().unwrap();
        prune_sessions(&mut sessions);
        sessions.get(id).cloned().ok_or(LoginError::NotFound)
    }
}

#[cfg(test)]
#[path = "oauth/session_fixes_tests.rs"]
mod session_fixes_tests;

#[cfg(test)]
#[path = "oauth/manual_tests.rs"]
mod manual_tests;

#[cfg(test)]
#[path = "oauth/device_tests.rs"]
mod device_tests;

#[cfg(all(feature = "testkit", debug_assertions))]
#[derive(Clone)]
pub struct DeviceTestConfig {
    pub usercode_url: String,
    pub poll_url: String,
    pub poll_interval: Duration,
    pub lifetime: Duration,
}

#[cfg(all(feature = "testkit", debug_assertions))]
impl LoginService {
    pub fn with_device_test_config(mut self, test: DeviceTestConfig) -> Result<Self, LoginError> {
        for endpoint in [&test.usercode_url, &test.poll_url] {
            let url = reqwest::Url::parse(endpoint).map_err(|_| LoginError::InvalidTestEndpoint)?;
            let ip = url
                .host_str()
                .ok_or(LoginError::InvalidTestEndpoint)?
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .map_err(|_| LoginError::InvalidTestEndpoint)?;
            if !ip.is_loopback()
                || url.scheme() != "http"
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || matches!(url.port_or_known_default(), Some(10100 | 10101))
            {
                return Err(LoginError::InvalidTestEndpoint);
            }
        }
        if test.poll_interval.is_zero()
            || test.poll_interval > Duration::from_secs(60)
            || test.lifetime.is_zero()
            || test.lifetime > Duration::from_secs(900)
        {
            return Err(LoginError::InvalidTestEndpoint);
        }
        Arc::get_mut(&mut self.inner)
            .ok_or(LoginError::InvalidTestEndpoint)?
            .device_test = Some(test);
        Ok(self)
    }
}

#[cfg(all(feature = "testkit", debug_assertions))]
#[derive(Clone)]
pub struct LoginTestConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub profile_url: String,
    /// Zero selects a safe ephemeral port shared by IPv4 and IPv6.
    pub callback_port: u16,
}

#[cfg(all(feature = "testkit", debug_assertions))]
impl LoginService {
    pub fn with_test_config(
        store: Arc<AccountStore>,
        client: reqwest::Client,
        obs: Arc<Observability>,
        test: LoginTestConfig,
    ) -> Result<Self, LoginError> {
        let mut params = Parameters::new(Provider::Claude);
        params.apply_test(&test)?;
        let mut service = Self::new(store, client, obs);
        Arc::get_mut(&mut service.inner).unwrap().test = Some(test);
        Ok(service)
    }
}
