use super::LoginError;
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use zeroize::Zeroizing;

pub(super) struct Listeners {
    v4: TcpListener,
    v6: TcpListener,
    port: u16,
}
impl Listeners {
    pub async fn bind(port: u16) -> Result<Self, LoginError> {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|_| LoginError::CallbackPortBusy { port })?;
        let port = v4
            .local_addr()
            .map_err(|_| LoginError::CallbackPortBusy { port })?
            .port();
        let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port))
            .await
            .map_err(|_| LoginError::CallbackPortBusy { port })?;
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
        loop {
            let accepted = tokio::select! {
                value = self.v4.accept() => value,
                value = self.v6.accept() => value,
            }
            .map_err(|_| LoginError::InvalidCallback)?;
            let (mut stream, peer) = accepted;
            if !peer.ip().is_loopback() {
                continue;
            }
            let parsed = tokio::time::timeout(
                Duration::from_secs(5),
                read_callback(&mut stream, path, expected_state),
            )
            .await;
            let result = match parsed {
                Ok(result) => result,
                Err(_) => {
                    continue;
                }
            };
            let response = match &result {
                Ok(Some(_)) => {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n로그인 요청을 받았습니다. 이 창을 닫아도 됩니다."
                }
                _ => {
                    "HTTP/1.1 400 Bad Request\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nInvalid login callback."
                }
            };
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                stream.write_all(response.as_bytes()),
            )
            .await;
            match result {
                Ok(Some(code)) => return Ok(code),
                Ok(None) => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

async fn read_callback(
    stream: &mut TcpStream,
    path: &str,
    expected_state: &str,
) -> Result<Option<Zeroizing<String>>, LoginError> {
    let mut request = Zeroizing::new(Vec::new());
    loop {
        let mut chunk = [0u8; 1024];
        let size = stream
            .read(&mut chunk)
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
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| LoginError::InvalidCallback)?;
    if url.path() != path {
        return Ok(None);
    }
    let mut state = None;
    let mut code = None;
    let mut provider_error = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" if state.is_none() => state = Some(Zeroizing::new(value.into_owned())),
            "code" if code.is_none() => code = Some(Zeroizing::new(value.into_owned())),
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
