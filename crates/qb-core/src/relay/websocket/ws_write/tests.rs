//! Pure sink boundary tests; actual pump wiring is checked separately.
//! Manual polling uses no socket, executor, timer, reservation or runtime gate.
//! drive() models only write()'s fragment send/flush loop; coordinator must also
//! verify actual write_initial/write wiring and cancellation ownership later.
use super::*;
use futures_util::{SinkExt, task::noop_waker};
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio_tungstenite::tungstenite::protocol::frame::{
    Frame,
    coding::{Data, OpCode},
};

const ACCOUNT: &str = "draft-account";
const KEY: &str = "draft-key";
const MODEL: &str = "draft-model";

#[derive(Default)]
struct Trace {
    sends: usize,
    flushes: usize,
    final_frames: Vec<bool>,
}
#[derive(Default)]
struct Script {
    // One-based next send/flush index. Pending is deliberately persistent:
    // tests poll once and cancel; no wakeup/progress claim is made.
    ready_pending: Option<usize>,
    ready_error: Option<usize>,
    send_error: Option<usize>,
    flush_pending: Option<usize>,
    flush_error: Option<usize>,
}
struct PhysicalSink {
    script: Script,
    trace: Arc<Mutex<Trace>>,
}
impl Sink<Message> for PhysicalSink {
    type Error = &'static str;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let next = self.trace.lock().unwrap().sends + 1;
        if self.script.ready_error == Some(next) {
            Poll::Ready(Err("ready"))
        } else if self.script.ready_pending == Some(next) {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
    fn start_send(self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
        let mut trace = self.trace.lock().unwrap();
        trace.sends += 1;
        if self.script.send_error == Some(trace.sends) {
            return Err("send");
        }
        match message {
            Message::Frame(frame) => trace.final_frames.push(frame.header().is_final),
            _ => panic!("drive must send physical frames"),
        }
        Ok(())
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut trace = self.trace.lock().unwrap();
        let next = trace.flushes + 1;
        if self.script.flush_error == Some(next) {
            Poll::Ready(Err("flush"))
        } else if self.script.flush_pending == Some(next) {
            Poll::Pending
        } else {
            trace.flushes += 1;
            Poll::Ready(Ok(()))
        }
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.poll_flush(cx)
    }
}

fn setup(script: Script, count: usize) -> (Leg, ForwardWrite<PhysicalSink>, Arc<Mutex<Trace>>) {
    let leg = Leg::new(ACCOUNT);
    let trace = Arc::new(Mutex::new(Trace::default()));
    let sink = PhysicalSink {
        script,
        trace: trace.clone(),
    };
    let write = ForwardWrite::new(
        sink,
        leg.clone(),
        Some(KEY.to_owned()),
        Some(MODEL.to_owned()),
        NonZeroUsize::new(count).unwrap(),
    );
    (leg, write, trace)
}
fn frame(index: usize, count: usize) -> Message {
    Message::Frame(Frame::message(
        vec![b'x'],
        OpCode::Data(if index == 0 {
            Data::Text
        } else {
            Data::Continue
        }),
        index + 1 == count,
    ))
}
fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = noop_waker();
    future.poll(&mut Context::from_waker(&waker))
}
async fn drive(
    mut write: ForwardWrite<PhysicalSink>,
    count: usize,
) -> (PhysicalSink, Option<Sent>) {
    for index in 0..count {
        if write.send(frame(index, count)).await.is_err() {
            return write.finish(false);
        }
    }
    // Only the whole write's success reaches this call. No await between
    // observing completion and consuming the ticket.
    write.finish(true)
}
fn assert_burned(leg: &Leg) {
    assert!(leg.quota_candidate().is_none());
    assert!(
        leg.start(Some(KEY), Some(MODEL)).is_none(),
        "partial write must not revive"
    );
}
fn assert_untouched(leg: &Leg) {
    let (_, ticket) = leg
        .start(Some(KEY), Some(MODEL))
        .expect("no send means no witness/taint");
    ticket.flush();
    assert!(leg.quota_candidate().is_some());
}

#[test]
fn ready_pending_then_cancel_before_send_leaves_leg_untouched() {
    let (leg, write, trace) = setup(
        Script {
            ready_pending: Some(1),
            ..Script::default()
        },
        3,
    );
    let mut future = Box::pin(drive(write, 3));
    assert!(poll_once(future.as_mut()).is_pending());
    assert_eq!(trace.lock().unwrap().sends, 0);
    assert!(leg.quota_candidate().is_none());
    drop(future);
    assert_untouched(&leg);
}

#[test]
fn unpolled_future_or_ready_without_send_does_not_begin_witness() {
    let (leg, write, _) = setup(Script::default(), 1);
    drop(drive(write, 1));
    assert_untouched(&leg);

    let (leg, mut write, trace) = setup(Script::default(), 1);
    let waker = noop_waker();
    assert!(matches!(
        Pin::new(&mut write).poll_ready(&mut Context::from_waker(&waker)),
        Poll::Ready(Ok(()))
    ));
    assert!(!write.started);
    assert_eq!(trace.lock().unwrap().sends, 0);
    drop(write);
    assert_untouched(&leg);
}

#[test]
fn successful_multi_fragment_write_has_one_witness_and_original_context() {
    let (leg, write, trace) = setup(Script::default(), 3);
    let mut future = Box::pin(drive(write, 3));
    let sent = match poll_once(future.as_mut()) {
        Poll::Ready((_, Some(sent))) => sent,
        _ => panic!("all fragments should succeed synchronously"),
    };
    let trace = trace.lock().unwrap();
    assert_eq!((trace.sends, trace.flushes), (3, 3));
    assert_eq!(trace.final_frames, vec![false, false, true]);
    // Starting per fragment would taint the same leg and deny this candidate.
    assert_eq!(
        leg.quota_candidate()
            .unwrap()
            .with_current(ACCOUNT, Some(KEY), Some(MODEL), || 7,),
        Some(7)
    );
    assert!(leg.quota_candidate().is_none());
    leg.retire(&sent);
    assert_untouched(&leg);
}

#[test]
fn failed_initial_send_or_later_ready_send_flush_burns() {
    for script in [
        Script {
            send_error: Some(1),
            ..Script::default()
        },
        Script {
            ready_error: Some(2),
            ..Script::default()
        },
        Script {
            send_error: Some(2),
            ..Script::default()
        },
        Script {
            flush_error: Some(2),
            ..Script::default()
        },
    ] {
        let (leg, write, _) = setup(script, 3);
        let mut future = Box::pin(drive(write, 3));
        assert!(matches!(
            poll_once(future.as_mut()),
            Poll::Ready((_, Some(_)))
        ));
        assert_burned(&leg);
    }
}

#[test]
fn dropped_future_after_initial_or_during_final_flush_cannot_authorize() {
    for script in [
        Script {
            ready_pending: Some(2),
            ..Script::default()
        },
        Script {
            flush_pending: Some(3),
            ..Script::default()
        },
    ] {
        let (leg, write, trace) = setup(script, 3);
        let mut future = Box::pin(drive(write, 3));
        assert!(poll_once(future.as_mut()).is_pending());
        assert!(trace.lock().unwrap().sends >= 1);
        assert!(leg.quota_candidate().is_none());
        drop(future);
        assert_burned(&leg);
    }
}

#[test]
fn intermediate_flush_and_premature_finish_true_are_insufficient() {
    let (leg, mut write, trace) = setup(Script::default(), 3);
    {
        let mut send = Box::pin(write.send(frame(0, 3)));
        assert!(matches!(poll_once(send.as_mut()), Poll::Ready(Ok(()))));
    }
    assert!(write.started && write.sent.is_some());
    assert_eq!(trace.lock().unwrap().flushes, 1);
    assert!(leg.quota_candidate().is_none());
    let (_, sent) = write.finish(true); // deliberately incorrect caller
    assert!(sent.is_some());
    assert_burned(&leg);
}

#[test]
fn final_physical_flush_without_whole_write_finish_still_cannot_authorize() {
    let (leg, mut write, _) = setup(Script::default(), 2);
    for index in 0..2 {
        let mut send = Box::pin(write.send(frame(index, 2)));
        assert!(matches!(poll_once(send.as_mut()), Poll::Ready(Ok(()))));
    }
    assert!(leg.quota_candidate().is_none());
    // Models cancellation in the pump's yield after the last fragment flush.
    drop(write);
    assert_burned(&leg);
}

#[test]
fn sink_error_cannot_be_overridden_by_finish_true() {
    let (leg, mut write, _) = setup(
        Script {
            flush_error: Some(1),
            ..Script::default()
        },
        1,
    );
    {
        let mut send = Box::pin(write.send(frame(0, 1)));
        assert!(matches!(
            poll_once(send.as_mut()),
            Poll::Ready(Err("flush"))
        ));
    }
    // Error burns immediately, before the adapter itself is dropped.
    assert_burned(&leg);
    write.finish(true);
    assert!(leg.quota_candidate().is_none());
}
