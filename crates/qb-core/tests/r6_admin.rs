#![cfg(all(feature = "testkit", debug_assertions))]
//! Authenticated native-file takeover through the real admin and D17 refresh path.
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::FutureExt;
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
async fn r6_admin_inspect_then_explicit_replace_keeps_id_and_refreshes_once() {
    for provider in [Provider::Codex, Provider::Claude] {
        let h = Harness::new().await;
        h.write(provider);
        if provider == Provider::Claude {
            h.profile();
        }
        let initial = h
            .call(
                &json!({"provider":provider,"path":h.source,"phase":"inspect"}),
                StatusCode::OK,
            )
            .await;
        if provider == Provider::Claude {
            h.profile();
        }
        h.rotation();
        let first = h.call(&json!({"provider":provider,"path":h.source,"phase":"commit","inspect_digest":initial["inspect_digest"]}), StatusCode::OK).await;
        let id = first["id"].as_str().unwrap();
        let before = h.store.load(id).unwrap();

        // A genuinely new source object, in a separate home so its retirement
        // destination cannot collide with the first completed operation.
        let path = h.write(provider);
        let mut native: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let expiry = (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp();
        if provider == Provider::Claude {
            native["claudeAiOauth"]["expiresAt"] = json!(expiry * 1000);
        } else {
            native["last_refresh"] = json!(chrono::Utc::now().to_rfc3339());
        }
        fs::write(&path, serde_json::to_vec(&native).unwrap()).unwrap();
        let replacement = h.source.join("replacement");
        fs::create_dir(&replacement).unwrap();
        let path2 = replacement.join(path.file_name().unwrap());
        fs::rename(path, &path2).unwrap();
        fs::File::options()
            .write(true)
            .open(&path2)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(300))
            .unwrap();
        if provider == Provider::Claude {
            h.profile();
        }
        // The UI discovers the existing identity before the user can consent.
        let inspect = h
            .call(
                &json!({"provider":provider,"path":replacement,"phase":"inspect","replace":false}),
                StatusCode::OK,
            )
            .await;
        assert_eq!(inspect["existing_id"], id);
        let mut commit = json!({"provider":provider,"path":replacement,"phase":"commit","inspect_digest":inspect["inspect_digest"],"replace":false});
        if provider == Provider::Claude {
            h.profile();
        }
        h.call(&commit, StatusCode::CONFLICT).await;
        assert!(path2.exists());
        assert_eq!(h.store.load(id).unwrap().generation, before.generation);
        if provider == Provider::Claude {
            h.profile();
        }
        h.rotation();
        commit["replace"] = json!(true);
        let result = h.call(&commit, StatusCode::OK).await;
        assert_eq!(result["id"], id);
        assert_eq!(result["refresh"]["ok"], true);
        assert!(!path2.exists());
        assert_eq!(h.store.list().unwrap().len(), 1);
        assert!(h.store.load(id).unwrap().generation > before.generation);
        assert_eq!(h.call(&commit, StatusCode::OK).await, result);
        let generation = h.store.load(id).unwrap().generation;
        commit["replace"] = json!(false);
        h.call(&commit, StatusCode::CONFLICT).await;
        assert_eq!(h.store.load(id).unwrap().generation, generation);
        assert_eq!(
            h.fake
                .state
                .requests()
                .iter()
                .filter(|r| r.method == reqwest::Method::POST)
                .count(),
            2
        );
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

// Only the new R15 fixtures use this close seam; older fixtures stay unchanged.
async fn r15_join_servers(mut servers: RunningServers, errors: &mut Vec<String>) {
    servers.shutdown();
    for (name, task) in [
        ("data", &mut servers.data_task),
        ("admin", &mut servers.admin_task),
    ] {
        match tokio::time::timeout(Duration::from_secs(5), &mut *task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(result) => errors.push(format!("{name} server join: {result:?}")),
            Err(_) => {
                errors.push(format!("{name} server shutdown deadline"));
                task.abort();
                // Join cancellation too: never detach a timed-out server task.
                if let Err(error) = task.await
                    && !error.is_cancelled()
                {
                    errors.push(format!("{name} server abort join: {error}"));
                }
            }
        }
    }
}

impl Harness {
    async fn r15_close(self, secondary: Option<RunningServers>) -> Vec<String> {
        let mut errors = Vec::new();
        let refresh = std::panic::AssertUnwindSafe(self.state.shutdown_refreshes())
            .catch_unwind()
            .await;
        match refresh {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(format!("refresh shutdown: {error:?}")),
            Err(_) => errors.push("refresh shutdown panicked".into()),
        }
        // Signal every listener before joining either server set.
        self.servers.shutdown();
        if let Some(servers) = secondary.as_ref() {
            servers.shutdown();
        }
        let Self {
            servers,
            state,
            store,
            fake,
            client,
            source,
            root,
        } = self;
        drop(client);
        if let Some(servers) = secondary {
            r15_join_servers(servers, &mut errors).await;
        }
        r15_join_servers(servers, &mut errors).await;
        drop(fake); // ScriptedUpstream exposes abort-on-drop only, no join API.
        drop(source);
        // Outside Tokio, Observability::drop joins its worker synchronously.
        // Release every fixture state/store reference before explicit temp close.
        let close = std::thread::spawn(move || {
            drop(state);
            drop(store);
            root.close()
        });
        match close.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(format!("temporary home close: {error}")),
            Err(_) => errors.push("fixture close thread panicked".into()),
        }
        errors
    }
}

async fn r15_finish(h: Harness, secondary: Option<RunningServers>, body: std::thread::Result<()>) {
    let errors = h.r15_close(secondary).await;
    if let Err(panic) = body {
        if !errors.is_empty() {
            eprintln!("R15 cleanup also failed: {errors:?}");
        }
        std::panic::resume_unwind(panic);
    }
    assert!(errors.is_empty(), "R15 cleanup failed: {errors:?}");
}

// Separate synthetic account import: no takeover/profile/upstream request.
fn r15_control_account(h: &Harness) -> String {
    let dir = h.root.path().join("r15-control-cpa");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("synthetic.json"), serde_json::to_vec(&json!({
        "type":"codex","email":"control@example.invalid","account_id":"synthetic-control-origin",
        "access_token":"synthetic-control-access","refresh_token":"synthetic-control-refresh",
        "expired":"2099-01-01T00:00:00Z","last_refresh":"2026-10-24T00:00:00Z"
    })).unwrap()).unwrap();
    let rows = qb_core::accounts::import::run(
        &h.store,
        &dir,
        &["codex:control@example.invalid".into()],
        false,
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    rows[0].id.clone().unwrap()
}

#[tokio::test]
async fn r15_lifecycle_real_protected_body_preserves_domain_bindings_and_release_order() {
    for route in ["drain", "shutdown"] {
        let h = Harness::new().await;
        let body = std::panic::AssertUnwindSafe(async {
            let id = r15_control_account(&h);
            // Restore through the existing validated recovery API, not a new proof
            // constructor or a persisted schema change. This is no routing proof.
            let at = chrono::Utc::now();
            h.state
                .bindings
                .import(json!({"schema":1,"bindings":[
                    {"key":"codex:synthetic-sparse","account":id,"last_used":at},
                    {"key":"codex:synthetic-observed","account":id,"last_used":at,
                     "accepted":true,"first_seen_at":at-chrono::Duration::hours(1),"bound_since":at,
                     "client_kind":"codex_cli"}
                ]}))
                .unwrap();
            let exported = h.state.bindings.export().unwrap();
            let url = format!("http://{}/admin/v1/lifecycle/{route}", h.servers.admin_addr);
            tokio::time::timeout(Duration::from_secs(12), async {
                let denied = h
                    .client
                    .post(&url)
                    .json(&json!({"drain_timeout_secs":1}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
                denied.bytes().await.unwrap();
                assert!(!h.state.is_draining());
                let request = h
                    .client
                    .post(&url)
                    .bearer_auth(ADMIN)
                    .json(&json!({"drain_timeout_secs":1}))
                    .send();
                let owner = async {
                    // Mirror the production owner: freeze -> stop listeners ->
                    // actual lease release. Never signal success before drain-ready.
                    h.state.wait_for_drain().await;
                    h.servers.shutdown();
                    h.servers.wait_for_listener_close().await;
                    h.state.release_lease_after_drain();
                };
                let (response, ()) = tokio::join!(request, owner);
                let response = response.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["content-type"], "application/json");
                let body = response.bytes().await.unwrap();
                let decoded: qb_proto::admin_control::LifecycleReply<Value> =
                    serde_json::from_slice(&body).unwrap();
                assert!(decoded.lease_released);
                assert_eq!(decoded.bindings.as_array().unwrap().len(), 2);
                assert_eq!(decoded.bindings, exported["bindings"]);
                let expected = json!({"bindings":exported["bindings"],"lease_released":true});
                assert!(
                    serde_json::to_vec(&expected).unwrap().as_slice() == body.as_ref(),
                    "lifecycle projected fields/order changed"
                );
                assert!(
                    serde_json::to_vec(&decoded).unwrap().as_slice() == body.as_ref(),
                    "same protected lifecycle body did not roundtrip"
                );
                let fields: Vec<_> = serde_json::from_slice::<Value>(&body)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect();
                assert_eq!(fields, ["bindings", "lease_released"]);
                // The real account-store lease, not merely a wire boolean, was freed.
                let successor = AccountStore::acquire(h.store.home()).unwrap();
                drop(successor);
                assert!(h.fake.state.requests().is_empty());
            })
            .await
            .expect("synthetic lifecycle contract deadline");
        })
        .catch_unwind()
        .await;
        r15_finish(h, None, body).await;
    }
}

#[tokio::test]
async fn r15_debug_pin_real_protected_post_delete_bodies_keep_exact_ack() {
    let h = Harness::new().await;
    let mut secondary = None;
    let body = std::panic::AssertUnwindSafe(async {
        let id = r15_control_account(&h);
        let pins = Arc::new(DebugPins::default());
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        secondary = Some(
            http::spawn_with_admin(
                &config,
                LocalSecrets {
                    client_key: "synthetic-client".into(),
                    admin_token: ADMIN.into(),
                },
                qb_core::debug_pin::routes(h.store.clone(), pins.clone()),
            )
            .await
            .unwrap(),
        );
        let servers = secondary.as_ref().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            let url = format!("http://{}/admin/v1/debug/pin", servers.admin_addr);
            for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
                let mut denied = h.client.request(method.clone(), &url);
                if method == reqwest::Method::POST {
                    denied = denied.json(&json!({"account_id":id}));
                }
                let response = denied.send().await.unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                response.bytes().await.unwrap();
                let mut request = h.client.request(method.clone(), &url).bearer_auth(ADMIN);
                if method == reqwest::Method::POST {
                    request = request.json(&json!({"account_id":id}));
                }
                let response = request.send().await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["content-type"], "application/json");
                let body = response.bytes().await.unwrap();
                let decoded: qb_proto::admin_control::DebugPinReply =
                    serde_json::from_slice(&body).unwrap();
                assert!(decoded.ok);
                assert!(
                    body.as_ref() == br#"{"ok":true}"#,
                    "pin acknowledgement shape changed"
                );
                assert!(
                    serde_json::to_vec(&decoded).unwrap().as_slice() == body.as_ref(),
                    "same protected pin body did not roundtrip"
                );
                assert_eq!(
                    pins.get(Provider::Codex, None),
                    (method == reqwest::Method::POST).then(|| id.clone())
                );
            }
            assert!(h.fake.state.requests().is_empty());
        })
        .await
        .expect("synthetic debug pin contract deadline");
    })
    .catch_unwind()
    .await;
    r15_finish(h, secondary, body).await;
}
