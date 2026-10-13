#![cfg(all(feature = "testkit", debug_assertions))]

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use qb_core::{
    accounts::{AccountStore, Health, Provider, Source, StoreError},
    admin,
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    oauth::{LoginService, LoginStart, LoginState, LoginStatus, LoginTestConfig},
    observability::Observability,
    relay::{self, RelayState},
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::{Value, json};
use std::{fs, net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration};

const ADMIN: &str = "synthetic-m1b-admin-key";
const CLIENT: &str = "synthetic-m1b-client-key";
const CODE: &str = "synthetic-m1b-admin-code";
const ACCESS: &str = "synthetic-m1b-admin-access";
const REFRESH: &str = "synthetic-m1b-admin-refresh";
const NEW_ACCESS: &str = "synthetic-m1b-admin-access-rotated";
const NEW_REFRESH: &str = "synthetic-m1b-admin-refresh-rotated";
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    servers: RunningServers,
    service: LoginService,
    store: Arc<AccountStore>,
    state: Arc<RelayState>,
    obs: Arc<Observability>,
    client: reqwest::Client,
    oauth: ScriptedUpstream,
    refresh: ScriptedUpstream,
    data: ScriptedUpstream,
    _usage: ScriptedUpstream,
    home: PathBuf,
    _root: tempfile::TempDir,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Harness {
    async fn new(callback_port: u16) -> Self {
        let serial = SERIAL.lock().await;
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m1b-admin-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let store = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match AccountStore::acquire(&home) {
                    Ok(store) => break Arc::new(store),
                    Err(StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(_) => panic!("synthetic admin fixture could not acquire account store"),
                }
            }
        })
        .await
        .unwrap();
        let oauth = ScriptedUpstream::start().await.unwrap();
        let refresh = ScriptedUpstream::start().await.unwrap();
        let data = ScriptedUpstream::start().await.unwrap();
        let usage = ScriptedUpstream::start().await.unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.anthropic_base = data.url.clone();
        config.upstream.codex_base = data.url.clone();
        config.upstream.claude_usage_base = usage.url.clone();
        config.upstream.codex_usage_base = usage.url.clone();
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap()
            .with_token_endpoint(Provider::Claude, format!("{}/token", refresh.url))
            .with_token_endpoint(Provider::Codex, format!("{}/token", refresh.url)),
        );
        // Schedulers are intentionally not started: every request is driven by this test.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("quota-baton/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let service = LoginService::with_test_config(
            store.clone(),
            client.clone(),
            obs.clone(),
            LoginTestConfig {
                authorize_url: format!("{}/authorize", oauth.url),
                token_url: format!("{}/token", oauth.url),
                profile_url: format!("{}/profile", oauth.url),
                callback_port,
            },
        )
        .unwrap();
        let routes =
            admin::login_routes(service.clone()).merge(admin::runtime_routes(state.clone(), 0));
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: CLIENT.into(),
                admin_token: ADMIN.into(),
            },
            relay::router(state.clone()),
            routes,
        )
        .await
        .unwrap();
        Self {
            servers,
            service,
            store,
            state,
            obs,
            client,
            oauth,
            refresh,
            data,
            _usage: usage,
            home,
            _root: root,
            _serial: serial,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}/admin/v1{path}", self.servers.admin_addr)
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.url(path))
            .bearer_auth(ADMIN)
    }
    async fn json(&self, response: reqwest::Response, expected: StatusCode) -> Value {
        assert_eq!(response.status(), expected);
        let text = response.text().await.unwrap();
        secret_free(&text);
        serde_json::from_str(&text).unwrap()
    }
    async fn start(&self, provider: Provider) -> LoginStart {
        serde_json::from_value(
            self.json(
                self.request(reqwest::Method::POST, "/accounts/login")
                    .json(&json!({"provider":provider}))
                    .send()
                    .await
                    .unwrap(),
                StatusCode::OK,
            )
            .await,
        )
        .unwrap()
    }
    async fn status(&self, id: &str) -> LoginStatus {
        serde_json::from_value(
            self.json(
                self.request(reqwest::Method::GET, &format!("/accounts/login/{id}"))
                    .send()
                    .await
                    .unwrap(),
                StatusCode::OK,
            )
            .await,
        )
        .unwrap()
    }
    async fn terminal(&self, id: &str) -> LoginStatus {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = self.status(id).await;
                if status.status != LoginState::Pending {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn cancel(&self, id: &str) -> LoginStatus {
        serde_json::from_value(
            self.json(
                self.request(
                    reqwest::Method::POST,
                    &format!("/accounts/login/{id}/cancel"),
                )
                .send()
                .await
                .unwrap(),
                StatusCode::OK,
            )
            .await,
        )
        .unwrap()
    }
    fn replies(&self, provider: Provider) {
        let claims = json!({"email":"admin@example.invalid", "https://api.openai.com/auth": {
            "chatgpt_account_id":"synthetic-admin-account", "chatgpt_user_id":"synthetic-admin-user", "chatgpt_plan_type":"plus"
        }});
        let id_token = format!(
            "e30.{}.synthetic-admin-signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        self.oauth.state.push(ScriptedResponse::json(
            StatusCode::OK,
            serde_json::to_vec(&json!({
                "access_token":ACCESS,"refresh_token":REFRESH,"id_token":id_token,"expires_in":3600
            }))
            .unwrap(),
        ));
        if provider == Provider::Claude {
            self.oauth.state.push(ScriptedResponse::json(
                StatusCode::OK,
                serde_json::to_vec(&json!({
                    "account":{"uuid":"synthetic-admin-uuid","email":"admin@example.invalid"},
                    "organization":{"uuid":"synthetic-admin-org"}
                }))
                .unwrap(),
            ));
        }
    }
    async fn callback(&self, start: &LoginStart, mismatch: bool) {
        let auth = reqwest::Url::parse(&start.auth_url).unwrap();
        let state = auth
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let redirect = auth
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .unwrap()
            .1
            .into_owned();
        let mut callback = reqwest::Url::parse(&redirect).unwrap();
        callback.set_host(Some("127.0.0.1")).unwrap();
        callback.query_pairs_mut().extend_pairs([
            ("code", CODE),
            ("state", if mismatch { "mismatched-state" } else { &state }),
        ]);
        // Never request the authorize URL: only the injected loopback callback.
        let response = self.client.get(callback).send().await.unwrap();
        assert_eq!(
            response.status(),
            if mismatch {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            }
        );
        secret_free(&response.text().await.unwrap());
    }
    async fn closed(&self, start: &LoginStart) {
        let auth = reqwest::Url::parse(&start.auth_url).unwrap();
        let redirect = auth
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .unwrap()
            .1
            .into_owned();
        let port = reqwest::Url::parse(&redirect).unwrap().port().unwrap();
        for address in [
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        ] {
            assert!(
                tokio::net::TcpStream::connect((address, port))
                    .await
                    .is_err()
            );
        }
    }
    fn ledger(&self) -> Vec<Value> {
        self.obs.flush().unwrap();
        let mut rows = Vec::new();
        for entry in fs::read_dir(self.home.join("ledger")).unwrap() {
            let path = entry.unwrap().path();
            if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                let text = fs::read_to_string(path).unwrap();
                secret_free(&text);
                rows.extend(
                    text.lines()
                        .map(|line| serde_json::from_str::<Value>(line).unwrap()),
                );
            }
        }
        rows
    }
}

fn secret_free(text: &str) {
    for secret in [
        ADMIN,
        CLIENT,
        CODE,
        ACCESS,
        REFRESH,
        NEW_ACCESS,
        NEW_REFRESH,
        "synthetic-admin-signature",
        "?code=",
        "code_verifier",
    ] {
        assert!(
            !text.contains(secret),
            "public output contained a synthetic secret"
        );
    }
}

#[tokio::test]
async fn protected_admin_login_routes_reject_bad_auth_host_origin_and_invalid_requests() {
    let h = Harness::new(0).await;
    for method in [reqwest::Method::GET, reqwest::Method::POST] {
        let path = if method == reqwest::Method::GET {
            "/accounts/login/missing"
        } else {
            "/accounts/login"
        };
        let response = h
            .client
            .request(method, h.url(path))
            .json(&json!({"provider":"codex"}))
            .send()
            .await
            .unwrap();
        h.json(response, StatusCode::UNAUTHORIZED).await;
    }
    for path in ["/accounts/login", "/accounts/login/missing/cancel"] {
        let response = h
            .client
            .post(h.url(path))
            .bearer_auth(CLIENT)
            .json(&json!({"provider":"codex"}))
            .send()
            .await
            .unwrap();
        h.json(response, StatusCode::UNAUTHORIZED).await;
    }
    h.json(
        h.request(reqwest::Method::POST, "/accounts/login")
            .header("host", "evil.example.invalid")
            .json(&json!({"provider":"codex"}))
            .send()
            .await
            .unwrap(),
        StatusCode::MISDIRECTED_REQUEST,
    )
    .await;
    h.json(
        h.request(reqwest::Method::POST, "/accounts/login")
            .header("origin", "http://127.0.0.1")
            .json(&json!({"provider":"codex"}))
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
    )
    .await;
    for body in [
        json!({"provider":"unknown"}),
        json!({}),
        json!({"provider":"codex","replace_linked":"yes"}),
    ] {
        h.json(
            h.request(reqwest::Method::POST, "/accounts/login")
                .json(&body)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    for (method, path) in [
        (reqwest::Method::GET, "/accounts/login/missing"),
        (reqwest::Method::POST, "/accounts/login/missing/cancel"),
    ] {
        h.json(
            h.request(method, path).send().await.unwrap(),
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    assert!(h.oauth.state.requests().is_empty());
    assert!(h.store.list().unwrap().is_empty());
    h.service.cancel_all().await;
}

#[tokio::test]
async fn authenticated_login_completes_for_both_providers_and_releases_callbacks() {
    let h = Harness::new(0).await;
    for provider in [Provider::Claude, Provider::Codex] {
        h.replies(provider);
        let start = h.start(provider).await;
        assert_eq!(h.status(&start.login_id).await.status, LoginState::Pending);
        h.callback(&start, false).await;
        let status = h.terminal(&start.login_id).await;
        assert_eq!(status.status, LoginState::Completed);
        assert!(status.error.is_none());
        let account = h.store.load(status.account_id.as_deref().unwrap()).unwrap();
        assert_eq!(account.provider, provider);
        assert_eq!(account.source, Source::Oauth);
        assert_eq!(account.health, Health::Ready);
        h.closed(&start).await;
        let rows = h.ledger();
        for outcome in ["started", "completed"] {
            assert_eq!(
                rows.iter()
                    .filter(|row| row["event"] == "login"
                        && row["login_id"] == start.login_id
                        && row["outcome"] == outcome)
                    .count(),
                1
            );
        }
    }
    h.service.cancel_all().await;
}

#[tokio::test]
async fn authenticated_cancel_and_mismatch_wait_until_expiry_are_secret_free() {
    let h = Harness::new(0).await;
    let cancelled = h.start(Provider::Codex).await;
    assert_eq!(
        h.cancel(&cancelled.login_id).await.status,
        LoginState::Cancelled
    );
    assert_eq!(
        h.cancel(&cancelled.login_id).await.status,
        LoginState::Cancelled
    );
    h.closed(&cancelled).await;
    let mismatch = h.start(Provider::Claude).await;
    h.callback(&mismatch, true).await;
    let status = h.status(&mismatch.login_id).await;
    assert_eq!(status.status, LoginState::Pending);
    assert!(status.error.is_none());
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(600)).await;
    tokio::time::resume();
    assert_eq!(
        h.terminal(&mismatch.login_id).await.status,
        LoginState::Expired
    );
    h.closed(&mismatch).await;
    let expired = h.start(Provider::Codex).await;
    // Advance only after the HTTP start response: no paused-clock network races.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(600)).await;
    tokio::time::resume();
    assert_eq!(
        h.terminal(&expired.login_id).await.status,
        LoginState::Expired
    );
    h.closed(&expired).await;
    assert!(h.store.list().unwrap().is_empty());
    assert!(h.oauth.state.requests().is_empty());
    h.ledger();
    h.service.cancel_all().await;
}

#[tokio::test]
async fn busy_callback_port_returns_safe_admin_conflict_without_fallback() {
    let busy = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = busy.local_addr().unwrap().port();
    let h = Harness::new(port).await;
    let value = h
        .json(
            h.request(reqwest::Method::POST, "/accounts/login")
                .json(&json!({"provider":"codex"}))
                .send()
                .await
                .unwrap(),
            StatusCode::CONFLICT,
        )
        .await;
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains(&port.to_string()));
    assert!(value.get("login_id").is_none());
    assert!(value.get("auth_url").is_none());
    assert!(h.oauth.state.requests().is_empty());
    h.service.cancel_all().await;
}

#[tokio::test]
async fn authenticated_sse_stream_delivers_redacted_login_progress() {
    let h = Harness::new(0).await;
    let response = h
        .request(reqwest::Method::GET, "/events")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let mut stream = response.bytes_stream();
    let mut buffered = String::new();
    let start = h.start(Provider::Codex).await;
    h.cancel(&start.login_id).await;
    let progress = tokio::time::timeout(Duration::from_secs(5), async {
        let mut progress = Vec::new();
        while let Some(chunk) = stream.next().await {
            buffered.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
            while let Some(end) = buffered.find("\n\n") {
                let frame: String = buffered.drain(..end + 2).collect();
                secret_free(&frame);
                let mut event = None;
                let mut data = None;
                for line in frame.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        event = Some(value.trim());
                    }
                    if let Some(value) = line.strip_prefix("data:") {
                        data = Some(value.trim());
                    }
                }
                if event == Some("login_progress") {
                    let value: Value = serde_json::from_str(data.unwrap()).unwrap();
                    assert!(value["login_id"] == start.login_id);
                    assert!(value.get("auth_url").is_none());
                    progress.push(value);
                    if progress.iter().any(|value| value["status"] == "cancelled") {
                        return progress;
                    }
                }
            }
        }
        panic!("admin SSE stream ended before terminal login progress");
    })
    .await
    .expect("admin SSE route must forward login_progress");
    assert!(progress.iter().any(|value| value["status"] == "started"));
    h.closed(&start).await;
    drop(stream);
    h.service.cancel_all().await;
}

#[tokio::test]
async fn completed_admin_login_reaches_owned_refresh_through_public_http_relay() {
    let h = Harness::new(0).await;
    h.replies(Provider::Codex);
    let start = h.start(Provider::Codex).await;
    h.callback(&start, false).await;
    let status = h.terminal(&start.login_id).await;
    assert_eq!(status.status, LoginState::Completed);
    let before = h.store.load(status.account_id.as_deref().unwrap()).unwrap();
    assert_eq!(before.source, Source::Oauth);
    h.refresh.state.push(ScriptedResponse::json(
        StatusCode::OK,
        serde_json::to_vec(&json!({
            "access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":7200
        }))
        .unwrap(),
    ));
    h.data.state.push(ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"message":"authentication failed"}}"#,
    ));
    h.data.state.push(ScriptedResponse::json(
        StatusCode::OK,
        r#"{"id":"synthetic-response","output":[]}"#,
    ));
    let body = json!({"model":"gpt-5","input":[{"role":"user","content":"synthetic admin relay test"}],"stream":false});
    let response = h
        .client
        .post(format!("http://{}/v1/responses", h.servers.data_addr))
        .bearer_auth(CLIENT)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    secret_free(&text);
    assert!(serde_json::from_str::<Value>(&text).unwrap()["id"] == "synthetic-response");
    let requests = h.data.state.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].headers["authorization"] == format!("Bearer {ACCESS}"));
    assert!(requests[1].headers["authorization"] == format!("Bearer {NEW_ACCESS}"));
    for request in &requests {
        assert_eq!(request.path_and_query, "/responses");
        assert!(request.headers["chatgpt-account-id"] == "synthetic-admin-account");
        assert!(serde_json::from_slice::<Value>(&request.body).unwrap() == body);
    }
    let refresh = h.refresh.state.requests();
    assert_eq!(refresh.len(), 1);
    assert_eq!(refresh[0].path_and_query, "/token");
    assert!(refresh[0].headers["user-agent"] == concat!("quota-baton/", env!("CARGO_PKG_VERSION")));
    let form = reqwest::Url::parse(&format!(
        "http://127.0.0.1/?{}",
        std::str::from_utf8(&refresh[0].body).unwrap()
    ))
    .unwrap();
    assert!(
        form.query_pairs()
            .any(|(k, v)| k == "grant_type" && v == "refresh_token")
    );
    assert!(
        form.query_pairs()
            .any(|(k, v)| k == "refresh_token" && v == REFRESH)
    );
    let after = h.store.load(&before.id).unwrap();
    assert_eq!(after.source, Source::Oauth);
    assert_eq!(after.health, Health::Ready);
    assert!(!after.token_meta.refresh_uncertain);
    assert!(after.generation > before.generation);
    assert!(after.token_meta.expires_at > before.token_meta.expires_at);
    let tokens = h.store.tokens(&after).unwrap();
    assert!(tokens.access_token == NEW_ACCESS);
    assert!(tokens.refresh_token == NEW_REFRESH);
    let rows = h.ledger();
    assert!(rows.iter().any(|row| row["event"] == "token_refresh"
        && row["account"] == before.id
        && row["trigger"] == "upstream_401"
        && row["outcome"] == "success"));
    // Assert the runtime used by the actual routes observes the saved account.
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .any(|account| account["id"] == before.id)
    );
    h.closed(&start).await;
    h.service.cancel_all().await;
}
