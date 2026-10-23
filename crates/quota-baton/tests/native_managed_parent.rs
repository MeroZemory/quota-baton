//! Unregistered owner scaffold. Synthetic WS peers and owned synthetic I/O/jobs.
//! No reset qualification or automatic activation is claimed by these tests.
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
}

struct Proof {
    state: Mutex<FakeState>,
    changed: Condvar,
    entered: Notify,
    written: Notify,
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
            }),
            changed: Condvar::new(),
            entered: Notify::new(),
            written: Notify::new(),
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
        state.input.extend_from_slice(bytes);
        self.0.written.notify_one();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for FakeOutput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let mut state = self.0.state.lock().unwrap();
        while state.native.is_empty() && !state.stopped {
            state = self.0.changed.wait(state).unwrap();
        }
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
            lifetime(),
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
            lifetime(),
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
            lifetime(),
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
            lifetime(),
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
            lifetime(),
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
                lifetime(),
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
                    lifetime(),
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
