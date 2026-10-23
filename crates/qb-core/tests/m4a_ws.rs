use axum::http::{HeaderMap, StatusCode};
#[path = "support/routing_ws.rs"]
mod routing_ws;
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
    pins: Arc<DebugPins>,
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
        Self::new_with_rate_options(count, owned, fail_first, None).await
    }
    async fn new_with_rate_options(
        count: usize,
        owned: bool,
        fail_first: bool,
        rate: Option<(u64, u32)>,
    ) -> Self {
        Self::new_with_options(count, owned, fail_first, rate, None).await
    }
    async fn new_with_deadlines(http: u64, ws: u64) -> Self {
        Self::new_with_options(1, false, false, None, Some((http, ws))).await
    }
    async fn new_with_options(
        count: usize,
        owned: bool,
        fail_first: bool,
        rate: Option<(u64, u32)>,
        deadlines: Option<(u64, u64)>,
    ) -> Self {
        Self::new_with_budget_options(count, owned, fail_first, rate, deadlines, None).await
    }
    async fn new_with_budget_options(
        count: usize,
        owned: bool,
        fail_first: bool,
        rate: Option<(u64, u32)>,
        deadlines: Option<(u64, u64)>,
        budget: Option<u64>,
    ) -> Self {
        Self::new_configured(count, owned, fail_first, rate, deadlines, budget, |_| {}).await
    }
    async fn new_configured(
        count: usize,
        owned: bool,
        fail_first: bool,
        rate: Option<(u64, u32)>,
        deadlines: Option<(u64, u64)>,
        budget: Option<u64>,
        configure: impl FnOnce(&mut Config),
    ) -> Self {
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
        if let Some(budget) = budget {
            config.guard.inflight_budget_mib = budget;
        }
        if let Some((hold, retries)) = rate {
            config.routing.rate_hold_secs = hold;
            config.routing.ws_upgrade_hold_secs = hold;
            config.routing.rate_hold_max_retries = retries;
        }
        if let Some((http, ws)) = deadlines {
            config.routing.request_timeout_secs = http;
            config.routing.move_guard.trip_seconds = http.saturating_sub(1).min(240);
            config.routing.ws_upgrade_hold_secs = ws;
        }
        configure(&mut config);
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
            RelayState::new(store.clone(), &config, obs.clone(), pins.clone())
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
            pins,
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
async fn upgrade_full_meter_without_limit_marker_fails_over_as_quota() {
    let h = Harness::new(2).await;
    let mut headers = HeaderMap::new();
    for (key, value) in [
        ("x-codex-primary-used-percent", "100"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-after-seconds", "3600"),
    ] {
        headers.insert(key, value.parse().unwrap());
    }
    h.fake
        .state
        .push_handshake(StatusCode::TOO_MANY_REQUESTS, "{}", headers);
    let (mut client, account) = h.connect().await;
    assert_eq!(account, h.ids[1]);
    assert_eq!(h.fake.state.requests().len(), 2);
    assert!(matches!(h.store.load(&h.ids[0]).unwrap().health,
        Health::Cooling { reason: HealthReason::Quota, until, .. }
        if (until - Utc::now()).num_seconds().abs_diff(3600) <= 2));
    client.close(None).await.unwrap();
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
    let (mut client, account) = h.connect().await;
    let started = std::time::Instant::now();
    create(&mut client, "fixture-model").await;
    assert_eq!(receive(&mut client).await, Message::Text(rate.into()));
    assert!(started.elapsed() < Duration::from_millis(750));
    let pacing = h.state.accounts.pacing_snapshot(&account);
    assert!(pacing.rate_step > 0);
    assert!(
        pacing
            .rate_backoff_until
            .unwrap()
            .saturating_duration_since(std::time::Instant::now())
            >= Duration::from_secs(4)
    );
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_3_ws_rate_frame_without_advice_closes_1012_same_account() {
    // Real shared pacing now applies; bound the protocol assertion rather than
    // waiting through the default exponential resend sequence.
    let h = Harness::new_with_rate_options(2, false, false, Some((1, 3))).await;
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
async fn r12_9_upgrade_connection_limit_spreads_while_old_socket_stays_live() {
    let h = Harness::new(2).await;
    h.fake.state.keep_old_connections();
    let (mut first, original) = h.connect().await;
    h.fake.state.push_handshake(
        StatusCode::BAD_REQUEST,
        r#"{"error":{"code":"websocket_connection_limit_reached","message":"never-ledger-capacity-payload"}}"#,
        HeaderMap::new(),
    );
    // This session is sticky on the first account, so the injected rejection
    // must be learned before its upgrade can spread to the other account.
    let (mut second, destination) = h.connect().await;
    assert_ne!(destination, original);
    let mut request = h.request();
    request
        .headers_mut()
        .insert("session-id", "synthetic-capacity-new".parse().unwrap());
    let (mut third, response) = connect_async(request).await.unwrap();
    assert_eq!(
        response.headers()["x-quota-baton-account"],
        destination.as_str()
    );
    let requests = h.fake.state.requests();
    assert_eq!(
        requests.len(),
        4,
        "one rejection, then no further dial to capped account"
    );
    assert_eq!(
        requests[0].headers["authorization"],
        requests[1].headers["authorization"]
    );
    assert_ne!(
        requests[1].headers["authorization"],
        requests[2].headers["authorization"]
    );
    assert_eq!(
        requests[2].headers["authorization"],
        requests[3].headers["authorization"]
    );
    for id in &h.ids {
        let account = h.store.load(id).unwrap();
        assert_eq!(account.health, Health::Ready);
        assert_eq!(account.auth_failures, 0);
        assert_eq!(account.payment_failures, 0);
    }
    let rows = h.ledger();
    assert!(rows.iter().any(
        |row| row["attempt_details"].as_array().is_some_and(|attempts| {
            attempts
                .iter()
                .any(|attempt| attempt["failover_reason"] == "ws_connection_limit")
        })
    ));
    assert!(
        !rows
            .iter()
            .any(|row| row.to_string().contains("never-ledger-capacity-payload"))
    );
    first.close(None).await.unwrap();
    second.close(None).await.unwrap();
    third.close(None).await.unwrap();
}

#[tokio::test]
async fn r12_9_all_capped_upgrades_expire_without_redial_or_account_failure() {
    let h = Harness::new_with_deadlines(30, 1).await;
    h.fake.state.keep_old_connections();
    let (mut first, account) = h.connect().await;
    h.fake.state.push_handshake(
        StatusCode::BAD_REQUEST,
        r#"{"error":{"code":"websocket_connection_limit_reached"}}"#,
        HeaderMap::new(),
    );
    let started = std::time::Instant::now();
    let (status, body) = h.error().await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body["error"]["type"], "request_timeout");
    assert!(started.elapsed() >= Duration::from_millis(800));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(
        h.fake.state.requests().len(),
        2,
        "full account must not be redialed"
    );
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
    first.close(None).await.unwrap();
}

#[tokio::test]
async fn r12_9_capacity_wait_resumes_when_live_socket_drops() {
    let h = Harness::new_with_deadlines(30, 5).await;
    h.fake.state.keep_old_connections();
    let (mut first, original) = h.connect().await;
    h.fake.state.push_handshake(
        StatusCode::BAD_REQUEST,
        r#"{"error":{"code":"websocket_connection_limit_reached"}}"#,
        HeaderMap::new(),
    );
    let request = h.request();
    let pending = tokio::spawn(async move { connect_async(request).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.fake.state.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !pending.is_finished(),
        "upgrade should wait while the only slot is live"
    );
    assert_eq!(h.fake.state.requests().len(), 2);
    first.close(None).await.unwrap();
    drop(first);
    let (mut replacement, response) = tokio::time::timeout(Duration::from_secs(4), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        response.headers()["x-quota-baton-account"],
        original.as_str()
    );
    assert_eq!(h.fake.state.requests().len(), 3);
    assert_eq!(h.store.load(&original).unwrap().health, Health::Ready);
    replacement.close(None).await.unwrap();
}

// All capacity evidence below comes from synthetic real WS upgrades/frames.
async fn capacity_count(h: &Harness, account: &str, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while h.state.ws_reserved_for_test(account) != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("socket reservation transition");
}

async fn capacity_wait_started(h: &Harness) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.state.ws_capacity_waits_for_test() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual upgrade entered capacity wait");
}

fn session_request(h: &Harness, session: &str) -> axum::http::Request<()> {
    let mut request = h.request();
    request
        .headers_mut()
        .insert("session-id", session.parse().unwrap());
    request
}

async fn learn_one_socket_cap(h: &Harness, id: &str, session: &str) -> Client {
    h.pins.set(Provider::Codex, None, id.into());
    let (live, _) = connect_async(session_request(h, session)).await.unwrap();
    let error = json!({"type":"error","status":400,"error":{
        "code":"websocket_connection_limit_reached","message":"never-ledger-stream-body"
    }})
    .to_string();
    h.fake.state.queue_send(Message::Text(error.clone().into()));
    let (mut rejected, _) = connect_async(session_request(h, session)).await.unwrap();
    assert_eq!(receive(&mut rejected).await, Message::Text(error.into()));
    // Rejected sockets remain occupied until close, but are not usable capacity.
    assert_eq!(h.state.ws_reserved_for_test(id), 2);
    rejected.close(None).await.unwrap();
    drop(rejected);
    capacity_count(h, id, 1).await;
    h.pins.clear();
    live
}

#[tokio::test]
async fn r12_9_losing_actual_upgrade_slot_race_preserves_binding_and_old_create() {
    let h = Harness::new_with_options(2, false, false, None, Some((30, 2))).await;
    h.fake.state.keep_old_connections();
    let a = &h.ids[0];
    let b = &h.ids[1];
    let mut original = learn_one_socket_cap(&h, a, "synthetic-ws-session").await;
    let mut b_live = learn_one_socket_cap(&h, b, "synthetic-b").await;
    b_live.close(None).await.unwrap();
    drop(b_live);
    capacity_count(&h, b, 0).await;
    let (collected, resume) = h.state.pause_next_ws_selection();
    let request = h.request();
    let losing = tokio::spawn(async move { connect_async(request).await });
    tokio::time::timeout(Duration::from_secs(2), collected)
        .await
        .unwrap()
        .unwrap();
    // B was available in the losing upgrade's collected candidates. Another
    // actual upgrade reserves its final slot before guarded selection resumes.
    h.pins.set(Provider::Codex, None, b.clone());
    let (mut winner, response) = connect_async(session_request(&h, "synthetic-winner"))
        .await
        .unwrap();
    assert_eq!(response.headers()["x-quota-baton-account"], b.as_str());
    h.pins.clear();
    resume.send(()).unwrap();
    let Error::Http(response) = tokio::time::timeout(Duration::from_secs(3), losing)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
    else {
        panic!("capacity timeout expected")
    };
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        h.state
            .bindings
            .current(Some("codex:synthetic-ws-session"))
            .as_deref(),
        Some(a.as_str())
    );
    let before = h.fake.state.requests().len();
    h.fake.state.set_echo(true);
    create(&mut original, "race-survivor").await;
    let Message::Text(echo) = receive(&mut original).await else {
        panic!("original socket create must survive")
    };
    assert_eq!(
        serde_json::from_str::<Value>(&echo).unwrap()["model"],
        "race-survivor"
    );
    assert_eq!(
        h.fake.state.requests().len(),
        before,
        "loser must never dial"
    );
    original.close(None).await.unwrap();
    winner.close(None).await.unwrap();
}

#[tokio::test]
async fn r12_9_strict_recovery_veto_precedes_capacity_wait() {
    let h = Harness::new(1).await;
    h.fake.state.keep_old_connections();
    let mut original = learn_one_socket_cap(&h, &h.ids[0], "synthetic-ws-session").await;
    routing_ws::strict(&h).await;
    let path = h.home.join("state/routing-policies.json");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let table = h.state.bindings.clone();
    tokio::task::spawn_blocking(move || {
        assert!(
            table
                .policies
                .put_project(
                    None,
                    qb_core::routing::Project {
                        name: "synthetic-write-failure".into(),
                        settings: qb_core::routing::Settings::default(),
                    }
                )
                .is_err()
        );
    })
    .await
    .unwrap();
    assert!(
        !h.state
            .bindings
            .policies
            .permits_dispatch(Some("codex:synthetic-ws-session"), &h.ids[0])
    );
    let before = h.fake.state.requests().len();
    let (status, body) = tokio::time::timeout(Duration::from_secs(2), h.error())
        .await
        .expect("policy recovery veto must return before capacity expiry");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["type"], "routing_policy_blocked");
    assert_eq!(h.state.ws_capacity_waits_for_test(), 0);
    assert_eq!(h.fake.state.requests().len(), before);
    assert_eq!(h.state.ws_reserved_for_test(&h.ids[0]), 1);
    let _ = original.close(None).await;
}

#[tokio::test]
async fn r12_9_healthy_unanchored_strict_first_placement_waits_then_dispatches() {
    let h = Harness::new(1).await;
    h.fake.state.keep_old_connections();
    let mut live = learn_one_socket_cap(&h, &h.ids[0], "synthetic-existing").await;
    let table = h.state.bindings.clone();
    tokio::task::spawn_blocking(move || {
        table
            .set_routing_session(
                &qb_core::routing::session_id("codex:synthetic-ws-session"),
                Some(qb_core::routing::Settings {
                    mode: qb_core::routing::Mode::Strict,
                    reset_wait_secs: 0,
                }),
                None,
                false,
            )
            .unwrap();
    })
    .await
    .unwrap();
    assert!(
        h.state
            .bindings
            .policies
            .plan(Some("codex:synthetic-ws-session"))
            .anchor
            .is_none()
    );
    let before = h.fake.state.requests().len();
    let request = h.request();
    let pending = tokio::spawn(async move { connect_async(request).await });
    capacity_wait_started(&h).await;
    assert!(!pending.is_finished());
    assert_eq!(h.fake.state.requests().len(), before);
    live.close(None).await.unwrap();
    drop(live);
    let (mut placed, response) = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        response.headers()["x-quota-baton-account"],
        h.ids[0].as_str()
    );
    assert_eq!(
        h.state
            .bindings
            .policies
            .plan(Some("codex:synthetic-ws-session"))
            .anchor
            .as_deref(),
        Some(h.ids[0].as_str())
    );
    placed.close(None).await.unwrap();
}

#[tokio::test]
async fn r12_9_timed_out_blocking_admission_cannot_move_binding_or_break_old_create() {
    let h = Harness::new_with_options(2, false, false, None, Some((30, 1))).await;
    h.fake.state.keep_old_connections();
    let a = &h.ids[0];
    let b = &h.ids[1];
    let mut original = learn_one_socket_cap(&h, a, "synthetic-ws-session").await;
    let mut b_live = learn_one_socket_cap(&h, b, "synthetic-b").await;
    b_live.close(None).await.unwrap();
    drop(b_live);
    capacity_count(&h, b, 0).await;
    let (entered, resume) = h.state.pause_next_ws_admission();
    let request = h.request();
    let waiting_since = std::time::Instant::now();
    let pending = tokio::spawn(async move { connect_async(request).await });
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .unwrap()
        .unwrap();
    // The blocking selector now owns the mutation gate. Let the actual HTTP
    // upgrade expire and drop its owner before allowing admission to proceed.
    let Error::Http(response) = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
    else {
        panic!("upgrade expiry expected")
    };
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(
        waiting_since.elapsed() < Duration::from_secs(3),
        "upgrade timeout must not wait for the five-second hook safety bound"
    );
    let before = h.fake.state.requests().len();
    resume.send(()).unwrap();
    // This gate acquisition cannot finish before the detached selector exits.
    let barrier = h
        .state
        .bindings
        .begin_request_async(Some("codex:synthetic-ws-session"), a)
        .await
        .unwrap();
    drop(barrier);
    assert_eq!(
        h.state
            .bindings
            .current(Some("codex:synthetic-ws-session"))
            .as_deref(),
        Some(a.as_str())
    );
    assert_eq!(h.state.ws_reserved_for_test(b), 0);
    h.fake.state.set_echo(true);
    create(&mut original, "detached-admission-survivor").await;
    let Message::Text(echo) = receive(&mut original).await else {
        panic!("old socket must remain usable")
    };
    assert_eq!(
        serde_json::from_str::<Value>(&echo).unwrap()["model"],
        "detached-admission-survivor"
    );
    assert_eq!(
        h.fake.state.requests().len(),
        before,
        "expired selector must never dial"
    );
    original.close(None).await.unwrap();
}

#[tokio::test]
async fn r12_9_unrelated_cap_cannot_mask_unavailable_strict_anchor() {
    let h = Harness::new(2).await;
    h.fake.state.keep_old_connections();
    h.pins.set(Provider::Codex, None, h.ids[0].clone());
    let (mut anchor, _) = h.connect().await;
    h.pins.clear();
    routing_ws::strict(&h).await;
    let mut unrelated = learn_one_socket_cap(&h, &h.ids[1], "synthetic-unrelated").await;
    h.state
        .accounts
        .change(&h.ids[0], Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    let (status, body) = tokio::time::timeout(Duration::from_secs(2), h.error())
        .await
        .expect("strict policy failure must not wait for unrelated capacity");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["type"], "routing_policy_blocked");
    let _ = anchor.close(None).await;
    unrelated.close(None).await.unwrap();
}

#[tokio::test]
async fn r12_9_healthy_strict_anchor_waits_for_own_slot_despite_free_other_account() {
    let h = Harness::new(2).await;
    h.fake.state.keep_old_connections();
    let mut anchor = learn_one_socket_cap(&h, &h.ids[0], "synthetic-ws-session").await;
    routing_ws::strict(&h).await;
    let (collected, resume) = h.state.pause_next_ws_selection();
    let request = h.request();
    let pending = tokio::spawn(async move { connect_async(request).await });
    collected.await.unwrap();
    let before = h.fake.state.requests().len();
    resume.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), async {
            while !pending.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "strict capped anchor must wait"
    );
    assert_eq!(h.fake.state.requests().len(), before);
    anchor.close(None).await.unwrap();
    drop(anchor);
    let (mut replacement, response) = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        response.headers()["x-quota-baton-account"],
        h.ids[0].as_str()
    );
    replacement.close(None).await.unwrap();
}

#[tokio::test]
async fn r12_9_capacity_wait_and_predial_selection_cancel_on_drain() {
    // Repeat at a capacity wait and while candidate selection is suspended.
    for paused_selection in [false, true] {
        let h = Harness::new(1).await;
        h.fake.state.keep_old_connections();
        let mut live = learn_one_socket_cap(&h, &h.ids[0], "synthetic-ws-session").await;
        let (collected, resume) = h.state.pause_next_ws_selection();
        let request = h.request();
        let pending = tokio::spawn(async move { connect_async(request).await });
        collected.await.unwrap();
        let mut resume = Some(resume);
        if !paused_selection {
            resume.take().unwrap().send(()).unwrap();
            capacity_wait_started(&h).await;
        }
        let before = h.fake.state.requests().len();
        let state = h.state.clone();
        let drain = tokio::spawn(async move { state.begin_drain().await });
        h.state.wait_for_drain_start().await;
        // Freeing the old slot during drain must not authorize another dial.
        let _ = live.close(None).await;
        let Error::Http(response) = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("drain cancels queued upgrade promptly")
            .unwrap()
            .unwrap_err()
        else {
            panic!("handover response expected")
        };
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "1");
        assert_eq!(h.fake.state.requests().len(), before);
        drop(resume);
        drain.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn r12_9_reserved_predial_rate_wait_cancels_and_releases_slot_on_drain() {
    let h = Harness::new(1).await;
    h.state
        .accounts
        .observe_429(&h.ids[0], Some(Duration::from_secs(20)));
    let request = h.request();
    let pending = tokio::spawn(async move { connect_async(request).await });
    capacity_count(&h, &h.ids[0], 1).await;
    let state = h.state.clone();
    let drain = tokio::spawn(async move { state.begin_drain().await });
    let Error::Http(response) = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
    else {
        panic!("handover response expected")
    };
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    capacity_count(&h, &h.ids[0], 0).await;
    assert!(
        h.fake.state.requests().is_empty(),
        "cancelled admission must not dial"
    );
    drain.await.unwrap().unwrap();
}

#[tokio::test]
async fn r12_9_capped_upgrade_cancels_while_drain_waits_for_detached_refresh() {
    let h = Harness::new_with_owned(2, true).await;
    h.fake.state.keep_old_connections();
    let mut live = learn_one_socket_cap(&h, &h.ids[0], "synthetic-ws-session").await;
    routing_ws::strict(&h).await;
    h.pins.set(Provider::Codex, None, h.ids[1].clone());
    h.fake.state.push_handshake(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"token_expired"}}"#,
        HeaderMap::new(),
    );
    let refresh_response = ScriptedResponse::json(
        StatusCode::OK,
        r#"{"access_token":"synthetic-after-drain","refresh_token":"synthetic-drain-rotation","expires_in":3600}"#,
    );
    let (refresh_arrived, refresh_release) = h.token_fake.state.pause_next_response();
    h.token_fake.state.push(refresh_response);
    let refresh_request = session_request(&h, "synthetic-refresh-owner");
    let refreshing = tokio::spawn(async move { connect_async(refresh_request).await });
    tokio::time::timeout(Duration::from_secs(2), refresh_arrived)
        .await
        .expect("detached synthetic refresh started")
        .unwrap();
    h.pins.clear();
    let (collected, resume) = h.state.pause_next_ws_selection();
    let request = h.request();
    let queued = tokio::spawn(async move { connect_async(request).await });
    collected.await.unwrap();
    resume.send(()).unwrap();
    capacity_wait_started(&h).await;
    let state = h.state.clone();
    let drain = tokio::spawn(async move { state.begin_drain().await });
    h.state.wait_for_drain_start().await;
    assert!(
        !drain.is_finished(),
        "refresh shutdown must still be pending"
    );
    let before = h.fake.state.requests().len();
    let _ = live.close(None).await;
    let Error::Http(response) = tokio::time::timeout(Duration::from_secs(1), queued)
        .await
        .expect("capacity waiter cancels before refresh finishes")
        .unwrap()
        .unwrap_err()
    else {
        panic!("handover response expected")
    };
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        !drain.is_finished(),
        "response does not wait for binding freeze"
    );
    refresh_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        h.fake.state.requests().len(),
        before,
        "free slot during refresh shutdown must not redial"
    );
    let Error::Http(response) = refreshing.await.unwrap().unwrap_err() else {
        panic!("refresh owner also cancels")
    };
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn r12_9_stream_rejection_close_spreads_and_records_connection_code() {
    let h = Harness::new(2).await;
    h.fake.state.keep_old_connections();
    let mut live = learn_one_socket_cap(&h, &h.ids[0], "synthetic-ws-session").await;
    let (mut next, destination) = h.connect().await;
    assert_eq!(destination, h.ids[1]);
    let rows = h.ledger();
    let diagnostic = rows
        .iter()
        .find(|row| row["outcome"] == "ws_connection_error")
        .unwrap();
    assert_eq!(diagnostic["account"], h.ids[0]);
    assert_eq!(diagnostic["upstream_status"], 400);
    assert_eq!(
        diagnostic["upstream_error_code"],
        "websocket_connection_limit_reached"
    );
    assert!(diagnostic["session"].is_null());
    assert!(
        rows.iter()
            .any(|row| row["event"] == "ws_open" && row["connection"] == diagnostic["req"])
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.to_string().contains("never-ledger-stream-body"))
    );
    live.close(None).await.unwrap();
    next.close(None).await.unwrap();
}

#[tokio::test]
async fn r12_9_unknown_response_error_is_sanitized_without_finishing_active_turn() {
    let h = Harness::new(1).await;
    let (mut client, _) = h.connect().await;
    h.fake.state.push_reply([Message::Text(
        json!({"type":"response.created","response":{"id":"known-response"}})
            .to_string()
            .into(),
    )]);
    create(&mut client, "fixture-model").await;
    receive(&mut client).await;
    for code in [
        "websocket_connection_limit_reached",
        "sk-never-ledger-secret",
        "invalid code with spaces",
    ] {
        let event = json!({"type":"error","response_id":"unknown-response","status":400,
            "error":{"code":code,"message":"never-ledger-unknown-body"}})
        .to_string();
        h.fake
            .state
            .send(Message::Text(event.clone().into()))
            .unwrap();
        assert_eq!(receive(&mut client).await, Message::Text(event.into()));
    }
    let rows = h.ledger();
    let errors: Vec<_> = rows
        .iter()
        .filter(|row| row["outcome"] == "ws_connection_error")
        .collect();
    assert_eq!(errors.len(), 3);
    assert_eq!(
        errors[0]["upstream_error_code"],
        "websocket_connection_limit_reached"
    );
    assert!(errors[1]["upstream_error_code"].is_null());
    assert!(errors[2]["upstream_error_code"].is_null());
    assert!(!rows.iter().any(|row| row["outcome"] == "upstream_error"));
    assert!(
        !rows
            .iter()
            .any(|row| row.to_string().contains("never-ledger-unknown-body")
                || row.to_string().contains("sk-never-ledger-secret"))
    );
    h.fake
        .state
        .send(Message::Text(
            json!({"type":"response.completed","response":{"id":"known-response"}})
                .to_string()
                .into(),
        ))
        .unwrap();
    receive(&mut client).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !h
            .ledger()
            .iter()
            .any(|row| row["outcome"] == "ok" && row["model"] == "fixture-model")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("known turn still completes normally");
    client.close(None).await.unwrap();
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
    let limit = r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":2}}"#;
    h.fake
        .state
        .push_handshake(StatusCode::TOO_MANY_REQUESTS, limit, HeaderMap::new());
    let paused = Arc::new(tokio::sync::Notify::new());
    let notify = paused.clone();
    h.fake.state.push_handshake_with_callback(
        StatusCode::TOO_MANY_REQUESTS,
        limit,
        HeaderMap::new(),
        move || {
            // The first hold uses real time: quota eligibility uses Utc, too.
            // Freeze before the second rejection can start another hold.
            tokio::time::pause();
            notify.notify_one();
        },
    );
    let started = std::time::Instant::now();
    let connection = h.connect();
    tokio::pin!(connection);
    tokio::select! {
        biased;
        _ = paused.notified() => {}
        _ = &mut connection => panic!("upgrade completed without A's second rejection"),
    }
    let frozen_at = tokio::time::Instant::now();
    let watchdog = std::time::Instant::now();
    let (mut retry, account) = loop {
        tokio::select! {
            biased;
            result = &mut connection => break result,
            _ = tokio::task::yield_now() => {
                // Keep a task runnable so Tokio cannot auto-advance the frozen
                // clock. This is a liveness watchdog, not the no-second-hold proof.
                assert!(watchdog.elapsed() < Duration::from_secs(15),
                    "failover did not complete with the second reset timer frozen");
            }
        }
    };
    assert_eq!(tokio::time::Instant::now(), frozen_at);
    tokio::time::resume();
    // A second two-second hold must fit even at completion; otherwise C9's
    // real-time connect deadline could skip it and make this proof vacuous.
    assert!(
        started.elapsed() + Duration::from_secs(2) < Duration::from_secs(10),
        "fixture exhausted the budget needed to exercise a second reset hold"
    );
    assert_ne!(account, original);
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[2].received_at - requests[1].received_at >= Duration::from_millis(1800));
    assert_eq!(
        requests[1].headers["authorization"],
        requests[2].headers["authorization"]
    );
    assert_ne!(
        requests[2].headers["authorization"],
        requests[3].headers["authorization"]
    );
    let upgrade = h
        .ledger()
        .into_iter()
        .find(|row| row["outcome"] == "ws_upgrade" && row["attempts"] == 3)
        .unwrap();
    let attempts = upgrade["attempt_details"].as_array().unwrap();
    assert_eq!(attempts[0]["failover_reason"], "reset_hold_retry");
    assert_eq!(attempts[1]["failover_reason"], "quota");
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt["failover_reason"] == "reset_hold_retry")
            .count(),
        1
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
async fn c7_1_idle_pinned_socket_closed_1000_and_reconnect_replaced() {
    let h = Harness::new(2).await;
    let (mut idle, original) = h.connect().await;
    // A marked idle binding is drained at the socket boundary.  Pausing the
    // account is the deterministic testkit way to create the same exclusion
    // while the socket remains pinned and no request is in flight.
    h.state
        .accounts
        .change(&original, Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    assert_close(receive(&mut idle).await, 1000);
    let (mut reconnect, replacement) = h.connect().await;
    assert_ne!(replacement, original);
    assert_eq!(replacement, h.ids[1]);
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
async fn r14_ws_repeated_payment_rejections_reach_third_account_with_default_move_budget() {
    let h = Harness::new_with_options(3, false, false, None, Some((3, 1))).await;
    let body = r#"{"error":{"message":"synthetic payment required"}}"#;
    for _ in 0..2 {
        h.fake
            .state
            .push_handshake(StatusCode::PAYMENT_REQUIRED, body, HeaderMap::new());
    }
    let (mut client, served) = h.connect().await;
    assert_eq!(served, h.ids[2]);
    assert_eq!(h.fake.state.requests().len(), 3);
    for id in &h.ids[..2] {
        assert!(matches!(
            h.store.load(id).unwrap().health,
            Health::Cooling {
                reason: HealthReason::Payment,
                ..
            }
        ));
    }
    assert_eq!(h.store.load(&served).unwrap().health, Health::Ready);
    assert_eq!(
        h.state.bindings.current(Some("codex:synthetic-ws-session")),
        Some(served)
    );
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_4_402_websocket_upgrade_cools_account_then_moves_binding() {
    let h = Harness::new(2).await;
    let body = r#"{"error":{"message":"synthetic payment required"}}"#;
    h.fake
        .state
        .push_handshake(StatusCode::PAYMENT_REQUIRED, body, HeaderMap::new());
    let started = Utc::now();
    let (mut client, served) = h.connect().await;
    assert_eq!(served, h.ids[1]);
    assert_eq!(
        h.state.bindings.current(Some("codex:synthetic-ws-session")),
        Some(served.clone())
    );
    let cooled = h.store.load(&h.ids[0]).unwrap();
    assert!(matches!(
        cooled.source,
        qb_core::accounts::Source::Cpa { .. }
    ));
    assert_eq!(cooled.payment_failures, 1);
    assert!(
        matches!(cooled.health, Health::Cooling { reason: HealthReason::Payment, until, ref scope }
        if scope == "account" && until >= started + ChronoDuration::hours(6)
            && until <= Utc::now() + ChronoDuration::hours(6))
    );
    assert_eq!(h.store.load(&served).unwrap().health, Health::Ready);
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn c6_4_402_websocket_native_when_no_account_remains() {
    let h = Harness::new(2).await;
    let body = r#"{"error":{"message":"synthetic payment required"}}"#;
    for _ in 0..2 {
        h.fake
            .state
            .push_handshake(StatusCode::PAYMENT_REQUIRED, body, HeaderMap::new());
    }
    let (status, returned) = h.error().await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(returned, serde_json::from_str::<Value>(body).unwrap());
    for id in &h.ids {
        let a = h.store.load(id).unwrap();
        assert!(matches!(a.source, qb_core::accounts::Source::Cpa { .. }));
        assert!(
            matches!(a.health, Health::Cooling { reason: HealthReason::Payment, until, .. }
            if until > Utc::now() + ChronoDuration::hours(5))
        );
    }
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
    // Isolate three permission-exclusion moves; Ready health supplies no R14
    // fresh-unusable exception. Default budgets remain covered independently.
    let h = Harness::new_configured(4, false, false, None, None, None, |config| {
        config.routing.move_guard.session_ten_minutes = 3;
    })
    .await;
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

#[tokio::test]
async fn r1_ws_open_and_redial_force_one_send_at_the_same_request_deadline() {
    for redial in [false, true] {
        let h = Harness::new_with_rate_options(1, false, false, Some((1, 5))).await;
        let rate = Message::Text(
            r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into(),
        );
        if redial {
            h.fake.state.push_reply([
                rate.clone(),
                Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: 1012.into(),
                    reason: "synthetic close".into(),
                })),
            ]);
        } else {
            h.fake.state.push_reply([rate.clone()]);
        }
        h.fake.state.push_reply([rate]);
        let (mut client, account) = h.connect().await;
        let started = std::time::Instant::now();
        create(&mut client, "fixture-model").await;
        assert_close(
            tokio::time::timeout(Duration::from_millis(1800), receive(&mut client))
                .await
                .unwrap(),
            1012,
        );
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "classified Rate must feed the shared gate"
        );
        assert!(h.state.accounts.pacing_snapshot(&account).rate_step > 0);
        assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
        assert_eq!(
            h.fake
                .state
                .messages()
                .iter()
                .filter(|message| matches!(message, Message::Text(_)))
                .count(),
            2
        );
        let auth: Vec<_> = h
            .fake
            .state
            .requests()
            .into_iter()
            .map(|request| request.headers["authorization"].clone())
            .collect();
        assert!(auth.iter().all(|value| value == auth[0]));
        let row = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(row) = h.ledger().into_iter().find(|row| {
                    row["transport"] == "ws" && row["outcome"] == "rate_hold_relayed_429"
                }) {
                    break row;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(row["attempts"], 2);
        assert_eq!(row["rate_retries"], 1);
        assert!(row["rate_wait_ms"].as_u64().unwrap() >= 800);
    }
}

#[tokio::test]
async fn r1_ws_client_close_cancels_rate_wait_and_ledgers_elapsed_wait() {
    let h = Harness::new_with_rate_options(1, false, false, Some((1, 5))).await;
    h.fake.state.push_reply([Message::Text(
        r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into(),
    )]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while h.state.accounts.pacing_snapshot(&account).waiting == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let ping = bytes::Bytes::from_static(b"held-ping");
    client.send(Message::Ping(ping.clone())).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(250), receive(&mut client))
            .await
            .unwrap(),
        Message::Pong(ping),
    );
    client
        .send(Message::Pong(bytes::Bytes::from_static(b"held-pong")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    client.close(None).await.unwrap();
    // Complete the close exchange; the relay's bounded close handshake may
    // finish after admission cancellation, which we check independently.
    assert!(matches!(
        tokio::time::timeout(Duration::from_millis(500), client.next())
            .await
            .unwrap(),
        Some(Ok(Message::Close(_))) | None
    ));
    tokio::time::timeout(Duration::from_millis(100), async {
        while h.state.accounts.pacing_snapshot(&account).waiting != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let row = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(row) = h
                .ledger()
                .into_iter()
                .find(|row| row["transport"] == "ws" && row["outcome"] == "client_drop")
            {
                break row;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("client-close completion missing: {:?}", h.ledger()));
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["rate_retries"], 0);
    assert!(row["rate_wait_ms"].as_u64().unwrap() >= 30);
    tokio::time::timeout(Duration::from_millis(100), async {
        while h.state.accounts.pacing_snapshot(&account).waiting != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|message| matches!(message, Message::Text(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn r1_ws_upgrade_http_date_observed_and_forced_initial_send_is_final() {
    let h = Harness::new_with_rate_options(1, false, false, Some((1, 5))).await;
    let id = &h.ids[0];
    h.state
        .accounts
        .observe_429(id, Some(Duration::from_secs(60)));
    let mut advice = HeaderMap::new();
    let date = (Utc::now() + ChronoDuration::seconds(120))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    advice.insert("retry-after", date.parse().unwrap());
    h.fake.state.push_handshake(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"code":"rate_limit_exceeded"}}"#,
        advice,
    );
    let started = std::time::Instant::now();
    let (status, _) = tokio::time::timeout(Duration::from_millis(1800), h.error())
        .await
        .unwrap();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(
        h.fake.state.requests().len(),
        1,
        "forced initial upgrade cannot retry its 429"
    );
    let pacing = h.state.accounts.pacing_snapshot(id);
    assert!(
        pacing
            .rate_backoff_until
            .unwrap()
            .saturating_duration_since(std::time::Instant::now())
            > Duration::from_secs(59),
        "HTTP-date advice must extend the old window (capped at 60s)"
    );
    let row = h
        .ledger()
        .into_iter()
        .find(|row| row["outcome"] == "ws_upgrade_error")
        .unwrap();
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["rate_retries"], 0);
    assert!(row["rate_wait_ms"].as_u64().unwrap() >= 800);
    assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
}

#[tokio::test]
async fn r1_ws_upgrade_retries_share_the_original_pacing_deadline() {
    let h = Harness::new_with_rate_options(1, false, false, Some((1, 5))).await;
    let id = &h.ids[0];
    // Set a synthetic window whose remaining backoff is 600ms. The first
    // upgrade consumes that budget before receiving a fresh Rate 429.
    let old = std::time::Instant::now() - Duration::from_millis(1400);
    h.state.accounts.observe_429_at(id, old, old, None, 1.0);
    for _ in 0..2 {
        h.fake.state.push_handshake(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"code":"rate_limit_exceeded"}}"#,
            HeaderMap::new(),
        );
    }
    let started = std::time::Instant::now();
    let (status, _) = tokio::time::timeout(Duration::from_millis(1400), h.error())
        .await
        .unwrap();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(h.fake.state.requests().len(), 2);
    let row = h
        .ledger()
        .into_iter()
        .find(|row| row["outcome"] == "ws_upgrade_error")
        .unwrap();
    assert_eq!(row["attempts"], 2);
    assert_eq!(row["rate_retries"], 1);
    assert!(row["rate_wait_ms"].as_u64().unwrap() >= 800);
    assert!(
        row["rate_wait_ms"].as_u64().unwrap() < 1400,
        "admission must not restart its hold on retry"
    );
}

#[tokio::test]
async fn r1_ws_rate_wait_defers_one_charged_text_or_binary_and_preserves_bytes() {
    for extra in [
        Message::Text("  {\"type\":\"future.unknown\",\"opaque\":\"untouched\"} \n".into()),
        Message::Binary(vec![0, 255, 128, 13, 10, 0, 42].into()),
    ] {
        let h = Harness::new_with_rate_options(1, false, false, Some((1, 5))).await;
        h.fake.state.push_reply([Message::Text(
            r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into(),
        )]);
        let created = Message::Text(
            r#"{"type":"response.created","response":{"id":"deferred-fixture"}}"#.into(),
        );
        h.fake.state.push_reply([created.clone()]);
        h.fake.state.set_echo(true);
        let (mut client, account) = h.connect().await;
        create(&mut client, "fixture-model").await;
        tokio::time::timeout(Duration::from_millis(500), async {
            while h.state.accounts.pacing_snapshot(&account).waiting == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let retry_bytes = h.state.budget.used();
        assert!(retry_bytes > 0);
        client.send(extra.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_millis(500), async {
            while h.state.budget.used() != retry_bytes + extra.len() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // This reserves accounting only, without allocating a large payload.
        // The deferred slot must drain even while the shared budget is full.
        let ballast_bytes = qb_core::guard::mib_bytes(Config::default().guard.inflight_budget_mib);
        let ballast = h.state.budget.reserve_message(ballast_bytes);
        let second = Message::Text("second unknown message, still in order".into());
        // A second pending data frame now closes with 1008 so controls remain
        // readable with one bounded slot. Send this later frame after that slot
        // drains; the separate N5b overflow regression covers the close.
        // Memory protection blocks fresh reads, not an already charged slot.
        assert!(h.state.force_memory_sample_for_test(u64::MAX).guarded);
        // The retained slot remains charged while native pacing is pending.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            h.state.budget.used(),
            retry_bytes + extra.len() + ballast_bytes
        );
        assert_eq!(
            h.fake
                .state
                .messages()
                .iter()
                .filter(|message| matches!(message, Message::Text(_) | Message::Binary(_)))
                .count(),
            1
        );

        assert_eq!(receive(&mut client).await, created);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), receive(&mut client))
                .await
                .unwrap(),
            extra,
            "original payload must pass through, never a 1008 close"
        );
        assert!(h.state.force_memory_sample_for_test(u64::MAX).guarded);
        h.state.force_memory_sample_for_test(0);
        drop(ballast);
        client.send(second.clone()).await.unwrap();
        assert_eq!(receive(&mut client).await, second);
        let messages: Vec<_> = h
            .fake
            .state
            .messages()
            .into_iter()
            .filter(|message| matches!(message, Message::Text(_) | Message::Binary(_)))
            .collect();
        assert_eq!(messages.len(), 4); // initial create, resend, deferred, next
        assert_eq!(messages[2], extra);
        assert_eq!(messages[3], second);
        tokio::time::timeout(Duration::from_millis(500), async {
            while h.state.budget.used() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        client.close(None).await.unwrap();
    }
}

async fn r1_create_wait_for_initial_queue(h: &Harness, account: &str) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while h.state.accounts.pacing_snapshot(account).waiting == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn r1_create_record(h: &Harness, model: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let Some(row) = h
                .ledger()
                .into_iter()
                .find(|row| row["transport"] == "ws" && row["model"] == model)
            {
                return row;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn r1_create_first_and_subsequent_unpaced_frames_are_fast_and_byte_exact() {
    let h = Harness::new_with_rate_options(1, false, false, Some((1, 5))).await;
    let done = Message::Text(r#"{"type":"response.completed"}"#.into());
    h.fake.state.push_reply([done.clone()]);
    h.fake.state.push_reply([done.clone()]);
    let (mut client, account) = h.connect().await;
    for model in ["initial-unpaced", "next-unpaced"] {
        let raw = Message::Text(
            format!("  {{\"type\":\"response.create\",\"model\":\"{model}\",\"input\":[]}} \n")
                .into(),
        );
        client.send(raw.clone()).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(750), receive(&mut client))
                .await
                .unwrap(),
            done
        );
        assert_eq!(h.fake.state.messages().last(), Some(&raw));
        let row = r1_create_record(&h, model).await;
        assert_eq!(row["attempts"], 1);
        assert_eq!(row["rate_retries"], 0);
        assert_eq!(row["account"], account);
        assert_eq!(row["outcome"], "ok");
    }
    client.close(None).await.unwrap();
}

#[tokio::test]
async fn r1_create_first_and_persistent_second_gate_after_other_connection_429() {
    for previous_turn in [false, true] {
        // The other connection uses its one allowed retry, returns the native
        // HTTP 429, and leaves the existing socket's shared gate closed.
        let h = Harness::new_with_rate_options(1, false, false, Some((1, 1))).await;
        let done = Message::Text(r#"{"type":"response.completed"}"#.into());
        let (mut client, account) = h.connect().await;
        if previous_turn {
            h.fake.state.push_reply([done.clone()]);
            create(&mut client, "previous-turn").await;
            assert_eq!(receive(&mut client).await, done);
            r1_create_record(&h, "previous-turn").await;
        }
        for _ in 0..2 {
            h.fake.state.push_handshake(
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"error":{"code":"rate_limit_exceeded"}}"#,
                HeaderMap::new(),
            );
        }
        assert_eq!(h.error().await.0, StatusCode::TOO_MANY_REQUESTS);
        h.fake.state.push_reply([done.clone()]);
        let before = h.fake.state.messages().len();
        let started = std::time::Instant::now();
        create(&mut client, "gated-create").await;
        r1_create_wait_for_initial_queue(&h, &account).await;
        assert_eq!(h.state.accounts.inflight_count(&account), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), client.next())
                .await
                .is_err()
        );
        assert_eq!(h.fake.state.messages().len(), before);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), receive(&mut client))
                .await
                .unwrap(),
            done
        );
        assert!(started.elapsed() >= Duration::from_millis(800));
        let row = r1_create_record(&h, "gated-create").await;
        assert_eq!(row["attempts"], 1);
        assert_eq!(row["rate_retries"], 0);
        assert_eq!(row["account"], account);
        assert!(row["rate_wait_ms"].as_u64().unwrap() >= 800);
        assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
        client.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn r1_create_forced_initial_rate_is_final_without_an_extra_send() {
    let h = Harness::new_with_rate_options(1, false, false, Some((1, 5))).await;
    let (mut client, account) = h.connect().await;
    h.state
        .accounts
        .observe_429(&account, Some(Duration::from_secs(3)));
    h.fake.state.push_reply([Message::Text(
        r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into(),
    )]);
    create(&mut client, "forced-initial").await;
    assert_close(
        tokio::time::timeout(Duration::from_secs(2), receive(&mut client))
            .await
            .unwrap(),
        1012,
    );
    // Finish the native close exchange before asking for the final ledger row.
    let _ = client.flush().await;
    let row = r1_create_record(&h, "forced-initial").await;
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(row["outcome"], "rate_hold_relayed_429");
    assert!(row["rate_wait_ms"].as_u64().unwrap() >= 800);
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count(),
        1
    );
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
}

#[tokio::test]
async fn r1_create_initial_queue_keeps_ping_and_close_responsive_without_attempt() {
    let h = Harness::new_with_rate_options(1, false, false, Some((10, 5))).await;
    let (mut client, account) = h.connect().await;
    h.state
        .accounts
        .observe_429(&account, Some(Duration::from_secs(5)));
    create(&mut client, "cancel-initial").await;
    r1_create_wait_for_initial_queue(&h, &account).await;
    assert_eq!(h.state.accounts.inflight_count(&account), 1);
    let ping = bytes::Bytes::from_static(b"initial-wait-ping");
    client.send(Message::Ping(ping.clone())).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(500), receive(&mut client))
            .await
            .unwrap(),
        Message::Pong(ping)
    );
    client
        .send(Message::Pong(bytes::Bytes::from_static(
            b"initial-wait-pong",
        )))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    client.close(None).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_millis(500), client.next())
            .await
            .unwrap(),
        Some(Ok(Message::Close(_))) | None
    ));
    let row = r1_create_record(&h, "cancel-initial").await;
    assert_eq!(row["attempts"], 0);
    assert_eq!(row["attempt_details"], json!([]));
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(row["account"], account);
    assert!(row["rate_wait_ms"].as_u64().unwrap() >= 30);
    assert_eq!(h.state.accounts.inflight_count(&account), 0);
    assert_eq!(h.state.accounts.pacing_snapshot(&account).waiting, 0);
    assert_eq!(h.state.budget.used(), 0);
    assert!(
        h.fake
            .state
            .messages()
            .iter()
            .all(|m| !matches!(m, Message::Text(_) | Message::Binary(_)))
    );
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
}

#[tokio::test]
async fn r1_create_queued_initial_rechecks_eligibility_and_drain_without_dispatch() {
    for drain in [false, true] {
        let h = Harness::new_with_rate_options(1, false, false, Some((1, 5))).await;
        let (mut client, account) = h.connect().await;
        h.state
            .accounts
            .observe_429(&account, Some(Duration::from_secs(3)));
        create(&mut client, "withdraw-initial").await;
        r1_create_wait_for_initial_queue(&h, &account).await;
        let initial_bytes = h.state.budget.used();
        let extra = Message::Binary(vec![0, 255, 128, 13, 10].into());
        client.send(extra.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_millis(500), async {
            while h.state.budget.used() != initial_bytes + extra.len() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if drain {
            h.state.begin_drain().await.unwrap();
        } else {
            h.state
                .accounts
                .change(&account, Some(UserState::Paused), None, None, None)
                .await
                .unwrap();
        }
        assert_close(
            tokio::time::timeout(Duration::from_secs(2), receive(&mut client))
                .await
                .unwrap(),
            if drain { 1001 } else { 1000 },
        );
        let _ = client.flush().await;
        let row = r1_create_record(&h, "withdraw-initial").await;
        assert_eq!(row["attempts"], 0);
        assert_eq!(row["rate_retries"], 0);
        assert_eq!(row["account"], account);
        assert_eq!(h.state.accounts.inflight_count(&account), 0);
        assert_eq!(h.state.budget.used(), 0);
        assert!(
            h.fake
                .state
                .messages()
                .iter()
                .all(|m| !matches!(m, Message::Text(_) | Message::Binary(_)))
        );
    }
}

#[tokio::test]
async fn r1_create_drain_cancels_queued_turn_but_finishes_dispatched_turn() {
    let h = Harness::new_with_rate_options(1, false, false, Some((10, 5))).await;
    let created =
        Message::Text(r#"{"type":"response.created","response":{"id":"older-turn"}}"#.into());
    h.fake.state.push_reply([created.clone()]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "dispatched-turn").await;
    assert_eq!(receive(&mut client).await, created);
    h.state
        .accounts
        .observe_429(&account, Some(Duration::from_secs(5)));
    create(&mut client, "queued-during-drain").await;
    r1_create_wait_for_initial_queue(&h, &account).await;
    assert_eq!(h.state.accounts.inflight_count(&account), 2);
    h.state.begin_drain().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while h.state.accounts.inflight_count(&account) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let done =
        Message::Text(r#"{"type":"response.completed","response":{"id":"older-turn"}}"#.into());
    h.fake.state.send(done.clone()).unwrap();
    assert_eq!(receive(&mut client).await, done);
    assert_close(receive(&mut client).await, 1001);
    let _ = client.flush().await;
    let queued = r1_create_record(&h, "queued-during-drain").await;
    let dispatched = r1_create_record(&h, "dispatched-turn").await;
    assert_eq!(queued["attempts"], 0);
    assert_eq!(dispatched["attempts"], 1);
    assert_eq!(dispatched["outcome"], "ok");
    assert_eq!(h.state.accounts.inflight_count(&account), 0);
    assert_eq!(h.state.budget.used(), 0);
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
async fn r1_ws_upstream_close_cancels_wait_and_releases_deferred_slot() {
    for extra in [
        Message::Text("  opaque text with original whitespace \n".into()),
        Message::Binary(vec![0, 255, 128, 13, 10, 0, 42].into()),
    ] {
        let h = Harness::new_with_rate_options(1, false, false, Some((1, 5))).await;
        h.fake.state.push_reply([Message::Text(
            r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into(),
        )]);
        let (mut client, account) = h.connect().await;
        // A previous_response_id preserves the upstream close without redial.
        client.send(Message::Text(
            r#"{"type":"response.create","model":"fixture-model","previous_response_id":"fixture-prior","input":[]}"#.into(),
        )).await.unwrap();
        tokio::time::timeout(Duration::from_millis(500), async {
            while h.state.accounts.pacing_snapshot(&account).waiting == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let retry_bytes = h.state.budget.used();
        assert!(retry_bytes > 0);
        client.send(extra.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_millis(500), async {
            while h.state.budget.used() != retry_bytes + extra.len() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let close = Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: 1001.into(),
            reason: "synthetic slot cancellation".into(),
        }));
        h.fake.state.send(close.clone()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(500), receive(&mut client))
                .await
                .unwrap(),
            close,
        );
        tokio::time::timeout(Duration::from_millis(500), async {
            while h.state.budget.used() != 0
                || h.state.accounts.pacing_snapshot(&account).waiting != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let messages: Vec<_> = h
            .fake
            .state
            .messages()
            .into_iter()
            .filter(|message| matches!(message, Message::Text(_) | Message::Binary(_)))
            .collect();
        assert_eq!(
            messages.len(),
            1,
            "close cancels the unsent retry and deferred message"
        );
    }
}

// R1-tray.2: real fake-upstream WS paths, no UI/native calls.
fn tray2_ws_notices(events: &mut tokio::sync::broadcast::Receiver<Value>) -> Vec<Value> {
    let mut notices = Vec::new();
    while let Ok(event) = events.try_recv() {
        for secret in [
            "synthetic-access",
            "synthetic-refresh",
            "conversation-marker",
        ] {
            assert!(!event.to_string().contains(secret));
        }
        if event["event"] == "core_warning" && event["data"]["kind"] == "quota_notice" {
            notices.push(event["data"].clone());
        }
    }
    notices
}

#[tokio::test]
async fn tray2_ws_usage_credits_requires_current_serving_and_disable_clears() {
    for serving in [false, true] {
        let h = Harness::new(1).await;
        let id = &h.ids[0];
        h.state
            .accounts
            .change(id, None, None, None, Some(true))
            .await
            .unwrap();
        let (mut client, account) = h.connect().await;
        let mut events = h.state.accounts.subscribe();
        let event = json!({"type":"codex.rate_limits", "rate_limits":{
            "secondary":{"used_percent":99,"window_minutes":10080,"reset_after_seconds":172800},
            "active_limit":"seven_day", "serving_from_credits":serving,
            "credits":{"has_credits":true}}})
        .to_string();
        h.fake
            .state
            .send(Message::Text(event.clone().into()))
            .unwrap();
        assert_eq!(receive(&mut client).await, Message::Text(event.into()));
        let notices = tray2_ws_notices(&mut events);
        let views = h.state.accounts.views().await.unwrap();
        if serving {
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0]["account"], account);
            assert_eq!(notices[0]["action"], "credits");
            assert_eq!(notices[0]["window"], "7d");
            assert_eq!(views[0]["quota_notice"]["action"], "credits");
        } else {
            assert!(notices.is_empty());
            assert!(views[0]["quota_notice"].is_null());
        }
        h.state
            .accounts
            .change(id, None, None, None, Some(false))
            .await
            .unwrap();
        assert!(h.state.accounts.views().await.unwrap()[0]["quota_notice"].is_null());
        assert!(tray2_ws_notices(&mut events).is_empty());
        let _ = client.close(None).await;
    }
}

#[tokio::test]
async fn tray2_ws_upgrade_actual_credit_headers_publish_flat_context() {
    let h = Harness::new(1).await;
    h.state
        .accounts
        .change(&h.ids[0], None, None, None, Some(true))
        .await
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-codex-credits-has-credits", "true".parse().unwrap());
    headers.insert("x-codex-active-limit", "credits".parse().unwrap());
    h.fake.state.set_success_headers(headers);
    let mut events = h.state.accounts.subscribe();
    let (mut client, id) = h.connect().await;
    let notices = tray2_ws_notices(&mut events);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["account"], id);
    assert_eq!(notices[0]["action"], "credits");
    assert!(notices[0]["reset_at"].is_null());
    assert!(notices[0]["earliest_account"].is_null());
    assert_eq!(
        h.state.accounts.views().await.unwrap()[0]["quota_notice"]["action"],
        "credits"
    );
    let _ = client.close(None).await;
}

#[tokio::test]
async fn tray2_ws_upgrade_reset_hold_suppresses_warning_during_and_after_retry() {
    let h = Harness::new(2).await;
    let (mut first, original) = h.connect().await;
    first.close(None).await.unwrap();
    h.fake.state.push_handshake(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":2}}"#,
        HeaderMap::new(),
    );
    let mut events = h.state.accounts.subscribe();
    let retry = h.connect();
    tokio::pin!(retry);
    tokio::select! {
        _ = &mut retry => panic!("reset hold did not wait"),
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
    }
    assert!(tray2_ws_notices(&mut events).is_empty());
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    let (mut client, account) = retry.await;
    assert_eq!(account, original);
    assert!(tray2_ws_notices(&mut events).is_empty());
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    let _ = client.close(None).await;
}

#[tokio::test]
async fn tray2_ws_committed_quota_resolves_hold_on_next_connection_without_toast() {
    let h = Harness::new(2).await;
    h.fake.state.push_reply([
        Message::Text(r#"{"type":"response.created"}"#.into()),
        Message::Text(r#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","resets_in_seconds":2}}}"#.into()),
    ]);
    let (mut client, original) = h.connect().await;
    let mut events = h.state.accounts.subscribe();
    create(&mut client, "synthetic-model").await;
    assert!(matches!(receive(&mut client).await, Message::Text(_)));
    assert!(matches!(receive(&mut client).await, Message::Text(_)));
    assert!(tray2_ws_notices(&mut events).is_empty());
    let mut other_request = h.request();
    other_request
        .headers_mut()
        .insert("session-id", "tray2-unrelated-session".parse().unwrap());
    let (mut unrelated, _) = connect_async(other_request).await.unwrap();
    assert!(tray2_ws_notices(&mut events).is_empty());
    let _ = unrelated.close(None).await;
    let _ = client.close(None).await;
    let (mut next, id) = h.connect().await;
    assert_eq!(id, original);
    assert!(tray2_ws_notices(&mut events).is_empty());
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    let _ = next.close(None).await;
}

#[tokio::test]
async fn tray2_ws_pending_quota_preference_change_cannot_create_delayed_toast() {
    let h = Harness::new(2).await;
    h.fake.state.push_reply([
        Message::Text(r#"{"type":"response.created"}"#.into()),
        Message::Text(r#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","resets_in_seconds":172800}}}"#.into()),
    ]);
    let (mut client, original) = h.connect().await;
    let mut events = h.state.accounts.subscribe();
    create(&mut client, "synthetic-model").await;
    assert!(matches!(receive(&mut client).await, Message::Text(_)));
    assert!(matches!(receive(&mut client).await, Message::Text(_)));
    h.state
        .accounts
        .change(&original, None, None, None, Some(true))
        .await
        .unwrap();
    let _ = client.close(None).await;
    let (mut next, id) = h.connect().await;
    assert_ne!(id, original);
    assert!(tray2_ws_notices(&mut events).is_empty());
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    let _ = next.close(None).await;
}

#[tokio::test]
async fn tray2_ws_passive_usage_waits_for_actual_reconnect_or_no_eligible() {
    for count in [1, 2] {
        let h = Harness::new(count).await;
        let (mut client, source) = h.connect().await;
        let mut events = h.state.accounts.subscribe();
        let usage = json!({"type":"codex.rate_limits","rate_limits":{
            "secondary":{"used_percent":100,"window_minutes":10080,"reset_after_seconds":172800},
            "active_limit":"secondary"}})
        .to_string();
        h.fake
            .state
            .send(Message::Text(usage.clone().into()))
            .unwrap();
        assert_eq!(receive(&mut client).await, Message::Text(usage.into()));
        assert!(tray2_ws_notices(&mut events).is_empty());
        assert!(
            h.state
                .accounts
                .views()
                .await
                .unwrap()
                .iter()
                .all(|v| v["quota_notice"].is_null())
        );
        assert_close(receive(&mut client).await, 1000);
        if count == 1 {
            let (status, _) = h.error().await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
            let notices = tray2_ws_notices(&mut events);
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0]["account"], source);
            assert_eq!(notices[0]["action"], "exhausted");
        } else {
            let (mut next, served) = h.connect().await;
            assert_ne!(served, source);
            let notices = tray2_ws_notices(&mut events);
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0]["account"], source);
            assert_eq!(notices[0]["action"], "failover");
            assert_eq!(notices[0]["next_account"], served);
            let account = h.store.load(&served).unwrap();
            let token = h.store.tokens(&account).unwrap();
            assert_eq!(
                h.fake.state.requests().last().unwrap().headers["authorization"],
                format!("Bearer {}", token.access_token)
            );
            let _ = next.close(None).await;
        }
    }
}

include!("support/m4a_ws_r11.inc.rs");

const R14_WS_MODEL: &str = "r14-ws-model";
const R14_WS_KEY: &str = "codex:synthetic-ws-session";

fn r14_ws_reply(id: &str, input: u64, cached: u64) -> [Message; 2] {
    [
        Message::text(json!({"type":"response.created","response":{"id":id}}).to_string()),
        Message::text(json!({
            "type":"response.completed","response":{"id":id,"status":"completed",
            "usage":{"input_tokens":input,"input_tokens_details":{"cached_tokens":cached},"output_tokens":1}}
        }).to_string()),
    ]
}

async fn r14_ws_receive_turn(client: &mut Client, id: &str) {
    for kind in ["response.created", "response.completed"] {
        let Message::Text(text) = receive(client).await else {
            panic!("expected {kind} for {id}");
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["type"], kind);
        assert_eq!(event["response"]["id"], id);
    }
}

async fn r14_ws_wait_turns(h: &Harness, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while h
            .ledger()
            .iter()
            .filter(|row| {
                row["transport"] == "ws"
                    && row["model"] == R14_WS_MODEL
                    && matches!(row["outcome"].as_str(), Some("ok" | "rate_hold_ok"))
            })
            .count()
            < count
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual turns reached terminal ledger/accounting");
}

async fn r14_ws_wait_cost(h: &Harness, expected: (u64, u64, u64)) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let cost = h.obs.accounting_snapshot(Utc::now()).hour;
            if (cost.tokens, cost.known_count, cost.unknown_count) == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expected move cost reached");
}

#[tokio::test]
async fn r14_ws_upgrade_unknown_model_claims_cost_only_on_actual_create() {
    let h = Harness::new(2).await;
    h.fake.state.set_echo(false);
    let (mut client, original) = h.connect().await;
    h.fake
        .state
        .push_reply(r14_ws_reply("r14-baseline", 100, 80));
    create(&mut client, R14_WS_MODEL).await;
    r14_ws_receive_turn(&mut client, "r14-baseline").await;
    r14_ws_wait_turns(&h, 1).await;
    client.close(None).await.unwrap();
    h.state
        .accounts
        .change(&original, Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    let (mut next, replacement) = h.connect().await;
    assert_ne!(replacement, original);
    let pending = h.obs.accounting_snapshot(Utc::now()).hour;
    assert_eq!(
        (pending.known_count, pending.unknown_count),
        (0, 1),
        "101 must not consume a usage claim"
    );
    h.fake.state.push_reply(r14_ws_reply("r14-cold", 200, 0));
    create(&mut next, R14_WS_MODEL).await;
    r14_ws_receive_turn(&mut next, "r14-cold").await;
    r14_ws_wait_cost(&h, (180, 1, 0)).await;
    let measured = h.obs.accounting_snapshot(Utc::now()).hour;
    assert_eq!(
        (
            measured.tokens,
            measured.known_count,
            measured.unknown_count
        ),
        (180, 1, 0)
    );
    h.fake.state.push_reply(r14_ws_reply("r14-normal", 50, 20));
    create(&mut next, R14_WS_MODEL).await;
    r14_ws_receive_turn(&mut next, "r14-normal").await;
    r14_ws_wait_turns(&h, 3).await;
    next.close(None).await.unwrap();
    let rows = h.ledger();
    assert_eq!(
        rows.iter()
            .filter(|r| r["event"] == "binding_move_cost" && r["measurement"] == "known")
            .count(),
        1
    );
    assert_eq!(h.fake.state.requests().len(), 2, "real 101 on each account");
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count(),
        3
    );
}

#[tokio::test]
async fn r14_ws_breaker_refuses_return_before_third_upgrade() {
    let h = Harness::new_with_budget_options(2, false, false, None, Some((3, 1)), None).await;
    let (mut first, original) = h.connect().await;
    first.close(None).await.unwrap();
    drop(first);
    capacity_count(&h, &original, 0).await;
    // Leave a healthy A without changing its semantic evidence. Resuming a
    // paused A would correctly authorize a return with changed evidence.
    let replacement = h.ids.iter().find(|id| **id != original).unwrap().clone();
    h.pins.set(Provider::Codex, None, replacement.clone());
    let (mut second, selected) = h.connect().await;
    assert_eq!(selected, replacement);
    h.pins.clear();
    second.close(None).await.unwrap();
    drop(second);
    capacity_count(&h, &replacement, 0).await;
    assert_eq!(h.store.load(&original).unwrap().health, Health::Ready);
    h.state
        .accounts
        .change(&replacement, Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let (status, error) = h.error().await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error["error"]["type"], "account_move_guard");
    assert!(started.elapsed() >= Duration::from_millis(700));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(
        h.fake.state.requests().len(),
        2,
        "refused return must never reach upstream upgrade"
    );
    assert_eq!(
        h.state.bindings.current(Some(R14_WS_KEY)),
        Some(replacement.clone())
    );
    let rows = h.ledger();
    assert!(rows.iter().any(|row| row["event"] == "binding_move_refused"
        && row["from"] == replacement
        && row["to"] == original
        && row["reason"] == "ping_pong"));
    assert!(!rows.iter().any(|row| row["event"] == "binding_move"
        && row["from"] == replacement
        && row["to"] == original));
}

#[tokio::test]
async fn r14_ws_cost_claim_follows_dispatch_order_and_ignores_unsent_veto() {
    for veto in [false, true] {
        let h = Harness::new(2).await;
        h.fake.state.keep_old_connections();
        let (mut source, original) = h.connect().await;
        h.fake
            .state
            .push_reply(r14_ws_reply("r14-order-baseline", 100, 80));
        create(&mut source, R14_WS_MODEL).await;
        r14_ws_receive_turn(&mut source, "r14-order-baseline").await;
        r14_ws_wait_turns(&h, 1).await;
        source.close(None).await.unwrap();
        drop(source);
        capacity_count(&h, &original, 0).await;
        h.state
            .accounts
            .change(&original, Some(UserState::Paused), None, None, None)
            .await
            .unwrap();
        let (mut delayed, replacement) = h.connect().await;
        let (mut first_sent, same_account) = h.connect().await;
        assert_eq!(same_account, replacement);
        assert_ne!(replacement, original);
        r14_ws_wait_cost(&h, (0, 0, 1)).await;

        let raw = json!({"type":"response.create","model":R14_WS_MODEL,"input":[],
            "fixture_marker":"r14-delayed-create"})
        .to_string();
        let (arrived, release) = h
            .state
            .pause_ws_ordinary_before_write(R14_WS_KEY, raw.as_bytes());
        let mut release = Some(release);
        delayed.send(Message::text(raw)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            h.fake
                .state
                .messages()
                .iter()
                .filter(|m| matches!(m, Message::Text(_)))
                .count(),
            1,
            "only the baseline was dispatched before the final write gate"
        );

        if veto {
            // Closing the admission gate rejects this unsent create. The pump
            // must keep the move pending for another socket's actual dispatch.
            drop(release.take());
            assert_close(receive(&mut delayed).await, 1001);
            capacity_count(&h, &replacement, 1).await;
            assert!(
                !h.ledger()
                    .iter()
                    .any(|row| row["event"] == "binding_move_cost"
                        && row["measurement"] == "unknown")
            );
        }

        h.fake
            .state
            .push_reply(r14_ws_reply("r14-first-dispatched", 200, 0));
        create(&mut first_sent, R14_WS_MODEL).await;
        r14_ws_receive_turn(&mut first_sent, "r14-first-dispatched").await;
        r14_ws_wait_turns(&h, 2).await;
        r14_ws_wait_cost(&h, (180, 1, 0)).await;

        if !veto {
            h.fake
                .state
                .push_reply(r14_ws_reply("r14-dispatched-later", 30, 0));
            release.take().unwrap().send(()).unwrap();
            r14_ws_receive_turn(&mut delayed, "r14-dispatched-later").await;
            r14_ws_wait_turns(&h, 3).await;
            delayed.close(None).await.unwrap();
        }
        first_sent.close(None).await.unwrap();
        let cost = h.obs.accounting_snapshot(Utc::now()).hour;
        assert_eq!(
            (cost.tokens, cost.known_count, cost.unknown_count),
            (180, 1, 0)
        );
        let creates: Vec<Value> = h
            .fake
            .state
            .messages()
            .iter()
            .filter_map(|message| {
                if let Message::Text(text) = message {
                    serde_json::from_str(text).ok()
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(creates.len(), if veto { 2 } else { 3 });
        assert!(
            creates[1]["fixture_marker"].is_null(),
            "winner dispatched before the gated create"
        );
        if !veto {
            assert_eq!(creates[2]["fixture_marker"], "r14-delayed-create");
        }
        assert_eq!(
            h.fake.state.requests().len(),
            3,
            "two destination sockets, one committed move"
        );
        assert_eq!(
            h.ledger()
                .iter()
                .filter(|row| row["event"] == "binding_move_cost" && row["measurement"] == "known")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn r14_ws_failed_cold_rate_attempt_stays_unknown_after_resend_and_redial() {
    for redial in [false, true] {
        let h = Harness::new_with_rate_options(2, false, false, Some((1, 3))).await;
        let (mut source, original) = h.connect().await;
        h.fake
            .state
            .push_reply(r14_ws_reply("r14-rate-baseline", 100, 80));
        create(&mut source, R14_WS_MODEL).await;
        r14_ws_receive_turn(&mut source, "r14-rate-baseline").await;
        r14_ws_wait_turns(&h, 1).await;
        source.close(None).await.unwrap();
        drop(source);
        capacity_count(&h, &original, 0).await;
        h.state
            .accounts
            .change(&original, Some(UserState::Paused), None, None, None)
            .await
            .unwrap();
        let (mut destination, replacement) = h.connect().await;
        assert_ne!(replacement, original);
        r14_ws_wait_cost(&h, (0, 0, 1)).await;

        let mut failed = vec![Message::text(
            r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#,
        )];
        if redial {
            failed.push(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: 1012.into(),
                    reason: "synthetic rate redial".into(),
                },
            )));
        }
        h.fake.state.push_reply(failed);
        h.fake
            .state
            .push_reply(r14_ws_reply("r14-rate-replayed", 200, 0));
        create(&mut destination, R14_WS_MODEL).await;
        r14_ws_receive_turn(&mut destination, "r14-rate-replayed").await;
        r14_ws_wait_turns(&h, 2).await;
        r14_ws_wait_cost(&h, (0, 0, 1)).await;
        let rows = h.ledger();
        assert!(rows.iter().any(|row| row["transport"] == "ws"
            && row["outcome"] == "rate_hold_ok"
            && row["rate_retries"] == 1
            && row["cache_usage"]["input_tokens"] == 200));
        assert_eq!(rows.iter().filter(|row| row["event"] == "binding_move_cost"
            && row["measurement"] == "unknown").count(), 1);
        assert_eq!(h.fake.state.requests().len(), if redial { 3 } else { 2 });

        // A fresh normal turn is the only B baseline. The cold replay's 200
        // must not train it; the next committed move should cost 130 - 30.
        h.fake
            .state
            .push_reply(r14_ws_reply("r14-rate-normal", 60, 30));
        create(&mut destination, R14_WS_MODEL).await;
        r14_ws_receive_turn(&mut destination, "r14-rate-normal").await;
        r14_ws_wait_turns(&h, 3).await;
        destination.close(None).await.unwrap();
        drop(destination);
        capacity_count(&h, &replacement, 0).await;
        h.state
            .accounts
            .change(&original, Some(UserState::Active), None, None, None)
            .await
            .unwrap();
        h.state
            .accounts
            .change(&replacement, Some(UserState::Paused), None, None, None)
            .await
            .unwrap();
        let (mut returned, selected) = h.connect().await;
        assert_eq!(
            selected, original,
            "changed evidence legitimately permits this return"
        );
        h.fake
            .state
            .push_reply(r14_ws_reply("r14-after-rate-move", 130, 0));
        create(&mut returned, R14_WS_MODEL).await;
        r14_ws_receive_turn(&mut returned, "r14-after-rate-move").await;
        r14_ws_wait_cost(&h, (100, 1, 1)).await;
        returned.close(None).await.unwrap();
    }
}

// N5b supporting control-read unit: no quota wait authority or new hold limits.
#[cfg(feature = "testkit")]
async fn n5b_controls_deferred(h: &Harness, client: &mut Client, extra: Message) {
    let before = h.state.budget.used();
    client.send(extra.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.state.budget.used() != before + extra.len() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("one extra application frame must occupy the deferred reservation");
}

#[cfg(feature = "testkit")]
async fn n5b_controls_released(h: &Harness, account: &str) {
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let upstream_closed =
                h.fake.state.events().iter().any(|event| {
                    matches!(event, qb_testkit::websocket::ConnectionEvent::Closed(_))
                });
            if upstream_closed
                && h.state
                    .inflight_requests
                    .load(std::sync::atomic::Ordering::Acquire)
                    == 0
                && h.state.accounts.inflight_count(account) == 0
                && h.state.accounts.pacing_snapshot(account).waiting == 0
                && h.state.bindings.pending_count(account) == 0
                && h.state.ws_reserved_for_test(account) == 0
                && h.state.budget.used() == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("control cancellation must close upstream and release native owners");
}

#[cfg(feature = "testkit")]
fn n5b_controls_data(h: &Harness) -> Vec<Message> {
    h.fake
        .state
        .messages()
        .into_iter()
        .filter(|message| matches!(message, Message::Text(_) | Message::Binary(_)))
        .collect()
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn n5b_ws_deferred_initial_keeps_ping_close_and_eof_responsive() {
    for eof in [false, true] {
        let h = Harness::new(1).await;
        let (mut client, account) = h.connect().await;
        let raw =
            json!({"type":"response.create", "model":"synthetic-deferred-controls", "input":[]})
                .to_string();
        let (arrived, release) = h
            .state
            .pause_ws_ordinary_after_pacing("codex:synthetic-ws-session", raw.as_bytes());
        client.send(Message::text(raw)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), arrived)
            .await
            .expect("actual initial admission must reach existing deterministic barrier")
            .unwrap();
        assert!(n5b_controls_data(&h).is_empty());
        n5b_controls_deferred(
            &h,
            &mut client,
            Message::Binary(vec![0, 255, 13, 10, 42].into()),
        )
        .await;
        // Wire ordering proves Pong was consumed before this Ping is answered.
        client
            .send(Message::Pong(bytes::Bytes::from_static(
                b"synthetic-deferred-pong",
            )))
            .await
            .unwrap();
        let ping = bytes::Bytes::from_static(b"synthetic-deferred-ping");
        client.send(Message::Ping(ping.clone())).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receive(&mut client))
                .await
                .expect("Ping after a deferred frame must not wait for admission"),
            Message::Pong(ping),
        );
        assert!(
            !release.is_closed(),
            "Ping must not release/cancel the pending create"
        );
        if eof {
            drop(client);
        } else {
            client.close(None).await.unwrap();
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(1), client.next())
                    .await
                    .expect("Close after deferred frame must precede admission release"),
                Some(Ok(Message::Close(_))) | None
            ));
        }
        n5b_controls_released(&h, &account).await;
        assert!(
            release.is_closed(),
            "Close/EOF must drop the held admission future"
        );
        assert!(
            release.send(()).is_err(),
            "released barrier cannot revive a cancelled create"
        );
        assert!(
            n5b_controls_data(&h).is_empty(),
            "no initial or deferred send after close"
        );
        let row = r1_create_record(&h, "synthetic-deferred-controls").await;
        assert_eq!(row["attempts"], 0);
        assert_eq!(row["attempt_details"], json!([]));
        assert_eq!(row["rate_retries"], 0);
        assert_eq!(row["account"], account);
        assert_eq!(h.fake.state.requests().len(), 1);
        assert_eq!(
            h.state
                .bindings
                .current(Some("codex:synthetic-ws-session"))
                .as_deref(),
            Some(account.as_str())
        );
        assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
    }
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn n5b_ws_second_deferred_data_closes_without_queue_growth() {
    let h = Harness::new(1).await;
    let (mut client, account) = h.connect().await;
    let raw = json!({"type":"response.create", "model":"synthetic-deferred-overflow", "input":[]})
        .to_string();
    let (arrived, release) = h
        .state
        .pause_ws_ordinary_after_pacing("codex:synthetic-ws-session", raw.as_bytes());
    client.send(Message::text(raw)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), arrived)
        .await
        .unwrap()
        .unwrap();
    n5b_controls_deferred(&h, &mut client, Message::text("synthetic-first-deferred")).await;
    client
        .send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    assert_close(
        tokio::time::timeout(Duration::from_secs(1), receive(&mut client))
            .await
            .expect("second extra application frame must trigger bounded protocol close"),
        1008,
    );
    let _ = client.flush().await;
    n5b_controls_released(&h, &account).await;
    assert!(release.is_closed());
    assert!(release.send(()).is_err());
    assert!(n5b_controls_data(&h).is_empty());
    let row = r1_create_record(&h, "synthetic-deferred-overflow").await;
    assert_eq!(row["attempts"], 0);
    assert_eq!(row["rate_retries"], 0);
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn n5b_ws_deferred_rate_resend_keeps_controls_and_cancels_native_queue() {
    let h = Harness::new_configured(1, false, false, Some((60, 5)), None, None, |config| {
        config.routing.ws_upgrade_hold_secs = 10;
    })
    .await;
    h.fake.state.push_reply([Message::text(
        r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#,
    )]);
    let (mut client, account) = h.connect().await;
    // Continuation forbids same-account redial; no quota evidence is introduced.
    client.send(Message::text(
        r#"{"type":"response.create","model":"synthetic-deferred-rate","previous_response_id":"synthetic-prior","input":[]}"#,
    )).await.unwrap();
    r1_create_wait_for_initial_queue(&h, &account).await;
    assert_eq!(
        n5b_controls_data(&h).len(),
        1,
        "actual rate response followed first send"
    );
    // Extend only this synthetic account's native pacing backoff so scheduling
    // cannot release a resend during the bounded control assertions. The turn's
    // existing 60s rate budget is unchanged; this is not a new product default.
    let backoff = h
        .state
        .accounts
        .observe_429(&account, Some(Duration::from_secs(30)));
    let held_until = backoff.rate_backoff_until.unwrap();
    n5b_controls_deferred(&h, &mut client, Message::text("synthetic-rate-deferred")).await;
    client
        .send(Message::Pong(bytes::Bytes::from_static(
            b"synthetic-rate-pong",
        )))
        .await
        .unwrap();
    let ping = bytes::Bytes::from_static(b"synthetic-rate-ping");
    client.send(Message::Ping(ping.clone())).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), receive(&mut client))
            .await
            .expect("rate resend Ping after deferred data must remain responsive"),
        Message::Pong(ping),
    );
    client.close(None).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), client.next())
            .await
            .expect("rate resend Close after deferred data must cancel native admission"),
        Some(Ok(Message::Close(_))) | None
    ));
    n5b_controls_released(&h, &account).await;
    assert!(
        std::time::Instant::now() < held_until,
        "cleanup must precede native backoff expiry"
    );
    let row = r1_create_record(&h, "synthetic-deferred-rate").await;
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(
        n5b_controls_data(&h).len(),
        1,
        "cancelled resend/deferred frame never sent"
    );
    assert_eq!(
        h.fake.state.requests().len(),
        1,
        "no redial after continuation close"
    );
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
}
