//! Windows foreground lifecycle with process-private listener transfer.
//! Routes MUST be merged inside http.rs's existing admin Safety middleware.
use crate::{config::Config, relay::RelayState};
use anyhow::{Result, anyhow, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use std::{
    future::Future,
    io::{BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

#[path = "listener_bank.rs"]
pub(super) mod listener_bank;

const REQUEST_LIMIT: Duration = Duration::from_secs(2);
const EXPORT_PATH: &str = "/admin/v1/lifecycle/listeners/export";
const READY_PATH: &str = "/admin/v1/lifecycle/listeners/ready";
static READY_TARGET: Mutex<Option<ReadyTarget>> = Mutex::new(None);
static READINESS: Readiness = Readiness::new();

struct Readiness {
    initialized: AtomicBool,
    ready: AtomicBool,
}

impl Readiness {
    const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            ready: AtomicBool::new(false),
        }
    }
    fn initialize(&self) {
        self.clear();
        self.initialized.store(true, Ordering::Release);
    }
    fn acknowledge(&self) {
        self.ready.store(true, Ordering::Release);
    }
    fn clear(&self) {
        self.ready.store(false, Ordering::Release);
    }
    fn health(&self) -> bool {
        !self.initialized.load(Ordering::Acquire) || self.ready.load(Ordering::Acquire)
    }
}

struct ReadyTarget {
    // Reuse the connection whose export response came from the incumbent. A new
    // connection could be accepted by either process once listeners are shared.
    connection: BufReader<TcpStream>,
    port: u16,
    token: Zeroizing<String>,
    ready: listener_bank::ReadyRequest,
}
pub(crate) fn prepare_process() -> anyhow::Result<()> {
    Ok(())
}
pub(crate) async fn handle_takeover(home: &Path, config: &Config, takeover: bool) -> Result<bool> {
    if !takeover || config.server.admin_port == 0 {
        return Ok(false);
    }
    let home = home.to_owned();
    let ports = [config.server.data_port, config.server.admin_port];
    // Native bounded TCP has no proxy, redirects or automatic POST retries.
    // Run the export off the async executor; only an explicit 404 is legacy.
    let target = tokio::task::spawn_blocking(move || -> Result<Option<ReadyTarget>> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, ports[1]));
        let stream = match TcpStream::connect_timeout(&address, REQUEST_LIMIT) {
            Ok(stream) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(None),
            Err(_) => bail!("listener export connection failed"),
        };
        stream.set_nodelay(true)?;
        let token = Zeroizing::new(
            crate::platform::load_existing_secret_file(&home.join("admin.token"))
                .map_err(|_| anyhow!("listener export authentication unavailable"))?,
        );
        let mut connection = BufReader::new(stream);
        let request = Zeroizing::new(serde_json::to_vec(&listener_bank::ExportRequest {
            recipientpid: std::process::id(),
        })?);
        let (status, bytes) = private_post(
            &mut connection,
            ports[1],
            &token,
            EXPORT_PATH,
            &request,
            false,
        )?;
        if status == 404 {
            return Ok(None);
        }
        ensure!(status == 200, "listener export refused");
        let export: listener_bank::Export = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("listener export response invalid"))?;
        listener_bank::import(&export, ports)?;
        Ok(Some(ReadyTarget {
            connection,
            port: ports[1],
            token,
            ready: listener_bank::ReadyRequest {
                recipientpid: export.recipientpid,
                nonce: export.nonce.clone(),
            },
        }))
    })
    .await
    .map_err(|_| anyhow!("listener export task failed"))??;
    if let Some(target) = target {
        let mut slot = READY_TARGET
            .lock()
            .map_err(|_| anyhow!("listener ready target unavailable"))?;
        ensure!(slot.is_none(), "listener ready target already set");
        *slot = Some(target);
    }
    // Common CLI still drains/imports bindings and acquires the credential lease.
    Ok(false)
}
pub(crate) fn initialize_listeners(config: &Config) -> Result<()> {
    listener_bank::initialize([config.server.data_port, config.server.admin_port])?;
    READINESS.initialize();
    Ok(())
}
pub(crate) fn routes(relay: Arc<RelayState>) -> Router {
    Router::new()
        .route(EXPORT_PATH, post(export_listeners))
        .route(READY_PATH, post(listeners_ready))
        .layer(DefaultBodyLimit::max(listener_bank::WIRE_LIMIT))
        .with_state(relay)
}
async fn export_listeners(
    State(_relay): State<Arc<RelayState>>,
    Json(request): Json<listener_bank::ExportRequest>,
) -> axum::response::Response {
    // Failed drain preparation keeps the incumbent and its listeners alive.
    // The bank alone fences shutdown and any previous recipient capability;
    // the relay's draining flag does not mean preparation succeeded.
    match listener_bank::export(request.recipientpid) {
        Ok(export) => ([(header::CACHE_CONTROL, "no-store")], Json(export)).into_response(),
        Err(_) => (StatusCode::CONFLICT, "listener export unavailable").into_response(),
    }
}

async fn listeners_ready(
    Json(request): Json<listener_bank::ReadyRequest>,
) -> (StatusCode, &'static str) {
    match listener_bank::ready(&request) {
        Ok(()) => (StatusCode::OK, "listener ready accepted"),
        Err(_) => (StatusCode::CONFLICT, "listener ready unavailable"),
    }
}

pub(crate) fn notify_ready() -> Result<()> {
    let target = READY_TARGET
        .lock()
        .map_err(|_| anyhow!("listener ready target unavailable"))?
        .take();
    if let Some(mut target) = target {
        let request = Zeroizing::new(serde_json::to_vec(&target.ready)?);
        let (status, _) = private_post(
            &mut target.connection,
            target.port,
            &target.token,
            READY_PATH,
            &request,
            true,
        )?;
        ensure!(status == 200, "listener ready acknowledgement refused");
    }
    READINESS.acknowledge();
    Ok(())
}
pub(crate) async fn wait_for_shutdown(relay: Arc<RelayState>) -> Result<()> {
    tokio::select! {
        result = tokio::signal::ctrl_c() => { result?; }
        _ = relay.wait_for_drain() => {}
    }
    Ok(())
}

/// Main calls after box admission and schedulers stop, immediately before
/// closing HTTP acceptors. Failure must continue through mandatory cleanup.
pub(crate) async fn prepare_listener_shutdown(relay: Arc<RelayState>) {
    READINESS.clear();
    let deadline = listener_bank::prepare_shutdown();
    if !relay.is_draining() {
        return;
    }
    let Some(deadline) = deadline else {
        return;
    };
    // Normal shutdown already observed completion. Startup-error/Ctrl-C cleanup
    // may arrive earlier: await that same completion without repeating freeze
    // or cancelling the real writer. Timeout must still enter ordinary cleanup.
    if tokio::time::timeout_at(deadline.into(), relay.wait_for_drain())
        .await
        .is_err()
    {
        tracing::warn!("Listener transfer drain deadline expired; continuing shutdown");
        return;
    }
    relay.release_lease_after_drain();
    if !listener_bank::wait_ready(deadline).await {
        tracing::warn!("Listener transfer readiness expired; continuing shutdown");
    }
}

pub(crate) async fn wait_handover_ready() -> bool {
    listener_bank::wait_handover_ready().await
}

pub(crate) fn listener_closed(role: &'static str) {
    listener_bank::listener_closed(role);
}

/// Minimal bounded HTTP/1.1 on the incumbent's private keep-alive connection.
/// Json responses have Content-Length; reject ambiguous framing and cap headers
/// and body separately. Never include request/response bytes in errors.
fn private_post(
    connection: &mut BufReader<TcpStream>,
    port: u16,
    token: &str,
    path: &str,
    body: &[u8],
    close: bool,
) -> Result<(u16, Zeroizing<Vec<u8>>)> {
    ensure!(
        body.len() <= listener_bank::WIRE_LIMIT && !token.contains(['\r', '\n']),
        "listener control request invalid"
    );
    let deadline = Instant::now() + REQUEST_LIMIT;
    let request = Zeroizing::new(format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
        body.len(),
        if close { "close" } else { "keep-alive" }
    ));
    connection
        .get_mut()
        .set_write_timeout(Some(REQUEST_LIMIT))?;
    connection
        .get_mut()
        .write_all(request.as_bytes())
        .map_err(|_| anyhow!("listener control write failed"))?;
    connection
        .get_mut()
        .write_all(body)
        .map_err(|_| anyhow!("listener control write failed"))?;
    let mut headers = Zeroizing::new(Vec::new());
    loop {
        ensure!(headers.len() < 8192, "listener control headers too large");
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("listener control deadline exceeded"))?;
        if connection.buffer().is_empty() {
            connection.get_mut().set_read_timeout(Some(remaining))?;
        }
        let mut byte = [0];
        connection
            .read_exact(&mut byte)
            .map_err(|_| anyhow!("listener control response failed"))?;
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let headers =
        std::str::from_utf8(&headers).map_err(|_| anyhow!("listener control headers invalid"))?;
    let mut lines = headers.split("\r\n");
    let mut status_line = lines.next().unwrap_or_default().split_whitespace();
    ensure!(
        status_line.next() == Some("HTTP/1.1"),
        "listener control HTTP version invalid"
    );
    let status = status_line
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("listener control status invalid"))?;
    let mut length = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("listener control header invalid"))?;
        ensure!(
            !name.eq_ignore_ascii_case("transfer-encoding"),
            "listener control framing invalid"
        );
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(length.is_none(), "listener control framing ambiguous");
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| anyhow!("listener control length invalid"))?,
            );
        }
    }
    let length = length.ok_or_else(|| anyhow!("listener control length missing"))?;
    ensure!(
        length <= listener_bank::WIRE_LIMIT,
        "listener control body too large"
    );
    let mut bytes = Zeroizing::new(vec![0; length]);
    let mut filled = 0;
    while filled < length {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("listener control deadline exceeded"))?;
        connection.get_mut().set_read_timeout(Some(remaining))?;
        let read = connection
            .read(&mut bytes[filled..])
            .map_err(|_| anyhow!("listener control body failed"))?;
        ensure!(read != 0, "listener control body incomplete");
        filled += read;
    }
    Ok((status, bytes))
}
pub(crate) fn should_release_lease_early() -> bool {
    true
}
pub(crate) async fn complete_shutdown<F: Future<Output = anyhow::Result<()>>>(
    work: F,
) -> anyhow::Result<()> {
    work.await
}
pub(crate) fn finish() -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn validate_listener_drain(_: bool) -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn health_ready() -> bool {
    READINESS.health()
}
pub(crate) fn retry_durable_cleanup() -> bool {
    false
}

pub(crate) fn defer_error_exit(code: i32) {
    std::process::exit(code)
}
pub(crate) fn finish_process() -> anyhow::Result<()> {
    Ok(())
}
pub(crate) fn retain_observability(
    _: Arc<crate::observability::Observability>,
) -> anyhow::Result<()> {
    Ok(())
}
pub(crate) async fn drain_connections<F: Future<Output = bool>>(
    work: F,
    limit: std::time::Duration,
) -> bool {
    tokio::time::timeout(limit, work)
        .await
        .is_ok_and(|clean| clean)
}
pub(crate) fn defer_startup_error(
    result: anyhow::Result<()>,
) -> anyhow::Result<Option<anyhow::Error>> {
    if result.is_err() {
        READINESS.clear();
    }
    Ok(result.err())
}
