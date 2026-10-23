//! Single-use, local WebSocket transport; deliberately unregistered.
//!
//! The URL is nonsecret; the separate bearer token is a transport capability,
//! not proof of process identity or goal/core permission. Put the token only in
//! the intended child's environment via native `--remote-auth-token-env`, never
//! argv, URLs, files, or logs. Before releasing admission, the parent must own
//! this endpoint and both child jobs. The parent remains responsible for child
//! identity, cancellation, process cleanup, and all goal/core authorization.
//!
//! The first TCP connection consumes admission, even if its handshake fails.
//! A local process can therefore deny this run; it cannot rearm or claim it.
//! Dropping the endpoint or its `accept` future closes the listener/socket;
//! there is no detached accept task or retry loop.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use rand::{TryRngCore, rngs::OsRng};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{
    StatusCode,
    header::{AUTHORIZATION, HOST, ORIGIN, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_PROTOCOL},
};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{WebSocketStream, accept_hdr_async_with_config};
use zeroize::{Zeroize, Zeroizing};

const MAX_BYTES: usize = 1024 * 1024;
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(5);
const MAX_HEADER_BYTES: usize = 8 * 1024;
const PATH: &str = "/qb-native";

/// Fixed diagnostics: underlying errors may contain secret handshake headers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointError {
    Entropy,
    Bind,
    Accept,
    Handshake,
    Timeout,
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Entropy => "endpoint-entropy",
            Self::Bind => "endpoint-bind",
            Self::Accept => "endpoint-accept",
            Self::Handshake => "endpoint-handshake",
            Self::Timeout => "endpoint-timeout",
        })
    }
}

impl std::error::Error for EndpointError {}

/// Intentionally neither `Debug` nor `Clone`: contains a single-use secret.
pub struct Endpoint {
    listener: TcpListener,
    port: u16,
    token: Zeroizing<String>,
}

impl Endpoint {
    pub async fn bind() -> Result<Self, EndpointError> {
        let mut token = Zeroizing::new([0_u8; 32]);
        OsRng
            .try_fill_bytes(&mut *token)
            .map_err(|_| EndpointError::Entropy)?;
        let mut encoded = Zeroizing::new(String::with_capacity(64));
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in token.iter().copied() {
            encoded.push(HEX[usize::from(byte >> 4)] as char);
            encoded.push(HEX[usize::from(byte & 15)] as char);
        }
        token.zeroize();
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| EndpointError::Bind)?;
        let port = listener
            .local_addr()
            .map_err(|_| EndpointError::Bind)?
            .port();
        Ok(Self {
            listener,
            port,
            token: encoded,
        })
    }

    /// Nonsecret URL for the intended child's `--remote` argument.
    pub fn url(&self) -> String {
        format!("ws://127.0.0.1:{}{PATH}", self.port)
    }

    /// Secret for a fresh environment variable in the intended child only.
    /// Do not inherit that variable into the server child or log this value.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Claims at most one connection, bounded by the caller's absolute deadline.
    /// The handshake additionally has a five-second limit. Cancel by dropping
    /// this future; never detach it from the owning parent's cancellation scope.
    pub async fn accept(
        self,
        deadline: Instant,
    ) -> Result<WebSocketStream<EndpointIo>, EndpointError> {
        if Instant::now() >= deadline {
            return Err(EndpointError::Timeout);
        }
        let Self {
            listener,
            port,
            token,
        } = self;
        let connection = timeout_at(deadline, listener.accept()).await;
        // Close admission before processing even the first handshake.
        drop(listener);
        let (stream, _) = connection
            .map_err(|_| EndpointError::Timeout)?
            .map_err(|_| EndpointError::Accept)?;
        stream
            .set_nodelay(true)
            .map_err(|_| EndpointError::Accept)?;
        let handshake_deadline = deadline.min(Instant::now() + HANDSHAKE_LIMIT);
        if Instant::now() >= handshake_deadline {
            return Err(EndpointError::Timeout);
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_BYTES))
            .max_frame_size(Some(MAX_BYTES))
            .max_write_buffer_size(MAX_BYTES + 128 * 1024);
        let expected_host = format!("127.0.0.1:{port}");
        let mut expected_auth = Zeroizing::new(String::with_capacity(71));
        expected_auth.push_str("Bearer ");
        expected_auth.push_str(&token);
        drop(token);
        // Tungstenite requires an unboxed HTTP rejection response here.
        #[allow(clippy::result_large_err)]
        let check = move |request: &Request, response: Response| {
            let uri = request.uri();
            let headers = request.headers();
            let mut authorization = headers.get_all(AUTHORIZATION).iter();
            let auth_matches = authorization.next().is_some_and(|value| {
                let bytes = value.as_bytes();
                // Fixed-length comparison visits every capability byte, without
                // a data-dependent early return. This is not user isolation.
                bytes.len() == expected_auth.len()
                    && bytes
                        .iter()
                        .zip(expected_auth.as_bytes())
                        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
                        == 0
            }) && authorization.next().is_none();
            let mut hosts = headers.get_all(HOST).iter();
            let host_matches = hosts
                .next()
                .is_some_and(|value| value.as_bytes() == expected_host.as_bytes())
                && hosts.next().is_none();
            if uri.scheme().is_none()
                && uri.authority().is_none()
                && uri.path() == PATH
                && uri.query().is_none()
                && auth_matches
                && host_matches
                && !headers.contains_key(ORIGIN)
                && !headers.contains_key(SEC_WEBSOCKET_EXTENSIONS)
                && !headers.contains_key(SEC_WEBSOCKET_PROTOCOL)
            {
                Ok(response)
            } else {
                let mut rejection = ErrorResponse::new(None);
                *rejection.status_mut() = StatusCode::FORBIDDEN;
                Err(rejection)
            }
        };
        let websocket = timeout_at(
            handshake_deadline,
            accept_hdr_async_with_config(EndpointIo::new(stream), check, Some(config)),
        )
        .await
        .map_err(|_| EndpointError::Timeout)?
        .map_err(|_| EndpointError::Handshake)?;
        if !websocket.get_ref().header_complete {
            return Err(EndpointError::Handshake);
        }
        // A ready future must not return a claim after its deadline expired.
        if Instant::now() >= handshake_deadline {
            return Err(EndpointError::Timeout);
        }
        Ok(websocket)
    }
}

/// Owned socket with an 8 KiB HTTP-header read limit, including the terminator.
/// No buffered authorization bytes or `Debug` implementation are retained here.
/// After the header, tungstenite handles all WebSocket framing and limits.
pub struct EndpointIo {
    stream: TcpStream,
    header_bytes: usize,
    suffix: u32,
    header_complete: bool,
}

impl EndpointIo {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            header_bytes: 0,
            suffix: 0,
            header_complete: false,
        }
    }
}

impl AsyncRead for EndpointIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.header_complete {
            return Pin::new(&mut self.stream).poll_read(cx, output);
        }
        let available = (MAX_HEADER_BYTES - self.header_bytes).min(output.remaining());
        if available == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "endpoint-header-limit",
            )));
        }
        let mut scratch = Zeroizing::new([0_u8; MAX_HEADER_BYTES]);
        let mut input = ReadBuf::new(&mut scratch[..available]);
        match Pin::new(&mut self.stream).poll_read(cx, &mut input) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                for byte in input.filled() {
                    self.header_bytes += 1;
                    self.suffix = (self.suffix << 8) | u32::from(*byte);
                    if self.suffix == 0x0d0a0d0a {
                        self.header_complete = true;
                        self.suffix = 0;
                        break;
                    }
                }
                output.put_slice(input.filled());
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl AsyncWrite for EndpointIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
