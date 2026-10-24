//! Codex device authorization. Raw responses deliberately never implement Debug.
use super::{DevicePrompt, LoginError, exchange, parameters::Parameters};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::time::Instant;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const VERIFY: &str = "https://auth.openai.com/codex/device";
const MAX_BODY: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct CodeReply {
    #[serde(deserialize_with = "secret")]
    device_auth_id: Zeroizing<String>,
    #[serde(alias = "usercode", deserialize_with = "secret")]
    user_code: Zeroizing<String>,
    interval: String,
    expires_in: Option<u64>,
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct Authorized {
    #[serde(deserialize_with = "secret")]
    authorization_code: Zeroizing<String>,
    #[serde(deserialize_with = "secret")]
    code_challenge: Zeroizing<String>,
    #[serde(deserialize_with = "secret")]
    code_verifier: Zeroizing<String>,
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct ErrorReply {
    error: String,
}

pub(super) struct Authorization {
    id: Zeroizing<String>,
    user_code: Zeroizing<String>,
    interval: Duration,
    pub expires: Instant,
}

impl Authorization {
    pub fn prompt(&self) -> DevicePrompt {
        DevicePrompt {
            verification_url: VERIFY.into(),
            user_code: self.user_code.to_string(),
            expires_in: self
                .expires
                .saturating_duration_since(Instant::now())
                .as_secs(),
        }
    }
}

pub(super) async fn request(
    client: &reqwest::Client,
    params: &Parameters,
) -> Result<Authorization, LoginError> {
    let started = Instant::now();
    let (status, _, bytes) = post(
        client,
        &params.device_code,
        &ClientId {
            client_id: params.client_id,
        },
    )
    .await?;
    match status {
        StatusCode::NOT_FOUND => return Err(LoginError::DeviceUnavailable),
        StatusCode::TOO_MANY_REQUESTS => return Err(LoginError::DeviceRateLimited),
        _ => {}
    }
    if !status.is_success() {
        return Err(explicit_error(&bytes).unwrap_or(LoginError::ExchangeFailed));
    }
    let reply: CodeReply =
        serde_json::from_slice(&bytes).map_err(|_| LoginError::ExchangeFailed)?;
    let interval = reply
        .interval
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0 && *value <= 900)
        .ok_or(LoginError::ExchangeFailed)?;
    if !valid_field(&reply.device_auth_id, 4096) || !valid_field(&reply.user_code, 128) {
        return Err(LoginError::ExchangeFailed);
    }
    let lifetime = reply.expires_in.unwrap_or(900).min(900);
    if lifetime == 0 {
        return Err(LoginError::Expired);
    }
    let expires = started + params.device_lifetime.min(Duration::from_secs(lifetime));
    if Instant::now() >= expires {
        return Err(LoginError::Expired);
    }
    Ok(Authorization {
        id: reply.device_auth_id.clone(),
        user_code: reply.user_code.clone(),
        interval: params
            .device_interval
            .unwrap_or(Duration::from_secs(interval)),
        expires,
    })
}

#[derive(Serialize)]
struct ClientId<'a> {
    client_id: &'a str,
}
#[derive(Serialize)]
struct Poll<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

pub(super) async fn poll(
    client: &reqwest::Client,
    params: &Parameters,
    mut authorization: Authorization,
) -> Result<(Zeroizing<String>, Zeroizing<String>), LoginError> {
    loop {
        tokio::time::sleep(authorization.interval).await;
        if Instant::now() >= authorization.expires {
            return Err(LoginError::Expired);
        }
        let (status, retry, bytes) = post(
            client,
            &params.device_token,
            &Poll {
                device_auth_id: &authorization.id,
                user_code: &authorization.user_code,
            },
        )
        .await?;
        if Instant::now() >= authorization.expires {
            return Err(LoginError::Expired);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            slow_down(&mut authorization, retry);
            continue;
        }
        if status.is_success() {
            // OAuth error envelopes may also arrive with HTTP200.
            if let Ok(error) = serde_json::from_slice::<ErrorReply>(&bytes) {
                match error.error.as_str() {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        slow_down(&mut authorization, retry);
                        continue;
                    }
                    _ => return Err(explicit_error(&bytes).unwrap_or(LoginError::ExchangeFailed)),
                }
            }
            let reply: Authorized =
                serde_json::from_slice(&bytes).map_err(|_| LoginError::ExchangeFailed)?;
            let verifier = reply.code_verifier.as_bytes();
            if !valid_field(&reply.authorization_code, 8192)
                || !(43..=128).contains(&verifier.len())
                || !verifier
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(byte))
                || reply.code_challenge.as_str() != URL_SAFE_NO_PAD.encode(Sha256::digest(verifier))
            {
                return Err(LoginError::ExchangeFailed);
            }
            return Ok((
                reply.authorization_code.clone(),
                reply.code_verifier.clone(),
            ));
        }
        if let Ok(error) = serde_json::from_slice::<ErrorReply>(&bytes) {
            match error.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    slow_down(&mut authorization, retry);
                    continue;
                }
                _ => {
                    // Explicit errors take precedence over the ambiguous403/404.
                    return Err(explicit_error(&bytes).unwrap_or(LoginError::ExchangeFailed));
                }
            }
        }
        match status {
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {} // Official pending response.
            StatusCode::TOO_MANY_REQUESTS => slow_down(&mut authorization, retry),
            _ => return Err(LoginError::ExchangeFailed),
        }
    }
}

fn slow_down(authorization: &mut Authorization, retry: Option<Duration>) {
    authorization.interval = authorization
        .interval
        .saturating_add(Duration::from_secs(5))
        .max(retry.unwrap_or_default())
        .min(Duration::from_secs(900));
}

fn explicit_error(bytes: &[u8]) -> Option<LoginError> {
    let error: ErrorReply = serde_json::from_slice(bytes).ok()?;
    match error.error.as_str() {
        "expired_token" => Some(LoginError::Expired),
        // No guessed disabled-account strings: add only with official evidence.
        _ => None,
    }
}

fn valid_field(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn secret<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Zeroizing<String>, D::Error> {
    String::deserialize(deserializer).map(Zeroizing::new)
}

async fn post<T: Serialize>(
    client: &reqwest::Client,
    endpoint: &str,
    payload: &T,
) -> Result<(StatusCode, Option<Duration>, Zeroizing<Vec<u8>>), LoginError> {
    // Reserve the bounded maximum so serializing cannot leave realloc copies.
    let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_BODY));
    serde_json::to_writer(&mut *bytes, payload).map_err(|_| LoginError::ExchangeFailed)?;
    if bytes.len() > MAX_BODY {
        return Err(LoginError::ExchangeFailed);
    }
    struct SecretBody(Zeroizing<Vec<u8>>);
    impl AsRef<[u8]> for SecretBody {
        fn as_ref(&self) -> &[u8] {
            &self.0
        }
    }
    let request = async {
        let mut response = client
            .post(endpoint)
            .header(reqwest::header::USER_AGENT, exchange::user_agent())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes::Bytes::from_owner(SecretBody(bytes)))
            .send()
            .await
            .map_err(|_| LoginError::ExchangeFailed)?;
        let status = response.status();
        let retry = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|header| header.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        let mut body = Zeroizing::new(Vec::with_capacity(MAX_BODY));
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| LoginError::ExchangeFailed)?
        {
            if chunk.len() > MAX_BODY - body.len() {
                return Err(LoginError::ExchangeFailed);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, retry, body))
    };
    tokio::time::timeout(REQUEST_TIMEOUT, request)
        .await
        .map_err(|_| LoginError::ExchangeFailed)?
}
