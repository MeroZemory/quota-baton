//! Pure ownership and serialized-parent proof tests. No native process or core
//! authority is simulated by an observer event; authorization callbacks are local.
#[path = "../src/native_resume/dispatcher.rs"]
mod dispatcher;
#[allow(dead_code)] // This isolated target exercises a subset of the production adapter.
#[path = "../src/native_resume/framing.rs"]
mod framing;
#[path = "../src/native_resume/goal.rs"]
mod goal;
#[path = "../src/native_resume/ingress.rs"]
mod ingress;
#[allow(dead_code)] // This isolated target exercises a subset of the production adapter.
#[path = "../src/native_resume/parent_dispatch.rs"]
mod parent_dispatch;

use goal::{Action, Controller, Delivery, NextEpisodeProof};
use ingress::{Frame, Publisher, Source};
use parent_dispatch::{Error, Parent};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

const PREFIX: &str = "qb-goal-00000000000000000000000000000000:";
const THREAD: &str = "thread-a";

fn namespace(n: usize) -> String {
    format!("qb-goal-{n:032x}:")
}
fn snapshot(status: &str) -> Value {
    json!({"threadId":THREAD,"objective":"original objective","createdAt":123,
        "updatedAt":456,"status":status})
}
fn notice(method: &str, params: Value) -> Value {
    json!({"method":method,"params":params})
}
fn turn(method: &str, thread: &str, id: &str, status: &str) -> Value {
    notice(
        method,
        json!({"threadId":thread,"turn":{"id":id,"status":status}}),
    )
}
fn echo(status: &str) -> Value {
    notice(
        "thread/goal/updated",
        json!({"threadId":THREAD,"goal":snapshot(status)}),
    )
}
fn reply(prefix: &str, serial: u8, status: &str) -> Value {
    json!({"id":format!("{prefix}{serial}"),"result":{"goal":snapshot(status)}})
}
fn ownership() -> Vec<(Source, Value)> {
    vec![
        (
            Source::Ui,
            json!({"id":1,"method":"thread/start","params":{}}),
        ),
        (
            Source::Native,
            json!({"id":1,"result":{"thread":{"id":THREAD}}}),
        ),
        (
            Source::Ui,
            json!({"id":2,"method":"thread/goal/set","params":{
            "threadId":THREAD,"objective":"original objective","origin":"user","status":"active"}}),
        ),
        (
            Source::Native,
            json!({"id":2,"result":{"goal":snapshot("active")}}),
        ),
    ]
}
fn stopped_frames() -> Vec<(Source, Value)> {
    vec![
        (Source::Native, echo("usageLimited")),
        (
            Source::Ui,
            json!({"id":3,"method":"thread/goal/get","params":{"threadId":THREAD}}),
        ),
        (
            Source::Native,
            json!({"id":3,"result":{"goal":snapshot("usageLimited")}}),
        ),
    ]
}
fn feed(c: &mut Controller, source: Source, value: Value) -> Action {
    let bytes = value.to_string().into_bytes();
    match source {
        Source::Ui => c.ui_frame(&bytes),
        Source::Native => c.native_frame(&bytes),
    }
    .unwrap()
}
fn pure(history: Vec<Value>) -> Controller {
    let mut c = Controller::new(PREFIX.into()).unwrap();
    for frame in history {
        feed(&mut c, Source::Native, frame);
    }
    for (source, frame) in ownership().into_iter().chain(stopped_frames()) {
        feed(&mut c, source, frame);
    }
    c
}
fn activate(c: &mut Controller, prefix: &str) {
    let ticket = c.stopped_ticket().expect("one stopped ticket");
    assert_eq!(ticket.thread_id(), THREAD);
    assert!(c.stopped_ticket().is_none());
    assert!(matches!(
        c.begin_final_read(ticket).unwrap(),
        Action::ToNative(_)
    ));
    assert!(matches!(
        feed(c, Source::Native, reply(prefix, 1, "usageLimited")),
        Action::ResumeNative(_)
    ));
    assert_eq!(c.delivery(), Delivery::UnknownNoRetry);
}
fn progress(c: &mut Controller, prefix: &str, id: &str) -> NextEpisodeProof {
    feed(c, Source::Native, reply(prefix, 2, "active"));
    feed(
        c,
        Source::Native,
        turn("turn/started", THREAD, id, "inProgress"),
    );
    feed(
        c,
        Source::Native,
        turn("turn/completed", THREAD, id, "completed"),
    );
    c.take_next_episode_proof()
        .unwrap()
        .expect("correlated progress")
}
fn vetoed(c: &mut Controller) {
    assert!(c.take_next_episode_proof().is_err());
    assert!(c.stopped_ticket().is_none());
    let raw = br#" {"method":"ordinary","params":{}} "#;
    assert!(matches!(c.ui_frame(raw).unwrap(), Action::ToNative(out) if out == raw.as_slice()));
}

#[test]
fn ack_before_between_or_after_turn_notifications_mints_only_one_proof() {
    for order in [[0, 1, 2], [1, 0, 2], [1, 2, 0]] {
        let mut c = pure(vec![]);
        activate(&mut c, PREFIX);
        // Genuine native active echo may arrive before the automatic RPC ACK.
        feed(&mut c, Source::Native, echo("active"));
        let frames = [
            reply(PREFIX, 2, "active"),
            turn("turn/started", THREAD, "fresh", "inProgress"),
            turn("turn/completed", THREAD, "fresh", "completed"),
        ];
        for (index, event) in order.into_iter().enumerate() {
            feed(&mut c, Source::Native, frames[event].clone());
            if index < 2 {
                assert!(matches!(c.take_next_episode_proof(), Ok(None)));
            }
        }
        let proof = c.take_next_episode_proof().unwrap().unwrap();
        assert!(matches!(c.take_next_episode_proof(), Ok(None)));
        feed(&mut c, Source::Native, frames[2].clone());
        feed(&mut c, Source::Native, frames[0].clone());
        assert!(matches!(c.take_next_episode_proof(), Ok(None)));
        c.rearm(proof, namespace(1)).unwrap();
        assert_eq!(c.delivery(), Delivery::NotSent);
        assert!(
            c.stopped_ticket().is_none(),
            "new stop must be independently observed"
        );
        for (source, frame) in stopped_frames() {
            feed(&mut c, source, frame);
        }
        activate(&mut c, &namespace(1));
    }
}

#[test]
fn preexisting_history_foreign_or_duplicate_started_ids_cannot_prove_progress() {
    let histories = vec![
        vec![turn("turn/started", THREAD, "old", "inProgress")],
        vec![turn("turn/completed", THREAD, "old", "completed")],
        vec![turn("turn/started", "foreign", "old", "inProgress")],
        vec![json!({"id":"history","result":{"thread":{"turns":[{"id":"old"}]}}})],
    ];
    for history in histories {
        let mut c = pure(history);
        activate(&mut c, PREFIX);
        feed(&mut c, Source::Native, reply(PREFIX, 2, "active"));
        feed(
            &mut c,
            Source::Native,
            turn("turn/started", THREAD, "old", "inProgress"),
        );
        feed(
            &mut c,
            Source::Native,
            turn("turn/completed", THREAD, "old", "completed"),
        );
        vetoed(&mut c);
    }
    let mut c = pure(vec![]);
    activate(&mut c, PREFIX);
    feed(
        &mut c,
        Source::Native,
        turn("turn/started", THREAD, "duplicate", "inProgress"),
    );
    feed(
        &mut c,
        Source::Native,
        turn("turn/started", THREAD, "duplicate", "inProgress"),
    );
    feed(&mut c, Source::Native, reply(PREFIX, 2, "active"));
    feed(
        &mut c,
        Source::Native,
        turn("turn/completed", THREAD, "duplicate", "completed"),
    );
    vetoed(&mut c);
}

#[test]
fn completion_without_matching_fresh_start_or_ack_is_only_pending() {
    let mut c = pure(vec![]);
    activate(&mut c, PREFIX);
    feed(&mut c, Source::Native, reply(PREFIX, 2, "active"));
    for frame in [
        turn("turn/completed", THREAD, "no-start", "completed"),
        turn("turn/started", "foreign", "foreign", "inProgress"),
        turn("turn/completed", "foreign", "foreign", "completed"),
        notice(
            "turn/completed",
            json!({"threadId":THREAD,"turn":{"status":"completed"}}),
        ),
    ] {
        feed(&mut c, Source::Native, frame);
        assert!(matches!(c.take_next_episode_proof(), Ok(None)));
    }
    let mut c = pure(vec![]);
    activate(&mut c, PREFIX);
    feed(
        &mut c,
        Source::Native,
        turn("turn/started", THREAD, "fresh", "inProgress"),
    );
    feed(
        &mut c,
        Source::Native,
        turn("turn/completed", THREAD, "fresh", "completed"),
    );
    assert!(
        matches!(c.take_next_episode_proof(), Ok(None)),
        "missing ACK"
    );
}

#[test]
fn foreign_failed_interrupted_or_error_completion_is_terminal() {
    for (thread, status, error) in [
        ("foreign", "completed", Value::Null),
        (THREAD, "failed", Value::Null),
        (THREAD, "interrupted", Value::Null),
        (THREAD, "completed", json!({"message":"synthetic failure"})),
    ] {
        let mut c = pure(vec![]);
        activate(&mut c, PREFIX);
        feed(
            &mut c,
            Source::Native,
            turn("turn/started", THREAD, "fresh", "inProgress"),
        );
        let mut completed = turn("turn/completed", thread, "fresh", status);
        completed["params"]["turn"]["error"] = error;
        feed(&mut c, Source::Native, completed);
        feed(&mut c, Source::Native, reply(PREFIX, 2, "active"));
        feed(
            &mut c,
            Source::Native,
            turn("turn/completed", THREAD, "fresh", "completed"),
        );
        vetoed(&mut c);
    }
}

#[test]
fn ack_must_match_original_goal_identity_and_active_status() {
    for field in ["threadId", "objective", "createdAt", "status"] {
        let mut c = pure(vec![]);
        activate(&mut c, PREFIX);
        feed(
            &mut c,
            Source::Native,
            turn("turn/started", THREAD, "fresh", "inProgress"),
        );
        feed(
            &mut c,
            Source::Native,
            turn("turn/completed", THREAD, "fresh", "completed"),
        );
        let mut ack = reply(PREFIX, 2, "active");
        ack["result"]["goal"][field] = json!("changed");
        feed(&mut c, Source::Native, ack);
        assert_eq!(c.delivery(), Delivery::Rejected);
        feed(&mut c, Source::Native, reply(PREFIX, 2, "active"));
        vetoed(&mut c);
    }
}

#[test]
fn pause_clear_archive_foreground_switch_or_any_ui_frame_vetoes_progress() {
    for (source, frame) in [
        (Source::Native, echo("paused")),
        (Source::Native, echo("completed")),
        (Source::Native, echo("usageLimited")),
        (
            Source::Native,
            notice("thread/goal/cleared", json!({"threadId":THREAD})),
        ),
        (
            Source::Native,
            notice("thread/archived", json!({"threadId":THREAD})),
        ),
        (
            Source::Ui,
            json!({"id":8,"method":"thread/resume","params":{"threadId":"foreign"}}),
        ),
        (
            Source::Ui,
            json!({"id":8,"method":"account/read","params":{}}),
        ),
        (
            Source::Ui,
            json!({"id":"approval","result":{"decision":"decline"}}),
        ),
    ] {
        let mut c = pure(vec![]);
        activate(&mut c, PREFIX);
        feed(
            &mut c,
            Source::Native,
            turn("turn/started", THREAD, "fresh", "inProgress"),
        );
        feed(&mut c, source, frame);
        feed(&mut c, Source::Native, reply(PREFIX, 2, "active"));
        feed(
            &mut c,
            Source::Native,
            turn("turn/completed", THREAD, "fresh", "completed"),
        );
        vetoed(&mut c);
    }
}

#[test]
fn turn_bounds_disable_only_automatic_work_and_retired_bound_is_exactly_64() {
    assert_eq!(goal::MAX_RETIRED_NAMESPACES, 64);
    let mut c = Controller::new(PREFIX.into()).unwrap();
    for id in 0..=goal::MAX_SEEN_TURNS {
        feed(
            &mut c,
            Source::Native,
            turn(
                "turn/completed",
                "foreign",
                &format!("old-{id}"),
                "completed",
            ),
        );
    }
    vetoed(&mut c);
    let mut c = pure(vec![]);
    activate(&mut c, PREFIX);
    feed(
        &mut c,
        Source::Native,
        turn(
            "turn/started",
            THREAD,
            &"x".repeat(goal::MAX_TURN_ID_BYTES + 1),
            "inProgress",
        ),
    );
    vetoed(&mut c);

    let mut c = pure(vec![]);
    for episode in 0..goal::MAX_RETIRED_NAMESPACES {
        activate(&mut c, &namespace(episode));
        let proof = progress(&mut c, &namespace(episode), &format!("turn-{episode}"));
        c.rearm(proof, namespace(episode + 1)).unwrap();
        for (source, frame) in stopped_frames() {
            feed(&mut c, source, frame);
        }
    }
    activate(&mut c, &namespace(64));
    feed(&mut c, Source::Native, reply(&namespace(64), 2, "active"));
    feed(
        &mut c,
        Source::Native,
        turn("turn/started", THREAD, "last", "inProgress"),
    );
    feed(
        &mut c,
        Source::Native,
        turn("turn/completed", THREAD, "last", "completed"),
    );
    assert!(matches!(
        c.take_next_episode_proof(),
        Err(goal::Error::RetiredNamespaceBound)
    ));
    vetoed(&mut c);
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    flushes: usize,
    fail: bool,
}
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Output>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut output = self.0.lock().unwrap();
        if output.fail {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        output.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flushes += 1;
        Ok(())
    }
}
struct Fixture {
    parent: Parent<Sink, Sink>,
    publisher: Publisher,
    native: Sink,
    ui: Sink,
}
impl Fixture {
    fn new() -> Self {
        let native = Sink::default();
        let ui = Sink::default();
        let (parent, publisher) = Parent::new(PREFIX.into(), native.clone(), ui.clone()).unwrap();
        let mut f = Self {
            parent,
            publisher,
            native,
            ui,
        };
        for (source, frame) in ownership().into_iter().chain(stopped_frames()) {
            f.send(source, frame);
        }
        f
    }
    fn publish(&self, source: Source, frame: Value) {
        self.publisher
            .publish(source, Frame::new(frame.to_string().into_bytes()).unwrap())
            .unwrap();
    }
    fn send(&mut self, source: Source, frame: Value) {
        self.publish(source, frame);
        assert_eq!(
            self.parent.process_next(|| panic!("ordinary frame")),
            Ok(true)
        );
    }
    fn final_read(&mut self) {
        let ticket = self.parent.stopped_ticket().unwrap();
        self.parent.begin_final_read(ticket).unwrap();
    }
    fn activate(&mut self, prefix: &str) {
        self.final_read();
        self.publish(Source::Native, reply(prefix, 1, "usageLimited"));
        let flushes = self.native.0.lock().unwrap().flushes;
        assert_eq!(self.parent.process_next(|| true), Ok(true));
        assert_eq!(self.native.0.lock().unwrap().flushes, flushes + 1);
        assert_eq!(self.parent.delivery(), Delivery::UnknownNoRetry);
    }
    fn proof(&mut self) -> NextEpisodeProof {
        self.activate(PREFIX);
        self.send(Source::Native, echo("active"));
        self.send(
            Source::Native,
            turn("turn/started", THREAD, "fresh", "inProgress"),
        );
        self.send(
            Source::Native,
            turn("turn/completed", THREAD, "fresh", "completed"),
        );
        assert!(matches!(self.parent.take_next_episode_proof(), Ok(None)));
        self.send(Source::Native, reply(PREFIX, 2, "active"));
        self.parent.take_next_episode_proof().unwrap().unwrap()
    }
    fn denied(&mut self) {
        assert!(matches!(
            self.parent.take_next_episode_proof(),
            Err(Error::Dispatch(dispatcher::Error::AuthorityDenied))
        ));
    }
}

#[test]
fn parent_rearms_same_writer_and_ingress_and_suppresses_old_private_replies() {
    let mut f = Fixture::new();
    let proof = f.proof();
    assert!(matches!(f.parent.take_next_episode_proof(), Ok(None)));
    f.parent.rearm(proof, namespace(1)).unwrap();
    let before = f.ui.0.lock().unwrap().bytes.len();
    for serial in [1, 2, 1, 2] {
        f.send(Source::Native, reply(PREFIX, serial, "active"));
    }
    assert_eq!(f.ui.0.lock().unwrap().bytes.len(), before);
    assert_eq!(f.parent.delivery(), Delivery::NotSent);
    for (source, frame) in stopped_frames() {
        f.send(source, frame);
    }
    f.activate(&namespace(1));
    let output = f.native.0.lock().unwrap();
    let automatic: Vec<Value> = output
        .bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .filter(|value| value["params"]["origin"] == "automatic")
        .collect();
    assert_eq!(automatic.len(), 2);
    assert_eq!(automatic[0]["id"], format!("{PREFIX}2"));
    assert_eq!(automatic[1]["id"], format!("{}2", namespace(1)));
}

#[test]
fn parent_pending_ui_at_rearm_consumes_proof_and_keeps_input_forwarding() {
    let mut f = Fixture::new();
    let proof = f.proof();
    let user = json!({"id":9,"method":"thread/goal/set","params":{
        "threadId":THREAD,"status":"paused","origin":"user"}});
    f.publish(Source::Ui, user.clone());
    assert_eq!(
        f.parent.rearm(proof, namespace(1)),
        Err(Error::Dispatch(dispatcher::Error::AuthorityDenied))
    );
    f.denied();
    assert_eq!(
        f.parent.process_next(|| panic!("pending user input")),
        Ok(true)
    );
    assert!(
        f.native
            .0
            .lock()
            .unwrap()
            .bytes
            .ends_with(&[user.to_string().as_bytes(), b"\n"].concat())
    );
    f.send(
        Source::Native,
        json!({"id":9,"result":{"goal":snapshot("paused")}}),
    );
    f.denied();
}

#[test]
fn parent_processed_ui_or_reused_namespace_invalidates_taken_proof() {
    for processed_input in [false, true] {
        let mut f = Fixture::new();
        let proof = f.proof();
        if processed_input {
            f.send(
                Source::Ui,
                json!({"id":9,"method":"account/read","params":{}}),
            );
        }
        let next = if processed_input {
            namespace(1)
        } else {
            PREFIX.into()
        };
        assert_eq!(
            f.parent.rearm(proof, next),
            Err(Error::Dispatch(dispatcher::Error::AuthorityDenied))
        );
        f.denied();
        f.send(Source::Ui, json!({"method":"ordinary","params":{}}));
    }
}

#[test]
fn retired_unknown_ids_method_collisions_and_ui_ids_poison_transport() {
    for (source, frame) in [
        (
            Source::Native,
            json!({"id":format!("{PREFIX}unknown"),"result":{}}),
        ),
        (
            Source::Native,
            json!({"id":format!("{PREFIX}1"),"method":"request","params":{}}),
        ),
        (
            Source::Ui,
            json!({"id":format!("{PREFIX}2"),"method":"account/read","params":{}}),
        ),
    ] {
        let mut f = Fixture::new();
        let proof = f.proof();
        f.parent.rearm(proof, namespace(1)).unwrap();
        f.publish(source, frame);
        assert_eq!(
            f.parent.process_next(|| panic!("collision")),
            Err(Error::Dispatch(dispatcher::Error::Protocol))
        );
        assert!(
            f.publisher
                .publish(Source::Ui, Frame::new(b"{}".to_vec()).unwrap())
                .is_err()
        );
    }
}

#[test]
fn parent_denial_rejection_and_uncertain_write_never_allow_rearm() {
    for mode in 0..3 {
        let mut f = Fixture::new();
        f.final_read();
        if mode == 2 {
            f.native.0.lock().unwrap().fail = true;
        }
        f.publish(Source::Native, reply(PREFIX, 1, "usageLimited"));
        let result = f.parent.process_next(|| mode != 0);
        if mode == 2 {
            assert_eq!(
                result,
                Err(Error::Dispatch(dispatcher::Error::WriteUncertain))
            );
            assert!(f.parent.take_next_episode_proof().is_err());
            continue;
        }
        if mode == 0 {
            assert_eq!(
                result,
                Err(Error::Dispatch(dispatcher::Error::AuthorityDenied))
            );
        } else {
            assert_eq!(result, Ok(true));
            f.send(
                Source::Native,
                json!({"id":format!("{PREFIX}2"),"error":{"code":-1}}),
            );
        }
        f.send(Source::Native, reply(PREFIX, 2, "active"));
        f.send(
            Source::Native,
            turn("turn/started", THREAD, "fresh", "inProgress"),
        );
        f.send(
            Source::Native,
            turn("turn/completed", THREAD, "fresh", "completed"),
        );
        f.denied();
        f.send(Source::Ui, json!({"method":"ordinary","params":{}}));
    }
}
