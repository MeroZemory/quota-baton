//! M7c.1: load the real release `serve` process from a separate test process.
//! All homes, tokens, and upstreams in this file are synthetic.

use axum::{
    Router,
    body::Body,
    extract::{
        Request,
        ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade},
    },
    response::Response,
    routing::post,
};
use bytes::Bytes;
use futures_util::{SinkExt, future::join_all};
use qb_core::accounts::{
    Account, AccountStore, Health, Identity, Provider, Source, TokenMeta, Tokens, UserState,
};
use reqwest::Client;
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::net::{TcpListener as TokioListener, TcpStream};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use windows::Win32::System::{
    ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX},
    Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ},
};

struct A2Reader {
    remaining: usize,
}

impl std::io::Read for A2Reader {
    fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
        let count = self.remaining.min(target.len());
        target[..count].fill(b'z');
        self.remaining -= count;
        Ok(count)
    }
}

#[derive(Clone, Copy)]
enum A2Shape {
    Plain(usize),
    Zstd(usize),
}

struct R7Sampler {
    state: u64,
}

impl R7Sampler {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn unit(&mut self) -> f64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.state >> 11) as f64 / ((1u64 << 53) as f64)
    }

    fn next(&mut self) -> (&'static str, A2Shape) {
        let p = self.unit();
        let size = if p < 0.35 {
            800 * 1024
        } else if p < 0.50 {
            1_400 * 1024
        } else if p < 0.90 {
            6 * 1024 * 1024
        } else if p < 0.97 {
            33 * 1024 * 1024
        } else {
            51 * 1024 * 1024
        };
        let compressed = size >= 33 * 1024 * 1024 && self.unit() < 0.35;
        let bucket = if size < 1024 * 1024 {
            "<1 MB"
        } else if size <= 10 * 1024 * 1024 {
            "1-10 MB"
        } else {
            ">10 MB"
        };
        (
            bucket,
            if compressed {
                A2Shape::Zstd(size)
            } else {
                A2Shape::Plain(size)
            },
        )
    }
}

fn a2_body(shape: A2Shape) -> (reqwest::Body, bool) {
    const CHUNK: usize = 64 * 1024;
    match shape {
        A2Shape::Plain(size) => {
            let stream = futures_util::stream::unfold(0usize, move |offset| async move {
                if offset >= size {
                    return None;
                }
                let count = (size - offset).min(CHUNK);
                Some((
                    Ok::<_, std::io::Error>(Bytes::from(vec![b'p'; count])),
                    offset + count,
                ))
            });
            (reqwest::Body::wrap_stream(stream), false)
        }
        A2Shape::Zstd(size) => {
            let (sender, receiver) = tokio::sync::mpsc::channel(2);
            tokio::task::spawn_blocking(move || {
                let mut encoder =
                    zstd::stream::read::Encoder::new(A2Reader { remaining: size }, 1).unwrap();
                loop {
                    let mut chunk = vec![0; CHUNK];
                    let count = std::io::Read::read(&mut encoder, &mut chunk).unwrap();
                    if count == 0 {
                        break;
                    }
                    chunk.truncate(count);
                    if sender
                        .blocking_send(Ok::<_, std::io::Error>(Bytes::from(chunk)))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|item| (item, receiver))
            });
            (reqwest::Body::wrap_stream(stream), true)
        }
    }
}

struct FakeUpstream {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl FakeUpstream {
    async fn start() -> Self {
        let listener = TokioListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        async fn http(_request: Request) -> Response {
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: {\"type\":\"response.output_text.delta\"}\n\n"));
                tokio::time::sleep(Duration::from_millis(200)).await;
                yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: {\"type\":\"response.completed\"}\n\n"));
            };
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        async fn websocket(ws: WebSocketUpgrade) -> Response {
            ws.on_upgrade(|mut socket: WebSocket| async move {
                let mut active = false;
                loop {
                    tokio::select! {
                        message = socket.recv() => {
                            if message.is_none() { break; }
                            if !active {
                                active = true;
                                if socket.send(AxumMessage::Text("{\"type\":\"response.output_text.delta\"}".into())).await.is_err() { break; }
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_secs(2)), if active => {
                            if socket.send(AxumMessage::Text("{\"type\":\"response.output_text.delta\"}".into())).await.is_err() { break; }
                        }
                    }
                }
            })
        }
        let app = Router::new().route("/responses", post(http).get(websocket));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { url, task }
    }
}
impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Home {
    _root: TempDir,
    path: PathBuf,
    data: u16,
    admin: u16,
    upstream: FakeUpstream,
}

impl Home {
    async fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m7c-spawned");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let data = TcpListener::bind("127.0.0.1:0").unwrap();
        let admin = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = FakeUpstream::start().await;
        let home = Self {
            path: root.path().join("home"),
            _root: root,
            data: data.local_addr().unwrap().port(),
            admin: admin.local_addr().unwrap().port(),
            upstream,
        };
        fs::create_dir_all(&home.path).unwrap();
        home.write_accounts();
        fs::write(home.path.join("config.toml"), format!(
            "[server]\ndata_port={}\nadmin_port={}\ndrain_timeout_secs=20\n[upstream]\ncodex_base='{}'\nanthropic_base='{}'\nclaude_usage_base='{}'\ncodex_usage_base='{}'\nclaude_token_endpoint='{}/oauth/token'\ncodex_token_endpoint='{}/oauth/token'\n",
            home.data, home.admin, home.upstream.url, home.upstream.url, home.upstream.url, home.upstream.url, home.upstream.url, home.upstream.url)).unwrap();
        home
    }
    fn write_accounts(&self) {
        let store = AccountStore::acquire(&self.path).unwrap();
        for n in 0..3 {
            let email = format!("m7c-{n}@example.invalid");
            let identity = Identity::Codex {
                account_id: format!("m7c-{n}"),
                user_id: None,
                email: email.clone(),
            };
            let account = Account {
                schema: 1,
                id: identity.id(),
                provider: Provider::Codex,
                label: email.clone(),
                email,
                plan: None,
                identity,
                source: Source::Oauth,
                user_state: UserState::Active,
                health: Health::Ready,
                auth_failures: 0,
                auth_probe_pending: false,
                quota_cooling: None,
                drain: false,
                allow_credits: false,
                generation: 0,
                token_meta: TokenMeta {
                    expires_at: Some("2099-01-01T00:00:00Z".parse().unwrap()),
                    last_refresh_at: Some("2030-01-01T00:00:00Z".parse().unwrap()),
                    refresh_uncertain: false,
                },
            };
            let tokens = Tokens {
                access_token: format!("synthetic-access-{n}"),
                refresh_token: format!("synthetic-refresh-{n}"),
                id_token: None,
            };
            store.save(&account, &tokens, None).unwrap();
        }
    }
    fn data_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1/responses", self.data)
    }
    fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/responses", self.data)
    }
    fn admin_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.admin)
    }
    fn key(&self) -> String {
        fs::read_to_string(self.path.join("client.key"))
            .unwrap()
            .trim()
            .to_owned()
    }
}

struct Core {
    child: Child,
    pid: u32,
    log: PathBuf,
}

type TestSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect_socket(home: &Home, key: &str, session: String) -> Option<TestSocket> {
    let url = format!("{}?session_id={session}", home.ws_url());
    let mut request = url.into_client_request().ok()?;
    let value = format!("Bearer {key}").parse().ok()?;
    request.headers_mut().insert("authorization", value);
    connect_async(request).await.ok().map(|(socket, _)| socket)
}
impl Core {
    fn spawn(home: &Home, takeover: bool) -> Self {
        let log = home.path.join(if takeover {
            "successor.stderr"
        } else {
            "owner.stderr"
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
        command
            .env("QB_HOME", &home.path)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(fs::File::create(&log).unwrap()));
        if takeover {
            command.arg("--takeover");
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        Self { child, pid, log }
    }
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for Core {
    fn drop(&mut self) {
        self.kill();
    }
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    at: f64,
    private: u64,
    cpu: f64,
}

fn sample(
    pid: u32,
    started: Instant,
    previous: Option<(u64, Instant)>,
) -> Option<(Sample, (u64, Instant))> {
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
        .ok()?
    };
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    let mut creation = windows::Win32::Foundation::FILETIME::default();
    let mut exit = windows::Win32::Foundation::FILETIME::default();
    let mut kernel = windows::Win32::Foundation::FILETIME::default();
    let mut user = windows::Win32::Foundation::FILETIME::default();
    let counters_ptr =
        &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS;
    let ok_mem = unsafe {
        GetProcessMemoryInfo(
            process,
            counters_ptr,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
        .is_ok()
    };
    let ok_time = unsafe {
        GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user).is_ok()
    };
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(process) };
    if !ok_mem || !ok_time {
        return None;
    }
    let ticks = |f: windows::Win32::Foundation::FILETIME| {
        (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)
    };
    let now = Instant::now();
    let total = ticks(kernel) + ticks(user);
    let cpu = previous.map_or(0.0, |(old, then)| {
        (total.saturating_sub(old) as f64 / 10_000_000.0) / now.duration_since(then).as_secs_f64()
            * 100.0
    });
    Some((
        Sample {
            at: started.elapsed().as_secs_f64(),
            private: counters.PrivateUsage as u64,
            cpu,
        },
        (total, now),
    ))
}

async fn ready(client: &Client, home: &Home, pid: u32) -> String {
    for _ in 0..300 {
        if let Ok(token) = fs::read_to_string(home.path.join("admin.token")) {
            let token = token.trim().to_owned();
            if let Ok(response) = client
                .get(home.admin_url("/admin/v1/health"))
                .bearer_auth(&token)
                .send()
                .await
                && response.status().is_success()
                && response
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["pid"].as_u64())
                    == Some(pid as u64)
            {
                return token;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("core did not become ready");
}

#[tokio::test]
#[ignore = "M7c.1 spawned-core A1 run; set M7C_SPAWNED_SECS=7200 for the acceptance run"]
async fn m7c_spawned_core_a1_and_takeover() {
    let home = Home::new().await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap();
    let mut core = Core::spawn(&home, false);
    let _admin_token = ready(&client, &home, core.pid).await;
    let duration = std::env::var("M7C_SPAWNED_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7200u64);
    let started = Instant::now();
    let mut takeover_done = false;
    let mut r7 = R7Sampler::new(0x4d_3743);
    let light = std::env::var("M7C_LIGHT").is_ok();
    let http_count = if light { 8 } else { 32 };
    let websocket_count = if light { 10 } else { 50 };
    let key = home.key();
    let ws_jobs = (0..websocket_count).map(|n| {
        let url = format!("{}?session_id=m7c-ws-{n}", home.ws_url());
        let key = key.clone();
        async move {
            let mut request = url.into_client_request().ok()?;
            let value = format!("Bearer {key}").parse().ok()?;
            request.headers_mut().insert("authorization", value);
            let (mut socket, _) = connect_async(request).await.ok()?;
            let _ = socket
                .send(Message::Text("{\"type\":\"response.create\"}".into()))
                .await;
            Some(socket)
        }
    });
    let mut sockets = join_all(ws_jobs)
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        sockets.len(),
        websocket_count,
        "not all streaming WebSockets connected"
    );
    let current_pid = Arc::new(AtomicU32::new(core.pid));
    let samples = Arc::new(Mutex::new(Vec::new()));
    let sampler_pid = Arc::clone(&current_pid);
    let sampler_rows = Arc::clone(&samples);
    let (stop_sampling, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let sampler = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        let mut pid = 0;
        let mut previous = None;
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = interval.tick() => {
                    let next_pid = sampler_pid.load(Ordering::Relaxed);
                    if next_pid != pid {
                        pid = next_pid;
                        previous = None;
                    }
                    if let Some((row, prior)) = sample(pid, started, previous) {
                        previous = Some(prior);
                        sampler_rows.lock().unwrap().push(row);
                    }
                }
            }
        }
    });
    while started.elapsed() < Duration::from_secs(duration) {
        let http_jobs = (0..http_count).map(|n| {
            let c = client.clone();
            let url = home.data_url();
            let key = key.clone();
            let (_, shape) = r7.next();
            async move {
                for attempt in 0..3 {
                    let (body, compressed) = a2_body(shape);
                    let mut request = c
                        .post(url.clone())
                        .bearer_auth(key.clone())
                        .header("session-id", format!("m7c-a1-{n}-{attempt}"))
                        .header("content-type", "application/json")
                        .body(body);
                    if compressed {
                        request = request.header("content-encoding", "zstd");
                    }
                    match request.send().await {
                        Ok(response) => {
                            let status = response.status().as_u16();
                            let _ = response.bytes().await;
                            if status == 200 {
                                return status;
                            }
                        }
                        Err(error) => {
                            if attempt == 2 {
                                eprintln!("synthetic request {n} final transport error: {error}");
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                0
            }
        });
        let results = join_all(http_jobs).await;
        if !results.iter().all(|v| *v == 200) {
            eprintln!("HTTP result statuses: {results:?}");
            eprintln!(
                "core log: {}",
                fs::read_to_string(&core.log).unwrap_or_default()
            );
            panic!("spawned core request failed");
        }
        for socket in &mut sockets {
            let _ = socket
                .send(Message::Text("{\"type\":\"response.create\"}".into()))
                .await;
        }
        if !takeover_done && started.elapsed() > Duration::from_secs(duration / 2) {
            let successor = Core::spawn(&home, true);
            let _ = ready(&client, &home, successor.pid).await;
            let deadline = Instant::now() + Duration::from_secs(30);
            while core.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(
                core.child.try_wait().unwrap().is_some(),
                "takeover predecessor did not exit before drain deadline"
            );
            core = successor;
            current_pid.store(core.pid, Ordering::Relaxed);
            takeover_done = true;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    for socket in &mut sockets {
        let _ = socket.close(None).await;
    }
    let recovery = std::env::var("M7C_RECOVERY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300u64);
    tokio::time::sleep(Duration::from_secs(recovery)).await;
    let _ = stop_sampling.send(());
    let _ = sampler.await;
    let samples = samples.lock().unwrap().clone();
    let evidence =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/reports/M7c-evidence");
    fs::create_dir_all(&evidence).unwrap();
    let run_name = std::env::var("M7C_RUN_NAME").unwrap_or_else(|_| "spawned-core".into());
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(evidence.join(format!("{run_name}-samples.jsonl")))
        .unwrap();
    for row in &samples {
        writeln!(
            file,
            "{}",
            serde_json::to_string(
                &json!({"elapsed_secs":row.at,"private_bytes":row.private,"cpu_percent":row.cpu})
            )
            .unwrap()
        )
        .unwrap();
    }
    assert!(takeover_done, "takeover did not run");
    assert!(samples.iter().map(|s| s.private).max().unwrap_or(0) <= 512 * 1024 * 1024);
}

#[tokio::test]
#[ignore = "M7c.1 spawned-core 300 idle WebSocket measurement"]
async fn m7c_spawned_core_300_idle_websockets() {
    let home = Home::new().await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let mut core = Core::spawn(&home, false);
    let _ = ready(&client, &home, core.pid).await;
    let key = home.key();
    let sockets = join_all((0..300).map(|n| {
        let key = key.clone();
        let home = &home;
        async move { connect_socket(home, &key, format!("m7c-idle-{n}")).await }
    }))
    .await;
    let mut sockets = sockets.into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(sockets.len(), 300, "not all idle WebSockets connected");
    let started = Instant::now();
    let first = sample(core.pid, started, None).expect("initial child sample");
    tokio::time::sleep(Duration::from_secs(10)).await;
    let second = sample(core.pid, started, Some(first.1)).expect("second child sample");
    let evidence =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/reports/M7c-evidence");
    fs::create_dir_all(&evidence).unwrap();
    fs::write(
        evidence.join("spawned-300-idle.json"),
        serde_json::to_vec_pretty(&json!({
            "pairs": sockets.len(),
            "private_bytes": second.0.private,
            "cpu_percent": second.0.cpu,
            "target_private_bytes": 128 * 1024 * 1024,
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(second.0.private <= 128 * 1024 * 1024);
    for socket in &mut sockets {
        let _ = socket.close(None).await;
    }
    core.kill();
}

#[tokio::test]
#[ignore = "M7c.1 spawned-core idle 5 minute measurement"]
async fn m7c_spawned_core_idle_five_minutes() {
    let home = Home::new().await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let mut core = Core::spawn(&home, false);
    let _ = ready(&client, &home, core.pid).await;
    let seconds = std::env::var("M7C_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300u64);
    let started = Instant::now();
    let mut previous = None;
    let mut rows = Vec::new();
    while started.elapsed() < Duration::from_secs(seconds) {
        if let Some((row, prior)) = sample(core.pid, started, previous) {
            previous = Some(prior);
            rows.push(row);
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let evidence =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/reports/M7c-evidence");
    fs::create_dir_all(&evidence).unwrap();
    fs::write(
        evidence.join("spawned-idle-5m.json"),
        serde_json::to_vec_pretty(
            &rows
                .iter()
                .map(|row| {
                    json!({
                        "elapsed_secs": row.at,
                        "private_bytes": row.private,
                        "cpu_percent": row.cpu,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(rows.iter().map(|row| row.private).max().unwrap_or(0) <= 40 * 1024 * 1024);
    assert!(rows.iter().map(|row| row.cpu).sum::<f64>() / rows.len().max(1) as f64 <= 1.0);
    core.kill();
}
