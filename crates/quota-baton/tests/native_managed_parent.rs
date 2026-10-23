//! Managed-parent composition with synthetic WS peers and owned synthetic I/O/jobs.
//! Exercises local ordering and write lifetime, not real core reset qualification.
#![allow(dead_code)] // Path-included staged APIs are not product registration.
#[path = "../src/native_resume/control_actor.rs"]
mod control_actor;
#[path = "../src/native_resume/core_control.rs"]
mod core_control;
#[path = "../src/native_resume/dispatcher.rs"]
mod dispatcher;
#[path = "../src/native_resume/endpoint.rs"]
mod endpoint;
#[path = "../src/native_resume/framing.rs"]
mod framing;
#[path = "../src/native_resume/goal.rs"]
mod goal;
#[path = "../src/native_resume/ingress.rs"]
mod ingress;
#[cfg(windows)]
#[path = "../src/launcher_label.rs"]
mod launcher_label;
#[path = "../src/native_resume/managed_parent.rs"]
mod managed_parent;
#[cfg(all(feature = "testkit", debug_assertions))]
#[path = "../src/native_resume/observation.rs"]
mod observation;
#[path = "../src/native_resume/parent_dispatch.rs"]
mod parent_dispatch;
#[path = "../src/native_resume/socket_pump.rs"]
mod socket_pump;

use std::{
    collections::VecDeque,
    future::Future,
    io::{self, Read, Write},
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use core_control::CoreControl;
use endpoint::Endpoint;
use futures_util::{SinkExt, StreamExt};
use managed_parent::{Error, JobStatus, Jobs, ManagedParent};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Notify,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, accept_async, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

const PREFIX: &str = "qb-goal-0123456789abcdef0123456789abcdef:";
const REQUEST: &[u8] = br#" {"id":1,"method":"account/read","params":{}} "#;
const RESPONSE: &[u8] = br#" {"id":1,"result":{"synthetic":true}} "#;
const READY: &[u8] = br#"{"method":"server/ready","params":{}}"#;
type UiPeer = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic operation failed"),
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    require(timeout(Duration::from_secs(15), future).await)
}

fn lifetime() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

fn text(bytes: &[u8]) -> Message {
    Message::Text(require(String::from_utf8(bytes.to_vec())).into())
}

async fn peers() -> (
    WebSocketStream<endpoint::EndpointIo>,
    UiPeer,
    CoreControl,
    WebSocketStream<TcpStream>,
) {
    let endpoint = require(Endpoint::bind().await);
    let mut request = require(endpoint.url().into_client_request());
    let mut auth: HeaderValue = require(format!("Bearer {}", endpoint.token()).parse());
    auth.set_sensitive(true);
    request.headers_mut().insert("authorization", auth);
    let (owned, peer) = tokio::join!(endpoint.accept(lifetime()), connect_async(request));

    let listener = require(TcpListener::bind("127.0.0.1:0").await);
    let address = require(listener.local_addr());
    let base = format!("http://{address}/l/{}/v1", "a".repeat(43));
    let accept = async {
        let (stream, _) = require(listener.accept().await);
        require(accept_async(stream).await)
    };
    let (control, core_peer) = tokio::join!(
        CoreControl::connect(&base, HeaderValue::from_static("Bearer synthetic-only")),
        accept,
    );
    (require(owned), require(peer).0, require(control), core_peer)
}

async fn closed<S>(peer: &mut WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    assert!(matches!(
        require(timeout(Duration::from_secs(3), peer.next()).await),
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
}

struct FakeState {
    stopped: bool,
    stops: usize,
    status: JobStatus,
    native: VecDeque<u8>,
    input: Vec<u8>,
    block: bool,
    writing: bool,
    blocked_at_stop: bool,
    pipe_drops: usize,
    fail_after: Option<usize>,
    block_flush: bool,
    flushes: usize,
    native_read_waiting: bool,
    native_eof: bool,
}

struct Proof {
    state: Mutex<FakeState>,
    changed: Condvar,
    entered: Notify,
    written: Notify,
    flushing: Notify,
    native_read_waiting: Notify,
}

impl Proof {
    fn new(block: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(FakeState {
                stopped: false,
                stops: 0,
                status: JobStatus::Running,
                native: VecDeque::new(),
                input: Vec::new(),
                block,
                writing: false,
                blocked_at_stop: false,
                pipe_drops: 0,
                fail_after: None,
                block_flush: false,
                flushes: 0,
                native_read_waiting: false,
                native_eof: false,
            }),
            changed: Condvar::new(),
            entered: Notify::new(),
            written: Notify::new(),
            flushing: Notify::new(),
            native_read_waiting: Notify::new(),
        })
    }

    fn native(&self, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        state.native.extend(bytes);
        state.native.push_back(b'\n');
        self.changed.notify_all();
    }

    async fn line_written(&self) {
        loop {
            let written = self.written.notified();
            tokio::pin!(written);
            written.as_mut().enable();
            if self.state.lock().unwrap().input.ends_with(b"\n") {
                return;
            }
            written.await;
        }
    }

    fn cleaned(&self, blocked: bool) {
        let state = self.state.lock().unwrap();
        assert!(state.stopped);
        assert_eq!(state.stops, 1);
        assert_eq!(state.pipe_drops, 2);
        assert_eq!(state.blocked_at_stop, blocked);
    }
}

struct FakeInput(Arc<Proof>);
struct FakeOutput(Arc<Proof>);

impl Write for FakeInput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.0.state.lock().unwrap();
        state.writing = true;
        self.0.entered.notify_one();
        while state.block && !state.stopped {
            state = self.0.changed.wait(state).unwrap();
        }
        state.writing = false;
        if state.stopped {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let remaining = state
            .fail_after
            .unwrap_or(usize::MAX)
            .saturating_sub(state.input.len());
        if remaining == 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let size = bytes.len().min(remaining);
        state.input.extend_from_slice(&bytes[..size]);
        self.0.written.notify_one();
        Ok(size)
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        if state.block_flush {
            self.0.flushing.notify_one();
        }
        while state.block_flush && !state.stopped {
            state = self.0.changed.wait(state).unwrap();
        }
        if state.stopped {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        state.flushes += 1;
        self.0.written.notify_one();
        Ok(())
    }
}

impl Read for FakeOutput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let mut state = self.0.state.lock().unwrap();
        while state.native.is_empty() && !state.native_eof && !state.stopped {
            state.native_read_waiting = true;
            self.0.native_read_waiting.notify_one();
            state = self.0.changed.wait(state).unwrap();
        }
        state.native_read_waiting = false;
        let len = bytes.len().min(state.native.len());
        for byte in &mut bytes[..len] {
            *byte = state.native.pop_front().unwrap();
        }
        Ok(len)
    }
}

impl Drop for FakeInput {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.pipe_drops += 1;
    }
}
impl Drop for FakeOutput {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.pipe_drops += 1;
    }
}

struct FakeJobs {
    proof: Arc<Proof>,
    pipes: Option<(FakeInput, FakeOutput)>,
}
impl FakeJobs {
    fn new(proof: Arc<Proof>) -> Self {
        Self {
            pipes: Some((FakeInput(proof.clone()), FakeOutput(proof.clone()))),
            proof,
        }
    }
}
impl Jobs for FakeJobs {
    type Input = FakeInput;
    type Output = FakeOutput;
    fn take_stdio(&mut self) -> io::Result<(FakeInput, FakeOutput)> {
        self.pipes.take().ok_or(io::ErrorKind::NotConnected.into())
    }
    fn try_wait(&self) -> io::Result<JobStatus> {
        Ok(self.proof.state.lock().unwrap().status)
    }
    fn stop_jobs(&mut self) -> io::Result<()> {
        let mut state = self.proof.state.lock().unwrap();
        state.stops += 1;
        state.blocked_at_stop = state.block && state.writing;
        state.stopped = true;
        self.proof.changed.notify_all();
        Ok(())
    }
}

// Current-thread runtime deliberately proves cleanup needs no other async poll.
#[tokio::test]
async fn managed_ordinary_bytes_then_drop_closes_local_futures_and_joins_workers() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let mut run = Box::pin(owner.run());
        tokio::select! {
            result = &mut run => { let _ = result; panic!("owner exited early"); }
            () = async {
                require(ui.send(text(REQUEST)).await);
                proof.line_written().await;
                assert_eq!(proof.state.lock().unwrap().input, [REQUEST, b"\n"].concat());
                proof.native(RESPONSE);
                let message = require(require(ui.next().await.ok_or(())));
                assert!(matches!(message, Message::Text(bytes) if bytes.as_bytes() == RESPONSE));
            } => {}
        }
        drop(run);
        proof.cleaned(false);
        closed(&mut ui).await;
        // No Observe/Inspect/Consume was emitted, even during ordinary traffic.
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_unpolled_owner_drop_stops_jobs_and_joins_waiting_readers() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        drop(owner);
        proof.cleaned(false);
        closed(&mut ui).await;
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_cancel_blocked_write_stops_jobs_before_join_without_async_progress() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(true);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let mut run = Box::pin(owner.run());
        tokio::select! {
            result = &mut run => { let _ = result; panic!("blocked writer exited early"); }
            () = async {
                require(ui.send(text(REQUEST)).await);
                proof.entered.notified().await;
                assert!(proof.state.lock().unwrap().writing);
            } => {}
        }
        drop(run);
        proof.cleaned(true);
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_watchdog_independently_stops_a_blocked_physical_write() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(true);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let peer = async {
            require(ui.send(text(REQUEST)).await);
            proof.entered.notified().await;
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(result, Err(Error::Watchdog));
        proof.cleaned(true);
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_pump_loss_interrupts_blocked_write_and_cleans_both_directions() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(true);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let peer = async {
            require(ui.send(text(REQUEST)).await);
            proof.entered.notified().await;
            require(ui.send(Message::Close(None)).await);
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(result, Err(Error::Pump));
        proof.cleaned(true);
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_child_status_preserves_which_child_exited_before_cleanup() {
    bounded(async {
        for status in [JobStatus::TuiExited(7), JobStatus::ServerExited(8)] {
            let (socket, _ui, control, mut core) = peers().await;
            let proof = Proof::new(false);
            proof.state.lock().unwrap().status = status;
            let owner = require(ManagedParent::new(
                FakeJobs::new(proof.clone()),
                PREFIX.into(),
                control,
                socket,
            ));
            assert_eq!(owner.run().await, Ok(status));
            proof.cleaned(false);
            closed(&mut core).await;
        }
    })
    .await;
}

#[cfg(windows)]
mod actual_remote {
    use super::*;
    use std::{
        ffi::{OsStr, OsString},
        fs::File,
        path::PathBuf,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct ActualProof {
        read_calls: AtomicUsize,
        in_read: AtomicBool,
        reading: Notify,
        in_write: AtomicBool,
        entered: Notify,
        stops: AtomicUsize,
        stopped: AtomicBool,
        stopped_during_write: AtomicBool,
        pipe_drops: AtomicUsize,
        pipe_closed: AtomicBool,
    }

    struct Input(File, Arc<ActualProof>);
    impl Write for Input {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.1.in_write.store(true, Ordering::Release);
            self.1.entered.notify_one();
            let result = self.0.write(bytes);
            self.1.in_write.store(false, Ordering::Release);
            result
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }
    impl Drop for Input {
        fn drop(&mut self) {
            self.1.pipe_drops.fetch_add(1, Ordering::AcqRel);
        }
    }
    struct Output(File, Arc<ActualProof>);
    impl Read for Output {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.1.in_read.store(true, Ordering::Release);
            self.1.read_calls.fetch_add(1, Ordering::AcqRel);
            self.1.reading.notify_one();
            let result = self.0.read(bytes);
            self.1.in_read.store(false, Ordering::Release);
            if matches!(&result, Ok(0))
                || matches!(&result, Err(error) if error.kind() == io::ErrorKind::BrokenPipe)
            {
                self.1.pipe_closed.store(true, Ordering::Release);
            }
            result
        }
    }
    impl Drop for Output {
        fn drop(&mut self) {
            self.1.pipe_drops.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct ActualJobs {
        run: launcher_label::RemoteRun,
        proof: Arc<ActualProof>,
    }
    impl Jobs for ActualJobs {
        type Input = Input;
        type Output = Output;
        fn take_stdio(&mut self) -> io::Result<(Input, Output)> {
            let (input, output) = Jobs::take_stdio(&mut self.run)?;
            Ok((
                Input(input, self.proof.clone()),
                Output(output, self.proof.clone()),
            ))
        }
        fn try_wait(&self) -> io::Result<JobStatus> {
            Jobs::try_wait(&self.run)
        }
        fn stop_jobs(&mut self) -> io::Result<()> {
            self.proof.stops.fetch_add(1, Ordering::AcqRel);
            self.proof.stopped_during_write.store(
                self.proof.in_write.load(Ordering::Acquire),
                Ordering::Release,
            );
            let result = Jobs::stop_jobs(&mut self.run);
            self.proof.stopped.store(
                matches!(Jobs::try_wait(&self.run), Ok(JobStatus::Stopped)),
                Ordering::Release,
            );
            result
        }
    }

    fn synthetic_pair(proof: Arc<ActualProof>) -> ActualJobs {
        let program = PathBuf::from(std::env::var_os("SystemRoot").expect("Windows system root"))
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let args = |script: &str| -> Vec<OsString> {
            ["-NoProfile", "-NonInteractive", "-Command", script]
                .into_iter()
                .map(OsString::from)
                .collect()
        };
        // These are synthetic child commands, not an installed native client.
        // The server deliberately never reads its stdin. No global env/config.
        let server = args(
            "[Console]::Out.WriteLine('{\"method\":\"server/ready\",\"params\":{}}'); [Console]::Out.Flush(); Start-Sleep -Seconds 30",
        );
        let tui = args("Start-Sleep -Seconds 30");
        let run = require(launcher_label::spawn_remote_pair(
            &program,
            &server,
            &tui,
            "QB_SYNTHETIC_PARENT_TOKEN",
            OsStr::new("synthetic-only"),
            None,
        ));
        ActualJobs { run, proof }
    }

    #[tokio::test]
    async fn managed_actual_remote_pair_large_write_cancel_and_pump_loss_drain_jobs_and_join() {
        bounded(async {
            for pump_loss in [false, true] {
                let (socket, mut ui, control, mut core) = peers().await;
                let proof = Arc::new(ActualProof::default());
                let owner = require(ManagedParent::new(
                    synthetic_pair(proof.clone()),
                    PREFIX.into(),
                    control,
                    socket,
                ));
                let large = serde_json::json!({
                    "id": 1, "method": "account/read",
                    "params": {"syntheticPadding": "x".repeat(512 * 1024)}
                })
                .to_string();
                let mut run = Box::pin(owner.run());
                tokio::select! {
                    result = &mut run => { let _ = result; panic!("synthetic pair exited early"); }
                    () = async {
                        let ready = require(require(ui.next().await.ok_or(())));
                        assert!(matches!(ready, Message::Text(bytes) if bytes.as_bytes() == READY));
                        // Prove stdout's next owned read has entered before teardown;
                        // its EOF is independent evidence that the job pipe closed.
                        loop {
                            let reading = proof.reading.notified();
                            tokio::pin!(reading);
                            reading.as_mut().enable();
                            if proof.read_calls.load(Ordering::Acquire) >= 2
                                && proof.in_read.load(Ordering::Acquire) { break; }
                            reading.await;
                        }
                        require(ui.send(Message::Text(large.into())).await);
                        proof.entered.notified().await;
                        assert!(proof.in_write.load(Ordering::Acquire));
                    } => {}
                }
                if pump_loss {
                    require(ui.send(Message::Close(None)).await);
                    assert_eq!(run.as_mut().await, Err(Error::Pump));
                }
                // Cancellation is synchronous even on this current-thread runtime.
                drop(run);
                assert_eq!(proof.stops.load(Ordering::Acquire), 1);
                assert!(proof.stopped.load(Ordering::Acquire));
                assert!(proof.stopped_during_write.load(Ordering::Acquire));
                assert_eq!(proof.pipe_drops.load(Ordering::Acquire), 2);
                assert!(proof.pipe_closed.load(Ordering::Acquire));
                closed(&mut core).await;
            }
        })
        .await;
    }
}

// Real local control client/actor and Dispatcher, with synthetic peers only.
// These peers assert protocol/correlation and local ordering, not core quota authority.
const THREAD: &str = "11111111-2222-4333-8444-555555555555";
const EPISODE: u64 = 17;
use qb_proto::native_reset::{Command, Event, Refusal, Reply, Request as CoreRequest, VERSION};
use serde_json::{Value, json};

impl Proof {
    async fn wait_native_frame(&self, id: &Value) -> Value {
        loop {
            let changed = self.written.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let found = self
                .state
                .lock()
                .unwrap()
                .input
                .split(|b| *b == b'\n')
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                .find(|frame| &frame["id"] == id);
            if let Some(frame) = found {
                return frame;
            }
            changed.await;
        }
    }
    fn automatic_count(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .input
            .split(|b| *b == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .filter(|frame| frame["params"]["origin"] == "automatic")
            .count()
    }
    fn live(&self) {
        let state = self.state.lock().unwrap();
        assert!(!state.stopped);
        assert_eq!(state.stops, 0);
    }
}
fn native_goal(status: &str) -> Value {
    json!({"threadId":THREAD,"objective":"synthetic objective","createdAt":1,"updatedAt":2,"status":status})
}
async fn ui_value(ui: &mut UiPeer) -> Value {
    let message = require(require(ui.next().await.ok_or(())));
    let Message::Text(text) = message else {
        panic!("expected synthetic UI JSON");
    };
    require(serde_json::from_str(text.as_str()))
}
async fn core_request(core: &mut WebSocketStream<TcpStream>) -> CoreRequest {
    let message = require(require(core.next().await.ok_or(())));
    let Message::Text(text) = message else {
        panic!("expected bounded control request");
    };
    let request: CoreRequest = require(serde_json::from_str(text.as_str()));
    assert_eq!(request.version, VERSION);
    assert_ne!(request.request_id, 0);
    request
}
async fn core_reply(core: &mut WebSocketStream<TcpStream>, id: Option<u64>, event: Event) {
    let reply = Reply {
        version: VERSION,
        request_id: id,
        event,
    };
    assert!(reply.valid_shape());
    require(
        core.send(Message::Text(require(serde_json::to_string(&reply)).into()))
            .await,
    );
}
async fn stopped_dialogue(ui: &mut UiPeer, proof: &Proof) {
    for (request, response) in [
        (
            json!({"id":21,"method":"thread/start","params":{}}),
            json!({"id":21,"result":{"thread":{"id":THREAD}}}),
        ),
        (
            json!({"id":22,"method":"thread/goal/set","params":{
            "threadId":THREAD,"objective":"synthetic objective","origin":"user","status":"active"}}),
            json!({"id":22,"result":{"goal":native_goal("active")}}),
        ),
    ] {
        require(ui.send(text(request.to_string().as_bytes())).await);
        assert_eq!(proof.wait_native_frame(&request["id"]).await, request);
        proof.native(response.to_string().as_bytes());
        assert_eq!(ui_value(ui).await, response);
    }
    let notification = json!({"method":"thread/goal/updated","params":{"threadId":THREAD,"goal":native_goal("usageLimited")}});
    proof.native(notification.to_string().as_bytes());
    assert_eq!(ui_value(ui).await, notification);
    let request = json!({"id":23,"method":"thread/goal/get","params":{"threadId":THREAD}});
    require(ui.send(text(request.to_string().as_bytes())).await);
    assert_eq!(proof.wait_native_frame(&request["id"]).await, request);
    let response = json!({"id":23,"result":{"goal":native_goal("usageLimited")}});
    proof.native(response.to_string().as_bytes());
    assert_eq!(ui_value(ui).await, response);
}
async fn waiting(core: &mut WebSocketStream<TcpStream>) {
    let request = core_request(core).await;
    assert!(matches!(&request.command, Command::Observe { thread_id } if thread_id == THREAD));
    core_reply(
        core,
        Some(request.request_id),
        Event::Waiting {
            episode: EPISODE,
            reset_unix_ms: 2_000_000_000_000,
        },
    )
    .await;
}
async fn ready(core: &mut WebSocketStream<TcpStream>, proof: &Proof) {
    core_reply(core, None, Event::Due { episode: EPISODE }).await;
    let request = core_request(core).await;
    assert!(matches!(
        request.command,
        Command::Inspect { episode: EPISODE }
    ));
    // Repeated same-episode hints must coalesce rather than schedule another turn.
    core_reply(core, None, Event::Due { episode: EPISODE }).await;
    core_reply(
        core,
        Some(request.request_id),
        Event::Ready { episode: EPISODE },
    )
    .await;
    let final_get = proof.wait_native_frame(&json!(format!("{PREFIX}1"))).await;
    assert_eq!(final_get["method"], "thread/goal/get");
    assert_eq!(final_get["params"]["threadId"], THREAD);
}
fn final_native_reply(proof: &Proof) {
    proof.native(
        json!({"id":format!("{PREFIX}1"),"result":{"goal":native_goal("usageLimited")}})
            .to_string()
            .as_bytes(),
    );
}
async fn ordinary_still_live(ui: &mut UiPeer, proof: &Proof, id: u64) {
    proof.live();
    let request = json!({"id":id,"method":"account/read","params":{"synthetic":true}});
    require(ui.send(text(request.to_string().as_bytes())).await);
    assert_eq!(proof.wait_native_frame(&request["id"]).await, request);
    let response = json!({"id":id,"result":{"synthetic":true}});
    proof.native(response.to_string().as_bytes());
    assert_eq!(ui_value(ui).await, response);
    proof.live();
}

#[tokio::test]
async fn managed_episode_observe_inspect_final_reply_then_consume_once_live_through_flush() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let owner = require(ManagedParent::new(FakeJobs::new(proof.clone()), PREFIX.into(), control, socket));
        let mut run = Box::pin(owner.run());
        tokio::select! {
            result = &mut run => { let _ = result; panic!("run exited during episode"); }
            () = async {
                stopped_dialogue(&mut ui, &proof).await;
                waiting(&mut core).await;
                ready(&mut core, &proof).await;
                assert_eq!(proof.automatic_count(), 0);
                // The next command cannot be Consume until the final native reply.
                assert!(timeout(Duration::from_millis(30), core.next()).await.is_err());
                proof.state.lock().unwrap().block_flush = true;
                final_native_reply(&proof);
                let consume = core_request(&mut core).await;
                assert!(matches!(consume.command, Command::Consume { episode: EPISODE }));
                core_reply(&mut core, Some(consume.request_id), Event::Granted { episode: EPISODE }).await;
                proof.flushing.notified().await;
                assert_eq!(proof.automatic_count(), 1);
                // A physical flush is positively blocked: control must remain live.
                assert!(timeout(Duration::from_millis(30), core.next()).await.is_err(), "managed_core_live_through_activation_flush");
                {
                    let mut state = proof.state.lock().unwrap();
                    state.block_flush = false;
                    proof.changed.notify_all();
                }
                let command = proof.wait_native_frame(&json!(format!("{PREFIX}2"))).await;
                assert_eq!(command["params"], json!({"threadId":THREAD,"status":"active","origin":"automatic"}));
                ordinary_still_live(&mut ui, &proof, 30).await;
                final_native_reply(&proof);
                ordinary_still_live(&mut ui, &proof, 31).await;
                assert_eq!(proof.automatic_count(), 1);
            } => {}
        }
        drop(run);
        proof.cleaned(false);
    }).await;
}

#[tokio::test]
async fn managed_episode_refused_core_loss_wrong_hint_and_inspect_refusal_preserve_tui() {
    bounded(async {
        for mode in 0..4 {
            let (socket, mut ui, control, mut core) = peers().await;
            let proof = Proof::new(false);
            let owner = require(ManagedParent::new(FakeJobs::new(proof.clone()), PREFIX.into(), control, socket));
            let mut run = Box::pin(owner.run());
            tokio::select! {
                result = &mut run => { let _ = result; panic!("core authority loss killed ordinary run mode={mode}"); }
                () = async {
                    stopped_dialogue(&mut ui, &proof).await;
                    if mode == 0 {
                        let request = core_request(&mut core).await;
                        assert!(matches!(&request.command, Command::Observe { thread_id } if thread_id == THREAD));
                        core_reply(&mut core, Some(request.request_id), Event::Refused { reason: Refusal::Unavailable }).await;
                    } else {
                        waiting(&mut core).await;
                        match mode {
                            1 => require(core.send(Message::Close(None)).await),
                            2 => core_reply(&mut core, None, Event::Due { episode: EPISODE + 1 }).await,
                            _ => {
                                core_reply(&mut core, None, Event::Due { episode: EPISODE }).await;
                                let request = core_request(&mut core).await;
                                assert!(matches!(request.command, Command::Inspect { episode: EPISODE }));
                                core_reply(&mut core, Some(request.request_id), Event::Refused { reason: Refusal::Cancelled }).await;
                            }
                        }
                    }
                    closed(&mut core).await;
                    ordinary_still_live(&mut ui, &proof, 40).await;
                    ordinary_still_live(&mut ui, &proof, 41).await;
                    assert_eq!(proof.automatic_count(), 0);
                } => {}
            }
            drop(run);
            proof.cleaned(false);
        }
    }).await;
}

#[tokio::test]
async fn managed_episode_ui_during_inspect_is_forwarded_and_no_consume_occurs() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let mut owner = require(ManagedParent::new(FakeJobs::new(proof.clone()), PREFIX.into(), control, socket));
        let mut admitted = owner.test_ingress();
        let mut run = Box::pin(owner.run());
        tokio::select! {
            result = &mut run => { let _ = result; panic!("inspect veto killed ordinary run"); }
            () = async {
                stopped_dialogue(&mut ui, &proof).await;
                waiting(&mut core).await;
                core_reply(&mut core, None, Event::Due { episode: EPISODE }).await;
                let request = core_request(&mut core).await;
                assert!(matches!(request.command, Command::Inspect { episode: EPISODE }));
                while admitted.try_recv().is_ok() {}
                require(ui.send(text(br#"{"id":50,"method":"account/read","params":{}}"#)).await);
                assert_eq!(admitted.recv().await, Some(ingress::Source::Ui));
                // Publication, not WebSocket send completion, establishes the veto.
                core_reply(&mut core, Some(request.request_id), Event::Ready { episode: EPISODE }).await;
                closed(&mut core).await;
                proof.wait_native_frame(&json!(50)).await;
                proof.native(br#"{"id":50,"result":{}}"#);
                assert_eq!(ui_value(&mut ui).await["id"], 50);
                assert_eq!(proof.automatic_count(), 0);
                ordinary_still_live(&mut ui, &proof, 51).await;
            } => {}
        }
        drop(run);
        proof.cleaned(false);
    }).await;
}

#[tokio::test]
async fn managed_episode_consumed_partial_write_is_uncertain_and_never_retried() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let peer = async {
            stopped_dialogue(&mut ui, &proof).await;
            waiting(&mut core).await;
            ready(&mut core, &proof).await;
            let before = proof.state.lock().unwrap().input.len();
            proof.state.lock().unwrap().fail_after = Some(before + 5);
            final_native_reply(&proof);
            let consume = core_request(&mut core).await;
            assert!(matches!(
                consume.command,
                Command::Consume { episode: EPISODE }
            ));
            core_reply(
                &mut core,
                Some(consume.request_id),
                Event::Granted { episode: EPISODE },
            )
            .await;
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(result, Err(Error::WriteUncertain));
        proof.cleaned(false);
        {
            let state = proof.state.lock().unwrap();
            assert_eq!(
                Some(state.input.len()),
                state.fail_after,
                "exactly one partial write, no retry"
            );
        }
        assert_eq!(
            proof.automatic_count(),
            0,
            "partial frame is not a complete activation receipt"
        );
    })
    .await;
}

#[tokio::test]
async fn managed_episode_ui_published_during_consume_vetoes_grant_and_preserves_tui() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let mut owner = require(ManagedParent::new(FakeJobs::new(proof.clone()), PREFIX.into(), control, socket));
        let mut admitted = owner.test_ingress();
        let mut run = Box::pin(owner.run());
        tokio::select! {
            result = &mut run => { let _ = result; panic!("consume veto killed ordinary run"); }
            () = async {
                stopped_dialogue(&mut ui, &proof).await;
                waiting(&mut core).await;
                ready(&mut core, &proof).await;
                final_native_reply(&proof);
                let consume = core_request(&mut core).await;
                assert!(matches!(consume.command, Command::Consume { episode: EPISODE }));
                while admitted.try_recv().is_ok() {}
                require(ui.send(text(br#"{"id":60,"method":"account/read","params":{}}"#)).await);
                assert_eq!(admitted.recv().await, Some(ingress::Source::Ui));
                core_reply(&mut core, Some(consume.request_id), Event::Granted { episode: EPISODE }).await;
                closed(&mut core).await;
                proof.wait_native_frame(&json!(60)).await;
                proof.native(br#"{"id":60,"result":{}}"#);
                assert_eq!(ui_value(&mut ui).await["id"], 60);
                ordinary_still_live(&mut ui, &proof, 61).await;
                assert_eq!(proof.automatic_count(), 0);
            } => {}
        }
        drop(run);
        proof.cleaned(false);
    }).await;
}

#[tokio::test]
async fn managed_episode_consume_timeout_disables_auto_without_whole_run_expiry() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let owner = require(ManagedParent::new(FakeJobs::new(proof.clone()), PREFIX.into(), control, socket));
        let mut run = Box::pin(owner.run());
        tokio::select! {
            result = &mut run => { let _ = result; panic!("AwaitCore timeout killed ordinary run"); }
            () = async {
                stopped_dialogue(&mut ui, &proof).await;
                waiting(&mut core).await;
                ready(&mut core, &proof).await;
                final_native_reply(&proof);
                let consume = core_request(&mut core).await;
                assert!(matches!(consume.command, Command::Consume { episode: EPISODE }));
                // Deliberately withhold the reply beyond the one original
                // five-second event budget. No physical write has begun.
                let end = require(timeout(Duration::from_secs(7), core.next()).await);
                assert!(matches!(end, None | Some(Err(_)) | Some(Ok(Message::Close(_)))));
                ordinary_still_live(&mut ui, &proof, 70).await;
                // A stale final response cannot rearm a second episode.
                final_native_reply(&proof);
                ordinary_still_live(&mut ui, &proof, 71).await;
                assert_eq!(proof.automatic_count(), 0);
            } => {}
        }
        drop(run);
        proof.cleaned(false);
    }).await;
}

#[tokio::test]
async fn managed_episode_delayed_consume_blocked_write_keeps_original_candidate_deadline() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let peer = async {
            stopped_dialogue(&mut ui, &proof).await;
            waiting(&mut core).await;
            ready(&mut core, &proof).await;
            final_native_reply(&proof);
            let consume = core_request(&mut core).await;
            let consume_at = Instant::now();
            assert!(matches!(
                consume.command,
                Command::Consume { episode: EPISODE }
            ));
            let before = {
                let mut state = proof.state.lock().unwrap();
                state.block = true;
                state.input.len()
            };
            // Spend part of this candidate's existing budget in the real
            // Consume exchange. Physical admission must not refill five seconds.
            tokio::time::sleep_until(consume_at + Duration::from_secs(2)).await;
            core_reply(
                &mut core,
                Some(consume.request_id),
                Event::Granted { episode: EPISODE },
            )
            .await;
            loop {
                let entered = proof.entered.notified();
                tokio::pin!(entered);
                entered.as_mut().enable();
                {
                    let state = proof.state.lock().unwrap();
                    if state.writing && state.block {
                        assert!(!state.stopped, "activation_write_started_before_watchdog");
                        assert_eq!(state.input.len(), before, "activation_payload_is_blocked");
                        break;
                    }
                }
                // Earlier ordinary writes may have left a Notify permit; the
                // checked writer state, rather than that permit, is the barrier.
                entered.await;
            }
            consume_at
        };
        let run = async {
            let result = owner.run().await;
            (result, Instant::now())
        };
        let ((result, stopped_at), consume_at) = tokio::join!(run, peer);
        let elapsed = stopped_at.duration_since(consume_at);
        assert_eq!(result, Err(Error::Watchdog));
        assert!(
            elapsed >= Duration::from_millis(4500),
            "original_candidate_deadline_not_early: {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_millis(6500),
            "original_candidate_deadline_not_refilled_after_consume: {elapsed:?}"
        );
        proof.cleaned(true);
        assert_eq!(
            proof.automatic_count(),
            0,
            "blocked activation delivered no frame"
        );
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_peer_closed_then_delayed_actual_tui_exit_reports_zero() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let peer = async {
            // Confirm an operational ordinary transport before the genuine
            // synthetic TUI socket closes. No job exit is published yet.
            ordinary_still_live(&mut ui, &proof, 80).await;
            require(ui.send(Message::Close(None)).await);
            closed(&mut ui).await;
            tokio::time::sleep(Duration::from_millis(75)).await;
            {
                let mut state = proof.state.lock().unwrap();
                assert_eq!(state.status, JobStatus::Running);
                assert!(
                    !state.stopped,
                    "peer_close_must_allow_actual_tui_exit_status_to_arrive"
                );
                state.status = JobStatus::TuiExited(0);
            }
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(
            result,
            Ok(JobStatus::TuiExited(0)),
            "peer_close_followed_by_actual_tui_exit_preserves_zero_status"
        );
        proof.cleaned(false);
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_peer_closed_without_actual_job_exit_remains_pump_failure() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let peer = async {
            ordinary_still_live(&mut ui, &proof, 81).await;
            require(ui.send(Message::Close(None)).await);
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(
            result,
            Err(Error::Pump),
            "peer_close_is_not_a_fabricated_job_exit"
        );
        assert_eq!(proof.state.lock().unwrap().status, JobStatus::Running);
        proof.cleaned(false);
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_invalid_binary_without_actual_job_exit_remains_pump_failure() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let peer = async {
            ordinary_still_live(&mut ui, &proof, 82).await;
            require(ui.send(Message::Binary(vec![0_u8].into())).await);
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(
            result,
            Err(Error::Pump),
            "invalid_binary_is_not_a_successful_tui_exit"
        );
        assert_eq!(proof.state.lock().unwrap().status, JobStatus::Running);
        proof.cleaned(false);
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_successful_tui_exit_with_pending_write_reports_uncertainty() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(true);
        let owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let peer = async {
            require(ui.send(text(REQUEST)).await);
            proof.entered.notified().await;
            {
                let mut state = proof.state.lock().unwrap();
                assert!(
                    state.writing && state.block,
                    "native_write_pending_before_successful_exit"
                );
                assert!(!state.stopped);
                assert_eq!(state.status, JobStatus::Running);
                // No pump failure or watchdog has initiated cleanup. The
                // successful job status itself makes the supervisor stop jobs.
                state.status = JobStatus::TuiExited(0);
            }
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(
            result,
            Err(Error::WriteUncertain),
            "successful_child_exit_must_not_hide_cleanup_write_uncertainty"
        );
        proof.cleaned(true);
        assert_eq!(proof.state.lock().unwrap().status, JobStatus::TuiExited(0));
        assert!(
            proof.state.lock().unwrap().input.is_empty(),
            "blocked_native_write_was_not_retried"
        );
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_peer_close_during_consume_publisher_shutdown_preserves_delayed_tui_exit() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let mut owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let mut events = owner.test_lifecycle();
        let gate = owner.test_pause_consume_return();
        let peer = async {
            stopped_dialogue(&mut ui, &proof).await;
            waiting(&mut core).await;
            ready(&mut core, &proof).await;
            final_native_reply(&proof);
            let consume = core_request(&mut core).await;
            assert!(matches!(
                consume.command,
                Command::Consume { episode: EPISODE }
            ));
            // Prove native stdout is waiting before peer closure. Its next
            // valid frame must return through the normal stopped-reader path.
            loop {
                let reading = proof.native_read_waiting.notified();
                tokio::pin!(reading);
                reading.as_mut().enable();
                if proof.state.lock().unwrap().native_read_waiting {
                    break;
                }
                reading.await;
            }
            // Do not grant Consume: actual peer closure cancels the exchange.
            require(ui.send(Message::Close(None)).await);
            gate.entered().await;
            loop {
                match events.recv().await {
                    Some(managed_parent::TestLifecycle::UiPublisherFinished) => break,
                    Some(_) => {}
                    None => panic!("missing_ui_publisher_shutdown_observation"),
                }
            }
            proof.live();
            assert_eq!(proof.automatic_count(), 0);
            proof.native(READY);
            loop {
                match events.recv().await {
                    Some(managed_parent::TestLifecycle::NativeReaderFinished) => break,
                    Some(_) => {}
                    None => panic!("missing_stopped_native_reader_completion_observation"),
                }
            }
            // Both reader exit paths have completed while the real Consume
            // callback is still paused. Neither may manufacture ingress loss.
            gate.release();
            loop {
                match events.recv().await {
                    Some(managed_parent::TestLifecycle::DispatchWorkerFinished) => break,
                    Some(_) => {}
                    None => panic!("missing_post_latch_dispatch_worker_completion_observation"),
                }
            }
            // Actual status arrives only after publisher shutdown AND the worker
            // wrapper has latched its final result. Callback completion alone
            // precedes that latch and could hide a real Revoked/Ingress error
            // behind a successful child status.
            let mut state = proof.state.lock().unwrap();
            assert!(
                !state.stopped,
                "peer_close_during_consume_preserves_actual_exit_zero"
            );
            assert_eq!(state.status, JobStatus::Running);
            state.status = JobStatus::TuiExited(0);
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(
            result,
            Ok(JobStatus::TuiExited(0)),
            "peer_close_during_consume_preserves_actual_exit_zero"
        );
        assert_eq!(
            proof.automatic_count(),
            0,
            "peer_close_during_consume_sends_no_activation"
        );
        proof.cleaned(false);
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_direct_write_failure_before_cleanup_overrides_selected_pump_error() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(true);
        let mut owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let mut events = owner.test_lifecycle();
        let gate = owner.test_pause_before_cleanup();
        let peer = async {
            require(ui.send(text(REQUEST)).await);
            proof.entered.notified().await;
            assert!(proof.state.lock().unwrap().writing);
            require(ui.send(Message::Binary(vec![0_u8].into())).await);
            // The supervisor has selected the real pump failure; cleanup has
            // not begun. Fail the writer independently of stop_jobs().
            gate.entered().await;
            {
                let mut state = proof.state.lock().unwrap();
                assert!(!state.stopped, "direct_write_failure_precedes_job_cleanup");
                assert!(state.writing);
                state.fail_after = Some(state.input.len());
                state.block = false;
                proof.changed.notify_all();
            }
            loop {
                match events.recv().await {
                    Some(managed_parent::TestLifecycle::FailureLatched(Error::WriteUncertain)) => {
                        break;
                    }
                    Some(_) => {}
                    None => panic!("missing_precleanup_uncertainty_latch_observation"),
                }
            }
            assert!(!proof.state.lock().unwrap().stopped);
            gate.release();
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(
            result,
            Err(Error::WriteUncertain),
            "direct_precleanup_uncertainty_outranks_selected_pump_failure"
        );
        proof.cleaned(false);
        assert!(proof.state.lock().unwrap().input.is_empty());
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_genuine_ingress_overflow_during_consume_remains_fatal() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let mut owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let mut events = owner.test_lifecycle();
        let mut admitted = owner.test_ingress();
        let peer = async {
            stopped_dialogue(&mut ui, &proof).await;
            waiting(&mut core).await;
            ready(&mut core, &proof).await;
            final_native_reply(&proof);
            let consume = core_request(&mut core).await;
            assert!(matches!(
                consume.command,
                Command::Consume { episode: EPISODE }
            ));
            while admitted.try_recv().is_ok() {}
            // Consume is actually pending, so no dispatcher can drain these
            // eight admitted frames. The ninth real UI frame overflows ingress.
            for id in 100..108 {
                let request = json!({"id":id,"method":"account/read","params":{}});
                require(ui.send(text(request.to_string().as_bytes())).await);
                assert_eq!(admitted.recv().await, Some(ingress::Source::Ui));
            }
            require(
                ui.send(text(br#"{"id":108,"method":"account/read","params":{}}"#))
                    .await,
            );
            loop {
                match events.recv().await {
                    Some(managed_parent::TestLifecycle::FailureLatched(Error::Ingress)) => break,
                    Some(_) => {}
                    None => panic!("missing_genuine_ingress_failure_observation"),
                }
            }
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(
            result,
            Err(Error::Ingress),
            "genuine_ingress_overflow_is_not_clean_peer_shutdown"
        );
        assert_eq!(proof.automatic_count(), 0);
        proof.cleaned(false);
        closed(&mut core).await;
    })
    .await;
}

#[tokio::test]
async fn managed_native_eof_before_cleanup_preserves_root_over_selected_pump_failure() {
    bounded(async {
        let (socket, mut ui, control, mut core) = peers().await;
        let proof = Proof::new(false);
        let mut owner = require(ManagedParent::new(
            FakeJobs::new(proof.clone()),
            PREFIX.into(),
            control,
            socket,
        ));
        let mut events = owner.test_lifecycle();
        let gate = owner.test_pause_before_cleanup();
        let peer = async {
            ordinary_still_live(&mut ui, &proof, 83).await;
            // The next native read must already be in flight when the pump
            // stops admission. No job termination creates this EOF.
            loop {
                let reading = proof.native_read_waiting.notified();
                tokio::pin!(reading);
                reading.as_mut().enable();
                if proof.state.lock().unwrap().native_read_waiting {
                    break;
                }
                reading.await;
            }
            require(ui.send(Message::Binary(vec![0_u8].into())).await);
            gate.entered().await;
            {
                let mut state = proof.state.lock().unwrap();
                assert!(!state.stopped, "native_eof_precedes_cleanup");
                assert_eq!(state.status, JobStatus::Running);
                assert!(state.native_read_waiting);
                assert!(state.native.is_empty());
                // Close the synthetic native stdout independently. The actual
                // reader returns zero, read_frame returns None, and the native
                // reader reports NativeEof through the ordinary failure path.
                state.native_eof = true;
                proof.changed.notify_all();
            }
            loop {
                match events.recv().await {
                    Some(managed_parent::TestLifecycle::FailureLatched(Error::NativeEof)) => break,
                    Some(_) => {}
                    None => panic!("missing_native_eof_before_cleanup_latch"),
                }
            }
            assert!(!proof.state.lock().unwrap().stopped);
            gate.release();
        };
        let (result, ()) = tokio::join!(owner.run(), peer);
        assert_eq!(
            result,
            Err(Error::NativeEof),
            "native_eof_latched_before_cleanup_outranks_selected_pump_failure"
        );
        assert_eq!(proof.state.lock().unwrap().status, JobStatus::Running);
        assert_eq!(proof.automatic_count(), 0);
        proof.cleaned(false);
        closed(&mut core).await;
    })
    .await;
}
