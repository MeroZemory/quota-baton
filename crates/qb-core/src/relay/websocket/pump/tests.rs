use super::*;
use axum::{Router, extract::Request, http::StatusCode, response::Response, routing::get};
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use tokio_tungstenite::tungstenite::{
    Message as UpstreamMessage, protocol::CloseFrame as UpstreamClose,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, accept_async_with_config, connect_async_with_config,
    tungstenite::protocol::{Role, WebSocketConfig},
};

type Peer = WebSocketStream<TcpStream>;
type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Pair {
    client: Client,
    peer: Peer,
    budget: Arc<InflightBudget>,
    memory_guard: Arc<crate::guard::MemoryGuard>,
    obs: Arc<Observability>,
    home: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
    relay: Option<Arc<crate::relay::RelayState>>,
    drain: Option<tokio::sync::oneshot::Sender<()>>,
}

#[tokio::test]
async fn m7b_guard_pauses_ws_reads_and_resumes() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    let create = Message::Text(r#"{"type":"response.create","model":"fixture","input":[]}"#.into());
    tokio::time::sleep(Duration::from_millis(50)).await;
    pair.memory_guard.force_sample(u64::MAX);
    tokio::time::sleep(Duration::from_millis(50)).await;
    pair.client.send(create.clone()).await.unwrap();
    assert!(pair.memory_guard.is_guarded());
    let held = tokio::time::timeout(Duration::from_millis(100), pair.peer.next()).await;
    assert!(held.is_err(), "guarded WS read was admitted");
    pair.memory_guard.force_sample(0);
    assert_eq!(next(&mut pair.peer).await, create);
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(192 * PARSE_LIMIT))
        .max_frame_size(Some(192 * PARSE_LIMIT))
        .write_buffer_size(0)
}

async fn pair(idle_close: Duration, close_after_response: bool, limit: usize) -> Pair {
    pair_with_read_timeout(
        idle_close,
        close_after_response,
        limit,
        UPSTREAM_READ_TIMEOUT,
    )
    .await
}

async fn pair_with_read_timeout(
    idle_close: Duration,
    close_after_response: bool,
    limit: usize,
    read_timeout: Duration,
) -> Pair {
    pair_with_tracking(idle_close, close_after_response, limit, read_timeout, false).await
}

async fn pair_with_tracking(
    idle_close: Duration,
    close_after_response: bool,
    limit: usize,
    read_timeout: Duration,
    tracking: bool,
) -> Pair {
    pair_with_rate_options(
        idle_close,
        close_after_response,
        limit,
        read_timeout,
        tracking,
        None,
    )
    .await
}

async fn pair_with_rate_options(
    idle_close: Duration,
    close_after_response: bool,
    limit: usize,
    read_timeout: Duration,
    tracking: bool,
    rate: Option<(u64, u32)>,
) -> Pair {
    pair_with_pump_timer_probe(
        idle_close,
        close_after_response,
        limit,
        read_timeout,
        tracking,
        rate,
        None,
    )
    .await
}

async fn pair_with_pump_timer_probe(
    idle_close: Duration,
    close_after_response: bool,
    limit: usize,
    read_timeout: Duration,
    tracking: bool,
    rate: Option<(u64, u32)>,
    timer_probe: Option<Arc<PumpTimerProbe>>,
) -> Pair {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/pump-tests");
    std::fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(root).unwrap();
    let obs = Arc::new(Observability::new(home.path(), None).unwrap());
    let budget = InflightBudget::new(limit);
    let memory_guard = Arc::new(crate::guard::MemoryGuard::new(100));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = listener.local_addr().unwrap();
    let relay = if tracking {
        let store = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match crate::accounts::AccountStore::acquire(home.path()) {
                    Ok(store) => break Arc::new(store),
                    Err(crate::accounts::StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => panic!("synthetic credential lease: {error}"),
                }
            }
        })
        .await
        .unwrap();
        let mut config = crate::config::Config::default();
        if let Some((hold, retries)) = rate {
            config.routing.rate_hold_secs = hold;
            config.routing.rate_hold_max_retries = retries;
        }
        config.upstream.codex_base = format!("http://{upstream_address}");
        config.upstream.anthropic_base = format!("http://{upstream_address}");
        let relay = Arc::new(
            crate::relay::RelayState::new(store, &config, obs.clone(), Arc::default()).unwrap(),
        );
        relay
            .bindings
            .import(serde_json::json!({"schema":1,"bindings":[{
                "key":"codex:overlap", "account":"fake-account", "last_used":chrono::Utc::now()
            }]}))
            .unwrap();
        Some(relay)
    } else {
        None
    };
    let accept = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        accept_async_with_config(socket, Some(config()))
            .await
            .unwrap()
    });
    let response = reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{upstream_address}/responses"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 101);
    let socket = response.upgrade().await.unwrap();
    let upstream = UpstreamSocket::from_raw_socket(socket, Role::Client, Some(config())).await;
    let peer = accept.await.unwrap();
    let upstream = Arc::new(Mutex::new(Some(upstream)));
    let handler_obs = obs.clone();
    let handler_budget = budget.clone();
    let handler_memory_guard = memory_guard.clone();
    let handler_relay = relay.clone();
    let handler_timer_probe = timer_probe;
    let (drain, drained) = tokio::sync::oneshot::channel();
    let drained = Arc::new(Mutex::new(Some(drained)));
    let router = Router::new().route(
        "/",
        get(move |mut request: Request| {
            let upstream = upstream.clone();
            let obs = handler_obs.clone();
            let budget = handler_budget.clone();
            let drained = drained.clone();
            let relay = handler_relay.clone();
            let timer_probe = handler_timer_probe.clone();
            async move {
                let upstream = upstream.lock().await.take().unwrap();
                let drained = drained.lock().await.take().unwrap();
                let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(
                    request.headers()["sec-websocket-key"].as_bytes(),
                );
                let upgrade = hyper::upgrade::on(&mut request);
                tokio::spawn(async move {
                    let client = ClientSocket::from_raw_socket(
                        hyper_util::rt::TokioIo::new(upgrade.await.unwrap()),
                        Role::Server,
                        Some(config()),
                    )
                    .await;
                    let pump = run_with_read_timeout(
                        client,
                        upstream,
                        Context {
                            launch_pin: None,
                            runtime: None,
                            session: Session {
                                key: relay.as_ref().map(|_| "codex:overlap".into()),
                                ..Session::default()
                            },
                            relay,
                            drain_signal: Some(Box::pin(async move {
                                // Cancellation is not a takeover signal.
                                if drained.await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                            })),
                            credentials: None,
                            budget,
                            memory_guard: handler_memory_guard.clone(),
                            observability: obs,
                            account: "fake-account".into(),
                            headers: HeaderMap::new(),
                            upstream_url: String::new(),
                            idle_close,
                            close_after_response,
                            capture_ticket: None,
                            connection: "fake-connection".into(),
                        },
                        read_timeout,
                    );
                    if let Some(probe) = timer_probe {
                        PUMP_TIMER_PROBE.scope(probe, pump).await;
                    } else {
                        pump.await;
                    }
                });
                Response::builder()
                    .status(StatusCode::SWITCHING_PROTOCOLS)
                    .header("connection", "upgrade")
                    .header("upgrade", "websocket")
                    .header("sec-websocket-accept", accept)
                    .body(axum::body::Body::empty())
                    .unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (client, _) = connect_async_with_config(format!("ws://{address}/"), Some(config()), true)
        .await
        .unwrap();
    Pair {
        client,
        peer,
        budget,
        memory_guard,
        obs,
        home,
        server,
        relay,
        drain: Some(drain),
    }
}

async fn next<S>(stream: &mut S) -> UpstreamMessage
where
    S: futures_util::Stream<Item = Result<UpstreamMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

fn records(pair: &Pair) -> Vec<Value> {
    pair.obs.flush().unwrap();
    let mut values = Vec::new();
    for entry in std::fs::read_dir(pair.home.path().join("ledger")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            for line in std::fs::read_to_string(path).unwrap().lines() {
                if let Ok(value) = serde_json::from_str::<Value>(line) {
                    values.push(value);
                }
            }
        }
    }
    values
}

async fn requests(pair: &Pair, count: usize) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let values: Vec<_> = records(pair)
                .into_iter()
                .filter(|value| value.get("req").is_some())
                .collect();
            if values.len() >= count {
                return values;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[test]
fn bounded_detection_and_type_capture() {
    assert!(response_create(r#"{"input":[],"type":"response.create"}"#));
    assert!(!response_create(r#"{"type":"response.interrupt"}"#));
    let huge = format!(
        "{{ \"type\" : \"response.create\",\"input\":\"{}\"}}",
        "x".repeat(PARSE_LIMIT)
    );
    assert!(response_create(&huge));
    assert!(parsed(&huge).is_none());
    assert!(!response_create(&format!(
        "{{\"type\":\"response. create\",\"input\":\"{}\"}}",
        "x".repeat(PARSE_LIMIT)
    )));
    assert!(!response_create(&format!(
        "{{\"type\":\"response.createX\",\"input\":\"{}\"}}",
        "x".repeat(PARSE_LIMIT)
    )));
    let mut counts = BTreeMap::new();
    for index in 0..150 {
        count_type(
            &mut counts,
            &serde_json::json!({"type":format!("event.{index}")}),
        );
    }
    count_type(
        &mut counts,
        &serde_json::json!({"type":"secret with spaces"}),
    );
    count_type(&mut counts, &serde_json::json!({"type":"sk-fake-secret"}));
    count_type(&mut counts, &serde_json::json!({"type":"event.0"}));
    assert_eq!(counts.len(), TYPE_LIMIT);
    assert_eq!(counts["event.0"], 2);
}

#[tokio::test]
async fn duplex_pong_precedes_large_message_completion_and_keeps_reservation() {
    for role in [Role::Client, Role::Server] {
        duplex_pong_for_role(role).await;
    }
}

async fn duplex_pong_for_role(role: Role) {
    let (relay_io, peer_io) = tokio::io::duplex(1024);
    let peer_role = match role {
        Role::Client => Role::Server,
        Role::Server => Role::Client,
    };
    let relay = WebSocketStream::from_raw_socket(relay_io, role, Some(config())).await;
    let mut peer = WebSocketStream::from_raw_socket(peer_io, peer_role, Some(config())).await;
    let (sink, mut relay_read) = relay.split();
    let budget = InflightBudget::new(PARSE_LIMIT);
    let size = 40 * PARSE_LIMIT;
    let writer = write(
        sink,
        Message::Binary(vec![0x73; size].into()),
        budget.reserve_message(size),
        Delivered {
            bytes: size,
            request: None,
            terminal: None,
            completed: false,
        },
    );
    let reader = async {
        assert_eq!(
            next(&mut relay_read).await,
            Message::Ping(bytes::Bytes::from_static(b"duplex-ping"))
        );
        // 다음 읽기 poll은 자동 pong을 flush한다. 쓰기 잠금을 await 동안 붙잡지 않는다.
        let _ = relay_read.next().await;
    };
    let monitor = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(budget.used(), size);
        peer.send(Message::Ping(bytes::Bytes::from_static(b"duplex-ping")))
            .await
            .unwrap();
        assert_eq!(
            next(&mut peer).await,
            Message::Pong(bytes::Bytes::from_static(b"duplex-ping"))
        );
        assert_eq!(budget.used(), size);
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            _ = writer => panic!("pong보다 먼저 전체 메시지 쓰기가 끝남"),
            _ = reader => panic!("pong 검증 전에 읽기가 끝남"),
            _ = monitor => {},
        }
    })
    .await
    .unwrap();
    assert_eq!(budget.used(), 0);
}

#[tokio::test]
async fn overlapping_creates_keep_reservations_and_match_out_of_order_terminals() {
    let mut pair = pair_with_tracking(
        Duration::from_secs(60),
        false,
        4 * PARSE_LIMIT,
        UPSTREAM_READ_TIMEOUT,
        true,
    )
    .await;
    let relay = pair.relay.as_ref().unwrap().clone();
    let before = relay.bindings.export().unwrap()["bindings"][0]["last_used"].clone();
    let creates = [
        r#"{ "type":"response.create", "model":"first", "input":"first-private" }"#,
        r#"{ "type":"response.create", "model":"second", "input":"second-private" }"#,
    ];
    for (index, create) in creates.iter().enumerate() {
        pair.client
            .send(Message::Text((*create).into()))
            .await
            .unwrap();
        assert_eq!(next(&mut pair.peer).await, Message::Text((*create).into()));
        assert_eq!(
            relay.inflight_requests.load(Ordering::Relaxed),
            (index + 1) as u64
        );
        assert_eq!(relay.bindings.pending_count("fake-account"), index + 1);
        assert!(!records(&pair).iter().any(|row| row.get("req").is_some()));
        assert_eq!(
            relay.bindings.export().unwrap()["bindings"][0]["last_used"],
            before
        );
    }
    let mut created_bytes = Vec::new();
    for id in ["r-first", "r-second"] {
        let raw = format!(r#"{{ "type":"response.created", "response":{{"id":"{id}"}} }}"#);
        created_bytes.push(raw.len());
        pair.peer
            .send(Message::Text(raw.clone().into()))
            .await
            .unwrap();
        assert_eq!(next(&mut pair.client).await, Message::Text(raw.into()));
    }
    // The second turn finishes first; its ID lives at the event's top level.
    let second = r#"{ "type":"response.completed", "response_id":"r-second", "usage":{"input_tokens":200,"input_tokens_details":{"cached_tokens":22},"private":"never-ledger"} }"#;
    pair.peer.send(Message::Text(second.into())).await.unwrap();
    assert_eq!(next(&mut pair.client).await, Message::Text(second.into()));
    let rows = requests(&pair, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["model"], "second");
    assert_eq!(rows[0]["bytes_in"], creates[1].len());
    assert_eq!(rows[0]["bytes_out"], created_bytes[1] + second.len());
    assert_eq!(rows[0]["cache_usage"]["cached_tokens"], 22);
    assert_eq!(relay.inflight_requests.load(Ordering::Relaxed), 1);
    assert_eq!(relay.bindings.pending_count("fake-account"), 1);
    // The first turn is still active, and its terminal uses response.id.
    let first = r#"{ "type":"response.completed", "response":{"id":"r-first","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":11}}} }"#;
    pair.peer.send(Message::Text(first.into())).await.unwrap();
    assert_eq!(next(&mut pair.client).await, Message::Text(first.into()));
    let rows = requests(&pair, 2).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["model"], "first");
    assert_eq!(rows[1]["bytes_in"], creates[0].len());
    assert_eq!(rows[1]["bytes_out"], created_bytes[0] + first.len());
    assert_eq!(rows[1]["cache_usage"]["cached_tokens"], 11);
    assert!(rows.iter().all(|row| row["outcome"] == "ok"));
    assert!(!rows.iter().any(|row| row.to_string().contains("private")));
    // Request logging happens just before the completion fields release guards.
    tokio::time::timeout(Duration::from_secs(3), async {
        while relay.inflight_requests.load(Ordering::Relaxed) != 0
            || relay.bindings.pending_count("fake-account") != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(relay.bindings.export().unwrap()["bindings"][0]["last_used"] != before);
}

#[tokio::test]
async fn no_id_error_finishes_pending_create_and_unknown_id_keeps_active_turn() {
    let mut pair = pair(Duration::from_secs(60), false, 4 * PARSE_LIMIT).await;
    for model in ["first", "second"] {
        let raw = format!(r#"{{"type":"response.create","model":"{model}"}}"#);
        pair.client
            .send(Message::Text(raw.clone().into()))
            .await
            .unwrap();
        assert_eq!(next(&mut pair.peer).await, Message::Text(raw.into()));
    }
    for raw in [
        r#"{"type":"response.created","response":{"id":"r-first"}}"#,
        r#"{"type":"response.completed","response_id":"unknown"}"#,
    ] {
        pair.peer.send(Message::Text(raw.into())).await.unwrap();
        assert_eq!(next(&mut pair.client).await, Message::Text(raw.into()));
    }
    assert!(!records(&pair).iter().any(|row| row.get("req").is_some()));
    let error = r#"{"type":"error","status":400,"error":{"code":"invalid_request_error"}}"#;
    pair.peer.send(Message::Text(error.into())).await.unwrap();
    assert_eq!(next(&mut pair.client).await, Message::Text(error.into()));
    let rows = requests(&pair, 1).await;
    assert_eq!(rows[0]["model"], "second");
    assert_eq!(rows[0]["outcome"], "upstream_error");
    let done = r#"{"type":"response.done","usage":{"input_tokens_details":{"cached_tokens":17}}}"#;
    pair.peer.send(Message::Text(done.into())).await.unwrap();
    assert_eq!(next(&mut pair.client).await, Message::Text(done.into()));
    let rows = requests(&pair, 2).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["model"], "first");
    assert_eq!(rows[1]["cache_usage"]["cached_tokens"], 17);
}

#[tokio::test]
async fn terminal_is_recorded_without_eof_and_failed_is_upstream_error() {
    let mut pair = pair(Duration::from_secs(60), false, 4 * PARSE_LIMIT).await;
    for (terminal, outcome) in [
        ("response.completed", "ok"),
        ("response.done", "ok"),
        ("response.failed", "upstream_error"),
    ] {
        let create = r#"{"type":"response.create","input":[]}"#;
        pair.client
            .send(UpstreamMessage::Text(create.into()))
            .await
            .unwrap();
        assert_eq!(
            next(&mut pair.peer).await,
            UpstreamMessage::Text(create.into())
        );
        let terminal_message = serde_json::json!({"type":terminal,"response":{"error":{"code":"server_error","message":"never-ledger-this"}}}).to_string();
        pair.peer
            .send(UpstreamMessage::Text(terminal_message.clone().into()))
            .await
            .unwrap();
        assert_eq!(
            next(&mut pair.client).await,
            UpstreamMessage::Text(terminal_message.into())
        );
        let count = if terminal == "response.completed" {
            1
        } else if terminal == "response.done" {
            2
        } else {
            3
        };
        let rows = requests(&pair, count).await;
        let row = &rows[count - 1];
        assert_eq!(row["outcome"], outcome);
        assert_eq!(row["transport"], "ws");
        assert_eq!(row["status"], 101);
        assert_eq!(row["upstream_status"], 101);
        assert_eq!(row["bytes_in"], create.len());
        if terminal == "response.failed" {
            assert_eq!(row["upstream_error_code"], "server_error");
        }
        assert!(!row.to_string().contains("never-ledger-this"));
    }
    assert_eq!(requests(&pair, 3).await.len(), 3);
}

#[tokio::test]
async fn precommit_error_and_interrupt_are_verbatim() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    for text in [
        r#"{"type":"response.create"}"#,
        r#"{"type":"response.interrupt"}"#,
    ] {
        pair.client
            .send(UpstreamMessage::Text(text.into()))
            .await
            .unwrap();
        assert_eq!(
            next(&mut pair.peer).await,
            UpstreamMessage::Text(text.into())
        );
    }
    let error = r#"{"type":"error","status":400,"error":{"code":"invalid_request_error","message":"fake"}}"#;
    pair.peer
        .send(UpstreamMessage::Text(error.into()))
        .await
        .unwrap();
    assert_eq!(
        next(&mut pair.client).await,
        UpstreamMessage::Text(error.into())
    );
    let rows = requests(&pair, 1).await;
    assert_eq!(rows[0]["outcome"], "upstream_error");
    assert_eq!(rows[0]["upstream_error_code"], "invalid_request_error");
    assert_eq!(rows[0]["status"], 400);
    assert_eq!(rows[0]["upstream_status"], 400);
}

#[tokio::test]
async fn error_status_accepts_only_integer_http_statuses() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    let cases = [
        (serde_json::json!(100), 100),
        (serde_json::json!(599), 599),
        (serde_json::json!(99), 101),
        (serde_json::json!(600), 101),
        (serde_json::json!(-1), 101),
        (serde_json::json!(400.5), 101),
        (serde_json::json!("400"), 101),
    ];
    for (index, (status, expected)) in cases.into_iter().enumerate() {
        pair.client
            .send(Message::Text(r#"{"type":"response.create"}"#.into()))
            .await
            .unwrap();
        next(&mut pair.peer).await;
        let error =
            serde_json::json!({"type":"error","status":status,"error":{"type":"server_error"}})
                .to_string();
        pair.peer
            .send(Message::Text(error.clone().into()))
            .await
            .unwrap();
        assert_eq!(next(&mut pair.client).await, Message::Text(error.into()));
        let rows = requests(&pair, index + 1).await;
        assert_eq!(rows[index]["status"], expected);
        assert_eq!(rows[index]["upstream_status"], expected);
        assert_eq!(rows[index]["upstream_error_code"], "server_error");
    }
}

#[tokio::test]
async fn r12_9_failed_done_envelopes_record_only_bounded_safe_error_codes_verbatim() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    let cases = [
        (
            "response.done",
            "usage_limit_reached".to_owned(),
            Some("usage_limit_reached"),
        ),
        (
            "response.completed",
            "server_error".to_owned(),
            Some("server_error"),
        ),
        ("response.failed", "x".repeat(101), None),
        ("error", "private message with spaces".to_owned(), None),
        ("error", "sk-synthetic-secret".to_owned(), None),
        ("error", "eyJsynthetic".to_owned(), None),
    ];
    for (index, (kind, code, expected)) in cases.into_iter().enumerate() {
        pair.client
            .send(Message::Text(r#"{"type":"response.create"}"#.into()))
            .await
            .unwrap();
        next(&mut pair.peer).await;
        let error = serde_json::json!({"code":code,"message":"never-ledger-payload"});
        let event = if kind == "error" {
            serde_json::json!({"type":kind,"error":error})
        } else {
            serde_json::json!({"type":kind,"response":{"status":"failed","error":error}})
        }
        .to_string();
        pair.peer
            .send(Message::Text(event.clone().into()))
            .await
            .unwrap();
        assert_eq!(next(&mut pair.client).await, Message::Text(event.into()));
        let rows = requests(&pair, index + 1).await;
        let row = &rows[index];
        assert_eq!(row["outcome"], "upstream_error");
        assert_eq!(
            row.get("upstream_error_code").and_then(Value::as_str),
            expected
        );
        assert!(!row.to_string().contains("never-ledger-payload"));
        assert!(!row.to_string().contains("private message"));
        assert!(!row.to_string().contains("synthetic-secret"));
    }
}

#[tokio::test]
async fn forty_mib_single_frames_are_verbatim_in_both_directions() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    let bytes = bytes::Bytes::from(vec![0xa5; 40 * PARSE_LIMIT]);
    pair.client
        .send(UpstreamMessage::Binary(bytes.clone()))
        .await
        .unwrap();
    assert_eq!(
        next(&mut pair.peer).await,
        UpstreamMessage::Binary(bytes.clone())
    );
    pair.peer
        .send(UpstreamMessage::Binary(bytes.clone()))
        .await
        .unwrap();
    assert_eq!(next(&mut pair.client).await, UpstreamMessage::Binary(bytes));
    // UTF-8 코드 포인트가 continuation 경계를 가로질러도 원본 텍스트는 유지된다.
    let text = format!(
        "{}🙂{}",
        "a".repeat(65535),
        "b".repeat(40 * PARSE_LIMIT - 65539)
    );
    pair.client
        .send(Message::Text(text.clone().into()))
        .await
        .unwrap();
    assert_eq!(
        next(&mut pair.peer).await,
        Message::Text(text.clone().into())
    );
    pair.peer
        .send(Message::Text(text.clone().into()))
        .await
        .unwrap();
    assert_eq!(next(&mut pair.client).await, Message::Text(text.into()));
    tokio::time::timeout(Duration::from_secs(3), async {
        while pair.budget.used() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn upstream_ping_is_answered_before_blocked_forty_mib_write_finishes() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    let bytes = bytes::Bytes::from(vec![0x5a; 40 * PARSE_LIMIT]);
    let input = bytes.clone();
    let upload = async {
        pair.client.send(Message::Binary(input)).await.unwrap();
    };
    let check = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while pair.budget.used() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // 수신을 멈춰 큰 쓰기를 막은 상태에서도 upstream 읽기는 예산을 기다리지 않는다.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(pair.budget.used(), bytes.len());
        pair.peer
            .send(Message::Ping(bytes::Bytes::from_static(b"during-upload")))
            .await
            .unwrap();
        let first = next(&mut pair.peer).await;
        assert_eq!(
            first,
            Message::Pong(bytes::Bytes::from_static(b"during-upload"))
        );
        assert_eq!(next(&mut pair.peer).await, Message::Binary(bytes));
    };
    tokio::join!(upload, check);
    assert_eq!(
        next(&mut pair.client).await,
        Message::Ping(bytes::Bytes::from_static(b"during-upload"))
    );
}

#[tokio::test]
async fn client_ping_is_answered_before_blocked_forty_mib_write_finishes() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    let bytes = bytes::Bytes::from(vec![0x6b; 40 * PARSE_LIMIT]);
    let input = bytes.clone();
    let upload = async {
        pair.peer.send(Message::Binary(input)).await.unwrap();
    };
    let check = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while pair.budget.used() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(pair.budget.used(), bytes.len());
        pair.client
            .send(Message::Ping(bytes::Bytes::from_static(b"during-download")))
            .await
            .unwrap();
        assert_eq!(
            next(&mut pair.client).await,
            Message::Pong(bytes::Bytes::from_static(b"during-download"))
        );
        assert_eq!(next(&mut pair.client).await, Message::Binary(bytes));
    };
    tokio::join!(upload, check);
    assert_eq!(
        next(&mut pair.peer).await,
        Message::Ping(bytes::Bytes::from_static(b"during-download"))
    );
}

#[tokio::test]
async fn quota_events_are_observed_and_relayed_without_transcript() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    let event = r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":42,"window_minutes":300,"reset_after_seconds":30}},"plan_type":"plus"}"#;
    pair.peer.send(Message::Text(event.into())).await.unwrap();
    assert_eq!(next(&mut pair.client).await, Message::Text(event.into()));
    pair.obs.flush().unwrap();
    let quota: Value =
        serde_json::from_slice(&std::fs::read(pair.home.path().join("state/quota.json")).unwrap())
            .unwrap();
    assert_eq!(
        quota["accounts"]["fake-account"]["source"],
        "codex.rate_limits"
    );
    assert_eq!(
        quota["accounts"]["fake-account"]["windows"][0]["used_pct"],
        42.0
    );
    assert_eq!(
        records(&pair)
            .iter()
            .filter(|row| row.get("req").is_some())
            .count(),
        0
    );
}

#[tokio::test]
async fn upstream_ping_resets_read_timeout_then_timeout_finalizes_active_request() {
    assert_eq!(UPSTREAM_READ_TIMEOUT, Duration::from_secs(600));
    let mut pair = pair_with_read_timeout(
        Duration::from_secs(10),
        false,
        PARSE_LIMIT,
        Duration::from_millis(300),
    )
    .await;
    pair.client
        .send(Message::Text(r#"{"type":"response.create"}"#.into()))
        .await
        .unwrap();
    next(&mut pair.peer).await;
    tokio::time::sleep(Duration::from_millis(180)).await;
    pair.peer
        .send(Message::Ping(bytes::Bytes::from_static(b"keepalive")))
        .await
        .unwrap();
    assert_eq!(
        next(&mut pair.peer).await,
        Message::Pong(bytes::Bytes::from_static(b"keepalive"))
    );
    assert_eq!(
        next(&mut pair.client).await,
        Message::Ping(bytes::Bytes::from_static(b"keepalive"))
    );
    // 연결 시작 기준으로 300 ms를 넘겨도 마지막 upstream ping 기준 300 ms 전에는 닫히지 않는다.
    assert!(
        tokio::time::timeout(Duration::from_millis(180), pair.client.next())
            .await
            .is_err()
    );
    assert!(
        matches!(next(&mut pair.client).await, Message::Close(Some(frame)) if frame.code == 1001.into())
    );
    let rows = requests(&pair, 1).await;
    assert_eq!(rows[0]["outcome"], "upstream_error");
    let connection = records(&pair);
    assert!(connection.iter().any(|row| row["event"] == "ws_open"));
    assert!(connection.iter().any(|row| row["event"] == "ws_close"
        && row["close_code"] == 1001
        && row["reason"] == "upstream_read_timeout"));
}

#[tokio::test]
async fn exhausted_budget_pauses_then_resumes_reading() {
    let mut pair = pair(Duration::from_secs(60), false, 1).await;
    pair.client
        .send(Message::Frame(Frame::message(
            vec![1; 8],
            OpCode::Data(Data::Binary),
            false,
        )))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    let reservation = pair.budget.reserve_message(1);
    // 이미 시작한 읽기 하나는 예산 초과 상태에서도 끝낼 수 있다.
    pair.client
        .send(Message::Frame(Frame::message(
            vec![1; 8],
            OpCode::Data(Data::Continue),
            true,
        )))
        .await
        .unwrap();
    assert_eq!(
        next(&mut pair.peer).await,
        UpstreamMessage::Binary(vec![1; 16].into())
    );
    pair.client
        .send(UpstreamMessage::Binary(vec![7; 1024].into()))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(80), pair.peer.next())
            .await
            .is_err()
    );
    drop(reservation);
    assert_eq!(
        next(&mut pair.peer).await,
        UpstreamMessage::Binary(vec![7; 1024].into())
    );
}

#[tokio::test]
async fn pings_have_local_pongs_and_forwarded_payloads() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    pair.client
        .send(UpstreamMessage::Ping(bytes::Bytes::from_static(
            b"client-ping",
        )))
        .await
        .unwrap();
    assert_eq!(
        next(&mut pair.client).await,
        UpstreamMessage::Pong(bytes::Bytes::from_static(b"client-ping"))
    );
    assert_eq!(
        next(&mut pair.peer).await,
        UpstreamMessage::Ping(bytes::Bytes::from_static(b"client-ping"))
    );
    // 상대의 자동 pong을 먼저 flush하여 다음 ping의 순서를 고정한다.
    pair.peer.flush().await.unwrap();
    assert_eq!(
        next(&mut pair.client).await,
        UpstreamMessage::Pong(bytes::Bytes::from_static(b"client-ping"))
    );
    pair.peer
        .send(UpstreamMessage::Ping(bytes::Bytes::from_static(
            b"upstream-ping",
        )))
        .await
        .unwrap();
    assert_eq!(
        next(&mut pair.peer).await,
        UpstreamMessage::Pong(bytes::Bytes::from_static(b"upstream-ping"))
    );
    assert_eq!(
        next(&mut pair.client).await,
        UpstreamMessage::Ping(bytes::Bytes::from_static(b"upstream-ping"))
    );
}

#[tokio::test]
async fn idle_and_phase0_close_use_normal_code() {
    let mut idle = pair(Duration::from_millis(80), false, PARSE_LIMIT).await;
    assert!(
        matches!(next(&mut idle.client).await, UpstreamMessage::Close(Some(frame)) if frame.code == 1000.into())
    );
    assert!(
        matches!(next(&mut idle.peer).await, UpstreamMessage::Close(Some(frame)) if frame.code == 1000.into())
    );
    let mut phase0 = pair(Duration::from_secs(60), true, PARSE_LIMIT).await;
    phase0
        .client
        .send(UpstreamMessage::Text(
            r#"{"type":"response.create"}"#.into(),
        ))
        .await
        .unwrap();
    next(&mut phase0.peer).await;
    phase0
        .peer
        .send(UpstreamMessage::Text(
            r#"{"type":"response.completed"}"#.into(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut phase0.client).await,
        UpstreamMessage::Text(_)
    ));
    assert!(
        matches!(next(&mut phase0.client).await, UpstreamMessage::Close(Some(frame)) if frame.code == 1000.into())
    );
    assert_eq!(requests(&phase0, 1).await[0]["outcome"], "ok");
}

#[tokio::test]
async fn close_code_and_reason_are_mirrored_both_ways() {
    for upstream_first in [false, true] {
        let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
        let close = UpstreamMessage::Close(Some(UpstreamClose {
            code: 1001.into(),
            reason: "fake-close".into(),
        }));
        if upstream_first {
            pair.peer.send(close.clone()).await.unwrap();
            assert_eq!(next(&mut pair.client).await, close);
        } else {
            pair.client.send(close.clone()).await.unwrap();
            assert_eq!(next(&mut pair.peer).await, close);
        }
    }
}

#[tokio::test]
async fn c6_row_13a_only_response_events_commit_the_current_request() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/pump-tests");
    std::fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(root).unwrap();
    let context = Context {
        launch_pin: None,
        runtime: None,
        relay: None,
        drain_signal: None,
        credentials: None,
        budget: InflightBudget::new(PARSE_LIMIT),
        memory_guard: Arc::new(crate::guard::MemoryGuard::new(u64::MAX)),
        observability: Arc::new(Observability::new(home.path(), None).unwrap()),
        account: "fixture".into(),
        session: Session::default(),
        headers: HeaderMap::new(),
        upstream_url: String::new(),
        idle_close: Duration::from_secs(60),
        close_after_response: false,
        capture_ticket: None,
        connection: "fixture".into(),
    };
    let mut current = CurrentCompletion::new(&context).await.unwrap();
    for event in [
        None,
        Some(serde_json::json!({"type":"metadata"})),
        Some(serde_json::json!({"type":"codex.rate_limits"})),
    ] {
        current.upstream(event.as_ref());
        assert!(!current.committed);
    }
    current.upstream(Some(&serde_json::json!({"type":"response.created"})));
    assert!(current.committed);
}

#[test]
fn health_broadcast_filters_unrelated_accounts_before_idle_eligibility_load() {
    for kind in ["account_changed", "quota_updated"] {
        assert!(!event_affects_account(
            &serde_json::json!({"event":kind,"data":{"id":"other-account"}}),
            "pinned-account"
        ));
        assert!(event_affects_account(
            &serde_json::json!({"event":kind,"data":{"id":"pinned-account"}}),
            "pinned-account"
        ));
    }
    assert!(!event_affects_account(
        &serde_json::json!({"event":"account_changed","data":{}}),
        "pinned-account"
    ));
}

#[test]
fn auth_retry_marker_expires_before_natural_access_token_expiry() {
    let now = std::time::Instant::now();
    let marker = crate::relay::websocket::RetriedToken {
        relay: std::sync::Weak::new(),
        account: "synthetic-account".into(),
        digest: [7; 32],
        expires_at: now + crate::relay::websocket::RETRY_MARKER_TTL,
    };
    assert!(marker.matches(&[7; 32], now + Duration::from_secs(119)));
    assert!(!marker.matches(&[7; 32], now + Duration::from_secs(120)));
    assert!(!marker.matches(&[7; 32], now + Duration::from_secs(3600)));
    assert!(!marker.matches(&[8; 32], now));
}

#[tokio::test]
async fn takeover_drain_closes_idle_socket_with_1001() {
    let mut pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    pair.drain.take().unwrap().send(()).unwrap();
    assert!(
        matches!(next(&mut pair.client).await, Message::Close(Some(frame)) if frame.code == 1001.into())
    );
    assert!(
        matches!(next(&mut pair.peer).await, Message::Close(Some(frame)) if frame.code == 1001.into())
    );
    assert!(records(&pair).iter().any(|row| row["event"] == "ws_close"
        && row["close_code"] == 1001
        && row["reason"] == "takeover_drain"));
}

#[tokio::test]
async fn takeover_drain_delivers_current_response_before_1001() {
    for close_after_response in [false, true] {
        let mut pair = pair(Duration::from_secs(60), close_after_response, PARSE_LIMIT).await;
        let create = Message::Text(r#"{"type":"response.create"}"#.into());
        pair.client.send(create.clone()).await.unwrap();
        assert_eq!(next(&mut pair.peer).await, create);
        let created = Message::Text(
            r#"{"type":"response.created","response":{"id":"synthetic-response"}}"#.into(),
        );
        pair.peer.send(created.clone()).await.unwrap();
        assert_eq!(next(&mut pair.client).await, created);
        pair.drain.take().unwrap().send(()).unwrap();
        // Keep the current response open until its terminal event.
        assert!(
            tokio::time::timeout(Duration::from_millis(80), pair.client.next())
                .await
                .is_err()
        );
        // New turns cannot prolong the old core's drain.
        pair.client.send(create).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), pair.peer.next())
                .await
                .is_err()
        );
        for text in [
            r#"{"type":"response.output_text.delta","response_id":"synthetic-response","delta":"fixture"}"#,
            r#"{"type":"response.completed","response":{"id":"synthetic-response"}}"#,
        ] {
            let message = Message::Text(text.into());
            pair.peer.send(message.clone()).await.unwrap();
            assert_eq!(next(&mut pair.client).await, message);
        }
        assert!(
            matches!(next(&mut pair.client).await, Message::Close(Some(frame)) if frame.code == 1001.into())
        );
        assert!(
            matches!(next(&mut pair.peer).await, Message::Close(Some(frame)) if frame.code == 1001.into())
        );
        let rows = requests(&pair, 1).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], "ok");
    }
}

// No runtime is attached to these protocol pairs: they exercise pump policy,
// not exponential backoff, and catch the removed fixed one-second sleep.
#[tokio::test]
async fn r1_ws_configured_retry_caps_one_and_above_three() {
    for cap in [1, 5] {
        let mut pair = pair_with_rate_options(
            Duration::from_secs(60),
            false,
            PARSE_LIMIT,
            UPSTREAM_READ_TIMEOUT,
            true,
            Some((60, cap)),
        )
        .await;
        let create = Message::Text(r#"{"type":"response.create","model":"fixture"}"#.into());
        pair.client.send(create.clone()).await.unwrap();
        assert_eq!(next(&mut pair.peer).await, create);
        let started = std::time::Instant::now();
        for _ in 0..cap {
            pair.peer
                .send(Message::Text(
                    r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#
                        .into(),
                ))
                .await
                .unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_millis(750), pair.peer.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                create
            );
        }
        assert!(started.elapsed() < Duration::from_secs(u64::from(cap)));
        pair.peer
            .send(Message::Text(
                r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into(),
            ))
            .await
            .unwrap();
        assert!(
            matches!(next(&mut pair.client).await, Message::Close(Some(frame)) if frame.code == 1012.into())
        );
        let rows = requests(&pair, 1).await;
        assert_eq!(rows[0]["attempts"], cap + 1);
        assert_eq!(rows[0]["rate_retries"], cap);
        assert_eq!(rows[0]["rate_wait_ms"], 0);
        assert_eq!(rows[0]["outcome"], "rate_hold_relayed_429");
    }
}

#[tokio::test]
async fn r1_ws_cancelled_open_wait_and_redial_share_deadline_and_wait_metadata() {
    let pair = pair_with_tracking(
        Duration::from_secs(60),
        false,
        PARSE_LIMIT,
        UPSTREAM_READ_TIMEOUT,
        true,
    )
    .await;
    let runtime = &pair.relay.as_ref().unwrap().accounts;
    runtime.observe_429("fake-account", Some(Duration::from_secs(60)));
    let rate = Arc::new(RateBudget::with_limits(Duration::from_millis(160), 5));
    let deadline = rate.deadline;
    let mut open = Box::pin(rate.admit(Some(runtime), "fake-account"));
    tokio::select! {
        _ = &mut open => panic!("rate wait was bypassed"),
        _ = tokio::time::sleep(Duration::from_millis(80)) => {}
    }
    drop(open); // the upstream closes while the open-socket wait is pending
    tokio::time::timeout(Duration::from_millis(30), async {
        while runtime.pacing_snapshot("fake-account").waiting != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let redial_started = std::time::Instant::now();
    tokio::time::timeout(
        Duration::from_millis(140),
        rate.admit(Some(runtime), "fake-account"),
    )
    .await
    .unwrap();
    assert_eq!(rate.deadline, deadline);
    assert!(redial_started.elapsed() < Duration::from_millis(140));
    rate.sent();
    assert!(!rate.can_retry(), "the forced send must be final");
    let state = rate.state.lock().unwrap();
    assert_eq!(state.retries, 1, "cancelled wait was not an attempt");
    assert!(state.waited >= Duration::from_millis(140));
    assert!(state.forced);
}

#[test]
fn r1_create_initial_dispatch_is_not_a_retry_and_forcing_is_terminal() {
    let rate = RateBudget::with_limits(Duration::ZERO, 5);
    assert!(!rate.initial_sent());
    rate.initial_dispatch();
    assert!(rate.initial_sent());
    assert!(!rate.can_retry());
    assert_eq!(rate.state.lock().unwrap().retries, 0);
}

#[tokio::test]
async fn r1_create_unsent_completion_keeps_guard_but_ledgers_zero_attempts() {
    let pair = pair_with_tracking(
        Duration::from_secs(60),
        false,
        PARSE_LIMIT,
        UPSTREAM_READ_TIMEOUT,
        true,
    )
    .await;
    let relay = pair.relay.as_ref().unwrap();
    let context = Context {
        launch_pin: None,
        runtime: Some(relay.accounts.clone()),
        relay: Some(relay.clone()),
        drain_signal: None,
        credentials: None,
        budget: pair.budget.clone(),
        memory_guard: pair.memory_guard.clone(),
        observability: pair.obs.clone(),
        account: "fake-account".into(),
        session: Session::default(),
        headers: HeaderMap::new(),
        upstream_url: String::new(),
        idle_close: Duration::from_secs(60),
        close_after_response: false,
        capture_ticket: None,
        connection: "queued-fixture".into(),
    };
    let active = CurrentCompletion::new(&context).await.unwrap();
    assert_eq!(relay.accounts.inflight_count("fake-account"), 1);
    let rate = active.rate.clone();
    let wait = RateWait::new(rate.clone());
    rate.state.lock().unwrap().waiting_since =
        Some(std::time::Instant::now() - Duration::from_millis(30));
    let mut queued = BTreeMap::from([(0, active)]);
    assert_eq!(
        response_completion(
            &mut queued,
            Some(&serde_json::json!({"type":"response.created","response":{"id":"unsolicited"}}))
        ),
        None
    );
    assert_eq!(
        response_completion(
            &mut queued,
            Some(&serde_json::json!({"type":"error","status":429}))
        ),
        None
    );
    let active = queued.remove(&0).unwrap();
    // The fallback must also work if the completion drops before its wait.
    active.finish("client_drop");
    drop(wait);
    assert_eq!(relay.accounts.inflight_count("fake-account"), 0);
    let rows = requests(&pair, 1).await;
    assert_eq!(rows[0]["attempts"], 0);
    assert_eq!(rows[0]["attempt_details"], serde_json::json!([]));
    assert_eq!(rows[0]["rate_retries"], 0);
    assert!(rows[0]["rate_wait_ms"].as_u64().unwrap() >= 30);
}

#[test]
fn r1_ws_retry_advice_is_a_sidecopy() {
    let event = serde_json::json!({"type":"error","status":429,"error":{"code":"slow_down"},"headers":{"ReTrY-AfTeR":"7"}});
    let original = event.clone();
    assert!(ws_rate_advice(&event));
    assert_eq!(ws_retry_after(&event), Some(Duration::from_secs(7)));
    assert_eq!(event, original);
}

#[tokio::test]
async fn r1_ws_completion_outlives_context_and_snapshots_both_wait_drop_orders() {
    for explicit_finish in [false, true] {
        for wait_dropped_first in [false, true] {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(".sandbox/pump-tests");
            std::fs::create_dir_all(&root).unwrap();
            let home = tempfile::tempdir_in(root).unwrap();
            let obs = Arc::new(Observability::new(home.path(), None).unwrap());
            let context = Context {
                launch_pin: None,
                runtime: None,
                relay: None,
                drain_signal: None,
                credentials: None,
                budget: InflightBudget::new(PARSE_LIMIT),
                memory_guard: Arc::new(crate::guard::MemoryGuard::new(u64::MAX)),
                observability: obs.clone(),
                account: "fixture".into(),
                session: Session::default(),
                headers: HeaderMap::new(),
                upstream_url: String::new(),
                idle_close: Duration::from_secs(60),
                close_after_response: false,
                capture_ticket: None,
                connection: "fixture".into(),
            };
            let active = CurrentCompletion::new(&context).await.unwrap();
            let rate = active.rate.clone();
            rate.sent(); // one actual resend, followed by an unsent wait
            let wait = RateWait::new(rate.clone());
            rate.state.lock().unwrap().waiting_since =
                Some(std::time::Instant::now() - Duration::from_millis(25));
            drop(context);
            if wait_dropped_first {
                drop(wait);
                if explicit_finish {
                    active.finish("ok");
                } else {
                    drop(active);
                }
            } else {
                if explicit_finish {
                    active.finish("ok");
                } else {
                    drop(active);
                }
                drop(wait);
            }
            obs.flush().unwrap();
            let mut rows = Vec::<Value>::new();
            for entry in std::fs::read_dir(home.path().join("ledger")).unwrap() {
                let path = entry.unwrap().path();
                if path.is_file() {
                    for line in std::fs::read_to_string(path).unwrap().lines() {
                        let row: Value = serde_json::from_str(line).unwrap();
                        if row.get("req").is_some() {
                            rows.push(row);
                        }
                    }
                }
            }
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["attempts"], 2);
            assert_eq!(
                rows[0]["rate_retries"], 1,
                "pending wait must not become an attempt"
            );
            assert!(rows[0]["rate_wait_ms"].as_u64().unwrap() >= 25);
            assert_eq!(
                rows[0]["outcome"],
                if explicit_finish {
                    "rate_hold_ok"
                } else {
                    "client_drop"
                }
            );
        }
    }
}

fn n5b_inspection_context(pair: &Pair) -> Context {
    Context {
        launch_pin: None,
        runtime: None,
        relay: pair.relay.clone(),
        drain_signal: None,
        credentials: None,
        budget: pair.budget.clone(),
        memory_guard: pair.memory_guard.clone(),
        observability: pair.obs.clone(),
        account: "fake-account".into(),
        session: Session::default(),
        headers: HeaderMap::new(),
        upstream_url: String::new(),
        idle_close: Duration::from_secs(60),
        close_after_response: false,
        capture_ticket: None,
        connection: "synthetic-inspection".into(),
    }
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn n5b_create_revalidation_keeps_expired_original_deadline() {
    let pair = pair_with_tracking(
        Duration::from_secs(60),
        false,
        PARSE_LIMIT,
        UPSTREAM_READ_TIMEOUT,
        true,
    )
    .await;
    let context = Arc::new(n5b_inspection_context(&pair));
    let relay = context.relay.as_ref().unwrap();
    let (arrived, release) = pause_create_dispatch(relay);
    // The managed revalidation entry must honor the supplied original bound,
    // even with an unreleased native test gate and a fresh Context clock.
    let original = Instant::now() - Duration::from_millis(1);
    let result = tokio::time::timeout(Duration::from_millis(250), async {
        let (arrived, result) = tokio::join!(
            arrived,
            admit_create(context.clone(), Some("synthetic-model".into()), original)
        );
        arrived.unwrap();
        result
    })
    .await
    .expect("managed create revalidation must not renew an expired create deadline");
    assert!(matches!(result, Err(DataLaunchError::Unavailable)));
    assert!(release.is_closed());
    assert_eq!(pair.budget.used(), 0);
    assert_eq!(relay.bindings.pending_count("fake-account"), 0);
}

#[tokio::test]
async fn n5b_initial_admission_preserves_forced_expiry_classification() {
    let pair = pair(Duration::from_secs(60), false, PARSE_LIMIT).await;
    let context = Arc::new(n5b_inspection_context(&pair));
    for forced in [false, true] {
        let rate = Arc::new(RateBudget::with_limits(Duration::from_secs(30), 3));
        rate.state.lock().unwrap().forced = forced;
        rate.commit(); // Native admit returns false; eligible(None runtime) remains true.
        let pending = PendingInitial {
            request: 0,
            message: Message::text("synthetic"),
            reservation: pair.budget.reserve_message(9),
            rate,
            create: CreateLifetime::new(Instant::now() + Duration::from_secs(30)),
        };
        assert_eq!(
            admit_initial(context.clone(), &pending, None, false).await,
            InitialAdmission::Rejected { expired: forced }
        );
        drop(pending);
    }
    // A ready native admission retains the same rate deadline and send counters.
    let rate = Arc::new(RateBudget::with_limits(Duration::from_secs(30), 3));
    let deadline = rate.deadline;
    let pending = PendingInitial {
        request: 1,
        message: Message::text("synthetic"),
        reservation: pair.budget.reserve_message(9),
        rate: rate.clone(),
        create: CreateLifetime::new(Instant::now() + Duration::from_secs(30)),
    };
    assert_eq!(
        admit_initial(context, &pending, Some("synthetic-model".into()), false).await,
        InitialAdmission::Ready
    );
    assert_eq!(rate.deadline, deadline);
    assert!(!rate.initial_sent());
    assert_eq!(rate.state.lock().unwrap().retries, 0);
    drop(pending);
    assert_eq!(pair.budget.used(), 0);
}

#[tokio::test]
async fn n5b_initial_admission_forced_but_ineligible_does_not_report_rate_expiry() {
    let pair = pair_with_tracking(
        Duration::from_secs(60),
        false,
        PARSE_LIMIT,
        UPSTREAM_READ_TIMEOUT,
        true,
    )
    .await;
    let mut context = n5b_inspection_context(&pair);
    context.runtime = Some(pair.relay.as_ref().unwrap().accounts.clone());
    // The pair intentionally has a binding but no native account in its store.
    assert!(
        !context
            .runtime
            .as_ref()
            .unwrap()
            .eligible("fake-account", None)
            .await
    );
    let rate = Arc::new(RateBudget::with_limits(Duration::ZERO, 3));
    rate.state.lock().unwrap().forced = true;
    rate.commit();
    let pending = PendingInitial {
        request: 0,
        message: Message::text("synthetic"),
        reservation: pair.budget.reserve_message(9),
        rate,
        create: CreateLifetime::new(Instant::now() + Duration::from_secs(30)),
    };
    assert_eq!(
        admit_initial(Arc::new(context), &pending, None, false).await,
        InitialAdmission::Rejected { expired: false }
    );
    drop(pending);
    assert_eq!(pair.budget.used(), 0);
}

#[test]
fn n5b_create_guard_is_not_kept_alive_by_detached_operation() {
    let create = CreateLifetime::new(Instant::now() + Duration::from_secs(30));
    let detached_operation = create.operation.clone();
    let pending_future_owner = create.clone();
    drop(create);
    assert!(detached_operation.can_reply());
    assert!(detached_operation.check().is_ok());
    drop(pending_future_owner);
    assert!(!detached_operation.can_reply());
    assert!(detached_operation.check().is_err());
    assert!(detached_operation.admit().is_err());
}

#[test]
fn n5b_create_expiry_is_not_rate_or_metadata_admission() {
    let create = CreateLifetime::new(Instant::now() - Duration::from_millis(1));
    let metadata = IssueOperation::new(Instant::now() + Duration::from_secs(30));
    let _metadata_caller = metadata.caller_guard();
    metadata.admit().unwrap();
    let rate = RateBudget::with_limits(Duration::ZERO, 3);
    assert!(
        rate.can_send(),
        "native rate expiry may still allow its one forced send"
    );
    assert!(
        !create.admit_first_write(),
        "neither metadata nor rate expiry grants expired create admission"
    );
    assert!(!rate.initial_sent());
}

#[tokio::test]
async fn n5b_initial_write_keeps_caller_through_backpressure_then_drop() {
    let (entered, arrived) = tokio::sync::oneshot::channel::<()>();
    let (_release, blocked) = tokio::sync::oneshot::channel::<()>();
    let sink = Box::pin(futures_util::sink::unfold(
        (Some(entered), Some(blocked)),
        |(entered, blocked), _: Message| async move {
            let _ = entered.unwrap().send(());
            let _ = blocked.unwrap().await;
            Ok::<_, std::io::Error>((None, None))
        },
    ));
    let budget = InflightBudget::new(32);
    let create = CreateLifetime::new(Instant::now() + Duration::from_secs(30));
    let operation = create.operation.clone();
    assert!(create.admit_first_write());
    let mut writer = write_initial(
        sink,
        Message::text("synthetic"),
        budget.reserve_message(9),
        Delivered {
            bytes: 9,
            request: Some(0),
            terminal: None,
            completed: false,
        },
        create,
        PhysicalLeg::new("sample-source"),
        DispatchSnapshot {
            account: "sample-source".into(),
            key: None,
            model: None,
            pin: None,
            owner: None,
            deadline: None,
        },
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            arrived = arrived => arrived.unwrap(),
            _ = &mut writer => panic!("initial write completed before native sink release"),
        }
    })
    .await
    .unwrap();
    assert!(
        operation.can_reply(),
        "physical Pending write must retain its original caller"
    );
    assert_eq!(budget.used(), 9);
    drop(writer);
    assert!(
        !operation.can_reply(),
        "cancelled physical write must drop its caller owner"
    );
    assert_eq!(budget.used(), 0);
}

// These one-shot controls are scoped to one actual pump task, not a global
// writer override. They retain operation identity only, never its CallerGuard.
tokio::task_local! {
    static PUMP_TIMER_PROBE: Arc<PumpTimerProbe>;
}

type CreateTimerSnapshot = (std::sync::Weak<CreateLifetime>, Instant);
type InitialFlushObservation = (std::sync::Weak<CreateLifetime>, IssueOperation);

// A gate in the retained final-admission future lets the native read-budget
// branch settle before this test releases a write. The snapshot comes from the
// real outer select, after that branch is disabled by client_admitted == true.
struct PumpTimerProbe {
    before_write: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    snapshot: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<CreateTimerSnapshot>>>,
    flush: std::sync::Mutex<Option<InitialFlushGate>>,
    timer: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<bool>>>,
}

struct InitialFlushGate {
    arrived: Option<tokio::sync::oneshot::Sender<InitialFlushObservation>>,
    release: tokio::sync::oneshot::Receiver<()>,
}

struct PumpTimerControl {
    probe: Arc<PumpTimerProbe>,
    at_snapshot: tokio::sync::oneshot::Receiver<CreateTimerSnapshot>,
    start_write: tokio::sync::oneshot::Sender<()>,
    at_flush: tokio::sync::oneshot::Receiver<InitialFlushObservation>,
    at_timer: tokio::sync::oneshot::Receiver<bool>,
    release: tokio::sync::oneshot::Sender<()>,
}

impl PumpTimerControl {
    fn new() -> Self {
        let (start_write, before_write) = tokio::sync::oneshot::channel();
        let (snapshot, at_snapshot) = tokio::sync::oneshot::channel();
        let (arrived, at_flush) = tokio::sync::oneshot::channel();
        let (release, blocked) = tokio::sync::oneshot::channel();
        let (timer, at_timer) = tokio::sync::oneshot::channel();
        Self {
            probe: Arc::new(PumpTimerProbe {
                before_write: std::sync::Mutex::new(Some(before_write)),
                snapshot: std::sync::Mutex::new(Some(snapshot)),
                flush: std::sync::Mutex::new(Some(InitialFlushGate {
                    arrived: Some(arrived),
                    release: blocked,
                })),
                timer: std::sync::Mutex::new(Some(timer)),
            }),
            at_snapshot,
            start_write,
            at_flush,
            at_timer,
            release,
        }
    }
}

pub(super) fn observe_outer_create_snapshot(
    pending: Option<&CreateTimerSnapshot>,
    stable_read_admission: bool,
) {
    if !stable_read_admission {
        return;
    }
    let Some((create, deadline)) = pending else {
        return;
    };
    let _ = PUMP_TIMER_PROBE.try_with(|probe| {
        if let Some(snapshot) = probe.snapshot.lock().unwrap().take() {
            let _ = snapshot.send((create.clone(), *deadline));
        }
    });
}

pub(super) async fn wait_outer_timer_write_gate() {
    let gate = PUMP_TIMER_PROBE
        .try_with(|probe| probe.before_write.lock().unwrap().take())
        .ok()
        .flatten();
    if let Some(gate) = gate {
        let _ = gate.await;
    }
}

pub(super) fn observe_outer_create_timer(still_pending: bool) {
    let _ = PUMP_TIMER_PROBE.try_with(|probe| {
        if let Some(timer) = probe.timer.lock().unwrap().take() {
            let _ = timer.send(still_pending);
        }
    });
}

// The native writer still calls start_send on its real upstream sink. Only the
// first poll_flush is held Pending, so the OUTER select must keep polling its
// captured deadline while the admitted native write has not completed.
pub(super) struct TimerWriteSink<S> {
    inner: S,
    gate: Option<InitialFlushGate>,
    operation: Option<InitialFlushObservation>,
}

impl<S> TimerWriteSink<S> {
    pub(super) fn new(inner: S, create: &Arc<CreateLifetime>) -> Self {
        let gate = PUMP_TIMER_PROBE
            .try_with(|probe| probe.flush.lock().unwrap().take())
            .ok()
            .flatten();
        let operation = gate
            .as_ref()
            .map(|_| (Arc::downgrade(create), create.operation.clone()));
        Self {
            inner,
            gate,
            operation,
        }
    }

    pub(super) fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: Sink<Message> + Unpin> Sink<Message> for TimerWriteSink<S> {
    type Error = S::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        Pin::new(&mut self.get_mut().inner).start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        if let Some(gate) = &mut this.gate {
            if let Some(arrived) = gate.arrived.take() {
                let _ = arrived.send(this.operation.take().expect("first flush owns operation"));
            }
            if Pin::new(&mut gate.release).poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.gate = None;
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

#[tokio::test]
async fn n5b_outer_timer_does_not_cancel_admitted_initial_flush() {
    let control = PumpTimerControl::new();
    let mut pair = pair_with_pump_timer_probe(
        Duration::from_secs(60),
        false,
        PARSE_LIMIT,
        UPSTREAM_READ_TIMEOUT,
        false,
        None,
        Some(control.probe.clone()),
    )
    .await;
    let create =
        Message::text(r#"{"type":"response.create","model":"synthetic-model","input":[]}"#);
    pair.client.send(create.clone()).await.unwrap();
    // Reading the create resets client_admitted. Its already-ready budget
    // branch can otherwise win after start_send, rebuilding the select without
    // any pending create. Hold final admission until that native branch has run
    // and the REAL outer select captured this exact pending create's deadline.
    let (captured_create, captured_deadline) =
        tokio::time::timeout(Duration::from_secs(5), control.at_snapshot)
            .await
            .expect("outer select must capture pending create after read admission settles")
            .expect("pending create snapshot was dropped");
    assert!(Instant::now() < captured_deadline);
    control
        .start_write
        .send(())
        .expect("final write admission remains held until the real snapshot");
    let (written_create, operation) =
        tokio::time::timeout(Duration::from_secs(5), control.at_flush)
            .await
            .expect("actual pump must reach admitted native sink flush")
            .expect("native initial flush observation was dropped");
    assert!(
        captured_create.ptr_eq(&written_create),
        "captured admission timer belongs to the admitted native write"
    );
    assert_eq!(captured_deadline, operation.deadline());
    assert!(Instant::now() < operation.deadline());
    assert!(pair.budget.used() >= create.len());

    // Socket setup and the first native start_send have occurred. Advance
    // only this current-thread runtime through the ORIGINAL create deadline.
    tokio::time::pause();
    let until_deadline = operation
        .deadline()
        .saturating_duration_since(Instant::now());
    tokio::time::advance(until_deadline + Duration::from_millis(1)).await;
    let still_pending = tokio::time::timeout(Duration::from_secs(1), control.at_timer)
        .await
        .expect("outer create timer must fire while admitted sink is Pending")
        .expect("outer create timer observation was dropped");
    let completion_allowed = operation.check().is_ok();
    // Resume before any further real socket IO; paused-time auto-advance must
    // not turn a normal socket round trip into an unrelated idle/read timeout.
    tokio::time::resume();
    assert!(!still_pending, "admitted create no longer awaits admission");
    assert!(
        completion_allowed,
        "admitted operation permits completion after expiry"
    );
    assert!(pair.budget.used() >= create.len());
    control
        .release
        .send(())
        .expect("admitted native flush remains owned");
    assert_eq!(next(&mut pair.peer).await, create);

    let created = Message::text(
        r#"{"type":"response.created","response":{"id":"synthetic-timer-response"}}"#,
    );
    pair.peer.send(created.clone()).await.unwrap();
    assert_eq!(
        next(&mut pair.client).await,
        created,
        "admitted create must survive the outer pending-deadline timer",
    );
    let completed = Message::text(
        r#"{"type":"response.completed","response":{"id":"synthetic-timer-response"}}"#,
    );
    pair.peer.send(completed.clone()).await.unwrap();
    assert_eq!(next(&mut pair.client).await, completed);
    let rows = requests(&pair, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["outcome"], "ok");
    assert!(
        !records(&pair)
            .iter()
            .any(|row| row["event"] == "ws_close" && row["reason"] == "request_timeout")
    );
    pair.client.close(None).await.unwrap();
    assert!(matches!(next(&mut pair.peer).await, Message::Close(_)));
}
