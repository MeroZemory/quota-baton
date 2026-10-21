//! Cancellation competes with admission, never with an admitted write.
use std::sync::{Arc, Mutex};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Pending,
    Cancelled,
    Admitted,
}

struct State {
    phase: Phase,
    caller_gone: bool,
}

struct Inner {
    deadline: Instant,
    state: Mutex<State>,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

#[derive(Clone)]
pub(crate) struct IssueOperation(Arc<Inner>);

pub(crate) struct CallerGuard(Option<IssueOperation>);

impl IssueOperation {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self::with_clock(deadline, Arc::new(Instant::now))
    }

    fn with_clock(deadline: Instant, clock: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        Self(Arc::new(Inner {
            deadline,
            state: Mutex::new(State {
                phase: Phase::Pending,
                caller_gone: false,
            }),
            clock,
        }))
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.0.deadline
    }

    pub(crate) fn caller_guard(&self) -> CallerGuard {
        CallerGuard(Some(self.clone()))
    }

    /// Call after queue/gate acquisition; this is not commit admission.
    pub(crate) fn check(&self) -> Result<(), ()> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.phase {
            Phase::Admitted => Ok(()),
            Phase::Cancelled => Err(()),
            Phase::Pending => {
                if (self.0.clock)() >= self.0.deadline {
                    state.phase = Phase::Cancelled;
                    Err(())
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Linearization: the short state lock serializes deadline observation,
    /// Pending->Admitted and caller Drop. Hold the actual authority gate first.
    /// Once admitted, all later checks allow required completion, even on Drop.
    pub(crate) fn admit(&self) -> Result<(), ()> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.phase {
            Phase::Admitted => Ok(()),
            Phase::Cancelled => Err(()),
            Phase::Pending => {
                if (self.0.clock)() >= self.0.deadline {
                    state.phase = Phase::Cancelled;
                    Err(())
                } else {
                    state.phase = Phase::Admitted;
                    Ok(())
                }
            }
        }
    }

    /// Reply handoff is still refused after expiry. This never undoes admission.
    pub(crate) fn can_reply(&self) -> bool {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.caller_gone && (self.0.clock)() < self.0.deadline
    }
}

impl CallerGuard {
    /// Normal handoff to an outer response owner is not cancellation.
    pub(crate) fn disarm(mut self) {
        self.0.take();
    }
}

impl Drop for CallerGuard {
    fn drop(&mut self) {
        let Some(operation) = &self.0 else { return; };
        let mut state = operation.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.caller_gone = true;
        if state.phase == Phase::Pending {
            state.phase = Phase::Cancelled;
        }
    }
}

#[cfg(test)]
mod tests {
use super::*;
use std::sync::{Barrier, atomic::{AtomicU64, Ordering}, mpsc};
use std::time::Duration;

fn fixture() -> (IssueOperation, Arc<AtomicU64>) {
    let origin = Instant::now();
    let seconds = Arc::new(AtomicU64::new(0));
    let clock = seconds.clone();
    (IssueOperation::with_clock(origin + Duration::from_secs(8),
        Arc::new(move || origin + Duration::from_secs(clock.load(Ordering::SeqCst)))), seconds)
}

#[test]
fn same_deadline_covers_preplacement_and_catalog_then_admission() {
    let (op, seconds) = fixture();
    let deadline = op.deadline();
    seconds.store(3, Ordering::SeqCst); // enumeration
    assert!(op.check().is_ok());
    seconds.store(7, Ordering::SeqCst); // all catalog waves
    assert_eq!(deadline, op.deadline());
    assert!(op.check().is_ok());
    seconds.store(8, Ordering::SeqCst); // queued commit cannot reset budget
    assert!(op.admit().is_err());
}

#[test]
fn normal_handoff_disarms_inner_guard_without_cancelling_outer_owner() {
    let (op, _) = fixture();
    let outer = op.caller_guard();
    op.caller_guard().disarm();
    assert!(op.can_reply());
    assert!(op.admit().is_ok());
    drop(outer);
    assert!(!op.can_reply());
    assert!(op.check().is_ok());
}

#[test]
fn cancel_wins_while_waiting_for_each_gate() {
    // Model the exact check-after-lock seams: binding mutation and policy writer.
    // Real-caller tests still need their actual gate fixtures (README).
    for _gate in ["binding mutation", "policy writer"] {
        let (op, _) = fixture();
        let caller = op.caller_guard();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let ready = Arc::new(Barrier::new(2));
        let (send, recv) = mpsc::channel();
        let task = {
            let op = op.clone();
            let gate = gate.clone();
            let ready = ready.clone();
            std::thread::spawn(move || {
                ready.wait();
                let _gate = gate.lock().unwrap();
                send.send(op.check().and_then(|_| op.admit())).unwrap();
            })
        };
        ready.wait();
        drop(caller); // cancellation precedes release, independent of scheduler
        drop(held);
        assert!(recv.recv().unwrap().is_err());
        task.join().unwrap();
    }
}

#[test]
fn deadline_wins_while_waiting_for_writer() {
    let (op, seconds) = fixture();
    let gate = Arc::new(Mutex::new(()));
    let held = gate.lock().unwrap();
    let ready = Arc::new(Barrier::new(2));
    let task = {
        let op = op.clone();
        let gate = gate.clone();
        let ready = ready.clone();
        std::thread::spawn(move || {
            ready.wait();
            let _writer = gate.lock().unwrap();
            op.admit()
        })
    };
    ready.wait();
    seconds.store(8, Ordering::SeqCst);
    drop(held);
    assert!(task.join().unwrap().is_err());
}

#[test]
fn admission_wins_before_caller_drop_and_slow_write() {
    let (op, seconds) = fixture();
    let caller = op.caller_guard();
    let admitted = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let task = {
        let op = op.clone();
        let admitted = admitted.clone();
        let resume = resume.clone();
        std::thread::spawn(move || {
            op.admit().unwrap();
            admitted.wait();
            resume.wait(); // synthetic already-admitted durable write
            assert!(op.check().is_ok());
            assert!(op.admit().is_ok()); // late dispatch must remain possible
        })
    };
    admitted.wait();
    drop(caller);
    seconds.store(100, Ordering::SeqCst);
    assert!(!op.can_reply());
    resume.wait();
    task.join().unwrap();
    assert_eq!(op.0.state.lock().unwrap().phase, Phase::Admitted);
}

#[test]
fn completed_output_still_owns_cleanup_until_receiver_drop() {
    struct Owned(Arc<AtomicU64>);
    impl Drop for Owned {
        fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
    }
    let drops = Arc::new(AtomicU64::new(0));
    let (send, recv) = mpsc::channel();
    let task_drops = drops.clone();
    std::thread::spawn(move || send.send(Owned(task_drops)).unwrap_or_else(|_| panic!("receiver")))
        .join().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(recv); // analogous to unconsumed blocking task output, not a Tokio proof
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn actual_async_future_drop_signals_pending_operation() {
    let (op, _) = fixture();
    let task_op = op.clone();
    let mut future = Box::pin(async move {
        let _caller = task_op.caller_guard();
        std::future::pending::<()>().await;
    });
    use std::future::Future;
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    assert!(future.as_mut().poll(&mut cx).is_pending());
    drop(future);
    assert!(op.admit().is_err());
}

}

