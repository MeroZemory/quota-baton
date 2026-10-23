//! Real loopback upgrades and genuine physical qualification, with synthetic
//! credentials only. Never format scoped URLs, frames, or transport errors.

use super::{PendingPublication, next_reset_wake};
use crate::{
    account_runtime::{AccountRuntime, DeferredResetId, DeferredResetStatus, HealthSignal},
    accounts::{AccountStore, HealthReason, Provider, Tokens},
    bindings::{Candidate, HttpWaitRequest, LaunchRequest},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    issue_operation::IssueOperation,
    launch_tokens::{LaunchMetadata, LaunchOwnerScope, LaunchPin, LaunchToken},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{RelayState, router},
};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use qb_proto::native_reset::{Command, Event, Refusal, Reply, Request, VERSION};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpStream, task::JoinHandle, time::Instant};
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{
        Error as WsError, Message, client::IntoClientRequest,
        handshake::client::Request as UpgradeRequest,
    },
};

const THREAD: &str = "11111111-2222-4333-8444-555555555555";
const OTHER_THREAD: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const ROOT: &str = "codex:11111111-2222-4333-8444-555555555555";
const ANCHOR: &str = "launch:synthetic-native-control";
const MODEL: &str = "synthetic-model";
type Socket = WebSocketStream<TcpStream>;

fn require<T, E>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("synthetic control operation failed"),
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    require(tokio::time::timeout(Duration::from_secs(5), future).await)
}

/// Every server task is retained. Success paths abort AND join; unwind still
/// aborts instead of detaching a listener. No server owns a real configuration.
struct Servers(Vec<JoinHandle<()>>);

impl Servers {
    async fn stop(mut self) {
        for task in self.0.drain(..) {
            task.abort();
            let result = task.await;
            assert!(matches!(result, Err(error) if error.is_cancelled()));
        }
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

struct Fixture {
    state: Arc<RelayState>,
    address: SocketAddr,
    pin: LaunchPin,
    token: LaunchToken,
    account: String,
    upstream_calls: Arc<AtomicUsize>,
    local_servers: RunningServers,
    servers: Servers,
    _home: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r13-part-c/n5d-native/control-fixtures");
        require(std::fs::create_dir_all(&base));
        let home = require(tempfile::tempdir_in(base));
        let store = Arc::new(require(AccountStore::acquire(home.path())));
        let account = require(store.save(
            &crate::accounts::health::tests::fixture(),
            &Tokens {
                access_token: "synthetic-access".into(),
                refresh_token: "synthetic-refresh".into(),
                id_token: None,
            },
            None,
        ));
        let upstream = require(tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await);
        let upstream_address = require(upstream.local_addr());
        let upstream_calls = Arc::new(AtomicUsize::new(0));
        let calls = upstream_calls.clone();
        let upstream_router = axum::Router::new().fallback(move || {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                axum::http::StatusCode::BAD_GATEWAY
            }
        });
        let mut servers = Servers(Vec::new());
        servers.0.push(tokio::spawn(async move {
            require(axum::serve(upstream, upstream_router).await);
        }));
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.codex_base = format!("http://{upstream_address}");
        config.upstream.anthropic_base = format!("http://{upstream_address}");
        let observability = Arc::new(require(Observability::new(home.path(), None)));
        let state = Arc::new(require(RelayState::new(
            store,
            &config,
            observability,
            Arc::new(DebugPins::default()),
        )));
        let models = BTreeSet::from([MODEL.to_owned()]);
        require(state.bindings.place_launch(
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
            state
                .bindings
                .resolve_launch(Some(ANCHOR), Some(ROOT), Utc::now()),
        );
        let placement = state.bindings.begin_request(Some(ROOT), &account.id);
        placement.dispatched();
        drop(placement);
        let token = require(state.launch_tokens.issue(LaunchMetadata {
            binding_anchor_key: ANCHOR.into(),
            advertised_models: models,
        }));
        let pin = require(state.launch_tokens.get_or_pin(token.expose_secret()));
        // Use the ordinary data-port authentication/Host/Origin middleware,
        // not a test imitation of its checks. Secrets are synthetic and in memory.
        let local_servers = require(
            http::spawn_with_routes(
                &config,
                LocalSecrets {
                    client_key: "synthetic-control-client".into(),
                    admin_token: "synthetic-control-admin".into(),
                },
                router(state.clone()),
                axum::Router::new(),
            )
            .await,
        );
        let address = local_servers.data_addr;
        Self {
            state,
            address,
            pin,
            token,
            account: account.id,
            upstream_calls,
            local_servers,
            servers,
            _home: home,
        }
    }

    fn upgrade_request(&self) -> UpgradeRequest {
        let mut request = require(
            format!(
                "ws://{}/l/{}/v1/qb/native-owner",
                self.address,
                self.token.expose_secret(),
            )
            .into_client_request(),
        );
        let mut bearer: axum::http::HeaderValue =
            require("Bearer synthetic-control-client".parse());
        bearer.set_sensitive(true);
        request.headers_mut().insert("authorization", bearer);
        request
    }

    async fn connect(&self) -> Socket {
        let stream = require(bounded(TcpStream::connect(self.address)).await);
        let (socket, _) = require(bounded(client_async(self.upgrade_request(), stream)).await);
        socket
    }

    fn scope(&self) -> LaunchOwnerScope {
        match self.pin.deferred_owner_scope() {
            Some(scope) => scope,
            None => panic!("actual upgrade did not create an owner"),
        }
    }

    /// Complete physical header/body adapter, as in the runtime deferred-reset
    /// fixtures. No proof/evidence constructor or manual owner lease is used.
    async fn register(&self) -> (DeferredResetId, LaunchOwnerScope) {
        self.register_after(120).await
    }

    async fn register_after(&self, seconds: i64) -> (DeferredResetId, LaunchOwnerScope) {
        let runtime = &self.state.accounts;
        let bindings = &self.state.bindings;
        let reset = require(
            DateTime::from_timestamp(
                (Utc::now() + chrono::Duration::seconds(seconds)).timestamp(),
                0,
            )
            .ok_or(()),
        );
        let mut headers = axum::http::HeaderMap::new();
        for (name, value) in [
            ("x-codex-primary-used-percent", "100".into()),
            ("x-codex-primary-window-minutes", "300".into()),
            ("x-codex-primary-reset-at", reset.timestamp().to_string()),
            ("x-codex-secondary-used-percent", "20".into()),
            ("x-codex-secondary-window-minutes", "10080".into()),
            (
                "x-codex-secondary-reset-at",
                (reset + chrono::Duration::days(1)).timestamp().to_string(),
            ),
        ] {
            headers.insert(name, require(value.parse()));
        }
        let origin = require(runtime.capture_http_origin(&self.account).await.ok_or(()));
        let asof = bindings.begin_usage_observation_for(&self.account);
        let (_, pending) = runtime
            .observe_http_429_headers_pending(
                &self.account,
                Provider::Codex,
                &headers,
                Some(MODEL),
                (bindings, asof),
                Some(origin),
            )
            .await;
        let body = require(serde_json::to_vec(&serde_json::json!({
            "error": {"type": "usage_limit_reached", "resets_at": reset.timestamp()}
        })));
        let body = AccountRuntime::qualify_http_429_body(Some(&body), Some(MODEL));
        let observed = require(
            require(
                runtime
                    .finish_http_429_report_observed(
                        &self.account,
                        pending,
                        Some(reset),
                        Some(HealthSignal::Cooling {
                            reason: HealthReason::Quota,
                            until: reset,
                            scope: "account".into(),
                        }),
                        body,
                    )
                    .await,
            )
            .ok_or(()),
        );
        let operation = IssueOperation::new(Instant::now() + Duration::from_secs(1));
        let _caller = operation.caller_guard();
        require(
            require(runtime.register_http_deferred_reset(
                bindings,
                HttpWaitRequest {
                    provider: Provider::Codex,
                    key: Some(ROOT),
                    model: Some(MODEL),
                    expected_source: Some(&self.account),
                    operation: &operation,
                },
                &observed,
                &self.pin,
            ))
            .ok_or(()),
        )
    }

    async fn finish(self) {
        assert!(self.pin.deferred_owner_scope().is_none());
        assert!(self.upstream_calls.load(Ordering::SeqCst) == 0);
        self.local_servers.shutdown();
        bounded(self.local_servers.wait_for_listener_close()).await;
        let mut local_servers = self.local_servers;
        for task in [&mut local_servers.data_task, &mut local_servers.admin_task] {
            task.abort();
            match bounded(task).await {
                Ok(Ok(())) => {}
                Err(error) if error.is_cancelled() => {}
                _ => panic!("synthetic authenticated listener failed"),
            }
        }
        self.servers.stop().await;
    }
}

async fn ping(socket: &mut Socket) {
    require(bounded(socket.send(Message::Ping(b"phase".to_vec().into()))).await);
    assert!(matches!(bounded(socket.next()).await,
        Some(Ok(Message::Pong(bytes))) if bytes.as_ref() == b"phase"));
}

async fn send_command(socket: &mut Socket, id: u64, command: Command) {
    let wire = require(serde_json::to_string(&Request {
        version: VERSION,
        request_id: id,
        command,
    }));
    require(bounded(socket.send(Message::Text(wire.into()))).await);
}

async fn reply(socket: &mut Socket, id: u64) -> Reply {
    let wire = match bounded(socket.next()).await {
        Some(Ok(Message::Text(wire))) => wire,
        _ => panic!("missing correlated control reply"),
    };
    let reply: Reply = require(serde_json::from_str(&wire));
    assert!(reply.valid_shape() && reply.request_id == Some(id));
    reply
}

async fn terminal(socket: &mut Socket) {
    match bounded(socket.next()).await {
        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {}
        _ => panic!("closed control leaked an application frame"),
    }
}

async fn close_owner(socket: &mut Socket, scope: &LaunchOwnerScope) {
    let notified = scope.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    require(bounded(socket.close(None)).await);
    if scope.live() {
        bounded(notified).await;
    }
    assert!(!scope.live());
    terminal(socket).await;
}

#[tokio::test]
async fn actual_idle_owner_close_revokes_and_fresh_epoch_can_reopen() {
    let fixture = Fixture::new().await;
    let mut socket = fixture.connect().await;
    ping(&mut socket).await;
    let scope = fixture.scope();
    assert!(scope.live() && scope.root().is_none());
    // A second actual upgrade cannot take ownership while the first is idle.
    let peer = require(bounded(TcpStream::connect(fixture.address)).await);
    let denied = bounded(client_async(fixture.upgrade_request(), peer)).await;
    assert!(matches!(denied, Err(WsError::Http(response))
        if response.status().as_u16() == 409));
    assert!(fixture.scope().same_owner(&scope));
    close_owner(&mut socket, &scope).await;
    let mut reopened = fixture.connect().await;
    ping(&mut reopened).await;
    let next = fixture.scope();
    assert!(next.live() && !next.same_owner(&scope) && !scope.live());
    close_owner(&mut reopened, &next).await;
    fixture.finish().await;
}

#[tokio::test]
async fn actual_data_auth_rejects_missing_wrong_and_admin_bearer_before_owner() {
    let fixture = Fixture::new().await;
    for bearer in [
        None,
        Some("Bearer synthetic-wrong"),
        Some("Bearer synthetic-control-admin"),
    ] {
        let mut request = fixture.upgrade_request();
        request.headers_mut().remove("authorization");
        if let Some(value) = bearer {
            let mut value: axum::http::HeaderValue = require(value.parse());
            value.set_sensitive(true);
            request.headers_mut().insert("authorization", value);
        }
        let peer = require(bounded(TcpStream::connect(fixture.address)).await);
        let result = bounded(client_async(request, peer)).await;
        assert!(matches!(result, Err(WsError::Http(response))
            if response.status().as_u16() == 401));
        assert!(fixture.pin.deferred_owner_scope().is_none());
    }
    // Same genuine scoped token, now with the ordinary client credential.
    let mut socket = fixture.connect().await;
    ping(&mut socket).await;
    let scope = fixture.scope();
    assert!(scope.live());
    close_owner(&mut socket, &scope).await;
    fixture.finish().await;
}

#[tokio::test]
async fn actual_upgrade_rejects_unissued_token_origin_query_and_protocol() {
    let fixture = Fixture::new().await;
    for case in 0..6 {
        let mut request = fixture.upgrade_request();
        let expected = match case {
            0 => {
                // Change a byte of a real token: well shaped but never issued.
                let mut token = fixture.token.expose_secret().as_bytes().to_vec();
                token[0] = if token[0] == b'A' { b'B' } else { b'A' };
                let token = require(String::from_utf8(token));
                *request.uri_mut() = require(
                    format!("ws://{}/l/{token}/v1/qb/native-owner", fixture.address,).parse(),
                );
                403
            }
            1 => {
                request.headers_mut().insert("origin", require("".parse()));
                403 // The ordinary outer data listener rejects Origin first.
            }
            2 => {
                let uri = format!("{}?synthetic=1", request.uri());
                *request.uri_mut() = require(uri.parse());
                400
            }
            3 => {
                request
                    .headers_mut()
                    .insert("sec-websocket-protocol", require("synthetic".parse()));
                400
            }
            4 => {
                request.headers_mut().insert(
                    "sec-websocket-extensions",
                    require("permessage-deflate".parse()),
                );
                400
            }
            _ => {
                *request.uri_mut() =
                    require(format!("ws://{}/v1/qb/native-owner", fixture.address,).parse());
                403
            }
        };
        let peer = require(bounded(TcpStream::connect(fixture.address)).await);
        let response = bounded(client_async(request, peer)).await;
        assert!(matches!(response, Err(WsError::Http(response))
            if response.status().as_u16() == expected));
        assert!(fixture.pin.deferred_owner_scope().is_none());
    }
    fixture.finish().await;
}

#[tokio::test]
async fn actual_replayed_id_and_thread_switch_close_without_payload_echo() {
    for replay in [true, false] {
        let fixture = Fixture::new().await;
        let mut socket = fixture.connect().await;
        ping(&mut socket).await;
        let scope = fixture.scope();
        send_command(
            &mut socket,
            1,
            Command::Observe {
                thread_id: THREAD.into(),
            },
        )
        .await;
        assert!(matches!(
            reply(&mut socket, 1).await.event,
            Event::Refused {
                reason: Refusal::Unavailable
            }
        ));
        let notified = scope.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        send_command(
            &mut socket,
            if replay { 1 } else { 2 },
            Command::Observe {
                thread_id: if replay { THREAD } else { OTHER_THREAD }.into(),
            },
        )
        .await;
        if scope.live() {
            bounded(notified).await;
        }
        assert!(!scope.live());
        terminal(&mut socket).await;
        fixture.finish().await;
    }
}

#[tokio::test]
async fn actual_malformed_unknown_fields_and_invalid_shape_close_without_echo() {
    for wire in [
        "synthetic-private-invalid-json",
        r#"{"version":1,"request_id":1,"command":{"type":"observe","thread_id":"11111111-2222-4333-8444-555555555555"},"private":"must-not-echo"}"#,
        r#"{"version":1,"request_id":0,"command":{"type":"cancel","episode":1}}"#,
        r#"{"version":1,"request_id":1,"command":{"type":"observe","thread_id":"synthetic-not-a-native-uuid"}}"#,
    ] {
        let fixture = Fixture::new().await;
        let mut socket = fixture.connect().await;
        ping(&mut socket).await;
        let scope = fixture.scope();
        let notified = scope.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        require(bounded(socket.send(Message::text(wire))).await);
        if scope.live() {
            bounded(notified).await;
        }
        assert!(!scope.live());
        terminal(&mut socket).await;
        fixture.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_registered_episode_before_observe_terminates_actual_owner() {
    let fixture = Fixture::new().await;
    let mut socket = fixture.connect().await;
    // This proves the task is parked on real socket/notification work, with no
    // Observe ever sent. Registration claims its core-confirmed root below.
    ping(&mut socket).await;
    let (id, scope) = fixture.register().await;
    assert!(scope.live() && scope.matches_thread(THREAD));
    let notified = scope.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    drop(PendingPublication::new(
        fixture.state.accounts.clone(),
        scope.clone(),
        id,
    ));
    // A ready cancelled watcher must terminate, not spin ahead of Close/Observe.
    // Notification is from actual control-task owner Drop, not our cancellation.
    if scope.live() {
        bounded(notified).await;
    }
    assert!(!scope.live());
    terminal(&mut socket).await;
    assert!(matches!(
        fixture.state.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Cancelled
    ));
    fixture.finish().await;
}

#[tokio::test]
async fn kept_physical_publication_survives_to_correlated_waiting_then_owner_close() {
    let fixture = Fixture::new().await;
    let mut socket = fixture.connect().await;
    ping(&mut socket).await;
    let (id, scope) = fixture.register().await;
    PendingPublication::new(fixture.state.accounts.clone(), scope.clone(), id).keep();
    send_command(
        &mut socket,
        1,
        Command::Observe {
            thread_id: THREAD.into(),
        },
    )
    .await;
    assert!(matches!(reply(&mut socket, 1).await.event,
        Event::Waiting { episode, .. } if episode == id.as_u64()));
    assert!(scope.live());
    assert!(
        matches!(fixture.state.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Waiting { id: waiting, .. } if waiting == id)
    );
    close_owner(&mut socket, &scope).await;
    assert!(matches!(
        fixture.state.accounts.deferred_reset_status(&scope, THREAD),
        DeferredResetStatus::Cancelled
    ));
    fixture.finish().await;
}

#[tokio::test]
async fn physically_registered_wrong_thread_never_receives_waiting_or_grant() {
    let fixture = Fixture::new().await;
    let mut socket = fixture.connect().await;
    ping(&mut socket).await;
    let (id, scope) = fixture.register().await;
    PendingPublication::new(fixture.state.accounts.clone(), scope.clone(), id).keep();
    let notified = scope.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    send_command(
        &mut socket,
        1,
        Command::Observe {
            thread_id: OTHER_THREAD.into(),
        },
    )
    .await;
    assert!(matches!(
        reply(&mut socket, 1).await.event,
        Event::Refused {
            reason: Refusal::Cancelled
        }
    ));
    if scope.live() {
        bounded(notified).await;
    }
    assert!(!scope.live());
    terminal(&mut socket).await;
    fixture.finish().await;
}

#[tokio::test]
async fn actual_consumed_grant_keeps_owner_live_for_ping_and_refuses_duplicate() {
    // Expiry can notify the watcher again. Only same-episode Due hints may
    // precede the correlated reply; the entire exchange has one bounded wait.
    async fn phase_reply(socket: &mut Socket, request_id: u64, episode: u64) -> Reply {
        bounded(async {
            loop {
                let wire = match socket.next().await {
                    Some(Ok(Message::Text(wire))) => wire,
                    _ => panic!("missing grant-lifetime phase reply"),
                };
                let reply: Reply = require(serde_json::from_str(&wire));
                assert!(reply.valid_shape());
                if reply.request_id == Some(request_id) {
                    return reply;
                }
                assert!(
                    reply.request_id.is_none()
                        && matches!(reply.event, Event::Due { episode: due } if due == episode)
                );
            }
        })
        .await
    }

    let fixture = Fixture::new().await;
    let mut socket = fixture.connect().await;
    ping(&mut socket).await;
    let (id, scope) = fixture.register_after(3).await;
    let (reset_at, wake) = match fixture.state.accounts.deferred_reset_status(&scope, THREAD) {
        DeferredResetStatus::Waiting {
            id: waiting,
            reset_at,
            wake,
        } if waiting == id => (reset_at, wake),
        _ => panic!("short physical episode did not register"),
    };
    PendingPublication::new(fixture.state.accounts.clone(), scope.clone(), id).keep();
    send_command(
        &mut socket,
        1,
        Command::Observe {
            thread_id: THREAD.into(),
        },
    )
    .await;
    assert!(matches!(reply(&mut socket, 1).await.event,
        Event::Waiting { episode, reset_unix_ms }
            if episode == id.as_u64() && reset_unix_ms == reset_at.timestamp_millis()));

    // Actual Due is the timer/notification barrier; no synthetic clock advance,
    // sleep, or newly minted reset replaces the original mono and wall bounds.
    let due_wire = match bounded(socket.next()).await {
        Some(Ok(Message::Text(wire))) => wire,
        _ => panic!("missing original-reset Due barrier"),
    };
    let due: Reply = require(serde_json::from_str(&due_wire));
    assert!(due.valid_shape() && due.request_id.is_none());
    assert!(matches!(due.event, Event::Due { episode } if episode == id.as_u64()));
    assert!(Instant::now() >= wake && Utc::now() >= reset_at);

    send_command(
        &mut socket,
        2,
        Command::Inspect {
            episode: id.as_u64(),
        },
    )
    .await;
    assert!(
        matches!(phase_reply(&mut socket, 2, id.as_u64()).await.event,
        Event::Ready { episode } if episode == id.as_u64())
    );
    send_command(
        &mut socket,
        3,
        Command::Consume {
            episode: id.as_u64(),
        },
    )
    .await;
    let granted = phase_reply(&mut socket, 3, id.as_u64()).await;
    assert!(granted.grants(3, id.as_u64()));

    // A real roundtrip after Granted proves the control task did not self-close
    // or spin on the consumed record's immediately-ready notification watcher.
    ping(&mut socket).await;
    assert!(scope.live() && fixture.scope().same_owner(&scope));
    send_command(
        &mut socket,
        4,
        Command::Consume {
            episode: id.as_u64(),
        },
    )
    .await;
    assert!(matches!(
        phase_reply(&mut socket, 4, id.as_u64()).await.event,
        Event::Refused {
            reason: Refusal::Unavailable
        }
    ));
    assert!(scope.live());
    close_owner(&mut socket, &scope).await;
    fixture.finish().await;
}

#[test]
fn reset_wake_after_wall_clock_rollback_is_bounded_and_never_busy_due() {
    let mono = Instant::now();
    let wall = require(DateTime::from_timestamp(1_800_000_000, 0).ok_or(()));
    let wake = mono + Duration::from_secs(10);
    let reset = wall + chrono::Duration::seconds(10);
    assert!(next_reset_wake(wake, reset, mono, wall) == Some(wake));
    let reached = wake + Duration::from_secs(1);
    let rollback = wall - chrono::Duration::hours(1);
    assert!(
        next_reset_wake(wake, reset, reached, rollback) == Some(reached + Duration::from_secs(1))
    );
    assert!(
        next_reset_wake(
            wake,
            reset,
            reached,
            reset - chrono::Duration::milliseconds(250)
        ) == Some(reached + Duration::from_millis(250))
    );
    assert!(next_reset_wake(wake, reset, reached, reset).is_none());
    assert!(next_reset_wake(wake, reset, reached, reset + chrono::Duration::seconds(1)).is_none());
}
