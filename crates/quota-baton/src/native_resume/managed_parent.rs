//! Unregistered ordinary-traffic run owner. No automatic activation is wired.
//!
//! The caller keeps admission/catalog alive until this owner has returned or
//! finished Drop. Own the authenticated endpoint and both jobs before entering
//! this API. A transport capability is not child identity or core authority.
//!
//! Three owned std threads serialize dispatch, read native stdout, and publish UI
//! input. The supervisor never calls Publisher (which may wait on ingress).
//! Actor/pump futures are driven locally, never spawned. Every process_next,
//! including its complete physical write/LF/flush, has a five-second watchdog.
//! Cleanup first stops BOTH jobs without ingress locking, then synchronously
//! drops actor/pump futures and their channels, wakes workers and joins them.
//! Job drain itself uses RemoteRun's fail-closed lifetime, not a bounded promise.
//!
//! Observe/Inspect/ticket handoff is deliberately absent. No final read is begun,
//! the dispatch callback always denies, and Due cannot authorize any write. The
//! command parent is retained on the dispatcher thread for a later typed handoff.
//! A future Consume bridge belongs ONLY inside its native committed callback,
//! on that std thread, never a Tokio worker or while holding ingress across await.

use std::{
    future::{Future, pending},
    io::{self, BufReader, Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use tokio::{sync::Notify, time::Instant};
use tokio_tungstenite::WebSocketStream;

use super::{
    control_actor,
    core_control::CoreControl,
    endpoint::EndpointIo,
    framing,
    ingress::{Frame, Publisher, Source},
    parent_dispatch::Parent,
    socket_pump,
};

const EVENT_BUDGET: Duration = Duration::from_secs(5);
const CHILD_POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobStatus {
    Running,
    TuiExited(i32),
    ServerExited(i32),
    BothExited { tui: i32, server: i32 },
    Stopped,
}

/// Resource seam, never permission. stop_jobs MUST drain both jobs and unblock
/// all transferred pipe I/O even when it reports an initial termination error.
/// Production uses RemoteRun; tests may supply owned synthetic I/O only.
pub(crate) trait Jobs {
    type Input: Write + Send + 'static;
    type Output: Read + Send + 'static;
    fn take_stdio(&mut self) -> io::Result<(Self::Input, Self::Output)>;
    fn try_wait(&self) -> io::Result<JobStatus>;
    fn stop_jobs(&mut self) -> io::Result<()>;
}

#[cfg(windows)]
impl Jobs for crate::launcher_label::RemoteRun {
    type Input = std::fs::File;
    type Output = std::fs::File;

    fn take_stdio(&mut self) -> io::Result<(Self::Input, Self::Output)> {
        self.take_stdio()
    }
    fn try_wait(&self) -> io::Result<JobStatus> {
        use crate::launcher_label::RemoteStatus as R;
        self.try_wait().map(|status| match status {
            R::Running => JobStatus::Running,
            R::TuiExited(code) => JobStatus::TuiExited(code),
            R::ServerExited(code) => JobStatus::ServerExited(code),
            R::BothExited { tui, server } => JobStatus::BothExited { tui, server },
            R::Stopped => JobStatus::Stopped,
        })
    }
    fn stop_jobs(&mut self) -> io::Result<()> {
        self.stop_jobs()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Setup,
    Worker,
    Ingress,
    Dispatch,
    NativeRead,
    NativeEof,
    Pump,
    Core,
    UnexpectedHint,
    Job,
    Watchdog,
    Lifetime,
    Cleanup,
}

#[derive(Default)]
struct State {
    failure: Option<Error>,
    event_deadline: Option<Instant>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Notify,
    stopped: AtomicBool,
    enabled: AtomicBool,
    dirty: Mutex<bool>,
    dispatch_wake: Condvar,
}

impl Shared {
    // No untrusted work or pipe/ingress calls run under these small state locks.
    fn fail(&self, error: Error) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.failure.get_or_insert(error);
        drop(state);
        self.changed.notify_one();
    }

    fn start_event(&self) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .event_deadline = Some(Instant::now() + EVENT_BUDGET);
        self.changed.notify_one();
    }

    fn end_event(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // A late completion cannot erase expiry before the supervisor sees it.
        if state.event_deadline.is_some_and(|d| Instant::now() >= d) {
            state.failure.get_or_insert(Error::Watchdog);
        }
        state.event_deadline = None;
        drop(state);
        self.changed.notify_one();
    }

    fn wake_dispatch(&self) {
        *self.dirty.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.dispatch_wake.notify_one();
    }

    fn wait_dispatch(&self) {
        let mut dirty = self.dirty.lock().unwrap_or_else(|p| p.into_inner());
        while !*dirty && !self.stopped.load(Ordering::Acquire) {
            dirty = self
                .dispatch_wake
                .wait(dirty)
                .unwrap_or_else(|p| p.into_inner());
        }
        *dirty = false;
    }
}

/// FrameWriter writes payload, then LF, then flush. This adapter queues exactly
/// that one frame on flush; queue admission is NOT physical WS delivery.
struct UiAdapter {
    sender: socket_pump::UiSender,
    bytes: Vec<u8>,
}

impl Write for UiAdapter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > framing::MAX_FRAME_BYTES + 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.bytes.pop() != Some(b'\n') {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let frame = socket_pump::Frame::new(std::mem::take(&mut self.bytes))
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        self.sender
            .try_send(frame)
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }
}

type Actor = Pin<Box<dyn Future<Output = Result<(), control_actor::Error>> + Send>>;
type Pump = Pin<Box<dyn Future<Output = Result<(), socket_pump::Error>> + Send>>;

/// Not Clone; Drop synchronously finishes ownership cleanup. Async actor/pump
/// fields are explicitly taken BEFORE joins, including cancellation of run().
pub(crate) struct ManagedParent<J: Jobs> {
    jobs: J,
    shared: Arc<Shared>,
    actor: Option<Actor>,
    pump: Option<Pump>,
    due: Option<tokio::sync::mpsc::Receiver<control_actor::DueHint>>,
    workers: Vec<JoinHandle<()>>,
    lifetime: Instant,
    cleaned: bool,
}

impl<J: Jobs> ManagedParent<J> {
    pub(crate) fn new(
        jobs: J,
        namespace: String,
        control: CoreControl,
        socket: WebSocketStream<EndpointIo>,
        lifetime: Instant,
    ) -> Result<Self, Error> {
        let mut owner = Self {
            jobs,
            shared: Arc::new(Shared::default()),
            actor: None,
            pump: None,
            due: None,
            workers: Vec::with_capacity(3),
            lifetime,
            cleaned: false,
        };
        let (stdin, stdout) = owner.jobs.take_stdio().map_err(|_| Error::Setup)?;
        let (ui, socket_io) = socket_pump::channels();
        let (mut core, actor_io) = control_actor::channels();
        owner.due = core.take_due();
        owner.actor = Some(Box::pin(control_actor::run(control, actor_io, lifetime)));
        owner.pump = Some(Box::pin(socket_pump::run(socket, socket_io)));
        let (mut parent, publisher) = Parent::new(
            namespace,
            stdin,
            UiAdapter {
                sender: ui.to_ui,
                bytes: Vec::new(),
            },
        )
        .map_err(|_| Error::Setup)?;

        let native_publisher = publisher.clone();
        owner.spawn("qb-native-reader", move |shared| {
            let mut reader = BufReader::new(stdout);
            reader_loop(
                &shared,
                &native_publisher,
                || {
                    framing::read_frame(&mut reader)
                        .map_err(|_| Error::NativeRead)?
                        .ok_or(Error::NativeEof)
                },
                Source::Native,
            )
        })?;

        let ui_publisher = publisher;
        owner.spawn("qb-ui-publisher", move |shared| {
            let mut receiver = ui.from_ui;
            reader_loop(
                &shared,
                &ui_publisher,
                || {
                    receiver
                        .blocking_recv()
                        .map(|f| f.as_bytes().to_vec())
                        .ok_or(Error::Pump)
                },
                Source::Ui,
            )
        })?;

        owner.spawn("qb-native-dispatch", move |shared| {
            // Deliberately no Observe/Inspect/Consume in this scaffold. Keeping
            // the noncloneable command parent here establishes future ownership.
            let _core = core;
            while !shared.stopped.load(Ordering::Acquire) {
                if !shared.enabled.load(Ordering::Acquire) {
                    shared.wait_dispatch();
                    continue;
                }
                shared.start_event();
                let result = parent.process_next(|| false);
                shared.end_event();
                match result {
                    Ok(true) => {}
                    Ok(false) => shared.wait_dispatch(),
                    Err(_) => return Err(Error::Dispatch),
                }
            }
            Ok(())
        })?;
        Ok(owner)
    }

    fn spawn(
        &mut self,
        name: &str,
        body: impl FnOnce(Arc<Shared>) -> Result<(), Error> + Send + 'static,
    ) -> Result<(), Error> {
        let shared = self.shared.clone();
        let worker = thread::Builder::new()
            .name(name.into())
            .spawn(
                move || match catch_unwind(AssertUnwindSafe(|| body(shared.clone()))) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => shared.fail(error),
                    Err(_) => shared.fail(Error::Worker),
                },
            )
            .map_err(|_| Error::Setup)?;
        self.workers.push(worker);
        Ok(())
    }

    /// Local futures only. No Publisher or blocking dispatch runs on this task.
    /// Child snapshots retain the distinction between TUI and server exits.
    pub(crate) async fn run(mut self) -> Result<JobStatus, Error> {
        self.shared.enabled.store(true, Ordering::Release);
        self.shared.wake_dispatch();
        let result = self.supervise().await;
        let cleanup = self.cleanup();
        match result {
            Err(error) => Err(error),
            Ok(status) => cleanup.map(|()| status),
        }
    }

    async fn supervise(&mut self) -> Result<JobStatus, Error> {
        loop {
            let deadline = {
                let state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(error) = state.failure {
                    return Err(error);
                }
                if state.event_deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(Error::Watchdog);
                }
                state.event_deadline
            };
            if Instant::now() >= self.lifetime {
                return Err(Error::Lifetime);
            }
            let status = self.jobs.try_wait().map_err(|_| Error::Job)?;
            if status != JobStatus::Running {
                return Ok(status);
            }
            tokio::select! {
                () = self.shared.changed.notified() => {},
                () = wait_deadline(deadline) => {},
                _ = self.actor.as_mut().ok_or(Error::Core)? => return Err(Error::Core),
                _ = self.pump.as_mut().ok_or(Error::Pump)? => return Err(Error::Pump),
                _ = self.due.as_mut().ok_or(Error::Core)?.recv() => return Err(Error::UnexpectedHint),
                () = tokio::time::sleep_until(self.lifetime) => return Err(Error::Lifetime),
                () = tokio::time::sleep(CHILD_POLL) => {},
            }
        }
    }

    fn cleanup(&mut self) -> Result<(), Error> {
        if self.cleaned {
            return Ok(());
        }
        self.shared.stopped.store(true, Ordering::Release);
        // No ingress locks before this drain. Both pipe ends can be blocked in
        // other threads; killing both jobs is their synchronous cancellation.
        let mut failed = self.jobs.stop_jobs().is_err();
        // Dropping a polled actor also drops in-flight/queued reply senders.
        // Thus a future std-thread Consume bridge wakes WITHOUT runtime progress
        // even if this Drop is itself running on the only runtime worker.
        drop(self.actor.take());
        drop(self.pump.take());
        drop(self.due.take());
        self.shared.wake_dispatch();
        for worker in self.workers.drain(..) {
            failed |= worker.join().is_err();
        }
        self.cleaned = true;
        if failed { Err(Error::Cleanup) } else { Ok(()) }
    }
}

impl<J: Jobs> Drop for ManagedParent<J> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending().await,
    }
}

fn reader_loop(
    shared: &Shared,
    publisher: &Publisher,
    mut next: impl FnMut() -> Result<Vec<u8>, Error>,
    source: Source,
) -> Result<(), Error> {
    let result = (|| {
        while !shared.stopped.load(Ordering::Acquire) {
            let bytes = next()?;
            if shared.stopped.load(Ordering::Acquire) {
                break;
            }
            let frame = Frame::new(bytes).map_err(|_| Error::Ingress)?;
            publisher
                .publish(source, frame)
                .map_err(|_| Error::Ingress)?;
            shared.wake_dispatch();
        }
        Ok(())
    })();
    // Notify BEFORE close, since close itself may wait for the physical writer's
    // ingress mutex. The independent supervisor can kill jobs to release it.
    if let Err(error) = result {
        shared.fail(error);
    }
    publisher.close();
    result
}
