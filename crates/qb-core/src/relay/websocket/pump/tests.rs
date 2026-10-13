use super::*;
use axum::{Router, extract::Request, http::StatusCode, response::Response, routing::get};
use std::path::Path;
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
    obs: Arc<Observability>,
    home: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
    relay: Option<Arc<crate::relay::RelayState>>,
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/pump-tests");
    std::fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(root).unwrap();
    let obs = Arc::new(Observability::new(home.path(), None).unwrap());
    let budget = InflightBudget::new(limit);
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
    let handler_relay = relay.clone();
    let router = Router::new().route(
        "/",
        get(move |mut request: Request| {
            let upstream = upstream.clone();
            let obs = handler_obs.clone();
            let budget = handler_budget.clone();
            let relay = handler_relay.clone();
            async move {
                let upstream = upstream.lock().await.take().unwrap();
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
                    run_with_read_timeout(
                        client,
                        upstream,
                        Context {
                            runtime: None,
                            session: Session {
                                key: relay.as_ref().map(|_| "codex:overlap".into()),
                                ..Session::default()
                            },
                            relay,
                            credentials: None,
                            budget,
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
                    )
                    .await;
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
        obs,
        home,
        server,
        relay,
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

#[test]
fn c6_row_13a_only_response_events_commit_the_current_request() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/pump-tests");
    std::fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(root).unwrap();
    let context = Context {
        runtime: None,
        relay: None,
        credentials: None,
        budget: InflightBudget::new(PARSE_LIMIT),
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
    let mut current = CurrentCompletion::new(&context);
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
