//! One-shot manual input. Parse borrowed bytes without a secret-bearing Url.
use super::{LoginError, LoginState, LoginStatus, callback, parameters::Parameters};
use crate::accounts::Provider;
use std::sync::Mutex;
use subtle::ConstantTimeEq;
use tokio::sync::{oneshot, watch};
use zeroize::Zeroizing;

type Code = Result<Zeroizing<String>, LoginError>;

pub(super) enum Input {
    Browser(callback::Listeners),
    Manual(oneshot::Receiver<Code>),
}

impl Input {
    pub async fn code(self, params: &Parameters, state: &str) -> Code {
        match self {
            Self::Browser(listeners) => listeners.callback(&params.path, state).await,
            Self::Manual(receiver) => receiver.await.map_err(|_| LoginError::InvalidCallback)?,
        }
    }
}

pub(super) struct Submission {
    sender: Mutex<Option<oneshot::Sender<Code>>>,
    provider: Provider,
    state: Zeroizing<String>,
    redirect: String,
    path: String,
    expires: tokio::time::Instant,
}

impl Submission {
    pub fn new(
        params: &Parameters,
        state: &str,
        expires: tokio::time::Instant,
    ) -> (Self, oneshot::Receiver<Code>) {
        let (sender, receiver) = oneshot::channel();
        (
            Self {
                sender: Mutex::new(Some(sender)),
                provider: params.provider,
                state: Zeroizing::new(state.to_owned()),
                redirect: params.redirect(),
                path: params.path.clone(),
                expires,
            },
            receiver,
        )
    }

    pub fn submit(
        &self,
        input: &str,
        status: &watch::Sender<LoginStatus>,
        cancel: &watch::Sender<bool>,
    ) -> Result<(), LoginError> {
        let mut slot = self.sender.lock().unwrap();
        if *cancel.borrow() || status.borrow().status == LoginState::Cancelled {
            return Err(LoginError::Cancelled);
        }
        if tokio::time::Instant::now() >= self.expires
            || status.borrow().status == LoginState::Expired
        {
            return Err(LoginError::Expired);
        }
        if status.borrow().status != LoginState::Pending {
            return Err(LoginError::AlreadySubmitted);
        }
        let sender = slot.take().ok_or(LoginError::AlreadySubmitted)?;
        let result = self.parse(input);
        let error = result.as_ref().err().copied();
        sender
            .send(result)
            .map_err(|_| LoginError::AlreadySubmitted)?;
        error.map_or(Ok(()), Err)
    }

    fn parse(&self, input: &str) -> Code {
        // Bound allocations and reject internal whitespace/control bytes. Only
        // terminal line endings/outer whitespace may surround a pasted value.
        if input.len() > 8192 {
            return Err(LoginError::InvalidCallback);
        }
        let input = Zeroizing::new(input.trim().to_owned());
        if input.is_empty()
            || input
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(LoginError::InvalidCallback);
        }
        match self.provider {
            Provider::Claude => {
                let (code, state) = input.split_once('#').ok_or(LoginError::InvalidCallback)?;
                if code.is_empty() || state.is_empty() || state.contains('#') {
                    return Err(LoginError::InvalidCallback);
                }
                if !bool::from(state.as_bytes().ct_eq(self.state.as_bytes())) {
                    return Err(LoginError::StateMismatch);
                }
                Ok(Zeroizing::new(code.to_owned()))
            }
            Provider::Codex => {
                // Exact registered redirect means literal localhost, exact port
                // and path, no userinfo, aliases, normalized paths or fragment.
                let query = input
                    .strip_prefix(self.redirect.as_str())
                    .and_then(|suffix| suffix.strip_prefix('?'))
                    .filter(|query| !query.contains('#'))
                    .ok_or(LoginError::InvalidCallback)?;
                let target = Zeroizing::new(format!("{}?{query}", self.path));
                callback::parse_target(&target, &self.path, &self.state)?
                    .ok_or(LoginError::InvalidCallback)
            }
        }
    }
}
