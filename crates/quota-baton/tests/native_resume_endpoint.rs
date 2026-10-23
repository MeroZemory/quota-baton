//! Synthetic loopback only; path inclusion keeps this transport unregistered.
//! Never format a request, bearer token, endpoint, or generic transport error.

#[path = "../src/native_resume/endpoint.rs"]
mod endpoint;

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use endpoint::{Endpoint, EndpointError};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::header::{
    AUTHORIZATION, HOST, ORIGIN, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_PROTOCOL,
};
use tokio_tungstenite::tungstenite::http::{HeaderValue, Uri};
use tokio_tungstenite::tungstenite::protocol::frame::{
    Frame,
    coding::{Data, OpCode},
};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

const LIMIT: usize = 1024 * 1024;

// `expect`/`unwrap` on external errors could print a secret-bearing request.
fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic operation failed"),
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    require(timeout(Duration::from_secs(12), future).await)
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn request(endpoint: &Endpoint) -> Request {
    let mut request = require(endpoint.url().into_client_request());
    let mut authorization: HeaderValue = require(format!("Bearer {}", endpoint.token()).parse());
    authorization.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, authorization);
    request
}

fn set_path(request: &mut Request, path: &str) {
    // Keep the ws scheme required by the real client's handshake validator.
    let mut parts = request.uri().clone().into_parts();
    parts.path_and_query = Some(require(path.parse()));
    *request.uri_mut() = require(Uri::from_parts(parts));
}

fn peer_address(request: &Request) -> SocketAddr {
    assert!(request.uri().host() == Some("127.0.0.1"));
    let port = match request.uri().port_u16() {
        Some(port) => port,
        None => panic!("missing synthetic port"),
    };
    assert!(port != 0);
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

async fn assert_listener_closed(address: SocketAddr) {
    assert!(TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn authenticated_text_ping_and_close_roundtrip() {
    bounded(async {
        let endpoint = require(Endpoint::bind().await);
        assert!(endpoint.token().len() == 64);
        assert!(!endpoint.url().contains(endpoint.token()));
        let request = request(&endpoint);
        let address = peer_address(&request);
        let server = async {
            let mut socket = require(endpoint.accept(deadline()).await);
            assert_listener_closed(address).await;
            assert!(matches!(socket.next().await, Some(Ok(Message::Ping(_)))));
            require(socket.flush().await);
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    assert!(text.as_str() == "synthetic hello");
                    require(socket.send(Message::Text(text)).await);
                }
                _ => panic!("missing synthetic text"),
            }
            assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
            assert!(matches!(
                socket.flush().await,
                Ok(()) | Err(WsError::ConnectionClosed)
            ));
        };
        let client = async {
            let stream = require(TcpStream::connect(address).await);
            let (mut socket, _) = require(client_async(request, stream).await);
            require(socket.send(Message::Ping(Vec::new().into())).await);
            assert!(matches!(socket.next().await, Some(Ok(Message::Pong(_)))));
            require(socket.send(Message::text("synthetic hello")).await);
            assert!(matches!(socket.next().await, Some(Ok(Message::Text(text)))
                if text.as_str() == "synthetic hello"));
            require(socket.close(None).await);
            assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
        };
        tokio::join!(server, client);
    })
    .await;
}

async fn rejected(mutator: impl FnOnce(&mut Request)) {
    bounded(async {
        let endpoint = require(Endpoint::bind().await);
        let mut request = request(&endpoint);
        let address = peer_address(&request);
        mutator(&mut request);
        let client = async {
            let stream = require(TcpStream::connect(address).await);
            let result = client_async(request, stream).await;
            assert!(matches!(result, Err(WsError::Http(response))
                if response.status().as_u16() == 403));
        };
        let (result, ()) = tokio::join!(endpoint.accept(deadline()), client);
        assert!(matches!(result, Err(EndpointError::Handshake)));
        assert_listener_closed(address).await;
    })
    .await;
}

#[tokio::test]
async fn wrong_path_and_query_are_rejected_without_rearming() {
    rejected(|request| {
        set_path(request, "/wrong");
    })
    .await;
    rejected(|request| {
        set_path(request, "/qb-native?extra=1");
    })
    .await;
    rejected(|request| {
        set_path(request, "/qb-native?");
    })
    .await;
}

#[tokio::test]
async fn origin_and_protocol_negotiation_are_rejected() {
    rejected(|request| {
        request.headers_mut().insert(ORIGIN, require("".parse()));
    })
    .await;
    rejected(|request| {
        request
            .headers_mut()
            .insert(ORIGIN, require("https://synthetic.invalid".parse()));
    })
    .await;
    rejected(|request| {
        request
            .headers_mut()
            .insert(SEC_WEBSOCKET_EXTENSIONS, require("permessage-deflate".parse()));
    })
    .await;
    rejected(|request| {
        request
            .headers_mut()
            .insert(SEC_WEBSOCKET_PROTOCOL, require("synthetic".parse()));
    })
    .await;
}

#[tokio::test]
async fn missing_wrong_and_duplicate_authorization_are_rejected() {
    rejected(|request| {
        request.headers_mut().remove(AUTHORIZATION);
    })
    .await;
    rejected(|request| {
        let mut wrong = match request.headers().get(AUTHORIZATION) {
            Some(value) => value.as_bytes().to_vec(),
            None => panic!("missing synthetic authorization"),
        };
        wrong[7] = if wrong[7] == b'0' { b'1' } else { b'0' };
        request
            .headers_mut()
            .insert(AUTHORIZATION, require(HeaderValue::from_bytes(&wrong)));
    })
    .await;
    rejected(|request| {
        let duplicate = match request.headers().get(AUTHORIZATION) {
            Some(value) => value.clone(),
            None => panic!("missing synthetic authorization"),
        };
        request.headers_mut().append(AUTHORIZATION, duplicate);
    })
    .await;
}

#[tokio::test]
async fn host_must_match_numeric_listener() {
    rejected(|request| {
        request
            .headers_mut()
            .insert(HOST, require("localhost:1".parse()));
    })
    .await;
    rejected(|request| {
        request
            .headers_mut()
            .insert(HOST, require("127.0.0.1:1".parse()));
    })
    .await;
}

#[tokio::test]
async fn fragmented_text_is_reassembled() {
    bounded(async {
        let endpoint = require(Endpoint::bind().await);
        let request = request(&endpoint);
        let address = peer_address(&request);
        let client = async {
            let stream = require(TcpStream::connect(address).await);
            let (mut socket, _) = require(client_async(request, stream).await);
            require(socket.send(Message::Frame(Frame::message(
                b"synthetic ".to_vec(),
                OpCode::Data(Data::Text),
                false,
            ))).await);
            require(socket.send(Message::Frame(Frame::message(
                b"fragment".to_vec(),
                OpCode::Data(Data::Continue),
                true,
            ))).await);
        };
        let server = async {
            let mut socket = require(endpoint.accept(deadline()).await);
            assert!(matches!(socket.next().await, Some(Ok(Message::Text(text)))
                if text.as_str() == "synthetic fragment"));
        };
        tokio::join!(server, client);
    })
    .await;
}

async fn rejects_large_message(fragmented: bool) {
    bounded(async {
        let endpoint = require(Endpoint::bind().await);
        let request = request(&endpoint);
        let address = peer_address(&request);
        let client = async {
            let stream = require(TcpStream::connect(address).await);
            let (mut socket, _) = require(client_async(request, stream).await);
            if fragmented {
                require(socket.send(Message::Frame(Frame::message(
                    vec![b'a'; LIMIT / 2 + 1],
                    OpCode::Data(Data::Text),
                    false,
                ))).await);
                // Server may close immediately on the aggregate-size error.
                let _ = socket.send(Message::Frame(Frame::message(
                    vec![b'a'; LIMIT / 2 + 1],
                    OpCode::Data(Data::Continue),
                    true,
                ))).await;
            } else {
                let _ = socket.send(Message::Binary(vec![0; LIMIT + 1].into())).await;
            }
        };
        let server = async {
            let mut socket = require(endpoint.accept(deadline()).await);
            assert!(matches!(socket.next().await, Some(Err(WsError::Capacity(_)))));
        };
        tokio::join!(server, client);
    })
    .await;
}

#[tokio::test]
async fn aggregate_fragmented_message_and_single_frame_are_bounded() {
    rejects_large_message(true).await;
    rejects_large_message(false).await;
}

#[tokio::test]
async fn no_peer_deadline_and_pending_accept_cancellation_release_listener() {
    bounded(async {
        let endpoint = require(Endpoint::bind().await);
        let address = peer_address(&request(&endpoint));
        let result = endpoint.accept(Instant::now() + Duration::from_millis(30)).await;
        assert!(matches!(result, Err(EndpointError::Timeout)));
        drop(require(TcpListener::bind(address).await));

        let endpoint = require(Endpoint::bind().await);
        let address = peer_address(&request(&endpoint));
        let mut accepting = Box::pin(endpoint.accept(deadline()));
        tokio::select! {
            biased;
            _ = &mut accepting => panic!("unexpected synthetic admission"),
            _ = sleep(Duration::from_millis(30)) => {}
        }
        drop(accepting);
        drop(require(TcpListener::bind(address).await));
    })
    .await;
}

#[tokio::test]
async fn cancelled_handshake_closes_socket_and_admission() {
    bounded(async {
        let endpoint = require(Endpoint::bind().await);
        let address = peer_address(&request(&endpoint));
        let mut peer = require(TcpStream::connect(address).await);
        let mut accepting = Box::pin(endpoint.accept(deadline()));
        tokio::select! {
            biased;
            _ = &mut accepting => panic!("unexpected synthetic admission"),
            _ = sleep(Duration::from_millis(30)) => {}
        }
        drop(accepting);
        let mut byte = [0];
        assert!(matches!(peer.read(&mut byte).await, Ok(0) | Err(_)));
        assert_listener_closed(address).await;
    })
    .await;
}

#[tokio::test]
async fn handshake_has_caller_and_five_second_limits() {
    bounded(async {
        for caller_limit in [Duration::from_millis(30), Duration::from_secs(10)] {
            let endpoint = require(Endpoint::bind().await);
            let address = peer_address(&request(&endpoint));
            let mut peer = require(TcpStream::connect(address).await);
            let started = Instant::now();
            let result = endpoint.accept(started + caller_limit).await;
            assert!(matches!(result, Err(EndpointError::Timeout)));
            assert!(started.elapsed() < Duration::from_secs(7));
            let mut byte = [0];
            assert!(matches!(peer.read(&mut byte).await, Ok(0) | Err(_)));
            assert_listener_closed(address).await;
        }
    })
    .await;
}

#[tokio::test]
async fn header_limit_accepts_exactly_8192_and_rejects_8193() {
    bounded(async {
        for size in [8192, 8193] {
            let endpoint = require(Endpoint::bind().await);
            let request = request(&endpoint);
            let address = peer_address(&request);
            let host = match request.headers().get(HOST) {
                Some(host) => require(host.to_str()),
                None => panic!("missing synthetic host"),
            };
            let prefix = format!(
                concat!(
                    "GET /qb-native HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\n",
                    "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n",
                    "Sec-WebSocket-Key: AAECAwQFBgcICQoLDA0ODw==\r\n",
                    "Authorization: Bearer {}\r\nX-Pad: ",
                ),
                host, endpoint.token(),
            );
            let padding = size - prefix.len() - 4;
            let wire = format!("{prefix}{}\r\n\r\n", "a".repeat(padding));
            assert!(wire.len() == size);
            let client = async {
                let mut peer = require(TcpStream::connect(address).await);
                // Split the header terminator across writes.
                require(peer.write_all(&wire.as_bytes()[..size - 1]).await);
                let _ = peer.write_all(&wire.as_bytes()[size - 1..]).await;
                let mut response = Vec::new();
                let _ = peer.read_to_end(&mut response).await;
                assert!(response.starts_with(b"HTTP/1.1 101") == (size == 8192));
            };
            let server = async {
                let result = endpoint.accept(deadline()).await;
                if size == 8192 {
                    drop(require(result));
                } else {
                    assert!(matches!(result, Err(EndpointError::Handshake)));
                }
            };
            tokio::join!(server, client);
            assert_listener_closed(address).await;
        }
    })
    .await;
}

#[tokio::test]
async fn expired_deadline_cannot_claim_a_ready_peer() {
    bounded(async {
        let endpoint = require(Endpoint::bind().await);
        let address = peer_address(&request(&endpoint));
        let _peer = require(TcpStream::connect(address).await);
        let result = endpoint.accept(Instant::now()).await;
        assert!(matches!(result, Err(EndpointError::Timeout)));
        assert_listener_closed(address).await;
    })
    .await;
}
