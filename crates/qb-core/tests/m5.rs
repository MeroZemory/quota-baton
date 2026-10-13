#![cfg(all(feature = "testkit", debug_assertions))]

use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use futures_util::StreamExt;
use qb_core::{
    accounts::{Account, AccountStore, Health, HealthReason, Provider, Source, UserState, import},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    quota::PollError,
    relay::{self, RelayState},
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    state: Arc<RelayState>,
    obs: Arc<Observability>,
    store: Arc<AccountStore>,
    usage: ScriptedUpstream,
    data: ScriptedUpstream,
    token: ScriptedUpstream,
    servers: RunningServers,
    client: reqwest::Client,
    id: String,
    cpa_file: PathBuf,
    _root: tempfile::TempDir,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Harness {
    async fn new(provider: Provider, owned: bool) -> Self {
        let serial = SERIAL.lock().await;
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m5-fixtures");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let cpa = root.path().join("synthetic-cpa");
        fs::create_dir_all(&cpa).unwrap();
        let cpa_file = cpa.join("fixture.json");
        fs::write(
            &cpa_file,
            serde_json::to_vec(&json!({
                "type": provider.as_str(), "email": "m5@example.invalid",
                "account_id": "synthetic-m5", "account_uuid": "synthetic-m5",
                "organization_uuid": "synthetic-org", "access_token": "synthetic-access-old",
                "refresh_token": "synthetic-refresh-old", "expired": "2099-01-01T00:00:00Z",
                "last_refresh": "2026-01-01T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
        let store = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match AccountStore::acquire(&home) {
                    Ok(store) => break Arc::new(store),
                    Err(qb_core::accounts::StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => panic!("synthetic M5 store: {error}"),
                }
            }
        })
        .await
        .unwrap();
        import::run(
            &store,
            &cpa,
            &[format!("{}:m5@example.invalid", provider.as_str())],
            false,
        )
        .unwrap();
        let mut account = store.list().unwrap().remove(0);
        if owned {
            let tokens = store.tokens(&account).unwrap();
            account.source = Source::Oauth;
            account = store
                .save(&account, &tokens, Some(account.generation))
                .unwrap();
        }
        let usage = ScriptedUpstream::start().await.unwrap();
        let data = ScriptedUpstream::start().await.unwrap();
        let token = ScriptedUpstream::start().await.unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.codex_base = data.url.clone();
        config.upstream.anthropic_base = data.url.clone();
        config.upstream.codex_usage_base = usage.url.clone();
        config.upstream.claude_usage_base = usage.url.clone();
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap()
            .with_token_endpoint(provider, token.url.clone()),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-client".into(),
                admin_token: "synthetic-admin".into(),
            },
            relay::router(state.clone()),
            qb_core::admin::runtime_routes(state.clone(), 0),
        )
        .await
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self {
            state,
            obs,
            store,
            usage,
            data,
            token,
            servers,
            client,
            id: account.id,
            cpa_file,
            _root: root,
            _serial: serial,
        }
    }

    fn account(&self) -> Account {
        self.store.load(&self.id).unwrap()
    }

    fn newer_link(&self) {
        let mut value: Value = serde_json::from_slice(&fs::read(&self.cpa_file).unwrap()).unwrap();
        value["last_refresh"] = json!(Utc::now());
        value["access_token"] = json!("synthetic-access-new");
        value["refresh_token"] = json!("synthetic-refresh-new");
        fs::write(&self.cpa_file, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    async fn seed_quota(&self, reset: i64) {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("x-codex-primary-window-minutes", "300".to_owned()),
            ("x-codex-primary-used-percent", "100".to_owned()),
            ("x-codex-primary-reset-at", reset.to_string()),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        self.state
            .accounts
            .observe_headers(&self.id, Provider::Codex, &headers)
            .await;
        assert_quota(&self.account(), reset);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    async fn data_request(&self) -> reqwest::Response {
        self.client
            .post(format!("http://{}/v1/responses", self.servers.data_addr))
            .bearer_auth("synthetic-client")
            .json(&json!({"input": "synthetic-input"}))
            .send()
            .await
            .unwrap()
    }

    async fn refresh(&self) -> reqwest::Response {
        self.client
            .post(format!(
                "http://{}/admin/v1/accounts/{}/refresh-usage",
                self.servers.admin_addr, self.id
            ))
            .bearer_auth("synthetic-admin")
            .send()
            .await
            .unwrap()
    }
}

fn usage(used: f64, reset: i64) -> ScriptedResponse {
    ScriptedResponse::json(
        StatusCode::OK,
        json!({"rate_limit": {"limit_reached": false,
        "primary_window": {"used_percent": used, "limit_window_seconds": 18000, "reset_at": reset}},
        "plan_type": "plus", "credits": {"has_credits": true, "unlimited": false, "balance": 10}})
        .to_string(),
    )
}

fn unauthorized() -> ScriptedResponse {
    ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"message":"synthetic expired token"}}"#,
    )
}

fn assert_quota(account: &Account, reset: i64) {
    assert!(
        matches!(&account.health, Health::Cooling { reason: HealthReason::Quota, until, scope }
        if until.timestamp() == reset && scope == "account"),
        "health: {:?}",
        account.health
    );
}

#[tokio::test]
async fn poll_linked_401_reloads_newer_cpa_and_retries_once() {
    let h = Harness::new(Provider::Codex, false).await;
    h.newer_link();
    h.usage.state.push(unauthorized());
    h.usage
        .state
        .push(usage(25.0, Utc::now().timestamp() + 3600));
    assert_eq!(
        h.state.quota.poll(&h.id).await.unwrap().health,
        Health::Ready
    );
    let requests = h.usage.state.requests();
    assert_eq!(requests.len(), 2);
    for (request, token) in requests
        .iter()
        .zip(["synthetic-access-old", "synthetic-access-new"])
    {
        assert_eq!(request.method, axum::http::Method::GET);
        assert_eq!(request.path_and_query, "/backend-api/wham/usage");
        assert_eq!(request.headers["authorization"], format!("Bearer {token}"));
        assert_eq!(request.headers["chatgpt-account-id"], "synthetic-m5");
        assert_eq!(request.headers["accept"], "application/json");
        assert!(
            request.headers["user-agent"]
                .to_str()
                .unwrap()
                .starts_with("quota-baton/")
        );
    }
    assert!(h.token.state.requests().is_empty());
    assert_eq!(
        h.store.tokens(&h.account()).unwrap().access_token,
        "synthetic-access-new"
    );
}

#[tokio::test]
async fn poll_linked_401_without_newer_token_cools_link_stale() {
    let h = Harness::new(Provider::Codex, false).await;
    h.usage.state.push(unauthorized());
    assert_eq!(
        h.state.quota.poll(&h.id).await.unwrap_err(),
        PollError::Unavailable
    );
    assert!(matches!(
        h.account().health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
    assert_eq!(h.usage.state.requests().len(), 1);
    assert!(h.token.state.requests().is_empty());
}

#[tokio::test]
async fn poll_and_data_401_share_owned_singleflight_refresh() {
    let h = Harness::new(Provider::Codex, true).await;
    let mut denied = unauthorized();
    denied.chunks[0].0 = Duration::from_millis(50);
    h.usage.state.push(denied.clone());
    h.data.state.push(denied);
    h.usage
        .state
        .push(usage(25.0, Utc::now().timestamp() + 3600));
    h.data
        .state
        .push(ScriptedResponse::json(StatusCode::OK, r#"{"ok":true}"#));
    let mut rotated = ScriptedResponse::json(
        StatusCode::OK,
        r#"{"access_token":"synthetic-access-new","refresh_token":"synthetic-refresh-new","expires_in":3600}"#,
    );
    rotated.chunks[0].0 = Duration::from_secs(1);
    h.token.state.push(rotated);
    let (poll, response) = tokio::join!(h.state.quota.poll(&h.id), h.data_request());
    assert_eq!(poll.unwrap().health, Health::Ready);
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let requests = h.token.state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, axum::http::Method::POST);
    let body = std::str::from_utf8(&requests[0].body).unwrap();
    assert!(body.contains("grant_type=refresh_token"));
    assert!(body.contains("refresh_token=synthetic-refresh-old"));
    for requests in [h.usage.state.requests(), h.data.state.requests()] {
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers["authorization"],
            "Bearer synthetic-access-old"
        );
        assert_eq!(
            requests[1].headers["authorization"],
            "Bearer synthetic-access-new"
        );
    }
    assert_eq!(
        h.store.tokens(&h.account()).unwrap().refresh_token,
        "synthetic-refresh-new"
    );
}

#[tokio::test]
async fn stale_poll_revocation_retries_current_token_without_revoking_account() {
    let h = Harness::new(Provider::Codex, true).await;
    let mut revoked = ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"token_revoked"}}"#,
    );
    revoked.chunks[0].0 = Duration::from_millis(150);
    h.usage.state.push(revoked);
    h.usage
        .state
        .push(usage(20.0, Utc::now().timestamp() + 3600));
    let rotate = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.usage.state.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let a = h.account();
        let mut tokens = h.store.tokens(&a).unwrap();
        tokens.access_token = "synthetic-current-poll-token".into();
        h.store.save(&a, &tokens, Some(a.generation)).unwrap();
    };
    let (result, ()) = tokio::join!(h.state.quota.poll(&h.id), rotate);
    assert_eq!(result.unwrap().health, Health::Ready);
    let requests = h.usage.state.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].headers["authorization"],
        "Bearer synthetic-current-poll-token"
    );
    assert!(h.token.state.requests().is_empty());
}

#[tokio::test]
async fn poll_terminal_owned_refresh_requires_relogin() {
    for code in [
        "invalid_grant",
        "refresh_token_invalidated",
        "refresh_token_reused",
        "refresh_token_expired",
        "refresh_token_revoked",
        "token_endpoint_401",
    ] {
        let h = Harness::new(Provider::Codex, true).await;
        h.usage.state.push(unauthorized());
        h.token.state.push(ScriptedResponse::json(
            if code == "token_endpoint_401" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            },
            json!({"error": code}).to_string(),
        ));
        assert_eq!(
            h.state.quota.poll(&h.id).await.unwrap_err(),
            PollError::Unavailable
        );
        assert!(
            matches!(h.account().health, Health::NeedsRelogin { reason, .. } if reason == code)
        );
        assert_eq!(h.usage.state.requests().len(), 1);
        assert_eq!(h.token.state.requests().len(), 1);
        assert_eq!(
            h.state.quota.poll(&h.id).await.unwrap_err(),
            PollError::Excluded
        );
    }
}

#[tokio::test]
async fn poll_observed_exhaustion_and_credit_hold_use_limiting_reset() {
    for (used, allow_credits, cooling) in [
        (100.0, true, true),
        (99.0, false, true),
        (99.0, true, false),
        (98.0, false, false),
    ] {
        let h = Harness::new(Provider::Codex, false).await;
        h.state
            .accounts
            .change(&h.id, None, None, None, Some(allow_credits))
            .await
            .unwrap();
        let reset = Utc::now().timestamp() + 3600;
        let mut body: Value = serde_json::from_slice(&usage(used, reset).chunks[0].1).unwrap();
        body["rate_limit"]["secondary_window"] = json!({"used_percent": 10,
            "limit_window_seconds": 604800, "reset_at": reset + 86400});
        h.usage
            .state
            .push(ScriptedResponse::json(StatusCode::OK, body.to_string()));
        let account = h.state.quota.poll(&h.id).await.unwrap();
        if cooling {
            assert_quota(&account, reset);
        } else {
            assert_eq!(account.health, Health::Ready);
        }
        let views = h.state.accounts.views().await.unwrap();
        assert_eq!(views[0]["windows"][0]["used_pct"], used);
    }
}

#[tokio::test]
async fn poll_early_reset_requires_more_than_three_hundred_seconds() {
    for delta in [301, -301, 300, -300, 0] {
        let h = Harness::new(Provider::Codex, false).await;
        let reset = Utc::now().timestamp() + 3600;
        h.seed_quota(reset).await;
        h.usage.state.push(usage(25.0, reset + delta));
        let account = h.state.quota.poll(&h.id).await.unwrap();
        if delta.abs() > 300 {
            assert_eq!(account.health, Health::Ready);
        } else {
            assert_quota(&account, reset);
        }
        assert_eq!(h.usage.state.requests().len(), 1);
    }
}

#[tokio::test]
async fn claude_404_invalid_json_and_shape_mismatch_disable_polling_six_hours() {
    for (status, body) in [
        (StatusCode::NOT_FOUND, "{}"),
        (StatusCode::OK, "not json"),
        (StatusCode::OK, r#"{"unexpected":{"value":12}}"#),
    ] {
        let h = Harness::new(Provider::Claude, false).await;
        h.usage.state.push(ScriptedResponse::json(status, body));
        assert_eq!(
            h.state.quota.poll(&h.id).await.unwrap_err(),
            PollError::Unavailable
        );
        match h.state.quota.poll(&h.id).await.unwrap_err() {
            PollError::TooSoon { retry_after } => assert!(
                (21590..=21600).contains(&retry_after),
                "retry_after={retry_after}"
            ),
            other => panic!("expected six-hour disable, got {other:?}"),
        }
        let requests = h.usage.state.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path_and_query, "/api/oauth/usage");
        assert_eq!(requests[0].headers["anthropic-beta"], "oauth-2025-04-20");
        assert_eq!(h.account().health, Health::Ready);
    }
}

#[tokio::test]
async fn admin_refresh_returns_account_view_too_soon_and_quota_updated_event() {
    let h = Harness::new(Provider::Codex, false).await;
    let events = h
        .client
        .get(format!("http://{}/admin/v1/events", h.servers.admin_addr))
        .bearer_auth("synthetic-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), StatusCode::OK);
    let mut stream = events.bytes_stream();
    h.usage
        .state
        .push(usage(25.0, Utc::now().timestamp() + 3600));
    let response = h.refresh().await;
    assert_eq!(response.status(), StatusCode::OK);
    let view: Value = response.json().await.unwrap();
    assert_eq!(view["id"], h.id);
    assert_eq!(view["plan"], "plus");
    assert_eq!(view["windows"][0]["used_pct"], 25.0);
    assert_eq!(view["credits"]["balance"], 10.0);
    assert_eq!(view["bound_sessions"], 0);
    for secret in [
        "synthetic-access",
        "synthetic-refresh",
        "secret_dpapi",
        "token_meta",
    ] {
        assert!(!view.to_string().contains(secret));
    }
    let event = tokio::time::timeout(Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
            let text = String::from_utf8_lossy(&bytes);
            if text.contains("event: quota_updated") && text.contains("\n\n") {
                return text.into_owned();
            }
        }
        panic!("event stream ended before quota_updated");
    })
    .await
    .unwrap();
    assert!(event.contains(&h.id));
    assert!(event.contains("used_pct"));
    let event_data: Value = serde_json::from_str(
        event
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(event_data["snapshot"]["source"], "poll");
    let response = h.refresh().await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "poll_too_soon");
    assert_eq!(body["error"]["retry_after"], retry);
    assert!((1..=330).contains(&retry));
    assert_eq!(h.usage.state.requests().len(), 1);
}

#[tokio::test]
async fn row_fourteen_pre_unavailable_poll_is_bounded_to_three_seconds() {
    let h = Harness::new(Provider::Codex, false).await;
    h.seed_quota(Utc::now().timestamp() + 3600).await;
    let mut slow = usage(25.0, Utc::now().timestamp() + 7200);
    slow.chunks[0].0 = Duration::from_secs(10);
    h.usage.state.push(slow);
    let started = Instant::now();
    let response = tokio::time::timeout(Duration::from_secs(4), h.data_request())
        .await
        .expect("row 14 exceeded the three-second poll budget");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    response.bytes().await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(2800));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(h.usage.state.requests().len(), 1);
    assert!(h.data.state.requests().is_empty());
    assert!(matches!(
        h.account().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    assert!(matches!(
        h.state.quota.poll(&h.id).await,
        Err(PollError::TooSoon { .. })
    ));
}

#[tokio::test]
async fn disabled_account_is_excluded_without_a_usage_request() {
    let h = Harness::new(Provider::Codex, false).await;
    h.state
        .accounts
        .change(&h.id, Some(UserState::Disabled), None, None, None)
        .await
        .unwrap();
    assert_eq!(
        h.state.quota.poll(&h.id).await.unwrap_err(),
        PollError::Excluded
    );
    assert!(h.usage.state.requests().is_empty());
    assert!(h.token.state.requests().is_empty());
}

#[tokio::test]
async fn claude_subbucket_retains_scoped_window_without_cooling_unknown_model() {
    let h = Harness::new(Provider::Claude, false).await;
    h.usage.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({
            "five_hour": {"utilization": 0.25}, "seven_day": {"utilization": 25},
            "seven_day_opus": {"utilization": 100, "resets_at": Utc::now().timestamp() + 3600}
        })
        .to_string(),
    ));
    assert_eq!(
        h.state.quota.poll(&h.id).await.unwrap().health,
        Health::Ready
    );
    let views = h.state.accounts.views().await.unwrap();
    let windows = views[0]["windows"].as_array().unwrap();
    let opus = windows.iter().find(|w| w["name"] == "7d_oi").unwrap();
    assert_eq!(opus["used_pct"], 100.0);
    assert!(!opus["reset_at"].is_null());
    assert_eq!(windows.len(), 3);
}

#[tokio::test]
async fn poll_body_limit_accepts_256_kib_and_rejects_one_byte_more() {
    for length in [256 * 1024, 256 * 1024 + 1] {
        let h = Harness::new(Provider::Codex, false).await;
        let response = usage(25.0, Utc::now().timestamp() + 3600);
        let mut body = String::from_utf8(response.chunks[0].1.to_vec()).unwrap();
        body.push_str(&" ".repeat(length - body.len()));
        h.usage
            .state
            .push(ScriptedResponse::json(StatusCode::OK, body));
        let result = h.state.quota.poll(&h.id).await;
        if length == 256 * 1024 {
            assert_eq!(result.unwrap().health, Health::Ready);
            assert_eq!(
                h.state.accounts.views().await.unwrap()[0]["windows"][0]["used_pct"],
                25.0
            );
        } else {
            assert_eq!(result.unwrap_err(), PollError::Unavailable);
            assert!(
                h.state.accounts.views().await.unwrap()[0]["windows"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(h.account().health, Health::Ready);
        }
        assert_eq!(h.usage.state.requests().len(), 1);
    }
}

#[tokio::test]
async fn selection_pre_poll_recovers_eligibility_before_row_fourteen() {
    let h = Harness::new(Provider::Codex, false).await;
    let reset = Utc::now().timestamp() + 3600;
    h.seed_quota(reset).await;
    h.usage.state.push(usage(25.0, reset + 301));
    h.data
        .state
        .push(ScriptedResponse::json(StatusCode::OK, r#"{"ok":true}"#));
    let response = h.data_request().await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-quota-baton-account"], h.id);
    response.bytes().await.unwrap();
    assert_eq!(h.account().health, Health::Ready);
    assert_eq!(h.usage.state.requests().len(), 1);
    assert_eq!(h.data.state.requests().len(), 1);
}

#[tokio::test]
async fn partial_poll_early_reset_does_not_restore_limit_signal_on_reload() {
    let h = Harness::new(Provider::Codex, false).await;
    let reset = Utc::now().timestamp() + 3600;
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-codex-primary-window-minutes", "300".to_owned()),
        ("x-codex-primary-used-percent", "100".to_owned()),
        ("x-codex-primary-reset-at", reset.to_string()),
        ("x-codex-secondary-window-minutes", "10080".to_owned()),
        ("x-codex-secondary-reset-at", (reset + 86400).to_string()),
        ("x-codex-limit-reached", "true".to_owned()),
    ] {
        headers.insert(name, value.parse().unwrap());
    }
    h.state
        .accounts
        .observe_headers(&h.id, Provider::Codex, &headers)
        .await;
    assert_quota(&h.account(), reset);
    h.obs.flush().unwrap();
    let before = h.obs.quota_snapshot(&h.id).unwrap();
    assert_eq!(before["signals"]["limit_reached"], true);
    assert!(
        before["windows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| { w["name"] == "7d" && w["used_pct"].is_null() })
    );
    tokio::time::sleep(Duration::from_millis(2)).await;

    h.usage.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({"rate_limit": {"primary_window": {
            "used_percent": 25, "limit_window_seconds": 18000, "reset_at": reset + 301
        }}})
        .to_string(),
    ));
    assert_eq!(
        h.state.quota.poll(&h.id).await.unwrap().health,
        Health::Ready
    );
    let current = h.obs.quota_snapshot(&h.id).unwrap();
    assert!(current["signals"]["limit_reached"].is_null());
    h.obs.flush().unwrap();

    let reopened = Observability::new(h.store.home(), None).unwrap();
    let restored = reopened.quota_snapshot(&h.id).unwrap();
    assert!(restored["signals"]["limit_reached"].is_null());
    assert_eq!(restored, current);
    assert!(
        restored["windows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| { w["name"] == "7d" && w["used_pct"].is_null() })
    );
    assert_eq!(h.usage.state.requests().len(), 1);
}

#[tokio::test]
async fn cancelled_row_fourteen_poll_still_cools_owned_transient_refresh_failure() {
    let h = Harness::new(Provider::Codex, true).await;
    h.usage.state.push(unauthorized());
    // A nonterminal 400 is read before classification; 5xx returns at headers.
    let mut failure = ScriptedResponse::json(
        StatusCode::BAD_REQUEST,
        r#"{"error":"temporarily_unavailable"}"#,
    );
    failure.chunks[0].0 = Duration::from_millis(3500);
    h.token.state.push(failure);

    let started = Instant::now();
    // Exercise the row-14/15 hook directly with an otherwise ready candidate.
    tokio::time::timeout(
        Duration::from_secs(4),
        h.state.quota.before_unavailable(Provider::Codex),
    )
    .await
    .expect("row-14 poll hook exceeded its three-second budget");
    assert!(started.elapsed() >= Duration::from_millis(2800));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(h.account().health, Health::Ready);
    assert_eq!(h.token.state.requests().len(), 1);

    let account = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let account = h.account();
            if matches!(
                account.health,
                Health::Cooling {
                    reason: HealthReason::Auth,
                    ..
                }
            ) {
                break account;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled poll lost the transient-refresh auth cooling");
    let Health::Cooling {
        reason,
        until,
        scope,
    } = &account.health
    else {
        panic!("expected auth cooling: {:?}", account.health);
    };
    assert_eq!(*reason, HealthReason::Auth);
    assert_eq!(scope, "account");
    assert!((295..=300).contains(&(until.to_owned() - Utc::now()).num_seconds()));
    assert_eq!(
        h.store.tokens(&account).unwrap().access_token,
        "synthetic-access-old"
    );
    assert_eq!(h.usage.state.requests().len(), 1);
    assert_eq!(h.token.state.requests().len(), 1);
    assert!(h.data.state.requests().is_empty());
}
