//! Composed local ordering only: authorization callbacks are synthetic, not core permits.
#[allow(dead_code)] // This isolated target exercises a subset of the production adapter.
#[path = "../src/native_resume/dispatcher.rs"]
mod dispatcher;
#[path = "../src/native_resume/framing.rs"]
mod framing;
#[allow(dead_code)] // This isolated target exercises a subset of the production adapter.
#[path = "../src/native_resume/goal.rs"]
mod goal;
#[path = "../src/native_resume/ingress.rs"]
mod ingress;
#[allow(dead_code)] // This isolated target exercises a subset of the production adapter.
#[path = "../src/native_resume/parent_dispatch.rs"]
mod parent_dispatch;

use ingress::{Frame, Publisher, Source};
use parent_dispatch::{Error, Parent};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

const PREFIX: &str = "qb-goal-0123456789abcdef0123456789abcdef:";
const THREAD: &str = "11111111-2222-4333-8444-555555555555";
const BOUND: Duration = Duration::from_secs(3);
#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    fail_after: Option<usize>,
    flushes: usize,
}
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Output>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut output = self.0.lock().unwrap();
        let remaining = output
            .fail_after
            .unwrap_or(usize::MAX)
            .saturating_sub(output.bytes.len());
        if remaining == 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let n = remaining.min(bytes.len());
        output.bytes.extend_from_slice(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flushes += 1;
        Ok(())
    }
}
fn goal(status: &str) -> Value {
    json!({"threadId":THREAD,"objective":"synthetic objective","createdAt":1,"updatedAt":2,"status":status})
}
fn publish(publisher: &Publisher, source: Source, value: Value) {
    assert!(
        publisher
            .publish(source, Frame::new(value.to_string().into_bytes()).unwrap())
            .is_ok()
    );
}
fn ordinary(parent: &mut Parent<Sink, Sink>, publisher: &Publisher, source: Source, value: Value) {
    publish(publisher, source, value);
    assert_eq!(
        parent.process_next(|| panic!("ordinary input must never authorize")),
        Ok(true)
    );
}
fn stopped_candidate() -> (
    Parent<Sink, Sink>,
    Publisher,
    Sink,
    Sink,
    goal::StoppedGoalTicket,
) {
    let n = Sink::default();
    let u = Sink::default();
    let (mut parent, publisher) = Parent::new(PREFIX.into(), n.clone(), u.clone()).unwrap();
    ordinary(
        &mut parent,
        &publisher,
        Source::Ui,
        json!({"id":1,"method":"thread/start","params":{}}),
    );
    ordinary(
        &mut parent,
        &publisher,
        Source::Native,
        json!({"id":1,"result":{"thread":{"id":THREAD}}}),
    );
    ordinary(
        &mut parent,
        &publisher,
        Source::Ui,
        json!({"id":2,"method":"thread/goal/set","params":{
        "threadId":THREAD,"objective":"synthetic objective","origin":"user","status":"active"}}),
    );
    ordinary(
        &mut parent,
        &publisher,
        Source::Native,
        json!({"id":2,"result":{"goal":goal("active")}}),
    );
    ordinary(
        &mut parent,
        &publisher,
        Source::Native,
        json!({"method":"thread/goal/updated","params":{
        "threadId":THREAD,"goal":goal("usageLimited")}}),
    );
    ordinary(
        &mut parent,
        &publisher,
        Source::Ui,
        json!({"id":3,"method":"thread/goal/get","params":{"threadId":THREAD}}),
    );
    ordinary(
        &mut parent,
        &publisher,
        Source::Native,
        json!({"id":3,"result":{"goal":goal("usageLimited")}}),
    );
    let ticket = parent.stopped_ticket().unwrap();
    assert_eq!(ticket.thread_id(), THREAD);
    (parent, publisher, n, u, ticket)
}
fn stopped() -> (Parent<Sink, Sink>, Publisher, Sink, Sink) {
    let (mut parent, publisher, n, u, ticket) = stopped_candidate();
    parent.begin_final_read(ticket).unwrap();
    (parent, publisher, n, u)
}
fn final_response() -> Value {
    json!({"id":format!("{PREFIX}1"),"result":{"goal":goal("usageLimited")}})
}
fn automatic(n: &Sink) -> Vec<Value> {
    n.0.lock()
        .unwrap()
        .bytes
        .split(|b| *b == b'\n')
        .filter(|b| !b.is_empty())
        .map(|b| serde_json::from_slice::<Value>(b).unwrap())
        .filter(|value| value["params"]["origin"] == "automatic")
        .collect()
}
#[test]
fn parent_positive_one_same_uuid_status_only_command_complete_and_flushed() {
    let (mut parent, publisher, n, u) = stopped();
    let before_ui = u.0.lock().unwrap().bytes.len();
    let before_flushes = n.0.lock().unwrap().flushes;
    publish(&publisher, Source::Native, final_response());
    let mut calls = 0;
    assert_eq!(
        parent.process_next(|| {
            calls += 1;
            true
        }),
        Ok(true)
    );
    assert_eq!(calls, 1);
    assert_eq!(n.0.lock().unwrap().flushes, before_flushes + 1);
    assert_eq!(u.0.lock().unwrap().bytes.len(), before_ui);
    assert_eq!(automatic(&n).len(), 1);
    assert_eq!(
        automatic(&n)[0]["params"],
        json!({"threadId":THREAD,"status":"active","origin":"automatic"})
    );
    assert_eq!(parent.delivery(), goal::Delivery::UnknownNoRetry);
    ordinary(&mut parent, &publisher, Source::Native, final_response());
    assert_eq!(automatic(&n).len(), 1);
    ordinary(
        &mut parent,
        &publisher,
        Source::Native,
        json!({"id":format!("{PREFIX}2"),"result":{"goal":goal("active")}}),
    );
    assert_eq!(parent.delivery(), goal::Delivery::Acknowledged);
    assert!(parent.stopped_ticket().is_none());
}
#[test]
fn parent_already_queued_ui_behind_final_reply_vetoes_without_consuming() {
    let (mut parent, publisher, n, _) = stopped();
    publish(&publisher, Source::Native, final_response());
    let user = json!({"id":9,"method":"account/read","params":{}});
    publish(&publisher, Source::Ui, user.clone());
    assert_eq!(
        parent.process_next(|| panic!("known backlog must not Consume")),
        Err(Error::Dispatch(dispatcher::Error::AuthorityDenied))
    );
    assert!(automatic(&n).is_empty());
    assert_eq!(parent.process_next(|| panic!("ordinary frame")), Ok(true));
    assert!(
        n.0.lock()
            .unwrap()
            .bytes
            .ends_with(&[user.to_string().as_bytes(), b"\n"].concat())
    );
    ordinary(&mut parent, &publisher, Source::Native, final_response());
    assert!(automatic(&n).is_empty());
}
#[test]
fn parent_ui_admitted_during_consume_vetoes_then_forwards_ordinary_input() {
    let (mut parent, publisher, n, _) = stopped();
    publish(&publisher, Source::Native, final_response());
    let other = publisher.clone();
    let user = json!({"id":9,"method":"account/read","params":{}});
    let worker_user = user.clone();
    let (start_tx, start_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        start_rx.recv_timeout(BOUND).unwrap();
        publish(&other, Source::Ui, worker_user);
        done_tx.send(()).unwrap();
    });
    let mut calls = 0;
    let result = parent.process_next(|| {
        calls += 1;
        start_tx.send(()).unwrap();
        done_rx.recv_timeout(BOUND).unwrap();
        true
    });
    worker.join().unwrap();
    assert_eq!(calls, 1);
    assert_eq!(
        result,
        Err(Error::Dispatch(dispatcher::Error::AuthorityDenied))
    );
    assert!(automatic(&n).is_empty());
    assert_eq!(parent.process_next(|| panic!("ordinary input")), Ok(true));
    assert!(
        n.0.lock()
            .unwrap()
            .bytes
            .ends_with(&[user.to_string().as_bytes(), b"\n"].concat())
    );
    ordinary(&mut parent, &publisher, Source::Native, final_response());
    assert!(automatic(&n).is_empty());
}
#[test]
fn parent_denial_preserves_tui_and_burns_candidate() {
    let (mut parent, publisher, n, _) = stopped();
    publish(&publisher, Source::Native, final_response());
    assert_eq!(
        parent.process_next(|| false),
        Err(Error::Dispatch(dispatcher::Error::AuthorityDenied))
    );
    assert!(automatic(&n).is_empty());
    ordinary(
        &mut parent,
        &publisher,
        Source::Ui,
        json!({"id":8,"method":"account/read","params":{}}),
    );
    ordinary(&mut parent, &publisher, Source::Native, final_response());
    assert!(automatic(&n).is_empty());
}
#[test]
fn parent_partial_activation_write_closes_and_never_retries() {
    let (mut parent, publisher, n, _) = stopped();
    let before = n.0.lock().unwrap().bytes.len();
    n.0.lock().unwrap().fail_after = Some(before + 5);
    publish(&publisher, Source::Native, final_response());
    assert_eq!(
        parent.process_next(|| true),
        Err(Error::Dispatch(dispatcher::Error::WriteUncertain))
    );
    assert_eq!(n.0.lock().unwrap().bytes.len(), before + 5);
    assert_eq!(parent.delivery(), goal::Delivery::UnknownNoRetry);
    assert!(matches!(
        parent.process_next(|| panic!("no retry")),
        Err(Error::Ingress(ingress::Error::Revoked))
    ));
    assert_eq!(n.0.lock().unwrap().bytes.len(), before + 5);
}
#[test]
fn parent_close_or_owner_unwind_blocks_activation_and_surviving_publishers() {
    let (mut parent, publisher, n, _) = stopped();
    publish(&publisher, Source::Native, final_response());
    assert!(matches!(
        parent.process_next(|| {
            publisher.close();
            true
        }),
        Err(Error::Ingress(ingress::Error::Revoked))
    ));
    assert!(automatic(&n).is_empty());
    let (mut parent, publisher, n, _) = stopped();
    publish(&publisher, Source::Native, final_response());
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let _ = parent.process_next(|| panic!("synthetic authorization unwind"));
        }))
        .is_err()
    );
    assert!(automatic(&n).is_empty());
    assert_eq!(
        publisher.publish(Source::Ui, Frame::new(b"{}".to_vec()).unwrap()),
        Err(ingress::Error::Revoked)
    );
    drop(parent);
    assert_eq!(
        publisher.publish(Source::Native, Frame::new(b"{}".to_vec()).unwrap()),
        Err(ingress::Error::Revoked)
    );
}
#[test]
fn parent_explicit_cancel_and_disconnect_preserve_separate_lifetimes() {
    let (mut parent, publisher, n, _) = stopped();
    parent.cancel_resume();
    ordinary(&mut parent, &publisher, Source::Native, final_response());
    assert!(automatic(&n).is_empty());
    parent.disconnect();
    assert!(matches!(
        parent.process_next(|| false),
        Err(Error::Ingress(ingress::Error::Revoked))
    ));
}

#[test]
fn imported_ordinary_dispatch_and_framing_remain_non_authorizing() {
    let mut dispatcher =
        dispatcher::Dispatcher::new(PREFIX.into(), Sink::default(), Sink::default()).unwrap();
    let notice = br#"{"method":"server/notice","params":{}}"#;
    dispatcher.native_frame(notice).unwrap();
    dispatcher
        .native_frame_admitted(notice, || panic!("ordinary notice cannot authorize"))
        .unwrap();
    let mut input = std::io::Cursor::new(b"{}\n".as_slice());
    assert_eq!(
        framing::read_frame(&mut input).unwrap(),
        Some(b"{}".to_vec())
    );
}

#[test]
fn parent_stale_final_read_ticket_denies_auto_but_keeps_ordinary_transport() {
    for user_edit in [false, true] {
        let (mut parent, publisher, n, u, ticket) = stopped_candidate();
        if user_edit {
            ordinary(
                &mut parent,
                &publisher,
                Source::Ui,
                json!({
                    "id":4,"method":"thread/goal/set","params":{
                        "threadId":THREAD,"objective":"new synthetic objective",
                        "origin":"user","status":"active"
                    }
                }),
            );
        } else {
            parent.cancel_resume();
        }
        let before_native = n.0.lock().unwrap().bytes.len();
        assert_eq!(
            parent.begin_final_read(ticket),
            Err(Error::Dispatch(dispatcher::Error::AuthorityDenied)),
            "a stale native ticket must cancel only automatic continuation"
        );
        assert_eq!(n.0.lock().unwrap().bytes.len(), before_native);
        assert!(parent.stopped_ticket().is_none(), "a veto must never rearm");
        let before_ui = u.0.lock().unwrap().bytes.len();
        ordinary(
            &mut parent,
            &publisher,
            Source::Native,
            json!({"method":"server/notice","params":{"synthetic":true}}),
        );
        assert!(u.0.lock().unwrap().bytes.len() > before_ui);
        ordinary(
            &mut parent,
            &publisher,
            Source::Ui,
            json!({"id":5,"method":"account/read","params":{}}),
        );
        assert!(n.0.lock().unwrap().bytes.len() > before_native);
        assert!(automatic(&n).is_empty());
    }
}

#[test]
fn parent_invalid_native_protocol_still_revokes_ingress() {
    let (mut parent, publisher, _, _) = stopped();
    publish(&publisher, Source::Native, json!({"id":[],"result":{}}));
    assert_eq!(
        parent.process_next(|| panic!("invalid protocol cannot authorize")),
        Err(Error::Dispatch(dispatcher::Error::Protocol))
    );
    assert_eq!(
        publisher.publish(Source::Ui, Frame::new(b"{}".to_vec()).unwrap()),
        Err(ingress::Error::Revoked)
    );
}

#[test]
fn parent_input_queued_during_inspect_denies_final_read_without_losing_input() {
    let (mut parent, publisher, n, _, ticket) = stopped_candidate();
    let before_native = n.0.lock().unwrap().bytes.len();
    publish(
        &publisher,
        Source::Ui,
        json!({"id":4,"method":"account/read","params":{}}),
    );
    assert_eq!(
        parent.begin_final_read(ticket),
        Err(Error::Dispatch(dispatcher::Error::AuthorityDenied)),
        "known input must veto the final read without losing ordinary traffic"
    );
    assert_eq!(n.0.lock().unwrap().bytes.len(), before_native);
    assert_eq!(
        parent.process_next(|| panic!("queued user input cannot authorize")),
        Ok(true)
    );
    assert!(n.0.lock().unwrap().bytes.len() > before_native);
    assert!(parent.stopped_ticket().is_none());
    assert!(automatic(&n).is_empty());
}
