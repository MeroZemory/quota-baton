//! Staged local ordering tests; no runtime, native process, or core authority.
#[path = "../src/native_resume/ingress.rs"]
mod ingress;

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::Duration,
};

use ingress::{CommitError, Error, Frame, QUEUE_FRAMES, Source, channel};

const BUDGET: Duration = Duration::from_secs(3);

fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic operation failed"),
    }
}

fn receive<T>(receiver: &Receiver<T>) -> T {
    require(receiver.recv_timeout(BUDGET))
}

fn frame(bytes: &[u8]) -> Frame {
    require(Frame::new(bytes.to_vec()))
}

#[test]
fn exact_bytes_fifo_and_checked_out_event_stays_pending_until_ack() {
    let (mut owner, publisher) = channel();
    let raw = br#"{ "id":9, "params":{"operand":"cancel resume"} }"#;
    require(publisher.publish(Source::Ui, frame(raw)));
    require(publisher.publish(Source::Native, frame(b"{}")));
    assert_eq!(require(owner.backlog()), 2);
    let first = require(owner.next_event()).expect("first event");
    assert_eq!(first.source(), Source::Ui);
    assert_eq!(first.frame().as_bytes(), raw);
    assert!(matches!(owner.next_event(), Err(Error::Pending)));
    assert!(matches!(owner.checkpoint(), Err(Error::Pending)));
    assert_eq!(require(owner.backlog()), 2);
    require(owner.ack_processed(first));
    let second = require(owner.next_event()).expect("second event");
    assert_eq!(second.source(), Source::Native);
    require(owner.ack_processed(second));
    assert!(require(owner.next_event()).is_none());
    let checkpoint = require(owner.checkpoint());
    assert_eq!(
        require(owner.with_current(checkpoint, || Ok::<_, ()>(7))),
        7
    );
}

#[test]
fn concurrent_admission_between_checkpoint_and_commit_vetoes_even_after_ack() {
    let (mut owner, publisher) = channel();
    let checkpoint = require(owner.checkpoint());
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = publisher.publish(Source::Ui, frame(br#"{"id":1,"method":"cancel"}"#));
        let _ = done_tx.send(result);
    });
    require(receive(&done_rx));
    require(worker.join());
    let event = require(owner.next_event()).expect("admitted UI event");
    require(owner.ack_processed(event));
    assert_eq!(require(owner.backlog()), 0);
    assert!(matches!(
        owner.with_current(checkpoint, || -> Result<(), ()> {
            panic!("stale checkpoint must not write")
        }),
        Err(CommitError::Ingress(Error::Stale))
    ));
    // A local veto burns the candidate, not the user's ordinary transport.
    require(fresh_checkpoint_after_veto(&mut owner));
}

fn fresh_checkpoint_after_veto(owner: &mut ingress::Owner) -> Result<(), Error> {
    let checkpoint = owner.checkpoint()?;
    match owner.with_current(checkpoint, || Ok::<_, ()>(())) {
        Ok(()) => Ok(()),
        Err(CommitError::Ingress(error)) => Err(error),
        Err(CommitError::Write(())) => panic!("infallible synthetic write"),
    }
}

#[test]
fn concurrent_close_between_checkpoint_and_commit_vetoes_without_input() {
    let (mut owner, publisher) = channel();
    let checkpoint = require(owner.checkpoint());
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        publisher.close();
        let _ = done_tx.send(());
    });
    receive(&done_rx);
    require(worker.join());
    assert!(matches!(
        owner.with_current(checkpoint, || -> Result<(), ()> {
            panic!("closed owner must not write")
        }),
        Err(CommitError::Ingress(Error::Revoked))
    ));
}

#[test]
fn writer_wins_gate_and_holds_it_for_whole_frame_before_admission() {
    let (mut owner, publisher) = channel();
    let checkpoint = require(owner.checkpoint());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (writer_done_tx, writer_done_rx) = mpsc::channel();
    let completed = Arc::new(AtomicBool::new(false));
    let writer_completed = completed.clone();
    let writer = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = owner.with_current(checkpoint, || {
            bytes.extend_from_slice(b"{}");
            let _ = entered_tx.send(());
            release_rx.recv_timeout(BUDGET).map_err(|_| ())?;
            bytes.push(b'\n');
            // This marker stands for completion of the physical frame + flush.
            writer_completed.store(true, Ordering::SeqCst);
            Ok::<_, ()>(())
        });
        let _ = writer_done_tx.send((owner, result, bytes));
    });
    receive(&entered_rx);
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let publisher_worker = thread::spawn(move || {
        let _ = attempt_tx.send(());
        let result = publisher.publish(Source::Native, frame(b"{}"));
        let after_whole_write = completed.load(Ordering::SeqCst);
        let _ = admitted_tx.send((result, after_whole_write));
    });
    receive(&attempt_rx);
    assert!(matches!(
        admitted_rx.recv_timeout(Duration::from_millis(30)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    require(release_tx.send(()));
    let (mut owner, result, bytes) = receive(&writer_done_rx);
    require(result);
    assert_eq!(bytes, b"{}\n");
    let (result, after_whole_write) = receive(&admitted_rx);
    require(result);
    assert!(after_whole_write);
    require(writer.join());
    require(publisher_worker.join());
    let event = require(owner.next_event()).expect("native event after write");
    require(owner.ack_processed(event));
}

#[test]
fn queue_bound_counts_checked_out_receipt_and_overflow_latches_revocation() {
    let (mut owner, publisher) = channel();
    require(publisher.publish(Source::Ui, frame(b"{}")));
    let first = require(owner.next_event()).expect("first event");
    for index in 1..QUEUE_FRAMES {
        let source = if index % 2 == 0 {
            Source::Ui
        } else {
            Source::Native
        };
        require(publisher.publish(source, frame(b"{}")));
    }
    assert_eq!(require(owner.backlog()), QUEUE_FRAMES);
    assert_eq!(
        publisher.publish(Source::Ui, frame(b"{}")),
        Err(Error::QueueFull)
    );
    assert!(matches!(owner.ack_processed(first), Err(Error::Revoked)));
    assert!(matches!(owner.next_event(), Err(Error::Revoked)));
    assert!(matches!(owner.checkpoint(), Err(Error::Revoked)));
    assert_eq!(
        publisher.publish(Source::Native, frame(b"{}")),
        Err(Error::Revoked)
    );
}

#[test]
fn checkpoint_is_not_aliased_across_owners_or_commits() {
    let (mut first, _) = channel();
    let (mut second, second_publisher) = channel();
    let foreign = require(first.checkpoint());
    assert!(matches!(
        second.with_current(foreign, || -> Result<(), ()> { panic!("foreign write") }),
        Err(CommitError::Ingress(Error::WrongOwner))
    ));
    assert_eq!(
        second_publisher.publish(Source::Ui, frame(b"{}")),
        Err(Error::Revoked)
    );
    let current = require(first.checkpoint());
    let alias = require(first.checkpoint());
    require(first.with_current(current, || Ok::<_, ()>(())));
    assert!(matches!(
        first.with_current(alias, || -> Result<(), ()> { panic!("old alias write") }),
        Err(CommitError::Ingress(Error::Stale))
    ));
}

#[test]
fn wrong_owner_ack_revokes_and_abandoned_receipt_never_clears_backlog() {
    let (mut first, publisher) = channel();
    let (mut second, _) = channel();
    require(publisher.publish(Source::Ui, frame(b"{}")));
    let event = require(first.next_event()).expect("event");
    assert_eq!(second.ack_processed(event), Err(Error::WrongOwner));
    assert!(matches!(first.checkpoint(), Err(Error::Pending)));
    assert!(matches!(first.next_event(), Err(Error::Pending)));
    assert!(matches!(second.checkpoint(), Err(Error::Revoked)));
}

#[test]
fn eof_close_vetoes_checkpoint_discards_burst_and_never_reopens() {
    let (mut owner, publisher) = channel();
    let checkpoint = require(owner.checkpoint());
    require(publisher.publish(Source::Ui, frame(b"{}")));
    publisher.close();
    publisher.close();
    assert!(matches!(
        owner.with_current(checkpoint, || -> Result<(), ()> { panic!("closed write") }),
        Err(CommitError::Ingress(Error::Revoked))
    ));
    assert!(matches!(owner.next_event(), Err(Error::Revoked)));
    assert_eq!(
        publisher.publish(Source::Ui, frame(b"{}")),
        Err(Error::Revoked)
    );
}

#[test]
fn owner_drop_revokes_surviving_publisher_clones() {
    let (owner, publisher) = channel();
    let other_reader = publisher.clone();
    drop(owner);
    assert_eq!(
        publisher.publish(Source::Ui, frame(b"{}")),
        Err(Error::Revoked)
    );
    assert_eq!(
        other_reader.publish(Source::Native, frame(b"{}")),
        Err(Error::Revoked)
    );
}

#[test]
fn partial_write_error_revokes_without_retry() {
    let (mut owner, publisher) = channel();
    let checkpoint = require(owner.checkpoint());
    let mut bytes = Vec::new();
    assert!(matches!(
        owner.with_current(checkpoint, || {
            bytes.push(b'{');
            Err::<(), _>("synthetic-write-error")
        }),
        Err(CommitError::Write("synthetic-write-error"))
    ));
    assert_eq!(bytes, b"{");
    assert!(matches!(owner.checkpoint(), Err(Error::Revoked)));
    assert_eq!(
        publisher.publish(Source::Ui, frame(b"{}")),
        Err(Error::Revoked)
    );
}

#[test]
fn panicking_writer_poison_is_latched_and_cannot_be_recovered_live() {
    let (mut owner, publisher) = channel();
    let checkpoint = require(owner.checkpoint());
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let _ = owner.with_current(checkpoint, || -> Result<(), ()> {
                panic!("synthetic writer panic")
            });
        }))
        .is_err()
    );
    assert_eq!(
        publisher.publish(Source::Native, frame(b"{}")),
        Err(Error::Poisoned)
    );
    assert!(matches!(owner.checkpoint(), Err(Error::Poisoned)));
    publisher.close();
    assert_eq!(
        publisher.publish(Source::Ui, frame(b"{}")),
        Err(Error::Poisoned)
    );
}

#[test]
fn malformed_and_oversized_validation_has_fixed_errors_no_payload_formatting() {
    for raw in [b"".as_slice(), b"[]", b"{", b"{}\n", b"{}\r", b"\xff"] {
        assert!(matches!(Frame::new(raw.to_vec()), Err(Error::InvalidFrame)));
    }
    assert!(matches!(
        Frame::new(vec![b' '; ingress::MAX_FRAME_BYTES + 1]),
        Err(Error::FrameBound)
    ));
    let (mut owner, publisher) = channel();
    // Reader refuses malformed input before admission, then closes explicitly.
    if Frame::new(b"not-json".to_vec()).is_err() {
        publisher.close();
    }
    assert!(matches!(owner.checkpoint(), Err(Error::Revoked)));
}
