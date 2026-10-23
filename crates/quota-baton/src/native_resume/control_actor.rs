//! Owned single-episode core control actor; no native writes or authority policy.
//!
//! The dedicated serial parent sends Consume INSIDE native_frame_committed via
//! its runtime handle, checks the opaque Grant, then uses ingress.with_current
//! around the whole physical write. Due/Ready never authorize that write. Keep
//! this actor alive until the write ends; then Close. No reconnect or retries.
//!
//! Command deadlines are absolute, including queue time, and capped at five
//! seconds. Initial-command and Due idle waits may have no deadline; after Due,
//! waiting for each explicit Inspect/Consume command is capped at five seconds.
//! next_due is polled continuously after Waiting: ONLY an actor command, actor
//! shutdown, hint receiver loss or that lifetime deadline can interrupt it.
//! Ordinary UI/native ingress must NOT be sent as actor commands. Early Inspect
//! or Consume terminates the episode; no inspect retry after refusal is supported.
//!
//! run spawns nothing. Outer parent retains/aborts/joins its task and both jobs.
//! Drop of run closes control, including a polled operation. Like CoreControl,
//! this does not independently observe peer close after returning a Grant; its
//! deadline, parent lifetime and ingress/write vetoes still apply.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, mpsc, oneshot},
    time::{Instant, sleep_until, timeout_at},
};

use super::core_control::{self, Consumption, CoreControl, Inspection, Observation};

pub(crate) const COMMAND_CAPACITY: usize = 8;
pub(crate) const HINT_CAPACITY: usize = 1;
const COMMAND_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Core(core_control::Error),
    Timeout,
    Cancelled,
    Closed,
    ParentGone,
    ReplyLost,
    HintLost,
    HintFull,
    QueueFull,
    Phase,
    Refused,
    InvalidThread,
}

/// A notification, deliberately not convertible to a core Grant.
pub(crate) struct DueHint {
    episode: u64,
}

impl DueHint {
    pub(crate) fn episode(&self) -> u64 {
        self.episode
    }
}

enum Command {
    Observe {
        thread: String,
        deadline: Instant,
        reply: oneshot::Sender<Result<Observation, Error>>,
    },
    Inspect {
        episode: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<Inspection, Error>>,
    },
    Consume {
        episode: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<Consumption, Error>>,
    },
    Advance {
        episode: u64,
        deadline: Instant,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    #[allow(dead_code)] // Explicit protocol close is exercised by isolated clients.
    Close {
        reply: oneshot::Sender<Result<(), Error>>,
    },
}

impl Command {
    fn refuse(self, error: Error) {
        match self {
            Self::Observe { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Inspect { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Consume { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Close { reply } => {
                let _ = reply.send(Err(error));
            }
            Self::Advance { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

struct Shutdown {
    stopped: AtomicBool,
    changed: Notify,
}

impl Shutdown {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn cancelled(&self) {
        let changed = self.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if !self.stopped.load(Ordering::Acquire) {
            changed.await;
        }
    }
}

struct RequestGuard {
    shutdown: Arc<Shutdown>,
    armed: bool,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shutdown.stop();
        }
    }
}

/// Not Clone. Future cancellation after submission closes this whole episode.
pub(crate) struct Parent {
    commands: mpsc::Sender<Command>,
    due: Option<mpsc::Receiver<DueHint>>,
    shutdown: Arc<Shutdown>,
}

pub(crate) struct ActorIo {
    commands: mpsc::Receiver<Command>,
    hints: mpsc::Sender<DueHint>,
    shutdown: Arc<Shutdown>,
}

pub(crate) fn channels() -> (Parent, ActorIo) {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (hints, due) = mpsc::channel(HINT_CAPACITY);
    let shutdown = Arc::new(Shutdown {
        stopped: AtomicBool::new(false),
        changed: Notify::new(),
    });
    (
        Parent {
            commands,
            due: Some(due),
            shutdown: shutdown.clone(),
        },
        ActorIo {
            commands: receiver,
            hints,
            shutdown,
        },
    )
}

impl Parent {
    /// Transfer hints once to the supervisor while the serial dispatcher retains
    /// commands. Parent drop still cancels the actor even if this receiver lives.
    pub(crate) fn take_due(&mut self) -> Option<mpsc::Receiver<DueHint>> {
        self.due.take()
    }

    async fn request<T>(
        &self,
        deadline: Instant,
        command: impl FnOnce(Instant, oneshot::Sender<Result<T, Error>>) -> Command,
    ) -> Result<T, Error> {
        let deadline = deadline.min(Instant::now() + COMMAND_LIMIT);
        if Instant::now() >= deadline {
            self.shutdown.stop();
            return Err(Error::Timeout);
        }
        if self.shutdown.stopped.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let (reply, receiver) = oneshot::channel();
        if let Err(error) = self.commands.try_send(command(deadline, reply)) {
            self.shutdown.stop();
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => Error::QueueFull,
                mpsc::error::TrySendError::Closed(_) => Error::ParentGone,
            });
        }
        let mut guard = RequestGuard {
            shutdown: self.shutdown.clone(),
            armed: true,
        };
        let result = timeout_at(deadline, receiver)
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::ReplyLost)?;
        guard.armed = false;
        result
    }

    pub(crate) async fn observe(
        &self,
        thread: &str,
        deadline: Instant,
    ) -> Result<Observation, Error> {
        if thread.len() > 256 {
            self.shutdown.stop();
            return Err(Error::InvalidThread);
        }
        self.request(deadline, |deadline, reply| Command::Observe {
            thread: thread.to_owned(),
            deadline,
            reply,
        })
        .await
    }

    pub(crate) async fn inspect(
        &self,
        episode: u64,
        deadline: Instant,
    ) -> Result<Inspection, Error> {
        self.request(deadline, |deadline, reply| Command::Inspect {
            episode,
            deadline,
            reply,
        })
        .await
    }

    pub(crate) async fn consume(
        &self,
        episode: u64,
        deadline: Instant,
    ) -> Result<Consumption, Error> {
        self.request(deadline, |deadline, reply| Command::Consume {
            episode,
            deadline,
            reply,
        })
        .await
    }

    pub(crate) async fn advance_episode(
        &self,
        episode: u64,
        deadline: Instant,
    ) -> Result<(), Error> {
        self.request(deadline, |deadline, reply| Command::Advance {
            episode,
            deadline,
            reply,
        })
        .await
    }

    #[allow(dead_code)] // Run ownership normally cancels via Parent Drop.
    pub(crate) async fn close(&self, deadline: Instant) -> Result<(), Error> {
        self.request(deadline, |_, reply| Command::Close { reply })
            .await
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        self.shutdown.stop();
    }
}

struct OwnedControl(CoreControl);

impl Drop for OwnedControl {
    fn drop(&mut self) {
        self.0.close();
    }
}

enum Interrupted {
    Command(Option<Command>),
    Error(Error),
}

/// A command interrupt is handled AFTER dropping the polled operation, so Close
/// acknowledgement cannot precede socket closure or invalidate a future's borrow.
async fn exchange<T>(
    future: impl Future<Output = Result<T, core_control::Error>>,
    reply: &mut oneshot::Sender<Result<T, Error>>,
    io: &mut ActorIo,
    deadline: Instant,
) -> Result<T, Interrupted> {
    tokio::select! {
        biased;
        _ = io.shutdown.cancelled() => Err(Interrupted::Error(Error::Cancelled)),
        _ = io.hints.closed() => Err(Interrupted::Error(Error::HintLost)),
        _ = reply.closed() => Err(Interrupted::Error(Error::ReplyLost)),
        _ = sleep_until(deadline) => Err(Interrupted::Error(Error::Timeout)),
        command = io.commands.recv() => Err(Interrupted::Command(command)),
        result = future => result.map_err(|error| Interrupted::Error(Error::Core(error))),
    }
}

async fn idle_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn bounded_deadline(deadline: Instant, lifetime: Option<Instant>) -> Instant {
    let deadline = deadline.min(Instant::now() + COMMAND_LIMIT);
    lifetime.map_or(deadline, |lifetime| deadline.min(lifetime))
}

async fn command(io: &mut ActorIo, deadline: Option<Instant>) -> Result<Command, Interrupted> {
    tokio::select! {
        biased;
        _ = io.shutdown.cancelled() => Err(Interrupted::Error(Error::Cancelled)),
        _ = io.hints.closed() => Err(Interrupted::Error(Error::HintLost)),
        _ = idle_deadline(deadline) => Err(Interrupted::Error(Error::Timeout)),
        command = io.commands.recv() => command.ok_or(Interrupted::Command(None)),
    }
}

fn terminate(control: &mut CoreControl, interrupted: Interrupted) -> Error {
    control.close();
    match interrupted {
        Interrupted::Error(error) => error,
        Interrupted::Command(Some(Command::Close { reply })) => {
            let _ = reply.send(Ok(()));
            Error::Closed
        }
        Interrupted::Command(Some(command)) => {
            command.refuse(Error::Phase);
            Error::Phase
        }
        Interrupted::Command(None) => Error::ParentGone,
    }
}

fn emit_due(io: &ActorIo, episode: u64) -> Result<(), Error> {
    io.hints
        .try_send(DueHint { episode })
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => Error::HintFull,
            mpsc::error::TrySendError::Closed(_) => Error::HintLost,
        })
}

/// Parent supplies the episode lifetime; elapsed lifetime is always terminal.
#[cfg(test)]
#[allow(dead_code)] // Finite-lifetime adapter for isolated actor tests.
pub(crate) async fn run(control: CoreControl, io: ActorIo, lifetime: Instant) -> Result<(), Error> {
    run_inner(control, io, Some(lifetime)).await
}

/// Cancellation-only initial-command/Due idle waits; all exchanges and post-Due
/// phases remain bounded. Outer parent must retain/abort/join this future.
pub(crate) async fn run_until_cancelled(control: CoreControl, io: ActorIo) -> Result<(), Error> {
    run_inner(control, io, None).await
}

async fn run_inner(
    control: CoreControl,
    mut io: ActorIo,
    lifetime: Option<Instant>,
) -> Result<(), Error> {
    let mut owned = OwnedControl(control);
    let result = loop {
        if let Err(error) = episode(&mut owned.0, &mut io, lifetime).await {
            break Err(error);
        }
    };
    // Close before returning or dropping queued replies, including graceful Close.
    owned.0.close();
    io.commands.close();
    while let Ok(command) = io.commands.try_recv() {
        command.refuse(Error::Closed);
    }
    match result {
        Err(Error::Closed) => Ok(()),
        result => result,
    }
}

async fn episode(
    control: &mut CoreControl,
    io: &mut ActorIo,
    lifetime: Option<Instant>,
) -> Result<(), Error> {
    let first = match command(io, lifetime).await {
        Ok(command) => command,
        Err(error) => return Err(terminate(control, error)),
    };
    let Command::Observe {
        thread,
        deadline,
        mut reply,
    } = first
    else {
        return Err(terminate(control, Interrupted::Command(Some(first))));
    };
    let deadline = bounded_deadline(deadline, lifetime);
    let observed =
        match exchange(control.observe(&thread, deadline), &mut reply, io, deadline).await {
            Ok(observed) => observed,
            Err(error) => {
                let error = terminate(control, error);
                let _ = reply.send(Err(error));
                return Err(error);
            }
        };
    let (episode, waiting) = match &observed {
        Observation::Waiting { episode, .. } => (*episode, true),
        Observation::Due { episode } => (*episode, false),
        Observation::Refused(_) => {
            control.close();
            reply.send(Ok(observed)).map_err(|_| Error::ReplyLost)?;
            return Err(Error::Refused);
        }
    };
    reply.send(Ok(observed)).map_err(|_| Error::ReplyLost)?;
    if waiting {
        let due = tokio::select! {
            biased;
            _ = io.shutdown.cancelled() => Err(Interrupted::Error(Error::Cancelled)),
            _ = io.hints.closed() => Err(Interrupted::Error(Error::HintLost)),
            next = io.commands.recv() => Err(Interrupted::Command(next)),
            result = async {
                match lifetime {
                    Some(lifetime) => control.next_due(lifetime).await,
                    None => control.next_due_until_cancelled().await,
                }
            } => result.map_err(|error| Interrupted::Error(Error::Core(error))),
        };
        match due {
            Ok(due) if due == episode => {}
            Ok(_) => return Err(Error::Phase),
            Err(error) => return Err(terminate(control, error)),
        }
    }
    emit_due(io, episode)?;

    let inspect_deadline = bounded_deadline(Instant::now() + COMMAND_LIMIT, lifetime);
    let inspect = match command(io, Some(inspect_deadline)).await {
        Ok(command) => command,
        Err(error) => return Err(terminate(control, error)),
    };
    let Command::Inspect {
        episode: expected,
        deadline,
        mut reply,
    } = inspect
    else {
        return Err(terminate(control, Interrupted::Command(Some(inspect))));
    };
    if expected != episode {
        reply.send(Err(Error::Phase)).ok();
        return Err(Error::Phase);
    }
    let deadline = bounded_deadline(deadline, lifetime);
    let inspected =
        match exchange(control.inspect(episode, deadline), &mut reply, io, deadline).await {
            Ok(result) => result,
            Err(error) => {
                let error = terminate(control, error);
                let _ = reply.send(Err(error));
                return Err(error);
            }
        };
    let ready = matches!(inspected, Inspection::Ready);
    if !ready {
        control.close();
    }
    reply.send(Ok(inspected)).map_err(|_| Error::ReplyLost)?;
    if !ready {
        return Err(Error::Refused);
    }

    let consume_deadline = bounded_deadline(Instant::now() + COMMAND_LIMIT, lifetime);
    let consume = match command(io, Some(consume_deadline)).await {
        Ok(command) => command,
        Err(error) => return Err(terminate(control, error)),
    };
    let Command::Consume {
        episode: expected,
        deadline,
        mut reply,
    } = consume
    else {
        return Err(terminate(control, Interrupted::Command(Some(consume))));
    };
    if expected != episode {
        reply.send(Err(Error::Phase)).ok();
        return Err(Error::Phase);
    }
    let deadline = bounded_deadline(deadline, lifetime);
    let consumed =
        match exchange(control.consume(episode, deadline), &mut reply, io, deadline).await {
            Ok(result) => result,
            Err(error) => {
                let error = terminate(control, error);
                let _ = reply.send(Err(error));
                return Err(error);
            }
        };
    let granted = matches!(consumed, Consumption::Granted(_));
    if !granted {
        control.close();
    }
    reply.send(Ok(consumed)).map_err(|_| Error::ReplyLost)?;
    if !granted {
        return Err(Error::Refused);
    }
    // The receipt retains its original five-second physical deadline. The
    // owner may then wait for real native ACK/progress without extending it.
    // Only an explicit Advance can open a later, separately qualified episode.
    let next = match command(io, lifetime).await {
        Ok(command) => command,
        Err(error) => return Err(terminate(control, error)),
    };
    let Command::Advance {
        episode: previous,
        deadline,
        mut reply,
    } = next
    else {
        return Err(terminate(control, Interrupted::Command(Some(next))));
    };
    if previous != episode {
        reply.send(Err(Error::Phase)).ok();
        return Err(Error::Phase);
    }
    let deadline = bounded_deadline(deadline, lifetime);
    match exchange(
        control.advance_episode(previous, deadline),
        &mut reply,
        io,
        deadline,
    )
    .await
    {
        Ok(()) => reply.send(Ok(())).map_err(|_| Error::ReplyLost),
        Err(error) => {
            let error = terminate(control, error);
            reply.send(Err(error)).ok();
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hint_capacity_is_one_and_overflow_is_not_authority() {
        let (mut parent, io) = channels();
        let mut due = parent.take_due().unwrap();
        assert!(parent.take_due().is_none());
        assert_eq!(emit_due(&io, 7), Ok(()));
        assert_eq!(emit_due(&io, 7), Err(Error::HintFull));
        assert_eq!(due.recv().await.unwrap().episode(), 7);
        drop(parent);
        assert!(io.shutdown.stopped.load(Ordering::Acquire));
        drop(due);
        assert_eq!(emit_due(&io, 7), Err(Error::HintLost));
    }
}
