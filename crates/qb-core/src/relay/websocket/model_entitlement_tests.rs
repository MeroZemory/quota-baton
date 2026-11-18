//! R30 through the real authenticated upgrade/pump and the existing WS fake.
//! No real credentials, usage polls, native owner or reset-credit calls.
use super::*;
use crate::{
    accounts::{Account, AccountStore, Identity, Tokens},
    bindings::{Candidate, Decision, LaunchRequest},
    catalog::{RefreshFailure, RefreshMode},
    config::Config,
    http::{RunningServers, spawn_with_routes},
    launch_tokens::{LaunchMetadata, LaunchToken},
    local_secrets::LocalSecrets,
    observability::Observability,
};
use futures_util::{SinkExt, StreamExt};
use qb_testkit::websocket::{Message, WebSocketUpstream};
use serde_json::json;
use std::{collections::BTreeMap, future::Future};
use tokio_tungstenite::{MaybeTlsStream, connect_async, tungstenite::client::IntoClientRequest};

const THREAD: &str = "a1111111-2222-4333-8444-555555555555";
const ROOT: &str = "codex:a1111111-2222-4333-8444-555555555555";
const ANCHOR: &str = "launch:synthetic-r30-ws";
const OLD: &str = "r30-old";
const MODEL: &str = "r30-only-b";
// Preserve whitespace and extensions through the client's own retry.
const CREATE: &str =
    " {\"type\":\"response.create\",\"model\":\"r30-only-b\",\"input\":[],\"extension\":17} ";
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[track_caller]
fn require<T, E>(value: Result<T, E>) -> T {
    match value {
        Ok(value) => value,
        Err(_) => panic!("synthetic R30 WS operation failed"),
    }
}

async fn bounded<F: Future>(future: F) -> F::Output {
    bounded_stage("bounded operation", future).await
}

async fn bounded_stage<F: Future>(stage: &str, future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .unwrap_or_else(|_| panic!("synthetic R30 WS stage timed out: {stage}"))
}

struct Fixture {
    relay: Arc<RelayState>,
    upstream: WebSocketUpstream,
    servers: RunningServers,
    accounts: [Account; 2],
    token: Option<LaunchToken>,
    _home: tempfile::TempDir,
}

impl Fixture {
    async fn new(managed: bool, source_supports_model: bool, strict: bool) -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/tests/r30-ws-entitlements");
        require(std::fs::create_dir_all(&base));
        let home = require(qb_testkit::fixture::home_in(base));
        let upstream = require(bounded(WebSocketUpstream::start()).await);
        upstream.state.set_echo(false);
        upstream.state.keep_old_connections();
        let store = Arc::new(require(AccountStore::acquire(home.path())));
        let mut accounts = Vec::new();
        for suffix in ["a", "b"] {
            let identity = Identity::Codex {
                account_id: format!("synthetic-r30-ws-{suffix}"),
                user_id: None,
                email: format!("r30-ws-{suffix}@example.invalid"),
            };
            let account: Account = require(serde_json::from_value(json!({
                "schema":1,"id":identity.id(),"provider":"codex","label":suffix,
                "email":format!("r30-ws-{suffix}@example.invalid"),"identity":identity,
                "source":{"kind":"oauth"},"user_state":"active","health":{"kind":"ready"},
                "generation":0,
                "token_meta":{"expires_at":"2099-01-01T00:00:00Z","refresh_uncertain":false}
            })));
            accounts.push(require(store.save(
                &account,
                &Tokens {
                    access_token: format!("synthetic-r30-ws-access-{suffix}"),
                    refresh_token: format!("synthetic-r30-ws-refresh-{suffix}"),
                    id_token: None,
                },
                None,
            )));
        }
        let accounts: [Account; 2] = accounts.try_into().ok().unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        // Strict is a scoped session policy; global Strict is invalid.
        config.routing.mode = crate::routing::Mode::Recover;
        config.websocket.idle_close_secs = 60;
        config.upstream.codex_base = upstream.url.clone();
        config.upstream.anthropic_base = upstream.url.clone();
        config.upstream.codex_usage_base = upstream.url.clone();
        config.upstream.claude_usage_base = upstream.url.clone();
        #[cfg(debug_assertions)]
        {
            config.upstream.codex_token_endpoint = Some(format!("{}/token", upstream.url));
            config.upstream.claude_token_endpoint = Some(format!("{}/token", upstream.url));
        }
        let relay = Arc::new(
            RelayState::new(
                store,
                &config,
                Arc::new(require(Observability::new(home.path(), None))),
                Arc::new(DebugPins::default()),
            )
            .unwrap_or_else(|_| panic!("synthetic R30 WS relay initialization failed")),
        );
        let source_models = if source_supports_model {
            vec![OLD, MODEL]
        } else {
            vec![OLD]
        };
        for (account, models) in [
            (&accounts[0], source_models),
            (&accounts[1], vec![OLD, MODEL]),
        ] {
            require(
                relay
                    .catalogs
                    .begin_refresh(&account.id, RefreshMode::Force),
            )
            .unwrap()
            .finish_with_client_version(
                Ok(json!({"models":models.iter().map(|id| json!({"slug":id})).collect::<Vec<_>>()})),
                Some(relay.codex_catalog_version.effective().to_string()),
            )
            .unwrap();
        }
        let candidate = Candidate {
            account: accounts[0].clone(),
            quota: None,
            inflight: 0,
        };
        let token = if managed {
            let models = BTreeSet::from([OLD.to_owned()]);
            require(relay.bindings.place_launch(
                LaunchRequest {
                    anchor_key: ANCHOR,
                    resume_key: None,
                    model: Some(OLD),
                    model_is_explicit: true,
                    account_pref: Some(&accounts[0].id),
                    allow_move_for_model: false,
                },
                std::slice::from_ref(&candidate),
                &BTreeMap::from([(accounts[0].id.clone(), models.clone())]),
                chrono::Utc::now(),
            ));
            require(
                relay
                    .bindings
                    .resolve_launch(Some(ANCHOR), Some(ROOT), chrono::Utc::now()),
            );
            Some(require(relay.launch_tokens.issue(LaunchMetadata {
                binding_anchor_key: ANCHOR.into(),
                advertised_models: models,
            })))
        } else {
            match relay.bindings.choose(
                Provider::Codex,
                Some(ROOT),
                None,
                Some(OLD),
                &[candidate],
                None,
                0,
            ) {
                Decision::Selected { lease, .. } => {
                    lease.dispatched();
                }
                _ => panic!("synthetic initial placement refused"),
            }
            None
        };
        if strict {
            // Adoption retains the managed launch's original policy identity.
            let policy_key = if managed { ANCHOR } else { ROOT };
            require(relay.bindings.policies.set_session(
                &crate::routing::session_id(policy_key),
                Some(crate::routing::Settings {
                    mode: crate::routing::Mode::Strict,
                    reset_wait_secs: config.routing.reset_hold_secs,
                    on_5h_limit: crate::routing::On5hLimit::Wait,
                }),
                None,
                Some(&accounts[0].id),
            ));
            let plan = relay.bindings.routing_plan(Some(ROOT));
            assert_eq!(plan.settings.mode, crate::routing::Mode::Strict);
            assert_eq!(plan.anchor.as_deref(), Some(accounts[0].id.as_str()));
        }
        let lease = relay.bindings.begin_request(Some(ROOT), &accounts[0].id);
        lease.dispatched();
        drop(lease);
        let servers = require(
            spawn_with_routes(
                &config,
                LocalSecrets {
                    client_key: "synthetic-r30-ws-client".into(),
                    admin_token: "synthetic-r30-ws-admin".into(),
                },
                super::super::router(relay.clone()),
                axum::Router::new(),
            )
            .await,
        );
        Self {
            relay,
            upstream,
            servers,
            accounts,
            token,
            _home: home,
        }
    }

    async fn connect(&self) -> Socket {
        let path = self.token.as_ref().map_or_else(
            || "/v1/responses".to_owned(),
            |token| format!("/l/{}/v1/responses", token.expose_secret()),
        );
        let mut request =
            require(format!("ws://{}{path}", self.servers.data_addr).into_client_request());
        request.headers_mut().insert(
            "authorization",
            "Bearer synthetic-r30-ws-client".parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("session-id", THREAD.parse().unwrap());
        let (socket, response) = match bounded_stage("client upgrade", connect_async(request)).await
        {
            Ok(upgraded) => upgraded,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                panic!(
                    "synthetic R30 WS client upgrade refused: status={}",
                    response.status()
                );
            }
            Err(_) => panic!("synthetic R30 WS client upgrade transport failed"),
        };
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        socket
    }

    fn current_upstream_identity(&self) -> String {
        self.upstream
            .state
            .requests()
            .iter()
            .rev()
            .find(|request| request.path_and_query == "/responses")
            .expect("actual provider upgrade")
            .headers["chatgpt-account-id"]
            .to_str()
            .unwrap()
            .to_owned()
    }

    async fn finish(self) {
        self.servers.shutdown();
        bounded(self.servers.wait_for_listener_close()).await;
        self.servers.data_task.abort();
        self.servers.admin_task.abort();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.servers.shutdown();
        self.servers.data_task.abort();
        self.servers.admin_task.abort();
    }
}

async fn next(socket: &mut Socket) -> Message {
    loop {
        match require(
            bounded_stage("client frame read", socket.next())
                .await
                .expect("synthetic client frame"),
        ) {
            Message::Ping(bytes) => {
                require(socket.send(Message::Pong(bytes)).await);
            }
            Message::Pong(_) => {}
            message => return message,
        }
    }
}

async fn unsent_model_reconnect(managed: bool, binary: bool) {
    let f = Fixture::new(managed, false, false).await;
    let mut original = f.connect().await;
    assert_eq!(f.current_upstream_identity(), "synthetic-r30-ws-a");
    let create = if binary {
        Message::Binary(CREATE.as_bytes().to_vec().into())
    } else {
        Message::text(CREATE)
    };
    require(bounded(original.send(create.clone())).await);
    assert!(
        matches!(next(&mut original).await, Message::Close(Some(frame)) if frame.code == 1012.into())
    );
    // The close is the causal barrier: no create was ever sent to old A.
    assert!(
        f.upstream
            .state
            .messages()
            .iter()
            .all(|message| !matches!(message, Message::Text(_) | Message::Binary(_)))
    );
    assert_eq!(
        f.relay.bindings.current(Some(ROOT)),
        Some(f.accounts[0].id.clone()),
        "preview must not publish through a live socket pin"
    );
    let _ = original.close(None).await;
    drop(original);
    bounded(async {
        while f.relay.inflight_requests.load(Ordering::Acquire) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let mut retried = f.connect().await;
    assert_eq!(f.current_upstream_identity(), "synthetic-r30-ws-b");
    assert_eq!(
        f.relay.bindings.current(Some(ROOT)),
        Some(f.accounts[1].id.clone())
    );
    assert!(
        f.upstream
            .state
            .messages()
            .iter()
            .all(|message| !matches!(message, Message::Text(_) | Message::Binary(_))),
        "server must not replay the held frame on reconnect"
    );
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"r30-retry","status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}"#;
    f.upstream.state.push_reply([Message::text(DONE)]);
    require(bounded(retried.send(create.clone())).await);
    assert_eq!(next(&mut retried).await, Message::text(DONE));
    let frames: Vec<_> = f
        .upstream
        .state
        .messages()
        .into_iter()
        .filter(|message| matches!(message, Message::Text(_) | Message::Binary(_)))
        .collect();
    assert_eq!(
        frames,
        vec![create],
        "only the client's byte-preserved retry may reach B"
    );
    let _ = retried.close(None).await;
    f.finish().await;
}

#[tokio::test]
async fn managed_unsent_model_change_reconnects_before_any_create_reaches_old_account() {
    unsent_model_reconnect(true, false).await;
}

#[tokio::test]
async fn ordinary_binary_known_model_change_reconnects_and_client_retry_reaches_entitled_account() {
    unsent_model_reconnect(false, true).await;
}

#[tokio::test]
async fn strict_managed_model_change_never_forwards_or_reconnects_around_consent() {
    let f = Fixture::new(true, false, true).await;
    let mut socket = f.connect().await;
    require(bounded(socket.send(Message::text(CREATE))).await);
    let reply = next(&mut socket).await;
    assert!(matches!(reply, Message::Text(ref text) if text.contains("model_not_supported")));
    assert_eq!(
        f.relay.bindings.current(Some(ROOT)),
        Some(f.accounts[0].id.clone())
    );
    assert!(
        f.upstream
            .state
            .messages()
            .iter()
            .all(|message| !matches!(message, Message::Text(_) | Message::Binary(_)))
    );
    let _ = socket.close(None).await;
    f.finish().await;
}

#[tokio::test]
async fn model_change_does_not_reconnect_or_interrupt_an_older_committed_turn() {
    let f = Fixture::new(true, false, false).await;
    let mut socket = f.connect().await;
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r30-older"}}"#;
    f.upstream.state.push_reply([Message::text(CREATED)]);
    let old_create =
        Message::text(json!({"type":"response.create","model":OLD,"input":[]}).to_string());
    require(bounded(socket.send(old_create.clone())).await);
    assert_eq!(next(&mut socket).await, Message::text(CREATED));
    require(bounded(socket.send(Message::text(CREATE))).await);
    assert!(
        matches!(next(&mut socket).await, Message::Text(ref text) if text.contains("model_not_supported"))
    );
    assert_eq!(
        f.relay.bindings.current(Some(ROOT)),
        Some(f.accounts[0].id.clone())
    );
    const DONE: &str =
        r#"{"type":"response.completed","response":{"id":"r30-older","status":"completed"}}"#;
    require(f.upstream.state.send(Message::text(DONE)));
    assert_eq!(next(&mut socket).await, Message::text(DONE));
    let creates: Vec<_> = f
        .upstream
        .state
        .messages()
        .into_iter()
        .filter(|message| matches!(message, Message::Text(_) | Message::Binary(_)))
        .collect();
    assert_eq!(creates, vec![old_create]);
    let _ = socket.close(None).await;
    f.finish().await;
}

#[tokio::test]
async fn actual_ws_model_denial_survives_failed_refresh_and_clears_only_on_success() {
    let f = Fixture::new(true, true, false).await;
    let mut socket = f.connect().await;
    f.upstream.state.push_reply([Message::text(r#"{"type":"error","status":400,"error":{"code":"model_not_available","message":"model is not available for this account"}}"#)]);
    require(bounded(socket.send(Message::text(CREATE))).await);
    assert!(
        matches!(next(&mut socket).await, Message::Close(Some(frame)) if frame.code == 1012.into())
    );
    let source = &f.accounts[0].id;
    assert!(require(f.relay.catalogs.is_model_denied(source, MODEL)));
    require(f.relay.catalogs.begin_refresh(source, RefreshMode::Force))
        .unwrap()
        .finish(Err(RefreshFailure::Transport))
        .unwrap();
    assert!(require(f.relay.catalogs.is_model_denied(source, MODEL)));
    assert!(
        require(
            f.relay
                .catalogs
                .entitled_accounts(std::slice::from_ref(source), MODEL, false)
        )
        .is_empty()
    );
    require(f.relay.catalogs.begin_refresh(source, RefreshMode::Force))
        .unwrap()
        .finish(Ok(json!({"models":[{"slug":OLD},{"slug":MODEL}]})))
        .unwrap();
    assert!(!require(f.relay.catalogs.is_model_denied(source, MODEL)));
    assert!(
        require(
            f.relay
                .catalogs
                .entitled_accounts(std::slice::from_ref(source), MODEL, false)
        )
        .contains(source)
    );
    let _ = socket.close(None).await;
    f.finish().await;
}
