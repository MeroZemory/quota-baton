//! Loopback-only contract tests; registered by the implementation owner.
use super::{RoutingCliError, request_value};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
    routing::any,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::task::JoinHandle;

const TOKEN: &str = "synthetic-routing-admin-token";
const DELAYS: [Duration; 4] = [Duration::ZERO; 4];

struct Seen {
    peer: SocketAddr,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
}

struct Reply {
    status: StatusCode,
    body: String,
    // No Content-Length and no EOF: collecting the whole error must time out.
    hold_open: bool,
}

impl Reply {
    fn json(status: StatusCode, body: Value) -> Self {
        Self {
            status,
            body: body.to_string(),
            hold_open: false,
        }
    }

    fn error(status: StatusCode, message: &str) -> Self {
        Self::json(status, json!({"error": {"message": message}}))
    }
}

#[derive(Clone)]
struct FixtureState {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: tokio::sync::watch::Sender<bool>,
    received: Arc<tokio::sync::Notify>,
}

struct Fixture {
    address: SocketAddr,
    url: String,
    state: FixtureState,
    task: Option<JoinHandle<()>>,
}

async fn respond(
    State(state): State<FixtureState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.seen.lock().unwrap().push(Seen {
        peer,
        method,
        headers,
        body,
    });
    state.received.notify_one();
    // Extra attempts remain observable instead of hanging on an exhausted script.
    let reply = state
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| {
            Reply::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unexpected extra attempt",
            )
        });
    let body = if reply.hold_open {
        let mut stop = state.stop.subscribe();
        Body::from_stream(async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::from(reply.body));
            let stopping = *stop.borrow_and_update();
            if !stopping {
                let _ = stop.changed().await;
            }
        })
    } else {
        Body::from(reply.body)
    };
    Response::builder()
        .status(reply.status)
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}

impl Fixture {
    async fn start(replies: Vec<Reply>) -> Self {
        Self::start_at("127.0.0.1:0".parse().unwrap(), replies).await
    }

    async fn start_at(address: SocketAddr, replies: Vec<Reply>) -> Self {
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let address = listener.local_addr().unwrap();
        let url = format!("http://{address}/admin/v1/routing");
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let state = FixtureState {
            replies: Arc::new(Mutex::new(replies.into())),
            seen: Arc::new(Mutex::new(Vec::new())),
            stop,
            received: Arc::new(tokio::sync::Notify::new()),
        };
        let app = Router::new()
            .route("/admin/v1/routing", any(respond))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = stopped.changed().await;
            })
            .await
            .unwrap();
        });
        Self {
            address,
            url,
            state,
            task: Some(task),
        }
    }

    async fn request(&self, body: Option<&Value>, delays: &[Duration]) -> anyhow::Result<Value> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(3),
            request_value(&client, &self.url, TOKEN, body, delays),
        )
        .await
        .expect("loopback request exceeded test deadline")
    }

    async fn finish(mut self) -> Vec<Seen> {
        let mut task = self.task.take().unwrap();
        let _ = self.state.stop.send(true);
        match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
            Ok(result) => result.unwrap(),
            Err(_) => {
                task.abort();
                let _ = task.await;
                panic!("fixture failed to stop its connections");
            }
        }
        std::mem::take(&mut *self.state.seen.lock().unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Also release the listener on assertion failure; normal cleanup awaits it.
        let _ = self.state.stop.send(true);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn urgent(flag: &str) -> Value {
    json!({"kind": "urgent", "id": "synthetic-root", "on_5h_limit": flag})
}

fn assert_requests(seen: &[Seen], count: usize, body: Option<&Value>) {
    assert_eq!(seen.len(), count);
    for request in seen {
        // Boolean checks deliberately avoid printing authorization/body captures.
        assert!(
            request
                .headers
                .get("authorization")
                .is_some_and(|v| v == format!("Bearer {TOKEN}").as_str())
        );
        assert!(
            request
                .headers
                .get("connection")
                .is_some_and(|v| v == "close")
        );
        assert_eq!(
            request.method,
            if body.is_some() {
                Method::POST
            } else {
                Method::GET
            }
        );
        if let Some(body) = body {
            assert!(serde_json::from_slice::<Value>(&request.body).unwrap() == *body);
        } else {
            assert!(request.body.is_empty());
        }
    }
}

fn assert_safe_error(error: &anyhow::Error, status: u16, forbidden: &[&str]) {
    assert!(error.downcast_ref::<RoutingCliError>().is_some());
    for message in [error.to_string(), crate::friendly_error(error)] {
        assert!(message.contains(&status.to_string()));
        assert!(!message.chars().any(char::is_control));
        for secret in forbidden {
            assert!(!message.contains(secret), "unsafe error text");
        }
    }
}

#[tokio::test]
async fn urgent_conflict_retries_identical_absolute_wait_and_move_on_new_connections() {
    for flag in ["wait", "move"] {
        let fixture = Fixture::start(vec![
            Reply::error(StatusCode::CONFLICT, "routing changed; retry"),
            Reply::json(StatusCode::OK, json!({"ok": true})),
        ])
        .await;
        let body = urgent(flag);
        let result = fixture.request(Some(&body), &DELAYS).await;
        let seen = fixture.finish().await;
        assert_eq!(result.unwrap(), json!({"ok": true}));
        assert_requests(&seen, 2, Some(&body));
        assert!(seen[0].body == seen[1].body);
        assert_ne!(seen[0].peer, seen[1].peer);
    }
}

#[tokio::test]
async fn known_conflict_survives_closed_listener_gap_before_successor_rebinds() {
    for flag in ["wait", "move"] {
        let incumbent = Fixture::start(vec![Reply {
            status: StatusCode::CONFLICT,
            body: json!({"error":{"message":"handoff in progress"}}).to_string(),
            // Delay EOF until finish closes the incumbent, keeping the request
            // inside response_error while the test initiates listener shutdown.
            hold_open: true,
        }])
        .await;
        let address = incumbent.address;
        let url = incumbent.url.clone();
        let received = incumbent.state.received.clone();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(50))
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let body = urgent(flag);
        // Attempt two hits the closed port at ~100 ms with a bounded connect
        // timeout; attempt three waits one second, past the 500 ms handoff gap.
        let delays = [
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_millis(100),
            Duration::from_millis(100),
        ];
        let request = request_value(&client, &url, TOKEN, Some(&body), &delays);
        let handoff = async {
            tokio::time::timeout(Duration::from_secs(1), received.notified())
                .await
                .expect("incumbent did not receive the urgent request");
            let before = incumbent.finish().await;
            // This is a real unavailable TCP endpoint, not a scripted status.
            // Windows can defer a refused connect. Bound the connect itself,
            // rather than requiring the kernel to refuse within 100 ms.
            let probe = tokio::time::timeout(Duration::from_millis(300), client.get(&url).send())
                .await
                .expect("closed endpoint exceeded the explicit connect timeout")
                .expect_err("incumbent listener remained open");
            assert!(
                probe.is_connect(),
                "closed endpoint was not a connect failure"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
            let successor = Fixture::start_at(
                address,
                vec![Reply::json(StatusCode::OK, json!({"ok": true}))],
            )
            .await;
            (before, successor)
        };
        let (result, (before, successor)) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(request, handoff)
        })
        .await
        .expect("handoff retry exceeded the regression deadline");
        let after = successor.finish().await;
        assert_eq!(result.unwrap(), json!({"ok": true}));
        assert_requests(&before, 1, Some(&body));
        assert_requests(&after, 1, Some(&body));
        assert!(before[0].body == after[0].body);
        assert_ne!(before[0].peer, after[0].peer);
    }
}

#[tokio::test]
async fn persistent_conflict_has_exact_retry_budget_and_friendly_core_reason() {
    for budget in [0, 1, 4] {
        let fixture = Fixture::start(
            (0..budget + 2)
                .map(|_| Reply::error(StatusCode::CONFLICT, "routing revision changed"))
                .collect(),
        )
        .await;
        let body = urgent("wait");
        let result = fixture.request(Some(&body), &DELAYS[..budget]).await;
        let seen = fixture.finish().await;
        let error = result.unwrap_err();
        assert_requests(&seen, budget + 1, Some(&body));
        assert_safe_error(&error, 409, &[TOKEN]);
        assert!(error.to_string().contains("routing revision changed"));
        assert!(crate::friendly_error(&error).contains("routing revision changed"));
    }
}

#[tokio::test]
async fn other_statuses_and_nonurgent_conflicts_are_never_retried() {
    let cases = [
        (400, Some(urgent("wait"))),
        (401, Some(urgent("move"))),
        (500, Some(urgent("wait"))),
        (409, None),
        (
            409,
            Some(json!({"kind":"project", "id":"synthetic-project", "settings":{"mode":"strict"}})),
        ),
        (
            409,
            Some(json!({"kind":"session", "id":"synthetic-root", "inherit":true})),
        ),
        (409, Some(json!({"kind":"urgent", "id":"synthetic-root"}))),
        (409, Some(urgent("toggle"))),
    ];
    for (status, body) in cases {
        let fixture = Fixture::start(vec![
            Reply::error(StatusCode::from_u16(status).unwrap(), "mutation rejected"),
            Reply::json(StatusCode::OK, json!({"ok":true})),
        ])
        .await;
        let result = fixture.request(body.as_ref(), &DELAYS).await;
        let seen = fixture.finish().await;
        assert_requests(&seen, 1, body.as_ref());
        assert_safe_error(&result.unwrap_err(), status, &[TOKEN]);
    }
}

#[tokio::test]
async fn malformed_and_oversized_errors_never_expose_raw_body() {
    let canary = "RAW_ERROR_CANARY";
    for raw in [
        format!("<html>{canary} {TOKEN}</html>"),
        json!({"message":canary}).to_string(),
        json!({"error":{"message":format!("{canary}{}", "x".repeat(8192))}}).to_string(),
    ] {
        let fixture = Fixture::start(vec![Reply {
            status: StatusCode::BAD_REQUEST,
            body: raw,
            hold_open: false,
        }])
        .await;
        let body = urgent("wait");
        let result = fixture.request(Some(&body), &DELAYS).await;
        let seen = fixture.finish().await;
        assert_requests(&seen, 1, Some(&body));
        assert_safe_error(&result.unwrap_err(), 400, &[TOKEN, canary]);
    }
}

#[tokio::test]
async fn error_reader_stops_after_8192_bytes_without_waiting_for_eof() {
    let fixture = Fixture::start(vec![Reply {
        status: StatusCode::BAD_REQUEST,
        body: format!("RAW_STREAM_CANARY{}", "x".repeat(8193)),
        hold_open: true,
    }])
    .await;
    let body = urgent("wait");
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.request(Some(&body), &DELAYS),
    )
    .await;
    let seen = fixture.finish().await;
    assert_requests(&seen, 1, Some(&body));
    let error = result
        .expect("error reader waited for EOF beyond its byte cap")
        .unwrap_err();
    assert_safe_error(&error, 400, &[TOKEN, "RAW_STREAM_CANARY"]);
}

#[tokio::test]
async fn error_reader_accepts_valid_json_at_exact_8192_byte_boundary() {
    let mut raw = json!({"error":{"message":"boundary reason"}}).to_string();
    let padding = " ".repeat(8192 - raw.len());
    // JSON trailing whitespace counts toward the wire body cap too.
    raw.push_str(&padding);
    assert_eq!(raw.len(), 8192);
    let fixture = Fixture::start(vec![Reply {
        status: StatusCode::BAD_REQUEST,
        body: raw,
        hold_open: false,
    }])
    .await;
    let body = urgent("wait");
    let result = fixture.request(Some(&body), &DELAYS).await;
    let seen = fixture.finish().await;
    assert_requests(&seen, 1, Some(&body));
    let error = result.unwrap_err();
    assert_safe_error(&error, 400, &[TOKEN]);
    assert!(crate::friendly_error(&error).contains("boundary reason"));
}

#[tokio::test]
async fn valid_core_reason_redacts_token_and_flattens_controls() {
    let fixture = Fixture::start(vec![Reply::error(
        StatusCode::UNAUTHORIZED,
        &format!("denied {TOKEN}\nnext\r\t\u{001b}[31m"),
    )])
    .await;
    let body = urgent("move");
    let result = fixture.request(Some(&body), &DELAYS).await;
    let seen = fixture.finish().await;
    assert_requests(&seen, 1, Some(&body));
    let error = result.unwrap_err();
    assert_safe_error(&error, 401, &[TOKEN]);
    assert!(crate::friendly_error(&error).contains("denied"));
    assert!(crate::friendly_error(&error).contains("next"));
}

#[tokio::test]
async fn core_reason_is_capped_at_512_unicode_characters() {
    let fixture = Fixture::start(vec![Reply::error(
        StatusCode::BAD_REQUEST,
        &format!("{}TAIL_AFTER_MESSAGE_CAP", "界".repeat(512)),
    )])
    .await;
    let result = fixture.request(Some(&urgent("wait")), &DELAYS).await;
    let seen = fixture.finish().await;
    assert_eq!(seen.len(), 1);
    let error = result.unwrap_err();
    assert_safe_error(&error, 400, &[TOKEN, "TAIL_AFTER_MESSAGE_CAP"]);
    for message in [error.to_string(), crate::friendly_error(&error)] {
        let count = message.chars().filter(|c| *c == '界').count();
        assert!(count > 0 && count <= 512);
    }
}

#[tokio::test]
async fn successful_get_preserves_value_but_rejects_sparse_typed_settings() {
    let valid = json!({
        "revision":1,
        "defaults":{"mode":"recover","reset_wait_secs":13,"on_5h_limit":"wait"},
        "projects":{}, "sessions":{}, "observed_sessions":[], "forecast_shadow":[]
    });
    let mut sparse = valid.clone();
    sparse["defaults"]
        .as_object_mut()
        .unwrap()
        .remove("on_5h_limit");
    for (body, accepted) in [(valid.clone(), true), (sparse, false)] {
        let fixture = Fixture::start(vec![Reply::json(StatusCode::OK, body)]).await;
        let result = fixture.request(None, &DELAYS).await;
        let seen = fixture.finish().await;
        assert_requests(&seen, 1, None);
        if accepted {
            assert_eq!(result.unwrap(), valid);
        } else {
            assert!(result.is_err());
        }
    }
}
