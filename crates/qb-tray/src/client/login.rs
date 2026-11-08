//! Facts for the native login view. Neither viewing nor copying starts a session.
use super::{ClientError, LoginStart};

#[cfg(all(debug_assertions, feature = "testkit"))]
#[path = "login/fixture_urls.rs"]
mod fixture_urls;
#[cfg(not(all(debug_assertions, feature = "testkit")))]
#[path = "login/production_urls.rs"]
mod fixture_urls;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMethod {
    Web,
    Device,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginFailure {
    Busy,
    RateLimited,
    Unavailable,
    Unknown,
}

pub(super) const MAX_START_BODY: usize = 16 * 1024;

/// A safe session handle survives invalid nested presentation facts. No URL or
/// code from a malformed reply is ever returned to the shell.
pub fn known_session(error: &ClientError) -> Option<&str> {
    match error {
        ClientError::LoginStartUnresolved { login_id } => login_id.as_deref(),
        _ => None,
    }
}

pub(super) fn decode_start(
    provider: &str,
    method: LoginMethod,
    bytes: &[u8],
) -> Result<LoginStart, ClientError> {
    #[derive(serde::Deserialize)]
    struct Identity {
        login_id: Option<String>,
    }
    let id = serde_json::from_slice::<Identity>(bytes)
        .ok()
        .and_then(|value| value.login_id)
        .filter(|id| valid_login_id(id));
    match serde_json::from_slice::<LoginStart>(bytes) {
        Ok(start) if validate_start(provider, method, &start).is_ok() => Ok(start),
        _ => Err(ClientError::LoginStartUnresolved { login_id: id }),
    }
}

pub fn start_error(error: &ClientError) -> LoginFailure {
    match error {
        ClientError::Http(reqwest::StatusCode::CONFLICT) => LoginFailure::Busy,
        ClientError::Http(reqwest::StatusCode::TOO_MANY_REQUESTS) => LoginFailure::RateLimited,
        ClientError::Http(
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::SERVICE_UNAVAILABLE,
        ) => LoginFailure::Unavailable,
        _ => LoginFailure::Unknown,
    }
}

pub(super) fn request_body(provider: &str, method: LoginMethod) -> Option<serde_json::Value> {
    match (provider, method) {
        ("codex" | "claude", LoginMethod::Web) => Some(serde_json::json!({"provider":provider})),
        ("codex", LoginMethod::Device) => {
            Some(serde_json::json!({"provider":provider,"no_browser":true}))
        }
        _ => None,
    }
}

pub fn valid_login_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn trusted_url(provider: &str, method: LoginMethod, value: &str) -> bool {
    if value.len() > 8192
        || value.trim() != value
        || value.bytes().any(|b| b.is_ascii_control() || b == b'\\')
    {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if fixture_urls::allowed(provider, method, value) {
        return true;
    }
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    match (provider, method) {
        ("codex", LoginMethod::Device) => value == "https://auth.openai.com/codex/device",
        ("codex", LoginMethod::Web) => {
            url.host_str() == Some("auth.openai.com") && url.path() == "/oauth/authorize"
        }
        ("claude", LoginMethod::Web) => {
            url.host_str() == Some("claude.ai") && url.path() == "/oauth/authorize"
        }
        _ => false,
    }
}

pub fn validate_start(
    provider: &str,
    method: LoginMethod,
    start: &LoginStart,
) -> Result<(), &'static str> {
    if !valid_login_id(&start.login_id) || !trusted_url(provider, method, &start.auth_url) {
        return Err("invalid_start");
    }
    match (provider, method, start.device.as_ref()) {
        ("codex", LoginMethod::Device, Some(device))
            if trusted_url(provider, method, &device.verification_url)
                && device.verification_url == start.auth_url
                && !device.user_code.is_empty()
                && device.user_code.len() <= 128
                && device.user_code.bytes().all(|b| b.is_ascii_graphic())
                && (1..=900).contains(&device.expires_in) =>
        {
            Ok(())
        }
        ("codex" | "claude", LoginMethod::Web, None) => Ok(()),
        _ => Err("invalid_start"),
    }
}

#[cfg(test)]
#[path = "login/tests.rs"]
mod tests;
