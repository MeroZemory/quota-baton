//! Real CLI takeover retains a logical client's launch URL, without C8 identity.
#![cfg(all(windows, feature = "testkit", debug_assertions))]

use futures_util::{SinkExt, StreamExt};
use qb_core::accounts::{AccountStore, import};
use qb_testkit::{ScriptedResponse, ScriptedState, ScriptedUpstream, websocket::WebSocketUpstream};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const HARD: Duration = Duration::from_secs(60);
const CLIENT: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA";
const ADMIN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const CREATE: &str = r#"{"type":"response.create","model":"fixture","input":[]}"#;
const CREATED: &str = r#"{"type":"response.created","response":{"id":"synthetic-response"}}"#;
const COMPLETED: &str =
    r#"{"type":"response.completed","response":{"id":"synthetic-response","status":"completed"}}"#;

// Never format transport errors: they can contain the secret launch URL.
fn checked<T, E>(result: Result<T, E>, code: &'static str) -> T {
    result.unwrap_or_else(|_| panic!("{code}"))
}

fn local_port(url: &str) -> u16 {
    let url = checked(reqwest::Url::parse(url), "fixture_url");
    assert!(
        url.scheme() == "http" && url.host_str() == Some("127.0.0.1"),
        "fixture_loopback"
    );
    let port = url.port().expect("fixture_port");
    assert!(!matches!(port, 10100 | 10101), "fixture_reserved_port");
    port
}

struct Task(JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// Production has one Codex base for catalog, POST and WS. Preserve that shape,
// dispatching only inside the synthetic upstream to the established testkit fakes.
struct Upstream {
    url: String,
    http: ScriptedState,
    ws: WebSocketUpstream,
    _http_task: Task,
    _dispatch_task: Task,
}
impl Upstream {
    async fn start() -> Self {
        let http_listener = checked(
            tokio::net::TcpListener::bind("127.0.0.1:0").await,
            "http_bind",
        );
        let http_port = checked(http_listener.local_addr(), "http_addr").port();
        let http = ScriptedState::default();
        let router = http.router().route(
            "/models",
            axum::routing::get(|| async { axum::Json(json!({"models":[{"slug":"fixture"}]})) }),
        );
        // This tiny TCP dispatcher chooses a backend per accepted connection,
        // not per HTTP request. Close ordinary HTTP connections so a later WS
        // upgrade cannot reuse a connection already assigned to the HTTP fake.
        let router = router.layer(axum::middleware::map_response(
            |mut response: axum::response::Response| async {
                response
                    .headers_mut()
                    .insert("connection", axum::http::HeaderValue::from_static("close"));
                response
            },
        ));
        let http_task = Task(tokio::spawn(async move {
            let _ = axum::serve(http_listener, router).await;
        }));
        let ws = checked(WebSocketUpstream::start().await, "ws_bind");
        let ws_port = local_port(&ws.url);
        assert!(!matches!(http_port, 10100 | 10101), "http_reserved_port");
        let listener = checked(
            tokio::net::TcpListener::bind("127.0.0.1:0").await,
            "dispatch_bind",
        );
        let url = format!("http://{}", checked(listener.local_addr(), "dispatch_addr"));
        local_port(&url);
        let dispatch_task = Task(tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut client, _)) = accepted else { break };
                        connections.spawn(async move {
                            let _ = tokio::time::timeout(HARD, async {
                                // Read only the bounded header, leaving all frame/body bytes
                                // untouched. No recording or output of routing handles.
                                let mut head = Vec::new();
                                while !head.ends_with(b"\r\n\r\n") {
                                    if head.len() == 16 * 1024 { return; }
                                    let Ok(byte) = client.read_u8().await else { return };
                                    head.push(byte);
                                }
                                let websocket = head.split(|b| *b == b'\n').any(|line| {
                                    line.iter().position(|byte| *byte == b':').is_some_and(|colon| {
                                        line[..colon].eq_ignore_ascii_case(b"upgrade")
                                            && line[colon + 1..].trim_ascii().eq_ignore_ascii_case(b"websocket")
                                    })
                                });
                                let port = if websocket { ws_port } else { http_port };
                                let Ok(mut server) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else { return };
                                if server.write_all(&head).await.is_err() { return; }
                                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                            }).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
            // JoinSet drop aborts every accepted connection on fixture teardown.
        }));
        Self {
            url,
            http,
            ws,
            _http_task: http_task,
            _dispatch_task: dispatch_task,
        }
    }
}

// Independent deadline also covers synchronous setup/panics and stalled runtimes.
// Drop kills and reaps only this exact child, before the fixture home is removed.
struct Core {
    child: Arc<Mutex<Child>>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    watchdog: Option<std::thread::JoinHandle<()>>,
    pid: u32,
}
impl Core {
    fn spawn(root: &Path, takeover: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
        command.env_clear().arg("serve");
        if takeover {
            command.arg("--takeover");
        }
        command
            .env("QB_HOME", root.join("home"))
            .env("HOME", root.join("profile"))
            .env("USERPROFILE", root.join("profile"))
            .env("APPDATA", root.join("profile/AppData/Roaming"))
            .env("LOCALAPPDATA", root.join("profile/AppData/Local"))
            .env("CODEX_HOME", root.join("profile/.codex"))
            .env("CLAUDE_CONFIG_DIR", root.join("profile/.claude"))
            .env("TEMP", root.join("temp"))
            .env("TMP", root.join("temp"))
            .env("TOKIO_WORKER_THREADS", "2")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for name in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
        let child = checked(command.spawn(), "core_spawn");
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let watched = child.clone();
        let signal = stop.clone();
        let watchdog = std::thread::spawn(move || {
            let (lock, cv) = &*signal;
            let (stopped, result) = cv
                .wait_timeout_while(lock.lock().unwrap(), HARD + Duration::from_secs(2), |s| !*s)
                .unwrap();
            if result.timed_out() && !*stopped {
                let mut child = watched.lock().unwrap();
                let _ = child.kill();
                let _ = child.wait();
            }
        });
        Self {
            child,
            stop,
            watchdog: Some(watchdog),
            pid,
        }
    }
    fn exited(&self) -> bool {
        checked(self.child.lock().unwrap().try_wait(), "core_status").is_some()
    }
}
impl Drop for Core {
    fn drop(&mut self) {
        {
            let mut child = self.child.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
        }
        let (lock, cv) = &*self.stop;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

async fn ready(client: &Client, admin: &str, core: &Core) {
    checked(
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                assert!(!core.exited(), "core_early_exit");
                if let Ok(response) = client
                    .get(format!("{admin}/admin/v1/health"))
                    .bearer_auth(ADMIN)
                    .timeout(Duration::from_millis(250))
                    .send()
                    .await
                    && response.status() == StatusCode::OK
                    && let Ok(value) = response.json::<Value>().await
                    && value["pid"].as_u64() == Some(u64::from(core.pid))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await,
        "core_ready_timeout",
    );
}

async fn routing(client: &Client, admin: &str) -> Value {
    let response = checked(
        client
            .get(format!("{admin}/admin/v1/routing"))
            .bearer_auth(ADMIN)
            .send()
            .await,
        "routing_transport",
    );
    assert_eq!(response.status(), StatusCode::OK, "routing_status");
    checked(response.json().await, "routing_json")
}

fn posts(upstream: &Upstream) -> usize {
    upstream
        .http
        .requests()
        .iter()
        .filter(|r| r.method == reqwest::Method::POST)
        .count()
}

async fn post(client: &Client, upstream: &Upstream, old_url: &str, account: &str) {
    let before = posts(upstream);
    upstream
        .http
        .push(ScriptedResponse::json(StatusCode::OK, r#"{"ok":true}"#));
    // Deliberately no session-id, conversation id, prompt cache key or C8 body.
    let response = checked(
        client
            .post(format!("{old_url}/responses"))
            .bearer_auth(CLIENT)
            .json(&json!({"model":"fixture","input":[],"stream":false}))
            .send()
            .await,
        "post_transport",
    );
    assert_eq!(response.status(), StatusCode::OK, "post_status");
    assert!(
        response
            .headers()
            .get("x-quota-baton-account")
            .is_some_and(|v| v == account),
        "post_account"
    );
    let body: Value = checked(response.json().await, "post_json");
    assert!(body["ok"] == true, "post_upstream_ack");
    assert_eq!(posts(upstream), before + 1, "post_upstream_count");
    let requests = upstream.http.requests();
    let request = requests
        .iter()
        .rev()
        .find(|r| r.method == reqwest::Method::POST)
        .expect("post_record");
    assert!(
        request
            .headers
            .get("authorization")
            .is_some_and(|v| v == "Bearer takeover-synthetic-access"),
        "post_upstream_account"
    );
    let body: Value = checked(serde_json::from_slice(&request.body), "post_record_json");
    assert!(body["model"] == "fixture", "post_upstream_model");
}

async fn managed_model_gate(client: &Client, upstream: &Upstream, old_url: &str) {
    let before = posts(upstream);
    let response = checked(
        client
            .post(format!("{old_url}/responses"))
            .bearer_auth(CLIENT)
            .json(&json!({"model":"unsupported-fixture","input":[]}))
            .send()
            .await,
        "model_gate_transport",
    );
    assert_eq!(response.status(), StatusCode::CONFLICT, "model_gate_status");
    let body: Value = checked(response.json().await, "model_gate_json");
    assert!(
        body["error"]["type"] == "model_not_supported",
        "model_gate_code"
    );
    assert_eq!(posts(upstream), before, "model_gate_no_dispatch");
}

#[tokio::test]
async fn actual_takeover_retains_launch_without_c8_for_http_and_ws() {
    checked(tokio::time::timeout(HARD, async {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/takeover-launch-tests");
        checked(fs::create_dir_all(&base), "sandbox_create");
        let root = checked(tempfile::tempdir_in(base), "sandbox_private");
        let home = root.path().join("home");
        let cpa = root.path().join("synthetic-cpa");
        for path in [&home, &cpa, &root.path().join("profile"), &root.path().join("temp")] {
            checked(fs::create_dir_all(path), "fixture_directory");
        }
        checked(fs::write(cpa.join("fixture.json"), checked(serde_json::to_vec(&json!({
            "type":"codex", "email":"takeover@example.invalid", "account_id":"takeover-fixture",
            "access_token":"takeover-synthetic-access", "refresh_token":"takeover-synthetic-refresh",
            "expired":"2099-01-01T00:00:00Z", "last_refresh":"2026-10-30T00:00:00Z"
        })), "account_json")), "account_write");
        let account = {
            let store = checked(AccountStore::acquire(&home), "fixture_lease");
            checked(import::run(&store, &cpa, &["codex:takeover@example.invalid".into()], false), "fixture_import");
            let accounts = checked(store.list(), "fixture_accounts");
            assert_eq!(accounts.len(), 1, "fixture_account_count");
            accounts[0].id.clone()
        };
        checked(fs::write(home.join("client.key"), CLIENT), "client_write");
        checked(fs::write(home.join("admin.token"), ADMIN), "admin_write");
        let upstream = Upstream::start().await;
        let usage = checked(ScriptedUpstream::start().await, "usage_bind");
        let token = checked(ScriptedUpstream::start().await, "token_bind");
        local_port(&usage.url);
        local_port(&token.url);
        for _ in 0..16 {
            usage.state.push(ScriptedResponse::json(StatusCode::OK,
                r#"{"plan_type":"plus","rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_at":4102444800},"secondary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_at":4102444800}}}"#));
        }
        let data = checked(TcpListener::bind("127.0.0.1:0"), "data_bind");
        let admin = checked(TcpListener::bind("127.0.0.1:0"), "admin_bind");
        let data_port = checked(data.local_addr(), "data_addr").port();
        let admin_port = checked(admin.local_addr(), "admin_addr").port();
        let admin_url = format!("http://127.0.0.1:{admin_port}");
        local_port(&admin_url);
        local_port(&format!("http://127.0.0.1:{data_port}"));
        checked(fs::write(home.join("config.toml"), format!(
            "[server]\ndata_port={data_port}\nadmin_port={admin_port}\ndrain_timeout_secs=2\n[dashboard]\nresolve_claude_project=false\n[upstream]\ncodex_base='{0}'\nanthropic_base='{0}'\ncodex_usage_base='{1}'\nclaude_usage_base='{1}'\ncodex_token_endpoint='{2}/oauth/token'\nclaude_token_endpoint='{2}/oauth/token'\n",
            upstream.url, usage.url, token.url)), "config_write");
        let client = checked(Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5)).build(), "client_build");
        drop((data, admin));
        let a = Core::spawn(root.path(), false);
        ready(&client, &admin_url, &a).await;
        let response = checked(client.post(format!("{admin_url}/admin/v1/launches"))
            .bearer_auth(ADMIN).json(&json!({"model":"fixture","model_is_explicit":true,
                "account_pref":account,"resume_id":null,"move_for_model":false,"urgent":true}))
            .send().await, "issue_transport");
        assert_eq!(response.status(), StatusCode::OK, "issue_status");
        let launch: qb_proto::admin_launch::LaunchReply = checked(response.json().await, "issue_json");
        assert!(launch.account == account && launch.model.as_deref() == Some("fixture"), "issue_metadata");
        assert_eq!(local_port(&launch.base_url), data_port, "launch_listener");
        let old_url = launch.base_url;
        post(&client, &upstream, &old_url, &account).await;
        managed_model_gate(&client, &upstream, &old_url).await;
        let before = routing(&client, &admin_url).await;
        let rows = before["observed_sessions"].as_array().expect("routing_rows");
        assert_eq!(rows.len(), 1, "routing_single_launch");
        assert!(rows[0]["account"] == account && rows[0]["policy"]["settings"]["on_5h_limit"] == "move", "launch_policy");

        // Exercises both production adapters: negotiated lifecycle export in A,
        // bounded main.rs decode/import in B. The test never reads the snapshot.
        let b = Core::spawn(root.path(), true);
        ready(&client, &admin_url, &b).await;
        post(&client, &upstream, &old_url, &account).await;
        managed_model_gate(&client, &upstream, &old_url).await;
        let after = routing(&client, &admin_url).await;
        assert!(after["observed_sessions"] == before["observed_sessions"], "retained_account_policy");

        upstream.ws.state.set_echo(false);
        upstream.ws.state.push_reply([Message::Text(CREATED.into()), Message::Text(COMPLETED.into())]);
        let ws_url = format!("ws://{}/responses", old_url.strip_prefix("http://").expect("launch_scheme"));
        let mut request = checked(ws_url.into_client_request(), "ws_request");
        request.headers_mut().insert("authorization", checked(format!("Bearer {CLIENT}").parse(), "ws_auth"));
        // Again no C8 headers or session metadata; the old routing URL is enough.
        let (mut ws, response) = match tokio_tungstenite::connect_async(request).await {
            Ok(upgrade) => upgrade,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                let http_requests = upstream.http.requests();
                let upgrade_seen = http_requests.iter().any(|request| request.headers.contains_key("upgrade"));
                panic!("ws_http_status_{}_http_upgrade_seen_{}_ws_count_{}", response.status().as_u16(), upgrade_seen, upstream.ws.state.requests().len());
            }
            Err(tokio_tungstenite::tungstenite::Error::Io(error)) => {
                panic!("ws_io_kind_{:?}", error.kind());
            }
            Err(_) => panic!("ws_upgrade_protocol"),
        };
        assert_eq!(response.status().as_u16(), 101, "ws_upgrade");
        assert!(response.headers().get("x-quota-baton-account").is_some_and(|v| v == account.as_str()), "ws_account");
        checked(ws.send(Message::Text(CREATE.into())).await, "ws_send");
        for expected in [CREATED, COMPLETED] {
            let message = checked(ws.next().await.expect("ws_ack_missing"), "ws_ack_read");
            assert!(matches!(message, Message::Text(text) if text == expected), "ws_upstream_ack");
        }
        checked(ws.close(None).await, "ws_close");
        let requests = upstream.ws.state.requests();
        assert_eq!(requests.len(), 1, "ws_upstream_count");
        assert!(requests[0].headers.get("authorization").is_some_and(|v| v == "Bearer takeover-synthetic-access"), "ws_upstream_account");
        assert!(upstream.ws.state.messages().iter().any(|m| matches!(m, Message::Text(text) if text.as_str() == CREATE)), "ws_upstream_model");
        checked(tokio::time::timeout(Duration::from_secs(5), async {
            while !a.exited() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await, "owner_exit_timeout");
        assert!(!b.exited(), "successor_alive");
        assert!(token.state.requests().is_empty(), "no_refresh");
        drop((b, a));
        // Explicit close verifies the private home was removed after child reap.
        checked(root.close(), "fixture_remove");
    }).await, "takeover_launch_hard_timeout");
}
