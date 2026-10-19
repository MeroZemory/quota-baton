#![cfg(all(feature = "testkit", debug_assertions))]

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::{
    accounts::{AccountStore, Health, Identity, ImportMode, Provider, Source, SourceMatch},
    oauth::{LoginError, LoginService, LoginStart, LoginState, LoginStatus, LoginTestConfig},
    observability::Observability,
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::{Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    service: LoginService,
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    fake: ScriptedUpstream,
    root: tempfile::TempDir,
    client: reqwest::Client,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}
impl Harness {
    async fn new() -> Self {
        let serial = SERIAL.lock().await;
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m1b-login-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let store = Arc::new(AccountStore::acquire(&home).unwrap());
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let fake = ScriptedUpstream::start().await.unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("quota-baton/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap();
        let service = LoginService::with_test_config(
            store.clone(),
            client.clone(),
            obs.clone(),
            config(&fake, 0),
        )
        .unwrap();
        Self {
            service,
            store,
            obs,
            fake,
            root,
            client,
            _serial: serial,
        }
    }
    fn replies(&self, provider: Provider) {
        let id_token = format!("e30.{}.synthetic-signature", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
            "email":"login@example.invalid", "https://api.openai.com/auth": {
                "chatgpt_account_id":"synthetic-account", "chatgpt_user_id":"synthetic-user", "chatgpt_plan_type":"plus"
            }
        })).unwrap()));
        self.fake.state.push(ScriptedResponse::json(StatusCode::OK, serde_json::to_vec(&json!({
            "access_token":"synthetic-login-access", "refresh_token":"synthetic-login-refresh", "id_token": id_token, "expires_in":3600
        })).unwrap()));
        if provider == Provider::Claude {
            self.fake.state.push(ScriptedResponse::json(
                StatusCode::OK,
                serde_json::to_vec(&json!({
                    "account":{"uuid":"synthetic-uuid","email":"login@example.invalid"},
                    "organization":{"uuid":"synthetic-org"}
                }))
                .unwrap(),
            ));
        }
    }
    async fn login(&self, provider: Provider, replace: bool, ipv6: bool) -> LoginStatus {
        self.replies(provider);
        let start = self.service.start(provider, replace).await.unwrap();
        callback(&start, false, ipv6).await;
        let status = terminal(&self.service, &start.login_id).await;
        closed(&start).await;
        status
    }
    fn assert_no_secrets(&self, status: &LoginStatus) {
        let serialized = serde_json::to_string(status).unwrap();
        for secret in [
            "synthetic-login-access",
            "synthetic-login-refresh",
            "synthetic-code",
            "synthetic-signature",
        ] {
            assert!(!serialized.contains(secret));
        }
        self.obs.flush().unwrap();
        // Ledger filenames vary by date. Inspect only this synthetic fixture.
        fn scan(path: &std::path::Path) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    scan(&path);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                {
                    let text = fs::read_to_string(path).unwrap();
                    for secret in [
                        "synthetic-login-access",
                        "synthetic-login-refresh",
                        "synthetic-code",
                        "synthetic-signature",
                        "?code=",
                    ] {
                        assert!(!text.contains(secret));
                    }
                }
            }
        }
        scan(self.root.path());
    }
}
fn config(fake: &ScriptedUpstream, port: u16) -> LoginTestConfig {
    LoginTestConfig {
        authorize_url: format!("{}/authorize", fake.url),
        token_url: format!("{}/token", fake.url),
        profile_url: format!("{}/profile", fake.url),
        callback_port: port,
    }
}
fn authorize(start: &LoginStart) -> (reqwest::Url, String) {
    let auth = reqwest::Url::parse(&start.auth_url).unwrap();
    let redirect = auth
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    (auth, redirect)
}
async fn callback(start: &LoginStart, mismatch: bool, ipv6: bool) {
    let (auth, redirect) = authorize(start);
    let state = auth
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let mut url = reqwest::Url::parse(&redirect).unwrap();
    url.query_pairs_mut().extend_pairs([
        ("code", "synthetic-code"),
        ("state", if mismatch { "wrong-state" } else { &state }),
    ]);
    let mut socket = if ipv6 {
        tokio::net::TcpStream::connect((Ipv6Addr::LOCALHOST, url.port().unwrap()))
            .await
            .unwrap()
    } else {
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, url.port().unwrap()))
            .await
            .unwrap()
    };
    let request = format!(
        "GET {}?{} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        url.path(),
        url.query().unwrap()
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).await.unwrap();
    if mismatch {
        assert!(response.starts_with(b"HTTP/1.1 400 Bad Request"));
    }
    assert!(!String::from_utf8_lossy(&response).contains("synthetic-code"));
}
async fn terminal(service: &LoginService, id: &str) -> LoginStatus {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = service.status(id).await.unwrap();
            if status.status != LoginState::Pending {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}
async fn closed(start: &LoginStart) {
    let (_, redirect) = authorize(start);
    let port = reqwest::Url::parse(&redirect).unwrap().port().unwrap();
    let v4 = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let v6 = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port))
        .await
        .unwrap();
    drop((v4, v6));
}

#[tokio::test]
async fn credits_relogin_preserves_subscription_hold_provenance() {
    use qb_core::accounts::HealthReason;
    let h = Harness::new().await;
    let first = h.login(Provider::Codex, false, false).await;
    let id = first.account_id.unwrap();
    let before = h
        .store
        .update_metadata(&id, |account| {
            account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: chrono::Utc::now() + chrono::Duration::hours(1),
                scope: "account".into(),
            };
            account.quota_credit_releasable = true;
        })
        .unwrap();
    let again = h.login(Provider::Codex, false, false).await;
    assert_eq!(again.status, LoginState::Completed);
    let after = h.store.load(&id).unwrap();
    assert_eq!(after.health, before.health);
    assert!(after.quota_credit_releasable);
}

#[tokio::test]
async fn both_providers_exchange_identity_persistence_and_pkce() {
    let h = Harness::new().await;
    for provider in [Provider::Claude, Provider::Codex] {
        let start_index = h.fake.state.requests().len();
        h.replies(provider);
        let start = h.service.start(provider, false).await.unwrap();
        let (auth, redirect) = authorize(&start);
        assert!(
            auth.query_pairs()
                .any(|(k, v)| k == "code_challenge_method" && v == "S256")
        );
        callback(&start, false, provider == Provider::Codex).await;
        let status = terminal(&h.service, &start.login_id).await;
        assert_eq!(status.status, LoginState::Completed);
        let account = h.store.load(status.account_id.as_ref().unwrap()).unwrap();
        assert_eq!(account.schema, 1);
        assert_eq!(account.source, Source::Oauth);
        assert_eq!(account.health, Health::Ready);
        assert!(account.token_meta.expires_at.unwrap() > chrono::Utc::now());
        if provider == Provider::Codex {
            assert!(
                matches!(account.identity, Identity::Codex { user_id: Some(ref id), .. } if id == "synthetic-user")
            );
        }
        let requests = h.fake.state.requests();
        let token = &requests[start_index];
        assert_eq!(token.path_and_query, "/token");
        let fields: Value = if provider == Provider::Claude {
            assert!(
                token.headers["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("application/json")
            );
            serde_json::from_slice(&token.body).unwrap()
        } else {
            assert!(
                token.headers["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("application/x-www-form-urlencoded")
            );
            let url = reqwest::Url::parse(&format!(
                "http://127.0.0.1/?{}",
                std::str::from_utf8(&token.body).unwrap()
            ))
            .unwrap();
            Value::Object(
                url.query_pairs()
                    .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                    .collect(),
            )
        };
        assert!(fields["redirect_uri"] == redirect);
        let verifier = fields["code_verifier"].as_str().unwrap();
        assert!(URL_SAFE_NO_PAD.decode(verifier).unwrap().len() >= 32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert!(
            auth.query_pairs()
                .any(|(k, v)| k == "code_challenge" && v == challenge)
        );
        for request in &requests[start_index..] {
            assert!(
                request.headers["user-agent"] == concat!("quota-baton/", env!("CARGO_PKG_VERSION"))
            );
        }
        closed(&start).await;
        h.assert_no_secrets(&status);
    }
}

#[tokio::test]
async fn mismatch_keeps_waiting_and_exchange_failure_is_redacted_and_releases_listeners() {
    let h = Harness::new().await;
    let start = h.service.start(Provider::Claude, false).await.unwrap();
    callback(&start, true, false).await;
    let status = h.service.status(&start.login_id).await.unwrap();
    assert_eq!(status.status, LoginState::Pending);
    assert!(status.error.is_none());
    assert!(h.fake.state.requests().is_empty());
    h.replies(Provider::Claude);
    callback(&start, false, false).await;
    let status = terminal(&h.service, &start.login_id).await;
    assert_eq!(status.status, LoginState::Completed);
    closed(&start).await;
    h.assert_no_secrets(&status);
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::BAD_REQUEST,
        r#"{"error":"synthetic-code synthetic-login-access"}"#,
    ));
    let start = h.service.start(Provider::Codex, false).await.unwrap();
    callback(&start, false, false).await;
    let status = terminal(&h.service, &start.login_id).await;
    assert_eq!(status.status, LoginState::Failed);
    closed(&start).await;
    h.assert_no_secrets(&status);
}

#[tokio::test]
async fn cancel_and_ten_minute_expiry_release_both_listeners() {
    let h = Harness::new().await;
    let start = h.service.start(Provider::Claude, false).await.unwrap();
    h.service.cancel(&start.login_id).await.unwrap();
    assert_eq!(
        h.service.status(&start.login_id).await.unwrap().status,
        LoginState::Cancelled
    );
    closed(&start).await;
    tokio::time::pause();
    let start = h.service.start(Provider::Codex, false).await.unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(599)).await;
    assert_eq!(
        h.service.status(&start.login_id).await.unwrap().status,
        LoginState::Pending
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        terminal(&h.service, &start.login_id).await.status,
        LoginState::Expired
    );
    closed(&start).await;
    tokio::time::resume();
}

#[tokio::test]
async fn live_relogin_clears_refresh_attention_and_memory_store_error_only_after_success() {
    use qb_core::{account_runtime::AccountRuntime, accounts::HealthReason};

    let mut h = Harness::new().await;
    let first = h.login(Provider::Codex, false, false).await;
    assert_eq!(first.status, LoginState::Completed);
    let id = first.account_id.unwrap();
    let saved = h
        .store
        .update_metadata(&id, |account| {
            account.label = "synthetic-saved-label".into();
        })
        .unwrap();
    let runtime = AccountRuntime::new(h.store.clone(), h.obs.clone(), 1.0);
    h.service = h.service.clone().with_runtime(runtime.clone());
    runtime.set_refresh_attention(&id, true);
    let mut volatile = saved.clone();
    volatile.health = Health::Cooling {
        reason: HealthReason::StoreError,
        until: chrono::Utc::now() + chrono::Duration::minutes(10),
        scope: "account".into(),
    };
    runtime.emit_account(&volatile);
    assert!(runtime.blocked_in_memory(&id));
    let before = runtime.views().await.unwrap();
    let before = before.iter().find(|view| view["id"] == id).unwrap();
    assert_eq!(before["refresh_attention"], true);
    assert_eq!(before["health"]["reason"], "store_error");

    // A failed exchange must leave the live warning and memory guard intact.
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::BAD_REQUEST,
        r#"{"error":"synthetic-denied"}"#,
    ));
    let start = h.service.start(Provider::Codex, false).await.unwrap();
    callback(&start, false, false).await;
    let failed = terminal(&h.service, &start.login_id).await;
    assert_eq!(failed.status, LoginState::Failed);
    assert!(runtime.blocked_in_memory(&id));
    let failed_view = runtime.views().await.unwrap();
    assert_eq!(
        failed_view.iter().find(|view| view["id"] == id).unwrap()["refresh_attention"],
        true
    );
    closed(&start).await;

    let completed = h.login(Provider::Codex, false, false).await;
    assert_eq!(completed.status, LoginState::Completed);
    assert_eq!(completed.account_id.as_deref(), Some(id.as_str()));
    assert!(!runtime.blocked_in_memory(&id));
    let after = h.store.load(&id).unwrap();
    assert!(after.generation > saved.generation);
    assert_eq!(after.health, Health::Ready);
    assert_eq!(after.label, "synthetic-saved-label");
    let views = runtime.views().await.unwrap();
    let view = views.iter().find(|view| view["id"] == id).unwrap();
    assert_eq!(view["refresh_attention"], false);
    assert_eq!(view["health"]["kind"], "ready");
    assert_eq!(view["label"], "synthetic-saved-label");
    h.assert_no_secrets(&completed);
}

#[tokio::test]
async fn owned_relogin_and_explicit_linked_replacement() {
    let h = Harness::new().await;
    let status = h.login(Provider::Claude, false, false).await;
    let id = status.account_id.unwrap();
    h.store
        .update_metadata(&id, |account| {
            account.health = Health::NeedsRelogin {
                reason: "synthetic".into(),
                since: chrono::Utc::now(),
            }
        })
        .unwrap();
    let status = h.login(Provider::Claude, false, false).await;
    assert_eq!(status.status, LoginState::Completed);
    assert_eq!(h.store.load(&id).unwrap().health, Health::Ready);
    // Source changes belong to the account transaction, not metadata updates.
    let mut account = h.store.load(&id).unwrap();
    let tokens = h.store.tokens(&account).unwrap();
    account.source = Source::Cpa {
        mode: ImportMode::Linked,
        dir: h.root.path().join("synthetic-cpa"),
        r#match: SourceMatch::from(&account.identity),
    };
    h.store
        .save(&account, &tokens, Some(account.generation))
        .unwrap();
    let status = h.login(Provider::Claude, false, false).await;
    assert_eq!(status.status, LoginState::Failed);
    assert!(status.error.as_deref() == Some(LoginError::LinkedConflict.to_string().as_str()));
    assert!(matches!(
        h.store.load(&id).unwrap().source,
        Source::Cpa { .. }
    ));
    let status = h.login(Provider::Claude, true, false).await;
    assert_eq!(status.status, LoginState::Completed);
    assert_eq!(h.store.load(&id).unwrap().source, Source::Oauth);
    h.assert_no_secrets(&status);
}

#[tokio::test]
async fn busy_port_and_unsafe_test_injection_are_refused() {
    let h = Harness::new().await;
    let busy = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = busy.local_addr().unwrap().port();
    let service = LoginService::with_test_config(
        h.store.clone(),
        h.client.clone(),
        h.obs.clone(),
        config(&h.fake, port),
    )
    .unwrap();
    assert!(
        matches!(service.start(Provider::Codex, false).await, Err(LoginError::CallbackPortBusy { port: p }) if p == port)
    );
    for address in [
        "http://192.0.2.1/token",
        "http://localhost/token",
        "https://127.0.0.1/token",
        "http://127.0.0.1:10100/token",
        "http://[::1]:10101/token",
    ] {
        let mut test = config(&h.fake, 0);
        test.token_url = address.into();
        assert!(matches!(
            LoginService::with_test_config(h.store.clone(), h.client.clone(), h.obs.clone(), test),
            Err(LoginError::InvalidTestEndpoint)
        ));
    }
    let mut test = config(&h.fake, 10100);
    assert!(matches!(
        LoginService::with_test_config(
            h.store.clone(),
            h.client.clone(),
            h.obs.clone(),
            test.clone()
        ),
        Err(LoginError::InvalidTestEndpoint)
    ));
    test.callback_port = 10101;
    assert!(matches!(
        LoginService::with_test_config(h.store.clone(), h.client.clone(), h.obs.clone(), test),
        Err(LoginError::InvalidTestEndpoint)
    ));
}

#[tokio::test]
async fn cancel_during_exchange_and_exchange_timeout_are_bounded() {
    let h = Harness::new().await;
    for cancel in [true, false] {
        let count = h.fake.state.requests().len();
        let mut delayed = ScriptedResponse::json(StatusCode::OK, "{}");
        delayed.chunks[0].0 = Duration::from_secs(120);
        h.fake.state.push(delayed);
        let start = h.service.start(Provider::Codex, false).await.unwrap();
        callback(&start, false, false).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while h.fake.state.requests().len() == count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Listeners close before a potentially slow token exchange.
        closed(&start).await;
        if cancel {
            h.service.cancel(&start.login_id).await.unwrap();
            assert_eq!(
                h.service.status(&start.login_id).await.unwrap().status,
                LoginState::Cancelled
            );
        } else {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(31)).await;
            tokio::task::yield_now().await;
            tokio::time::resume();
            let status = terminal(&h.service, &start.login_id).await;
            assert_eq!(status.status, LoginState::Failed);
            h.assert_no_secrets(&status);
        }
    }
    assert!(h.store.list().unwrap().is_empty());
}

#[tokio::test]
async fn dropping_last_service_handle_cancels_callback_task() {
    let h = Harness::new().await;
    let service = LoginService::with_test_config(
        h.store.clone(),
        h.client.clone(),
        h.obs.clone(),
        config(&h.fake, 0),
    )
    .unwrap();
    let start = service.start(Provider::Claude, false).await.unwrap();
    drop(service);
    tokio::task::yield_now().await;
    closed(&start).await;
}
