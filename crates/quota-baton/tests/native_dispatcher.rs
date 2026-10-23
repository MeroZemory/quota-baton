//! Coupled ownership and physical writer ordering; no reset authority fixture.
#[path = "../src/native_resume/dispatcher.rs"]
mod dispatcher;
#[path = "../src/native_resume/framing.rs"]
mod framing;
#[path = "../src/native_resume/goal.rs"]
mod goal;

use dispatcher::{Dispatcher, Error};
use goal::Delivery;
use serde_json::{Value, json};
use std::{
    io::{self, Cursor, Write},
    sync::{Arc, Mutex},
};

const PREFIX: &str = "qb-goal-0123456789abcdef0123456789abcdef:";
#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    fail_after: Option<usize>,
    calls: usize,
}
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Output>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut out = self.0.lock().unwrap();
        out.calls += 1;
        let remaining = out
            .fail_after
            .unwrap_or(usize::MAX)
            .saturating_sub(out.bytes.len());
        if remaining == 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let n = bytes.len().min(remaining);
        out.bytes.extend_from_slice(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn goal(status: &str) -> Value {
    json!({"threadId":"a","objective":"synthetic objective","createdAt":1,"updatedAt":2,"status":status})
}
fn ui(d: &mut Dispatcher<Sink, Sink>, value: Value) {
    d.ui_frame(value.to_string().as_bytes()).unwrap();
}
fn native(d: &mut Dispatcher<Sink, Sink>, value: Value) {
    d.native_frame(value.to_string().as_bytes()).unwrap();
}
fn stopped() -> (Dispatcher<Sink, Sink>, Sink, Sink) {
    let n = Sink::default();
    let u = Sink::default();
    let mut d = Dispatcher::new(PREFIX.into(), n.clone(), u.clone()).unwrap();
    ui(&mut d, json!({"id":1,"method":"thread/start","params":{}}));
    native(&mut d, json!({"id":1,"result":{"thread":{"id":"a"}}}));
    ui(
        &mut d,
        json!({"id":2,"method":"thread/goal/set","params":{
        "threadId":"a","objective":"synthetic objective","origin":"user","status":"active"}}),
    );
    native(&mut d, json!({"id":2,"result":{"goal":goal("active")}}));
    native(
        &mut d,
        json!({"method":"thread/goal/updated","params":{"threadId":"a","goal":goal("usageLimited")}}),
    );
    ui(
        &mut d,
        json!({"id":3,"method":"thread/goal/get","params":{"threadId":"a"}}),
    );
    native(
        &mut d,
        json!({"id":3,"result":{"goal":goal("usageLimited")}}),
    );
    (d, n, u)
}
fn final_response() -> Vec<u8> {
    json!({"id":format!("{PREFIX}1"),"result":{"goal":goal("usageLimited")}})
        .to_string()
        .into_bytes()
}

#[test]
fn actual_automatic_write_is_private_status_only_once_and_ack_is_suppressed() {
    let (mut d, n, u) = stopped();
    let ticket = d.stopped_ticket().unwrap();
    d.begin_final_read(ticket).unwrap();
    let before_ui = u.0.lock().unwrap().bytes.len();
    d.native_frame(&final_response()).unwrap();
    native(
        &mut d,
        json!({"id":format!("{PREFIX}2"),"result":{"goal":goal("active")}}),
    );
    assert_eq!(d.delivery(), Delivery::Acknowledged);
    assert_eq!(u.0.lock().unwrap().bytes.len(), before_ui);
    d.native_frame(&final_response()).unwrap();
    let bytes = n.0.lock().unwrap().bytes.clone();
    let frames: Vec<Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|v| !v.is_empty())
        .map(|v| serde_json::from_slice(v).unwrap())
        .collect();
    let automatic: Vec<_> = frames
        .iter()
        .filter(|v| v["params"]["origin"] == "automatic")
        .collect();
    assert_eq!(automatic.len(), 1);
    assert_eq!(
        automatic[0]["params"],
        json!({"threadId":"a","status":"active","origin":"automatic"})
    );
}

#[test]
fn partial_automatic_write_disconnects_and_cannot_emit_a_second_byte() {
    let (mut d, n, _) = stopped();
    let ticket = d.stopped_ticket().unwrap();
    d.begin_final_read(ticket).unwrap();
    {
        let mut out = n.0.lock().unwrap();
        out.fail_after = Some(out.bytes.len() + 5);
    }
    assert_eq!(
        d.native_frame(&final_response()),
        Err(Error::WriteUncertain)
    );
    assert_eq!(d.delivery(), Delivery::UnknownNoRetry);
    let calls = n.0.lock().unwrap().calls;
    assert_eq!(d.native_frame(&final_response()), Err(Error::Protocol));
    assert_eq!(
        d.ui_frame(br#"{"id":9,"method":"account/read"}"#),
        Err(Error::Protocol)
    );
    assert!(d.stopped_ticket().is_none());
    assert_eq!(n.0.lock().unwrap().calls, calls);
}

#[test]
fn ui_transport_loss_and_reader_truncation_cancel_before_resume() {
    let (mut d, n, u) = stopped();
    let ticket = d.stopped_ticket().unwrap();
    d.begin_final_read(ticket).unwrap();
    {
        let mut out = u.0.lock().unwrap();
        out.fail_after = Some(out.bytes.len());
    }
    assert_eq!(
        d.native_frame(br#"{"method":"server/notice","params":{}}"#),
        Err(Error::WriteUncertain)
    );
    let calls = n.0.lock().unwrap().calls;
    assert_eq!(d.native_frame(&final_response()), Err(Error::Protocol));
    assert_eq!(d.delivery(), Delivery::Cancelled);
    assert_eq!(n.0.lock().unwrap().calls, calls);

    let (mut d, n, _) = stopped();
    let ticket = d.stopped_ticket().unwrap();
    d.begin_final_read(ticket).unwrap();
    assert_eq!(
        framing::read_frame(&mut Cursor::new(b"{\"id\":")),
        Err(framing::Error::Truncated)
    );
    d.disconnect();
    let calls = n.0.lock().unwrap().calls;
    assert_eq!(d.native_frame(&final_response()), Err(Error::Protocol));
    assert_eq!(n.0.lock().unwrap().calls, calls);
}
