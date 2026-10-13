use axum::http::{HeaderMap, StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use futures_util::{SinkExt, StreamExt};
use qb_core::{
    account_runtime::HealthSignal,
    accounts::{AccountStore, Health, HealthReason, Provider, UserState, import},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use qb_testkit::{
    ScriptedResponse, ScriptedUpstream,
    websocket::{Message, WebSocketUpstream},
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Error, client::IntoClientRequest},
};

type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
struct Harness {
    _root: tempfile::TempDir,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    store: Arc<AccountStore>,
    state: Arc<RelayState>,
    fake: WebSocketUpstream,
    token_fake: ScriptedUpstream,
    servers: RunningServers,
    ids: Vec<String>,
    obs: Arc<Observability>,
    home: PathBuf,
    _retry_proxy: Option<RetryProxy>,
}
struct RetryProxy(tokio::task::JoinHandle<()>);
impl Drop for RetryProxy {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Harness {
    async fn new(count: usize) -> Self {
        Self::new_with_owned(count, false).await
    }
    async fn new_with_owned(count: usize, owned: bool) -> Self {
        Self::new_with_network_retry(count, owned, false).await
    }
    async fn new_with_network_retry(count: usize, owned: bool, fail_first: bool) -> Self {
        let serial = SERIAL.lock().await;
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m4a-ws-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let cpa = root.path().join("cpa");
        fs::create_dir_all(&cpa).unwrap();
        let mut selectors = Vec::new();
        for n in 0..count {
            let email = format!("m4a-ws-{n}@example.invalid");
            fs::write(cpa.join(format!("{n}.json")), serde_json::to_vec(&json!({
                "type":"codex", "email":email, "account_id":format!("synthetic-ws-{n}"),
                "access_token":format!("synthetic-access-{n}"), "refresh_token":"synthetic-refresh",
                "expired":"2099-01-01T00:00:00Z", "last_refresh":"2026-10-13T00:00:01Z"
            })).unwrap()).unwrap();
            selectors.push(format!("codex:{email}"));
        }
        let store = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match AccountStore::acquire(&home) {
                    Ok(store) => break Arc::new(store),
                    Err(qb_core::accounts::StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => panic!("fixture store: {error}"),
                }
            }
        })
        .await
        .unwrap();
        import::run(&store, &cpa, &selectors, false).unwrap();
        let mut ids: Vec<_> = store.list().unwrap().into_iter().map(|a| a.id).collect();
        ids.sort();
        if owned {
            for id in &ids {
                let mut account = store.load(id).unwrap();
                let tokens = store.tokens(&account).unwrap();
                account.source = qb_core::accounts::Source::Oauth;
                store
                    .save(&account, &tokens, Some(account.generation))
                    .unwrap();
            }
        }
        let token_fake = ScriptedUpstream::start().await.unwrap();
        let fake = WebSocketUpstream::start().await.unwrap();
        fake.state.set_echo(false);
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.websocket.idle_close_secs = 60;
        let mut retry_proxy = None;
        config.upstream.codex_base = if fail_first {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let upstream = fake.url.strip_prefix("http://").unwrap().to_owned();
            retry_proxy = Some(RetryProxy(tokio::spawn(async move {
                let (first, _) = listener.accept().await.unwrap();
                drop(first); // A synthetic transport failure before any response bytes.
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let Ok((mut inbound, _)) = accepted else { break };
                            let upstream = upstream.clone();
                            connections.spawn(async move {
                                let mut outbound = tokio::net::TcpStream::connect(upstream).await.unwrap();
                                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                            });
                        }
                        _ = connections.join_next(), if !connections.is_empty() => {}
                    }
                }
            })));
            format!("http://{address}")
        } else {
            fake.url.clone()
        };
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let pins = Arc::new(DebugPins::default());
        let state = Arc::new(
            RelayState::new(store.clone(), &config, obs.clone(), pins)
                .unwrap()
                .with_token_endpoint(Provider::Codex, token_fake.url.clone()),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-local-key".into(),
                admin_token: "synthetic-admin-key".into(),
            },
            relay::router(state.clone()),
            axum::Router::new(),
        )
        .await
        .unwrap();
        Self {
            _root: root,
            _serial: serial,
            store,
            state,
            fake,
            token_fake,
            servers,
            ids,
            obs,
            home,
            _retry_proxy: retry_proxy,
        }
    }
    fn request(&self) -> axum::http::Request<()> {
        let mut request = format!("ws://{}/v1/responses", self.servers.data_addr)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            "Bearer synthetic-local-key".parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("session-id", "synthetic-ws-session".parse().unwrap());
        request
    }
    async fn connect(&self) -> (Client, String) {
        let (client, response) = connect_async(self.request()).await.unwrap();
        (
            client,
            response.headers()["x-quota-baton-account"]
                .to_str()
                .unwrap()
                .into(),
        )
    }
    async fn error(&self) -> (StatusCode, Value) {
        match connect_async(self.request()).await.unwrap_err() {
            Error::Http(response) => (
                response.status(),
                serde_json::from_slice(response.body().as_ref().unwrap()).unwrap(),
            ),
            error => panic!("unexpected handshake failure: {error}"),
        }
    }
    fn ledger(&self) -> Vec<Value> {
        self.obs.flush().unwrap();
        fs::read_dir(self.home.join("ledger"))
            .unwrap()
            .flat_map(|entry| {
                fs::read_to_string(entry.unwrap().path())
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}
async fn receive(client: &mut Client) -> Message {
    tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
async fn create(client: &mut Client, model: &str) {
    client
        .send(Message::Text(
            json!({"type":"response.create","model":model,"input":[]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
}
fn assert_close(message: Message, expected: u16) {
    match message {
        Message::Close(Some(frame)) => assert_eq!(u16::from(frame.code), expected),
        message => panic!("expected close {expected}, got {message:?}"),
    }
}
fn quota_error() -> String {
    json!({"type":"error","status":429,"error":{"type":"usage_limit_reached","resets_in_seconds":300,"message":"synthetic limit"}}).to_string()
}

#[tokio::test]
async fn c6_row_13_upgrade_quota_health_before_second_account() {
    let h = Harness::new(2).await;
    h.fake.state.push_handshake(
        StatusCode::TOO_MANY_REQUESTS,
        quota_error(),
        HeaderMap::new(),
    );
    let (mut client, account) = h.connect().await;
    assert_eq!(account, h.ids[1]);
    assert!(matches!(
        h.store.load(&h.ids[0]).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    assert_eq!(h.fake.state.requests().len(), 2);
    client.close(None).await.unwrap();
    let ledger = h.ledger();
    let upgrade = ledger
        .iter()
        .find(|r| r["outcome"] == "ws_upgrade")
        .unwrap();
    assert_eq!(upgrade["attempts"], 2);
    assert_eq!(upgrade["attempt_details"][0]["failover_reason"], "quota");
}

#[tokio::test]
async fn c7_upgrade_near_reset_retries_bound_account_once_without_moving() {
    let h = Harness::new(2).await;
    let (mut first, original) = h.connect().await;
    first.close(None).await.unwrap();
    h.fake.state.push_handshake(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":2}}"#,
        HeaderMap::new(),
    );
    let start = std::time::Instant::now();
    let (mut retry, account) = h.connect().await;
    assert_eq!(account, original);
    assert!(start.elapsed() >= Duration::from_millis(1800));
    assert!(start.elapsed() < Duration::from_secs(5));
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].headers["authorization"],
        requests[2].headers["authorization"]
    );
    let upgrade = h
        .ledger()
        .into_iter()
        .find(|row| row["outcome"] == "ws_upgrade" && row["attempts"] == 2)
        .unwrap();
    assert_eq!(
        upgrade["attempt_details"][0]["failover_reason"],
        "reset_hold_retry"
    );
    assert_eq!(h.store.load(&original).unwrap().health, Health::Ready);
    retry.close(None).await.unwrap();
}

#[tokio::test]
async fn rn2_upgrade_reset_sleep_releases_pending_auth_probe() {
    let h = Harness::new(2).await;
    let (mut first, original) = h.connect().await;
    first.close(None).await.unwrap();
    h.store
        .update_metadata(&original, |a| a.auth_probe_pending = true)
        .unwrap();
    h.fake.state.push_handshake(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":2}}"#,
        HeaderMap::new(),
    );
    let monitor = async {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if h.fake.state.requests().len() >= 2 && !h.state.accounts.probing(&original) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            h.store.load(&original).unwrap().health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
    };
    let ((mut retry, next), ()) = tokio::join!(h.connect(), monitor);
    assert_eq!(next, original);
    assert!(!h.store.load(&original).unwrap().auth_probe_pending);
    retry.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_3_ws_upgrade_hold_capped_below_15s() {
    let h = Harness::new(1).await;
    for _ in 0..5 {
        h.fake.state.push_handshake(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"code":"rate_limit_exceeded"}}"#,
            HeaderMap::new(),
        );
    }
    let result = tokio::time::timeout(Duration::from_secs(15), h.error()).await;
    assert!(
        result.is_ok(),
        "upgrade rate hold exceeded Codex connect bound"
    );
    assert_eq!(result.unwrap().0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn c6_3_ws_precommit_rate_frame_resent_on_open_socket() {
    let h = Harness::new(1).await;
    let rate = r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#;
    h.fake.state.push_reply([Message::Text(rate.into())]);
    h.fake
        .state
        .push_reply([Message::Text(r#"{"type":"response.completed"}"#.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_eq!(
        receive(&mut client).await,
        Message::Text(r#"{"type":"response.completed"}"#.into())
    );
    let text_messages = h
        .fake
        .state
        .messages()
        .into_iter()
        .filter(|message| matches!(message, Message::Text(_)))
        .count();
    assert_eq!(text_messages, 2);
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_3_ws_precommit_rate_redial_same_account_without_previous_response_id() {
    let h = Harness::new(1).await;
    let rate = r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#;
    h.fake.state.push_reply([
        Message::Text(rate.into()),
        Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: 1012.into(),
            reason: "synthetic close".into(),
        })),
    ]);
    h.fake
        .state
        .push_reply([Message::Text(r#"{"type":"response.completed"}"#.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_eq!(
        receive(&mut client).await,
        Message::Text(r#"{"type":"response.completed"}"#.into())
    );
    let events = h.fake.state.events();
    assert!(
        events
            .iter()
            .filter(|event| matches!(event, qb_testkit::websocket::ConnectionEvent::Opened(_)))
            .count()
            >= 2
    );
    let messages = h.fake.state.messages();
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, Message::Text(_)))
            .count(),
        2
    );
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_3_ws_rate_frame_with_advice_relayed_immediately() {
    let h = Harness::new(1).await;
    let rate = r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"},"headers":{"retry-after":"5"}}"#;
    h.fake.state.push_reply([Message::Text(rate.into())]);
    let (mut client, _) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_eq!(receive(&mut client).await, Message::Text(rate.into()));
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_3_ws_rate_frame_without_advice_closes_1012_same_account() {
    let h = Harness::new(2).await;
    let rate = r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#;
    h.fake.state.push_reply(
        (0..4)
            .map(|_| Message::Text(rate.into()))
            .collect::<Vec<_>>(),
    );
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    let (mut reconnect, same_account) = h.connect().await;
    assert_eq!(same_account, account);
    assert_eq!(same_account, h.ids[0]);
    let requests = h.fake.state.requests();
    let first_authorization = requests[0].headers["authorization"].clone();
    assert!(
        requests
            .iter()
            .all(|request| { request.headers["authorization"] == first_authorization })
    );
    assert_eq!(requests.len(), 2, "only A should receive both upgrades");
    reconnect.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_3_ws_connection_limit_frame_not_held() {
    let h = Harness::new(1).await;
    for code in [
        "websocket_connection_limit_reached",
        "previous_response_not_found",
    ] {
        let error = json!({"type":"error","status":429,"error":{"code":code}}).to_string();
        h.fake
            .state
            .push_reply([Message::Text(error.clone().into())]);
        let (mut client, _) = h.connect().await;
        create(&mut client, "fixture-model").await;
        assert_eq!(receive(&mut client).await, Message::Text(error.into()));
        client.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn c7_upgrade_move_then_reset_retry_preserves_first_turn_marker() {
    let h = Harness::new(2).await;
    let (mut first, original) = h.connect().await;
    first.close(None).await.unwrap();
    h.state
        .accounts
        .change(&original, Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    h.fake.state.push_handshake(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":2}}"#,
        HeaderMap::new(),
    );
    let (mut client, next) = h.connect().await;
    assert_ne!(next, original);
    for _ in 0..2 {
        h.fake
            .state
            .push_reply([Message::Text(r#"{"type":"response.completed"}"#.into())]);
        create(&mut client, "fixture-model").await;
        assert!(matches!(receive(&mut client).await, Message::Text(_)));
    }
    client.close(None).await.unwrap();
    let turns: Vec<_> = h
        .ledger()
        .into_iter()
        .filter(|r| r["outcome"] == "ok")
        .collect();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["after_move"], true);
    assert_ne!(turns[1]["after_move"], true);
}

#[tokio::test]
async fn c7_upgrade_second_near_reset_limit_fails_over_without_another_hold() {
    let h = Harness::new(2).await;
    let (mut first, original) = h.connect().await;
    first.close(None).await.unwrap();
    for _ in 0..2 {
        h.fake.state.push_handshake(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":2}}"#,
            HeaderMap::new(),
        );
    }
    let start = std::time::Instant::now();
    let (mut retry, account) = h.connect().await;
    assert_ne!(account, original);
    assert!(start.elapsed() >= Duration::from_millis(1800));
    assert!(start.elapsed() < Duration::from_millis(3800));
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests[1].headers["authorization"],
        requests[2].headers["authorization"]
    );
    assert_ne!(
        requests[2].headers["authorization"],
        requests[3].headers["authorization"]
    );
    retry.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13a_metadata_does_not_commit_health_then_1012_reconnect() {
    let h = Harness::new(2).await;
    h.fake.state.push_reply([
        Message::Text(r#"{"type":"metadata","request_id":"synthetic"}"#.into()),
        Message::Text(r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":20,"reset_after_seconds":300}}}"#.into()),
        Message::Text(quota_error().into()),
    ]);
    let (mut client, original) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert!(matches!(receive(&mut client).await, Message::Text(_)));
    assert!(matches!(receive(&mut client).await, Message::Text(_)));
    assert_close(receive(&mut client).await, 1012);
    assert!(matches!(
        h.store.load(&original).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    let (mut reconnected, next) = h.connect().await;
    assert_ne!(next, original);
    reconnected.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13b_committed_error_is_verbatim_and_applies_health() {
    let h = Harness::new(2).await;
    let error = quota_error();
    h.fake.state.push_reply([
        Message::Text(r#"{"type":"response.created"}"#.into()),
        Message::Text(error.clone().into()),
    ]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert!(matches!(receive(&mut client).await, Message::Text(_)));
    assert_eq!(receive(&mut client).await, Message::Text(error.into()));
    assert!(matches!(
        h.store.load(&account).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    assert_close(receive(&mut client).await, 1000);
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn c6_row_13c_idle_pause_closes_1000() {
    let h = Harness::new(2).await;
    let (mut client, account) = h.connect().await;
    h.state
        .accounts
        .change(&account, Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    assert_close(receive(&mut client).await, 1000);
    let (mut reconnect, next) = h.connect().await;
    assert_ne!(next, account);
    reconnect.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13c_model_checked_before_create_without_forwarding() {
    let h = Harness::new(2).await;
    h.fake
        .state
        .push_reply([Message::Text(r#"{"type":"response.completed"}"#.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "allowed-model").await;
    let _ = receive(&mut client).await;
    h.state
        .accounts
        .transition(
            &account,
            HealthSignal::Cooling {
                reason: HealthReason::Permission,
                until: Utc::now() + ChronoDuration::hours(1),
                scope: "model:blocked-model".into(),
            },
        )
        .await
        .unwrap();
    create(&mut client, "blocked-model").await;
    assert_close(receive(&mut client).await, 1000);
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn c6_row_14_cached_429_upgrade_native_terminal_response() {
    let h = Harness::new(1).await;
    let body = quota_error();
    h.fake.state.push_handshake(
        StatusCode::TOO_MANY_REQUESTS,
        body.clone(),
        HeaderMap::new(),
    );
    let (status, error) = h.error().await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error["error"]["type"], "usage_limit_reached");
    assert!(error["error"]["resets_in_seconds"].as_i64().unwrap() > 0);
    let replay = h
        .state
        .accounts
        .unavailable(Provider::Codex, false, None)
        .await;
    assert_eq!(replay.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(replay.headers().contains_key("retry-after"));
    assert_eq!(
        axum::body::to_bytes(replay.into_body(), 65536)
            .await
            .unwrap(),
        body
    );
    let _ = h.error().await;
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn c9_passive_rate_limits_credits_hold_idle_close() {
    let h = Harness::new(2).await;
    let (mut client, account) = h.connect().await;
    let event = json!({"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":99,"window_minutes":300,"reset_after_seconds":300},"credits":{"has_credits":true,"unlimited":false}}}).to_string();
    h.fake
        .state
        .send(Message::Text(event.clone().into()))
        .unwrap();
    assert_eq!(receive(&mut client).await, Message::Text(event.into()));
    assert_close(receive(&mut client).await, 1000);
    assert!(matches!(
        h.store.load(&account).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
}

#[tokio::test]
async fn c9_passive_upgrade_headers_exhaustion_closes_idle() {
    let h = Harness::new(2).await;
    let mut headers = HeaderMap::new();
    headers.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    headers.insert(
        "x-codex-primary-reset-after-seconds",
        "300".parse().unwrap(),
    );
    h.fake.state.set_success_headers(headers);
    let (mut client, account) = h.connect().await;
    assert_close(receive(&mut client).await, 1000);
    assert!(matches!(
        h.store.load(&account).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
}

#[tokio::test]
async fn c6_row_13_second_unknown_model_403_stops() {
    let h = Harness::new(3).await;
    let body = r#"{"error":{"type":"permission_error","message":"synthetic forbidden"}}"#;
    for _ in 0..2 {
        h.fake
            .state
            .push_handshake(StatusCode::FORBIDDEN, body, HeaderMap::new());
    }
    let (status, _) = h.error().await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(h.fake.state.requests().len(), 2);
    for id in &h.ids {
        assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
    }
}

#[tokio::test]
async fn c6_row_13_four_distinct_accounts_maximum() {
    let h = Harness::new(5).await;
    for _ in 0..4 {
        h.fake.state.push_handshake(
            StatusCode::TOO_MANY_REQUESTS,
            quota_error(),
            HeaderMap::new(),
        );
    }
    let _ = h.error().await;
    assert_eq!(h.fake.state.requests().len(), 4);
    assert_eq!(h.store.load(&h.ids[4]).unwrap().health, Health::Ready);
}

#[tokio::test]
async fn c6_row_13_402_and_6a_exhaustion_keep_last_response() {
    for (status, body) in [
        (
            StatusCode::PAYMENT_REQUIRED,
            r#"{"error":{"message":"synthetic payment required"}}"#,
        ),
        (
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"The model 'fixture-model' is not supported when using Codex with a ChatGPT account"}}"#,
        ),
    ] {
        let h = Harness::new(2).await;
        for _ in 0..2 {
            h.fake.state.push_handshake(status, body, HeaderMap::new());
        }
        let (actual, error) = h.error().await;
        assert_eq!(actual, status);
        assert_eq!(error, serde_json::from_str::<Value>(body).unwrap());
        assert_eq!(h.fake.state.requests().len(), 2);
    }
}

#[tokio::test]
async fn c6_row_13a_model_permission_records_scope_before_close() {
    let h = Harness::new(2).await;
    h.fake.state.push_reply([Message::Text(r#"{"type":"error","status":403,"error":{"type":"permission_error","message":"synthetic model denial"}}"#.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "blocked-model").await;
    assert_close(receive(&mut client).await, 1012);
    assert!(
        matches!(h.store.load(&account).unwrap().health, Health::Cooling {reason:HealthReason::Permission, scope, ..} if scope == "model:blocked-model")
    );
}

#[tokio::test]
async fn c6_row_13a_linked_revocation_closes_and_cools_link_stale_without_replay() {
    let h = Harness::new(2).await;
    h.fake.state.push_reply([Message::Text(r#"{"type":"error","status":401,"error":{"code":"token_revoked","message":"synthetic revoked token"}}"#.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    assert!(matches!(
        h.store.load(&account).unwrap().health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
    let (mut reconnect, next) = h.connect().await;
    assert_ne!(account, next);
    assert_eq!(h.fake.state.requests().len(), 2);
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count(),
        1
    );
    assert!(h.token_fake.state.requests().is_empty());
    reconnect.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13a_owned_revocation_closes_and_marks_relogin() {
    let h = Harness::new_with_owned(2, true).await;
    h.fake.state.push_reply([Message::Text(
        r#"{"type":"error","status":401,"error":{"code":"token_revoked"}}"#.into(),
    )]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    assert!(matches!(
        h.store.load(&account).unwrap().health,
        Health::NeedsRelogin { .. }
    ));
    assert!(h.token_fake.state.requests().is_empty());
    let (mut reconnect, next) = h.connect().await;
    assert_ne!(account, next);
    reconnect.close(None).await.unwrap();
}

#[tokio::test]
async fn m5_1_linked_revoked_handshake_reloads_cpa_or_cools_link_stale() {
    for newer in [false, true] {
        let h = Harness::new(2).await;
        let id = &h.ids[0];
        let account = h.store.load(id).unwrap();
        let old_token = h.store.tokens(&account).unwrap().access_token.clone();
        h.fake.state.push_handshake_delayed(
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"code":"token_revoked"}}"#,
            HeaderMap::new(),
            Duration::from_millis(500),
        );
        let request = h.request();
        let connecting = tokio::spawn(async move { connect_async(request).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while h.fake.state.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // Rotate after the initial handshake, so before_request cannot load it first.
        if newer {
            rotate_linked_cpa(&h, id, "synthetic-cpa-handshake-rotated");
        }
        let (mut client, response) = connecting.await.unwrap().unwrap();
        let next = response.headers()["x-quota-baton-account"]
            .to_str()
            .unwrap();
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers["authorization"],
            format!("Bearer {old_token}")
        );
        let updated = h.store.load(id).unwrap();
        if newer {
            assert_eq!(next, id.as_str());
            assert_eq!(
                requests[1].headers["authorization"],
                "Bearer synthetic-cpa-handshake-rotated"
            );
            assert_eq!(updated.health, Health::Ready);
            assert_eq!(
                h.store.tokens(&updated).unwrap().access_token,
                "synthetic-cpa-handshake-rotated"
            );
        } else {
            assert_eq!(next, h.ids[1]);
            assert!(matches!(
                updated.health,
                Health::Cooling {
                    reason: HealthReason::LinkStale,
                    ..
                }
            ));
        }
        assert_eq!(updated.auth_failures, 0);
        assert!(h.token_fake.state.requests().is_empty());
        assert!(h.fake.state.messages().is_empty());
        client.close(None).await.unwrap();
    }
}

fn rotate_linked_cpa(h: &Harness, id: &str, access_token: &str) {
    let account = h.store.load(id).unwrap();
    let qb_core::accounts::Source::Cpa { dir, .. } = account.source else {
        panic!("expected linked fixture")
    };
    let path = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            let fixture: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            fixture["email"] == account.email
        })
        .unwrap();
    let mut fixture: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fixture["access_token"] = access_token.into();
    fixture["last_refresh"] = "2026-10-13T00:00:02Z".into();
    fs::write(path, serde_json::to_vec(&fixture).unwrap()).unwrap();
}

#[tokio::test]
async fn m5_1_linked_revoked_open_frame_reloads_newer_cpa_without_replay() {
    let h = Harness::new(2).await;
    let (mut client, id) = h.connect().await;
    // The old token has already completed its handshake before CPA is rotated.
    rotate_linked_cpa(&h, &id, "synthetic-cpa-frame-rotated");
    h.fake.state.push_reply([Message::Text(
        r#"{"type":"error","status":401,"error":{"code":"token_revoked"}}"#.into(),
    )]);
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    let updated = h.store.load(&id).unwrap();
    assert_eq!(updated.health, Health::Ready);
    assert_eq!(updated.auth_failures, 0);
    assert_eq!(
        h.store.tokens(&updated).unwrap().access_token,
        "synthetic-cpa-frame-rotated"
    );
    assert_eq!(h.fake.state.requests().len(), 1);
    let (mut reconnect, next) = h.connect().await;
    assert_ne!(next, id);
    assert_eq!(h.fake.state.requests().len(), 2);
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count(),
        1
    );
    assert!(h.token_fake.state.requests().is_empty());
    reconnect.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13b_unrelated_precommit_error_is_verbatim() {
    let h = Harness::new(1).await;
    let raw = r#"{"type":"error","status":400,"error":{"code":"invalid_encrypted_content","message":"synthetic opaque rejection"}}"#;
    h.fake.state.push_reply([Message::Text(raw.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_eq!(receive(&mut client).await, Message::Text(raw.into()));
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13_large_raw_error_body_preserves_remaining_bytes() {
    let h = Harness::new(1).await;
    let body = json!({"error":{"message":"x".repeat(100_000)}}).to_string();
    h.fake
        .state
        .push_handshake(StatusCode::IM_A_TEAPOT, body.clone(), HeaderMap::new());
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{}/v1/responses", h.servers.data_addr))
        .headers(h.request().headers().clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    let raw = response.bytes().await.unwrap();
    assert_eq!(raw.len(), body.len());
    assert!(
        raw.as_ref() == body.as_bytes(),
        "raw handshake payload changed"
    );
}

#[tokio::test]
async fn c6_row_13_upstream_426_becomes_502() {
    let h = Harness::new(1).await;
    h.fake.state.push_handshake(
        StatusCode::UPGRADE_REQUIRED,
        "synthetic upgrade failure",
        HeaderMap::new(),
    );
    let (status, _) = h.error().await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn c6_row_13a_linked_401_staleness_before_close() {
    let h = Harness::new(2).await;
    h.fake.state.push_reply([Message::Text(r#"{"type":"error","status":401,"error":{"type":"authentication_error","message":"synthetic auth failure"}}"#.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    assert!(matches!(
        h.store.load(&account).unwrap().health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
}

#[tokio::test]
async fn c6_row_13a_linked_401_newer_token_reconnects_other_account_without_replay() {
    let h = Harness::new(2).await;
    let (mut client, id) = h.connect().await;
    let account = h.store.load(&id).unwrap();
    let qb_core::accounts::Source::Cpa { dir, .. } = account.source else {
        panic!("expected linked fixture")
    };
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let mut fixture: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if fixture["email"] == account.email {
            fixture["access_token"] = "synthetic-new-access".into();
            fixture["last_refresh"] = "2026-10-13T00:00:02Z".into();
            fs::write(path, serde_json::to_vec(&fixture).unwrap()).unwrap();
        }
    }
    h.fake.state.push_reply([Message::Text(
        r#"{"type":"error","status":401,"error":{"type":"authentication_error"}}"#.into(),
    )]);
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    assert_eq!(h.store.load(&id).unwrap().health, Health::Ready);
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count(),
        1
    );
    let (mut reconnect, next) = h.connect().await;
    assert_ne!(next, id);
    assert_eq!(h.fake.state.requests().len(), 2);
    reconnect.close(None).await.unwrap();
}

#[tokio::test]
async fn c9_passive_exhaustion_and_early_reset_during_response() {
    let h = Harness::new(1).await;
    h.fake.state.push_reply([
        Message::Text(json!({"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":100,"window_minutes":300,"reset_after_seconds":300}}}).to_string().into()),
        Message::Text(json!({"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":10,"window_minutes":300,"reset_after_seconds":1200}}}).to_string().into()),
        Message::Text(r#"{"type":"response.created"}"#.into()),
        Message::Text(r#"{"type":"response.completed"}"#.into()),
    ]);
    let (mut client, id) = h.connect().await;
    create(&mut client, "fixture-model").await;
    for _ in 0..4 {
        assert!(matches!(receive(&mut client).await, Message::Text(_)));
    }
    assert_eq!(h.store.load(&id).unwrap().health, Health::Ready);
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13_network_failure_retries_once_without_switching_accounts() {
    let mut h = Harness::new(2).await;
    let old = std::mem::replace(&mut h.fake, WebSocketUpstream::start().await.unwrap());
    drop(old);
    let (status, _) = h.error().await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let records = h.ledger();
    let upgrade = records
        .iter()
        .find(|record| record["outcome"] == "ws_upgrade_error")
        .unwrap();
    assert_eq!(upgrade["attempts"], 2);
    assert_eq!(
        upgrade["attempt_details"][0]["account"],
        upgrade["attempt_details"][1]["account"]
    );
    for id in &h.ids {
        assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
    }
}

#[tokio::test]
async fn c6_row_14_cache_only_complete_bodies_within_64kib() {
    for size in [65536, 65537] {
        let h = Harness::new(1).await;
        let mut body = json!({"error":{"type":"usage_limit_reached","padding":""}}).to_string();
        let padding = "x".repeat(size - body.len());
        body = json!({"error":{"type":"usage_limit_reached","padding":padding}}).to_string();
        assert_eq!(body.len(), size);
        h.fake.state.push_handshake(
            StatusCode::TOO_MANY_REQUESTS,
            body.clone(),
            HeaderMap::new(),
        );
        assert_eq!(h.error().await.0, StatusCode::TOO_MANY_REQUESTS);
        let replay = h
            .state
            .accounts
            .unavailable(Provider::Codex, false, None)
            .await;
        let replay = axum::body::to_bytes(replay.into_body(), 65536)
            .await
            .unwrap();
        if size == 65536 {
            assert_eq!(replay, body);
        } else {
            let error: Value = serde_json::from_slice(&replay).unwrap();
            assert_eq!(error["error"]["type"], "usage_limit_reached");
            assert!(error["error"]["padding"].is_null());
        }
    }
}

#[tokio::test]
async fn c5_expired_auth_probe_success_clears_failures_on_101() {
    let h = Harness::new(1).await;
    let id = &h.ids[0];
    h.state
        .accounts
        .transition(id, HealthSignal::AuthFailure)
        .await
        .unwrap();
    h.state
        .accounts
        .transition(id, HealthSignal::Ready)
        .await
        .unwrap();
    assert_eq!(h.store.load(id).unwrap().auth_failures, 1);
    let (mut client, account) = h.connect().await;
    assert_eq!(&account, id);
    assert_eq!(h.store.load(id).unwrap().auth_failures, 0);
    assert!(!h.state.accounts.probing(id));
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c5_auth_probe_is_exclusive_during_handshake() {
    let h = Harness::new(2).await;
    let id = &h.ids[0];
    h.state
        .accounts
        .transition(id, HealthSignal::AuthFailure)
        .await
        .unwrap();
    h.state
        .accounts
        .transition(id, HealthSignal::Ready)
        .await
        .unwrap();
    h.fake.state.push_handshake_delayed(
        StatusCode::BAD_REQUEST,
        r#"{"error":{"type":"invalid_request_error"}}"#,
        HeaderMap::new(),
        Duration::from_secs(1),
    );
    let request = h.request();
    let first = tokio::spawn(async move { connect_async(request).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(h.state.accounts.probing(id));
    let (mut client, account) = h.connect().await;
    assert_eq!(account, h.ids[1]);
    assert!(
        matches!(first.await.unwrap(), Err(Error::Http(response)) if response.status() == StatusCode::BAD_REQUEST)
    );
    assert!(!h.state.accounts.probing(id));
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13a_parsed_error_with_large_metadata_still_fails_over() {
    let h = Harness::new(2).await;
    let event = json!({"type":"error","status":429,"error":{"type":"usage_limit_reached","resets_in_seconds":300},"metadata":"x".repeat(100_000)}).to_string();
    h.fake.state.push_reply([Message::Text(event.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    assert!(matches!(
        h.store.load(&account).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
}

#[tokio::test]
async fn c6_row_13_owned_transient_refresh_cools_five_minutes_then_fails_over() {
    let h = Harness::new_with_owned(2, true).await;
    h.token_fake.state.push(ScriptedResponse::json(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"synthetic transient"}"#,
    ));
    let body = r#"{"error":{"type":"authentication_error","message":"synthetic auth failure"}}"#;
    h.fake
        .state
        .push_handshake(StatusCode::UNAUTHORIZED, body, HeaderMap::new());
    let started = Utc::now();
    let (mut client, next) = h.connect().await;
    assert_eq!(next, h.ids[1]);
    assert_eq!(h.fake.state.requests().len(), 2);
    assert_eq!(h.token_fake.state.requests().len(), 1);
    let account = h.store.load(&h.ids[0]).unwrap();
    assert!(
        matches!(account.health, Health::Cooling { reason: HealthReason::Auth, until, .. }
        if until >= started + ChronoDuration::minutes(5) && until <= Utc::now() + ChronoDuration::minutes(5))
    );
    assert_eq!(account.auth_failures, 0);
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13_owned_new_token_401_cools_auth_after_single_refresh() {
    let h = Harness::new_with_owned(2, true).await;
    h.token_fake.state.push(ScriptedResponse::json(StatusCode::OK, r#"{"access_token":"synthetic-refreshed","refresh_token":"synthetic-rotated-refresh","expires_in":3600}"#));
    for _ in 0..2 {
        h.fake.state.push_handshake(
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"type":"authentication_error"}}"#,
            HeaderMap::new(),
        );
    }
    let (mut client, next) = h.connect().await;
    assert_eq!(next, h.ids[1]);
    assert_eq!(h.fake.state.requests().len(), 3);
    assert_eq!(h.token_fake.state.requests().len(), 1);
    assert_eq!(
        h.fake.state.requests()[1].headers["authorization"],
        "Bearer synthetic-refreshed"
    );
    let account = h.store.load(&h.ids[0]).unwrap();
    assert!(matches!(
        account.health,
        Health::Cooling {
            reason: HealthReason::Auth,
            ..
        }
    ));
    assert_eq!(account.auth_failures, 1);
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13a_owned_transient_refresh_cools_without_auth_failure_then_reconnects_other() {
    let h = Harness::new_with_owned(2, true).await;
    h.token_fake.state.push(ScriptedResponse::json(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"synthetic transient"}"#,
    ));
    h.fake.state.push_reply([Message::Text(
        r#"{"type":"error","status":401,"error":{"type":"authentication_error"}}"#.into(),
    )]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    assert!(
        matches!(h.store.load(&account).unwrap().health, Health::Cooling { reason: HealthReason::Auth, until, .. }
        if (until - Utc::now()).num_seconds() >= 295 && (until - Utc::now()).num_seconds() <= 300)
    );
    assert_eq!(h.store.load(&account).unwrap().auth_failures, 0);
    assert_eq!(h.token_fake.state.requests().len(), 1);
    let (mut reconnect, next) = h.connect().await;
    assert_ne!(account, next);
    reconnect.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13a_unknown_model_403_multiple_reconnects_exclude_every_previous_account() {
    let h = Harness::new(4).await;
    let raw = r#"{"type":"error","status":403,"error":{"type":"permission_error"}}"#;
    // json! sorts input before type, which puts type outside the bounded
    // peek for a large message. Match Codex's type-first wire shape instead.
    let large_create = format!(
        r#"{{"type":"response.create","input":"{}"}}"#,
        "x".repeat(1024 * 1024)
    );
    assert!(large_create.len() > 1024 * 1024);
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..3 {
        h.fake.state.push_reply([Message::Text(raw.into())]);
        let (mut client, account) = h.connect().await;
        assert!(seen.insert(account.clone()), "reselected excluded account");
        client
            .send(Message::Text(large_create.clone().into()))
            .await
            .unwrap();
        assert_close(receive(&mut client).await, 1012);
        assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
    }
    let (mut client, account) = h.connect().await;
    assert!(seen.insert(account));
    // All alternatives are now session-excluded: relay, without another 1012.
    h.fake.state.push_reply([Message::Text(raw.into())]);
    create(&mut client, "fixture-model").await;
    assert_eq!(receive(&mut client).await, Message::Text(raw.into()));
    assert_close(receive(&mut client).await, 1000);
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|message| {
                matches!(message, Message::Text(text) if text.as_str() == large_create.as_str())
            })
            .count(),
        3,
        "large input must be forwarded unchanged"
    );
    let records = h.ledger();
    let failovers: Vec<_> = records
        .iter()
        .filter(|row| row["outcome"] == "failover")
        .collect();
    assert_eq!(failovers.len(), 3);
    assert!(failovers.iter().all(|row| row["model"].is_null()
        && row["attempt_details"][0]["failover_reason"] == "permission"));
}

#[tokio::test]
async fn c6_row_13a_unknown_model_403_without_session_key_relays_without_1012() {
    let h = Harness::new(2).await;
    let raw = r#"{"type":"error","status":403,"error":{"type":"permission_error"}}"#;
    h.fake.state.push_reply([Message::Text(raw.into())]);
    let mut request = h.request();
    request.headers_mut().remove("session-id");
    let (mut client, response) = connect_async(request).await.unwrap();
    client
        .send(Message::Text(
            r#"{"type":"response.create","input":[]}"#.into(),
        ))
        .await
        .unwrap();
    assert_eq!(receive(&mut client).await, Message::Text(raw.into()));
    assert_eq!(
        h.store
            .load(
                response.headers()["x-quota-baton-account"]
                    .to_str()
                    .unwrap()
            )
            .unwrap()
            .health,
        Health::Ready
    );
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13a_single_account_quota_frame_is_relayed_without_1012() {
    let h = Harness::new(1).await;
    let raw = quota_error();
    h.fake.state.push_reply([Message::Text(raw.clone().into())]);
    let (mut client, _) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_eq!(receive(&mut client).await, Message::Text(raw.into()));
    assert_close(receive(&mut client).await, 1000);
}

#[tokio::test]
async fn c6_row_13a_repeated_auth_frame_on_new_token_cools_without_second_refresh() {
    let h = Harness::new_with_owned(1, true).await;
    let raw = r#"{"type":"error","status":401,"error":{"type":"authentication_error"}}"#;
    h.token_fake.state.push(ScriptedResponse::json(StatusCode::OK,
        r#"{"access_token":"synthetic-frame-refreshed","refresh_token":"synthetic-frame-rotated","expires_in":3600}"#));
    h.fake.state.push_reply([Message::Text(raw.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_eq!(receive(&mut client).await, Message::Text(raw.into()));
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
    client.close(None).await.unwrap();
    let (mut reconnect, next) = h.connect().await;
    assert_eq!(next, account);
    assert_eq!(
        h.fake.state.requests().last().unwrap().headers["authorization"],
        "Bearer synthetic-frame-refreshed"
    );
    h.fake.state.push_reply([Message::Text(raw.into())]);
    create(&mut reconnect, "fixture-model").await;
    assert_eq!(receive(&mut reconnect).await, Message::Text(raw.into()));
    assert_close(receive(&mut reconnect).await, 1000);
    let updated = h.store.load(&account).unwrap();
    assert!(
        matches!(updated.health, Health::Cooling { reason: HealthReason::Auth, until, .. }
        if (until - Utc::now()).num_seconds() >= 895)
    );
    assert_eq!(updated.auth_failures, 1);
    assert_eq!(h.token_fake.state.requests().len(), 1);
}

#[tokio::test]
async fn c6_row_13_network_then_refresh_has_independent_retry_budgets() {
    let h = Harness::new_with_network_retry(2, true, true).await;
    h.token_fake.state.push(ScriptedResponse::json(StatusCode::OK,
        r#"{"access_token":"synthetic-network-refreshed","refresh_token":"synthetic-network-rotated","expires_in":3600}"#));
    h.fake.state.push_handshake(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"type":"authentication_error"}}"#,
        HeaderMap::new(),
    );
    let (mut client, account) = h.connect().await;
    assert_eq!(account, h.ids[0]);
    assert_eq!(h.token_fake.state.requests().len(), 1);
    assert_eq!(h.fake.state.requests().len(), 2);
    assert_eq!(
        h.fake.state.requests()[1].headers["authorization"],
        "Bearer synthetic-network-refreshed"
    );
    let ledger = h.ledger();
    let upgrade = ledger
        .iter()
        .find(|row| row["outcome"] == "ws_upgrade")
        .unwrap();
    assert_eq!(upgrade["attempts"], 3);
    assert_eq!(
        upgrade["attempt_details"][0]["failover_reason"],
        "network_retry"
    );
    assert_eq!(
        upgrade["attempt_details"][1]["failover_reason"],
        "token_retry"
    );
    assert!(
        upgrade["attempt_details"]
            .as_array()
            .unwrap()
            .iter()
            .all(|attempt| attempt["account"] == account)
    );
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13_stale_revocation_upgrade_retries_current_token_without_relogin() {
    let h = Harness::new_with_owned(2, true).await;
    h.fake.state.push_handshake_delayed(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"token_revoked","message":"synthetic revoked old token"}}"#,
        HeaderMap::new(),
        Duration::from_millis(500),
    );
    let request = h.request();
    let connecting = tokio::spawn(async move { connect_async(request).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let account = h.store.load(&h.ids[0]).unwrap();
    let mut tokens = h.store.tokens(&account).unwrap();
    tokens.access_token = "synthetic-concurrently-rotated".into();
    h.store
        .save(&account, &tokens, Some(account.generation))
        .unwrap();
    let (mut client, response) = connecting.await.unwrap().unwrap();
    assert_eq!(
        response.headers()["x-quota-baton-account"]
            .to_str()
            .unwrap(),
        h.ids[0]
    );
    assert_eq!(
        h.fake.state.requests()[1].headers["authorization"],
        "Bearer synthetic-concurrently-rotated"
    );
    assert_eq!(h.store.load(&h.ids[0]).unwrap().health, Health::Ready);
    assert!(h.token_fake.state.requests().is_empty());
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_rows_13a_13b_stale_revocation_frame_does_not_mark_current_token_relogin() {
    for committed in [false, true] {
        let h = Harness::new_with_owned(2, true).await;
        let (mut client, id) = h.connect().await;
        let account = h.store.load(&id).unwrap();
        let mut tokens = h.store.tokens(&account).unwrap();
        tokens.access_token = "synthetic-concurrently-rotated-frame".into();
        h.store
            .save(&account, &tokens, Some(account.generation))
            .unwrap();
        let raw = r#"{"type":"error","status":401,"error":{"code":"token_revoked"}}"#;
        let mut replies = Vec::new();
        if committed {
            replies.push(Message::Text(r#"{"type":"response.created"}"#.into()));
        }
        replies.push(Message::Text(raw.into()));
        h.fake.state.push_reply(replies);
        create(&mut client, "fixture-model").await;
        if committed {
            assert!(matches!(receive(&mut client).await, Message::Text(_)));
            assert_eq!(receive(&mut client).await, Message::Text(raw.into()));
            client.close(None).await.unwrap();
        } else {
            assert_close(receive(&mut client).await, 1012);
            let (mut reconnect, next) = h.connect().await;
            assert_ne!(next, id);
            reconnect.close(None).await.unwrap();
        }
        assert_eq!(h.store.load(&id).unwrap().health, Health::Ready);
        assert_eq!(h.store.load(&id).unwrap().auth_failures, 0);
        assert!(h.token_fake.state.requests().is_empty());
    }
}

#[tokio::test]
async fn c5_ws_turn_inflight_guard_is_released_after_terminal_frame() {
    let h = Harness::new(1).await;
    let (mut client, id) = h.connect().await;
    assert_eq!(h.state.accounts.inflight_count(&id), 0);
    create(&mut client, "fixture-model").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.fake.state.messages().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.state.accounts.inflight_count(&id), 1);
    h.fake
        .state
        .send(Message::Text(r#"{"type":"response.completed"}"#.into()))
        .unwrap();
    let _ = receive(&mut client).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.state.accounts.inflight_count(&id) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13a_successful_turn_clears_auth_retry_marker_for_next_401() {
    for terminal in ["response.completed", "response.done"] {
        let h = Harness::new_with_owned(1, true).await;
        let raw = r#"{"type":"error","status":401,"error":{"type":"authentication_error"}}"#;
        h.token_fake.state.push(ScriptedResponse::json(StatusCode::OK,
            r#"{"access_token":"synthetic-first-refresh","refresh_token":"synthetic-first-rotation","expires_in":3600}"#));
        h.fake.state.push_reply([Message::Text(raw.into())]);
        let (mut client, id) = h.connect().await;
        create(&mut client, "fixture-model").await;
        assert_eq!(receive(&mut client).await, Message::Text(raw.into()));
        client.close(None).await.unwrap();
        let (mut refreshed, next) = h.connect().await;
        assert_eq!(next, id);
        h.fake
            .state
            .push_reply([Message::Text(json!({"type":terminal}).to_string().into())]);
        create(&mut refreshed, "fixture-model").await;
        let _ = receive(&mut refreshed).await;
        // Terminal delivery finalizes the turn and clears the matching marker.
        tokio::time::timeout(Duration::from_secs(5), async {
            while h.state.accounts.inflight_count(&id) != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        h.token_fake.state.push(ScriptedResponse::json(StatusCode::OK,
            r#"{"access_token":"synthetic-second-refresh","refresh_token":"synthetic-second-rotation","expires_in":3600}"#));
        h.fake.state.push_reply([Message::Text(raw.into())]);
        create(&mut refreshed, "fixture-model").await;
        assert_eq!(receive(&mut refreshed).await, Message::Text(raw.into()));
        assert_eq!(h.token_fake.state.requests().len(), 2);
        let account = h.store.load(&id).unwrap();
        assert_eq!(account.health, Health::Ready);
        assert_eq!(account.auth_failures, 0);
        assert_eq!(
            h.store.tokens(&account).unwrap().access_token,
            "synthetic-second-refresh"
        );
        refreshed.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn c6_rows_9_10_ws_upgrade_limit_header_on_400_500_is_verbatim_without_cooling() {
    for status in [StatusCode::BAD_REQUEST, StatusCode::INTERNAL_SERVER_ERROR] {
        let h = Harness::new(2).await;
        let body =
            r#"{"error":{"type":"synthetic_nonquota_error","message":"synthetic untouched body"}}"#;
        let mut headers = HeaderMap::new();
        headers.insert("x-codex-limit-reached", "true".parse().unwrap());
        headers.insert("x-codex-primary-used-percent", "100".parse().unwrap());
        headers.insert(
            "x-codex-primary-reset-after-seconds",
            "300".parse().unwrap(),
        );
        h.fake.state.push_handshake(status, body, headers);
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{}/v1/responses", h.servers.data_addr))
            .headers(h.request().headers().clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["x-codex-limit-reached"], "true");
        assert_eq!(response.bytes().await.unwrap().as_ref(), body.as_bytes());
        assert_eq!(h.fake.state.requests().len(), 1);
        for id in &h.ids {
            assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
        }
    }
}

#[tokio::test]
async fn c6_row_13a_quota_error_frame_without_http_status_still_cools_then_fails_over() {
    let h = Harness::new(2).await;
    let raw = r#"{"type":"error","error":{"type":"usage_limit_reached","resets_in_seconds":300}}"#;
    h.fake.state.push_reply([Message::Text(raw.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    assert!(matches!(
        h.store.load(&account).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    let (mut reconnect, next) = h.connect().await;
    assert_ne!(next, account);
    reconnect.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_row_13_transient_refresh_cools_auth_even_while_own_probe_is_held() {
    let h = Harness::new_with_owned(2, true).await;
    let id = &h.ids[0];
    h.state
        .accounts
        .transition(id, HealthSignal::AuthFailure)
        .await
        .unwrap();
    h.state
        .accounts
        .transition(id, HealthSignal::Ready)
        .await
        .unwrap();
    assert_eq!(h.store.load(id).unwrap().auth_failures, 1);
    h.token_fake.state.push(ScriptedResponse::json(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"synthetic transient during probe"}"#,
    ));
    h.fake.state.push_handshake(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"type":"authentication_error"}}"#,
        HeaderMap::new(),
    );
    let (mut client, next) = h.connect().await;
    assert_eq!(next, h.ids[1]);
    let account = h.store.load(id).unwrap();
    assert!(
        matches!(account.health, Health::Cooling { reason: HealthReason::Auth, until, .. }
        if (until - Utc::now()).num_seconds() >= 295 && (until - Utc::now()).num_seconds() <= 300)
    );
    assert_eq!(account.auth_failures, 1);
    assert!(!h.state.accounts.probing(id));
    assert_eq!(h.token_fake.state.requests().len(), 1);
    assert_eq!(h.fake.state.requests().len(), 2);
    client.close(None).await.unwrap();
}
