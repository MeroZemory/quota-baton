#![cfg(all(feature = "testkit", debug_assertions))]
//! Authenticated native-file takeover through the real admin and D17 refresh path.
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::{
    accounts::{AccountStore, Provider, Source},
    admin,
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

const ADMIN: &str = "synthetic-r6-admin";
const ACCESS: &str = "synthetic-r6-source-access";
const REFRESH: &str = "synthetic-r6-source-refresh";
const NEW_ACCESS: &str = "synthetic-r6-rotated-access";
const NEW_REFRESH: &str = "synthetic-r6-rotated-refresh";

struct Harness {
    servers: RunningServers,
    state: Arc<RelayState>,
    store: Arc<AccountStore>,
    fake: ScriptedUpstream,
    client: reqwest::Client,
    source: PathBuf,
    root: tempfile::TempDir,
}
impl Harness {
    async fn new() -> Self {
        Self::with_token_endpoint(None).await
    }
    async fn with_token_endpoint(token_endpoint: Option<String>) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r6-admin-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        let source = root.path().join("external");
        fs::create_dir(&source).unwrap();
        let store = Arc::new(AccountStore::acquire(&root.path().join("home")).unwrap());
        let fake = ScriptedUpstream::start().await.unwrap();
        let token_endpoint = token_endpoint.unwrap_or_else(|| format!("{}/token", fake.url));
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.anthropic_base = fake.url.clone();
        config.upstream.codex_base = fake.url.clone();
        config.upstream.claude_usage_base = fake.url.clone();
        config.upstream.codex_usage_base = fake.url.clone();
        let obs = Arc::new(Observability::new(store.home(), None).unwrap());
        let state = Arc::new(
            RelayState::new(store.clone(), &config, obs, Arc::new(DebugPins::default()))
                .unwrap()
                .with_token_endpoint(Provider::Codex, token_endpoint.clone())
                .with_token_endpoint(Provider::Claude, token_endpoint)
                .with_takeover_test_source(&source, format!("{}/profile", fake.url))
                .unwrap(),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-client".into(),
                admin_token: ADMIN.into(),
            },
            relay::router(state.clone()),
            admin::runtime_routes(state.clone(), 0),
        )
        .await
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        Self {
            servers,
            state,
            store,
            fake,
            client,
            source,
            root,
        }
    }
    fn write(&self, provider: Provider) -> PathBuf {
        let expiry = chrono::Utc::now().timestamp() + 3600;
        let value = match provider {
            Provider::Codex => {
                let claims = json!({"email":"native@example.invalid","exp":expiry,
                    "https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-workspace","chatgpt_user_id":"synthetic-user","chatgpt_plan_type":"free"}});
                let token = format!(
                    "e30.{}.synthetic",
                    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
                );
                json!({"auth_mode":"chatgpt","tokens":{"access_token":ACCESS,"refresh_token":REFRESH,"id_token":token,"account_id":"synthetic-workspace"},"last_refresh":"지역화된 알 수 없는 시각"})
            }
            Provider::Claude => {
                json!({"claudeAiOauth":{"accessToken":ACCESS,"refreshToken":REFRESH,"expiresAt":expiry*1000,"scopes":["user:profile","user:inference"],"subscriptionType":"pro"}})
            }
        };
        let name = if provider == Provider::Codex {
            "auth.json"
        } else {
            ".credentials.json"
        };
        let path = self.source.join(name);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(300))
            .unwrap();
        path
    }
    fn profile(&self) {
        self.fake.state.push(ScriptedResponse::json(
            StatusCode::OK,
            json!({"account":{"uuid":"synthetic-user","email":"native@example.invalid"},
            "organization":{"uuid":"synthetic-org","rate_limit_tier":"default_claude_pro"}})
            .to_string(),
        ));
    }
    fn rotation(&self) {
        self.fake.state.push(ScriptedResponse::json(
            StatusCode::OK,
            json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":3600})
                .to_string(),
        ));
    }
    async fn call(&self, value: &Value, expected: StatusCode) -> Value {
        let response = self
            .client
            .post(format!(
                "http://{}/admin/v1/accounts/takeover",
                self.servers.admin_addr
            ))
            .bearer_auth(ADMIN)
            .json(value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let text = response.text().await.unwrap();
        for secret in [
            ACCESS,
            REFRESH,
            NEW_ACCESS,
            NEW_REFRESH,
            "access_token",
            "refresh_token",
            "secret_dpapi",
        ] {
            assert!(
                !text.contains(secret),
                "admin response must omit credentials"
            );
        }
        serde_json::from_str(&text).unwrap()
    }
    async fn stop(self) {
        self.state.shutdown_refreshes().await.unwrap();
        self.servers.shutdown();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn r6_admin_native_both_providers_retire_refresh_once_and_replay_without_source() {
    for provider in [Provider::Codex, Provider::Claude] {
        let h = Harness::new().await;
        let source = h.write(provider);
        let bytes = fs::read(&source).unwrap();
        if provider == Provider::Claude {
            h.profile();
        }
        let inspect = h
            .call(
                &json!({"provider":provider,"path":h.source,"phase":"inspect"}),
                StatusCode::OK,
            )
            .await;
        assert_eq!(inspect["email"], "native@example.invalid");
        assert_eq!(inspect["owner_alive"], false);
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert!(h.store.list().unwrap().is_empty());
        assert!(
            !h.store
                .home()
                .join("accounts")
                .read_dir()
                .unwrap()
                .any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("takeover"))
        );
        if provider == Provider::Claude {
            h.profile();
        }
        h.rotation();
        let request = json!({"provider":provider,"path":h.source,"phase":"commit","inspect_digest":inspect["inspect_digest"]});
        let result = h.call(&request, StatusCode::OK).await;
        assert_eq!(result["refresh"]["ok"], true);
        assert!(!source.exists());
        assert_eq!(
            fs::read(result["renamed_to"].as_str().unwrap()).unwrap(),
            bytes
        );
        let account = h.store.load(result["id"].as_str().unwrap()).unwrap();
        assert!(matches!(account.source, Source::Takeover { .. }));
        assert_eq!(h.store.tokens(&account).unwrap().access_token, NEW_ACCESS);
        assert_eq!(h.call(&request, StatusCode::OK).await, result);
        let posts = h
            .fake
            .state
            .requests()
            .into_iter()
            .filter(|r| r.method == reqwest::Method::POST)
            .collect::<Vec<_>>();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].path_and_query.ends_with("/token"));
        h.stop().await;
    }
}

#[tokio::test]
async fn r6_admin_guards_auth_recent_mtime_digest_api_key_and_outside_sandbox_source() {
    let h = Harness::new().await;
    let source = h.write(Provider::Codex);
    let url = format!("http://{}/admin/v1/accounts/takeover", h.servers.admin_addr);
    let unauth = h
        .client
        .post(&url)
        .json(&json!({"provider":"codex","path":h.source,"phase":"inspect"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
    let inspect = h
        .call(
            &json!({"provider":"codex","path":h.source,"phase":"inspect"}),
            StatusCode::OK,
        )
        .await;
    let request = json!({"provider":"codex","path":h.source,"phase":"commit","inspect_digest":inspect["inspect_digest"]});
    fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();
    h.call(&request, StatusCode::CONFLICT).await;
    // Old-enough but changed bytes still require a new warning/inspection.
    let mut changed: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
    changed["last_refresh"] = json!("another unknown localized timestamp");
    fs::write(&source, serde_json::to_vec(&changed).unwrap()).unwrap();
    fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(300))
        .unwrap();
    h.call(&request, StatusCode::CONFLICT).await;
    let outside = h.root.path().join("outside-allowed-source");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("auth.json"), fs::read(&source).unwrap()).unwrap();
    h.call(
        &json!({"provider":"codex","path":outside,"phase":"inspect"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    fs::write(
        &source,
        b"{\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"synthetic-key\"}",
    )
    .unwrap();
    h.call(
        &json!({"provider":"codex","path":h.source,"phase":"inspect"}),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert!(h.store.list().unwrap().is_empty());
    assert!(h.fake.state.requests().is_empty());
    h.stop().await;
}

#[tokio::test]
async fn r6_admin_cancelled_commit_after_token_post_persists_once_and_replays_completion() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let posts = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route("/token", axum::routing::post({
        let entered = entered.clone(); let release = release.clone(); let posts = posts.clone();
        move || {
            let entered = entered.clone(); let release = release.clone(); let posts = posts.clone();
            async move {
                posts.fetch_add(1, Ordering::SeqCst); entered.notify_one();
                release.notified().await;
                axum::Json(json!({"access_token":NEW_ACCESS,"refresh_token":NEW_REFRESH,"expires_in":3600}))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fake = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let h = Harness::with_token_endpoint(Some(format!("http://{address}/token"))).await;
    let source = h.write(Provider::Codex);
    let inspect = h
        .call(
            &json!({"provider":"codex","path":h.source,"phase":"inspect"}),
            StatusCode::OK,
        )
        .await;
    let request = json!({"provider":"codex","path":h.source,"phase":"commit","inspect_digest":inspect["inspect_digest"]});
    let client = h.client.clone();
    let body = request.clone();
    let url = format!("http://{}/admin/v1/accounts/takeover", h.servers.admin_addr);
    let caller =
        tokio::spawn(async move { client.post(url).bearer_auth(ADMIN).json(&body).send().await });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(
        !source.exists(),
        "source must be retired before token presentation"
    );
    caller.abort();
    let _ = caller.await;
    let state: Value = h
        .client
        .get(format!("http://{}/admin/v1/state", h.servers.admin_addr))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let account = &state["accounts"][0];
    assert_eq!(account["takeover_pending"], true);
    assert_eq!(account["pool_eligible"], false);
    assert_eq!(account["refresh_attention"], true);
    assert!(
        !h.state
            .accounts
            .eligible(account["id"].as_str().unwrap(), None)
            .await
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let accounts = h.store.list().unwrap();
            if accounts.first().is_some_and(|a| {
                h.store
                    .tokens(a)
                    .is_ok_and(|tokens| tokens.access_token == NEW_ACCESS)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let result = h.call(&request, StatusCode::OK).await;
    assert_eq!(result["refresh"]["ok"], true);
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    h.stop().await;
    fake.abort();
}
