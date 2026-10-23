//! Unregistered run owner for ONE native ticket and ONE core reset episode.
//!
//! The caller keeps admission/catalog alive until this owner has returned or
//! finished Drop. Own the authenticated endpoint and both jobs before entering
//! this API. A transport capability is not child identity or core authority.
//!
//! Three owned std threads serialize dispatch, read native stdout, and publish UI
//! input. The supervisor never calls Publisher (which may wait on ingress).
//! Actor/pump futures are driven locally, never spawned. Bounded core IPC and
//! physical dispatch have distinct watchdog phases. Consume and its complete
//! physical write/LF/flush share ONE original five-second candidate deadline.
//! Cleanup first stops BOTH jobs without ingress locking, then synchronously
//! drops actor/pump futures and their channels, wakes workers and joins them.
//! Job drain itself uses RemoteRun's fail-closed lifetime, not a bounded promise.
//!
//! Core loss/refusal/expiry disables auto but preserves ordinary traffic. Only
//! the dedicated dispatcher thread bridges core IPC with Handle::block_on, with
//! Consume ONLY inside its native committed callback, never holding ingress
//! across await. A real Grant is retained through that callback's physical write.
//! No controller replacement, owner reconnect, second Observe or write retry.
//! There is no whole-run deadline: caller/root-future Drop cancels ownership.

use std::{
    future::{Future, pending},
    io::{self, BufReader, Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use tokio::{runtime::Handle, sync::Notify, time::Instant};
use tokio_tungstenite::WebSocketStream;

use super::{
    control_actor,
    core_control::{Consumption, CoreControl, Grant, Inspection, Observation},
    dispatcher,
    endpoint::EndpointIo,
    framing,
    goal::{Delivery, StoppedGoalTicket},
    ingress::{Frame, Publisher, Source},
    parent_dispatch::{self, Parent},
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
    WriteUncertain,
    NativeRead,
    NativeEof,
    Pump,
    Job,
    Watchdog,
    Cleanup,
}

#[derive(Clone, Copy, Default)]
enum Operation {
    #[default]
    Idle,
    AwaitCore(Instant),
    Physical(Instant),
}

#[derive(Default)]
struct AutoMailbox {
    disabled: bool,
    episode: Option<u64>,
    due: Option<u64>,
}

#[derive(Default)]
struct State {
    failure: Option<Error>,
    operation: Operation,
    auto: AutoMailbox,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Notify,
    stopped: AtomicBool,
    enabled: AtomicBool,
    dirty: Mutex<bool>,
    dispatch_wake: Condvar,
    #[cfg(any(test, feature = "testkit"))]
    test_ingress: std::sync::OnceLock<tokio::sync::mpsc::Sender<Source>>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poison) => {
                let mut state = poison.into_inner();
                // Never recover a poisoned mailbox as live authority.
                state.auto.disabled = true;
                state
            }
        }
    }

    // No untrusted work or pipe/ingress calls run under these small state locks.
    fn fail(&self, error: Error) {
        let mut state = self.state();
        if error == Error::WriteUncertain {
            state.failure = Some(error);
        }
        state.failure.get_or_insert(error);
        drop(state);
        self.changed.notify_one();
    }

    fn start_event(&self) -> Instant {
        let deadline = Instant::now() + EVENT_BUDGET;
        self.state().operation = Operation::Physical(deadline);
        self.changed.notify_one();
        deadline
    }

    fn end_event(&self) {
        let mut state = self.state();
        // A clean IPC timeout is auto denial, never a physical watchdog failure.
        // Physical expiry cannot be erased by a late completion instead.
        if matches!(state.operation, Operation::Physical(d) if Instant::now() >= d) {
            state.failure.get_or_insert(Error::Watchdog);
        }
        state.operation = Operation::Idle;
        drop(state);
        self.changed.notify_one();
    }

    fn await_core(&self, deadline: Instant) -> bool {
        let mut state = self.state();
        if state.auto.disabled || self.stopped.load(Ordering::Acquire) {
            return false;
        }
        state.operation = Operation::AwaitCore(deadline);
        drop(state);
        self.changed.notify_one();
        true
    }

    fn enter_physical(&self, deadline: Instant) -> bool {
        let mut state = self.state();
        if state.auto.disabled || self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline
        {
            state.auto.disabled = true;
            return false;
        }
        // Keep the candidate's original bound, including time spent in Consume.
        state.operation = Operation::Physical(deadline);
        drop(state);
        self.changed.notify_one();
        true
    }

    fn disable_auto(&self) {
        self.state().auto.disabled = true;
        self.changed.notify_one();
        self.wake_dispatch();
    }

    fn auto_disabled(&self) -> bool {
        self.state().auto.disabled
    }

    fn bind_episode(&self, episode: u64) -> bool {
        let mut state = self.state();
        let auto = &mut state.auto;
        if episode == 0
            || auto.episode.is_some_and(|id| id != episode)
            || auto.due.is_some_and(|id| id != episode)
        {
            auto.disabled = true;
        }
        if !auto.disabled {
            auto.episode = Some(episode);
        }
        let bound = !auto.disabled;
        drop(state);
        self.changed.notify_one();
        self.wake_dispatch();
        bound
    }

    fn publish_due(&self, episode: u64) {
        let mut state = self.state();
        let auto = &mut state.auto;
        if episode == 0
            || auto.episode.is_some_and(|id| id != episode)
            || auto.due.is_some_and(|id| id != episode)
        {
            auto.disabled = true;
        }
        if !auto.disabled {
            // Observe::Due and a buffered DueHint can name the same episode.
            auto.due = Some(episode);
        }
        drop(state);
        self.changed.notify_one();
        self.wake_dispatch();
    }

    fn take_due(&self, episode: u64) -> bool {
        let mut state = self.state();
        if state.auto.disabled || state.auto.due != Some(episode) {
            return false;
        }
        state.auto.due.take();
        true
    }

    fn supervision(&self) -> Result<(Option<Instant>, bool), Error> {
        let mut state = self.state();
        if let Some(error) = state.failure {
            return Err(error);
        }
        let deadline = match state.operation {
            Operation::Idle => None,
            Operation::Physical(d) if Instant::now() >= d => return Err(Error::Watchdog),
            Operation::AwaitCore(d) if Instant::now() >= d => {
                state.auto.disabled = true;
                state.operation = Operation::Idle;
                None
            }
            Operation::Physical(d) | Operation::AwaitCore(d) => Some(d),
        };
        Ok((deadline, state.auto.disabled))
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
    cleaned: bool,
}

impl<J: Jobs> ManagedParent<J> {
    /// Install one bounded, lossy publication observer. A received Source proves
    /// successful ingress admission, never dispatch completion or authority.
    #[cfg(any(test, feature = "testkit"))]
    pub(crate) fn test_ingress(&mut self) -> tokio::sync::mpsc::Receiver<Source> {
        let (sender, receiver) = tokio::sync::mpsc::channel(super::ingress::QUEUE_FRAMES);
        assert!(
            self.shared.test_ingress.set(sender).is_ok(),
            "test ingress observer already installed"
        );
        receiver
    }

    pub(crate) fn new(
        jobs: J,
        namespace: String,
        control: CoreControl,
        socket: WebSocketStream<EndpointIo>,
    ) -> Result<Self, Error> {
        let mut owner = Self {
            jobs,
            shared: Arc::new(Shared::default()),
            actor: None,
            pump: None,
            due: None,
            workers: Vec::with_capacity(3),
            cleaned: false,
        };
        let runtime = Handle::try_current().map_err(|_| Error::Setup)?;
        let (stdin, stdout) = owner.jobs.take_stdio().map_err(|_| Error::Setup)?;
        let (ui, socket_io) = socket_pump::channels();
        let (mut core, actor_io) = control_actor::channels();
        owner.due = core.take_due();
        owner.actor = Some(Box::pin(control_actor::run_until_cancelled(
            control, actor_io,
        )));
        owner.pump = Some(Box::pin(socket_pump::run(socket, socket_io)));
        let (parent, publisher) = Parent::new(
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
            dispatch_loop(parent, core, runtime, &shared)
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
            let (deadline, auto_disabled) = self.shared.supervision()?;
            if auto_disabled {
                self.drop_actor();
            }
            let status = self.jobs.try_wait().map_err(|_| Error::Job)?;
            if status != JobStatus::Running {
                return Ok(status);
            }
            tokio::select! {
                () = self.shared.changed.notified() => {},
                () = wait_deadline(deadline) => {},
                _ = async {
                    match self.actor.as_mut() {
                        Some(actor) => actor.await,
                        None => pending().await,
                    }
                } => self.stop_auto(),
                _ = self.pump.as_mut().ok_or(Error::Pump)? => return Err(Error::Pump),
                hint = async {
                    match self.due.as_mut() {
                        Some(due) => due.recv().await,
                        None => pending().await,
                    }
                } => match hint {
                    Some(hint) => self.shared.publish_due(hint.episode()),
                    None => self.stop_auto(),
                },
                () = tokio::time::sleep(CHILD_POLL) => {},
            }
        }
    }

    fn drop_actor(&mut self) {
        let had_actor = self.actor.is_some() || self.due.is_some();
        drop(self.actor.take());
        drop(self.due.take());
        if had_actor {
            self.shared.wake_dispatch();
        }
    }

    fn stop_auto(&mut self) {
        self.shared.disable_auto();
        self.drop_actor();
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

enum AutoPhase {
    Learning,
    Waiting {
        ticket: StoppedGoalTicket,
        episode: u64,
    },
    FinalRead {
        episode: u64,
    },
    Terminal,
}

fn terminal_auto<N: Write, U: Write>(
    parent: &mut Parent<N, U>,
    core: &mut Option<control_actor::Parent>,
    phase: &mut AutoPhase,
    shared: &Shared,
) {
    *phase = AutoPhase::Terminal;
    parent.cancel_resume();
    // Called only outside process_next: never invalidate a held Grant before
    // its immediate physical write has returned (or failed/been vetoed).
    drop(core.take());
    shared.disable_auto();
}

fn dispatch_error(error: parent_dispatch::Error) -> Error {
    match error {
        parent_dispatch::Error::Dispatch(dispatcher::Error::WriteUncertain) => {
            Error::WriteUncertain
        }
        parent_dispatch::Error::Ingress(_) => Error::Ingress,
        parent_dispatch::Error::Dispatch(_) => Error::Dispatch,
    }
}

fn dispatch_loop<N: Write, U: Write>(
    mut parent: Parent<N, U>,
    core: control_actor::Parent,
    runtime: Handle,
    shared: &Shared,
) -> Result<(), Error> {
    let mut core = Some(core);
    let mut phase = AutoPhase::Learning;
    while !shared.stopped.load(Ordering::Acquire) {
        if !shared.enabled.load(Ordering::Acquire) {
            shared.wait_dispatch();
            continue;
        }
        if shared.auto_disabled() && core.is_some() {
            terminal_auto(&mut parent, &mut core, &mut phase, shared);
        }

        let deadline = shared.start_event();
        let episode = match phase {
            AutoPhase::FinalRead { episode } => Some(episode),
            _ => None,
        };
        let mut grant: Option<Grant> = None;
        let mut attempted = false;
        let result = parent.process_next(|| {
            attempted = true;
            let (Some(episode), Some(core)) = (episode, core.as_ref()) else {
                return false;
            };
            if !shared.await_core(deadline) {
                return false;
            }
            // ONLY the dedicated std thread, ONLY inside committed callback,
            // and NO ingress/mailbox mutex across this bounded exchange.
            match runtime.block_on(core.consume(episode, deadline)) {
                Ok(Consumption::Granted(receipt)) => {
                    let valid = receipt.is_current(episode);
                    grant = Some(receipt);
                    valid && shared.enter_physical(deadline)
                }
                _ => false,
            }
        });
        // Preserve physical uncertainty ahead of any secondary timeout/latch.
        if result
            == Err(parent_dispatch::Error::Dispatch(
                dispatcher::Error::WriteUncertain,
            ))
        {
            shared.fail(Error::WriteUncertain);
        }
        shared.end_event();
        // This exact nonClone receipt lived through payload, LF and flush.
        drop(grant);
        let denied = result
            == Err(parent_dispatch::Error::Dispatch(
                dispatcher::Error::AuthorityDenied,
            ));
        if attempted
            || denied
            || (matches!(phase, AutoPhase::FinalRead { .. })
                && parent.delivery() == Delivery::Cancelled)
        {
            terminal_auto(&mut parent, &mut core, &mut phase, shared);
        }
        let processed = match result {
            Ok(processed) => processed,
            Err(_) if denied => true,
            Err(error) => return Err(dispatch_error(error)),
        };
        if shared.auto_disabled() && core.is_some() {
            terminal_auto(&mut parent, &mut core, &mut phase, shared);
        }

        if matches!(phase, AutoPhase::Learning)
            && let Some(ticket) = parent.stopped_ticket()
        {
            // Taking this ticket permanently leaves Learning, on EVERY result.
            phase = AutoPhase::Terminal;
            let deadline = Instant::now() + EVENT_BUDGET;
            let observed = match core.as_ref() {
                Some(core) if shared.await_core(deadline) => {
                    runtime.block_on(core.observe(ticket.thread_id(), deadline))
                }
                _ => {
                    terminal_auto(&mut parent, &mut core, &mut phase, shared);
                    continue;
                }
            };
            shared.end_event();
            let immediate = matches!(observed, Ok(Observation::Due { .. }));
            match observed {
                Ok(Observation::Waiting { episode, .. }) | Ok(Observation::Due { episode })
                    if shared.bind_episode(episode) =>
                {
                    if immediate {
                        shared.publish_due(episode);
                    }
                    phase = AutoPhase::Waiting { ticket, episode };
                }
                _ => terminal_auto(&mut parent, &mut core, &mut phase, shared),
            }
            continue;
        }

        // Drain admitted ordinary events BEFORE Inspect. Checkpoint in
        // begin_final_read also vetoes input admitted while Inspect awaited.
        if !processed {
            if let AutoPhase::Waiting { episode, .. } = phase
                && shared.take_due(episode)
            {
                let AutoPhase::Waiting { ticket, episode } =
                    std::mem::replace(&mut phase, AutoPhase::Terminal)
                else {
                    return Err(Error::Worker);
                };
                let deadline = Instant::now() + EVENT_BUDGET;
                let inspected = match core.as_ref() {
                    Some(core) if shared.await_core(deadline) => {
                        runtime.block_on(core.inspect(episode, deadline))
                    }
                    _ => {
                        terminal_auto(&mut parent, &mut core, &mut phase, shared);
                        continue;
                    }
                };
                shared.end_event();
                if matches!(inspected, Ok(Inspection::Ready)) && shared.enter_physical(deadline) {
                    let result = parent.begin_final_read(ticket);
                    if result
                        == Err(parent_dispatch::Error::Dispatch(
                            dispatcher::Error::WriteUncertain,
                        ))
                    {
                        shared.fail(Error::WriteUncertain);
                    }
                    shared.end_event();
                    match result {
                        Ok(()) => phase = AutoPhase::FinalRead { episode },
                        Err(parent_dispatch::Error::Dispatch(
                            dispatcher::Error::AuthorityDenied,
                        )) => {
                            terminal_auto(&mut parent, &mut core, &mut phase, shared);
                        }
                        Err(error) => return Err(dispatch_error(error)),
                    }
                } else {
                    terminal_auto(&mut parent, &mut core, &mut phase, shared);
                }
                continue;
            }
            shared.wait_dispatch();
        }
    }
    Ok(())
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
            #[cfg(any(test, feature = "testkit"))]
            if let Some(observer) = shared.test_ingress.get() {
                // Full/closed is observation loss only; never block, retry,
                // alter ingress, or grant/deny automatic authority here.
                let _ = observer.try_send(source);
            }
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
