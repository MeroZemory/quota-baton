//! Two real cores per scenario; only disposable homes and numeric loopback fakes.
use futures_util::{SinkExt, StreamExt};
use qb_core::accounts::{
    Account, AccountStore, Health, Identity, Provider, Source, StoreError, TokenMeta, Tokens,
    UserState,
};
use qb_testkit::websocket::{Message as UpMessage, WebSocketUpstream};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use reqwest::{Client, StatusCode};
use std::{
    collections::BTreeMap,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex as TokioMutex, oneshot},
    task::JoinHandle,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const HARD: Duration = Duration::from_secs(60);
static SERIAL: OnceLock<TokioMutex<()>> = OnceLock::new();

// The outer timeout includes setup, polling, streaming, CLI waits, and cleanup.
macro_rules! e2e {
    ($name:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout(HARD, async {
                let _serial = SERIAL
                    .get_or_init(|| TokioMutex::const_new(()))
                    .lock()
                    .await;
                $body
            })
            .await
            .expect(concat!(stringify!($name), " exceeded hard timeout"));
        }
    };
}

fn client() -> Client {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(12))
        .build()
        .unwrap()
}
fn safe_url(url: &str) {
    let url = reqwest::Url::parse(url).unwrap();
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert!(!matches!(url.port(), None | Some(10100 | 10101)));
}

struct Home {
    _root: TempDir,
    path: PathBuf,
    data: u16,
    admin: u16,
    data_base: String,
    usage: ScriptedUpstream,
    token: ScriptedUpstream,
    drain_secs: u64,
}
impl Home {
    async fn new(data_base: &str) -> Self {
        safe_url(data_base);
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/takeover-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        // Hold both allocations until both distinct ports have been obtained.
        let data = TcpListener::bind("127.0.0.1:0").unwrap();
        let admin = TcpListener::bind("127.0.0.1:0").unwrap();
        let usage = ScriptedUpstream::start().await.unwrap();
        let token = ScriptedUpstream::start().await.unwrap();
        safe_url(&usage.url);
        safe_url(&token.url);
        for _ in 0..100 {
            usage.state.push(ScriptedResponse::json(StatusCode::OK,
                r#"{"plan_type":"plus","rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_at":4102444800},"secondary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_at":4102444800}}}"#));
        }
        let home = Self {
            path: root.path().to_owned(),
            _root: root,
            data: data.local_addr().unwrap().port(),
            admin: admin.local_addr().unwrap().port(),
            data_base: data_base.into(),
            usage,
            token,
            drain_secs: 20,
        };
        assert!(!matches!(home.data, 10100 | 10101));
        assert!(!matches!(home.admin, 10100 | 10101));
        home.write_config();
        home
    }
    fn write_config(&self) {
        fs::write(self.path.join("config.toml"), format!(
            "[server]\ndata_port={}\nadmin_port={}\ndrain_timeout_secs={}\n[upstream]\nanthropic_base='{}'\ncodex_base='{}'\nclaude_usage_base='{}'\ncodex_usage_base='{}'\nclaude_token_endpoint='{}/oauth/token'\ncodex_token_endpoint='{}/oauth/token'\n",
            self.data, self.admin, self.drain_secs, self.data_base, self.data_base,
            self.usage.url, self.usage.url, self.token.url, self.token.url)).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
        command
            .env("QB_HOME", &self.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        command
    }
    fn admin_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.admin)
    }
    fn data_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1/responses", self.data)
    }
    fn key(&self) -> String {
        fs::read_to_string(self.path.join("client.key"))
            .unwrap()
            .trim()
            .into()
    }
}

// A thread independent of the Tokio runtime also kills the child at its deadline.
// Drop wakes and joins that thread; there are no external taskkill/CLI processes.
struct Core {
    child: Arc<Mutex<Child>>,
    pid: u32,
    stop: Arc<(Mutex<bool>, Condvar)>,
    watchdog: Option<std::thread::JoinHandle<()>>,
    log: PathBuf,
}
impl Core {
    fn spawn(home: &Home, takeover: bool) -> Self {
        let mut command = home.command();
        command.arg("serve");
        if takeover {
            command.arg("--takeover");
        }
        Self::from_command(
            command,
            home.path.join(if takeover {
                "successor.stderr"
            } else {
                "owner.stderr"
            }),
        )
    }
    fn from_command(mut command: Command, log: PathBuf) -> Self {
        command.stderr(Stdio::from(fs::File::create(&log).unwrap()));
        let child = command.spawn().unwrap();
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = stop.clone();
        let watched = child.clone();
        let watchdog = std::thread::spawn(move || {
            let (lock, cv) = &*signal;
            let (stopped, result) = cv
                .wait_timeout_while(
                    lock.lock().unwrap(),
                    HARD + Duration::from_secs(2),
                    |stopped| !*stopped,
                )
                .unwrap();
            if result.timed_out() && !*stopped {
                let _ = watched.lock().unwrap().kill();
            }
        });
        Self {
            child,
            pid,
            stop,
            watchdog: Some(watchdog),
            log,
        }
    }
    fn status(&self) -> Option<ExitStatus> {
        self.child.lock().unwrap().try_wait().unwrap()
    }
    async fn wait(&self, limit: Duration) -> ExitStatus {
        tokio::time::timeout(limit, async {
            loop {
                if let Some(status) = self.status() {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("pid {} exit timeout: {}", self.pid, self.stderr()))
    }
    fn stderr(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}
impl Drop for Core {
    fn drop(&mut self) {
        {
            let mut child = self.child.lock().unwrap();
            let _ = child.kill();
        }
        let (lock, cv) = &*self.stop;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.join().unwrap();
        }
        // kill has already been issued. Never block a runtime thread on wait().
        let end = Instant::now() + Duration::from_secs(1);
        while Instant::now() < end {
            if self
                .child
                .lock()
                .unwrap()
                .try_wait()
                .ok()
                .flatten()
                .is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition did not become true");
}
async fn token(home: &Home) -> String {
    until(|| fs::read_to_string(home.path.join("admin.token")).is_ok_and(|s| !s.trim().is_empty()))
        .await;
    fs::read_to_string(home.path.join("admin.token"))
        .unwrap()
        .trim()
        .into()
}
async fn ready(client: &Client, home: &Home, token: &str, pid: u32) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(response) = client
                .get(home.admin_url("/admin/v1/health"))
                .bearer_auth(token)
                .timeout(Duration::from_millis(250))
                .send()
                .await
                && response.status().is_success()
                && let Ok(value) = response.json::<serde_json::Value>().await
                && value["pid"].as_u64() == Some(pid as u64)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("requested core did not become healthy");
}
async fn start(home: &Home, client: &Client) -> (Core, String) {
    let core = Core::spawn(home, false);
    let token = token(home).await;
    ready(client, home, &token, core.pid).await;
    (core, token)
}

// Save real encrypted synthetic OAuth credentials under the home lease, before A.
fn accounts(home: &Home, count: usize) -> Vec<String> {
    let store = AccountStore::acquire(&home.path).unwrap();
    (0..count)
        .map(|i| {
            let email = format!("synthetic-{i}@example.invalid");
            let identity = Identity::Codex {
                account_id: format!("synthetic-{i}"),
                user_id: None,
                email: email.clone(),
            };
            let account = Account {
                schema: 1,
                id: identity.id(),
                provider: Provider::Codex,
                label: format!("fixture-{i}"),
                email,
                plan: None,
                identity,
                source: Source::Oauth,
                user_state: UserState::Active,
                health: Health::Ready,
                auth_failures: 0,
                payment_failures: 0,
                payment_until: None,
                auth_probe_pending: false,
                quota_cooling: None,
                drain: false,
                allow_credits: false,
                quota_credit_releasable: false,
                generation: 0,
                token_meta: TokenMeta {
                    expires_at: Some("2099-01-01T00:00:00Z".parse().unwrap()),
                    last_refresh_at: Some("2030-01-01T00:00:00Z".parse().unwrap()),
                    refresh_uncertain: false,
                },
            };
            let tokens = Tokens {
                access_token: format!("synthetic-access-{i}"),
                refresh_token: format!("synthetic-refresh-{i}"),
                id_token: None,
            };
            store.save(&account, &tokens, None).unwrap().id
        })
        .collect()
}
fn request(client: &Client, home: &Home, session: &str) -> reqwest::RequestBuilder {
    client
        .post(home.data_url())
        .bearer_auth(home.key())
        .header("session-id", session)
        .json(&serde_json::json!({"model":"fixture","input":[],"stream":true}))
}
async fn data_account(client: &Client, home: &Home, session: &str) -> String {
    let response = request(client, home, session).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let account = response.headers()["x-quota-baton-account"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    account
}
fn ledger(home: &Home) -> Vec<serde_json::Value> {
    fs::read_dir(home.path.join("ledger"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .flat_map(|e| {
            fs::read_to_string(e.path())
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect::<Vec<_>>()
        })
        .collect()
}
fn phase(home: &Home, pid: u32, phase: &str) -> bool {
    ledger(home)
        .iter()
        .any(|row| row["event"] == "takeover" && row["pid"] == pid && row["phase"] == phase)
}
async fn drain(client: &Client, home: &Home, token: &str) {
    let response = client
        .post(home.admin_url("/admin/v1/lifecycle/drain"))
        .bearer_auth(token)
        .json(&serde_json::json!({"drain_timeout_secs":home.drain_secs}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["lease_released"],
        true
    );
}
fn lease_busy(home: &Home) {
    assert!(matches!(
        AccountStore::acquire(&home.path),
        Err(StoreError::LeaseBusy)
    ));
}

struct AbortTask<T>(Option<JoinHandle<T>>);
impl<T> AbortTask<T> {
    async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        self.0.take().unwrap().await
    }
}
impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

// A raw synthetic HTTP fake supplies headers now and releases the complete 401
// body only when the test signals it. This exercises production's commit hold.
struct Delayed401 {
    url: String,
    entered: Option<oneshot::Receiver<()>>,
    release: Option<oneshot::Sender<()>>,
    task: AbortTask<()>,
}
impl Delayed401 {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        safe_url(&url);
        let (entered_tx, entered) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 2048];
            loop {
                let n = socket.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&buffer[..n]);
                if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let body = b"{\"error\":{\"type\":\"invalid_request_error\",\"code\":\"invalid_api_key\",\"message\":\"synthetic current token 401\"}}";
            socket.write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nX-Synthetic-401: unchanged\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            entered_tx.send(()).unwrap();
            released.await.unwrap();
            socket.write_all(body).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        Self {
            url,
            entered: Some(entered),
            release: Some(release),
            task: AbortTask(Some(task)),
        }
    }
}
type Snapshot = BTreeMap<PathBuf, (Vec<u8>, SystemTime)>;
fn snapshot(home: &Home) -> Snapshot {
    fn collect(path: &Path, out: &mut Snapshot) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::metadata(&path).unwrap();
            if metadata.is_dir() {
                collect(&path, out);
            } else {
                out.insert(
                    path.clone(),
                    (fs::read(&path).unwrap(), metadata.modified().unwrap()),
                );
            }
        }
    }
    let mut result = Snapshot::new();
    collect(&home.path.join("accounts"), &mut result);
    collect(&home.path.join("state"), &mut result);
    assert!(!result.is_empty());
    result
}

e2e!(
    takeover_hands_over_lease_and_port_without_refusing_requests,
    {
        let upstream = ScriptedUpstream::start().await.unwrap();
        let home = Home::new(&upstream.url).await;
        accounts(&home, 1);
        let client = client();
        let (a, token) = start(&home, &client).await;
        // Traffic begins on A before B exists and continues independently of health polling.
        let url = home.data_url();
        let key = home.key();
        let traffic_client = client.clone();
        let (first_tx, first_rx) = oneshot::channel();
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let traffic = AbortTask(Some(tokio::spawn(async move {
            let mut first = Some(first_tx);
            let mut interval = tokio::time::interval(Duration::from_millis(20));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut successes = Vec::new();
            let mut failures = Vec::new();
            loop {
                tokio::select! { _ = &mut stop_rx => break, _ = interval.tick() => {} }
                let result = traffic_client
                    .post(&url)
                    .bearer_auth(&key)
                    .header("session-id", "continuous")
                    .json(&serde_json::json!({"model":"fixture","input":[]}))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await;
                match result {
                    Ok(response) if response.status().is_success() => {
                        match response.bytes().await {
                            Ok(_) => {
                                successes.push(Instant::now());
                                if successes.len() >= 5
                                    && let Some(tx) = first.take()
                                {
                                    let _ = tx.send(());
                                }
                            }
                            Err(error) => failures.push(error.to_string()),
                        }
                    }
                    Ok(response) => failures.push(format!("HTTP {}", response.status())),
                    Err(error) => failures.push(error.to_string()),
                }
            }
            (successes, failures)
        })));
        first_rx.await.unwrap();
        let b = Core::spawn(&home, true);
        ready(&client, &home, &token, b.pid).await;
        a.wait(Duration::from_secs(5)).await;
        // With A exited, a successful data request can only be served by B.
        data_account(&client, &home, "continuous").await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        stop_tx.send(()).unwrap();
        let (successes, failures) = traffic.join().await.unwrap();
        let longest = successes
            .windows(2)
            .map(|w| w[1].duration_since(w[0]))
            .max()
            .unwrap();
        eprintln!(
            "takeover longest successful data response gap: {longest:?}; successes={}; failures={}",
            successes.len(),
            failures.len()
        );
        assert!(
            failures.is_empty(),
            "continuous data failures: {failures:?}"
        );
        assert!(successes.len() > 20);
        lease_busy(&home);
        assert!(b.status().is_none());
        for (pid, phases) in [
            (a.pid, ["drain_started", "lease_released", "exited"]),
            (b.pid, ["lease_acquired", "bindings_imported", "listening"]),
        ] {
            for name in phases {
                until(|| phase(&home, pid, name)).await;
            }
        }
    }
);

e2e!(takeover_lets_inflight_http_stream_finish_on_old_core, {
    let upstream = ScriptedUpstream::start().await.unwrap();
    let home = Home::new(&upstream.url).await;
    accounts(&home, 1);
    let client = client();
    let (a, token) = start(&home, &client).await;
    upstream.state.push(ScriptedResponse::sse(vec![
        (
            Duration::ZERO,
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"one\"}\n\n".into(),
        ),
        (
            Duration::from_secs(3),
            "data: {\"type\":\"response.completed\"}\n\n".into(),
        ),
    ]));
    let response = request(&client, &home, "long-sse").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    let b = Core::spawn(&home, true);
    ready(&client, &home, &token, b.pid).await;
    assert!(
        a.status().is_none(),
        "A exited while its SSE response was unfinished"
    );
    data_account(&client, &home, "new-on-b").await;
    let mut bytes = first.to_vec();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(bytes.as_slice(), b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"one\"}\n\ndata: {\"type\":\"response.completed\"}\n\n");
    a.wait(Duration::from_secs(5)).await;
    assert!(b.status().is_none());
});

fn ws_request(home: &Home, session: &str) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut request = format!("ws://127.0.0.1:{}/v1/responses", home.data)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", home.key()).parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("session-id", session.parse().unwrap());
    request
}
const CREATE: &str = r#"{"type":"response.create","model":"fixture","input":[]}"#;
const CREATED: &str = r#"{"type":"response.created","response":{"id":"synthetic-response"}}"#;
const DELTA: &str =
    r#"{"type":"response.output_text.delta","response_id":"synthetic-response","delta":"fixture"}"#;
const COMPLETED: &str =
    r#"{"type":"response.completed","response":{"id":"synthetic-response","status":"completed"}}"#;
e2e!(takeover_ws_active_response_finishes_then_1001, {
    let upstream = WebSocketUpstream::start().await.unwrap();
    let home = Home::new(&upstream.url).await;
    let ids = accounts(&home, 3);
    let client = client();
    let (a, token) = start(&home, &client).await;
    upstream.state.set_echo(false);
    upstream.state.push_reply([
        UpMessage::Text(CREATED.into()),
        UpMessage::Text(DELTA.into()),
    ]);
    let (mut ws, response) = connect_async(ws_request(&home, "takeover-ws"))
        .await
        .unwrap();
    let account = response.headers()["x-quota-baton-account"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(ids.contains(&account));
    ws.send(Message::Text(CREATE.into())).await.unwrap();
    assert!(matches!(ws.next().await.unwrap().unwrap(), Message::Text(text) if text == CREATED));
    assert!(matches!(ws.next().await.unwrap().unwrap(), Message::Text(text) if text == DELTA));
    // The fake emitted a delta, but no completion, before B starts.
    let b = Core::spawn(&home, true);
    ready(&client, &home, &token, b.pid).await;
    assert!(
        a.status().is_none(),
        "A exited during the active WS response"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), ws.next())
            .await
            .is_err(),
        "A closed or completed before the fake finished"
    );
    upstream
        .state
        .send(UpMessage::Text(COMPLETED.into()))
        .unwrap();
    assert!(matches!(ws.next().await.unwrap().unwrap(), Message::Text(text) if text == COMPLETED));
    let close = ws.next().await;
    let is_1001 = match &close {
        Some(Ok(Message::Close(Some(frame)))) => u16::from(frame.code) == 1001,
        _ => false,
    };
    assert!(
        is_1001,
        "expected 1001 close, got {close:?}; A stderr: {}",
        a.stderr()
    );
    a.wait(Duration::from_secs(5)).await;
    upstream.state.push_reply([
        UpMessage::Text(CREATED.into()),
        UpMessage::Text(COMPLETED.into()),
    ]);
    let (mut again, response) = connect_async(ws_request(&home, "takeover-ws"))
        .await
        .unwrap();
    assert_eq!(
        response.headers()["x-quota-baton-account"],
        account.as_str()
    );
    ws.send(Message::Close(None)).await.ok();
    again.send(Message::Text(CREATE.into())).await.unwrap();
    assert!(matches!(again.next().await.unwrap().unwrap(), Message::Text(text) if text == CREATED));
    assert!(
        matches!(again.next().await.unwrap().unwrap(), Message::Text(text) if text == COMPLETED)
    );
    again.close(None).await.unwrap();
    assert!(b.status().is_none());
    lease_busy(&home);
});

e2e!(takeover_imports_bindings, {
    let upstream = ScriptedUpstream::start().await.unwrap();
    let home = Home::new(&upstream.url).await;
    let ids = accounts(&home, 5);
    let client = client();
    let (a, token) = start(&home, &client).await;
    let mut expected = BTreeMap::new();
    for i in 0..5 {
        let session = format!("real-session-{i}");
        let account = data_account(&client, &home, &session).await;
        assert!(
            ids.contains(&account),
            "session was not bound to a real fixture account"
        );
        expected.insert(session, account);
    }
    let state: serde_json::Value = client
        .get(home.admin_url("/admin/v1/state"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["bindings"]["count"], 5);
    let b = Core::spawn(&home, true);
    ready(&client, &home, &token, b.pid).await;
    a.wait(Duration::from_secs(5)).await;
    until(|| phase(&home, b.pid, "bindings_imported")).await;
    for (session, account) in expected {
        assert_eq!(
            data_account(&client, &home, &session).await,
            account,
            "binding changed for {session}"
        );
    }
    let state: serde_json::Value = client
        .get(home.admin_url("/admin/v1/state"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["bindings"]["count"], 5);
    lease_busy(&home);
    assert!(b.status().is_none());
});

e2e!(
    routing_legacy_strict_anchor_survives_two_process_takeover,
    {
        let upstream = ScriptedUpstream::start().await.unwrap();
        let home = Home::new(&upstream.url).await;
        let ids = accounts(&home, 2);
        fs::create_dir_all(home.path.join("state")).unwrap();
        let id = qb_core::routing::session_id("codex:held");
        let policy = serde_json::json!({"schema":1,"revision":9,"projects":{},"sessions":{
        id:{"project":null,"baseline":{"mode":"rebalance","reset_wait_secs":60},
        "explicit":{"mode":"strict","reset_wait_secs":0},"anchor":ids[0],"movements":[]}}});
        fs::write(
            home.path.join("state/routing-policies.json"),
            serde_json::to_vec(&policy).unwrap(),
        )
        .unwrap();
        let client = client();
        let (a, token) = start(&home, &client).await;
        assert_eq!(
            data_account(&client, &home, "held:agent:independent").await,
            ids[0]
        );
        assert_eq!(client.post(home.admin_url("/admin/v1/routing")).bearer_auth(&token)
        .json(&serde_json::json!({"kind":"project","name":"unrelated","settings":{"mode":"rebalance","reset_wait_secs":0}}))
        .send().await.unwrap().status(),StatusCode::OK);
        let b = Core::spawn(&home, true);
        ready(&client, &home, &token, b.pid).await;
        a.wait(Duration::from_secs(5)).await;
        until(|| phase(&home, b.pid, "bindings_imported")).await;
        assert_eq!(
            data_account(&client, &home, "held:agent:independent").await,
            ids[0]
        );
        for request in upstream.state.requests() {
            assert_eq!(
                request.headers["authorization"],
                "Bearer synthetic-access-0"
            );
            assert_eq!(request.headers["chatgpt-account-id"], "synthetic-0");
        }
        let response = client
            .post(home.admin_url(&format!("/admin/v1/accounts/{}/pause", ids[0])))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let before = upstream.state.requests().len();
        let response = request(&client, &home, "held:agent:independent")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        response.bytes().await.unwrap();
        assert_eq!(upstream.state.requests().len(), before);
        lease_busy(&home);
    }
);

e2e!(drain_stops_refresh_and_relays_401_unchanged, {
    let mut delayed = Delayed401::start().await;
    let mut home = Home::new(&delayed.url).await;
    let ids = accounts(&home, 1);
    let id = &ids[0];
    let client = client();
    let (a, token) = start(&home, &client).await;
    let pending = AbortTask(Some(tokio::spawn(
        request(&client, &home, "owned-current-401").send(),
    )));
    delayed.entered.take().unwrap().await.unwrap();
    drain(&client, &home, &token).await;
    until(|| phase(&home, a.pid, "lease_released")).await;
    assert!(a.status().is_none());
    assert!(home.token.state.requests().is_empty());
    // Only the test owns the released lease here. No A writer, and B has not
    // been spawned. Advance synthetic metadata across the refresh-ahead point.
    let store = AccountStore::acquire(&home.path).unwrap();
    let account_path = home.path.join("accounts").join(format!("{id}.json"));
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&account_path).unwrap()).unwrap();
    record["token_meta"]["last_refresh_at"] = serde_json::json!("1999-01-01T00:00:00Z");
    record["token_meta"]["expires_at"] = serde_json::json!("2000-01-01T00:00:00Z");
    fs::write(&account_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    let due = store.load(id).unwrap();
    assert_eq!(due.source, Source::Oauth);
    assert_eq!(due.health, Health::Ready);
    assert_eq!(
        store.tokens(&due).unwrap().access_token,
        "synthetic-access-0"
    );
    drop(store);
    let before = snapshot(&home);
    assert!(
        before
            .keys()
            .any(|p| p.starts_with(home.path.join("state"))),
        "no state file was captured"
    );
    delayed.release.take().unwrap().send(()).unwrap();
    let response = pending.join().await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()["x-synthetic-401"], "unchanged");
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"{\"error\":{\"type\":\"invalid_request_error\",\"code\":\"invalid_api_key\",\"message\":\"synthetic current token 401\"}}");
    a.wait(Duration::from_secs(5)).await;
    assert!(
        home.token.state.requests().is_empty(),
        "A consumed the owned refresh token during drain"
    );
    assert_eq!(
        snapshot(&home),
        before,
        "A changed account/state bytes or mtimes after lease release"
    );
    let metadata = AccountStore::list_metadata(&home.path).unwrap();
    assert_eq!(metadata[0].health, due.health);
    // Now the immutability measurement is over; allow B to write and refresh.
    let b_data = ScriptedUpstream::start().await.unwrap();
    home.data_base = b_data.url.clone();
    home.write_config();
    home.token.state.push(ScriptedResponse::json(StatusCode::OK,
        r#"{"access_token":"synthetic-rotated-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600,"token_type":"Bearer"}"#));
    let b = Core::spawn(&home, true);
    ready(&client, &home, &token, b.pid).await;
    until(|| !home.token.state.requests().is_empty()).await;
    data_account(&client, &home, "owned-current-401").await;
    let refreshes = home.token.state.requests();
    assert_eq!(refreshes.len(), 1);
    assert_eq!(refreshes[0].path_and_query, "/oauth/token");
    let form = String::from_utf8(refreshes[0].body.to_vec()).unwrap();
    assert!(form.contains("grant_type=refresh_token"));
    assert!(form.contains("refresh_token=synthetic-refresh-0"));
    assert!(b_data.state.requests().iter().any(|r| {
        r.headers
            .get("authorization")
            .is_some_and(|h| h == "Bearer synthetic-rotated-access")
    }));
    let refreshed = AccountStore::list_metadata(&home.path).unwrap();
    assert!(refreshed[0].generation > due.generation);
    assert!(refreshed[0].token_meta.last_refresh_at > due.token_meta.last_refresh_at);
    assert!(!refreshed[0].token_meta.refresh_uncertain);
    assert_eq!(refreshed[0].health, Health::Ready);
    lease_busy(&home);
    // Retain the gated fake until all its writes are completed.
    assert!(delayed.task.0.as_ref().unwrap().is_finished());
    assert!(b.status().is_none());
});

e2e!(second_serve_without_takeover_refuses_while_lease_held, {
    let upstream = ScriptedUpstream::start().await.unwrap();
    let home = Home::new(&upstream.url).await;
    let client = client();
    let (a, token) = start(&home, &client).await;
    let b = Core::spawn(&home, false);
    let status = b.wait(Duration::from_secs(5)).await;
    assert!(!status.success());
    assert!(
        b.stderr()
            .contains("다른 코어가 계정 저장소를 사용 중입니다")
    );
    ready(&client, &home, &token, a.pid).await;
    assert!(a.status().is_none());
    lease_busy(&home);
});

e2e!(drain_timeout_forces_exit, {
    let upstream = ScriptedUpstream::start().await.unwrap();
    let mut home = Home::new(&upstream.url).await;
    home.drain_secs = 2;
    home.write_config();
    accounts(&home, 1);
    let client = client();
    let (a, token) = start(&home, &client).await;
    upstream.state.push(ScriptedResponse::sse(vec![
        (
            Duration::ZERO,
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"held\"}\n\n".into(),
        ),
        (
            Duration::from_secs(15),
            "data: {\"type\":\"response.completed\"}\n\n".into(),
        ),
    ]));
    let response = request(&client, &home, "timeout-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    stream.next().await.unwrap().unwrap();
    let began = Instant::now();
    let b = Core::spawn(&home, true);
    ready(&client, &home, &token, b.pid).await;
    assert!(a.status().is_none());
    a.wait(Duration::from_secs(5)).await;
    assert!(began.elapsed() >= Duration::from_secs(2));
    assert!(began.elapsed() < Duration::from_secs(6));
    // Keep the client stream alive throughout A's exit. Completion cannot be the
    // cause of exit, since the fake's next chunk is still delayed for 15 seconds.
    let mut completed = false;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                completed |= String::from_utf8_lossy(&bytes).contains("response.completed")
            }
            Err(_) => break,
        }
    }
    assert!(
        !completed,
        "held stream completed instead of forcing the timeout"
    );
    data_account(&client, &home, "after-timeout").await;
    assert!(b.status().is_none());
});

async fn port_case(release_port: bool) {
    let upstream = ScriptedUpstream::start().await.unwrap();
    let home = Home::new(&upstream.url).await;
    accounts(&home, 1);
    let client = client();
    let (a, token) = start(&home, &client).await;
    upstream.state.push(ScriptedResponse::sse(vec![
        (
            Duration::ZERO,
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"held\"}\n\n".into(),
        ),
        (
            Duration::from_secs(4),
            "data: {\"type\":\"response.completed\"}\n\n".into(),
        ),
    ]));
    let response = request(&client, &home, "port-held-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    stream.next().await.unwrap().unwrap();
    drain(&client, &home, &token).await;
    assert!(a.status().is_none());
    // The authoritative listener Drop has occurred; ordinary OS contention now
    // simulates a delayed/never released data listener, with no production hook.
    let holder = TcpListener::bind(("127.0.0.1", home.data)).unwrap();
    let b = Core::spawn(&home, true);
    let began = Instant::now();
    if release_port {
        until(|| phase(&home, b.pid, "lease_acquired")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(b.status().is_none());
        drop(holder);
        ready(&client, &home, &token, b.pid).await;
        assert!(began.elapsed() >= Duration::from_millis(300));
        assert!(began.elapsed() < Duration::from_secs(3));
        data_account(&client, &home, "port-retry-success").await;
        lease_busy(&home);
    } else {
        let status = b.wait(Duration::from_secs(5)).await;
        assert!(!status.success());
        assert!(began.elapsed() >= Duration::from_secs(2));
        assert!(b.stderr().contains("포트 인계 대기 시간(2초)이 지났습니다"));
        // B's failed startup must release the lease even though A still serves
        // its admitted SSE response and the unrelated holder retains the port.
        assert!(a.status().is_none());
        let recovered = AccountStore::acquire(&home.path).unwrap();
        drop(recovered);
        drop(holder);
    }
    let mut remainder = Vec::new();
    while let Some(chunk) = stream.next().await {
        remainder.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(
        remainder.as_slice(),
        b"data: {\"type\":\"response.completed\"}\n\n"
    );
    assert!(
        a.wait(Duration::from_secs(5)).await.success(),
        "A must exit normally after its existing SSE completes"
    );
}
e2e!(port_bind_retry_window, {
    port_case(true).await;
    port_case(false).await;
});
