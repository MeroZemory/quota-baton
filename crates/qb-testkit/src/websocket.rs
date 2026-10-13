//! 로컬 테스트용 WebSocket 업스트림과 핸드셰이크/메시지 기록.

use crate::RecordedRequest;
use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::VecDeque,
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpListener,
    sync::mpsc,
    task::JoinSet,
};
pub use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    accept_hdr_async_with_config,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        protocol::WebSocketConfig,
    },
};

pub const MAX_MESSAGE_SIZE: usize = 192 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum ConnectionEvent {
    Opened(u64),
    Closed(u64),
    Error(u64, String),
}

struct HandshakeFailure {
    status: StatusCode,
    body: String,
    headers: HeaderMap,
    body_delay: Duration,
}

struct Inner {
    requests: Vec<RecordedRequest>,
    messages: Vec<Message>,
    events: Vec<ConnectionEvent>,
    handshakes: VecDeque<HandshakeFailure>,
    queued: VecDeque<Message>,
    replies: VecDeque<Vec<Message>>,
    active: Option<(u64, mpsc::UnboundedSender<Message>)>,
    next_id: u64,
    echo: bool,
    success_headers: HeaderMap,
}

#[derive(Clone)]
pub struct WebSocketState(Arc<Mutex<Inner>>);

impl Default for WebSocketState {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Inner {
            requests: Vec::new(),
            messages: Vec::new(),
            events: Vec::new(),
            handshakes: VecDeque::new(),
            queued: VecDeque::new(),
            replies: VecDeque::new(),
            active: None,
            next_id: 0,
            echo: true,
            success_headers: HeaderMap::new(),
        })))
    }
}

impl WebSocketState {
    /// 다음 핸드셰이크를 지정한 HTTP 응답으로 거절한다.
    pub fn push_handshake(&self, status: StatusCode, body: impl Into<String>, headers: HeaderMap) {
        self.push_handshake_delayed(status, body, headers, Duration::ZERO);
    }

    /// 거절 응답 헤더를 보낸 뒤 본문 전송을 지연한다.
    pub fn push_handshake_delayed(
        &self,
        status: StatusCode,
        body: impl Into<String>,
        headers: HeaderMap,
        body_delay: Duration,
    ) {
        self.0
            .lock()
            .unwrap()
            .handshakes
            .push_back(HandshakeFailure {
                status,
                body: body.into(),
                headers,
                body_delay,
            });
    }

    /// 협상 헤더를 건드리지 않고 shape 캡처 허용 목록만 설정한다(최대 32개/8 KiB).
    pub fn set_success_headers(&self, headers: HeaderMap) {
        let mut allowed = HeaderMap::new();
        let mut bytes = 0;
        for (name, value) in &headers {
            let name_text = name.as_str();
            if !(name_text.starts_with("x-codex-")
                || name_text.starts_with("openai-")
                || name_text == "x-request-id")
            {
                continue;
            }
            let size = name_text.len() + value.as_bytes().len();
            if allowed.len() >= 32 || bytes + size > 8192 {
                continue;
            }
            bytes += size;
            allowed.append(name.clone(), value.clone());
        }
        self.0.lock().unwrap().success_headers = allowed;
    }

    /// 가장 최근에 열린 연결로 보낸다. 연결이 없으면 오류를 반환한다.
    pub fn send(&self, message: Message) -> io::Result<()> {
        let inner = self.0.lock().unwrap();
        let (_, sender) = inner.active.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "no active fake WebSocket")
        })?;
        sender
            .send(message)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "fake WebSocket closed"))
    }

    /// 다음 성공한 연결이 열리면 순서대로 보낸다.
    pub fn queue_send(&self, message: Message) {
        self.0.lock().unwrap().queued.push_back(message);
    }

    /// 다음 text/binary 수신 시 echo 대신 지정한 이벤트들을 순서대로 보낸다.
    pub fn push_reply(&self, messages: impl IntoIterator<Item = Message>) {
        self.0
            .lock()
            .unwrap()
            .replies
            .push_back(messages.into_iter().collect());
    }

    pub fn set_echo(&self, enabled: bool) {
        self.0.lock().unwrap().echo = enabled;
    }
    pub fn messages(&self) -> Vec<Message> {
        self.0.lock().unwrap().messages.clone()
    }
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.0.lock().unwrap().requests.clone()
    }
    pub fn events(&self) -> Vec<ConnectionEvent> {
        self.0.lock().unwrap().events.clone()
    }
}

pub struct WebSocketUpstream {
    pub url: String,
    pub state: WebSocketState,
    task: tokio::task::JoinHandle<()>,
}

impl WebSocketUpstream {
    pub async fn start() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let state = WebSocketState::default();
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        connections.spawn(serve_socket(stream, shared.clone()));
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { url, state, task })
    }
}

impl Drop for WebSocketUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// tungstenite 콜백의 공개 시그니처가 큰 HTTP 오류 응답을 요구한다.
#[allow(clippy::result_large_err)]
async fn serve_socket(stream: tokio::net::TcpStream, state: WebSocketState) {
    let callback_state = state.clone();
    let delay = Arc::new(Mutex::new(Duration::ZERO));
    let callback_delay = delay.clone();
    let callback =
        move |request: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
            let mut inner = callback_state.0.lock().unwrap();
            inner.requests.push(RecordedRequest {
                method: request.method().clone(),
                path_and_query: request
                    .uri()
                    .path_and_query()
                    .map_or("/", |path| path.as_str())
                    .into(),
                headers: request.headers().clone(),
                body: Bytes::new(),
                received_at: std::time::Instant::now(),
            });
            if let Some(failure) = inner.handshakes.pop_front() {
                *callback_delay.lock().unwrap() = failure.body_delay;
                let mut response = ErrorResponse::new(Some(failure.body));
                *response.status_mut() = failure.status;
                *response.headers_mut() = failure.headers;
                if !response.headers().contains_key("content-length") {
                    let length = response.body().as_ref().map_or(0, String::len);
                    response
                        .headers_mut()
                        .insert("content-length", length.into());
                }
                Err(response)
            } else {
                for (name, value) in &inner.success_headers {
                    response.headers_mut().append(name.clone(), value.clone());
                }
                Ok(response)
            }
        };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_SIZE))
        .max_frame_size(Some(MAX_MESSAGE_SIZE));
    let stream = HandshakeStream {
        stream,
        delay,
        header_tail: Vec::new(),
        headers_sent: false,
        sleep: None,
    };
    let Ok(mut socket) = accept_hdr_async_with_config(stream, callback, Some(config)).await else {
        return;
    };
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let id = {
        let mut inner = state.0.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        for message in inner.queued.drain(..) {
            let _ = sender.send(message);
        }
        inner.active = Some((id, sender));
        inner.events.push(ConnectionEvent::Opened(id));
        id
    };
    loop {
        let result = tokio::select! {
            outgoing = receiver.recv() => {
                match outgoing {
                    Some(message) => socket.send(message).await,
                    None => break,
                }
            }
            incoming = socket.next() => {
                match incoming {
                    Some(Ok(message)) => {
                        let (echo, replies) = {
                            let mut inner = state.0.lock().unwrap();
                            inner.messages.push(message.clone());
                            let replies = if matches!(message, Message::Text(_) | Message::Binary(_)) {
                                inner.replies.pop_front()
                            } else { None };
                            (inner.echo, replies)
                        };
                        match (message, replies) {
                            (Message::Text(_) | Message::Binary(_), Some(replies)) => {
                                async {
                                    for reply in replies { socket.send(reply).await?; }
                                    Ok(())
                                }.await
                            }
                            (message @ (Message::Text(_) | Message::Binary(_)), None) if echo => socket.send(message).await,
                            // tungstenite가 큐에 넣은 pong/close만 내보내 중복 응답과 pong 루프를 피한다.
                            (Message::Ping(_), _) => socket.flush().await,
                            (Message::Close(_), _) => { let _ = socket.flush().await; break; }
                            _ => Ok(()),
                        }
                    }
                    Some(Err(error)) => Err(error),
                    None => break,
                }
            }
        };
        if let Err(error) = result {
            state
                .0
                .lock()
                .unwrap()
                .events
                .push(ConnectionEvent::Error(id, error.to_string()));
            break;
        }
    }
    let mut inner = state.0.lock().unwrap();
    if inner
        .active
        .as_ref()
        .is_some_and(|(active_id, _)| *active_id == id)
    {
        inner.active = None;
    }
    inner.events.push(ConnectionEvent::Closed(id));
}

// 최초 HTTP 헤더 경계에서만 쓰기를 나누어, 본문이 별도 패킷으로 늦게 도착하도록 한다.
struct HandshakeStream {
    stream: tokio::net::TcpStream,
    delay: Arc<Mutex<Duration>>,
    header_tail: Vec<u8>,
    headers_sent: bool,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl AsyncRead for HandshakeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for HandshakeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(sleep) = &mut self.sleep {
            if sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.sleep = None;
        }
        let mut limit = buf.len();
        if !self.headers_sent {
            let prefix = self.header_tail.len();
            // 경계 검사는 헤더 쓰기 중에만 수행한다.
            let combined: Vec<_> = self
                .header_tail
                .iter()
                .copied()
                .chain(buf.iter().copied())
                .collect();
            if let Some(end) = combined.windows(4).position(|window| window == b"\r\n\r\n") {
                limit = end + 4 - prefix;
            }
        }
        match Pin::new(&mut self.stream).poll_write(cx, &buf[..limit]) {
            Poll::Ready(Ok(written)) => {
                if !self.headers_sent {
                    self.header_tail.extend_from_slice(&buf[..written]);
                    if self.header_tail.ends_with(b"\r\n\r\n") {
                        self.headers_sent = true;
                        let delay = *self.delay.lock().unwrap();
                        if !delay.is_zero() {
                            self.sleep = Some(Box::pin(tokio::time::sleep(delay)));
                        }
                    }
                    let keep_from = self.header_tail.len().saturating_sub(3);
                    self.header_tail.drain(..keep_from);
                }
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio_tungstenite::{
        connect_async, connect_async_with_config,
        tungstenite::{Error, client::IntoClientRequest},
    };

    #[tokio::test]
    async fn handshake_failure_is_queued_and_recorded() {
        let upstream = WebSocketUpstream::start().await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "fake-id".parse().unwrap());
        upstream
            .state
            .push_handshake(StatusCode::TOO_MANY_REQUESTS, "fake limit", headers);
        let url = format!(
            "{}/responses?test=1",
            upstream.url.replacen("http", "ws", 1)
        );
        let mut request = url.clone().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("x-test", "record-me".parse().unwrap());
        let error = connect_async(request).await.unwrap_err();
        let Error::Http(response) = error else {
            panic!("expected HTTP failure")
        };
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["x-request-id"], "fake-id");
        assert_eq!(response.body().as_deref(), Some(b"fake limit".as_slice()));
        let recorded = upstream.state.requests();
        assert_eq!(recorded[0].path_and_query, "/responses?test=1");
        assert_eq!(recorded[0].headers["x-test"], "record-me");
        let mut success_headers = HeaderMap::new();
        success_headers.insert("x-codex-test", "codex-value".parse().unwrap());
        success_headers.insert("openai-test", "openai-value".parse().unwrap());
        success_headers.insert("x-request-id", "success-id".parse().unwrap());
        success_headers.insert("sec-websocket-accept", "invalid-override".parse().unwrap());
        success_headers.insert(
            "sec-websocket-extensions",
            "permessage-deflate".parse().unwrap(),
        );
        success_headers.insert("x-unlisted", "skip".parse().unwrap());
        upstream.state.set_success_headers(success_headers);
        let (mut client, response) = connect_async(url).await.unwrap();
        assert_eq!(response.headers()["x-codex-test"], "codex-value");
        assert_eq!(response.headers()["openai-test"], "openai-value");
        assert_eq!(response.headers()["x-request-id"], "success-id");
        assert_ne!(
            response.headers()["sec-websocket-accept"],
            "invalid-override"
        );
        assert!(!response.headers().contains_key("sec-websocket-extensions"));
        assert!(!response.headers().contains_key("x-unlisted"));
        client
            .send(Message::text("response.interrupt"))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            Message::text("response.interrupt")
        );
        let complete = Message::text(r#"{"type":"response.completed"}"#);
        upstream.state.push_reply([complete.clone()]);
        client
            .send(Message::text(r#"{"type":"response.create"}"#))
            .await
            .unwrap();
        assert_eq!(client.next().await.unwrap().unwrap(), complete);
        client.close(None).await.unwrap();

        upstream.state.push_handshake_delayed(
            StatusCode::BAD_REQUEST,
            "full delayed rejection body",
            HeaderMap::new(),
            Duration::from_millis(300),
        );
        let response = reqwest::Client::new()
            .get(format!("{}/responses", upstream.url))
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.bytes();
        tokio::pin!(body);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut body)
                .await
                .is_err()
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), body)
                .await
                .unwrap()
                .unwrap(),
            "full delayed rejection body"
        );
    }

    #[tokio::test]
    async fn scripted_events_and_control_frames_do_not_loop() {
        let upstream = WebSocketUpstream::start().await.unwrap();
        upstream.state.set_echo(false);
        let complete = Message::text(r#"{"type":"response.completed","response":{"id":"fake"}}"#);
        upstream.state.queue_send(complete.clone());
        let (mut client, _) = connect_async(upstream.url.replacen("http", "ws", 1))
            .await
            .unwrap();
        assert_eq!(client.next().await.unwrap().unwrap(), complete);
        upstream
            .state
            .send(Message::Ping(Bytes::from_static(b"server-ping")))
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            Message::Ping(Bytes::from_static(b"server-ping"))
        );
        // 읽는 쪽이 큐에 넣은 pong을 실제 소켓으로 내보낸다.
        client.flush().await.unwrap();
        client
            .send(Message::Ping(Bytes::from_static(b"client-ping")))
            .await
            .unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            Message::Pong(Bytes::from_static(b"client-ping"))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), client.next())
                .await
                .is_err()
        );
        assert!(
            upstream
                .state
                .messages()
                .iter()
                .any(|message| matches!(message, Message::Pong(_)))
        );
        upstream.state.send(Message::Close(None)).unwrap();
        assert_eq!(client.next().await.unwrap().unwrap(), Message::Close(None));
        client.flush().await.unwrap();
    }

    #[tokio::test]
    async fn single_forty_mib_frames_echo_in_both_directions() {
        let upstream = WebSocketUpstream::start().await.unwrap();
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_SIZE))
            .max_frame_size(Some(MAX_MESSAGE_SIZE));
        let (mut client, _) =
            connect_async_with_config(upstream.url.replacen("http", "ws", 1), Some(config), false)
                .await
                .unwrap();
        // send는 하나의 메시지를 단일 프레임으로 내보낸다. text와 binary를 모두 검증한다.
        for message in [
            Message::Text("x".repeat(40 * 1024 * 1024).into()),
            Message::Binary(vec![0xa5; 40 * 1024 * 1024].into()),
        ] {
            client.send(message.clone()).await.unwrap();
            let echoed = tokio::time::timeout(Duration::from_secs(30), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(echoed, message);
        }
        assert_eq!(upstream.state.messages().len(), 2);
        client.close(None).await.unwrap();
    }
}
