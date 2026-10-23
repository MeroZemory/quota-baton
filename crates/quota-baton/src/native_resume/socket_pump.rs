//! Owned, authenticated WebSocket <-> bounded frame queues; no authority or I/O
//! to native stdin. Staged/unregistered alongside endpoint and dispatcher.
//!
//! Parent retains BOTH jobs, admission and catalog, and drives this future with
//! its serialized dispatcher. It must disconnect the dispatcher on ANY pump exit,
//! discard buffered frames, terminate jobs and join workers before release. Native
//! input that may activate a goal MUST use Dispatcher::native_frame_admitted with
//! a fresh write-time core callback; its default native_frame path denies it.
//!
//! No tasks are spawned here. Dropping `run` cancels both halves and closes the
//! owned socket; if parent chooses to spawn it, parent must retain/abort/join that
//! task. No reconnect, retries, timers granting permission, or fabricated RPCs.
//! Channel frames exclude LF: the parent's sync FrameWriter adapter must remove
//! ONLY its delimiter, not forward the delimiter into this text-only transport.

use std::fmt;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use super::{endpoint::EndpointIo, framing::MAX_FRAME_BYTES};

pub(crate) const QUEUE_FRAMES: usize = 8;
const IO_DEADLINE: Duration = Duration::from_secs(5);

/// Fixed categories, never tungstenite/parser errors or payload content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    FrameBound,
    InvalidFrame,
    Binary,
    WebSocket,
    PeerClosed,
    ParentClosed,
    QueueFull,
    BackpressureTimeout,
    SendTimeout,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FrameBound => "socket-pump-frame-bound",
            Self::InvalidFrame => "socket-pump-invalid-frame",
            Self::Binary => "socket-pump-binary",
            Self::WebSocket => "socket-pump-websocket",
            Self::PeerClosed => "socket-pump-peer-closed",
            Self::ParentClosed => "socket-pump-parent-closed",
            Self::QueueFull => "socket-pump-queue-full",
            Self::BackpressureTimeout => "socket-pump-backpressure-timeout",
            Self::SendTimeout => "socket-pump-send-timeout",
        })
    }
}

impl std::error::Error for Error {}

/// Validated UTF-8 JSON object bytes, unchanged. No Debug/Clone/payload logging.
/// RPC shape, correlation and ownership remain the dispatcher's responsibility.
pub(crate) struct Frame(Vec<u8>);

impl Frame {
    pub(crate) fn new(bytes: Vec<u8>) -> Result<Self, Error> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(Error::FrameBound);
        }
        if bytes.is_empty() || bytes.iter().any(|b| matches!(*b, b'\r' | b'\n')) {
            return Err(Error::InvalidFrame);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| Error::InvalidFrame)?;
        if !value.is_object() {
            return Err(Error::InvalidFrame);
        }
        Ok(Self(bytes))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Parent consumes UI frames serially; only dispatcher-approved UI output enters
/// `to_ui`. Channel success means queued, NOT physically delivered/acknowledged.
/// Synchronous adapters use `try_send`; parent pipe reads/writes still need their
/// own owned worker cancellation. A socket timeout is not pipe cleanup evidence.
pub(crate) struct ParentIo {
    pub(crate) from_ui: mpsc::Receiver<Frame>,
    pub(crate) to_ui: UiSender,
}

/// No raw unbounded-wait sender escapes. Any enqueue failure is terminal for the
/// parent dispatcher; never retry a frame or treat queue admission as WS delivery.
pub(crate) struct UiSender(mpsc::Sender<Frame>);

impl UiSender {
    pub(crate) async fn send(&self, frame: Frame) -> Result<(), Error> {
        timeout(IO_DEADLINE, self.0.send(frame))
            .await
            .map_err(|_| Error::BackpressureTimeout)?
            .map_err(|_| Error::ParentClosed)
    }

    /// Sync FrameWriter adapters can enqueue on flush without blocking a runtime
    /// thread. Strip/check the one LF first, validate Frame, and map failure to a
    /// terminal io::Error so Dispatcher disconnects. No automatic queue retry.
    pub(crate) fn try_send(&self, frame: Frame) -> Result<(), Error> {
        self.0.try_send(frame).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => Error::QueueFull,
            mpsc::error::TrySendError::Closed(_) => Error::ParentClosed,
        })
    }
}

pub(crate) struct SocketIo {
    from_ui: mpsc::Sender<Frame>,
    to_ui: mpsc::Receiver<Frame>,
}

pub(crate) fn channels() -> (ParentIo, SocketIo) {
    let (from_ui_tx, from_ui_rx) = mpsc::channel(QUEUE_FRAMES);
    let (to_ui_tx, to_ui_rx) = mpsc::channel(QUEUE_FRAMES);
    (
        ParentIo {
            from_ui: from_ui_rx,
            to_ui: UiSender(to_ui_tx),
        },
        SocketIo {
            from_ui: from_ui_tx,
            to_ui: to_ui_rx,
        },
    )
}

/// Consumes an Endpoint-authenticated connection. Limits are also enforced by
/// Endpoint's WebSocketConfig before message allocation. No idle-session timeout;
/// only actual sends/queue backpressure have deadlines. Every exit cancels peer
/// work, including a blocked queue/sink. Parent child-exit cancellation is external.
pub(crate) async fn run(socket: WebSocketStream<EndpointIo>, io: SocketIo) -> Result<(), Error> {
    pump(socket, io, IO_DEADLINE).await
}

async fn pump<S>(socket: WebSocketStream<S>, io: SocketIo, budget: Duration) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let SocketIo { from_ui, mut to_ui } = io;
    let (mut sink, mut stream) = socket.split();
    let flush_control = Notify::new();
    let reader = async {
        loop {
            // Tungstenite handles framing and automatically queues the matching
            // pong. Notify the sole sink owner to flush even without JSON output.
            let message = stream
                .next()
                .await
                .ok_or(Error::PeerClosed)?
                .map_err(|_| Error::WebSocket)?;
            match message {
                Message::Text(text) => {
                    let frame = Frame::new(text.as_bytes().to_vec())?;
                    timeout(budget, from_ui.send(frame))
                        .await
                        .map_err(|_| Error::BackpressureTimeout)?
                        .map_err(|_| Error::ParentClosed)?;
                }
                Message::Ping(_) => flush_control.notify_one(),
                Message::Pong(_) => {}
                Message::Close(_) => return Err(Error::PeerClosed),
                Message::Binary(_) => return Err(Error::Binary),
                Message::Frame(_) => return Err(Error::WebSocket),
            }
        }
    };
    let writer = async {
        loop {
            tokio::select! {
                // Parent dropping its receiving half must wake an otherwise idle
                // pump too, not wait for another inbound text message.
                () = from_ui.closed() => return Err(Error::ParentClosed),
                () = flush_control.notified() => {
                    timeout(budget, sink.flush()).await
                        .map_err(|_| Error::SendTimeout)?
                        .map_err(|_| Error::WebSocket)?;
                }
                frame = to_ui.recv() => {
                    let frame = frame.ok_or(Error::ParentClosed)?;
                    let text = String::from_utf8(frame.0).map_err(|_| Error::InvalidFrame)?;
                    // Timeout/partial send drops the complete pump. Never retry a
                    // cancelled SinkExt::send on this or a replacement connection.
                    timeout(budget, sink.send(Message::Text(text.into()))).await
                        .map_err(|_| Error::SendTimeout)?
                        .map_err(|_| Error::WebSocket)?;
                }
            }
        }
    };
    // Both futures are local. Error in either drops the other and both WS halves.
    let ((), ()) = tokio::try_join!(reader, writer)?;
    Ok(())
}

/// Synthetic WS streams and short deadlines for deterministic blocked-sink tests;
/// production only accepts EndpointIo and uses its fixed transport bounds.
#[cfg(test)]
pub(crate) async fn run_test_socket<S>(
    socket: WebSocketStream<S>,
    io: SocketIo,
    budget: Duration,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pump(socket, io, budget).await
}
