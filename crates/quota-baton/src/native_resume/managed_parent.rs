//! Owned run owner for ONE native ticket and ONE core reset episode.
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

use futures_util::FutureExt;
use rand::TryRngCore;
use tokio::{runtime::Handle, sync::Notify, time::Instant};
use tokio_tungstenite::WebSocketStream;

#[cfg(all(feature = "testkit", debug_assertions))]
use super::observation::{self, Event, ThreadId};

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

impl JobStatus {
    fn clean_tui_exit(self) -> bool {
        matches!(
            self,
            Self::TuiExited(0) | Self::BothExited { tui: 0, server: 0 }
        )
    }
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
    NativeEofGrace,
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
    retired_episode: u64,
}

#[derive(Default)]
struct State {
    failure: Option<Error>,
    native_eof_seen: bool,
    non_eof_failure: Option<Error>,
    dispatch_failure: Option<Error>,
    cleanup_started: bool,
    direct_write_uncertain: bool,
    operation: Operation,
    auto: AutoMailbox,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    cleanup_wake: Condvar,
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
    #[cfg(any(test, feature = "testkit"))]
    test_dispatch_failure: std::sync::OnceLock<Arc<TestGateState>>,
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
        self.record_failure(error, false);
    }

    fn dispatch_failed(&self, error: Error) {
        self.record_failure(error, true);
    }

    fn record_failure(&self, error: Error, dispatch: bool) {
        let mut state = self.state();
        if dispatch {
            // Retain the worker's own result through join, even if its wrapper
            // is descheduled between computing the result and recording it.
            state.dispatch_failure = Some(error);
        }
        if error == Error::NativeEof {
            // Serialize EOF admission loss with AwaitCore/enter_physical, before
            // publishing failure or waiting for the local supervisor to run.
            self.stopped.store(true, Ordering::Release);
            state.auto.disabled = true;
            state.native_eof_seen = true;
        } else if error == Error::WriteUncertain || state.non_eof_failure.is_none() {
            // EOF is provisional only when it is the sole observed failure.
            state.non_eof_failure = Some(error);
        }
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
        if error == Error::NativeEof {
            self.wake_dispatch();
        }
    }

    fn begin_cleanup(&self) -> (Option<Error>, bool, Option<Error>) {
        let mut state = self.state();
        // Serialize the cause boundary with fail(): job drain has not started.
        state.cleanup_started = true;
        self.cleanup_wake.notify_all();
        (
            state.failure,
            state.direct_write_uncertain,
            state
                .native_eof_seen
                .then_some(state.non_eof_failure)
                .flatten(),
        )
    }

    fn retain_eof_dispatch(&self) {
        let mut state = self.state();
        // Admission is frozen, but dropping the dispatcher's queues before the
        // supervisor handles EOF would manufacture a competing pump failure.
        while state.native_eof_seen && !state.cleanup_started {
            state = self
                .cleanup_wake
                .wait(state)
                .unwrap_or_else(|p| p.into_inner());
        }
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
            // A provisional close must never mask a completed over-budget
            // physical write. Keep genuine failures, including uncertainty.
            if state
                .failure
                .is_none_or(|e| matches!(e, Error::Pump | Error::NativeEof))
            {
                state.failure = Some(Error::Watchdog);
            }
            if state.non_eof_failure.is_none_or(|e| e == Error::Pump) {
                state.non_eof_failure = Some(Error::Watchdog);
            }
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
            || episode <= auto.retired_episode
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
        if episode != 0 && episode <= auto.retired_episode {
            return;
        }
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

    fn advance_episode(&self, previous: u64) -> bool {
        let mut state = self.state();
        if state.failure.is_some()
            || state.auto.disabled
            || self.stopped.load(Ordering::Acquire)
            || state.auto.episode != Some(previous)
            || previous <= state.auto.retired_episode
        {
            return false;
        }
        state.auto.retired_episode = previous;
        state.auto.episode = None;
        state.auto.due = None;
        true
    }

    fn supervision(
        &self,
        allowed_failure: Option<Error>,
    ) -> Result<(Option<Instant>, bool), Error> {
        self.supervision_with_peer_close(allowed_failure, false)
    }

    fn supervision_with_peer_close(
        &self,
        allowed_failure: Option<Error>,
        peer_closed: bool,
    ) -> Result<(Option<Instant>, bool), Error> {
        let mut state = self.state();
        // Explicit PeerClosed also drops the UI queue. Its publisher's Pump
        // failure is secondary. Boundary EOF has a separate, stricter grace.
        if allowed_failure == Some(Error::NativeEof)
            && let Some(error) = state.non_eof_failure
            && !(peer_closed && error == Error::Pump)
        {
            return Err(error);
        }
        if let Some(error) = state.failure
            && Some(error) != allowed_failure
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
        self.wait_dispatch_until(None);
    }

    fn wait_dispatch_until(&self, deadline: Option<Instant>) {
        let mut dirty = self.dirty.lock().unwrap_or_else(|p| p.into_inner());
        while !*dirty && !self.stopped.load(Ordering::Acquire) {
            if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (next, timed) = self
                    .dispatch_wake
                    .wait_timeout(dirty, remaining)
                    .unwrap_or_else(|p| p.into_inner());
                dirty = next;
                if timed.timed_out() {
                    break;
                }
            } else {
                dirty = self
                    .dispatch_wake
                    .wait(dirty)
                    .unwrap_or_else(|p| p.into_inner());
            }
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
    peer_closed: bool,
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

    #[cfg(any(test, feature = "testkit"))]
    #[allow(dead_code)] // Optional deterministic terminal-result scheduling hook.
    pub(crate) fn test_pause_dispatch_failure(&mut self) -> TestGate {
        let gate = Arc::new(TestGateState::default());
        assert!(
            self.shared.test_dispatch_failure.set(gate.clone()).is_ok(),
            "test dispatch failure gate already installed"
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
            peer_closed: false,
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
                    let frame = framing::read_frame(&mut reader)
                        .map_err(|_| Error::NativeRead)?
                        .ok_or(Error::NativeEof)?;
                    #[cfg(all(feature = "testkit", debug_assertions))]
                    if let Some(event) = observation::classify_native_notification(&frame) {
                        observation::emit(event);
                    }
                    Ok(frame)
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
        let dispatch_worker = name == "qb-native-dispatch";
        let worker = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| body(shared.clone())))
                    .unwrap_or(Err(Error::Worker));
                if let Err(error) = result {
                    if dispatch_worker {
                        #[cfg(any(test, feature = "testkit"))]
                        if let Some(gate) = shared.test_dispatch_failure.get() {
                            gate.wait_thread();
                        }
                        shared.dispatch_failed(error);
                    } else {
                        shared.fail(error);
                    }
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
        #[cfg(all(feature = "testkit", debug_assertions))]
        if std::env::var_os("QB_NATIVE_OBSERVATION_PATH").is_some()
            && let Ok(status) = self.jobs.try_wait()
        {
            eprintln!("qb-native-child:{status:?}");
        }
        let (failure_before_cleanup, direct_write_uncertain, eof_other_failure) =
            self.shared.begin_cleanup();
        #[cfg(all(feature = "testkit", debug_assertions))]
        if std::env::var_os("QB_NATIVE_OBSERVATION_PATH").is_some() {
            eprintln!(
                "qb-native-outcome:{result:?}:{failure_before_cleanup:?}:{eof_other_failure:?}"
            );
        }
        let cleanup = self.cleanup();
        if direct_write_uncertain {
            return Err(Error::WriteUncertain);
        }
        // Only the dispatch worker's result survives the cleanup boundary.
        // Cleanup reader/pump errors cannot revoke its ingress (reader_loop).
        // A write failed by job termination stays secondary to a fatal cause.
        let dispatch_failure = self.shared.state().dispatch_failure;
        let root_failure = failure_before_cleanup
            .filter(|error| !matches!(error, Error::Pump | Error::NativeEof))
            .or(dispatch_failure.filter(|error| *error != Error::WriteUncertain))
            .or(failure_before_cleanup);
        match result {
            // A real ingress/worker failure can drop the UI receiver and make
            // the pump finish first. Preserve that already-latched root cause,
            // using the snapshot taken BEFORE job termination causes errors.
            Err(Error::Pump) if root_failure.is_some_and(|e| e != Error::Pump) => {
                Err(root_failure.unwrap_or(Error::Pump))
            }
            // Preserve the supervisor's fatal cause. Stopping jobs to handle a
            // Pump/Watchdog failure can itself make a blocked write fail; that
            // teardown consequence must not replace the original diagnosis.
            Err(error) => Err(error),
            Ok(_) if root_failure.is_some_and(|e| !matches!(e, Error::Pump | Error::NativeEof)) => {
                Err(root_failure.unwrap_or(Error::Worker))
            }
            Ok(status)
                if eof_other_failure.is_some_and(|error| {
                    error != Error::Pump || !self.peer_closed || !status.clean_tui_exit()
                }) =>
            {
                Err(eof_other_failure.unwrap_or(Error::NativeEof))
            }
            Ok(status)
                if failure_before_cleanup == Some(Error::NativeEof) && !status.clean_tui_exit() =>
            {
                // EOF can latch after the supervisor selects a child status.
                // Only an actual TUI zero, with no known server failure, is clean.
                Err(Error::NativeEof)
            }
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
            let (deadline, auto_disabled) = match self.shared.supervision(None) {
                Err(Error::NativeEof) => {
                    return self.native_eof_exit(Instant::now() + TUI_EXIT_GRACE).await;
                }
                result => result?,
            };
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
                    // Never poll this completed future again during EOF grace.
                    self.pump.take();
                    #[cfg(all(feature = "testkit", debug_assertions))]
                    if std::env::var_os("QB_NATIVE_OBSERVATION_PATH").is_some()
                        && let Err(error) = result
                    {
                        eprintln!("qb-native-pump:{error}");
                    }
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
        self.peer_closed = true;
        // Stop new work without touching ingress: an in-flight physical writer
        // may own its mutex. Never poll the completed pump again.
        self.shared.stopped.store(true, Ordering::Release);
        self.stop_auto();
        self.shared.wake_dispatch();
        let deadline = Instant::now() + TUI_EXIT_GRACE;
        loop {
            let (operation_deadline, _) = match self.shared.supervision(Some(Error::Pump)) {
                Err(Error::NativeEof) => return self.native_eof_exit(deadline).await,
                result => result?,
            };
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

    async fn native_eof_exit(&mut self, deadline: Instant) -> Result<JobStatus, Error> {
        // Only read_frame's EOF BETWEEN complete frames reaches this path.
        // Freeze admission, but never terminate jobs to manufacture exit proof.
        self.shared.stopped.store(true, Ordering::Release);
        self.stop_auto();
        self.shared.wake_dispatch();
        #[cfg(any(test, feature = "testkit"))]
        self.shared.test_record(TestLifecycle::NativeEofGrace);
        #[cfg(all(feature = "testkit", debug_assertions))]
        let mut last_status = None;
        loop {
            // UI reader closure alone does not prove a clean socket close.
            // Poll the actual pump once before permitting that provisional
            // Pump report; invalid/binary/transport failures remain fatal.
            if let Some(pump) = self.pump.as_mut()
                && let Some(result) = pump.as_mut().now_or_never()
            {
                self.pump.take();
                match result {
                    Err(socket_pump::Error::PeerClosed) => self.peer_closed = true,
                    _ => return Err(Error::Pump),
                }
            }
            let (operation_deadline, _) = self
                .shared
                .supervision_with_peer_close(Some(Error::NativeEof), self.peer_closed)?;
            if Instant::now() >= deadline {
                return Err(Error::NativeEof);
            }
            let status = self.jobs.try_wait().map_err(|_| Error::Job)?;
            #[cfg(all(feature = "testkit", debug_assertions))]
            if last_status != Some(status) {
                // JobStatus contains only fixed variants and numeric exit codes.
                if std::env::var_os("QB_NATIVE_OBSERVATION_PATH").is_some() {
                    eprintln!("qb-native-child:{status:?}");
                }
                last_status = Some(status);
            }
            match status {
                // Native 0.164 trial 28 reported TuiExited(0) before cleanup:
                // the parent still owns the app-server and must stop it after
                // that independent TUI exit. BOTH zero is sufficient, not required.
                status if status.clean_tui_exit() => return Ok(status),
                JobStatus::Running | JobStatus::ServerExited(0) => {}
                _ => return Err(Error::NativeEof),
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
            self.shared.test_dispatch_failure.get(),
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
        if failed {
            Err(Error::Cleanup)
        } else {
            #[cfg(all(feature = "testkit", debug_assertions))]
            observation::emit(Event::Cleanup);
            Ok(())
        }
    }
}

impl<J: Jobs> Drop for ManagedParent<J> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

enum AutoPhase {
    Learning,
    StopRead {
        deadline: Instant,
    },
    Waiting {
        ticket: StoppedGoalTicket,
        episode: u64,
    },
    FinalRead {
        episode: u64,
    },
    AwaitProgress {
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
    #[cfg(all(feature = "testkit", debug_assertions))]
    let mut acknowledged_emitted = false;
    while !shared.stopped.load(Ordering::Acquire) {
        if !shared.enabled.load(Ordering::Acquire) {
            shared.wait_dispatch();
            continue;
        }
        if shared.auto_disabled() && core.is_some() {
            terminal_auto(&mut parent, &mut core, &mut phase, shared);
        }
        if matches!(phase, AutoPhase::StopRead { deadline } if Instant::now() >= deadline) {
            terminal_auto(&mut parent, &mut core, &mut phase, shared);
        }

        let deadline = shared.start_event();
        let episode = match phase {
            AutoPhase::FinalRead { episode } => Some(episode),
            _ => None,
        };
        let mut grant: Option<Grant> = None;
        let mut attempted = false;
        #[cfg(all(feature = "testkit", debug_assertions))]
        let delivery_before = parent.delivery();
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
            #[cfg(all(feature = "testkit", debug_assertions))]
            observation::emit(Event::Consumed {
                episode,
                granted: matches!(consumed, Ok(Consumption::Granted(_))),
            });
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
        #[cfg(all(feature = "testkit", debug_assertions))]
        if result == Ok(true) && attempted && grant.is_some() {
            observation::emit(Event::ActivationWritten);
        }
        #[cfg(all(feature = "testkit", debug_assertions))]
        if !acknowledged_emitted
            && result.is_ok()
            && delivery_before != Delivery::Acknowledged
            && parent.delivery() == Delivery::Acknowledged
        {
            acknowledged_emitted = true;
            observation::emit(Event::ActivationAcknowledged);
        }
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
        if attempted && result.is_ok() {
            phase = AutoPhase::AwaitProgress {
                episode: episode.ok_or(Error::Worker)?,
            };
        } else if attempted
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

        if let AutoPhase::AwaitProgress { episode } = phase {
            match parent.take_next_episode_proof() {
                Ok(Some(proof)) => {
                    let mut nonce = [0_u8; 16];
                    if rand::rngs::OsRng.try_fill_bytes(&mut nonce).is_err() {
                        terminal_auto(&mut parent, &mut core, &mut phase, shared);
                        continue;
                    }
                    let namespace = format!(
                        "qb-goal-{}:",
                        nonce
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                    );
                    let deadline = Instant::now() + EVENT_BUDGET;
                    let advanced = match core.as_ref() {
                        Some(core) if shared.await_core(deadline) => runtime
                            .block_on(core.advance_episode(episode, deadline))
                            .is_ok(),
                        _ => false,
                    };
                    shared.end_event();
                    if advanced
                        && shared.advance_episode(episode)
                        && parent.rearm(proof, namespace).is_ok()
                    {
                        phase = AutoPhase::Learning;
                        #[cfg(all(feature = "testkit", debug_assertions))]
                        {
                            acknowledged_emitted = false;
                        }
                    } else {
                        terminal_auto(&mut parent, &mut core, &mut phase, shared);
                    }
                }
                Ok(None) => {}
                Err(_) => terminal_auto(&mut parent, &mut core, &mut phase, shared),
            }
        }

        if matches!(phase, AutoPhase::Learning | AutoPhase::StopRead { .. })
            && let Some(ticket) = parent.stopped_ticket()
        {
            #[cfg(all(feature = "testkit", debug_assertions))]
            if let Ok(thread_id) = ThreadId::parse(ticket.thread_id()) {
                observation::emit(Event::StoppedThread { thread_id });
            }
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
            #[cfg(all(feature = "testkit", debug_assertions))]
            match &observed {
                Ok(Observation::Refused(reason)) => {
                    observation::emit(Event::ObservationRejected { reason: *reason });
                }
                Err(_) => observation::emit(Event::ObservationFailed),
                _ => {}
            }
            let immediate = matches!(observed, Ok(Observation::Due { .. }));
            match observed {
                Ok(Observation::Waiting { episode, .. }) | Ok(Observation::Due { episode })
                    if shared.bind_episode(episode) =>
                {
                    #[cfg(all(feature = "testkit", debug_assertions))]
                    observation::emit(Event::Observed { episode });
                    if immediate {
                        shared.publish_due(episode);
                    }
                    phase = AutoPhase::Waiting { ticket, episode };
                }
                _ => terminal_auto(&mut parent, &mut core, &mut phase, shared),
            }
            continue;
        }

        if matches!(phase, AutoPhase::Learning) {
            let deadline = Instant::now() + EVENT_BUDGET;
            if shared.enter_physical(deadline) {
                let result = parent.begin_stop_read();
                shared.end_event();
                match result {
                    Ok(true) => phase = AutoPhase::StopRead { deadline },
                    Ok(false) => {}
                    Err(error) => return Err(dispatch_error(error)),
                }
            } else {
                terminal_auto(&mut parent, &mut core, &mut phase, shared);
            }
        }
        if matches!(phase, AutoPhase::StopRead { .. }) && parent.delivery() == Delivery::Cancelled {
            terminal_auto(&mut parent, &mut core, &mut phase, shared);
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
                #[cfg(all(feature = "testkit", debug_assertions))]
                observation::emit(Event::Inspected {
                    episode,
                    ready: matches!(inspected, Ok(Inspection::Ready)),
                });
                if matches!(inspected, Ok(Inspection::Pending)) {
                    // Retain the original stopped ticket and ingress checkpoint.
                    // Only a later evidence-driven Due can trigger another Inspect;
                    // Pending never performs a native goal read or resume write.
                    phase = AutoPhase::Waiting { ticket, episode };
                } else if matches!(inspected, Ok(Inspection::Ready))
                    && shared.enter_physical(deadline)
                {
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
            let deadline = match phase {
                AutoPhase::StopRead { deadline } => Some(deadline),
                _ => None,
            };
            shared.wait_dispatch_until(deadline);
        }
    }
    shared.retain_eof_dispatch();
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
    // Boundary native EOF is adjudicated against an actual clean TUI exit. Do
    // not manufacture ingress failure before that bounded status check.
    // Frame/publication failures still close ingress and remain fatal.
    let owned_shutdown = (source == Source::Native && result == Err(Error::NativeEof))
        || (source == Source::Ui && result == Err(Error::Pump))
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
    // Once job drain starts, pipe/read failures are cancellation consequences.
    // Do not turn them into an apparently genuine late dispatch Ingress error.
    // A reader that may still close ingress has already recorded its root cause
    // under the same state lock used by begin_cleanup.
    let cleanup_started = shared.state().cleanup_started;
    if !owned_shutdown && !cleanup_started {
        publisher.close();
    }
    result
}

#[cfg(test)]
mod eof_tests {
    use super::*;

    #[test]
    fn cleanup_reader_failures_do_not_revoke_dispatch_ingress() {
        for (source, error) in [
            (Source::Native, Error::NativeRead),
            (Source::Ui, Error::Pump),
        ] {
            let shared = Shared::default();
            let (ingress, publisher) = super::super::ingress::channel();
            let _ = shared.begin_cleanup();
            assert_eq!(
                reader_loop(&shared, &publisher, || Err(error), source),
                Err(error)
            );
            assert_eq!(ingress.backlog(), Ok(0));
        }
    }

    #[test]
    fn boundary_eof_grace_preserves_every_other_latched_failure() {
        for error in [
            Error::Pump,
            Error::Ingress,
            Error::Dispatch,
            Error::Worker,
            Error::NativeRead,
            Error::WriteUncertain,
            Error::Watchdog,
        ] {
            for eof_first in [false, true] {
                let shared = Shared::default();
                for failure in if eof_first {
                    [Error::NativeEof, error]
                } else {
                    [error, Error::NativeEof]
                } {
                    shared.fail(failure);
                }
                assert_eq!(shared.supervision(Some(Error::NativeEof)), Err(error));
            }
        }
    }

    #[test]
    fn boundary_eof_cannot_erase_expired_physical_deadline() {
        let shared = Shared::default();
        shared.fail(Error::NativeEof);
        shared.state().operation = Operation::Physical(Instant::now());
        assert_eq!(
            shared.supervision(Some(Error::NativeEof)),
            Err(Error::Watchdog)
        );
        shared.end_event();
        assert_eq!(
            shared.supervision(Some(Error::NativeEof)),
            Err(Error::Watchdog)
        );
    }

    #[test]
    fn confirmed_peer_close_cannot_erase_completed_physical_deadline() {
        for eof_first in [true, false] {
            let shared = Shared::default();
            for error in if eof_first {
                [Error::NativeEof, Error::Pump]
            } else {
                [Error::Pump, Error::NativeEof]
            } {
                shared.fail(error);
            }
            shared.state().operation = Operation::Physical(Instant::now());
            shared.end_event();
            assert_eq!(
                shared.supervision_with_peer_close(Some(Error::NativeEof), true),
                Err(Error::Watchdog)
            );
            assert_eq!(shared.begin_cleanup().2, Some(Error::Watchdog));
        }
    }
    #[test]
    fn advancing_mailbox_retires_old_due_without_reusing_authority() {
        let shared = Shared::default();
        assert!(!shared.advance_episode(7));
        assert!(shared.bind_episode(7));
        shared.publish_due(7);
        assert!(shared.take_due(7));
        assert!(shared.advance_episode(7));
        assert!(!shared.advance_episode(7));
        shared.publish_due(7);
        assert!(!shared.take_due(7));
        assert!(!shared.auto_disabled());
        assert!(shared.bind_episode(8));
        shared.publish_due(7);
        assert!(!shared.take_due(8));
        shared.publish_due(8);
        assert!(shared.take_due(8));
    }

    #[test]
    fn mailbox_never_advances_after_failure_or_auto_veto() {
        for fail in [true, false] {
            let shared = Shared::default();
            assert!(shared.bind_episode(7));
            if fail {
                shared.fail(Error::NativeEof);
            } else {
                shared.disable_auto();
            }
            assert!(!shared.advance_episode(7));
        }
    }
}
