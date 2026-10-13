//! Owned OAuth login. This module never opens a browser or logs callback URLs.
mod callback;
mod exchange;
mod parameters;

use crate::account_runtime::AccountRuntime;
use crate::accounts::{AccountStore, Provider};
use crate::observability::Observability;
use parameters::Parameters;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginStart {
    pub login_id: String,
    pub auth_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    Pending,
    Completed,
    Failed,
    Cancelled,
    Expired,
}

impl LoginState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginStatus {
    pub status: LoginState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

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
    #[cfg(all(feature = "testkit", debug_assertions))]
    test: Option<LoginTestConfig>,
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
                #[cfg(all(feature = "testkit", debug_assertions))]
                test: None,
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
        prune_sessions(&mut self.inner.sessions.lock().unwrap());
        let mut params = Parameters::new(provider);
        #[cfg(all(feature = "testkit", debug_assertions))]
        if let Some(test) = &self.inner.test {
            params.apply_test(test)?;
        }
        let login_id = parameters::random_secret();
        let listeners = match callback::Listeners::bind(params.port).await {
            Ok(listeners) => listeners,
            Err(error) => {
                self.inner
                    .obs
                    .record_login(provider, &login_id, "failed", None);
                return Err(error);
            }
        };
        params.port = listeners.port();
        let state = parameters::random_secret();
        let verifier = zeroize::Zeroizing::new(parameters::random_secret());
        let auth_url = params.auth_url(&state, &verifier);
        let pending = LoginStatus {
            status: LoginState::Pending,
            account_id: None,
            error: None,
        };
        let (status, _) = watch::channel(pending);
        let (cancel, mut cancelled) = watch::channel(false);
        let session = Arc::new(Session {
            status,
            cancel,
            finished_at: Mutex::new(None),
        });
        self.inner
            .sessions
            .lock()
            .unwrap()
            .insert(login_id.clone(), session.clone());
        let store = self.inner.store.clone();
        let client = self.inner.client.clone();
        let obs = self.inner.obs.clone();
        let runtime = self.inner.runtime.lock().unwrap().clone();
        let task_id = login_id.clone();
        let expires = tokio::time::Instant::now() + Duration::from_secs(600);
        obs.record_login(provider, &login_id, "started", None);
        // The task deliberately does not hold Inner: dropping the service cancels it.
        tokio::spawn(async move {
            let deadline = tokio::time::sleep_until(expires);
            tokio::pin!(deadline);
            let outcome = {
                let flow = async {
                    let code = listeners.callback(&params.path, &state).await?;
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
        Ok(LoginStart { login_id, auth_url })
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
        let sessions: Vec<_> = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
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
