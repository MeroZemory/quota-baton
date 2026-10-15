use super::LoginError;
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use zeroize::Zeroizing;

const MAX_CALLBACK_READS: usize = 16;

pub(super) struct Listeners {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
}
impl Listeners {
    pub async fn bind(port: u16) -> Result<Self, LoginError> {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|error| bind_error(&error, port))?;
        let port = v4
            .local_addr()
            .map_err(|_| LoginError::CallbackBindFailed { port })?
            .port();
        let v6 = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
            Ok(listener) => Some(listener),
            Err(error) if ipv6_unavailable(&error) => None,
            Err(error) => return Err(bind_error(&error, port)),
        };
        Ok(Self { v4, v6, port })
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    // Consuming self ensures both listeners close before the exchange begins.
    pub async fn callback(
        self,
        path: &str,
        expected_state: &str,
    ) -> Result<Zeroizing<String>, LoginError> {
        let mut reads = JoinSet::new();
        loop {
            tokio::select! {
                value = self.v4.accept() => {
                    spawn_callback_read(&mut reads, value, path, expected_state);
                }
                value = async {
                    match &self.v6 {
                        Some(listener) => listener.accept().await,
                        None => std::future::pending().await,
                    }
                } => {
                    spawn_callback_read(&mut reads, value, path, expected_state);
                }
                result = reads.join_next(), if !reads.is_empty() => {
                    if let Some(Ok(result)) = result {
                        match result {
                            Ok(Some(code)) => return Ok(code),
                            Ok(None) => {},
                            Err(LoginError::ExchangeFailed) => return Err(LoginError::ExchangeFailed),
                            Err(_) => {},
                        }
                    }
                }
            }
        }
    }
}

fn spawn_callback_read(
    reads: &mut JoinSet<Result<Option<Zeroizing<String>>, LoginError>>,
    accepted: Result<(TcpStream, std::net::SocketAddr), std::io::Error>,
    path: &str,
    expected_state: &str,
) {
    let Ok((mut stream, peer)) = accepted else {
        return;
    };
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(%error, "failed to set TCP_NODELAY on OAuth callback socket");
    }
    if !peer.ip().is_loopback() {
        return;
    }
    if reads.len() >= MAX_CALLBACK_READS {
        return;
    }
    let path = path.to_owned();
    let expected_state = expected_state.to_owned();
    reads.spawn(async move {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            read_callback(&mut stream, &path, &expected_state),
        ).await.unwrap_or(Err(LoginError::InvalidCallback));
        let response = match &result {
            Ok(Some(_)) => "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n로그인 요청을 받았습니다. 이 창을 닫아도 됩니다.",
            _ => "HTTP/1.1 400 Bad Request\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nInvalid login callback.",
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(response.as_bytes())).await;
        result
    });
}

async fn read_callback(
    stream: &mut TcpStream,
    path: &str,
    expected_state: &str,
) -> Result<Option<Zeroizing<String>>, LoginError> {
    let mut request = Zeroizing::new(Vec::with_capacity(8192));
    loop {
        let mut chunk = Zeroizing::new([0u8; 1024]);
        let size = stream
            .read(&mut chunk[..])
            .await
            .map_err(|_| LoginError::InvalidCallback)?;
        if size == 0 || request.len() + size > 8192 {
            return Err(LoginError::InvalidCallback);
        }
        request.extend_from_slice(&chunk[..size]);
        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break;
        }
    }
    let header = std::str::from_utf8(&request).map_err(|_| LoginError::InvalidCallback)?;
    let mut line = header
        .lines()
        .next()
        .ok_or(LoginError::InvalidCallback)?
        .split_whitespace();
    if line.next() != Some("GET") {
        return Err(LoginError::InvalidCallback);
    }
    let target = line.next().ok_or(LoginError::InvalidCallback)?;
    if !target.starts_with('/') || target.starts_with("//") {
        return Err(LoginError::InvalidCallback);
    }
    parse_target(target, path, expected_state)
}

fn parse_target(
    target: &str,
    path: &str,
    expected_state: &str,
) -> Result<Option<Zeroizing<String>>, LoginError> {
    // Parse borrowed request bytes directly: Url would keep an unwiped copy
    // of the entire callback, including the authorization code.
    let (request_path, query) = target.split_once('?').unwrap_or((target, ""));
    if request_path != path {
        return Ok(None);
    }
    let mut state = None;
    let mut code = None;
    let mut provider_error = false;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode_component(key)?;
        let value = decode_component(value)?;
        match key.as_str() {
            "state" if state.is_none() => state = Some(value),
            "code" if code.is_none() => code = Some(value),
            "state" | "code" => return Err(LoginError::InvalidCallback),
            "error" => provider_error = true,
            _ => {}
        }
    }
    let state = state.ok_or(LoginError::StateMismatch)?;
    if !bool::from(state.as_bytes().ct_eq(expected_state.as_bytes())) {
        return Err(LoginError::StateMismatch);
    }
    if provider_error {
        return Err(LoginError::ExchangeFailed);
    }
    let code = code
        .filter(|value| !value.is_empty())
        .ok_or(LoginError::InvalidCallback)?;
    Ok(Some(code))
}

fn decode_component(encoded: &str) -> Result<Zeroizing<String>, LoginError> {
    let mut decoded = Zeroizing::new(Vec::with_capacity(encoded.len()));
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(match byte {
            b'+' => b' ',
            b'%' => {
                let hi = bytes.next().and_then(|b| (b as char).to_digit(16));
                let lo = bytes.next().and_then(|b| (b as char).to_digit(16));
                match (hi, lo) {
                    (Some(hi), Some(lo)) => (hi * 16 + lo) as u8,
                    _ => return Err(LoginError::InvalidCallback),
                }
            }
            byte => byte,
        });
    }
    let text = std::str::from_utf8(&decoded).map_err(|_| LoginError::InvalidCallback)?;
    Ok(Zeroizing::new(text.to_owned()))
}

fn bind_error(error: &std::io::Error, port: u16) -> LoginError {
    if error.kind() == std::io::ErrorKind::AddrInUse {
        LoginError::CallbackPortBusy { port }
    } else {
        LoginError::CallbackBindFailed { port }
    }
}

fn ipv6_unavailable(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
    ) {
        return true;
    }
    // Windows WSAEAFNOSUPPORT may map to Uncategorized.
    #[cfg(windows)]
    if error.raw_os_error() == Some(10047) {
        return true;
    }
    false
}

#[cfg(test)]
#[path = "callback_fixes_tests.rs"]
mod fixes_tests;
