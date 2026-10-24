//! Native tray worker. One OS thread, one current-thread runtime.
//! No UI waits, process enumeration, process termination, or event payload forwarding.
use super::super::client::{AdminClient, CoreHealth, TrayRead};
use std::{
    future::Future,
    os::windows::process::CommandExt,
    pin::Pin,
    process::{Child, Command},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinSet,
};
use windows::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HWND, LPARAM, WPARAM},
    System::Threading::{
        CREATE_NO_WINDOW, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    },
    UI::WindowsAndMessaging::PostMessageW,
};

type Job = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

// Permit lifetime covers queued and active futures, including close draining.
const ORDINARY_BUDGET: usize = 64;
const CANCEL_BUDGET: usize = 4;
struct AdmissionLane {
    sender: mpsc::Sender<Job>,
    budget: Arc<Semaphore>,
}
impl AdmissionLane {
    fn new(limit: usize) -> (Self, mpsc::Receiver<Job>) {
        let (sender, receiver) = mpsc::channel(limit);
        (
            Self {
                sender,
                budget: Arc::new(Semaphore::new(limit)),
            },
            receiver,
        )
    }
    fn submit(&self, job: impl Future<Output = ()> + Send + 'static) -> bool {
        let Ok(permit) = self.budget.clone().try_acquire_owned() else {
            return false;
        };
        self.sender
            .try_send(Box::pin(async move {
                let _permit = permit;
                job.await;
            }))
            .is_ok()
    }
}

pub(super) struct Worker {
    pub(super) dashboard: Arc<super::dashboard_io::Coordinator>,
    jobs: AdmissionLane,
    urgent: AdmissionLane,
    admission: Mutex<()>,
    refresh: mpsc::Sender<()>,
    visible: watch::Sender<bool>,
    flyout_visible: AtomicBool,
    closing: watch::Sender<bool>,
    supervise: Arc<AtomicBool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Worker {
    pub(super) fn start(hwnd: isize, supervise: bool) -> anyhow::Result<Self> {
        let language = qb_tray::i18n::language();
        let dashboard = super::dashboard_io::Coordinator::new(hwnd);
        let dashboard_io = dashboard.clone();
        let (jobs, mut job_rx) = AdmissionLane::new(ORDINARY_BUDGET);
        let (urgent, mut urgent_rx) = AdmissionLane::new(CANCEL_BUDGET);
        let (refresh, mut refresh_rx) = mpsc::channel(1);
        let (visible, visible_rx) = watch::channel(false);
        let (closing, mut close_rx) = watch::channel(false);
        let enabled = Arc::new(AtomicBool::new(supervise));
        let supervision = enabled.clone();
        let refresh_tx = refresh.clone();
        // Runtime construction failure is reported to the UI asynchronously; no global
        // refresh latch exists that could strand future updates after a spawn failure.
        let thread = std::thread::Builder::new().name("quota-baton-tray-io".into()).spawn(move || {
            qb_tray::i18n::set_language(language);
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(_) => { unavailable(hwnd); return; }
            };
            runtime.block_on(async move {
                let health = Arc::new(Mutex::new(CoreHealth::Unhealthy { reason: "starting".into() }));
                let mut tasks = JoinSet::new();
                let health_task = tokio::spawn(health_loop(hwnd, supervision.clone(), health.clone(), refresh_tx.clone()));
                let events_task = tokio::spawn(events_loop(hwnd, visible_rx, refresh_tx, dashboard_io.clone()));
                let mut dashboard_task=tokio::spawn(dashboard_io.clone().run());
                let mut refreshing: Option<tokio::task::JoinHandle<()>> = None;
                let mut dirty = false;
                loop {
                    tokio::select! {
                        biased;
                        _ = close_rx.changed() => break,
                        Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                        Some(job) = urgent_rx.recv() => { tasks.spawn(job); }
                        Some(job) = job_rx.recv() => { tasks.spawn(job); }
                        Some(()) = refresh_rx.recv() => { dirty = true; }
                        _ = async { refreshing.as_mut().expect("guarded").await.ok(); }, if refreshing.is_some() => {
                            refreshing = None;
                        }
                    }
                    if dirty && refreshing.is_none() {
                        dirty = false;
                        refreshing = Some(tokio::spawn(refresh_state(hwnd, health.clone())));
                    }
                }
                supervision.store(false, Ordering::Release);
                job_rx.close();
                urgent_rx.close();
                health_task.abort();
                events_task.abort();
                dashboard_io.stop();
                if let Some(task) = refreshing { task.abort(); }
                // Drain accepted actions for up to five seconds on this worker, never
                // join from WM_DESTROY. Aborting drops all outstanding HTTP futures.
                while let Ok(job) = urgent_rx.try_recv() { tasks.spawn(job); }
                while let Ok(job) = job_rx.try_recv() { tasks.spawn(job); }
                let _ = tokio::time::timeout(Duration::from_secs(5), async {
                    while tasks.join_next().await.is_some() {}
                }).await;
                if tokio::time::timeout(Duration::from_secs(5),&mut dashboard_task).await.is_err(){dashboard_task.abort();}
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            });
        })?;
        Ok(Self {
            dashboard,
            jobs,
            urgent,
            admission: Mutex::new(()),
            refresh,
            visible,
            flyout_visible: AtomicBool::new(false),
            closing,
            supervise: enabled,
            thread: Mutex::new(Some(thread)),
        })
    }
    pub(super) fn submit(&self, job: impl Future<Output = ()> + Send + 'static) -> bool {
        let _guard = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        !*self.closing.borrow() && self.jobs.submit(job)
    }
    pub(super) fn submit_cancel(&self, job: impl Future<Output = ()> + Send + 'static) -> bool {
        let _guard = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        !*self.closing.borrow() && self.urgent.submit(job)
    }
    pub(super) fn refresh(&self) {
        let _ = self.refresh.try_send(());
    }
    pub(super) fn visible(&self, visible: bool) {
        self.flyout_visible.store(visible, Ordering::Release);
        self.refresh_interest();
    }
    pub(super) fn refresh_interest(&self) {
        let interested = self.flyout_visible.load(Ordering::Acquire) || self.dashboard.is_open();
        if *self.visible.borrow() != interested {
            self.visible.send_replace(interested);
        }
    }
    pub(super) fn disable_supervision(&self) {
        self.supervise.store(false, Ordering::Release);
    }
    /// Only after the GUI message loop has ended, never from a window callback.
    pub(super) fn finish(&self) {
        self.close();
        if let Some(thread) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = thread.join();
        }
    }
    pub(super) fn close(&self) {
        let _guard = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        self.disable_supervision();
        self.dashboard.stop();
        self.visible.send_replace(false);
        self.closing.send_replace(true);
    }
}

fn post(hwnd: isize, message: u32) {
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut std::ffi::c_void)),
            message,
            WPARAM(0),
            LPARAM(0),
        );
    }
}
fn unavailable(hwnd: isize) {
    super::notices()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .unavailable(0);
    let mut model = super::model().lock().unwrap_or_else(|e| e.into_inner());
    model.2 = false;
    super::invalidate_model();
    drop(model);
    post(hwnd, super::STATE_CHANGED);
}
async fn refresh_state(hwnd: isize, health: Arc<Mutex<CoreHealth>>) {
    let result: anyhow::Result<TrayRead> = async {
        let client = AdminClient::from_home()?;
        let snapshot = client.snapshot().await?;
        let health = health.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Ok(TrayRead { health, snapshot })
    }
    .await;
    match result {
        Ok(read) => {
            if super::publish_read(read).is_ok() {
                post(hwnd, super::STATE_CHANGED);
            } else {
                unavailable(hwnd);
            }
        }
        Err(_) => unavailable(hwnd),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Observation {
    Healthy(u32),
    Unhealthy,
    Unreachable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presence {
    Gone,
    Live,
    Unknown,
}

/// Pure supervision policy. Only health may establish the owned PID. Access
/// denied and PID reuse are conservative: neither licenses a restart.
#[derive(Debug)]
struct Policy {
    startup: bool,
    bootstrap_retry: bool,
    unreachable_since: Option<Duration>,
    last_attempt: Option<Duration>,
    delay: Duration,
    owned_pid: Option<u32>,
    pending: Option<(u32, bool)>,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            startup: true,
            bootstrap_retry: false,
            unreachable_since: None,
            last_attempt: None,
            delay: Duration::from_secs(5),
            owned_pid: None,
            pending: None,
        }
    }
}
impl Policy {
    fn observe(&mut self, now: Duration, health: Observation, presence: Presence) -> (bool, bool) {
        let startup = std::mem::replace(&mut self.startup, false);
        match health {
            Observation::Healthy(pid) => {
                self.bootstrap_retry = false;
                self.owned_pid = Some(pid);
                let restarted = self
                    .pending
                    .is_some_and(|(spawned, restart)| spawned == pid && restart);
                // A healthy *other* PID (lease owner) also disarms a pending toast.
                self.pending = None;
                self.unreachable_since = None;
                self.last_attempt = None;
                self.delay = Duration::from_secs(5);
                (false, restarted)
            }
            Observation::Unhealthy => {
                self.bootstrap_retry = false;
                self.unreachable_since = None;
                self.pending = None;
                (false, false)
            }
            Observation::Unreachable => {
                let since = *self.unreachable_since.get_or_insert(now);
                let due = self
                    .last_attempt
                    .is_none_or(|at| now.saturating_sub(at) >= self.delay);
                let restart = self.owned_pid.is_some()
                    && presence == Presence::Gone
                    && now.saturating_sub(since) >= Duration::from_secs(30);
                let bootstrap =
                    self.bootstrap_retry && now.saturating_sub(since) >= Duration::from_secs(30);
                (due && (startup || restart || bootstrap), false)
            }
        }
    }
    fn child_exited(&mut self) {
        self.bootstrap_retry = self.owned_pid.is_none();
        self.pending = None;
    }

    fn attempted(&mut self, now: Duration, pid: Option<u32>, restart: bool) {
        if self.last_attempt.is_some() {
            self.delay = (self.delay * 2).min(Duration::from_secs(300));
        }
        self.last_attempt = Some(now);
        self.bootstrap_retry = pid.is_none() && self.owned_pid.is_none();
        self.pending = pid.map(|pid| (pid, restart));
    }
}

fn known_pid_presence(pid: Option<u32>) -> Presence {
    let Some(pid) = pid.filter(|pid| *pid != 0) else {
        return Presence::Unknown;
    };
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let mut code = 0;
                let result = GetExitCodeProcess(handle, &mut code);
                let _ = CloseHandle(handle);
                match result {
                    Ok(()) if code == 259 => Presence::Live,
                    Ok(()) => Presence::Gone,
                    Err(_) => Presence::Unknown,
                }
            }
            Err(error)
                if error.code()
                    == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) =>
            {
                Presence::Gone
            }
            Err(_) => Presence::Unknown,
        }
    }
}
fn spawn_core() -> std::io::Result<Child> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("quota-baton.exe")));
    let binary = sibling
        .filter(|p| p.is_file())
        .unwrap_or_else(|| "quota-baton".into());
    Command::new(binary)
        .arg("serve")
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
}
/// Missing credentials alone do not establish an outage. Only a bounded TCP
/// connect to the numeric loopback config port can allow first startup.
fn bootstrap_target(
    token_missing: bool,
    load_port: impl FnOnce() -> Result<u16, ()>,
) -> Option<std::net::SocketAddr> {
    if !token_missing {
        return None;
    }
    let port = load_port().ok().filter(|port| *port != 0)?;
    Some(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
}

#[derive(Clone, Copy)]
enum PortProbe {
    Responding,
    Unreachable,
    Indeterminate,
}
fn bootstrap_health(result: PortProbe) -> CoreHealth {
    match result {
        PortProbe::Unreachable => CoreHealth::Unreachable {
            reason: "admin port unreachable without token".into(),
        },
        PortProbe::Responding | PortProbe::Indeterminate => CoreHealth::Unhealthy {
            reason: "admin credentials unavailable".into(),
        },
    }
}
async fn observe_health() -> CoreHealth {
    let unhealthy = || CoreHealth::Unhealthy {
        reason: "admin configuration".into(),
    };
    if let Ok(client) = AdminClient::from_home() {
        return client.health().await.unwrap_or_else(|_| unhealthy());
    }
    let Ok(home) = qb_core::home() else {
        return unhealthy();
    };
    // symlink_metadata rejects dangling symlinks, directories, unreadable tokens,
    // and existing malformed tokens; never mint secrets or read lease-free creds.
    let missing = matches!(std::fs::symlink_metadata(home.join("admin.token")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound);
    let Some(address) = bootstrap_target(missing, || {
        qb_core::config::Config::load(&home)
            .map(|c| c.server.admin_port)
            .map_err(|_| ())
    }) else {
        return unhealthy();
    };
    let result = match tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect(address),
    )
    .await
    {
        Ok(Ok(_stream)) => PortProbe::Responding,
        Err(_) => PortProbe::Unreachable,
        Ok(Err(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::NetworkUnreachable
                    | std::io::ErrorKind::HostUnreachable
            ) =>
        {
            PortProbe::Unreachable
        }
        Ok(Err(_)) => PortProbe::Indeterminate,
    };
    bootstrap_health(result)
}

async fn health_loop(
    hwnd: isize,
    enabled: Arc<AtomicBool>,
    cache: Arc<Mutex<CoreHealth>>,
    refresh: mpsc::Sender<()>,
) {
    let started = Instant::now();
    let mut policy = Policy::default();
    let mut child: Option<Child> = None;
    let mut timer = tokio::time::interval(Duration::from_secs(10));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        timer.tick().await; // first tick is immediate
        let health = observe_health().await;
        let observation = match &health {
            CoreHealth::Healthy(h) if h.pid != 0 => Observation::Healthy(h.pid),
            CoreHealth::Unreachable { .. } => Observation::Unreachable,
            _ => Observation::Unhealthy,
        };
        *cache.lock().unwrap_or_else(|e| e.into_inner()) = health;
        let presence = known_pid_presence(policy.owned_pid);
        // Exact child handle only; no name-based inspection. Lease-busy child
        // exit disarms the candidate without producing a restart notification.
        let child_running = match child.as_mut().map(Child::try_wait) {
            Some(Ok(Some(_))) => {
                child = None;
                policy.child_exited();
                false
            }
            Some(Ok(None)) | Some(Err(_)) => true,
            None => false,
        };
        let restart = policy.owned_pid.is_some();
        let now = started.elapsed();
        let (spawn, confirmed) = policy.observe(now, observation, presence);
        if confirmed
            && child_running
            && matches!(observation, Observation::Healthy(pid)
            if known_pid_presence(Some(pid)) == Presence::Live)
        {
            post(hwnd, super::CORE_RESTART_CONFIRMED);
        }
        if spawn && !child_running && enabled.load(Ordering::Acquire) {
            child = spawn_core().ok();
            policy.attempted(now, child.as_ref().map(Child::id), restart);
        }
        let _ = refresh.try_send(());
    }
}

const MAX_FRAME: usize = 4096;
const MAX_EVENT_NAME: usize = 128;

#[derive(Default, PartialEq, Eq)]
enum EventKind {
    #[default]
    Unknown,
    Refresh,
    CoreWarning,
    Quota,
    Sessions,
    Activity,
}
use qb_proto::admin_sse::{BurstWarning, WarningKind};
#[derive(serde::Deserialize)]
struct SessionChangeSignal {
    #[serde(default)]
    resync: bool,
}

/// Bounded frame parser. Only a validated numeric observation crosses to UI.
/// Oversized data suppresses burst decoding, but recognized events still resync.
#[derive(Default)]
struct EventParser {
    line: Vec<u8>,
    line_overflow: bool,
    frame_bytes: usize,
    event: EventKind,
    data: Vec<u8>,
    dashboard_flags: u8,
}
impl EventParser {
    fn feed(&mut self, bytes: &[u8], mut burst: impl FnMut(u32)) -> bool {
        let mut refresh = false;
        for &byte in bytes {
            if byte == b'\n' && !self.line_overflow && (self.line.is_empty() || self.line == b"\r")
            {
                use super::dashboard_io as d;
                refresh |= matches!(
                    self.event,
                    EventKind::Refresh | EventKind::CoreWarning | EventKind::Quota
                );
                self.dashboard_flags |= match self.event {
                    EventKind::Refresh => d::STATE | d::SESSIONS | d::ACTIVITY,
                    EventKind::CoreWarning => d::ACTIVITY,
                    EventKind::Quota => d::STATE | d::SESSIONS | d::USAGE,
                    EventKind::Sessions => d::SESSIONS,
                    EventKind::Activity => d::ACTIVITY,
                    EventKind::Unknown => 0,
                };
                if self.event == EventKind::Sessions
                    && (self.frame_bytes > MAX_FRAME
                        || serde_json::from_slice::<SessionChangeSignal>(&self.data)
                            .map_or(true, |signal| signal.resync))
                {
                    // A lag marker can stand in for lost account/activity/quota
                    // events too. Only the boolean crosses this parser boundary.
                    self.dashboard_flags |= d::STATE | d::SESSIONS | d::ACTIVITY | d::USAGE;
                    refresh = true;
                }
                if self.event == EventKind::CoreWarning
                    && self.frame_bytes <= MAX_FRAME
                    && let Ok(warning) = serde_json::from_slice::<BurstWarning>(&self.data)
                    && matches!(warning.kind, WarningKind::Burst)
                    && warning.burst_new_sessions > 0
                {
                    burst(warning.burst_new_sessions);
                }
                self.line.clear();
                self.data.clear();
                self.frame_bytes = 0;
                self.event = EventKind::Unknown;
                continue;
            }
            self.frame_bytes = self.frame_bytes.saturating_add(1).min(MAX_FRAME + 1);
            if self.frame_bytes > MAX_FRAME {
                self.data.clear(); // no partial/oversize JSON decoding, even after later event:
            }
            if byte == b'\n' {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                if !self.line_overflow {
                    if let Some(name) = self.line.strip_prefix(b"event:") {
                        let name = name.strip_prefix(b" ").unwrap_or(name);
                        self.event = if name.len() > MAX_EVENT_NAME {
                            EventKind::Unknown
                        } else {
                            match name {
                                b"core_warning" => EventKind::CoreWarning,
                                b"account_changed" | b"login_progress" => EventKind::Refresh,
                                b"quota_updated" => EventKind::Quota,
                                b"sessions_changed" => EventKind::Sessions,
                                b"activity" => EventKind::Activity,
                                _ => EventKind::Unknown,
                            }
                        };
                    } else if self.frame_bytes <= MAX_FRAME
                        && let Some(data) = self.line.strip_prefix(b"data:")
                    {
                        let data = data.strip_prefix(b" ").unwrap_or(data);
                        if !self.data.is_empty() {
                            self.data.push(b'\n');
                        }
                        self.data.extend_from_slice(data);
                    }
                } else if self.line.starts_with(b"event:") {
                    self.event = EventKind::Unknown;
                }
                self.line.clear();
                self.line_overflow = false;
            } else if self.line.len() < MAX_FRAME {
                self.line.push(byte);
            } else {
                self.line_overflow = true;
            }
        }
        refresh
    }
}
fn post_burst(hwnd: isize, count: u32) {
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut std::ffi::c_void)),
            super::CORE_BURST_OBSERVED,
            WPARAM(count as usize),
            LPARAM(0),
        );
    }
}
async fn events_loop(
    hwnd: isize,
    mut visible: watch::Receiver<bool>,
    refresh: mpsc::Sender<()>,
    dashboard: Arc<super::dashboard_io::Coordinator>,
) {
    loop {
        if !*visible.borrow_and_update() {
            if visible.changed().await.is_err() {
                return;
            }
            continue;
        }
        // Subscription includes reconnects; visibility changes drop the entire
        // future, including an in-flight connect, chunk read, or reconnect delay.
        tokio::select! {
            _ = visible.changed() => {},
            _ = async {
                loop {
                    dashboard.stream(false);
                    let _ = refresh.try_send(()); // authoritative resync after any gap
                    if let Ok(client) = AdminClient::from_home()
                        && let Ok(mut response) = client.events().await
                        && response.status().is_success()
                        && response.headers().get(reqwest::header::CONTENT_TYPE)
                            .and_then(|h| h.to_str().ok())
                            .is_some_and(|h| h.split(';').next() == Some("text/event-stream"))
                    {
                        dashboard.stream(true);
                        dashboard.dirty(super::dashboard_io::STATE|super::dashboard_io::SESSIONS|super::dashboard_io::ACTIVITY|super::dashboard_io::USAGE);
                        // Subscribe first, then resync: events racing with the
                        // state read are now buffered instead of falling in a gap.
                        let _ = refresh.try_send(());
                        let mut parser = EventParser::default();
                        while let Ok(Some(chunk)) = response.chunk().await {
                            if parser.feed(&chunk, |count| post_burst(hwnd, count)) { let _ = refresh.try_send(()); }
                            dashboard.dirty(std::mem::take(&mut parser.dashboard_flags));
                        }
                    }
                    dashboard.stream(false);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            } => {},
        }
        dashboard.stream(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dashboard_sse_lag_recovers_all_views_but_regular_sessions_do_not_poll_history() {
        use super::super::dashboard_io as d;
        for (resync, expected, refresh) in [
            (false, d::SESSIONS, false),
            (true, d::STATE | d::SESSIONS | d::ACTIVITY | d::USAGE, true),
        ] {
            let mut parser = EventParser::default();
            let frame = format!(
                "event: sessions_changed\ndata: {{\"resync\":{resync},\"changed\":[\"123456abcdef\"],\"removed\":[]}}\n\n"
            );
            let mut actual_refresh = false;
            for byte in frame.bytes() {
                actual_refresh |= parser.feed(&[byte], |_| panic!("not burst"));
            }
            assert_eq!(actual_refresh, refresh);
            assert_eq!(parser.dashboard_flags, expected);
            assert!(parser.data.is_empty());
        }
    }
    #[test]
    fn admission_bounds_active_jobs_and_reserves_finite_cancellation_lane() {
        let (ordinary, mut rx) = AdmissionLane::new(ORDINARY_BUDGET);
        let (urgent, mut cancel_rx) = AdmissionLane::new(CANCEL_BUDGET);
        let mut active = Vec::new();
        for _ in 0..ORDINARY_BUDGET {
            assert!(ordinary.submit(std::future::pending()));
            active.push(rx.try_recv().unwrap());
        }
        assert!(!ordinary.submit(async {})); // queue empty; permits active
        let mut cancels = Vec::new();
        for _ in 0..CANCEL_BUDGET {
            assert!(urgent.submit(std::future::pending()));
            cancels.push(cancel_rx.try_recv().unwrap());
        }
        assert!(!urgent.submit(async {}));
        drop(cancels.pop());
        assert!(urgent.submit(async {}));
        drop(active.pop());
        assert!(ordinary.submit(async {}));
        assert_eq!(ordinary.budget.available_permits(), 0);
    }
    #[test]
    fn completion_drop_and_closed_drain_release_permits() {
        let (lane, mut rx) = AdmissionLane::new(2);
        assert!(lane.submit(async {}));
        assert!(lane.submit(std::future::pending()));
        rx.close();
        let mut completed = rx.try_recv().unwrap();
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(completed.as_mut().poll(&mut context).is_ready());
        assert_eq!(lane.budget.available_permits(), 1);
        assert!(!lane.submit(async {})); // failed enqueue releases its permit
        assert_eq!(lane.budget.available_permits(), 1);
        let mut draining = rx.try_recv().unwrap();
        assert!(draining.as_mut().poll(&mut context).is_pending());
        drop(draining); // close timeout abort/drop
        assert_eq!(lane.budget.available_permits(), 2);
    }
    #[test]
    fn close_rejects_both_lanes_and_drain_cannot_exceed_total_budget() {
        let (jobs, mut rx) = AdmissionLane::new(ORDINARY_BUDGET);
        let (urgent, mut urgent_rx) = AdmissionLane::new(CANCEL_BUDGET);
        let (refresh, _refresh_rx) = mpsc::channel(1);
        let (visible, _visible_rx) = watch::channel(false);
        let (closing, _closing_rx) = watch::channel(false);
        let worker = Worker {
            dashboard: super::super::dashboard_io::Coordinator::new(0),
            jobs,
            urgent,
            admission: Mutex::new(()),
            refresh,
            visible,
            flyout_visible: AtomicBool::new(false),
            closing,
            supervise: Arc::new(AtomicBool::new(true)),
            thread: Mutex::new(None),
        };
        let mut active = Vec::new();
        for n in 0..ORDINARY_BUDGET {
            assert!(worker.submit(std::future::pending()));
            if n % 2 == 0 {
                active.push(rx.try_recv().unwrap());
            }
        }
        for _ in 0..CANCEL_BUDGET {
            assert!(worker.submit_cancel(std::future::pending()));
        }
        worker.close();
        assert!(!worker.submit(async {}));
        assert!(!worker.submit_cancel(async {}));
        rx.close();
        urgent_rx.close();
        while let Ok(job) = urgent_rx.try_recv() {
            active.push(job);
        }
        while let Ok(job) = rx.try_recv() {
            active.push(job);
        }
        assert_eq!(active.len(), ORDINARY_BUDGET + CANCEL_BUDGET);
        drop(active);
        assert_eq!(worker.jobs.budget.available_permits(), ORDINARY_BUDGET);
        assert_eq!(worker.urgent.budget.available_permits(), CANCEL_BUDGET);
    }
    fn t(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }
    #[test]
    fn startup_unreachable_is_immediate_but_unknown_pid_never_later_restarts() {
        let mut p = Policy::default();
        assert_eq!(
            p.observe(t(0), Observation::Unreachable, Presence::Unknown),
            (true, false)
        );
        p.attempted(t(0), Some(100), false);
        assert_eq!(
            p.observe(t(300), Observation::Unreachable, Presence::Unknown),
            (false, false)
        );
        p.child_exited(); // exact child handle established its exit
        assert_eq!(
            p.observe(t(301), Observation::Unreachable, Presence::Unknown),
            (true, false)
        );
        p.attempted(t(301), None, false); // CreateProcess failure: no unknown process
        assert!(
            !p.observe(t(310), Observation::Unreachable, Presence::Unknown)
                .0
        );
        assert!(
            p.observe(t(311), Observation::Unreachable, Presence::Unknown)
                .0
        );
    }
    #[test]
    fn restart_requires_continuous_outage_known_gone_pid_and_backoff() {
        let mut p = Policy::default();
        p.observe(t(0), Observation::Healthy(42), Presence::Unknown);
        assert!(!p.observe(t(10), Observation::Unreachable, Presence::Gone).0);
        assert!(!p.observe(t(39), Observation::Unreachable, Presence::Gone).0);
        assert!(!p.observe(t(40), Observation::Unreachable, Presence::Live).0);
        assert!(
            !p.observe(t(40), Observation::Unreachable, Presence::Unknown)
                .0
        );
        assert!(p.observe(t(40), Observation::Unreachable, Presence::Gone).0);
        p.attempted(t(40), None, true);
        assert!(!p.observe(t(44), Observation::Unreachable, Presence::Gone).0);
        assert!(p.observe(t(45), Observation::Unreachable, Presence::Gone).0);
        p.observe(t(51), Observation::Unhealthy, Presence::Gone);
        assert!(!p.observe(t(90), Observation::Unreachable, Presence::Gone).0);
    }
    #[test]
    fn toast_requires_same_spawned_pid_health_and_restart_not_startup() {
        let mut p = Policy::default();
        p.attempted(t(0), Some(10), false);
        assert!(!p.observe(t(1), Observation::Healthy(10), Presence::Live).1);
        p.attempted(t(2), Some(11), true);
        assert!(!p.observe(t(3), Observation::Healthy(12), Presence::Live).1);
        p.attempted(t(4), Some(13), true);
        assert!(p.observe(t(5), Observation::Healthy(13), Presence::Live).1);
        assert!(!p.observe(t(6), Observation::Healthy(13), Presence::Live).1);
    }
    #[test]
    fn lease_busy_child_exit_disarms_restart_toast_even_if_old_pid_is_reported() {
        let mut p = Policy::default();
        p.observe(t(0), Observation::Healthy(40), Presence::Live);
        p.attempted(t(50), Some(41), true);
        p.child_exited();
        assert!(!p.observe(t(60), Observation::Healthy(41), Presence::Live).1);
    }
    #[test]
    fn unhealthy_resets_the_continuous_unreachable_window() {
        let mut p = Policy::default();
        p.observe(t(0), Observation::Healthy(40), Presence::Live);
        p.observe(t(10), Observation::Unreachable, Presence::Gone);
        assert_eq!(
            p.observe(t(39), Observation::Unhealthy, Presence::Gone),
            (false, false)
        );
        assert!(!p.observe(t(40), Observation::Unreachable, Presence::Gone).0);
        assert!(!p.observe(t(69), Observation::Unreachable, Presence::Gone).0);
        assert!(p.observe(t(70), Observation::Unreachable, Presence::Gone).0);
    }
    #[test]
    fn unhealthy_never_spawns_and_backoff_caps() {
        let mut p = Policy::default();
        assert_eq!(
            p.observe(t(0), Observation::Unhealthy, Presence::Gone),
            (false, false)
        );
        for i in 0..20 {
            p.attempted(t(i), None, true);
        }
        assert_eq!(p.delay, t(300));
    }
    #[test]
    fn bootstrap_missing_token_projects_closed_open_and_invalid_config() {
        let address = bootstrap_target(true, || Ok(43210)).unwrap();
        assert!(address.ip().is_loopback());
        assert_eq!(address.port(), 43210); // projection only, no socket operation
        assert!(bootstrap_target(true, || Err(())).is_none()); // malformed/unreadable config
        assert!(bootstrap_target(true, || Ok(0)).is_none());
        assert!(
            bootstrap_target(false, || panic!("existing/invalid token must not probe")).is_none()
        );
        for (probe, should_spawn) in [
            (PortProbe::Unreachable, true),
            (PortProbe::Responding, false),
            (PortProbe::Indeterminate, false),
        ] {
            let observation = match bootstrap_health(probe) {
                CoreHealth::Unreachable { .. } => Observation::Unreachable,
                _ => Observation::Unhealthy,
            };
            assert_eq!(
                Policy::default()
                    .observe(t(0), observation, Presence::Unknown)
                    .0,
                should_spawn
            );
        }
    }
    #[test]
    fn sse_actual_burst_and_data_before_event_refresh_and_project_only_count() {
        let mut p = EventParser::default();
        let mut counts = Vec::new();
        assert!(p.feed(
            b"event: core_warning\ndata: {\"kind\":\"burst\",\"burst_new_sessions\":7}\n\n",
            |n| counts.push(n)
        ));
        assert!(p.feed(
            b"data: {\"kind\":\"burst\",\"burst_new_sessions\":9}\r\nevent: core_warning\r\n\r\n",
            |n| counts.push(n)
        ));
        assert_eq!(counts, [7, 9]);
    }
    #[test]
    fn sse_every_byte_fragmented_lf_and_crlf() {
        for newline in ["\n", "\r\n"] {
            let mut p = EventParser::default();
            let mut counts = Vec::new();
            let frame = format!(
                "event: core_warning{newline}data: {{\"kind\":\"burst\",\"burst_new_sessions\":3}}{newline}{newline}"
            );
            let mut refresh = false;
            for byte in frame.bytes() {
                refresh |= p.feed(&[byte], |n| counts.push(n));
            }
            assert!(refresh);
            assert_eq!(counts, [3]);
        }
    }
    #[test]
    fn sse_quota_warning_resyncs_authoritative_snapshot_without_projecting_burst() {
        for newline in ["\n", "\r\n"] {
            let mut parser = EventParser::default();
            let frame = format!(
                "event: core_warning{newline}data: {{\"kind\":\"quota_notice\",\"account\":\"cx-synthetic-notice\",\"action\":\"failover\",\"next_account\":\"cx-synthetic-next\",\"observed_at\":\"2026-10-18T12:00:00Z\"}}{newline}{newline}"
            );
            let mut resyncs = 0;
            for byte in frame.bytes() {
                if parser.feed(&[byte], |_| panic!("quota warning must not become a burst")) {
                    resyncs += 1;
                }
            }
            assert_eq!(resyncs, 1);
            assert!(parser.data.is_empty() && parser.line.is_empty());
            assert!(!parser.feed(b"\n", |_| panic!("empty frame")));
        }
    }
    #[test]
    fn sse_oversize_still_refreshes_but_never_projects_burst() {
        for data_first in [false, true] {
            let mut p = EventParser::default();
            let no_burst = |_| panic!("oversize burst");
            if !data_first {
                assert!(!p.feed(b"event: core_warning\n", no_burst));
            }
            p.feed(b"data: ", no_burst);
            p.feed(&vec![b'x'; 100_000], no_burst);
            assert!(p.line.len() + p.data.len() <= MAX_FRAME);
            p.feed(b"\n", no_burst);
            if data_first {
                p.feed(b"event: core_warning\n", no_burst);
            }
            assert!(p.feed(b"\n", no_burst));
            assert!(!p.feed(b"\n", no_burst));
        }
        // Many individually short lines must also obey the whole-frame bound.
        let mut p = EventParser::default();
        for _ in 0..1000 {
            p.feed(b"data: padding\n", |_| panic!("burst"));
        }
        assert!(p.data.len() <= MAX_FRAME);
        assert!(p.feed(b"event: quota_updated\n\n", |_| panic!("burst")));
    }
    #[test]
    fn sse_valid_json_at_frame_bound_and_after_oversize_recovers() {
        let mut p = EventParser::default();
        let body = "event: core_warning\ndata: {\"kind\":\"burst\",\"burst_new_sessions\":4}";
        let mut counts = Vec::new();
        for (size, expected) in [(MAX_FRAME, 1), (MAX_FRAME + 1, 1), (MAX_FRAME, 2)] {
            // Count non-separator wire bytes, including the data line's LF.
            let frame = format!("{body}{}\n\n", " ".repeat(size - body.len() - 1));
            assert!(p.feed(frame.as_bytes(), |n| counts.push(n)));
            assert_eq!(counts.len(), expected);
        }
        assert_eq!(counts, [4, 4]);
    }
    #[test]
    fn sse_unknown_bare_code_and_invalid_counts_never_project_burst() {
        let mut p = EventParser::default();
        for payload in [
            r#"{"code":"burst","burst_new_sessions":3}"#,
            r#"{"kind":"burst"}"#,
            r#"{"kind":"burst","burst_new_sessions":0}"#,
            r#"{"kind":"burst","burst_new_sessions":-1}"#,
            r#"{"kind":"burst","burst_new_sessions":1.5}"#,
            r#"{"kind":"burst","burst_new_sessions":"3"}"#,
            r#"{"kind":"burst","burst_new_sessions":4294967296}"#,
            "not json",
        ] {
            let frame = format!("event: core_warning\ndata: {payload}\n\n");
            assert!(p.feed(frame.as_bytes(), |_| panic!("invalid burst")));
        }
        for name in ["unknown".to_owned(), "x".repeat(MAX_EVENT_NAME + 1)] {
            let frame =
                format!("event: {name}\ndata: {{\"kind\":\"burst\",\"burst_new_sessions\":3}}\n\n");
            assert!(!p.feed(frame.as_bytes(), |_| panic!("unknown event")));
        }
        assert!(!p.feed(
            b"data: {\"kind\":\"burst\",\"burst_new_sessions\":3}\n\n: ping\n\n",
            |_| panic!("no event")
        ));
        assert!(p.feed(
            b"event: login_progress\n\nevent: account_changed\n\n",
            |_| panic!("refresh only")
        ));
    }
}

#[cfg(test)]
mod r15_shared_burst_projection_tests {
    use super::*;

    #[test]
    fn r15_sse_shared_body_projects_positive_count_only() {
        for count in [0, 7, u32::MAX] {
            let data = serde_json::to_string(&BurstWarning {
                kind: WarningKind::Burst,
                burst_new_sessions: count,
            })
            .unwrap();
            assert_eq!(
                data,
                format!(r#"{{"kind":"burst","burst_new_sessions":{count}}}"#)
            );
            let frame = format!("event: core_warning\ndata: {data}\n\n");
            let mut parser = EventParser::default();
            let mut counts = Vec::new();
            assert!(parser.feed(frame.as_bytes(), |n| counts.push(n)));
            assert_eq!(counts, if count > 0 { vec![count] } else { vec![] });
        }
    }
}
