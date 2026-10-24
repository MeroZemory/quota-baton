//! Managed Unix service control and process lifecycle.
//! Routes MUST be merged inside http.rs's existing admin Safety middleware.

use crate::{config::Config, relay::RelayState};
use anyhow::{Result, anyhow, bail};
use axum::{Router, extract::State, http::StatusCode, routing::post};
use futures_util::FutureExt;
use qb_proto::HealthResponse;
use std::{
    convert::Infallible,
    ffi::{OsStr, OsString},
    fs::File,
    future::Future,
    io,
    os::{fd::AsRawFd, unix::ffi::OsStrExt, unix::net::UnixDatagram},
    panic::AssertUnwindSafe,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};
use tokio::time::{Instant, sleep, timeout, timeout_at};
use zeroize::Zeroizing;

const IDLE: u8 = 0;
const DRAINING: u8 = 1;
const DRAINED: u8 = 2;
const FAILED: u8 = 3;
const STOPPING: u8 = 0x80;
const HELPER_LIMIT: Duration = Duration::from_secs(55);
const REQUEST_LIMIT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_LIMIT: usize = 16 * 1024;
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(45);

struct Process {
    executable: PathBuf,
    args: Vec<OsString>,
}

#[derive(Debug)]
struct StopBeforeExec;

impl std::fmt::Display for StopBeforeExec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("managed service stopping")
    }
}

impl std::error::Error for StopBeforeExec {}

static PROCESS: OnceLock<Process> = OnceLock::new();
static SIGNAL_GATE: Mutex<()> = Mutex::new(());
static SIGNAL_ERROR: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_FAILED: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static EXEC_READY: AtomicBool = AtomicBool::new(false);
static STOP_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();
static SHUTDOWN_DEADLINE: OnceLock<Instant> = OnceLock::new();
static DRAIN_TASK: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>> =
    tokio::sync::Mutex::const_new(None);
static HELD_LEASE: Mutex<Option<File>> = Mutex::new(None);
static HELD_OBSERVABILITY: Mutex<Option<Arc<crate::observability::Observability>>> =
    Mutex::new(None);

pub(crate) fn retain_lease_file(file: &File) -> Result<()> {
    if !runtime_managed() {
        return Ok(());
    }
    let mut held = HELD_LEASE
        .lock()
        .map_err(|_| anyhow!("service lease retention failed"))?;
    if held.is_some() {
        bail!("service lease already retained");
    }
    *held = Some(clone_lease_file(file)?);
    Ok(())
}

fn clone_lease_file(file: &File) -> Result<File> {
    // dup shares the existing flock open-file description: never open/acquire
    // another lease. Only this duplicate's descriptor flags are changed.
    let duplicate = file
        .try_clone()
        .map_err(|_| anyhow!("service lease retention failed"))?;
    let flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) };
    if flags < 0
        || unsafe {
            libc::fcntl(
                duplicate.as_raw_fd(),
                libc::F_SETFD,
                flags | libc::FD_CLOEXEC,
            )
        } < 0
    {
        bail!("service lease retention failed");
    }
    Ok(duplicate)
}

pub(crate) fn retain_observability(
    observability: Arc<crate::observability::Observability>,
) -> Result<()> {
    if !runtime_managed() {
        return Ok(());
    }
    let mut held = HELD_OBSERVABILITY
        .lock()
        .map_err(|_| anyhow!("service observability retention failed"))?;
    if held.is_some() {
        bail!("service observability already retained");
    }
    *held = Some(observability);
    Ok(())
}

fn take_retained<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    let mut held = slot.lock().unwrap_or_else(|poison| {
        SHUTDOWN_FAILED.store(true, Ordering::Release);
        poison.into_inner()
    });
    held.take()
}

pub(crate) fn defer_startup_error(result: Result<()>) -> Result<Option<anyhow::Error>> {
    defer_startup_result(result, runtime_managed(), &SHUTDOWN_FAILED, &EXEC_READY)
}

fn defer_startup_result(
    result: Result<()>,
    managed: bool,
    failed: &AtomicBool,
    armed: &AtomicBool,
) -> Result<Option<anyhow::Error>> {
    match result {
        Ok(()) => Ok(None),
        Err(error) if managed => {
            failed.store(true, Ordering::Release);
            armed.store(false, Ordering::Release);
            Ok(Some(error))
        }
        Err(error) => Err(error),
    }
}

pub(crate) async fn drain_connections<F>(drain: F, limit: Duration) -> bool
where
    F: Future<Output = bool>,
{
    if !runtime_managed() {
        return timeout(limit, drain).await.unwrap_or(false);
    }
    let deadline = shutdown_deadline().min(Instant::now() + limit);
    await_connection_drain(drain, deadline, &SHUTDOWN_FAILED).await
}

async fn await_connection_drain<F>(drain: F, deadline: Instant, failed: &AtomicBool) -> bool
where
    F: Future<Output = bool>,
{
    tokio::pin!(drain);
    match timeout_at(deadline, &mut drain).await {
        Ok(clean) => {
            if !clean {
                failed.store(true, Ordering::Release);
            }
            clean
        }
        Err(_) => {
            failed.store(true, Ordering::Release);
            // Preserve the actual parent/connection-owner future past deadline.
            // Finishing late remains failure, but cannot abandon its writers.
            let _ = drain.await;
            false
        }
    }
}

fn shutdown_deadline() -> Instant {
    *SHUTDOWN_DEADLINE.get_or_init(|| Instant::now() + SHUTDOWN_LIMIT)
}

fn runtime_managed() -> bool {
    PROCESS.get().is_some()
}

fn server_invocation(args: &[OsString]) -> bool {
    args.get(1).is_some_and(|arg| arg == "serve")
        && !args.iter().skip(2).any(|arg| arg == "--takeover")
}

fn capture_process(args: Vec<OsString>) -> Result<Process> {
    let executable = args
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("managed executable invalid"))?;
    if !executable.is_absolute()
        || executable.file_name().is_none()
        || executable.as_os_str().as_bytes().contains(&0)
        || executable
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        bail!("managed executable invalid");
    }
    Ok(Process {
        executable,
        args: args.into_iter().skip(1).collect(),
    })
}

fn stop_signals() -> libc::sigset_t {
    // All bytes initialized before libc fills the set.
    let mut signals = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut signals);
        libc::sigaddset(&mut signals, libc::SIGTERM);
        libc::sigaddset(&mut signals, libc::SIGINT);
    }
    signals
}

fn record_stop() {
    if runtime_managed() {
        shutdown_deadline();
    }
    RELOAD.stop();
    STOP_NOTIFY.notify_waiters();
}

fn pending_stop() -> io::Result<bool> {
    let mut pending = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigpending(&mut pending) } != 0 {
        return Err(io::Error::other("managed signal inspection failed"));
    }
    Ok(unsafe {
        libc::sigismember(&pending, libc::SIGTERM) == 1
            || libc::sigismember(&pending, libc::SIGINT) == 1
    })
}

fn receive_stop(signals: &libc::sigset_t, nanos: libc::c_long) -> io::Result<bool> {
    let wait = libc::timespec {
        tv_sec: 0,
        tv_nsec: nanos,
    };
    let signal = unsafe { libc::sigtimedwait(signals, std::ptr::null_mut(), &wait) };
    if signal == libc::SIGTERM || signal == libc::SIGINT {
        return Ok(true);
    }
    if signal < 0
        && matches!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN) | Some(libc::EINTR)
        )
    {
        return Ok(false);
    }
    Err(io::Error::other("managed signal wait failed"))
}

/// Must run on the original single main thread BEFORE constructing Tokio.
/// Managed helpers and unrelated commands never change their signal masks.
pub(crate) fn prepare_process() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if !managed() || !server_invocation(&args) {
        return Ok(());
    }
    if tokio::runtime::Handle::try_current().is_ok() || PROCESS.get().is_some() {
        bail!("managed process preparation order invalid");
    }
    let process = capture_process(args)?;
    let signals = stop_signals();
    // Every subsequently created runtime/monitor thread inherits this mask.
    if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()) } != 0 {
        bail!("managed signal preparation failed");
    }
    PROCESS
        .set(process)
        .map_err(|_| anyhow!("managed process already prepared"))?;
    {
        let _guard = SIGNAL_GATE
            .lock()
            .map_err(|_| anyhow!("managed signal lock failed"))?;
        // A replacement image inherits blocked signals and pending TERM/INT.
        // Consume them before it can publish READY or start a reload.
        while receive_stop(&signals, 0).map_err(|_| anyhow!("managed signal preparation failed"))? {
            record_stop();
        }
    }
    std::thread::Builder::new()
        .name("qb-service-signals".into())
        .spawn(move || {
            loop {
                let Ok(guard) = SIGNAL_GATE.lock() else {
                    SIGNAL_ERROR.store(true, Ordering::Release);
                    record_stop();
                    return;
                };
                // The same lock covers bounded waiting, signal consumption and
                // recording. Final exec cannot miss a signal consumed by this owner.
                match receive_stop(&signals, 50_000_000) {
                    Ok(true) => record_stop(),
                    Ok(false) => {}
                    Err(_) => {
                        SIGNAL_ERROR.store(true, Ordering::Release);
                        record_stop();
                        return;
                    }
                }
                drop(guard);
                // Permit a waiting final exec/READY guard to acquire the mutex.
                std::thread::sleep(Duration::from_millis(1));
            }
        })
        .map_err(|_| anyhow!("managed signal monitor unavailable"))?;
    Ok(())
}

pub(crate) fn stop_requested() -> bool {
    RELOAD.0.load(Ordering::Acquire) & STOPPING != 0
}

pub(crate) async fn wait_for_stop() -> Result<()> {
    if !runtime_managed() {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|_| anyhow!("service signal unavailable"))?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(|_| anyhow!("service signal unavailable"))?,
            result = term.recv() => if result.is_none() { bail!("service signal unavailable"); },
        }
        cancel_reload_for_stop();
        return Ok(());
    }
    loop {
        let notified = STOP_NOTIFY.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if SIGNAL_ERROR.load(Ordering::Acquire) {
            bail!("managed signal monitor failed");
        }
        if stop_requested() {
            return Ok(());
        }
        notified.await;
    }
}

/// Listener exec must run ONLY inside this closure, with the mask still blocked.
/// A signal arriving after sigpending is retained across exec and consumed by
/// the successor's pre-runtime prepare_process before READY.
pub(crate) fn with_exec_signal_guard<F>(exec: F) -> io::Result<Infallible>
where
    F: FnOnce() -> io::Result<Infallible>,
{
    if !runtime_managed() {
        return Err(io::Error::other("managed process not prepared"));
    }
    let _guard = SIGNAL_GATE
        .lock()
        .map_err(|_| io::Error::other("managed signal lock failed"))?;
    if pending_stop()? {
        record_stop();
    }
    if SIGNAL_ERROR.load(Ordering::Acquire) || stop_requested() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, StopBeforeExec));
    }
    if SHUTDOWN_FAILED.load(Ordering::Acquire) {
        return Err(io::Error::other("managed shutdown failed"));
    }
    if SHUTDOWN_DEADLINE
        .get()
        .is_some_and(|deadline| Instant::now() >= *deadline)
    {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "managed cleanup deadline exceeded",
        ));
    }
    exec()
}

pub(crate) async fn settle_reload() -> Result<()> {
    if !runtime_managed() {
        return Ok(());
    }
    if !await_drain_task(&DRAIN_TASK).await {
        RELOAD.finish(false);
        SHUTDOWN_FAILED.store(true, Ordering::Release);
    }
    // The budget governs exec eligibility, never writer/task lifetime.
    if Instant::now() >= shutdown_deadline() {
        SHUTDOWN_FAILED.store(true, Ordering::Release);
    }
    if reload_failed() || SIGNAL_ERROR.load(Ordering::Acquire) {
        bail!("managed reload drain failed");
    }
    Ok(())
}

async fn await_drain_task(slot: &tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>) -> bool {
    // Await BY REFERENCE while the handle remains in shared storage. Cancelling
    // this waiter releases the mutex but cannot detach/lose the actual writer.
    let mut task = slot.lock().await;
    if let Some(running) = task.as_mut() {
        let success = running.await.is_ok();
        // Remove the handle only after its actual terminal JoinHandle result.
        task.take();
        success
    } else {
        true
    }
}

pub(crate) async fn wait_for_shutdown(relay: Arc<RelayState>) -> Result<()> {
    if !runtime_managed() {
        tokio::select! {
            result = wait_for_stop() => result?,
            _ = relay.wait_for_drain() => {},
        }
        return Ok(());
    }
    tokio::select! {
        result = wait_for_stop() => {
            if result.is_err() {
                SHUTDOWN_FAILED.store(true, Ordering::Release);
            }
        },
        _ = relay.wait_for_drain_start() => {},
    }
    // Includes drain failure and the publication gap after drain_ready notify.
    // Failure must not bypass mandatory login/refresh/durable cleanup in main.
    if settle_reload().await.is_err() {
        SHUTDOWN_FAILED.store(true, Ordering::Release);
    }
    Ok(())
}

async fn await_durable_cleanup<F>(cleanup: F, deadline: Instant, failed: &AtomicBool) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    // Never race this future against a timer: cancelling it can lose token writes.
    let result = cleanup.await;
    if result.is_err() || Instant::now() >= deadline {
        failed.store(true, Ordering::Release);
    }
    result
}

pub(crate) async fn complete_shutdown<F>(cleanup: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    if runtime_managed() {
        // Startup-error shutdown can skip wait_for_shutdown. It must still
        // reconcile an already accepted drain before mandatory cleanup starts.
        if settle_reload().await.is_err() {
            SHUTDOWN_FAILED.store(true, Ordering::Release);
        }
        await_durable_cleanup(cleanup, shutdown_deadline(), &SHUTDOWN_FAILED).await?;
        if settle_reload().await.is_err() {
            SHUTDOWN_FAILED.store(true, Ordering::Release);
        }
        // Drain/deadline failure is reported by finish AFTER mandatory cleanup.
        Ok(())
    } else {
        cleanup.await
    }
}

pub(crate) fn should_release_lease_early() -> bool {
    !runtime_managed()
}

pub(crate) fn retry_durable_cleanup() -> bool {
    runtime_managed()
}

pub(crate) fn validate_listener_drain(clean: bool) -> Result<()> {
    if runtime_managed() && !clean {
        SHUTDOWN_FAILED.store(true, Ordering::Release);
    }
    // Preserve failure for finish, after main's mandatory durable cleanup.
    Ok(())
}

pub(crate) fn health_ready() -> bool {
    !runtime_managed() || READY.load(Ordering::Acquire)
}

pub(crate) fn initialize_listeners(config: &Config) -> Result<()> {
    super::listener::initialize_activation(super::listener::ListenerPorts {
        data: config.server.data_port,
        admin: config.server.admin_port,
    })
    .map_err(|_| anyhow!("service listener initialization failed"))
}

fn notify_address(value: &OsStr) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = value.as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty()
        || bytes.contains(&0)
        || bytes.len() >= address.sun_path.len()
        || !matches!(bytes[0], b'/' | b'@')
    {
        return Err(io::Error::other("managed notify address invalid"));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(bytes) {
        *target = *source as _;
    }
    let length = if bytes[0] == b'@' {
        if bytes.len() == 1 {
            return Err(io::Error::other("managed notify address invalid"));
        }
        address.sun_path[0] = 0;
        bytes.len()
    } else {
        bytes.len() + 1
    };
    Ok((
        address,
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + length) as libc::socklen_t,
    ))
}

pub(crate) fn notify_ready() -> Result<()> {
    if !runtime_managed() {
        return Ok(());
    }
    let _guard = SIGNAL_GATE
        .lock()
        .map_err(|_| anyhow!("managed signal lock failed"))?;
    if pending_stop().map_err(|_| anyhow!("managed signal inspection failed"))? {
        record_stop();
    }
    if stop_requested() || SIGNAL_ERROR.load(Ordering::Acquire) {
        bail!("managed service stopping");
    }
    let destination =
        std::env::var_os("NOTIFY_SOCKET").ok_or_else(|| anyhow!("managed notify unavailable"))?;
    let (address, length) =
        notify_address(&destination).map_err(|_| anyhow!("managed notify address invalid"))?;
    let socket = UnixDatagram::unbound().map_err(|_| anyhow!("managed notify unavailable"))?;
    let message = b"READY=1";
    let sent = unsafe {
        libc::sendto(
            socket.as_raw_fd(),
            message.as_ptr().cast(),
            message.len(),
            libc::MSG_DONTWAIT,
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if sent != message.len() as isize {
        bail!("managed notify failed");
    }
    READY.store(true, Ordering::Release);
    Ok(())
}

/// Call ONLY after ordinary cleanup, all joins and all durable flushes succeeded.
/// This only arms intent. It NEVER replaces a process that still owns Tokio.
pub(crate) fn finish() -> Result<()> {
    EXEC_READY.store(false, Ordering::Release);
    if !runtime_managed() {
        return Ok(());
    }
    if reload_failed() || SIGNAL_ERROR.load(Ordering::Acquire) {
        bail!("managed reload failed");
    }
    if RELOAD.0.load(Ordering::Acquire) & !STOPPING == DRAINING {
        bail!("managed reload incomplete");
    }
    if !reload_requested() || stop_requested() {
        return Ok(());
    }
    if Instant::now() >= shutdown_deadline() {
        SHUTDOWN_FAILED.store(true, Ordering::Release);
        bail!("managed cleanup deadline exceeded");
    }
    EXEC_READY.store(true, Ordering::Release);
    Ok(())
}

/// Sync main calls this ONLY AFTER drop(runtime) completed, including Tokio's
/// blocking-task shutdown and all writer destructors. Signal ownership outlives
/// that runtime and the listener still obtains the final shared signal guard.
pub(crate) fn finish_process() -> Result<()> {
    if !runtime_managed() {
        return Ok(());
    }
    if tokio::runtime::Handle::try_current().is_ok() {
        SHUTDOWN_FAILED.store(true, Ordering::Release);
        bail!("managed runtime still active");
    }
    // Even failed startup/ordinary stop must join native persistence before
    // releasing the same flock OFD. The local guard lives through every return
    // and is CLOEXEC, so successful exec releases it only at image replacement.
    let _lease = take_retained(&HELD_LEASE);
    if let Some(retained) = take_retained(&HELD_OBSERVABILITY) {
        finish_observability(retained, &SHUTDOWN_FAILED);
    }
    if SHUTDOWN_DEADLINE
        .get()
        .is_some_and(|deadline| Instant::now() >= *deadline)
    {
        SHUTDOWN_FAILED.store(true, Ordering::Release);
    }
    if reload_failed() || SIGNAL_ERROR.load(Ordering::Acquire) {
        EXEC_READY.store(false, Ordering::Release);
        bail!("managed reload failed");
    }
    if !EXEC_READY.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    if stop_requested() {
        return Ok(());
    }
    let process = PROCESS
        .get()
        .ok_or_else(|| anyhow!("managed process not prepared"))?;
    // listener owns the one signal guard around its actual exec commitment.
    match super::listener::exec_activation(&process.executable, &process.args) {
        Ok(never) => match never {},
        Err(error)
            if error
                .get_ref()
                .is_some_and(|cause| cause.is::<StopBeforeExec>())
                && stop_requested()
                && !SIGNAL_ERROR.load(Ordering::Acquire) =>
        {
            Ok(())
        }
        Err(_) => bail!("managed re-exec failed"),
    }
}

fn finish_observability(
    mut retained: Arc<crate::observability::Observability>,
    failed: &AtomicBool,
) {
    let observability = loop {
        match Arc::try_unwrap(retained) {
            Ok(observability) => break observability,
            Err(still_shared) => {
                retained = still_shared;
                // A native weak-table tick may temporarily own the last
                // graph reference. Let it finish without imposing token loss.
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    while observability.flush().is_err() {
        failed.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_secs(1));
    }
    // Outside Tokio this sends Stop and synchronously joins the worker.
    drop(observability);
}

/// Preserve runtime/blocking-writer teardown on managed errors. The caller must
/// return the error/code, drop its runtime, and exit afterward; never continue.
pub(crate) fn defer_error_exit(code: i32) {
    if runtime_managed() {
        EXEC_READY.store(false, Ordering::Release);
        SHUTDOWN_FAILED.store(true, Ordering::Release);
    } else {
        std::process::exit(code);
    }
}

struct ReloadIntent(AtomicU8);

impl ReloadIntent {
    const fn new() -> Self {
        Self(AtomicU8::new(IDLE))
    }

    fn claim(&self) -> bool {
        self.0
            .compare_exchange(IDLE, DRAINING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn finish(&self, success: bool) {
        // Preserve TERM's sticky bit, including when completion races with it.
        // Preserve failure evidence as well if a failed drain finishes after TERM.
        let _ = self
            .0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & !STOPPING == DRAINING)
                    .then_some((state & STOPPING) | if success { DRAINED } else { FAILED })
            });
    }

    fn stop(&self) {
        self.0.fetch_or(STOPPING, Ordering::AcqRel);
    }

    fn ready(&self) -> bool {
        self.0.load(Ordering::Acquire) == DRAINED
    }

    fn failed(&self) -> bool {
        self.0.load(Ordering::Acquire) & !STOPPING == FAILED
    }
}

static RELOAD: ReloadIntent = ReloadIntent::new();

fn managed() -> bool {
    std::env::var_os("QB_SYSTEMD_MANAGED").as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// True only after the one accepted reload's shared drain completed successfully.
/// This is necessary, NOT sufficient, authority for exec: ordinary cleanup must
/// also succeed and the final runtime signal gate must give TERM precedence.
pub(crate) fn reload_requested() -> bool {
    RELOAD.ready()
}

/// Runtime must invoke this for TERM/ordinary stop before beginning stop cleanup.
pub(crate) fn cancel_reload_for_stop() {
    // Every explicit stop writer uses the same gate as signal consumption/exec.
    // The monitor calls record_stop directly while already holding this lock.
    match SIGNAL_GATE.lock() {
        Ok(_guard) => record_stop(),
        Err(_) => {
            SIGNAL_ERROR.store(true, Ordering::Release);
            record_stop();
        }
    }
}

/// Runtime must treat a failed drain as a nonzero exit, never an ordinary success.
pub(crate) fn reload_failed() -> bool {
    RELOAD.failed() || SHUTDOWN_FAILED.load(Ordering::Acquire)
}

pub(crate) fn routes(relay: Arc<RelayState>) -> Router {
    if !managed() {
        return Router::new();
    }
    Router::new()
        .route("/admin/v1/service/reload", post(request_reload))
        .with_state(relay)
}

async fn request_reload(State(relay): State<Arc<RelayState>>) -> (StatusCode, &'static str) {
    // Publish ownership before any drain notification can wake the main task.
    let mut task = DRAIN_TASK.lock().await;
    // A separately initiated ordinary drain is not a managed reload.
    if relay.is_draining() || stop_requested() || reload_failed() || !RELOAD.claim() {
        return (StatusCode::CONFLICT, "service reload unavailable");
    }
    shutdown_deadline();
    // Shared ownership survives HTTP cancellation and settlement-waiter drops.
    // Catch panic as failure as well: no permanent intent may authorize exec.
    *task = Some(tokio::spawn(async move {
        let success = matches!(
            AssertUnwindSafe(relay.begin_drain()).catch_unwind().await,
            Ok(Ok(_))
        );
        RELOAD.finish(success);
    }));
    // Acceptance is not completion; only successor health makes the helper pass.
    (StatusCode::ACCEPTED, "service reload accepted")
}

/// Called before logging, home preparation, lease acquisition or server startup.
/// Config must have been read without creating anything. Ok(true) means that
/// managed takeover was handled; caller exits instead of proceeding to serve.
pub(crate) async fn handle_takeover(home: &Path, config: &Config, takeover: bool) -> Result<bool> {
    if !takeover || !managed() {
        return Ok(false);
    }
    let deadline = Instant::now() + HELPER_LIMIT;
    let home = home.to_owned();
    let port = config.server.admin_port;
    timeout_at(deadline, async move {
        if port == 0 {
            bail!("managed reload configuration invalid");
        }
        // Only the existing admin token is needed. No client key creation/read.
        let token = tokio::task::spawn_blocking(move || {
            crate::platform::load_existing_secret_file(&home.join("admin.token"))
                .map(Zeroizing::new)
                .map_err(|_| anyhow!("managed reload authentication unavailable"))
        })
        .await
        .map_err(|_| anyhow!("managed reload authentication unavailable"))??;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(REQUEST_LIMIT)
            .timeout(REQUEST_LIMIT)
            .build()
            .map_err(|_| anyhow!("managed reload client unavailable"))?;
        let base = format!("http://127.0.0.1:{port}/admin/v1");
        let old = health(&client, &base, &token)
            .await
            .map_err(|_| anyhow!("managed reload initial health unavailable"))?;
        // Exactly one POST. Ambiguous transport failure does not authorize retry.
        let response = client
            .post(format!("{base}/service/reload"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .map_err(|_| anyhow!("managed reload request failed"))?;
        if response.status() != StatusCode::ACCEPTED {
            bail!("managed reload request refused");
        }
        drop(response);
        loop {
            if let Ok(current) = health(&client, &base, &token).await
                && successor(&old, &current)?
            {
                return Ok(true);
            }
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| anyhow!("managed reload deadline exceeded"))?
}

async fn health(client: &reqwest::Client, base: &str, token: &str) -> Result<HealthResponse> {
    // Bound the entire response, not merely receipt of its headers. Outer helper
    // timeout remains authoritative across all requests and polling delays.
    timeout(REQUEST_LIMIT, async {
        let mut response = client
            .get(format!("{base}/health"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| anyhow!("managed reload health unavailable"))?;
        if response.status() != StatusCode::OK {
            bail!("managed reload health unavailable");
        }
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("managed reload health unavailable"))?
        {
            if chunk.len() > HEALTH_LIMIT.saturating_sub(bytes.len()) {
                bail!("managed reload health invalid");
            }
            bytes.extend_from_slice(&chunk);
        }
        let health: HealthResponse =
            serde_json::from_slice(&bytes).map_err(|_| anyhow!("managed reload health invalid"))?;
        if !health.ok
            || health.pid == 0
            || chrono::DateTime::parse_from_rfc3339(&health.started_at).is_err()
        {
            bail!("managed reload health invalid");
        }
        Ok(health)
    })
    .await
    .map_err(|_| anyhow!("managed reload health unavailable"))?
}

fn successor(old: &HealthResponse, current: &HealthResponse) -> Result<bool> {
    if current.pid != old.pid {
        bail!("managed reload process changed");
    }
    Ok(current.started_at != old.started_at)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "service_http_tests.rs"]
mod http_admin_tests;
