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
const TUI_EXIT_GRACE: Duration = Duration::from_secs(1);

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

#[cfg(any(test, feature = "testkit"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestLifecycle {
    NativeReaderFinished,
    DispatchWorkerFinished,
    UiPublisherFinished,
    ConsumeCallbackFinished,
    FailureLatched(Error),
}

#[cfg(any(test, feature = "testkit"))]
#[derive(Default)]
struct TestGateState {
    entered: Notify,
    released: Mutex<bool>,
    wake_thread: Condvar,
    wake_async: Notify,
}

#[cfg(any(test, feature = "testkit"))]
impl TestGateState {
    fn release(&self) {
        *self.released.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.wake_thread.notify_all();
        self.wake_async.notify_one();
    }

    fn wait_thread(&self) {
        let mut released = self.released.lock().unwrap_or_else(|p| p.into_inner());
        self.entered.notify_one();
        while !*released {
            released = self
                .wake_thread
                .wait(released)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    async fn wait_async(&self) {
        self.entered.notify_one();
        loop {
            let wake = self.wake_async.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if *self.released.lock().unwrap_or_else(|p| p.into_inner()) {
                return;
            }
            wake.await;
        }
    }
}

/// One-shot scheduling hook only. Dropping the test handle releases its waiter.
#[cfg(any(test, feature = "testkit"))]
pub(crate) struct TestGate(Arc<TestGateState>);

#[cfg(any(test, feature = "testkit"))]
#[allow(dead_code)] // Testkit binary builds need not install or drive test gates.
impl TestGate {
    pub(crate) async fn entered(&self) {
        self.0.entered.notified().await;
    }

    pub(crate) fn release(&self) {
        self.0.release();
    }
}

#[cfg(any(test, feature = "testkit"))]
impl Drop for TestGate {
    fn drop(&mut self) {
        self.0.release();
    }
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
    cleanup_started: bool,
    direct_write_uncertain: bool,
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
    #[cfg(any(test, feature = "testkit"))]
    test_lifecycle: std::sync::OnceLock<tokio::sync::mpsc::Sender<TestLifecycle>>,
    #[cfg(any(test, feature = "testkit"))]
    test_consume_return: std::sync::OnceLock<Arc<TestGateState>>,
    #[cfg(any(test, feature = "testkit"))]
    test_before_cleanup: std::sync::OnceLock<Arc<TestGateState>>,
}

impl Shared {
    #[cfg(any(test, feature = "testkit"))]
    fn test_record(&self, event: TestLifecycle) {
        if let Some(observer) = self.test_lifecycle.get() {
            // Observation loss never blocks or changes dispatch/authority.
            let _ = observer.try_send(event);
        }
    }

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
        if error == Error::WriteUncertain && !state.cleanup_started {
            state.direct_write_uncertain = true;
        }
        if error == Error::WriteUncertain
            || (state.failure == Some(Error::Pump) && error != Error::Pump)
        {
            state.failure = Some(error);
        }
        state.failure.get_or_insert(error);
        drop(state);
        #[cfg(any(test, feature = "testkit"))]
        self.test_record(TestLifecycle::FailureLatched(error));
        self.changed.notify_one();
    }

    fn begin_cleanup(&self) -> (Option<Error>, bool) {
        let mut state = self.state();
        // Serialize the cause boundary with fail(): job drain has not started.
        state.cleanup_started = true;
        (state.failure, state.direct_write_uncertain)
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

    fn supervision(&self, peer_closed: bool) -> Result<(Option<Instant>, bool), Error> {
        let mut state = self.state();
        // Explicit PeerClosed also drops the UI queue. Its publisher's Pump
        // failure is secondary; no other failure receives an exit grace.
        if let Some(error) = state.failure
            && !(peer_closed && error == Error::Pump)
        {
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
    #[cfg(any(test, feature = "testkit"))]
    #[allow(dead_code)] // Optional test observer, unused by normal testkit entry.
    pub(crate) fn test_lifecycle(&mut self) -> tokio::sync::mpsc::Receiver<TestLifecycle> {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        assert!(
            self.shared.test_lifecycle.set(sender).is_ok(),
            "test lifecycle observer already installed"
        );
        receiver
    }

    #[cfg(any(test, feature = "testkit"))]
    #[allow(dead_code)] // Optional deterministic test scheduling hook.
    pub(crate) fn test_pause_consume_return(&mut self) -> TestGate {
        let gate = Arc::new(TestGateState::default());
        assert!(
            self.shared.test_consume_return.set(gate.clone()).is_ok(),
            "test Consume gate already installed"
        );
        TestGate(gate)
    }

    #[cfg(any(test, feature = "testkit"))]
    #[allow(dead_code)] // Optional deterministic test scheduling hook.
    pub(crate) fn test_pause_before_cleanup(&mut self) -> TestGate {
        let gate = Arc::new(TestGateState::default());
        assert!(
            self.shared.test_before_cleanup.set(gate.clone()).is_ok(),
            "test cleanup gate already installed"
        );
        TestGate(gate)
    }

    /// Install one bounded, lossy publication observer. A received Source proves
    /// successful ingress admission, never dispatch completion or authority.
    #[cfg(any(test, feature = "testkit"))]
    #[allow(dead_code)] // Testkit binary builds need not install this test-only observer.
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
            let result = reader_loop(
                &shared,
                &native_publisher,
                || {
                    framing::read_frame(&mut reader)
                        .map_err(|_| Error::NativeRead)?
                        .ok_or(Error::NativeEof)
                },
                Source::Native,
            );
            #[cfg(any(test, feature = "testkit"))]
            shared.test_record(TestLifecycle::NativeReaderFinished);
            result
        })?;

        let ui_publisher = publisher;
        owner.spawn("qb-ui-publisher", move |shared| {
            let mut receiver = ui.from_ui;
            let result = reader_loop(
                &shared,
                &ui_publisher,
                || {
                    receiver
                        .blocking_recv()
                        .map(|f| f.as_bytes().to_vec())
                        .ok_or(Error::Pump)
                },
                Source::Ui,
            );
            #[cfg(any(test, feature = "testkit"))]
            shared.test_record(TestLifecycle::UiPublisherFinished);
            result
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
        #[cfg(any(test, feature = "testkit"))]
        let dispatch_worker = name == "qb-native-dispatch";
        let worker = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                match catch_unwind(AssertUnwindSafe(|| body(shared.clone()))) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => shared.fail(error),
                    Err(_) => shared.fail(Error::Worker),
                }
                #[cfg(any(test, feature = "testkit"))]
                if dispatch_worker {
                    shared.test_record(TestLifecycle::DispatchWorkerFinished);
                }
            })
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
        #[cfg(any(test, feature = "testkit"))]
        if let Some(gate) = self.shared.test_before_cleanup.get() {
            gate.wait_async().await;
        }
        let (failure_before_cleanup, direct_write_uncertain) = self.shared.begin_cleanup();
        let cleanup = self.cleanup();
        if direct_write_uncertain {
            return Err(Error::WriteUncertain);
        }
        match result {
            // A real ingress/worker failure can drop the UI receiver and make
            // the pump finish first. Preserve that already-latched root cause,
            // using the snapshot taken BEFORE job termination causes errors.
            Err(Error::Pump) if failure_before_cleanup.is_some_and(|e| e != Error::Pump) => {
                Err(failure_before_cleanup.unwrap_or(Error::Pump))
            }
            // Preserve the supervisor's fatal cause. Stopping jobs to handle a
            // Pump/Watchdog failure can itself make a blocked write fail; that
            // teardown consequence must not replace the original diagnosis.
            Err(error) => Err(error),
            Ok(_) if self.shared.state().failure == Some(Error::WriteUncertain) => {
                // A clean child exit cannot hide an unfinished physical write
                // whose uncertainty becomes visible only after job drain.
                Err(Error::WriteUncertain)
            }
            Ok(status) => cleanup.map(|()| status),
        }
    }

    async fn supervise(&mut self) -> Result<JobStatus, Error> {
        loop {
            let (deadline, auto_disabled) = self.shared.supervision(false)?;
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
                result = self.pump.as_mut().ok_or(Error::Pump)? => {
                    return match result {
                        Err(socket_pump::Error::PeerClosed) => self.peer_closed_exit().await,
                        _ => Err(Error::Pump),
                    };
                },
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

    async fn peer_closed_exit(&mut self) -> Result<JobStatus, Error> {
        // Stop new work without touching ingress: an in-flight physical writer
        // may own its mutex. Never poll the completed pump again.
        self.shared.stopped.store(true, Ordering::Release);
        self.stop_auto();
        self.shared.wake_dispatch();
        let deadline = Instant::now() + TUI_EXIT_GRACE;
        loop {
            let (operation_deadline, _) = self.shared.supervision(true)?;
            if Instant::now() >= deadline {
                return Err(Error::Pump);
            }
            match self.jobs.try_wait().map_err(|_| Error::Job)? {
                status @ (JobStatus::TuiExited(_) | JobStatus::BothExited { .. }) => {
                    return Ok(status);
                }
                JobStatus::Running => {}
                _ => return Err(Error::Pump),
            }
            tokio::select! {
                () = self.shared.changed.notified() => {},
                () = wait_deadline(operation_deadline) => {},
                () = tokio::time::sleep_until((Instant::now() + CHILD_POLL).min(deadline)) => {},
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
        let _ = self.shared.begin_cleanup();
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
        #[cfg(any(test, feature = "testkit"))]
        for gate in [
            self.shared.test_consume_return.get(),
            self.shared.test_before_cleanup.get(),
        ]
        .into_iter()
        .flatten()
        {
            // Cancellation cannot depend on a test task being polled during join.
            gate.release();
        }
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
            let consumed = runtime.block_on(core.consume(episode, deadline));
            #[cfg(any(test, feature = "testkit"))]
            if let Some(gate) = shared.test_consume_return.get() {
                gate.wait_thread();
            }
            match consumed {
                Ok(Consumption::Granted(receipt)) => {
                    let valid = receipt.is_current(episode);
                    grant = Some(receipt);
                    valid && shared.enter_physical(deadline)
                }
                _ => false,
            }
        });
        #[cfg(any(test, feature = "testkit"))]
        if attempted {
            shared.test_record(TestLifecycle::ConsumeCallbackFinished);
        }
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
    // The pump owns the UI queue: its closure is reported to the supervisor,
    // which stops dispatch and drops Consume before draining jobs. Closing shared
    // ingress here would manufacture Revoked during that clean callback denial.
    // A successful stopped reader of either source needs no extra ingress close.
    // Frame/publication failures still close ingress and remain fatal.
    let owned_shutdown = (source == Source::Ui && result == Err(Error::Pump))
        || (result.is_ok() && shared.stopped.load(Ordering::Acquire));
    if source == Source::Ui && result == Err(Error::Pump) {
        // Close admission before reporting EOF, even if the supervisor has not
        // yet observed the completed pump. AwaitCore/enter_physical both veto.
        shared.stopped.store(true, Ordering::Release);
        shared.disable_auto();
    }
    // Notify BEFORE close, since close itself may wait for the physical writer's
    // ingress mutex. The independent supervisor can kill jobs to release it.
    if let Err(error) = result {
        shared.fail(error);
    }
    if !owned_shutdown {
        publisher.close();
    }
    result
}
