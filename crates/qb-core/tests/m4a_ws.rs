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
}
impl Harness {
    async fn new(count: usize) -> Self {
        Self::new_with_owned(count, false).await
    }
    async fn new_with_owned(count: usize, owned: bool) -> Self {
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
        config.upstream.codex_base = fake.url.clone();
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
async fn c6_row_13a_revocation_closes_and_marks_relogin() {
    let h = Harness::new(2).await;
    h.fake.state.push_reply([Message::Text(r#"{"type":"error","status":401,"error":{"code":"token_revoked","message":"synthetic revoked token"}}"#.into())]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_close(receive(&mut client).await, 1012);
    assert!(matches!(
        h.store.load(&account).unwrap().health,
        Health::NeedsRelogin { .. }
    ));
    let (mut reconnect, next) = h.connect().await;
    assert_ne!(account, next);
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
async fn c6_row_13a_linked_401_newer_token_reconnects_same_account_without_replay() {
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
    assert_eq!(next, id);
    assert_eq!(
        h.fake.state.requests().last().unwrap().headers["authorization"],
        "Bearer synthetic-new-access"
    );
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
async fn c6_row_13_owned_transient_refresh_relays_original_401_without_cooling() {
    let h = Harness::new_with_owned(2, true).await;
    h.token_fake.state.push(ScriptedResponse::json(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"synthetic transient"}"#,
    ));
    let body = r#"{"error":{"type":"authentication_error","message":"synthetic auth failure"}}"#;
    h.fake
        .state
        .push_handshake(StatusCode::UNAUTHORIZED, body, HeaderMap::new());
    let (status, error) = h.error().await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error, serde_json::from_str::<Value>(body).unwrap());
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_eq!(h.token_fake.state.requests().len(), 1);
    let account = h.store.load(&h.ids[0]).unwrap();
    assert_eq!(account.health, Health::Ready);
    assert_eq!(account.auth_failures, 0);
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
async fn c6_row_13a_owned_transient_refresh_keeps_health_ready_on_close() {
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
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
    assert_eq!(h.store.load(&account).unwrap().auth_failures, 0);
    assert_eq!(h.token_fake.state.requests().len(), 1);
    let (mut reconnect, next) = h.connect().await;
    assert_eq!(account, next);
    reconnect.close(None).await.unwrap();
}
