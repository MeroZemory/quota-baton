//! Dashboard I/O on the existing tray runtime. The UI only coalesces intent.
use super::dashboard_window::{Bridge, Event, UiPort};
use crate::{
    client::AdminClient,
    dashboard_persistence::{self, Geometry},
};
use chrono::{Local, TimeZone, Utc};
use qb_tray::{
    dashboard_model as model,
    state::{Command, DetailTicket, Range},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::sync::Notify;
use tokio::time::Instant;
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::WindowsAndMessaging::PostMessageW,
};

pub(super) const STATE: u8 = 1;
pub(super) const SESSIONS: u8 = 2;
pub(super) const ACTIVITY: u8 = 4;
pub(super) const USAGE: u8 = 8;
const DETAIL: u8 = 16;
const CLOCK: u8 = 32;
const ALL: u8 = STATE | SESSIONS | ACTIVITY;

fn record_data_publication(
    data: &mut model::DashboardSnapshot,
    dirty: u8,
    ok: bool,
    clock: &model::Clock,
) {
    // Failed batches can retain older fields; do not label them as fresh.
    if ok && dirty & (ALL | USAGE) != 0 {
        data.data_at = Some(clock.utc());
    }
}

pub(super) enum Handoff {
    Account(String),
    Add,
}
struct PendingGeometry {
    owner: u64,
    sequence: u64,
    value: Geometry,
    revision: u64,
}
struct Control {
    port: Option<UiPort>,
    intent: u64,
    dirty: u8,
    range: Range,
    days: u8,
    detail: Option<DetailTicket>,
    usage: bool,
    stream: bool,
    load_geometry: bool,
    geometry_known: bool,
    last_geometry: Option<Geometry>,
    pending_geometry: Option<PendingGeometry>,
    geometry_revision: u64,
    saved_geometry: (u64, u64),
    handoff: Option<(u64, Handoff)>,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            port: None,
            intent: 0,
            dirty: 0,
            range: Range::Hour,
            days: 2,
            detail: None,
            usage: false,
            stream: false,
            load_geometry: false,
            geometry_known: false,
            last_geometry: None,
            pending_geometry: None,
            geometry_revision: 0,
            saved_geometry: (0, 0),
            handoff: None,
        }
    }
}
pub(super) struct Coordinator {
    #[cfg(test)]
    test_client: Mutex<Option<AdminClient>>,
    control: Mutex<Control>,
    changed: Notify,
    stop: AtomicBool,
    open: AtomicBool,
    parent: isize,
}
struct Work {
    port: UiPort,
    intent: u64,
    dirty: u8,
    range: Range,
    days: u8,
    detail: Option<DetailTicket>,
    usage: bool,
    stream: bool,
    geometry: bool,
    geometry_revision: u64,
}
impl Coordinator {
    pub(super) fn new(parent: isize) -> Arc<Self> {
        Arc::new(Self {
            #[cfg(test)]
            test_client: Mutex::new(None),
            control: Mutex::new(Control::default()),
            changed: Notify::new(),
            stop: AtomicBool::new(false),
            open: AtomicBool::new(false),
            parent,
        })
    }
    pub(super) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
    fn client(&self) -> anyhow::Result<AdminClient> {
        #[cfg(test)]
        if let Some(client) = self.test_client.lock().unwrap().as_ref() {
            return Ok(client.clone());
        }
        AdminClient::from_home()
    }
    pub(super) fn dirty(&self, flags: u8) {
        if flags == 0 {
            return;
        }
        let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if c.port.is_some() {
            c.dirty |= flags;
            self.changed.notify_one();
        }
    }
    pub(super) fn stream(&self, up: bool) {
        let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if c.stream != up {
            c.stream = up;
            if c.port.is_some() {
                c.dirty |= CLOCK;
                self.changed.notify_one();
            }
        }
    }
    pub(super) fn theme(&self, dark: bool) {
        let port = self
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .port
            .clone();
        if let Some(port) = port {
            port.theme(dark);
        }
    }
    pub(super) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.changed.notify_one();
    }
    pub(super) fn take_handoff(&self) -> Option<Handoff> {
        let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
        let (owner, action) = c.handoff.take()?;
        c.port
            .as_ref()
            .is_some_and(|p| p.owner() == owner)
            .then_some(action)
    }
    fn current(&self, work: &Work) -> bool {
        let c = self.control.lock().unwrap_or_else(|e| e.into_inner());
        c.intent == work.intent
            && c.port
                .as_ref()
                .is_some_and(|p| p.owner() == work.port.owner())
    }
    fn requeue_obsolete(&self, work: &Work) {
        let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if c.port
            .as_ref()
            .is_some_and(|p| p.owner() == work.port.owner())
        {
            // The query intent changed, not the data source. Refetch consumed
            // categories using the current range/ticket instead of losing them.
            c.dirty |= work.dirty;
            c.load_geometry |= work.geometry;
            self.changed.notify_one();
        }
    }
    fn work(&self) -> Option<Work> {
        let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
        let port = c.port.clone()?;
        let dirty = std::mem::take(&mut c.dirty);
        let geometry = std::mem::take(&mut c.load_geometry);
        Some(Work {
            port,
            intent: c.intent,
            dirty,
            range: c.range,
            days: c.days,
            detail: c.detail.clone(),
            usage: c.usage,
            stream: c.stream,
            geometry,
            geometry_revision: c.geometry_revision,
        })
    }
    async fn geometry_load(&self, work: &Work) {
        let cached = {
            let c = self.control.lock().unwrap_or_else(|e| e.into_inner());
            c.geometry_known.then_some(c.last_geometry)
        };
        let value = if let Some(value) = cached {
            value
        } else {
            tokio::task::spawn_blocking(|| {
                qb_core::home()
                    .ok()
                    .and_then(|home| dashboard_persistence::load(&home))
            })
            .await
            .ok()
            .flatten()
        };
        {
            let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
            if c.geometry_revision == work.geometry_revision {
                c.geometry_known = true;
                c.last_geometry = value;
            }
        }
        if self.current(work) {
            work.port.geometry(value);
        }
    }
    async fn geometry_save(&self) -> bool {
        let pending = {
            let c = self.control.lock().unwrap_or_else(|e| e.into_inner());
            c.pending_geometry
                .as_ref()
                .map(|g| (g.owner, g.sequence, g.value, g.revision))
        };
        let Some((owner, sequence, value, revision)) = pending else {
            return true;
        };
        let ok = tokio::task::spawn_blocking(move || {
            qb_core::home()
                .map_err(std::io::Error::other)
                .and_then(|home| dashboard_persistence::save(&home, value))
        })
        .await
        .is_ok_and(|r| r.is_ok());
        if ok {
            let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
            c.saved_geometry = c.saved_geometry.max((owner, sequence));
            if c.pending_geometry
                .as_ref()
                .is_some_and(|g| g.revision == revision)
            {
                c.pending_geometry = None;
            }
        }
        ok
    }
    pub(super) async fn run(self: Arc<Self>) {
        let mut cached: Option<Arc<model::DashboardSnapshot>> = None;
        let mut cached_owner = None;
        let mut revision = 0u64;
        let mut last_poll: Option<Instant> = None;
        let mut last_usage: Option<Instant> = None;
        let mut last_fetch: Option<Instant> = None;
        let mut last_intent = None;
        let mut usage_pending = false;
        let mut geometry_retry: Option<Instant> = None;
        loop {
            let interested = {
                let c = self.control.lock().unwrap_or_else(|e| e.into_inner());
                c.port.is_some() || c.pending_geometry.is_some()
            };
            if !interested && !self.stop.load(Ordering::Acquire) {
                cached = None;
                cached_owner = None;
                self.changed.notified().await;
            } else {
                tokio::select! {_ = self.changed.notified()=>{},_ = tokio::time::sleep(Duration::from_millis(250))=>{}}
            }
            let stopping = self.stop.load(Ordering::Acquire);
            if stopping || geometry_retry.is_none_or(|at| at.elapsed() >= Duration::from_secs(5)) {
                geometry_retry = (!self.geometry_save().await).then(Instant::now);
            }
            if stopping {
                break;
            }
            let Some(mut work) = self.work() else {
                cached = None;
                cached_owner = None;
                continue;
            };
            if cached_owner != Some(work.port.owner()) {
                cached = None;
                cached_owner = Some(work.port.owner());
                last_poll = None;
                last_usage = None;
                last_fetch = None;
                last_intent = None;
                usage_pending = false;
            }
            if work.geometry {
                self.geometry_load(&work).await;
            }
            if (!work.stream || cached.as_ref().is_none_or(|data| !data.connection.core_ok))
                && last_poll.is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
            {
                work.dirty |= ALL | DETAIL;
                last_poll = Some(Instant::now());
            }
            if usage_pending && last_usage.is_none_or(|at| at.elapsed() >= Duration::from_secs(30))
            {
                work.dirty |= USAGE;
                usage_pending = false;
            }
            if !work.usage {
                work.dirty &= !USAGE;
            }
            if work.usage
                && work.dirty & USAGE != 0
                && last_usage.is_some_and(|at| at.elapsed() < Duration::from_secs(30))
            {
                work.dirty &= !USAGE;
                usage_pending = true;
            }
            if work.dirty == 0 {
                continue;
            }
            if work.dirty == CLOCK
                && cached
                    .as_ref()
                    .is_some_and(|data| data.connection.stream_ok == work.stream)
            {
                if self.current(&work) {
                    work.port.clock(clock());
                }
                continue;
            }
            let needs_http = work.dirty & (ALL | DETAIL | USAGE) != 0;
            if needs_http
                && last_intent == Some(work.intent)
                && last_fetch.is_some_and(|at| at.elapsed() < Duration::from_secs(1))
            {
                let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
                if c.port
                    .as_ref()
                    .is_some_and(|p| p.owner() == work.port.owner())
                {
                    c.dirty |= work.dirty;
                }
                continue;
            }
            let mut data = cached.as_deref().cloned().unwrap_or_else(|| {
                model::DashboardSnapshot::new(
                    qb_tray::v2::project("{\"accounts\":[]}", SystemTime::now())
                        .expect("fixed empty state"),
                )
            });
            let mut ok = data.connection.core_ok;
            if needs_http {
                last_fetch = Some(Instant::now());
                last_intent = Some(work.intent);
                if work.usage && work.dirty & USAGE != 0 {
                    last_usage = Some(Instant::now());
                }
                match self.client() {
                    Ok(client) => {
                        let (state, sessions, activity, usage, detail) = tokio::join!(
                            async {
                                if work.dirty & STATE != 0 {
                                    Some(client.snapshot().await)
                                } else {
                                    None
                                }
                            },
                            async {
                                if work.dirty & SESSIONS != 0 {
                                    Some(client.dashboard_sessions(work.range).await)
                                } else {
                                    None
                                }
                            },
                            async {
                                if work.dirty & ACTIVITY != 0 {
                                    Some(client.dashboard_activity(work.days).await)
                                } else {
                                    None
                                }
                            },
                            async {
                                if work.usage && work.dirty & USAGE != 0 {
                                    Some(client.dashboard_usage_history().await)
                                } else {
                                    None
                                }
                            },
                            async {
                                if work.dirty & (DETAIL | SESSIONS) != 0
                                    && let Some(ticket) = &work.detail
                                {
                                    return Some((
                                        ticket.clone(),
                                        client.dashboard_session(&ticket.id).await,
                                    ));
                                }
                                None
                            }
                        );
                        ok = true;
                        if let Some(result) = state {
                            match result
                                .ok()
                                .and_then(|s| serde_json::to_string(&s).ok())
                                .and_then(|s| qb_tray::v2::project(&s, SystemTime::now()).ok())
                            {
                                Some(view) => data.accounts = view,
                                None => ok = false,
                            }
                        }
                        if let Some(result) = sessions {
                            match result {
                                Ok(s) => data.sessions = s,
                                Err(_) => ok = false,
                            }
                        }
                        if let Some(result) = activity {
                            match result {
                                Ok(a) => {
                                    data.activity = a.events;
                                    data.activity_truncated = a.truncated;
                                }
                                Err(_) => ok = false,
                            }
                        }
                        if let Some(result) = usage {
                            match result {
                                Ok(u) => {
                                    data.usage = u;
                                    usage_pending = false;
                                }
                                Err(_) => {
                                    ok = false;
                                    // A later successful state fetch must not
                                    // acknowledge a failed history request.
                                    usage_pending = true;
                                }
                            }
                        }
                        if let Some((ticket, result)) = detail
                            && self.current(&work)
                        {
                            match result {
                                Ok(detail) => {
                                    work.port.detail(ticket, detail);
                                }
                                Err(crate::client::ClientError::Http(
                                    reqwest::StatusCode::NOT_FOUND,
                                )) => {
                                    work.port.removed(ticket.id);
                                }
                                Err(_) => ok = false,
                            }
                        }
                    }
                    Err(_) => {
                        ok = false;
                        usage_pending |= work.usage && work.dirty & USAGE != 0;
                    }
                }
            }
            if self.current(&work) {
                let clock = clock();
                record_data_publication(&mut data, work.dirty, ok, &clock);
                revision = revision.saturating_add(1);
                data.revision = revision;
                data.connection = model::Connection {
                    core_ok: ok,
                    stream_ok: work.stream,
                    retry_seconds: 5,
                };
                let data = Arc::new(data);
                work.port.snapshot(data.clone(), clock);
                cached = Some(data);
            } else {
                self.requeue_obsolete(&work);
            }
        }
    }
}
impl Bridge for Coordinator {
    fn enqueue(&self, event: Event) {
        if self.stop.load(Ordering::Acquire) {
            return;
        }
        let mut c = self.control.lock().unwrap_or_else(|e| e.into_inner());
        let mut handoff = false;
        match event {
            Event::Opened { port } => {
                c.intent = c.intent.saturating_add(1);
                c.port = Some(port);
                c.detail = None;
                c.usage = false;
                c.dirty = ALL | CLOCK;
                c.load_geometry = true;
                self.open.store(true, Ordering::Release);
            }
            Event::Closed { owner } if c.port.as_ref().is_some_and(|p| p.owner() == owner) => {
                c.port = None;
                c.detail = None;
                c.dirty = 0;
                c.handoff = None;
                self.open.store(false, Ordering::Release);
            }
            Event::Command { owner, command }
                if c.port.as_ref().is_some_and(|p| p.owner() == owner) =>
            {
                c.intent = c.intent.saturating_add(1);
                match command {
                    Command::Sessions(range) => {
                        c.range = range;
                        c.detail = None;
                        c.dirty |= SESSIONS;
                    }
                    Command::Detail(ticket) => {
                        c.detail = Some(ticket);
                        c.dirty |= DETAIL;
                    }
                    Command::Activity(days) => {
                        c.days = days.clamp(1, 7);
                        c.dirty |= ACTIVITY;
                    }
                    Command::Usage => {
                        c.usage = true;
                        c.dirty |= USAGE;
                    }
                    Command::OpenTrayAccount(id) => {
                        c.handoff = Some((owner, Handoff::Account(id)));
                        handoff = true;
                    }
                    Command::OpenTrayAdd => {
                        c.handoff = Some((owner, Handoff::Add));
                        handoff = true;
                    }
                    Command::FocusSearch | Command::FocusEventSearch => {}
                }
            }
            Event::GeometryDirty {
                owner,
                sequence,
                geometry,
            } if geometry.valid()
                && (owner, sequence) > c.saved_geometry
                && c.pending_geometry
                    .as_ref()
                    .is_none_or(|p| (owner, sequence) > (p.owner, p.sequence)) =>
            {
                c.geometry_revision = c.geometry_revision.saturating_add(1);
                c.geometry_known = true;
                c.last_geometry = Some(geometry);
                c.pending_geometry = Some(PendingGeometry {
                    owner,
                    sequence,
                    value: geometry,
                    revision: c.geometry_revision,
                });
            }
            Event::ClockNeeded { owner } if c.port.as_ref().is_some_and(|p| p.owner() == owner) => {
                c.dirty |= CLOCK;
            }
            _ => return,
        }
        drop(c);
        self.changed.notify_one();
        if let Some(worker) = super::WORKER.get() {
            worker.refresh_interest();
        }
        if handoff {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(self.parent as *mut _)),
                    super::DASHBOARD_HANDOFF,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

pub(super) fn clock() -> model::Clock {
    let now = Local::now();
    let boundary = |date: chrono::NaiveDate| {
        let midnight = date.and_hms_opt(0, 0, 0).expect("calendar midnight");
        (0..=1440)
            .find_map(|minute| {
                Local
                    .from_local_datetime(&(midnight + chrono::Duration::minutes(minute)))
                    .earliest()
            })
            .map(|at| at.with_timezone(&Utc))
            .unwrap_or_else(|| now.with_timezone(&Utc))
    };
    model::Clock {
        now: now.fixed_offset(),
        day_start: boundary(now.date_naive()),
        day_end: boundary(
            now.date_naive()
                .succ_opt()
                .expect("current date has successor"),
        ),
    }
}

#[cfg(test)]
#[path = "dashboard_io_tests.rs"]
mod tests;
