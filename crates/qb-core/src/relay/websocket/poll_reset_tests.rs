//! Actual managed data/control upgrades and pump-driven C9, loopback only.
//! Included by websocket::poll_reset::tests; no direct registration calls.
use crate::{
    account_runtime::DeferredResetStatus,
    accounts::{AccountStore, Health, Tokens},
    bindings::{Candidate, LaunchRequest},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    launch_tokens::{LaunchMetadata, LaunchOwnerScope, LaunchPin, LaunchToken},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{RelayState, router},
};
use axum::{
    Json, Router,
    extract::{
        Request, State,
        ws::{Message as UpMessage, WebSocketUpgrade},
    },
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const THREAD: &str = "11111111-2222-4333-8444-555555555555";
const ROOT: &str = "codex:11111111-2222-4333-8444-555555555555";
const ANCHOR: &str = "launch:synthetic-poll-reset";
const MODEL: &str = "synthetic-poll-model";
const CREATED: &str = r#"{"type":"response.created","response":{"id":"poll-original"}}"#;
// Intentionally preserve whitespace, ordering and unknown fields. Parsing and
// reserializing this payload is a regression even if its JSON value is equal.
const QUOTA_ERROR: &str = "  {\"type\":\"error\",\"response_id\":\"poll-original\",\"status\":429,\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"synthetic only\"},\"extra\":17} \n";
type Socket = WebSocketStream<TcpStream>;

fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic poll-reset operation failed"),
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    require(tokio::time::timeout(Duration::from_secs(5), future).await)
}

async fn signal(semaphore: &Semaphore) {
    require(bounded(semaphore.acquire()).await).forget();
}

struct UsageReply {
    body: Value,
    release: oneshot::Receiver<()>,
}

struct Upstream {
    replies: Mutex<VecDeque<UsageReply>>,
    requests: Mutex<Vec<(Method, String)>>,
    arrived: Semaphore,
    returned: Semaphore,
    creates: Semaphore,
    pongs: Semaphore,
    pong_bytes: Mutex<Vec<Vec<u8>>>,
    create_bytes: Mutex<Vec<String>>,
    upgrades: AtomicUsize,
    unexpected: AtomicUsize,
    outbound: mpsc::UnboundedSender<UpMessage>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<UpMessage>>>,
    stop: watch::Sender<bool>,
}

impl Upstream {
    fn enqueue(&self, body: Value) -> oneshot::Sender<()> {
        let (release, receiver) = oneshot::channel();
        self.replies.lock().unwrap().push_back(UsageReply {
            body,
            release: receiver,
        });
        release
    }

    fn send(&self, wire: &str) {
        require(self.outbound.send(UpMessage::Text(wire.to_owned().into())));
    }

    fn assert_gets(&self, expected: usize) {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), expected);
        for (method, path) in requests.iter() {
            assert!(*method == Method::GET && path == "/backend-api/wham/usage");
        }
        assert_eq!(self.unexpected.load(Ordering::SeqCst), 0);
    }
}

async fn usage(State(state): State<Arc<Upstream>>, request: Request) -> Response {
    assert!(request.headers().get("authorization").is_some());
    assert!(request.headers().get("chatgpt-account-id").is_some());
    let path = request.uri().path_and_query().unwrap().as_str().to_owned();
    let method = request.method().clone();
    let body = require(axum::body::to_bytes(request.into_body(), 1024).await);
    assert!(body.is_empty());
    state.requests.lock().unwrap().push((method, path));
    let reply = state.replies.lock().unwrap().pop_front();
    state.arrived.add_permits(1);
    let Some(reply) = reply else {
        state.unexpected.fetch_add(1, Ordering::SeqCst);
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if reply.release.await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    state.returned.add_permits(1);
    Json(reply.body).into_response()
}

async fn upstream_ws(State(state): State<Arc<Upstream>>, upgrade: WebSocketUpgrade) -> Response {
    state.upgrades.fetch_add(1, Ordering::SeqCst);
    let receiver = state.receiver.lock().unwrap().take();
    let Some(mut receiver) = receiver else {
        state.unexpected.fetch_add(1, Ordering::SeqCst);
        return StatusCode::CONFLICT.into_response();
    };
    upgrade
        .on_upgrade(move |mut socket| async move {
            let mut stop = state.stop.subscribe();
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    outgoing = receiver.recv() => {
                        let Some(message) = outgoing else { break };
                        if socket.send(message).await.is_err() { break; }
                    }
                    incoming = socket.next() => match incoming {
                        Some(Ok(UpMessage::Text(text))) => {
                            let value: Value = require(serde_json::from_str(text.as_str()));
                            assert!(value["type"] == "response.create");
                            state.create_bytes.lock().unwrap().push(text.to_string());
                            state.creates.add_permits(1);
                        }
                        Some(Ok(UpMessage::Ping(bytes))) => {
                            if socket.send(UpMessage::Pong(bytes)).await.is_err() { break; }
                        }
                        Some(Ok(UpMessage::Pong(bytes))) => {
                            state.pong_bytes.lock().unwrap().push(bytes.to_vec());
                            state.pongs.add_permits(1);
                        }
                        _ => break,
                    }
                }
            }
        })
        .into_response()
}

struct Fixture {
    relay: Arc<RelayState>,
    address: SocketAddr,
    pin: LaunchPin,
    token: LaunchToken,
    account: String,
    upstream: Arc<Upstream>,
    local: Option<RunningServers>,
    server: Option<JoinHandle<()>>,
    _home: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/n5d-ws-prep/poll-reset-fixtures");
        require(std::fs::create_dir_all(&base));
        let home = require(tempfile::tempdir_in(base));
        let store = Arc::new(require(AccountStore::acquire(home.path())));
        let mut synthetic = crate::accounts::health::tests::fixture();
        synthetic.token_meta.expires_at = Some(require(
            chrono::DateTime::from_timestamp(4_070_908_800, 0).ok_or(()),
        ));
        let account = require(store.save(
            &synthetic,
            &Tokens {
                access_token: "synthetic-poll-access".into(),
                refresh_token: "synthetic-poll-refresh".into(),
                id_token: None,
            },
            None,
        ));
        let listener = require(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await);
        let upstream_base = format!("http://{}", require(listener.local_addr()));
        let (outbound, receiver) = mpsc::unbounded_channel();
        let (stop, _) = watch::channel(false);
        let upstream = Arc::new(Upstream {
            replies: Mutex::default(),
            requests: Mutex::default(),
            arrived: Semaphore::new(0),
            returned: Semaphore::new(0),
            creates: Semaphore::new(0),
            pongs: Semaphore::new(0),
            pong_bytes: Mutex::default(),
            create_bytes: Mutex::default(),
            upgrades: AtomicUsize::new(0),
            unexpected: AtomicUsize::new(0),
            outbound,
            receiver: Mutex::new(Some(receiver)),
            stop,
        });
        let routes = Router::new()
            .route("/responses", get(upstream_ws))
            .route("/backend-api/wham/usage", get(usage))
            .route(
                "/models",
                get(|| async { Json(json!({"models":[{"slug":MODEL}]})) }),
            )
            .fallback({
                let state = upstream.clone();
                move || {
                    state.unexpected.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::BAD_GATEWAY }
                }
            })
            .with_state(upstream.clone());
        let server = tokio::spawn(async move {
            require(axum::serve(listener, routes).await);
        });
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.websocket.idle_close_secs = 60;
        // All possible HTTP/token/catalog endpoints are synthetic loopback.
        config.upstream.codex_base = upstream_base.clone();
        config.upstream.anthropic_base = upstream_base.clone();
        config.upstream.codex_usage_base = upstream_base.clone();
        config.upstream.claude_usage_base = upstream_base.clone();
        #[cfg(debug_assertions)]
        {
            config.upstream.codex_token_endpoint = Some(format!("{upstream_base}/token"));
            config.upstream.claude_token_endpoint = Some(format!("{upstream_base}/token"));
        }
        let obs = Arc::new(require(Observability::new(home.path(), None)));
        let relay = Arc::new(require(RelayState::new(
            store,
            &config,
            obs,
            Arc::new(DebugPins::default()),
        )));
        let models = BTreeSet::from([MODEL.to_owned()]);
        require(relay.bindings.place_launch(
            LaunchRequest {
                anchor_key: ANCHOR,
                resume_key: None,
                model: Some(MODEL),
                model_is_explicit: true,
                account_pref: Some(&account.id),
                allow_move_for_model: false,
            },
            &[Candidate {
                account: account.clone(),
                quota: None,
                inflight: 0,
            }],
            &BTreeMap::from([(account.id.clone(), models.clone())]),
            Utc::now(),
        ));
        require(
            relay
                .bindings
                .resolve_launch(Some(ANCHOR), Some(ROOT), Utc::now()),
        );
        let placement = relay.bindings.begin_request(Some(ROOT), &account.id);
        placement.dispatched();
        drop(placement);
        let token = require(relay.launch_tokens.issue(LaunchMetadata {
            binding_anchor_key: ANCHOR.into(),
            advertised_models: models,
        }));
        let pin = require(relay.launch_tokens.get_or_pin(token.expose_secret()));
        let local = require(
            http::spawn_with_routes(
                &config,
                LocalSecrets {
                    client_key: "synthetic-poll-client".into(),
                    admin_token: "synthetic-poll-admin".into(),
                },
                router(relay.clone()),
                Router::new(),
            )
            .await,
        );
        Self {
            relay,
            address: local.data_addr,
            pin,
            token,
            account: account.id,
            upstream,
            local: Some(local),
            server: Some(server),
            _home: home,
        }
    }

    async fn connect(&self, suffix: &str) -> Socket {
        let mut request = require(
            format!(
                "ws://{}/l/{}/v1/{suffix}",
                self.address,
                self.token.expose_secret()
            )
            .into_client_request(),
        );
        request.headers_mut().insert(
            "authorization",
            require("Bearer synthetic-poll-client".parse()),
        );
        if suffix == "responses" {
            request
                .headers_mut()
                .insert("session-id", require(THREAD.parse()));
        }
        let stream = require(bounded(TcpStream::connect(self.address)).await);
        let (socket, response) = require(bounded(client_async(request, stream)).await);
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        socket
    }

    async fn start(&self) -> (Socket, Socket, LaunchOwnerScope) {
        let launch = require(self.relay.bindings.launch_info(ROOT, Utc::now()));
        assert!(launch.managed && launch.account == self.account);
        assert_eq!(launch.current_model.as_deref(), Some(MODEL));
        let mut owner = self.connect("qb/native-owner").await;
        ping(&mut owner).await;
        let scope = self
            .pin
            .deferred_owner_scope()
            .expect("actual control upgrade owns scope");
        assert!(scope.live() && scope.root().is_none());
        let mut data = self.connect("responses").await;
        let create = json!({"type":"response.create", "model":MODEL, "input":[]}).to_string();
        require(bounded(data.send(Message::Text(create.clone().into()))).await);
        signal(&self.upstream.creates).await;
        // A local control round trip lets the initial physical flush complete;
        // then a real created frame binds the original request's explicit ID.
        ping(&mut data).await;
        self.upstream.send(CREATED);
        assert_text(&mut data, CREATED).await;
        assert_eq!(*self.upstream.create_bytes.lock().unwrap(), vec![create]);
        assert_eq!(self.upstream.upgrades.load(Ordering::SeqCst), 1);
        (owner, data, scope)
    }

    async fn finish(mut self, mut owner: Socket, scope: &LaunchOwnerScope) {
        let notified = scope.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        require(bounded(owner.close(None)).await);
        if scope.live() {
            bounded(notified).await;
        }
        assert!(!scope.live());
        self.upstream.stop.send_replace(true);
        let mut local = self.local.take().unwrap();
        local.shutdown();
        bounded(local.wait_for_listener_close()).await;
        for task in [&mut local.data_task, &mut local.admin_task] {
            task.abort();
            match bounded(task).await {
                Ok(Ok(())) => {}
                Err(error) if error.is_cancelled() => {}
                _ => panic!("synthetic listener shutdown failed"),
            }
        }
        let task = self.server.take().unwrap();
        task.abort();
        assert!(matches!(bounded(task).await, Err(error) if error.is_cancelled()));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.upstream.stop.send_replace(true);
        if let Some(local) = &self.local {
            local.shutdown();
            local.data_task.abort();
            local.admin_task.abort();
        }
        if let Some(task) = &self.server {
            task.abort();
        }
    }
}

async fn ping(socket: &mut Socket) {
    require(bounded(socket.send(Message::Ping(b"poll-pending".to_vec().into()))).await);
    assert!(matches!(bounded(socket.next()).await,
        Some(Ok(Message::Pong(bytes))) if bytes.as_ref() == b"poll-pending"));
}

async fn assert_text(socket: &mut Socket, expected: &str) {
    assert!(matches!(bounded(socket.next()).await,
        Some(Ok(Message::Text(text))) if text.as_bytes() == expected.as_bytes()));
}

async fn close_data(socket: &mut Socket) {
    require(bounded(socket.close(None)).await);
    assert!(matches!(
        bounded(socket.next()).await,
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
}

fn usage_body(primary: u64, secondary: u64, reset: i64) -> Value {
    json!({"rate_limit": {
        "primary_window":{"used_percent":primary,"limit_window_seconds":18000,"reset_at":reset},
        "secondary_window":{"used_percent":secondary,"limit_window_seconds":604800,"reset_at":reset+86400}
    }})
}

#[tokio::test]
async fn managed_physical_error_actual_c9_registers_original_receipt_and_preserves_bytes() {
    let fixture = Fixture::new().await;
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    let release = fixture.upstream.enqueue(usage_body(100, 20, reset));
    let (owner, mut data, scope) = fixture.start().await;
    fixture.upstream.send(QUOTA_ERROR);
    signal(&fixture.upstream.arrived).await;
    fixture.upstream.assert_gets(1);
    assert!(matches!(
        fixture.relay.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Empty
    ));
    // C9 is still blocked; an actual data Ping must be answered immediately.
    ping(&mut data).await;
    require(release.send(()));
    assert_text(&mut data, QUOTA_ERROR).await;
    assert!(
        matches!(fixture.relay.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Waiting { reset_at, .. } if reset_at.timestamp() == reset)
    );
    assert!(
        matches!(require(fixture.relay.store.load(&fixture.account)).health,
        Health::Cooling { reason: crate::accounts::HealthReason::Quota, until, .. }
        if until.timestamp() == reset)
    );
    assert_eq!(scope.root(), Some(ROOT));
    fixture.upstream.assert_gets(1);
    // Closing the data leg after successful handoff must leave the owner's
    // published obligation intact; only owner termination revokes it here.
    close_data(&mut data).await;
    assert!(matches!(
        fixture.relay.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Waiting { .. }
    ));
    fixture.finish(owner, &scope).await;
}

#[tokio::test]
async fn delayed_c9_data_ping_and_close_cancel_without_owner_or_obligation_leak() {
    let fixture = Fixture::new().await;
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    let release = fixture.upstream.enqueue(usage_body(100, 20, reset));
    let (mut owner, mut data, scope) = fixture.start().await;
    fixture.upstream.send(QUOTA_ERROR);
    signal(&fixture.upstream.arrived).await;
    ping(&mut data).await;
    // Close acknowledgement must precede release of the outstanding C9 body.
    // Keeping the owner live prevents owner shutdown from hiding a leak.
    close_data(&mut data).await;
    assert!(scope.live());
    require(release.send(()));
    signal(&fixture.upstream.returned).await;
    ping(&mut owner).await;
    fixture.upstream.assert_gets(1);
    assert!(matches!(
        fixture.relay.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Empty
    ));
    assert!(matches!(
        require(fixture.relay.store.load(&fixture.account)).health,
        Health::Ready
    ));
    assert!(scope.root().is_none());
    fixture.finish(owner, &scope).await;
}

#[tokio::test]
async fn delayed_c9_upstream_ping_gets_local_pong_before_response_release() {
    let fixture = Fixture::new().await;
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    let release = fixture.upstream.enqueue(usage_body(100, 20, reset));
    let (owner, mut data, scope) = fixture.start().await;
    fixture.upstream.send(QUOTA_ERROR);
    signal(&fixture.upstream.arrived).await;
    require(
        fixture
            .upstream
            .outbound
            .send(UpMessage::Ping(b"upstream-c9-pending".to_vec().into())),
    );
    // Do not poll the data client here: it cannot supply an automatic Pong.
    // The pump must read the upstream Ping and flush its own Pong while C9
    // still owns the original error and its HTTP response is withheld.
    signal(&fixture.upstream.pongs).await;
    assert_eq!(
        *fixture.upstream.pong_bytes.lock().unwrap(),
        vec![b"upstream-c9-pending".to_vec()]
    );
    assert_eq!(fixture.upstream.returned.available_permits(), 0);
    assert!(matches!(
        fixture.relay.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Empty
    ));
    require(release.send(()));
    // Forwarding a control frame to the client is permitted, but it must not
    // be needed for the upstream Pong already observed above.
    bounded(async {
        loop {
            match data.next().await {
                Some(Ok(Message::Ping(bytes))) => {
                    assert_eq!(bytes.as_ref(), b"upstream-c9-pending");
                    require(data.flush().await);
                }
                Some(Ok(Message::Text(text))) => {
                    assert_eq!(text.as_bytes(), QUOTA_ERROR.as_bytes());
                    break;
                }
                _ => panic!("missing original error after upstream Ping"),
            }
        }
    })
    .await;
    assert!(matches!(
        fixture.relay.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Waiting { reset_at, .. } if reset_at.timestamp() == reset
    ));
    fixture.upstream.assert_gets(1);
    close_data(&mut data).await;
    fixture.finish(owner, &scope).await;
}

#[tokio::test]
async fn delayed_c9_upstream_close_replays_original_error_and_cannot_register_late() {
    let fixture = Fixture::new().await;
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    let release = fixture.upstream.enqueue(usage_body(100, 20, reset));
    let (mut owner, mut data, scope) = fixture.start().await;
    fixture.upstream.send(QUOTA_ERROR);
    signal(&fixture.upstream.arrived).await;
    require(fixture.upstream.outbound.send(UpMessage::Close(None)));
    // Original error must take the ordinary fallback before Close, without
    // waiting for the poll or publishing an obligation for the closed leg.
    assert_text(&mut data, QUOTA_ERROR).await;
    assert!(matches!(
        bounded(data.next()).await,
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
    assert_eq!(fixture.upstream.returned.available_permits(), 0);
    assert!(scope.live());
    assert!(matches!(
        fixture.relay.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Empty
    ));
    assert!(scope.root().is_none());
    // Keep the actual control owner alive after the late HTTP response, so
    // closing that owner cannot hide a stray asynchronous registration.
    require(release.send(()));
    signal(&fixture.upstream.returned).await;
    ping(&mut owner).await;
    assert!(scope.live());
    assert!(matches!(
        fixture.relay.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Empty
    ));
    assert!(scope.root().is_none());
    fixture.upstream.assert_gets(1);
    assert_eq!(fixture.upstream.upgrades.load(Ordering::SeqCst), 1);
    fixture.finish(owner, &scope).await;
}

#[tokio::test]
async fn partial_c9_cannot_borrow_cached_weekly_or_rewrite_original_error() {
    let fixture = Fixture::new().await;
    let reset = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    // Seed weekly evidence through an actual ordinary C9 GET, never a facade
    // or proof constructor. Nothing from this healthy report can register.
    let seed = fixture.upstream.enqueue(usage_body(20, 30, reset));
    require(seed.send(()));
    require(bounded(fixture.relay.quota.poll(&fixture.account)).await);
    signal(&fixture.upstream.arrived).await;
    signal(&fixture.upstream.returned).await;
    let cached = fixture
        .relay
        .observability
        .quota_snapshot(&fixture.account)
        .unwrap();
    let weekly = cached["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|window| window["name"] == "7d")
        .unwrap()
        .clone();
    // Advance only Tokio's scheduling clock past the capped regular interval,
    // with no live upgrades. Resume before subsequent real loopback socket I/O.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3601)).await;
    tokio::time::resume();
    let mut partial = usage_body(100, 30, reset);
    partial["rate_limit"]
        .as_object_mut()
        .unwrap()
        .remove("secondary_window");
    let release = fixture.upstream.enqueue(partial);
    let (owner, mut data, scope) = fixture.start().await;
    fixture.upstream.send(QUOTA_ERROR);
    signal(&fixture.upstream.arrived).await;
    require(release.send(()));
    assert_text(&mut data, QUOTA_ERROR).await;
    assert!(matches!(
        fixture.relay.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Empty
    ));
    let merged = fixture
        .relay
        .observability
        .quota_snapshot(&fixture.account)
        .unwrap();
    let windows = merged["windows"].as_array().unwrap();
    assert!(
        windows
            .iter()
            .any(|window| window["name"] == "5h" && window["used_pct"] == 100.0)
    );
    assert!(windows.iter().any(|window| window["name"] == "7d"
        && window["used_pct"] == weekly["used_pct"]
        && window["reset_at"] == weekly["reset_at"]));
    assert!(scope.root().is_none());
    fixture.upstream.assert_gets(2);
    close_data(&mut data).await;
    fixture.finish(owner, &scope).await;
}
