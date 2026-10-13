use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use qb_core::{
    accounts::{AccountStore, import},
    config::Config,
    debug_pin::DebugPins,
    guard::Reservation,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::{CaptureOptions, Observability},
    relay::{self, RelayState},
};
use qb_testkit::websocket::{Message, WebSocketUpstream};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{
        Error,
        client::IntoClientRequest,
        protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode},
    },
};

type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
struct Harness {
    _root: tempfile::TempDir,
    home: PathBuf,
    cpa: PathBuf,
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    fake: WebSocketUpstream,
    servers: RunningServers,
    state: Arc<RelayState>,
    #[cfg(feature = "shape-capture")]
    captures: PathBuf,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}
fn fixture(version: u8) -> Value {
    json!({"type":"codex","email":"ws@example.invalid","account_id":"synthetic-ws","access_token":format!("synthetic-access-{version}"),"refresh_token":"synthetic-refresh","id_token":"synthetic-id-token","expired":"2099-01-01T00:00:00Z","last_refresh":format!("2026-10-13T00:00:0{version}Z")})
}
impl Harness {
    async fn new(idle: u64, budget: u64, close_after: bool) -> Self {
        let serial = SERIAL.lock().await;
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m3-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let cpa = root.path().join("cpa");
        let captures = root.path().join("captures");
        fs::create_dir_all(&cpa).unwrap();
        fs::create_dir_all(&captures).unwrap();
        fs::write(
            cpa.join("codex.json"),
            serde_json::to_vec(&fixture(1)).unwrap(),
        )
        .unwrap();
        let store = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match AccountStore::acquire(&home) {
                    Ok(store) => break Arc::new(store),
                    Err(qb_core::accounts::StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => panic!("synthetic store failed: {error}"),
                }
            }
        })
        .await
        .expect("synthetic credential lease did not release");
        import::run(&store, &cpa, &["codex:ws@example.invalid".into()], false).unwrap();
        let fake = WebSocketUpstream::start().await.unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.websocket.idle_close_secs = idle;
        config.guard.inflight_budget_mib = budget;
        config.upstream.codex_base = fake.url.clone();
        let capture = if cfg!(feature = "shape-capture") {
            Some(CaptureOptions {
                dir: captures.clone(),
                limit: 20,
            })
        } else {
            None
        };
        let obs = Arc::new(Observability::new(&home, capture).unwrap());
        let pins = Arc::new(DebugPins::default());
        let state = RelayState::new(store.clone(), &config, obs.clone(), pins.clone()).unwrap();
        #[cfg(feature = "phase0")]
        let state = state.with_close_after_response(close_after);
        #[cfg(not(feature = "phase0"))]
        let _ = close_after;
        let state = Arc::new(state);
        let admin = qb_core::debug_pin::routes(store.clone(), pins);
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-local-key".into(),
                admin_token: "synthetic-admin-key".into(),
            },
            relay::router(state.clone()),
            admin,
        )
        .await
        .unwrap();
        Self {
            _root: root,
            home,
            cpa,
            store,
            obs,
            fake,
            servers,
            state,
            #[cfg(feature = "shape-capture")]
            captures,
            _serial: serial,
        }
    }
    fn request(&self) -> axum::http::Request<()> {
        let mut request = format!("ws://{}/v1/responses?synthetic=1", self.servers.data_addr)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            "Bearer synthetic-local-key".parse().unwrap(),
        );
        request
    }
    async fn connect(&self) -> Client {
        connect_async_with_config(
            self.request(),
            Some(
                WebSocketConfig::default()
                    .max_frame_size(Some(192 * 1024 * 1024))
                    .max_message_size(Some(192 * 1024 * 1024)),
            ),
            true,
        )
        .await
        .unwrap()
        .0
    }
    fn ledger(&self) -> Vec<Value> {
        self.obs.flush().unwrap();
        let mut records = Vec::new();
        if let Ok(entries) = fs::read_dir(self.home.join("ledger")) {
            for entry in entries {
                let text = fs::read_to_string(entry.unwrap().path()).unwrap();
                for secret in [
                    "synthetic-access",
                    "synthetic-refresh",
                    "synthetic-id-token",
                    "synthetic-local-key",
                    "synthetic-admin-key",
                    "synthetic-conversation",
                ] {
                    assert!(!text.contains(secret));
                }
                records.extend(
                    text.lines()
                        .map(|line| serde_json::from_str::<Value>(line).unwrap()),
                );
            }
        }
        records
    }
}
async fn receive(client: &mut Client) -> Message {
    tokio::time::timeout(Duration::from_secs(15), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
async fn wait_record(h: &Harness, event: &str) {
    for _ in 0..100 {
        if h.ledger().iter().any(|v| v["event"] == event) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("missing connection event");
}

#[tokio::test]
async fn handshake_headers_are_per_hop_and_c2_rejects_before_dial() {
    let h = Harness::new(30, 384, false).await;
    let mut request = h.request();
    let local_key = request.headers()["sec-websocket-key"].clone();
    request.headers_mut().insert(
        "sec-websocket-extensions",
        "permessage-deflate".parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("originator", "synthetic-cli".parse().unwrap());
    request
        .headers_mut()
        .insert("chatgpt-account-id", "wrong-id".parse().unwrap());
    let (mut client, response) = connect_async_with_config(request, None, true)
        .await
        .unwrap();
    assert_eq!(response.status(), 101);
    assert!(!response.headers().contains_key("sec-websocket-extensions"));
    assert!(!response.headers().contains_key("sec-websocket-protocol"));
    assert!(response.headers().contains_key("x-quota-baton-account"));
    let requests = h.fake.state.requests();
    let headers = &requests[0].headers;
    assert_eq!(requests[0].path_and_query, "/responses?synthetic=1");
    assert_ne!(headers["sec-websocket-key"], local_key);
    assert_eq!(headers["authorization"], "Bearer synthetic-access-1");
    assert_eq!(headers["chatgpt-account-id"], "synthetic-ws");
    assert_eq!(headers["originator"], "synthetic-cli");
    assert!(!headers.contains_key("sec-websocket-extensions"));
    assert!(!headers.contains_key("sec-websocket-protocol"));
    client.close(None).await.unwrap();
    for (name, value, status) in [
        ("authorization", "Bearer wrong", 401),
        ("host", "evil.invalid", 421),
        ("origin", "https://evil.invalid", 403),
    ] {
        let mut request = h.request();
        request.headers_mut().insert(name, value.parse().unwrap());
        match connect_async_with_config(request, None, true).await {
            Err(Error::Http(response)) => assert_eq!(response.status().as_u16(), status),
            _ => panic!("must refuse"),
        }
    }
    assert_eq!(h.fake.state.requests().len(), 1);
    // 클라이언트가 제안한 subprotocol도 upstream으로 복사하지 않는다.
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{}/v1/responses", h.servers.data_addr))
        .bearer_auth("synthetic-local-key")
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-protocol", "synthetic-protocol")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 101);
    assert!(!response.headers().contains_key("sec-websocket-protocol"));
    assert!(
        !h.fake
            .state
            .requests()
            .last()
            .unwrap()
            .headers
            .contains_key("sec-websocket-protocol")
    );
    drop(response.upgrade().await.unwrap());
    wait_record(&h, "ws_close").await;
}

#[tokio::test]
async fn handshake_failure_rows_never_return_426_and_keep_raw_bodies() {
    let h = Harness::new(30, 384, false).await;
    for (status, body, expected) in [
        (
            429,
            r#"{"error":{"type":"rate_limit_error","resets_at":2000000000,"resets_in_seconds":60}}"#,
            429,
        ),
        (
            400,
            r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":17}}"#,
            429,
        ),
        (401, r#"{"error":{"type":"authentication_error"}}"#, 400),
        (418, r#"{"synthetic":"raw handshake body"}"#, 418),
        (426, r#"{"error":{"type":"upgrade_required"}}"#, 502),
    ] {
        let id = h.store.list().unwrap()[0].id.clone();
        h.store
            .update_metadata(&id, |a| a.health = qb_core::accounts::Health::Ready)
            .unwrap();
        h.fake.state.push_handshake(
            StatusCode::from_u16(status).unwrap(),
            body,
            HeaderMap::new(),
        );
        match connect_async_with_config(h.request(), None, true).await {
            Err(Error::Http(response)) => {
                assert_eq!(response.status().as_u16(), expected);
                assert_ne!(response.status(), 426);
                let raw = response.body().as_ref().unwrap();
                if expected == 429 {
                    let value: Value = serde_json::from_slice(raw).unwrap();
                    assert_eq!(value["error"]["type"], "usage_limit_reached");
                    assert!(value["error"]["resets_in_seconds"].as_i64().unwrap() > 0);
                } else if status == 401 {
                    assert!(String::from_utf8_lossy(raw).contains("no usable codex account"));
                } else if status != 426 {
                    assert_eq!(raw, body.as_bytes());
                }
            }
            _ => panic!("expected handshake refusal"),
        }
    }
    let mut account = h.store.list().unwrap().remove(0);
    account.user_state = qb_core::accounts::UserState::Disabled;
    let tokens = h.store.tokens(&account).unwrap();
    h.store
        .save(&account, &tokens, Some(account.generation))
        .unwrap();
    match connect_async_with_config(h.request(), None, true).await {
        Err(Error::Http(response)) => {
            assert_eq!(response.status(), 400);
            let text = String::from_utf8_lossy(response.body().as_ref().unwrap());
            assert!(text.contains("no usable codex account"));
            assert!(text.contains("트레이를 여세요"));
        }
        _ => panic!("expected no account"),
    }
}

#[tokio::test]
async fn messages_are_verbatim_including_interrupt_and_single_40mib_frames() {
    let h = Harness::new(30, 384, false).await;
    let mut client = h.connect().await;
    for message in [
        Message::Text("{\"type\":\"response.interrupt\",\"synthetic\":true}".into()),
        Message::Text("x".repeat(40 * 1024 * 1024).into()),
        Message::Binary(Bytes::from(vec![171; 40 * 1024 * 1024])),
    ] {
        client.send(message.clone()).await.unwrap();
        assert_eq!(receive(&mut client).await, message);
    }
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_) | Message::Binary(_)))
            .count(),
        3
    );
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn quota_error_and_terminal_frames_update_ledger_without_waiting_for_eof() {
    let h = Harness::new(30, 384, false).await;
    let mut client = h.connect().await;
    let quota = r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":25,"window_minutes":300,"reset_after_seconds":60},"secondary":{"used_percent":75,"window_minutes":10080}},"plan_type":"plus","limit_reached":false,"credits":{"balance":12}}"#;
    let completed = r#"{"type":"response.completed","response":{"usage":{"input_tokens":1}}}"#;
    h.fake
        .state
        .push_reply([Message::Text(quota.into()), Message::Text(completed.into())]);
    client
        .send(Message::Text(
            r#"{"type":"response.create","input":"synthetic-conversation"}"#.into(),
        ))
        .await
        .unwrap();
    assert_eq!(receive(&mut client).await, Message::Text(quota.into()));
    assert_eq!(receive(&mut client).await, Message::Text(completed.into()));
    client.close(None).await.unwrap();
    drop(client);
    wait_record(&h, "ws_close").await;
    let records = h.ledger();
    let response = records
        .iter()
        .find(|v| v["route"] == "GET /v1/responses" && v["outcome"] == "ok")
        .unwrap();
    assert_eq!(response["transport"], "ws");
    assert_eq!(response["outcome"], "ok");
    assert!(response["bytes_in"].as_u64().unwrap() > 0);
    assert!(response["bytes_out"].as_u64().unwrap() > 0);
    let quota: Value =
        serde_json::from_slice(&fs::read(h.home.join("state/quota.json")).unwrap()).unwrap();
    let account = h.store.list().unwrap().remove(0);
    assert_eq!(
        quota["accounts"][&account.id]["source"],
        "codex.rate_limits"
    );
    assert_eq!(quota["accounts"][&account.id]["windows"][0]["name"], "5h");
    let mut client = h.connect().await;
    let error = r#"{"type":"error","status":400,"error":{"code":"invalid_encrypted_content","message":"synthetic-conversation"}}"#;
    h.fake.state.push_reply([Message::Text(error.into())]);
    client
        .send(Message::Text(
            r#"{"type":"response.create","input":[]}"#.into(),
        ))
        .await
        .unwrap();
    assert_eq!(receive(&mut client).await, Message::Text(error.into()));
    client.close(None).await.unwrap();
    drop(client);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        h.ledger()
            .iter()
            .any(|v| v["upstream_error_code"] == "invalid_encrypted_content"
                && v["outcome"] == "upstream_error")
    );
}

#[tokio::test]
async fn budget_backpressure_resumes_and_close_codes_mirror_both_directions() {
    let h = Harness::new(30, 1, false).await;
    let held: Reservation = h.state.budget.reserve_message(1024 * 1024);
    let mut client = h.connect().await;
    client
        .send(Message::Text("synthetic blocked input".into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(h.fake.state.messages().is_empty());
    drop(held);
    assert_eq!(
        receive(&mut client).await,
        Message::Text("synthetic blocked input".into())
    );
    client
        .close(Some(CloseFrame {
            code: CloseCode::Away,
            reason: "synthetic close".into(),
        }))
        .await
        .unwrap();
    drop(client);
    wait_record(&h, "ws_close").await;
    assert!(
        h.ledger()
            .iter()
            .any(|v| v["event"] == "ws_close" && v["close_code"] == 1001)
    );
    let mut client = h.connect().await;
    h.fake
        .state
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::Restart,
            reason: "synthetic restart".into(),
        })))
        .unwrap();
    assert!(
        matches!(receive(&mut client).await,Message::Close(Some(frame)) if frame.code==CloseCode::Restart)
    );
}

#[tokio::test]
async fn idle_pair_closes_normally_and_linked_401_redials_once() {
    let h = Harness::new(1, 384, false).await;
    let mut client = h.connect().await;
    assert!(
        matches!(receive(&mut client).await,Message::Close(Some(frame)) if frame.code==CloseCode::Normal)
    );
    drop(client);
    wait_record(&h, "ws_close").await;
    assert!(h.ledger().iter().any(|v| v["reason"] == "idle_close"));
    fs::write(
        h.cpa.join("codex.json"),
        serde_json::to_vec(&fixture(2)).unwrap(),
    )
    .unwrap();
    h.fake.state.push_handshake(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"type":"authentication_error"}}"#,
        HeaderMap::new(),
    );
    let mut client = h.connect().await;
    let requests = h.fake.state.requests();
    assert_eq!(
        requests[requests.len() - 2].headers["authorization"],
        "Bearer synthetic-access-1"
    );
    assert_eq!(
        requests.last().unwrap().headers["authorization"],
        "Bearer synthetic-access-2"
    );
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn network_failure_is_502_and_response_failed_finalizes_as_error() {
    let mut h = Harness::new(30, 384, false).await;
    let mut client = h.connect().await;
    let failed = r#"{"type":"response.failed","response":{"error":{"type":"server_error","code":"synthetic_failure"}}}"#;
    h.fake.state.push_reply([Message::Text(failed.into())]);
    client
        .send(Message::Text(
            r#"{"type":"response.create","input":[]}"#.into(),
        ))
        .await
        .unwrap();
    assert_eq!(receive(&mut client).await, Message::Text(failed.into()));
    client.close(None).await.unwrap();
    drop(client);
    wait_record(&h, "ws_close").await;
    assert!(h.ledger().iter().any(
        |v| v["outcome"] == "upstream_error" && v["upstream_error_code"] == "synthetic_failure"
    ));
    let old = std::mem::replace(&mut h.fake, WebSocketUpstream::start().await.unwrap());
    drop(old);
    match connect_async_with_config(h.request(), None, true).await {
        Err(Error::Http(response)) => assert_eq!(response.status(), 502),
        _ => panic!("network failure must refuse upgrade"),
    }
}

#[cfg(feature = "shape-capture")]
#[tokio::test]
async fn ws_shape_capture_has_allowlisted_response_headers_and_type_counts() {
    let h = Harness::new(30, 384, false).await;
    let mut headers = HeaderMap::new();
    headers.insert("x-codex-primary-used-percent", "12".parse().unwrap());
    headers.insert("openai-version", "synthetic-version".parse().unwrap());
    headers.insert("x-request-id", "synthetic-request".parse().unwrap());
    headers.insert("set-cookie", "synthetic-cookie-secret".parse().unwrap());
    h.fake.state.set_success_headers(headers);
    let mut client = h.connect().await;
    h.fake.state.push_reply([
        Message::Text(r#"{"type":"response.created"}"#.into()),
        Message::Text(r#"{"type":"response.done"}"#.into()),
    ]);
    client
        .send(Message::Text(r#"{"type":"response.create"}"#.into()))
        .await
        .unwrap();
    receive(&mut client).await;
    receive(&mut client).await;
    client.close(None).await.unwrap();
    drop(client);
    wait_record(&h, "ws_close").await;
    let mut text = String::new();
    for entry in fs::read_dir(&h.captures).unwrap() {
        text.push_str(&fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    assert!(text.contains("synthetic-version"));
    assert!(text.contains("synthetic-request"));
    assert!(text.contains("message_types"));
    assert!(text.contains("response.done"));
    assert!(!text.contains("synthetic-cookie-secret"));
    assert!(!text.contains("synthetic-access"));
    assert!(!text.contains("synthetic-local-key"));
}

#[cfg(feature = "phase0")]
#[tokio::test]
async fn phase0_close_after_response_is_normal_and_response_failed_is_error() {
    let h = Harness::new(30, 384, true).await;
    let mut client = h.connect().await;
    h.fake
        .state
        .push_reply([Message::Text(r#"{"type":"response.completed"}"#.into())]);
    client
        .send(Message::Text(r#"{"type":"response.create"}"#.into()))
        .await
        .unwrap();
    assert_eq!(
        receive(&mut client).await,
        Message::Text(r#"{"type":"response.completed"}"#.into())
    );
    assert!(
        matches!(receive(&mut client).await,Message::Close(Some(frame)) if frame.code==CloseCode::Normal)
    );
    drop(client);
    wait_record(&h, "ws_close").await;
    assert!(
        h.ledger()
            .iter()
            .any(|v| v["reason"] == "response_complete")
    );
}
