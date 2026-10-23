//! Unregistered, single-socket, single-episode core client. Not native activation.
//!
//! Requests have one absolute deadline, capped at five seconds. Waiting for an
//! unsolicited Due uses the parent's deadline (which may include a long reset),
//! not a new five-second idle timeout. Neither a timer, Due nor Ready is a grant.
//! Dropping any polled operation closes its socket; never retry an uncertain
//! exchange, reconnect, detach this client or block_on inside a runtime worker.
//!
//! The dedicated owned parent thread must use native_frame_committed. INSIDE
//! that synchronous callback, bridge its runtime to bounded async Consume (never
//! on a runtime worker), then check Grant::authorizes and its shared ingress
//! checkpoint while sending the COMPLETE frame through the sole FrameWriter.
//! Pre-consuming/caching a grant before callback entry is not an accepted path.
//! Keep this client alive through that write. This module implements neither
//! that parent, core policy authority, native CAS nor an independent close watcher.

use std::{
    fmt,
    net::SocketAddr,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use qb_proto::native_reset::{Command, Event, MAX_MESSAGE_BYTES, Refusal, Reply, Request, VERSION};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout_at},
};
use tokio_tungstenite::{
    WebSocketStream, client_async_with_config,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue, Uri, header::AUTHORIZATION},
        protocol::WebSocketConfig,
    },
};

const EXCHANGE_LIMIT: Duration = Duration::from_secs(5);

/// Fixed diagnostics only; no underlying transport errors, URLs or bearer Debug.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    InvalidBase,
    InvalidBearer,
    Connect,
    Handshake,
    Timeout,
    Closed,
    Transport,
    Protocol,
    Replay,
    Cancelled,
    SequenceExhausted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidBase => "native-core-invalid-base",
            Self::InvalidBearer => "native-core-invalid-bearer",
            Self::Connect => "native-core-connect",
            Self::Handshake => "native-core-handshake",
            Self::Timeout => "native-core-timeout",
            Self::Closed => "native-core-closed",
            Self::Transport => "native-core-transport",
            Self::Protocol => "native-core-protocol",
            Self::Replay => "native-core-replay",
            Self::Cancelled => "native-core-cancelled",
            Self::SequenceExhausted => "native-core-sequence-exhausted",
        })
    }
}

impl std::error::Error for Error {}

/// Hints are deliberately not convertible to Grant. No thread or URL is Debug.
pub(crate) enum Observation {
    Waiting { episode: u64, reset_unix_ms: i64 },
    Due { episode: u64 },
    Refused(Refusal),
}

pub(crate) enum Inspection {
    Ready,
    Refused(Refusal),
}

pub(crate) enum Consumption {
    Granted(Grant),
    Refused(Refusal),
}

/// Opaque, noncloneable, nonserializable, deadline-bound receipt for ONE callback.
/// Dropping/poisoning the client invalidates it. A peer close not yet polled is
/// not observable without a reader; the parent still owns event/write ordering.
pub(crate) struct Grant {
    request_id: u64,
    episode: u64,
    deadline: Instant,
    live: Weak<AtomicBool>,
}

impl Grant {
    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }

    pub(crate) fn authorizes(self, episode: u64) -> bool {
        self.episode == episode
            && Instant::now() < self.deadline
            && self
                .live
                .upgrade()
                .is_some_and(|live| live.load(Ordering::Acquire))
    }

    #[cfg(test)]
    pub(crate) fn test_deadline(&self) -> Instant {
        self.deadline
    }
}

/// Parsing uses the raw URI, never URL normalization, DNS, redirects or proxies.
/// The path handle syntax matches the issued launch base; entropy is the issuer's
/// responsibility. Accept canonical numeric loopback IPv4/IPv6 with explicit port.
struct Endpoint {
    address: SocketAddr,
    url: String,
}

impl Endpoint {
    fn parse(base: &str) -> Result<Self, Error> {
        if base.len() > 256 {
            return Err(Error::InvalidBase);
        }
        let uri = base.parse::<Uri>().map_err(|_| Error::InvalidBase)?;
        let authority = uri.authority().ok_or(Error::InvalidBase)?.as_str();
        let address = authority
            .parse::<SocketAddr>()
            .map_err(|_| Error::InvalidBase)?;
        let handle = uri
            .path()
            .strip_prefix("/l/")
            .and_then(|path| path.strip_suffix("/v1"))
            .filter(|handle| {
                handle.len() == 43
                    && handle
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
            .ok_or(Error::InvalidBase)?;
        if uri.scheme_str() != Some("http")
            || !address.ip().is_loopback()
            || address.port() == 0
            || authority != address.to_string()
            || uri.query().is_some()
            || base != format!("http://{authority}/l/{handle}/v1")
        {
            return Err(Error::InvalidBase);
        }
        Ok(Self {
            address,
            url: format!("ws://{authority}/l/{handle}/v1/qb/native-owner"),
        })
    }
}

struct OwnedSocket {
    socket: WebSocketStream<TcpStream>,
    live: Arc<AtomicBool>,
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Observe,
    Inspect(u64),
    Consume(u64),
    Cancel(u64),
}

struct Pending {
    id: u64,
    kind: Kind,
}

/// Pure bounded correlation. One episode, one outstanding request, one Due slot.
struct Correlation {
    last_id: u64,
    observed: bool,
    episode: Option<u64>,
    pending: Option<Pending>,
    due: Option<u64>,
    finished: bool,
}

impl Correlation {
    fn new() -> Self {
        Self {
            last_id: 0,
            observed: false,
            episode: None,
            pending: None,
            due: None,
            finished: false,
        }
    }

    fn begin(&mut self, command: Command) -> Result<Request, Error> {
        if self.finished || self.pending.is_some() {
            return Err(Error::Closed);
        }
        let kind = match &command {
            Command::Observe { .. } if !self.observed => Kind::Observe,
            Command::Inspect { episode } if self.episode == Some(*episode) => {
                Kind::Inspect(*episode)
            }
            Command::Consume { episode } if self.episode == Some(*episode) => {
                Kind::Consume(*episode)
            }
            Command::Cancel { episode } if self.episode == Some(*episode) => Kind::Cancel(*episode),
            _ => return Err(Error::Protocol),
        };
        let id = self
            .last_id
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let request = Request {
            version: VERSION,
            request_id: id,
            command,
        };
        if !request.valid_shape() {
            return Err(Error::Protocol);
        }
        self.last_id = id;
        self.observed = true;
        // Consume/Cancel are spent before the first send, including failed sends.
        self.finished = matches!(kind, Kind::Consume(_) | Kind::Cancel(_));
        self.pending = Some(Pending { id, kind });
        Ok(request)
    }

    fn bind_episode(&mut self, episode: u64) -> Result<(), Error> {
        if self.episode.is_some_and(|current| current != episode) {
            return Err(Error::Protocol);
        }
        self.episode = Some(episode);
        Ok(())
    }

    fn record_due(&mut self, episode: u64) -> Result<(), Error> {
        // The server may repeat this hint after state notifications. It has no
        // request identity and conveys no authority: coalesce the SAME episode.
        // Correlated responses still require the exact outstanding request ID.
        self.bind_episode(episode)
    }

    /// None means a retained unsolicited hint, never a correlated answer.
    fn receive(&mut self, reply: Reply) -> Result<Option<Reply>, Error> {
        if !reply.valid_shape() {
            return Err(Error::Protocol);
        }
        if reply.request_id.is_none() {
            match reply.event {
                Event::Due { episode } => {
                    if !self.observed || self.finished && self.pending.is_none() {
                        return Err(Error::Protocol);
                    }
                    self.record_due(episode)?;
                    self.due = Some(episode);
                    return Ok(None);
                }
                Event::Cancelled { episode } => {
                    self.bind_episode(episode)?;
                    return Err(Error::Cancelled);
                }
                _ => return Err(Error::Protocol),
            }
        }
        let pending = self.pending.as_ref().ok_or(Error::Replay)?;
        let (id, kind) = (pending.id, pending.kind);
        if reply.request_id != Some(id) {
            return Err(Error::Replay);
        }
        match (kind, &reply.event) {
            (Kind::Observe, Event::Waiting { episode, .. }) => self.bind_episode(*episode)?,
            (Kind::Observe, Event::Due { episode }) => self.record_due(*episode)?,
            (Kind::Inspect(expected), Event::Ready { episode })
            | (Kind::Cancel(expected), Event::Cancelled { episode })
                if expected == *episode => {}
            (Kind::Consume(episode), Event::Granted { .. }) if reply.grants(id, episode) => {}
            (_, Event::Refused { .. }) => {}
            _ => return Err(Error::Protocol),
        }
        self.pending = None;
        Ok(Some(reply))
    }
}

/// No Debug/Clone, background reader, reconnect, retry or unbounded hint queue.
pub(crate) struct CoreControl {
    socket: Option<OwnedSocket>,
    correlation: Correlation,
    live: Arc<AtomicBool>,
}

impl CoreControl {
    /// `bearer` is the complete Authorization header, supplied by the owner.
    pub(crate) async fn connect(base: &str, mut bearer: HeaderValue) -> Result<Self, Error> {
        let endpoint = Endpoint::parse(base)?;
        let bytes = bearer.as_bytes();
        if !(8..=1024).contains(&bytes.len())
            || !bytes.starts_with(b"Bearer ")
            || !bytes[7..].iter().all(|byte| byte.is_ascii_graphic())
        {
            return Err(Error::InvalidBearer);
        }
        bearer.set_sensitive(true);
        let mut request = endpoint
            .url
            .into_client_request()
            .map_err(|_| Error::InvalidBase)?;
        request.headers_mut().insert(AUTHORIZATION, bearer);
        let deadline = Instant::now() + EXCHANGE_LIMIT;
        let stream = before(deadline, TcpStream::connect(endpoint.address))
            .await?
            .map_err(|_| Error::Connect)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES))
            .write_buffer_size(0)
            .max_write_buffer_size(MAX_MESSAGE_BYTES + 2048);
        let (socket, _) = before(
            deadline,
            client_async_with_config(request, stream, Some(config)),
        )
        .await?
        .map_err(|_| Error::Handshake)?;
        let live = Arc::new(AtomicBool::new(true));
        Ok(Self {
            socket: Some(OwnedSocket {
                socket,
                live: live.clone(),
            }),
            correlation: Correlation::new(),
            live,
        })
    }

    pub(crate) async fn observe(
        &mut self,
        thread_id: &str,
        deadline: Instant,
    ) -> Result<Observation, Error> {
        if thread_id.len() > 256 {
            self.close();
            return Err(Error::Protocol);
        }
        let reply = self
            .exchange(
                Command::Observe {
                    thread_id: thread_id.to_owned(),
                },
                deadline,
            )
            .await?;
        match reply.event {
            Event::Waiting {
                episode,
                reset_unix_ms,
            } => Ok(Observation::Waiting {
                episode,
                reset_unix_ms,
            }),
            Event::Due { episode } => Ok(Observation::Due { episode }),
            Event::Refused { reason } => {
                self.close();
                Ok(Observation::Refused(reason))
            }
            _ => unreachable!("correlation validates observe event"),
        }
    }

    /// Idle waiting has only the caller's absolute lifetime, not reset authority.
    pub(crate) async fn next_due(&mut self, deadline: Instant) -> Result<u64, Error> {
        let mut owned = self.socket.take().ok_or(Error::Closed)?;
        if Instant::now() >= deadline || self.correlation.finished || !self.correlation.observed {
            return Err(if Instant::now() >= deadline {
                Error::Timeout
            } else {
                Error::Closed
            });
        }
        if let Some(episode) = self.correlation.due.take() {
            self.socket = Some(owned);
            return Ok(episode);
        }
        loop {
            let reply = read_reply(&mut owned.socket, deadline).await?;
            self.correlation.receive(reply)?;
            if let Some(episode) = self.correlation.due.take() {
                self.socket = Some(owned);
                return Ok(episode);
            }
        }
    }

    pub(crate) async fn inspect(
        &mut self,
        episode: u64,
        deadline: Instant,
    ) -> Result<Inspection, Error> {
        match self
            .exchange(Command::Inspect { episode }, deadline)
            .await?
            .event
        {
            Event::Ready { .. } => Ok(Inspection::Ready),
            Event::Refused { reason } => Ok(Inspection::Refused(reason)),
            _ => unreachable!("correlation validates inspect event"),
        }
    }

    pub(crate) async fn consume(
        &mut self,
        episode: u64,
        deadline: Instant,
    ) -> Result<Consumption, Error> {
        // Compute ONCE before any await; the exchange and callback receipt share
        // this absolute cap even if the parent supplied a much later deadline.
        let deadline = deadline.min(Instant::now() + EXCHANGE_LIMIT);
        let reply = self
            .exchange(Command::Consume { episode }, deadline)
            .await?;
        // Correlation already calls Reply::grants for the exact pending request.
        match reply.event {
            Event::Granted { .. } => Ok(Consumption::Granted(Grant {
                request_id: reply.request_id.expect("correlated reply"),
                episode,
                deadline,
                live: Arc::downgrade(&self.live),
            })),
            Event::Refused { reason } => {
                self.close();
                Ok(Consumption::Refused(reason))
            }
            _ => unreachable!("correlation validates consume event"),
        }
    }

    pub(crate) async fn cancel(&mut self, episode: u64, deadline: Instant) -> Result<(), Error> {
        let result = self.exchange(Command::Cancel { episode }, deadline).await;
        self.close();
        match result?.event {
            Event::Cancelled { .. } => Ok(()),
            Event::Refused { .. } => Err(Error::Cancelled),
            _ => unreachable!("correlation validates cancel event"),
        }
    }

    /// Immediate local close for child exit/cancel. No asynchronous close handshake.
    pub(crate) fn close(&mut self) {
        self.live.store(false, Ordering::Release);
        drop(self.socket.take());
        self.correlation.finished = true;
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.socket.is_none() || !self.live.load(Ordering::Acquire)
    }

    async fn exchange(&mut self, command: Command, deadline: Instant) -> Result<Reply, Error> {
        // The local owns the socket across every await. Cancellation/error drops
        // it and invalidates grants, while self remains permanently socketless.
        let mut owned = self.socket.take().ok_or(Error::Closed)?;
        let deadline = deadline.min(Instant::now() + EXCHANGE_LIMIT);
        if Instant::now() >= deadline {
            return Err(Error::Timeout);
        }
        let request = self.correlation.begin(command)?;
        let text = serde_json::to_string(&request).map_err(|_| Error::Protocol)?;
        if text.len() > MAX_MESSAGE_BYTES {
            return Err(Error::Protocol);
        }
        before(deadline, owned.socket.send(Message::Text(text.into())))
            .await?
            .map_err(|_| Error::Transport)?;
        loop {
            let reply = read_reply(&mut owned.socket, deadline).await?;
            if let Some(reply) = self.correlation.receive(reply)? {
                self.socket = Some(owned);
                return Ok(reply);
            }
        }
    }
}

async fn before<T>(
    deadline: Instant,
    future: impl std::future::Future<Output = T>,
) -> Result<T, Error> {
    if Instant::now() >= deadline {
        return Err(Error::Timeout);
    }
    let result = timeout_at(deadline, future)
        .await
        .map_err(|_| Error::Timeout)?;
    if Instant::now() >= deadline {
        return Err(Error::Timeout);
    }
    Ok(result)
}

async fn read_reply(
    socket: &mut WebSocketStream<TcpStream>,
    deadline: Instant,
) -> Result<Reply, Error> {
    loop {
        let message = before(deadline, socket.next())
            .await?
            .ok_or(Error::Closed)?
            .map_err(|_| Error::Transport)?;
        match message {
            Message::Text(text) if text.len() <= MAX_MESSAGE_BYTES => {
                return serde_json::from_str(&text).map_err(|_| Error::Protocol);
            }
            // Tungstenite queues the matching Pong; flushing does not admit work.
            Message::Ping(_) => before(deadline, socket.flush())
                .await?
                .map_err(|_| Error::Transport)?,
            Message::Pong(_) => {}
            Message::Close(_) => return Err(Error::Closed),
            _ => return Err(Error::Protocol),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_numeric_base_and_exact_issued_path_only() {
        let handle = "a".repeat(43);
        for host in ["127.0.0.1:10200", "[::1]:10200"] {
            let base = format!("http://{host}/l/{handle}/v1");
            let endpoint = Endpoint::parse(&base).unwrap();
            assert_eq!(
                endpoint.url,
                format!("ws://{host}/l/{handle}/v1/qb/native-owner")
            );
        }
        for base in [
            format!("http://localhost:10200/l/{handle}/v1"),
            format!("http://192.0.2.1:10200/l/{handle}/v1"),
            format!("http://127.0.0.1:0/l/{handle}/v1"),
            format!("https://127.0.0.1:10200/l/{handle}/v1"),
            format!("http://user@127.0.0.1:10200/l/{handle}/v1"),
            format!("http://127.0.0.1:10200/l/{handle}/v1?x=1"),
            format!("http://127.0.0.1:10200/l/{handle}/v1/"),
            format!("http://127.0.0.1:10200/l/{handle}/../v1"),
            format!("http://127.0.0.1:10200/l/{handle}/v1#fragment"),
            format!("http://127.0.0.1:10200/l/{}/v1", "%".repeat(43)),
        ] {
            assert!(matches!(Endpoint::parse(&base), Err(Error::InvalidBase)));
        }
    }

    #[test]
    fn pure_correlation_never_grants_on_ready_wrong_request_or_wrong_episode() {
        for (id, event) in [
            (2, Event::Ready { episode: 7 }),
            (1, Event::Granted { episode: 7 }),
            (2, Event::Granted { episode: 8 }),
        ] {
            let mut state = Correlation::new();
            state
                .begin(Command::Observe {
                    thread_id: "thread-a".into(),
                })
                .unwrap();
            state
                .receive(Reply {
                    version: VERSION,
                    request_id: Some(1),
                    event: Event::Waiting {
                        episode: 7,
                        reset_unix_ms: 1,
                    },
                })
                .unwrap();
            state.begin(Command::Consume { episode: 7 }).unwrap();
            assert!(
                state
                    .receive(Reply {
                        version: VERSION,
                        request_id: Some(id),
                        event
                    })
                    .is_err()
            );
        }
        let mut state = Correlation::new();
        state.last_id = u64::MAX;
        assert!(matches!(
            state.begin(Command::Observe {
                thread_id: "thread-a".into()
            }),
            Err(Error::SequenceExhausted)
        ));
    }

    #[test]
    fn an_elapsed_grant_deadline_is_not_authority_even_with_a_live_owner() {
        let live = Arc::new(AtomicBool::new(true));
        let grant = Grant {
            request_id: 2,
            episode: 7,
            deadline: Instant::now(),
            live: Arc::downgrade(&live),
        };
        assert!(!grant.authorizes(7));
    }
}
