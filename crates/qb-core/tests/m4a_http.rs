use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, Utc};
use qb_core::{
    accounts::{Account, AccountStore, Health, HealthReason, Provider, Source, UserState, import},
    config::{Config, RoutingConfig},
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
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
struct Harness {
    _root: tempfile::TempDir,
    home: PathBuf,
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    state: Arc<RelayState>,
    fake: ScriptedUpstream,
    servers: RunningServers,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}
impl Harness {
    async fn new(provider: Provider, n: usize, budget: u64, memory: u64) -> Self {
        Self::new_with_network(provider, n, budget, memory, false).await
    }
    async fn new_with_network(
        provider: Provider,
        n: usize,
        budget: u64,
        memory: u64,
        dead: bool,
    ) -> Self {
        Self::new_with_network_and_routing(
            provider,
            n,
            budget,
            memory,
            dead,
            RoutingConfig::default(),
        )
        .await
    }
    async fn new_with_routing(
        provider: Provider,
        n: usize,
        budget: u64,
        memory: u64,
        routing: RoutingConfig,
    ) -> Self {
        Self::new_with_network_and_routing(provider, n, budget, memory, false, routing).await
    }
    async fn new_with_network_and_routing(
        provider: Provider,
        n: usize,
        budget: u64,
        memory: u64,
        dead: bool,
        routing: RoutingConfig,
    ) -> Self {
        Self::new_configured(provider, n, budget, memory, dead, routing, |_| {}).await
    }
    async fn new_configured(
        provider: Provider,
        n: usize,
        budget: u64,
        memory: u64,
        dead: bool,
        routing: RoutingConfig,
        configure: impl FnOnce(&mut Config),
    ) -> Self {
        let serial = SERIAL.lock().await;
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m4a-http");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let cpa = root.path().join("cpa");
        fs::create_dir_all(&cpa).unwrap();
        let mut activate = Vec::new();
        for i in 0..n {
            let email = format!("fixture-{i}@example.invalid");
            let value = json!({"type":provider.as_str(),"email":email,"account_id":format!("synthetic-{i}"),"account_uuid":format!("uuid-{i}"),"organization_uuid":"org-fixture","access_token":format!("synthetic-access-{i}"),"refresh_token":format!("synthetic-refresh-{i}"),"id_token":"synthetic-id","expired":"2099-01-01T00:00:00Z","last_refresh":"2026-01-01T00:00:00Z"});
            fs::write(
                cpa.join(format!("{i}.json")),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
            activate.push(format!("{}:{email}", provider.as_str()));
        }
        let store = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match AccountStore::acquire(&home) {
                    Ok(s) => break Arc::new(s),
                    Err(qb_core::accounts::StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(e) => panic!("synthetic store: {e}"),
                }
            }
        })
        .await
        .unwrap();
        import::run(&store, &cpa, &activate, false).unwrap();
        let fake = ScriptedUpstream::start().await.unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.routing = routing;
        config.guard.inflight_budget_mib = budget;
        config.guard.memory_hard_limit_mib = memory;
        config.upstream.anthropic_base = fake.url.clone();
        config.upstream.codex_base = fake.url.clone();
        if dead {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            config.upstream.codex_base = format!("http://{}", listener.local_addr().unwrap());
            drop(listener);
        }
        configure(&mut config);
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap()
            .with_token_endpoint(provider, fake.url.clone()),
        );
        let admin = qb_core::admin::runtime_routes(state.clone(), 0);
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-client".into(),
                admin_token: "synthetic-admin".into(),
            },
            relay::router(state.clone()),
            admin,
        )
        .await
        .unwrap();
        Self {
            _root: root,
            home,
            store,
            obs,
            state,
            fake,
            servers,
            _serial: serial,
        }
    }
    async fn send(&self, provider: Provider, body: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!(
                "http://{}{}",
                self.servers.data_addr,
                if provider == Provider::Codex {
                    "/v1/responses"
                } else {
                    "/v1/messages"
                }
            ))
            .bearer_auth("synthetic-client")
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
    }
    async fn send_with_session(&self, provider: Provider, session: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!(
                "http://{}{}",
                self.servers.data_addr,
                if provider == Provider::Codex {
                    "/v1/responses"
                } else {
                    "/v1/messages"
                }
            ))
            .bearer_auth("synthetic-client")
            .header("session-id", session)
            .header("x-claude-code-session-id", session)
            .body("{}")
            .send()
            .await
            .unwrap()
    }
    fn ids(&self) -> Vec<String> {
        self.store
            .list()
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect()
    }
    fn ledger(&self) -> Vec<Value> {
        self.obs.flush().unwrap();
        fs::read_dir(self.home.join("ledger"))
            .unwrap()
            .flat_map(|e| {
                let text = fs::read_to_string(e.unwrap().path()).unwrap();
                for secret in [
                    "synthetic-access",
                    "synthetic-refresh",
                    "synthetic-id",
                    "synthetic-client",
                    "synthetic-admin",
                    "conversation-marker",
                ] {
                    assert!(!text.contains(secret));
                }
                text.lines()
                    .map(|s| serde_json::from_str(s).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}
fn ok() -> ScriptedResponse {
    ScriptedResponse::json(StatusCode::OK, "{\"ok\":true}")
}
fn quota(reset: i64) -> ScriptedResponse {
    ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS,json!({"error":{"type":"usage_limit_reached","resets_at":reset,"message":"conversation-marker"}}).to_string())
}

// AccountStore invokes this observer after a successful persisted write, under
// its transaction gate. Never query the store from inside the observer.
fn c6_4_health_writes(h: &Harness) -> Arc<std::sync::Mutex<Vec<(Instant, Account)>>> {
    let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = writes.clone();
    let obs = h.obs.clone();
    h.store.set_observer(Arc::new(move |account, previous| {
        if previous != &account.health {
            captured
                .lock()
                .unwrap()
                .push((Instant::now(), account.clone()));
            obs.record_health(&account.id, previous, &account.health);
        }
    }));
    writes
}

fn c6_4_quota(provider: Provider, reset: i64) -> ScriptedResponse {
    let mut response = quota(reset);
    if provider == Provider::Claude {
        response.headers.insert(
            "anthropic-ratelimit-unified-5h-status",
            "rejected".parse().unwrap(),
        );
        response.headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            reset.to_string().parse().unwrap(),
        );
    }
    response
}

fn c6_4_assert_cooling(
    account: &Account,
    reason: HealthReason,
    earliest: chrono::DateTime<Utc>,
    latest: chrono::DateTime<Utc>,
) {
    assert!(matches!(account.source, Source::Cpa { .. }));
    assert!(
        matches!(&account.health, Health::Cooling { reason: actual, until, scope }
        if *actual == reason && *until >= earliest && *until <= latest && scope == "account"),
        "unexpected health: {:?}",
        account.health
    );
    if reason == HealthReason::Payment {
        assert!(
            account
                .payment_until
                .is_some_and(|until| until >= earliest && until <= latest)
        );
    }
}

async fn c6_4_assert_served(response: reqwest::Response, h: &Harness, id: &str) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-quota-baton-account"], id);
    response.bytes().await.unwrap();
    assert!(matches!(
        h.store.load(id).unwrap().source,
        Source::Cpa { .. }
    ));
    assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
}

#[tokio::test]
async fn c6_4_exhausted_bound_account_moves_to_healthy() {
    for provider in [Provider::Claude, Provider::Codex] {
        let h = Harness::new(provider, 2, 384, 1024).await;
        let ids = h.ids();
        c6_4_assert_served(
            h.send_with_session(provider, "exhausted").await,
            &h,
            &ids[0],
        )
        .await;
        let reset = Utc::now() + ChronoDuration::hours(1);
        h.store
            .update_metadata(&ids[0], |a| {
                a.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: reset,
                    scope: "account".into(),
                }
            })
            .unwrap();
        c6_4_assert_served(
            h.send_with_session(provider, "exhausted").await,
            &h,
            &ids[1],
        )
        .await;
        assert_eq!(
            h.state
                .bindings
                .current(Some(&format!("{}:exhausted", provider.as_str()))),
            Some(ids[1].clone())
        );
        c6_4_assert_cooling(
            &h.store.load(&ids[0]).unwrap(),
            HealthReason::Quota,
            reset,
            reset,
        );
        assert_eq!(h.fake.state.requests().len(), 2);
    }
}

#[tokio::test]
async fn c6_4_stale_imported_binding_not_used() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let ids = h.ids();
    c6_4_assert_served(
        h.send_with_session(Provider::Codex, "restart").await,
        &h,
        &ids[0],
    )
    .await;
    h.state.bindings.flush().unwrap();
    let reset = Utc::now() + ChronoDuration::hours(1);
    h.store
        .update_metadata(&ids[0], |a| {
            a.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: reset,
                scope: "account".into(),
            }
        })
        .unwrap();
    let restarted = qb_core::bindings::BindingTable::new(&h.home, Default::default()).unwrap();
    let imported = restarted.export().unwrap();
    assert_eq!(imported["bindings"][0]["account"], ids[0]);
    h.state.bindings.import(imported).unwrap();
    c6_4_assert_served(
        h.send_with_session(Provider::Codex, "restart").await,
        &h,
        &ids[1],
    )
    .await;
    assert_eq!(
        h.state.bindings.current(Some("codex:restart")),
        Some(ids[1].clone())
    );
    c6_4_assert_cooling(
        &h.store.load(&ids[0]).unwrap(),
        HealthReason::Quota,
        reset,
        reset,
    );
}

#[tokio::test]
async fn c6_4_429_reset_headers_recorded_before_failover() {
    for provider in [Provider::Claude, Provider::Codex] {
        let h = Harness::new(provider, 2, 384, 1024).await;
        let ids = h.ids();
        let writes = c6_4_health_writes(&h);
        let reset = Utc::now().timestamp() + 3600;
        // No reset in the body: only the headers can supply the deadline.
        let mut response = ScriptedResponse::json(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"type":"usage_limit_reached","message":"synthetic quota limit"}}"#,
        );
        let headers = if provider == Provider::Codex {
            vec![
                ("x-codex-primary-used-percent", "100".to_owned()),
                ("x-codex-primary-reset-at", reset.to_string()),
            ]
        } else {
            vec![
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "1.0".to_owned(),
                ),
                ("anthropic-ratelimit-unified-5h-reset", reset.to_string()),
                (
                    "anthropic-ratelimit-unified-5h-status",
                    "rejected".to_owned(),
                ),
            ]
        };
        for (name, value) in headers {
            response.headers.insert(name, value.parse().unwrap());
        }
        h.fake.state.push(response);
        h.fake.state.push(ok());
        c6_4_assert_served(h.send_with_session(provider, "atomic").await, &h, &ids[1]).await;
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        let writes = writes.lock().unwrap();
        let (written_at, account) = writes
            .iter()
            .find(|(_, a)| {
                a.id == ids[0]
                    && matches!(
                        a.health,
                        Health::Cooling {
                            reason: HealthReason::Quota,
                            ..
                        }
                    )
            })
            .unwrap();
        let until = chrono::DateTime::from_timestamp(reset, 0).unwrap();
        c6_4_assert_cooling(account, HealthReason::Quota, until, until);
        c6_4_assert_cooling(
            &h.store.load(&ids[0]).unwrap(),
            HealthReason::Quota,
            until,
            until,
        );
        assert!(*written_at < requests[1].received_at);
        assert_ne!(
            requests[0].headers["authorization"],
            requests[1].headers["authorization"]
        );
    }
}

#[tokio::test]
async fn c6_4_401_revocation_owned_and_linked() {
    for provider in [Provider::Claude, Provider::Codex] {
        for owned in [false, true] {
            let h = Harness::new(provider, 2, 384, 1024).await;
            let ids = h.ids();
            if owned {
                let mut account = h.store.load(&ids[0]).unwrap();
                let tokens = h.store.tokens(&account).unwrap();
                account.source = Source::Oauth;
                h.store
                    .save(&account, &tokens, Some(account.generation))
                    .unwrap();
            }
            let writes = c6_4_health_writes(&h);
            let before = Utc::now();
            let body = if provider == Provider::Claude {
                r#"{"type":"error","error":{"type":"authentication_error","message":"OAuth token has been revoked"}}"#
            } else {
                r#"{"error":{"code":"token_revoked","message":"invalidated oauth token"}}"#
            };
            h.fake
                .state
                .push(ScriptedResponse::json(StatusCode::UNAUTHORIZED, body));
            h.fake.state.push(ok());
            c6_4_assert_served(h.send_with_session(provider, "revoked").await, &h, &ids[1]).await;
            let failed = h.store.load(&ids[0]).unwrap();
            if owned {
                assert_eq!(failed.source, Source::Oauth);
                assert!(
                    matches!(failed.health, Health::NeedsRelogin { since, .. } if since >= before && since <= Utc::now())
                );
            } else {
                assert!(matches!(failed.source, Source::Cpa { .. }));
                c6_4_assert_cooling(
                    &failed,
                    HealthReason::LinkStale,
                    before + ChronoDuration::minutes(5),
                    Utc::now() + ChronoDuration::minutes(5),
                );
            }
            let requests = h.fake.state.requests();
            assert_eq!(requests.len(), 2);
            assert!(writes.lock().unwrap().iter().any(|(at, a)| a.id == ids[0]
                && a.health == failed.health
                && *at < requests[1].received_at));
            assert_eq!(
                h.state
                    .bindings
                    .current(Some(&format!("{}:revoked", provider.as_str()))),
                Some(ids[1].clone())
            );
        }
    }
}

#[tokio::test]
async fn c6_4_402_payment_failover_and_relay_when_none() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let ids = h.ids();
    let body = r#"{"error":{"type":"payment_required","message":"synthetic billing rejection"}}"#;
    let writes = c6_4_health_writes(&h);
    assert_eq!(h.store.load(&ids[0]).unwrap().payment_failures, 0);
    // Repeated payment failures retain their persisted backoff across expiry.
    for (index, hours) in [6, 12, 24, 24].into_iter().enumerate() {
        h.store
            .update_metadata(&ids[0], |a| {
                let expired = Utc::now() - ChronoDuration::seconds(1);
                if let Health::Cooling { until, .. } = &mut a.health {
                    *until = expired;
                }
                a.payment_until = Some(expired);
            })
            .unwrap();
        h.state
            .bindings
            .import(json!({"schema":1,"bindings":[]}))
            .unwrap();
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::PAYMENT_REQUIRED, body));
        h.fake.state.push(ok());
        let session = format!("payment-{index}");
        let before = Utc::now();
        c6_4_assert_served(
            h.send_with_session(Provider::Codex, &session).await,
            &h,
            &ids[1],
        )
        .await;
        let account = h.store.load(&ids[0]).unwrap();
        c6_4_assert_cooling(
            &account,
            HealthReason::Payment,
            before + ChronoDuration::hours(hours),
            Utc::now() + ChronoDuration::hours(hours),
        );
        assert_eq!(account.payment_failures, index as u32 + 1);
        let requests = h.fake.state.requests();
        assert!(writes.lock().unwrap().iter().any(|(at, a)| a.id == ids[0]
            && a.health == account.health
            && *at < requests[index * 2 + 1].received_at));
        assert_eq!(
            h.state
                .bindings
                .current(Some(&format!("codex:payment-{index}"))),
            Some(ids[1].clone())
        );
    }
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::PAYMENT_REQUIRED, body));
    let before = Utc::now();
    let response = h.send_with_session(Provider::Codex, "payment-3").await;
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert_eq!(response.bytes().await.unwrap(), body);
    c6_4_assert_cooling(
        &h.store.load(&ids[1]).unwrap(),
        HealthReason::Payment,
        before + ChronoDuration::hours(6),
        Utc::now() + ChronoDuration::hours(6),
    );
}

#[tokio::test]
async fn c6_4_400_billing_error_is_row7_not_row9() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    let ids = h.ids();
    // Both pre-existing groups must belong to A; new placements otherwise
    // spread under D20 before this test exercises payment recovery.
    h.state
        .accounts
        .change(&ids[1], Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    for session in ["billing", "untouched"] {
        c6_4_assert_served(
            h.send_with_session(Provider::Claude, session).await,
            &h,
            &ids[0],
        )
        .await;
    }
    h.state
        .accounts
        .change(&ids[1], Some(UserState::Active), None, None, None)
        .await
        .unwrap();
    let writes = c6_4_health_writes(&h);
    let body = r#"{"type":"error","error":{"type":"billing_error","message":"Credit balance is too low"}}"#;
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::BAD_REQUEST, body));
    h.fake.state.push(ok());
    let before = Utc::now();
    c6_4_assert_served(
        h.send_with_session(Provider::Claude, "billing").await,
        &h,
        &ids[1],
    )
    .await;
    let failed = h.store.load(&ids[0]).unwrap();
    c6_4_assert_cooling(
        &failed,
        HealthReason::Payment,
        before + ChronoDuration::hours(6),
        Utc::now() + ChronoDuration::hours(6),
    );
    assert_eq!(
        h.state.bindings.current(Some("claude:billing")),
        Some(ids[1].clone())
    );
    assert_eq!(
        h.state.bindings.current(Some("claude:untouched")),
        Some(ids[0].clone())
    );
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 4);
    assert!(writes.lock().unwrap().iter().any(|(at, a)| a.id == ids[0]
        && a.health == failed.health
        && *at < requests[3].received_at));
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::BAD_REQUEST, body));
    let response = h.send_with_session(Provider::Claude, "billing").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.bytes().await.unwrap(), body);
    assert_eq!(h.store.load(&ids[0]).unwrap().health, failed.health);
}

#[tokio::test]
async fn c6_4_models_answered_by_eligible_account() {
    for provider in [Provider::Claude, Provider::Codex] {
        let h = Harness::new(provider, 2, 384, 1024).await;
        let ids = h.ids();
        let reset = Utc::now() + ChronoDuration::hours(1);
        h.store
            .update_metadata(&ids[0], |a| {
                a.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: reset,
                    scope: "account".into(),
                }
            })
            .unwrap();
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::OK,
            r#"{"object":"list","data":[{"id":"synthetic-model"}]}"#,
        ));
        let mut request = reqwest::Client::new()
            .get(format!("http://{}/v1/models", h.servers.data_addr))
            .bearer_auth("synthetic-client");
        if provider == Provider::Claude {
            request = request.header("anthropic-version", "2023-06-01");
        }
        c6_4_assert_served(request.send().await.unwrap(), &h, &ids[1]).await;
        assert_eq!(h.fake.state.requests().len(), 1);
        c6_4_assert_cooling(
            &h.store.load(&ids[0]).unwrap(),
            HealthReason::Quota,
            reset,
            reset,
        );
    }
}

#[tokio::test]
async fn c6_4_all_exhausted_genuine_429_no_credit_text() {
    for provider in [Provider::Claude, Provider::Codex] {
        let h = Harness::new(provider, 2, 384, 1024).await;
        let ids = h.ids();
        let now = Utc::now().timestamp();
        for seconds in [600, 300] {
            h.fake.state.push(c6_4_quota(provider, now + seconds));
        }
        let expected = quota(now + 300).chunks[0].1.clone();
        for _ in 0..2 {
            let response = h.send_with_session(provider, "all-exhausted").await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            let retry: i64 = response.headers()["retry-after"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert!((290..=300).contains(&retry));
            let body = response.bytes().await.unwrap();
            assert_eq!(body, expected);
            let text = String::from_utf8_lossy(&body).to_lowercase();
            assert!(!text.contains("credit") && !text.contains("billing"));
        }
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        for (request, id) in requests.iter().zip(&ids) {
            let account = h.store.load(id).unwrap();
            assert_eq!(
                request.headers["authorization"],
                format!("Bearer {}", h.store.tokens(&account).unwrap().access_token)
            );
        }
        assert_ne!(
            requests[0].headers["authorization"],
            requests[1].headers["authorization"]
        );
        for (id, seconds) in ids.iter().zip([600, 300]) {
            let reset = chrono::DateTime::from_timestamp(now + seconds, 0).unwrap();
            c6_4_assert_cooling(
                &h.store.load(id).unwrap(),
                HealthReason::Quota,
                reset,
                reset,
            );
        }
    }
}

#[tokio::test]
async fn c6_row_1_success_preserves_body_and_updates_headers() {
    let h = Harness::new(Provider::Claude, 1, 384, 1024).await;
    let mut response = ok();
    response.headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.25".parse().unwrap(),
    );
    h.fake.state.push(response);
    assert_eq!(
        h.send(Provider::Claude, r#"{"model":"fixture","messages":[]}"#)
            .await
            .status(),
        200
    );
    assert_eq!(
        h.state.accounts.views().await.unwrap()[0]["windows"][0]["used_pct"],
        25.0
    );
}
#[tokio::test]
async fn c6_row_2_usage_limit_fails_over_and_cools_before_selection() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let ids = h.ids();
    h.fake.state.push(quota(Utc::now().timestamp() + 600));
    h.fake.state.push(ok());
    let raw = r#"{"input":"conversation-marker","encrypted_content":"verbatim"}"#;
    let response = h.send(Provider::Codex, raw).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-quota-baton-account"], ids[1]);
    response.bytes().await.unwrap();
    assert!(matches!(
        h.store.load(&ids[0]).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|r| r.body == raw));
    let rows = h.ledger();
    let row = rows.iter().find(|r| r["attempts"] == 2).unwrap();
    assert_eq!(row["attempt_details"][0]["failover_reason"], "quota");
    let change = rows
        .iter()
        .position(|r| r["event"] == "health_changed")
        .unwrap();
    let finish = rows.iter().position(|r| r["attempts"] == 2).unwrap();
    assert!(change < finish);
}
#[tokio::test]
async fn c6_row_3_rate_limit_retries_in_place_without_failover() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    let mut r = ScriptedResponse::json(
        StatusCode::TOO_MANY_REQUESTS,
        "{\"error\":{\"type\":\"rate_limit_error\"}}",
    );
    r.headers.insert("retry-after", "1".parse().unwrap());
    h.fake.state.push(r);
    h.fake.state.push(ok());
    let response = h.send(Provider::Claude, "{}").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.fake.state.requests().len(), 2);
    let row = h
        .ledger()
        .into_iter()
        .find(|row| row["rate_retries"] == 1)
        .expect("in-place retry is ledgered");
    assert_eq!(row["outcome"], "rate_hold_ok");
}

#[tokio::test]
async fn c6_3_http_rate_429_retries_in_place_same_account() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    let mut limited = ScriptedResponse::json(
        StatusCode::TOO_MANY_REQUESTS,
        "{\"error\":{\"type\":\"rate_limit_error\"}}",
    );
    limited.headers.insert("retry-after", "1".parse().unwrap());
    h.fake.state.push(limited);
    h.fake.state.push(ok());
    let response = h.send(Provider::Claude, "{}").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.fake.state.requests().len(), 2);
    let row = h
        .ledger()
        .into_iter()
        .find(|row| row["rate_retries"] == 1)
        .unwrap();
    assert_eq!(row["outcome"], "rate_hold_ok");
}

#[tokio::test]
async fn c6_3_http_rate_429_budget_exhausted_relays_with_retry_after() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    for _ in 0..4 {
        let mut limited = ScriptedResponse::json(
            StatusCode::TOO_MANY_REQUESTS,
            "{\"error\":{\"type\":\"rate_limit_error\"}}",
        );
        limited.headers.insert("retry-after", "1".parse().unwrap());
        h.fake.state.push(limited);
    }
    let response = h.send(Provider::Claude, "{}").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!((1..=60).contains(&retry_after));
    assert_eq!(h.fake.state.requests().len(), 4);
    assert!(
        h.ledger()
            .iter()
            .any(|row| row["outcome"] == "rate_hold_relayed_429")
    );

    drop(h);
    let h = Harness::new(Provider::Claude, 1, 384, 1024).await;
    for _ in 0..4 {
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::TOO_MANY_REQUESTS,
            "{\"error\":{\"type\":\"rate_limit_error\"}}",
        ));
    }
    let response = h.send(Provider::Claude, "{}").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!((1..=60).contains(&retry_after));
    let remaining = h
        .state
        .accounts
        .pacing_snapshot(&h.ids()[0])
        .rate_backoff_until
        .unwrap()
        .saturating_duration_since(Instant::now());
    let gate = (remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0)).clamp(1, 60);
    assert!(
        retry_after >= gate,
        "retry-after {retry_after} was earlier than remaining pacing gate {gate}"
    );
}

#[tokio::test]
async fn c6_3_http_rate_429_no_failover_even_with_long_retry_after() {
    for value in [61_u64, 3600] {
        let h = Harness::new(Provider::Claude, 2, 0, 1024).await;
        h.fake.state.push(ok());
        let warm = h.send_with_session(Provider::Claude, "long-rate").await;
        assert_eq!(warm.status(), StatusCode::OK);
        let account = warm.headers()["x-quota-baton-account"]
            .to_str()
            .unwrap()
            .to_owned();
        warm.bytes().await.unwrap();
        h.state.accounts.set_rate_hold_secs(1);
        let mut limited = ScriptedResponse::json(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"type":"rate_limit_error"}}"#,
        );
        limited
            .headers
            .insert("retry-after", value.to_string().parse().unwrap());
        h.fake.state.push(limited);
        let response = h.send_with_session(Provider::Claude, "long-rate").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["x-quota-baton-account"], account);
        assert_eq!(response.headers()["retry-after"], value.to_string());
        response.bytes().await.unwrap();
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers["authorization"],
            requests[1].headers["authorization"]
        );
    }
}

#[tokio::test]
async fn c6_3_pacing_step_grows_under_concurrent_success() {
    let h = Harness::new(Provider::Claude, 1, 384, 1024).await;
    let id = h.ids()[0].clone();
    let start = Instant::now();
    h.state
        .accounts
        .observe_429_at(&id, start, start, None, 1.0);
    assert_eq!(h.state.accounts.pacing_snapshot(&id).rate_step, 1);
    h.fake.state.push(ok());
    assert_eq!(
        h.send(Provider::Claude, "{}").await.status(),
        StatusCode::OK
    );
    let second = start + Duration::from_secs(3);
    h.state
        .accounts
        .observe_429_at(&id, second, second, None, 1.0);
    assert_eq!(h.state.accounts.pacing_snapshot(&id).rate_step, 2);
}

#[tokio::test]
async fn c6_3_release_interval_adapts_after_429_window() {
    let h = Harness::new(Provider::Claude, 1, 384, 1024).await;
    let id = h.ids()[0].clone();
    let start = Instant::now();
    h.state
        .accounts
        .observe_429_at(&id, start, start, None, 1.0);
    assert_eq!(
        h.state.accounts.pacing_snapshot(&id).rate_release_interval,
        Duration::from_millis(250)
    );
    let second = start + Duration::from_secs(3);
    h.state
        .accounts
        .observe_429_at(&id, second, second, None, 1.0);
    assert_eq!(
        h.state.accounts.pacing_snapshot(&id).rate_release_interval,
        Duration::from_millis(500)
    );
    let mut now = second;
    for expected in [1000, 2000, 4000, 5000] {
        now = h
            .state
            .accounts
            .pacing_snapshot_at(&id, now)
            .rate_backoff_until
            .unwrap()
            + Duration::from_secs(1);
        h.state.accounts.observe_429_at(&id, now, now, None, 1.0);
        assert_eq!(
            h.state.accounts.pacing_snapshot(&id).rate_release_interval,
            Duration::from_millis(expected)
        );
    }
    let quiet = now + Duration::from_secs(61);
    assert_eq!(
        h.state
            .accounts
            .pacing_snapshot_at(&id, quiet)
            .rate_release_interval,
        Duration::from_millis(3750)
    );
    let floor = quiet + Duration::from_secs(20 * 60);
    assert_eq!(
        h.state
            .accounts
            .pacing_snapshot_at(&id, floor)
            .rate_release_interval,
        Duration::from_millis(250)
    );
}

#[tokio::test]
async fn c6_3_admission_wait_exhausted_sends_once() {
    let h = Harness::new(Provider::Claude, 1, 0, 1024).await;
    let id = h.ids()[0].clone();
    h.state.accounts.set_rate_hold_secs(1);
    h.state
        .accounts
        .observe_429(&id, Some(Duration::from_secs(60)));
    h.fake.state.push(ok());
    let response = h.send(Provider::Claude, "{}").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn r8_forced_initial_send_keeps_genuine_quota_recovery_independent() {
    for provider in [Provider::Claude, Provider::Codex] {
        let h = Harness::new_with_routing(
            provider,
            2,
            384,
            1024,
            RoutingConfig {
                rate_hold_secs: 1,
                ..RoutingConfig::default()
            },
        )
        .await;
        let ids = h.ids();
        let key = format!("{}:forced-quota", provider.as_str());
        h.state
            .bindings
            .import(json!({"schema":1,"bindings":[{
                "key":key,"account":ids[0],"last_used":Utc::now()
            }]}))
            .unwrap();
        h.state
            .accounts
            .observe_429(&ids[0], Some(Duration::from_secs(30)));
        h.fake.state.push(quota(Utc::now().timestamp() + 3600));
        h.fake.state.push(ok());
        let started = Instant::now();
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            h.send_with_session(provider, "forced-quota"),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-quota-baton-account"], ids[1]);
        response.bytes().await.unwrap();
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].received_at.duration_since(started) >= Duration::from_millis(900));
        for (request, id) in requests.iter().zip(&ids) {
            let account = h.store.load(id).unwrap();
            let tokens = h.store.tokens(&account).unwrap();
            assert_eq!(
                request.headers["authorization"],
                format!("Bearer {}", tokens.access_token.as_str())
            );
            assert_eq!(request.body, "{}");
        }
        assert!(matches!(
            h.store.load(&ids[0]).unwrap().health,
            Health::Cooling {
                reason: HealthReason::Quota,
                ..
            }
        ));
        assert_eq!(h.state.bindings.current(Some(&key)), Some(ids[1].clone()));
        let rows = h.ledger();
        let row = rows.iter().find(|row| row["attempts"] == 2).unwrap();
        assert_eq!(row["attempt_details"][0]["failover_reason"], "quota");
        assert_eq!(row["rate_retries"], 0);
        assert_eq!(row["relayed_rate_429s"], 0);
    }
}

#[tokio::test]
async fn r8_forced_initial_send_keeps_current_token_retry_independent() {
    let h = Harness::new_with_routing(
        Provider::Codex,
        1,
        384,
        1024,
        RoutingConfig {
            rate_hold_secs: 1,
            ..RoutingConfig::default()
        },
    )
    .await;
    let id = h.ids().remove(0);
    let mut account = h.store.load(&id).unwrap();
    let old = h.store.tokens(&account).unwrap();
    account.source = Source::Oauth;
    h.store
        .save(&account, &old, Some(account.generation))
        .unwrap();
    h.state
        .accounts
        .observe_429(&id, Some(Duration::from_secs(30)));
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::UNAUTHORIZED, "{}"));
    h.fake.state.push(ScriptedResponse::json(StatusCode::OK, r#"{"access_token":"synthetic-r8-refreshed","refresh_token":"synthetic-r8-rotated","expires_in":3600}"#));
    h.fake.state.push(ok());
    let response = tokio::time::timeout(Duration::from_secs(5), h.send(Provider::Codex, "{}"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-quota-baton-account"], id);
    response.bytes().await.unwrap();
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].headers["authorization"],
        format!("Bearer {}", old.access_token.as_str())
    );
    assert!(String::from_utf8_lossy(&requests[1].body).contains("refresh_token"));
    assert_eq!(
        requests[2].headers["authorization"],
        "Bearer synthetic-r8-refreshed"
    );
    assert_eq!(
        requests[0].headers["chatgpt-account-id"],
        requests[2].headers["chatgpt-account-id"]
    );
    assert_eq!(h.store.load(&id).unwrap().health, Health::Ready);
    let rows = h.ledger();
    let row = rows.iter().find(|row| row["attempts"] == 2).unwrap();
    assert_eq!(row["attempt_details"][0]["failover_reason"], "token_retry");
    assert_eq!(row["rate_retries"], 0);
}

#[tokio::test]
async fn c6_3_no_pacing_no_spacing() {
    let h = Harness::new(Provider::Claude, 1, 0, 1024).await;
    let start = h.fake.state.requests().len();
    let harness = &h;
    let responses = futures_util::future::join_all((0..32).map(|n| {
        let session = format!("unpaced-{n}");
        async move { harness.send_with_session(Provider::Claude, &session).await }
    }))
    .await;
    assert!(
        responses
            .iter()
            .all(|response| response.status().is_success())
    );
    let requests = h.fake.state.requests();
    let arrivals: Vec<_> = requests[start..]
        .iter()
        .map(|request| request.received_at)
        .collect();
    assert_eq!(arrivals.len(), 32);
    let span = arrivals
        .iter()
        .copied()
        .max()
        .unwrap()
        .saturating_duration_since(arrivals.iter().copied().min().unwrap());
    // 250 ms spacing would spread 32 requests over about 7.75 s; 2 s keeps
    // that detectable while tolerating a loaded machine.
    assert!(span < Duration::from_secs(2), "unpaced span was {span:?}");
}

#[tokio::test]
async fn c6_3_spacing_stops_after_decay() {
    let h = Harness::new(Provider::Claude, 1, 0, 1024).await;
    let id = h.ids()[0].clone();
    let now = Instant::now();
    h.state.accounts.observe_429_at(&id, now, now, None, 1.0);
    let decayed = h
        .state
        .accounts
        .pacing_snapshot_at(&id, now + Duration::from_secs(301));
    assert_eq!(decayed.rate_step, 0);
    assert!(decayed.rate_backoff_until.is_none());

    let start = h.fake.state.requests().len();
    let harness = &h;
    futures_util::future::join_all((0..32).map(|n| {
        let session = format!("decayed-{n}");
        async move { harness.send_with_session(Provider::Claude, &session).await }
    }))
    .await;
    let requests = h.fake.state.requests();
    let arrivals: Vec<_> = requests[start..]
        .iter()
        .map(|request| request.received_at)
        .collect();
    let span = arrivals
        .iter()
        .copied()
        .max()
        .unwrap()
        .saturating_duration_since(arrivals.iter().copied().min().unwrap());
    // 250 ms spacing would spread 32 requests over about 7.75 s; 2 s keeps
    // that detectable while tolerating a loaded machine.
    assert!(span < Duration::from_secs(2), "decayed span was {span:?}");
}

#[tokio::test]
async fn c6_3_placement_skips_pacing_account_but_binding_stays() {
    let h = Harness::new(Provider::Claude, 2, 0, 1024).await;
    let ids = h.ids();
    let id = ids[0].clone();
    for _ in 0..5 {
        h.fake.state.push(ok());
    }
    let first = h.send_with_session(Provider::Claude, "s1").await;
    assert_eq!(first.headers()["x-quota-baton-account"], id);
    h.state
        .accounts
        .observe_429(&id, Some(Duration::from_secs(1)));
    let bound = h.send_with_session(Provider::Claude, "s1").await;
    assert_eq!(bound.headers()["x-quota-baton-account"], id);
    let placed_elsewhere = h.send_with_session(Provider::Claude, "s2").await;
    assert_eq!(placed_elsewhere.headers()["x-quota-baton-account"], ids[1]);
    let decayed_at = Instant::now() + Duration::from_secs(61);
    let decayed = h.state.accounts.pacing_snapshot_at(&id, decayed_at);
    assert_eq!(decayed.rate_step, 0);
    assert!(decayed.rate_backoff_until.is_none());
    assert_eq!(decayed.rate_release_interval, Duration::from_millis(250));
    let _ = h.send_with_session(Provider::Claude, "s3").await;
    let still_bound = h.send_with_session(Provider::Claude, "s1").await;
    assert_eq!(still_bound.headers()["x-quota-baton-account"], id);
}

#[tokio::test]
async fn c6_3_retained_copy_budget_exhausted_falls_back_to_relay() {
    let h = Harness::new(Provider::Claude, 1, 0, 1024).await;
    let mut limited = ScriptedResponse::json(
        StatusCode::TOO_MANY_REQUESTS,
        "{\"error\":{\"type\":\"rate_limit_error\"}}",
    );
    limited.headers.insert("retry-after", "1".parse().unwrap());
    h.fake.state.push(limited);
    let response = h.send(Provider::Claude, "{}").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn c6_3_storm_simulation() {
    let routing = RoutingConfig {
        rate_hold_secs: 60,
        ..RoutingConfig::default()
    };
    let h = Harness::new_with_routing(Provider::Codex, 2, 384, 1024, routing).await;
    let ids = h.ids();
    h.state
        .accounts
        .change(&ids[1], Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    for n in 0..16 {
        let response = h
            .send_with_session(Provider::Codex, &format!("storm-session-{n}"))
            .await;
        assert_eq!(response.headers()["x-quota-baton-account"], ids[0]);
    }
    h.state
        .accounts
        .change(&ids[1], Some(UserState::Active), None, None, None)
        .await
        .unwrap();
    h.fake
        .state
        .enable_window_limiter(8, Duration::from_secs(1));
    // Let all 16 actual unpaced upstream requests arrive before the first
    // limiter decision. Otherwise early 429 feedback paces late arrivals and
    // makes the first-round baseline depend on client/IOCP scheduling.
    h.fake.state.gate_first_limiter_batch(16);
    let before = h.fake.state.requests().len();
    // Keep independent sessions producing work rather than inserting a global
    // quiet gap after every batch. Per-request tags attribute retries to their
    // originating round even while the three rounds overlap.
    for _ in 0..3 {
        h.fake.state.begin_limiter_round();
    }
    let harness = &h;
    let expected = ids[0].as_str();
    let batches = futures_util::future::join_all((0..16).map(|n| async move {
        let mut completed = Vec::new();
        for round in 0..3 {
            let response = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(75))
                .build()
                .unwrap()
                .post(format!("http://{}/v1/responses", harness.servers.data_addr))
                .bearer_auth("synthetic-client")
                .header("session-id", format!("storm-session-{n}"))
                .header("user-agent", format!("qb-storm-round/{round}"))
                .body("{}")
                .send()
                .await
                .unwrap();
            let status = response.status();
            assert_eq!(response.headers()["x-quota-baton-account"], expected);
            completed.push((status, response.bytes().await.unwrap()));
        }
        completed
    }))
    .await;
    let responses_with_bodies: Vec<_> = batches.into_iter().flatten().collect();
    let round_counts = h.fake.state.limiter_round_429_counts();
    eprintln!(
        "continuous storm counts={round_counts:?}, pacing={:?}",
        h.state.accounts.pacing_snapshot(&ids[0])
    );
    assert_eq!(round_counts.len(), 3);
    assert!(
        // All 16 initial arrivals share the real 8/s limiter: at least eight
        // genuine rejections, with retries and later rounds counted normally.
        round_counts[0] >= 8,
        "round one must exercise the limiter: {round_counts:?}"
    );
    assert!(
        round_counts[2] < round_counts[0],
        "round counts: {round_counts:?}"
    );
    let rate_body = Bytes::from_static(b"{\"detail\":\"Rate limit exceeded\"}");
    for (status, body) in responses_with_bodies {
        if status == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(body, rate_body);
        } else {
            assert_eq!(status, StatusCode::OK);
        }
    }
    let requests = h.fake.state.requests();
    let storm_requests = &requests[before..];
    eprintln!(
        "synthetic storm upstream requests: {}",
        storm_requests.len()
    );
    assert!(
        storm_requests
            .iter()
            .all(|request| { request.headers["authorization"] == "Bearer synthetic-access-0" })
    );
    assert!(storm_requests.len() <= 16 * 3 * 4);
    let ledger = h.ledger();
    assert!(ledger.iter().any(|row| row["outcome"] == "rate_hold_ok"));
    assert!(ledger.iter().any(|row| {
        row["outcome"] == "rate_hold_ok"
            && row["rate_wait_ms"].as_u64().is_some_and(|value| value > 0)
    }));
    eprintln!("synthetic limiter 429 counts by round: {round_counts:?}");
}

async fn confirm_relay_target(h: &Harness, source: &str, target: &str) {
    h.state
        .accounts
        .change(source, None, None, Some(true), None)
        .await
        .unwrap();
    let mut success = ok();
    success.headers = tray2_headers(10, (Utc::now() + ChronoDuration::hours(24)).timestamp());
    h.fake.state.push(success);
    let response = h
        .send_with_session(Provider::Codex, "independent-target-proof")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-quota-baton-account"], target);
    response.bytes().await.unwrap();
    h.state
        .accounts
        .change(source, None, None, Some(false), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn c7_1_rebalance_relay_never_ledgers_a_disabled_target() {
    let h = Harness::new(Provider::Codex, 3, 384, 1024).await;
    let ids = h.ids();
    // The disabled id sorts before both the source and the valid target.
    for index in [0, 2] {
        h.state
            .accounts
            .change(&ids[index], Some(UserState::Disabled), None, None, None)
            .await
            .unwrap();
    }
    for n in 0..6 {
        let response = h
            .send_with_session(Provider::Codex, &format!("r1-mark-{n}"))
            .await;
        assert_eq!(response.headers()["x-quota-baton-account"], ids[1]);
        response.bytes().await.unwrap();
    }
    h.state
        .accounts
        .change(&ids[2], Some(UserState::Active), None, None, None)
        .await
        .unwrap();
    confirm_relay_target(&h, &ids[1], &ids[2]).await;
    h.state.accounts.set_rate_hold_secs(1);
    h.fake
        .state
        .enable_window_limiter(0, Duration::from_secs(60));
    for n in 0..3 {
        let response = h
            .send_with_session(Provider::Codex, &format!("r1-mark-{n}"))
            .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        response.bytes().await.unwrap();
    }
    assert!((0..6).any(|n| {
        h.state
            .bindings
            .overload_marked(Some(&format!("codex:r1-mark-{n}")), Utc::now())
    }));
    for n in 0..6 {
        let response = h
            .send_with_session(Provider::Codex, &format!("r1-mark-{n}"))
            .await;
        assert_ne!(response.headers()["x-quota-baton-account"], ids[0]);
        response.bytes().await.unwrap();
    }
    let ledger = h.ledger();
    let moves = ledger
        .iter()
        .filter(|row| row["event"] == "binding_move" && row["reason"] == "overload")
        .collect::<Vec<_>>();
    assert!(!moves.is_empty());
    assert!(
        moves
            .iter()
            .all(|row| row["from"] == ids[1] && row["to"] == ids[2])
    );
    assert_eq!(h.state.bindings.active_sessions(&ids[0], Utc::now()), 0);
}

#[tokio::test]
async fn c7_1_storm_through_relay_concentrated_then_spread() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let ids = h.ids();
    h.state
        .accounts
        .change(&ids[1], Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    // Fill six distinct groups on A while B is unavailable, then make B an
    // eligible target for the overload trigger.
    for n in 0..6 {
        let response = h
            .send_with_session(Provider::Codex, &format!("c7-storm-{n}"))
            .await;
        assert_eq!(response.headers()["x-quota-baton-account"], ids[0]);
    }
    h.state
        .accounts
        .change(&ids[1], Some(UserState::Active), None, None, None)
        .await
        .unwrap();
    confirm_relay_target(&h, &ids[0], &ids[1]).await;
    h.state.accounts.set_rate_hold_secs(1);
    h.fake
        .state
        .enable_window_limiter(0, Duration::from_secs(60));
    let mut rate_responses = Vec::new();
    for n in 0..3 {
        let response = h
            .send_with_session(Provider::Codex, &format!("c7-storm-{n}"))
            .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["x-quota-baton-account"], ids[0]);
        rate_responses.push(response);
    }
    // A marked group moves only at the next request boundary; the 429s above
    // must all remain on A.
    let mut moved = None;
    for n in 0..6 {
        let response = h
            .send_with_session(Provider::Codex, &format!("c7-storm-{n}"))
            .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        if response.headers()["x-quota-baton-account"] == ids[1] {
            moved = Some(n);
            break;
        }
    }
    assert!(
        moved.is_some(),
        "a marked session must move to the second account"
    );
    drop(rate_responses);
    let ledger = h.ledger();
    assert!(
        ledger
            .iter()
            .any(|row| row["event"] == "overload_detected"
                && row["marked"].as_u64().unwrap_or(0) > 0)
    );
    assert!(ledger.iter().any(|row| row["event"] == "binding_move"
        && row["reason"] == "overload"
        && row["from"] == ids[0]
        && row["to"] == ids[1]));
    assert!(h.state.bindings.active_sessions(&ids[0], Utc::now()) <= 6);
}
#[tokio::test]
async fn c6_row_4_linked_revocation_cools_link_stale_and_fails_over() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"token_revoked","message":"conversation-marker"}}"#,
    ));
    h.fake.state.push(ok());
    assert_eq!(h.send(Provider::Codex, "{}").await.status(), 200);
    assert!(matches!(
        h.store.load(&h.ids()[0]).unwrap().health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
}
#[tokio::test]
async fn c6_row_4_owned_revocation_is_terminal_and_fails_over() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let id = h.ids().remove(0);
    let mut account = h.store.load(&id).unwrap();
    let tokens = h.store.tokens(&account).unwrap();
    account.source = qb_core::accounts::Source::Oauth;
    h.store
        .save(&account, &tokens, Some(account.generation))
        .unwrap();
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"token_revoked"}}"#,
    ));
    h.fake.state.push(ok());
    let response = h.send(Provider::Codex, "{}").await;
    assert_eq!(response.status(), 200);
    assert_ne!(response.headers()["x-quota-baton-account"], id);
    assert!(matches!(
        h.store.load(&id).unwrap().health,
        Health::NeedsRelogin { .. }
    ));
    assert_eq!(h.fake.state.requests().len(), 2);
}

#[tokio::test]
async fn m5_1_linked_revoked_401_reloads_newer_cpa_and_retries_same_account() {
    let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
    let id = h.ids().remove(0);
    let account = h.store.load(&id).unwrap();
    let old_token = h.store.tokens(&account).unwrap().access_token.clone();
    let qb_core::accounts::Source::Cpa { dir, .. } = account.source else {
        panic!("expected linked fixture")
    };
    let mut revoked = ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"token_revoked"}}"#,
    );
    revoked.chunks[0].0 = Duration::from_millis(500);
    h.fake.state.push(revoked);
    h.fake.state.push(ok());
    let raw = r#"{"input":"conversation-marker","encrypted_content":"verbatim"}"#;
    let (response, _) = tokio::join!(h.send(Provider::Codex, raw), async {
        // First-use reload must see the old CPA token; rotate only once it is in flight.
        tokio::time::timeout(Duration::from_secs(5), async {
            while h.fake.state.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let path = dir.join("0.json");
        let mut fixture: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        fixture["access_token"] = "synthetic-cpa-rotated".into();
        fixture["last_refresh"] = "2026-10-13T00:00:02Z".into();
        fs::write(path, serde_json::to_vec(&fixture).unwrap()).unwrap();
    });
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-quota-baton-account"], id);
    response.bytes().await.unwrap();
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].headers["authorization"],
        format!("Bearer {old_token}")
    );
    assert_eq!(
        requests[1].headers["authorization"],
        "Bearer synthetic-cpa-rotated"
    );
    assert!(requests.iter().all(|request| request.body == raw));
    let updated = h.store.load(&id).unwrap();
    assert_eq!(updated.health, Health::Ready);
    assert_eq!(updated.auth_failures, 0);
    assert_eq!(
        h.store.tokens(&updated).unwrap().access_token,
        "synthetic-cpa-rotated"
    );
}
#[tokio::test]
async fn c6_row_5_linked_401_without_newer_token_fails_over() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::UNAUTHORIZED, "{}"));
    h.fake.state.push(ok());
    assert_eq!(h.send(Provider::Codex, "{}").await.status(), 200);
    assert!(matches!(
        h.store.load(&h.ids()[0]).unwrap().health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
}
#[tokio::test]
async fn c6_row_6_unknown_model_stops_after_second_403() {
    let h = Harness::new(Provider::Codex, 3, 384, 1024).await;
    for _ in 0..3 {
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::FORBIDDEN,
            "{\"error\":{\"type\":\"permission_error\"}}",
        ));
    }
    assert_eq!(h.send(Provider::Codex, "{}").await.status(), 403);
    assert_eq!(h.fake.state.requests().len(), 2);
    assert!(
        h.store
            .list()
            .unwrap()
            .iter()
            .all(|a| a.health == Health::Ready)
    );
}
#[tokio::test]
async fn c6_row_6_claude_organization_denial_cools_whole_account() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    h.fake.state.push(ScriptedResponse::json(StatusCode::FORBIDDEN,r#"{"error":{"type":"permission_error","message":"OAuth is not allowed for this organization"}}"#));
    h.fake.state.push(ok());
    assert_eq!(
        h.send(Provider::Claude, r#"{"model":"fixture"}"#)
            .await
            .status(),
        200
    );
    assert!(
        matches!(h.store.load(&h.ids()[0]).unwrap().health,Health::Cooling{reason:HealthReason::Permission,scope,..}if scope=="account")
    );
}
#[tokio::test]
async fn c6_row_6a_model_plan_denial_scopes_then_allows_other_model() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    h.fake.state.push(ScriptedResponse::json(StatusCode::BAD_REQUEST,r#"{"error":{"message":"The 'fixture-pro' model is not supported when using Codex with a ChatGPT account"}}"#));
    h.fake.state.push(ok());
    assert_eq!(h.send(Provider::Codex, "{}").await.status(), 200);
    let a = h.store.load(&h.ids()[0]).unwrap();
    assert!(a.eligible(Some("fixture-other")));
    assert!(!a.eligible(Some("fixture-pro")));
}
#[tokio::test]
async fn c6_row_7_payment_fails_over_and_relays_last_when_none() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    for _ in 0..2 {
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::PAYMENT_REQUIRED,
            "{\"error\":{\"type\":\"payment_required\"}}",
        ));
    }
    assert_eq!(h.send(Provider::Codex, "{}").await.status(), 402);
    assert!(h.store.list().unwrap().iter().all(|a| matches!(
        a.health,
        Health::Cooling {
            reason: HealthReason::Payment,
            ..
        }
    )));
}
#[tokio::test]
async fn c6_row_8_9_10_plain_nonfailover_statuses_preserve_raw_response() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    for status in [400, 404, 409, 413, 422, 408, 500, 502, 507, 529] {
        let code = StatusCode::from_u16(status).unwrap();
        h.fake.state.push(ScriptedResponse::json(
            code,
            r#"{"error":{"code":"plain_error","message":"conversation-marker"}}"#,
        ));
        let response = h.send(Provider::Codex, "{}").await;
        assert_eq!(response.status(), code);
        assert!(
            response
                .text()
                .await
                .unwrap()
                .contains("conversation-marker")
        );
    }
    assert_eq!(h.fake.state.requests().len(), 10);
    assert!(
        h.store
            .list()
            .unwrap()
            .iter()
            .all(|a| a.health == Health::Ready)
    );
}
#[tokio::test]
async fn c6_row_11_network_retries_once_in_place_then_502() {
    let h = Harness::new_with_network(Provider::Codex, 2, 384, 1024, true).await;
    let start = std::time::Instant::now();
    assert_eq!(h.send(Provider::Codex, "{}").await.status(), 502);
    assert!(start.elapsed() >= Duration::from_millis(250));
    let rows = h.ledger();
    let row = rows.iter().find(|r| r["attempts"] == 2).unwrap();
    assert_eq!(
        row["attempt_details"][0]["account"],
        row["attempt_details"][1]["account"]
    );
    assert!(
        h.store
            .list()
            .unwrap()
            .iter()
            .all(|a| a.health == Health::Ready)
    );
}
#[tokio::test]
async fn c6_row_12a_precommit_error_replays_but_row_12_postcommit_does_not() {
    let h = Harness::new(Provider::Codex, 3, 384, 1024).await;
    let error=Bytes::from_static(b"event: error\ndata: {\"type\":\"error\",\"status\":429,\"error\":{\"type\":\"usage_limit_reached\",\"resets_in_seconds\":600}}\n\n");
    h.fake
        .state
        .push(ScriptedResponse::sse(vec![(Duration::ZERO, error.clone())]));
    h.fake.state.push(ok());
    assert_eq!(h.send(Provider::Codex, "{}").await.status(), 200);
    assert_eq!(h.fake.state.requests().len(), 2);
    let first = Bytes::from_static(b"data: {\"type\":\"response.created\"}\n\n");
    h.fake.state.push(ScriptedResponse::sse(vec![
        (Duration::ZERO, first.clone()),
        (Duration::from_millis(10), error.clone()),
    ]));
    let body = h.send(Provider::Codex, "{}").await.bytes().await.unwrap();
    assert_eq!(body, [first.as_ref(), error.as_ref()].concat());
    assert_eq!(h.fake.state.requests().len(), 3);
    assert_eq!(
        h.store
            .list()
            .unwrap()
            .iter()
            .filter(|a| matches!(
                a.health,
                Health::Cooling {
                    reason: HealthReason::Quota,
                    ..
                }
            ))
            .count(),
        2
    );
}
#[tokio::test]
async fn c6_row_14_replays_earliest_genuine_compressed_limit_with_retry_after() {
    use std::io::Write;
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let now = Utc::now().timestamp();
    let mut bodies = Vec::new();
    for seconds in [600, 300] {
        let raw=json!({"error":{"type":"usage_limit_reached","resets_at":now+seconds,"message":"conversation-marker"}}).to_string();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(raw.as_bytes()).unwrap();
        let body = encoder.finish().unwrap();
        let mut r = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, body.clone());
        r.headers
            .insert("content-encoding", "gzip".parse().unwrap());
        h.fake.state.push(r);
        bodies.push(body);
    }
    let response = h.send(Provider::Codex, "{}").await;
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["content-encoding"], "gzip");
    let retry: i64 = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((295..=300).contains(&retry));
    assert_eq!(response.bytes().await.unwrap(), bodies[1]);
    let again = h.send(Provider::Codex, "{}").await;
    assert_eq!(again.status(), 429);
    assert_eq!(again.bytes().await.unwrap(), bodies[1]);
    assert_eq!(h.fake.state.requests().len(), 2);
}
#[tokio::test]
async fn c6_row_15_paused_disabled_and_needs_relogin_are_unusable() {
    let h = Harness::new(Provider::Codex, 3, 384, 1024).await;
    let ids = h.ids();
    h.store
        .update_metadata(&ids[0], |a| a.user_state = UserState::Paused)
        .unwrap();
    h.store
        .update_metadata(&ids[1], |a| a.user_state = UserState::Disabled)
        .unwrap();
    h.store
        .update_metadata(&ids[2], |a| {
            a.health = Health::NeedsRelogin {
                reason: "token_revoked".into(),
                since: Utc::now(),
            }
        })
        .unwrap();
    let response = h.send(Provider::Codex, "{}").await;
    assert_eq!(response.status(), 400);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("no usable codex account")
    );
    assert!(h.fake.state.requests().is_empty());
}
#[tokio::test]
async fn c6_row_16_memory_guard_rejects_before_dial() {
    let h = Harness::new(Provider::Codex, 1, 384, 0).await;
    let response = h.send(Provider::Codex, "{}").await;
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["retry-after"], "5");
    assert!(h.fake.state.requests().is_empty());
}
#[tokio::test]
async fn c6_row_17_budget_without_copy_relays_first_limit() {
    let h = Harness::new(Provider::Codex, 2, 0, 1024).await;
    let r = quota(Utc::now().timestamp() + 600);
    let expected = r.chunks[0].1.clone();
    h.fake.state.push(r);
    let response = h.send(Provider::Codex, "{}").await;
    assert_eq!(response.status(), 429);
    assert_eq!(response.bytes().await.unwrap(), expected);
    assert_eq!(h.fake.state.requests().len(), 1);
}
#[tokio::test]
async fn c5_passive_header_exhaustion_credits_and_early_reset() {
    let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
    let id = &h.ids()[0];
    let mut headers = HeaderMap::new();
    headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
    headers.insert("x-codex-primary-used-percent", "101".parse().unwrap());
    headers.insert(
        "x-codex-primary-reset-at",
        (Utc::now() + ChronoDuration::hours(1))
            .timestamp()
            .to_string()
            .parse()
            .unwrap(),
    );
    h.state
        .accounts
        .observe_headers(id, Provider::Codex, &headers)
        .await;
    assert!(matches!(
        h.store.load(id).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    headers.insert("x-codex-primary-used-percent", "10".parse().unwrap());
    headers.insert(
        "x-codex-primary-reset-at",
        (Utc::now() + ChronoDuration::hours(5))
            .timestamp()
            .to_string()
            .parse()
            .unwrap(),
    );
    h.state
        .accounts
        .observe_headers(id, Provider::Codex, &headers)
        .await;
    assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
    let _inflight = h.state.accounts.track_inflight(id);
    headers.insert("x-codex-primary-used-percent", "98".parse().unwrap());
    h.state
        .accounts
        .observe_headers(id, Provider::Codex, &headers)
        .await;
    assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
    headers.insert("x-codex-credits-has-credits", "true".parse().unwrap());
    headers.insert("x-codex-primary-used-percent", "99".parse().unwrap());
    h.state
        .accounts
        .observe_headers(id, Provider::Codex, &headers)
        .await;
    assert!(matches!(
        h.store.load(id).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    h.state
        .accounts
        .change(id, None, None, None, Some(true))
        .await
        .unwrap();
    h.state
        .accounts
        .observe_headers(id, Provider::Codex, &headers)
        .await;
    assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
}
#[tokio::test]
async fn c11_admin_actions_patch_events_and_next_request_selection() {
    use futures_util::StreamExt;
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}/admin/v1", h.servers.admin_addr);
    let ids = h.ids();
    let events = client
        .get(format!("{base}/events"))
        .bearer_auth("synthetic-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), 200);
    let mut stream = events.bytes_stream();
    for (action, state) in [
        ("pause", UserState::Paused),
        ("resume", UserState::Active),
        ("disable", UserState::Disabled),
        ("enable", UserState::Active),
    ] {
        let response = client
            .post(format!("{base}/accounts/{}/{action}", ids[0]))
            .bearer_auth("synthetic-admin")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(h.store.load(&ids[0]).unwrap().user_state, state);
    }
    client
        .post(format!("{base}/accounts/{}/pause", ids[0]))
        .bearer_auth("synthetic-admin")
        .send()
        .await
        .unwrap();
    h.fake.state.push(ok());
    let response = h.send(Provider::Claude, "{}").await;
    assert_eq!(response.headers()["x-quota-baton-account"], ids[1]);
    let patch = client
        .patch(format!("{base}/accounts/{}", ids[1]))
        .bearer_auth("synthetic-admin")
        .json(&json!({"label":"검증 계정","drain":true,"allow_credits":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(patch.status(), 200);
    let a = h.store.load(&ids[1]).unwrap();
    assert_eq!(a.label, "검증 계정");
    assert!(a.drain && a.allow_credits);
    let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(&event)
            .unwrap()
            .contains("account_changed")
    );
    let view: Value = client
        .get(format!("{base}/state"))
        .bearer_auth("synthetic-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["bindings"]["count"], 0);
    assert_eq!(view["accounts"].as_array().unwrap().len(), 2);
    let text = view.to_string();
    assert!(!text.contains("token_meta"));
    assert!(!text.contains("secret_dpapi"));
}

#[tokio::test]
async fn c11_settings_patch_preserves_config_and_survives_initialization() {
    let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
    let id = &h.ids()[0];
    let text = format!(
        "# preserved header\n[accounts.{id}]\nallow_credits = false # preserved inline\ndrain = false\n[future]\nsetting = 'preserved value'\n"
    );
    fs::write(h.home.join("config.toml"), text).unwrap();
    h.state
        .accounts
        .change(id, None, None, Some(true), Some(true))
        .await
        .unwrap();
    let updated = fs::read_to_string(h.home.join("config.toml")).unwrap();
    assert!(updated.contains("# preserved header"));
    assert!(updated.contains("# preserved inline"));
    assert!(updated.contains("'preserved value'"));
    let config = Config::load(&h.home).unwrap();
    assert!(config.accounts[id].allow_credits && config.accounts[id].drain);
    h.store
        .update_metadata(id, |a| {
            a.drain = false;
            a.allow_credits = false;
        })
        .unwrap();
    h.state
        .accounts
        .initialize_settings(config.accounts)
        .await
        .unwrap();
    let account = h.store.load(id).unwrap();
    assert!(account.drain && account.allow_credits);
}

#[tokio::test]
async fn c5_early_reset_does_not_inherit_previous_rejected_status() {
    let h = Harness::new(Provider::Claude, 1, 384, 1024).await;
    let id = &h.ids()[0];
    let mut headers = HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "1.0".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-5h-status",
        "rejected".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        (Utc::now() + ChronoDuration::hours(1))
            .timestamp()
            .to_string()
            .parse()
            .unwrap(),
    );
    h.state
        .accounts
        .observe_headers(id, Provider::Claude, &headers)
        .await;
    assert!(matches!(
        h.store.load(id).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Quota,
            ..
        }
    ));
    headers.remove("anthropic-ratelimit-unified-5h-status");
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.1".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        (Utc::now() + ChronoDuration::hours(5))
            .timestamp()
            .to_string()
            .parse()
            .unwrap(),
    );
    h.state
        .accounts
        .observe_headers(id, Provider::Claude, &headers)
        .await;
    assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
    h.obs.flush().unwrap();
    let saved: Value =
        serde_json::from_slice(&fs::read(h.home.join("state/quota.json")).unwrap()).unwrap();
    assert!(saved["accounts"][id]["windows"][0]["status"].is_null());
}

#[tokio::test]
async fn c6_row_10_overload_type_never_retries_or_changes_health() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    for (status, kind) in [
        (429, "overloaded_error"),
        (503, "authentication_error"),
        (500, "permission_error"),
    ] {
        let mut response = ScriptedResponse::json(
            StatusCode::from_u16(status).unwrap(),
            json!({"error":{"type":kind}}).to_string(),
        );
        response
            .headers
            .insert("retry-after", "30".parse().unwrap());
        h.fake.state.push(response);
        assert_eq!(h.send(Provider::Claude, "{}").await.status(), status);
    }
    assert_eq!(h.fake.state.requests().len(), 3);
    assert!(
        h.store
            .list()
            .unwrap()
            .iter()
            .all(|a| a.health == Health::Ready)
    );
}

#[tokio::test]
async fn c6_row_14_model_scoped_cooling_respects_request_model() {
    let h = Harness::new(Provider::Claude, 1, 384, 1024).await;
    let id = h.ids().remove(0);
    h.store
        .update_metadata(&id, |a| {
            a.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: Utc::now() + ChronoDuration::minutes(5),
                scope: "model:blocked".into(),
            };
        })
        .unwrap();
    let blocked = h.send(Provider::Claude, r#"{"model":"blocked"}"#).await;
    assert_eq!(blocked.status(), 429);
    assert!(h.fake.state.requests().is_empty());
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{}"));
    let other = h.send(Provider::Claude, r#"{"model":"other"}"#).await;
    assert_eq!(other.status(), 200);
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn c6_row_2_until_uses_exhausted_window_not_unrelated_weekly_reset() {
    for (primary, secondary, seconds) in [(100, 40, 3600), (40, 100, 432000)] {
        // Each limiting-window scenario starts without the other's quota snapshot.
        let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
        let now = Utc::now();
        let mut r = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, "{}");
        for (slot, used, reset, minutes) in [
            ("primary", primary, 3600, 300),
            ("secondary", secondary, 432000, 10080),
        ] {
            r.headers.insert(
                format!("x-codex-{slot}-used-percent")
                    .parse::<axum::http::HeaderName>()
                    .unwrap(),
                used.to_string().parse().unwrap(),
            );
            r.headers.insert(
                format!("x-codex-{slot}-reset-at")
                    .parse::<axum::http::HeaderName>()
                    .unwrap(),
                (now.timestamp() + reset).to_string().parse().unwrap(),
            );
            r.headers.insert(
                format!("x-codex-{slot}-window-minutes")
                    .parse::<axum::http::HeaderName>()
                    .unwrap(),
                minutes.to_string().parse().unwrap(),
            );
        }
        h.fake.state.push(r);
        h.fake.state.push(ok());
        assert_eq!(h.send(Provider::Codex, "{}").await.status(), 200);
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        assert_ne!(
            requests[0].headers.get("chatgpt-account-id").unwrap(),
            requests[1].headers.get("chatgpt-account-id").unwrap()
        );
        let cooled = h
            .store
            .list()
            .unwrap()
            .into_iter()
            .find(|a| matches!(a.health, Health::Cooling { .. }))
            .unwrap();
        assert!(
            matches!(cooled.health,Health::Cooling{until,..} if (until-now).num_seconds().abs_diff(seconds)<=2)
        );
    }
}

#[tokio::test]
async fn c6_row_9_10_limit_header_does_not_override_plain_status() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    for status in [400, 500, 529] {
        let mut r = ScriptedResponse::json(StatusCode::from_u16(status).unwrap(), "{}");
        r.headers
            .insert("x-codex-limit-reached", "true".parse().unwrap());
        h.fake.state.push(r);
        assert_eq!(
            h.send(Provider::Codex, "{}").await.status().as_u16(),
            status
        );
    }
    assert_eq!(h.fake.state.requests().len(), 3);
}

#[tokio::test]
async fn r1_n3_http_date_retry_after_retries_same_account_after_advice() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    let mut limited = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, "{}");
    limited.headers.insert(
        "retry-after",
        (Utc::now() + ChronoDuration::seconds(8))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string()
            .parse()
            .unwrap(),
    );
    h.fake.state.push(limited);
    h.fake.state.push(ok());
    let started = Instant::now();
    assert_eq!(h.send(Provider::Claude, "{}").await.status(), 200);
    // Whole-second dates and pacing jitter leave margin; the old 1s fallback
    // plus the default pacing window would retry well before this threshold.
    assert!(started.elapsed() >= Duration::from_secs(5));
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].headers["authorization"] == requests[1].headers["authorization"]);
    assert_eq!(requests[0].body, requests[1].body);
}

#[tokio::test]
async fn r1_n3_http_date_retry_after_paces_without_retained_body_and_is_verbatim() {
    let h = Harness::new(Provider::Claude, 2, 0, 1024).await;
    let raw = r#"{"error":{"type":"rate_limit_error","message":"synthetic rate limit"}}"#;
    let advice = (Utc::now() + ChronoDuration::seconds(30))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let mut limited = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, raw);
    limited
        .headers
        .insert("retry-after", advice.parse().unwrap());
    h.fake.state.push(limited);
    let response = h.send(Provider::Claude, "{}").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], advice);
    let account = response.headers()["x-quota-baton-account"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(response.text().await.unwrap(), raw);
    let pacing = h.state.accounts.pacing_snapshot(&account);
    let remaining = pacing
        .rate_backoff_until
        .unwrap()
        .saturating_duration_since(Instant::now());
    // 29+ seconds before +/-20% jitter; the integer-only baseline paces ~2s.
    assert!(remaining >= Duration::from_secs(20));
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
}

#[tokio::test]
async fn c6_row_12b_both_codex_codes_and_claude_signature_are_verbatim() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    for code in ["invalid_encrypted_content", "thinking_signature_invalid"] {
        let raw = json!({"error":{"code":code}}).to_string();
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::BAD_REQUEST, raw.clone()));
        assert_eq!(
            h.send(Provider::Codex, "{}").await.text().await.unwrap(),
            raw
        );
    }
    drop(h);
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    let raw = r#"{"error":{"type":"invalid_request_error","message":"Invalid `signature` in `thinking` block"}}"#;
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::BAD_REQUEST, raw));
    assert_eq!(
        h.send(Provider::Claude, "{}").await.text().await.unwrap(),
        raw
    );
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn c5_known_model_http_403_cools_permission_for_one_hour() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::FORBIDDEN, "{}"));
    h.fake.state.push(ok());
    let before = Utc::now();
    assert_eq!(
        h.send(Provider::Claude, r#"{"model":"blocked"}"#)
            .await
            .status(),
        200
    );
    assert!(h.store.list().unwrap().iter().any(|a|matches!(&a.health,Health::Cooling{reason:HealthReason::Permission,until,scope} if scope=="model:blocked" && (*until-before).num_seconds().abs_diff(3600)<=2)));
}

#[tokio::test]
async fn c6_row_5_transient_and_throttled_refresh_cools_auth_five_minutes_and_fails_over() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let id = h.ids().remove(0);
    let mut owned = h.store.load(&id).unwrap();
    let tokens = h.store.tokens(&owned).unwrap();
    owned.source = qb_core::accounts::Source::Oauth;
    h.store
        .save(&owned, &tokens, Some(owned.generation))
        .unwrap();
    for round in 0..2 {
        if round == 1 {
            h.store
                .update_metadata(&id, |a| a.health = Health::Ready)
                .unwrap();
        }
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::UNAUTHORIZED, "{}"));
        if round == 0 {
            h.fake.state.push(ScriptedResponse::json(
                StatusCode::SERVICE_UNAVAILABLE,
                "{}",
            ));
        }
        h.fake.state.push(ok());
        let before = Utc::now();
        // Restore an existing affinity to A in each independent scenario.
        // Choosing a new session is subject to D20 and can legitimately select B.
        h.state
            .bindings
            .import(json!({"schema":1,"bindings":[{
                "key":format!("codex:refresh-round-{round}"),"account":id,"last_used":Utc::now()
            }]}))
            .unwrap();
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/responses", h.servers.data_addr))
            .bearer_auth("synthetic-client")
            .header("session-id", format!("refresh-round-{round}"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_ne!(response.headers()["x-quota-baton-account"], id);
        let account = h.store.load(&id).unwrap();
        assert_eq!(account.auth_failures, 0);
        assert!(
            matches!(account.health,Health::Cooling{reason:HealthReason::Auth,until,..} if (until-before).num_seconds().abs_diff(300)<=2)
        );
    }
    assert_eq!(h.fake.state.requests().len(), 5);
}

#[tokio::test]
async fn c5_auth_probe_race_waits_instead_of_502() {
    let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
    let id = h.ids().remove(0);
    h.store
        .update_metadata(&id, |a| a.auth_failures = 1)
        .unwrap();
    let mut first = ok();
    first.chunks[0].0 = Duration::from_millis(100);
    h.fake.state.push(first);
    h.fake.state.push(ok());
    let (one, two) = tokio::join!(h.send(Provider::Codex, "{}"), h.send(Provider::Codex, "{}"));
    assert_ne!(one.status(), 502);
    assert_ne!(two.status(), 502);
    assert!(one.status() == 200 || two.status() == 200);
    assert_eq!(h.store.load(&id).unwrap().auth_failures, 0);
}

#[tokio::test]
async fn c5_stale_revocation_401_retries_current_token_without_health_change() {
    let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
    let id = h.ids().remove(0);
    let mut r = ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"token_revoked"}}"#,
    );
    r.chunks[0].0 = Duration::from_millis(150);
    h.fake.state.push(r);
    h.fake.state.push(ok());
    let (response, _) = tokio::join!(h.send(Provider::Codex, "{}"), async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while h.fake.state.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut account = h.store.load(&id).unwrap();
        let mut tokens = h.store.tokens(&account).unwrap();
        tokens.access_token = "synthetic-rotated".into();
        account.token_meta.last_refresh_at = Some(Utc::now());
        h.store
            .save(&account, &tokens, Some(account.generation))
            .unwrap();
    });
    assert_eq!(response.status(), 200);
    assert_eq!(h.store.load(&id).unwrap().health, Health::Ready);
    assert_eq!(h.fake.state.requests().len(), 2);
}

#[tokio::test]
async fn c5_postcommit_stale_revocation_is_verbatim_without_health_change() {
    let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
    let id = h.ids().remove(0);
    let first = Bytes::from_static(b"data: {\"type\":\"response.created\"}\n\n");
    let error = Bytes::from_static(
        b"data: {\"type\":\"error\",\"status\":401,\"error\":{\"code\":\"token_revoked\"}}\n\n",
    );
    h.fake.state.push(ScriptedResponse::sse(vec![
        (Duration::ZERO, first.clone()),
        (Duration::from_millis(150), error.clone()),
    ]));
    let response = h.send(Provider::Codex, "{}").await;
    let mut account = h.store.load(&id).unwrap();
    let mut tokens = h.store.tokens(&account).unwrap();
    tokens.access_token = "synthetic-newer".into();
    account.token_meta.last_refresh_at = Some(Utc::now());
    h.store
        .save(&account, &tokens, Some(account.generation))
        .unwrap();
    assert_eq!(
        response.bytes().await.unwrap(),
        [first.as_ref(), error.as_ref()].concat()
    );
    assert_eq!(h.store.load(&id).unwrap().health, Health::Ready);
}

// This helper includes body EOF in the outer deadline, so a failed old-code
// run cannot wait for the synthetic 30s Retry-After or a default 60s hold.
async fn r1_http_total_hold_case(initial_budget_exhausted: bool) {
    let routing = RoutingConfig {
        rate_hold_secs: 1,
        rate_hold_max_retries: 3,
        ..RoutingConfig::default()
    };
    let h = Harness::new_with_routing(Provider::Claude, 2, 384, 1024, routing).await;
    let account = h.ids()[0].clone();
    let session = if initial_budget_exhausted {
        "r1-http-total-hold-initial"
    } else {
        "r1-http-total-hold-retry"
    };
    let binding = format!("claude:{session}");
    // Prebind without a warm-up send: exactly the tested attempts reach the
    // scripted upstream, and another Ready account can expose false failover.
    h.state
        .bindings
        .import(json!({"schema":1,"bindings":[{
            "key":binding,"account":account,"last_used":Utc::now()
        }]}))
        .unwrap();
    assert_eq!(
        h.state.bindings.current(Some(&binding)),
        Some(account.clone())
    );

    let first_body = r#"{"error":{"type":"rate_limit_error","message":"synthetic first rate"}}"#;
    let final_body = r#"{"error":{"type":"rate_limit_error","message":"synthetic final rate"}}"#;
    for body in [first_body, final_body] {
        let mut limited = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, body);
        limited.headers.insert("retry-after", "30".parse().unwrap());
        h.fake.state.push(limited);
    }
    let raw =
        " {\"model\":\"synthetic-model\", \"messages\":[], \"marker\":\"conversation-marker\"}\n";
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let request = client
        .post(format!("http://{}/v1/messages", h.servers.data_addr))
        .bearer_auth("synthetic-client")
        .header("x-claude-code-session-id", session)
        .body(raw.to_owned());
    let now = Instant::now();
    let observed_at = if initial_budget_exhausted {
        now
    } else {
        // First step is 2s; observing it 1.4s in the past leaves ~600ms
        // of initial admission, followed by only ~400ms of the same budget.
        now - Duration::from_millis(1400)
    };
    h.state.accounts.observe_429_at(
        &account,
        observed_at,
        observed_at,
        initial_budget_exhausted.then_some(Duration::from_secs(30)),
        1.0,
    );
    let started = Instant::now();
    let (status, headers, body) = tokio::time::timeout(Duration::from_millis(1600), async {
        let response = request.send().await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap();
        (status, headers, body)
    })
    .await
    .expect("HTTP total hold exceeded 1.6s: admission/retry must share one absolute deadline");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(900),
        "hold ended before its one-second budget"
    );
    assert!(
        elapsed <= Duration::from_millis(1600),
        "total hold reset between admission and retry"
    );
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "30");
    assert_eq!(headers["x-quota-baton-account"], account);
    assert_eq!(
        body,
        if initial_budget_exhausted {
            first_body
        } else {
            final_body
        }
    );

    let expected_attempts = if initial_budget_exhausted { 1 } else { 2 };
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), expected_attempts);
    let first_send_wait = requests[0].received_at.saturating_duration_since(started);
    if initial_budget_exhausted {
        assert!(first_send_wait >= Duration::from_millis(900));
    } else {
        assert!(
            (Duration::from_millis(450)..=Duration::from_millis(900)).contains(&first_send_wait),
            "initial admission must consume the seeded ~600ms before the first attempt"
        );
        assert!(
            requests[1].received_at.saturating_duration_since(started)
                >= Duration::from_millis(900)
        );
    }
    for request in &requests {
        assert!(request.headers["authorization"] == requests[0].headers["authorization"]);
        assert_eq!(request.body.as_ref(), raw.as_bytes());
        assert_eq!(request.headers["x-claude-code-session-id"], session);
    }
    assert_eq!(
        h.state.bindings.current(Some(&binding)),
        Some(account.clone())
    );
    for id in h.ids() {
        assert_eq!(h.store.load(&id).unwrap().health, Health::Ready);
    }
    let rows: Vec<_> = h
        .ledger()
        .into_iter()
        .filter(|row| row.get("req").is_some())
        .collect();
    assert_eq!(rows.len(), 1, "exactly one completed HTTP request");
    let row = &rows[0];
    assert_eq!(row["attempts"], expected_attempts as u64);
    assert_eq!(row["rate_retries"], (expected_attempts - 1) as u64);
    assert_eq!(row["relayed_rate_429s"], 1);
    assert_eq!(row["retained_copy"], true);
    assert_eq!(row["account"], account);
    assert_eq!(row["status"], 429);
    assert_eq!(row["outcome"], "rate_hold_relayed_429");
    let duration = row["dur_ms"].as_u64().unwrap();
    assert!((900..=1600).contains(&duration));
    let wait = row["rate_wait_ms"].as_u64().unwrap();
    assert!(
        (900..=1600).contains(&wait),
        "ledger wait must include initial admission once, plus remaining retry hold"
    );
    let attempts = row["attempt_details"].as_array().unwrap();
    assert_eq!(attempts.len(), expected_attempts);
    for attempt in attempts {
        assert_eq!(attempt["account"], account);
        assert!(attempt["failover_reason"].is_null());
    }
}

#[tokio::test]
async fn r1_http_total_hold_absolute_deadline_includes_initial_admission_and_retry() {
    r1_http_total_hold_case(false).await;
}

#[tokio::test]
async fn r1_http_total_hold_initial_admission_expiry_forces_one_send_without_retry() {
    r1_http_total_hold_case(true).await;
}

#[tokio::test]
async fn r8_round2_http_retry_expiry_sends_final_attempt_and_relays_its_429() {
    for provider in [Provider::Claude, Provider::Codex] {
        let routing = RoutingConfig {
            rate_hold_secs: 1,
            rate_hold_max_retries: 5,
            ..RoutingConfig::default()
        };
        let h = Harness::new_with_routing(provider, 1, 384, 1024, routing).await;
        let account = h.ids()[0].clone();
        let first_body =
            r#"{"error":{"type":"rate_limit_error","message":"synthetic first rate"}}"#;
        let final_body =
            r#"{"error":{"type":"rate_limit_error","message":"synthetic final forced rate"}}"#;
        for body in [first_body, final_body] {
            let mut rate = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, body);
            rate.headers.insert("retry-after", "30".parse().unwrap());
            h.fake.state.push(rate);
        }
        h.fake.state.push(ok()); // A forbidden third RATE send would consume this.
        let (path, session_header) = if provider == Provider::Claude {
            ("/v1/messages", "x-claude-code-session-id")
        } else {
            ("/v1/responses", "session-id")
        };
        let raw =
            " {\"model\":\"synthetic-model\",\"messages\":[],\"input\":\"conversation-marker\"}\n";
        let began = Instant::now();
        let (status, headers, body) = tokio::time::timeout(Duration::from_secs(2), async {
            let response = reqwest::Client::new()
                .post(format!("http://{}{path}", h.servers.data_addr))
                .bearer_auth("synthetic-client")
                .header(session_header, "round2-final-retry")
                .body(raw.to_owned())
                .send()
                .await
                .unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text().await.unwrap();
            (status, headers, body)
        })
        .await
        .expect("retry must use the original one-second hold");
        assert!(began.elapsed() >= Duration::from_millis(900));
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(headers["x-quota-baton-account"], account);
        assert_eq!(headers["retry-after"], "30");
        assert_eq!(
            body, final_body,
            "relay the final forced attempt, not its predecessor"
        );
        let requests = h.fake.state.requests();
        assert_eq!(
            requests.len(),
            2,
            "a forced attempt cannot schedule another RATE retry"
        );
        assert!(
            requests[1].received_at.saturating_duration_since(began) >= Duration::from_millis(900)
        );
        for request in &requests {
            assert_eq!(request.body.as_ref(), raw.as_bytes());
            assert_eq!(
                request.headers["authorization"],
                requests[0].headers["authorization"]
            );
            assert_eq!(request.headers[session_header], "round2-final-retry");
        }
        assert_eq!(
            h.state
                .bindings
                .current(Some(&format!("{}:round2-final-retry", provider.as_str()))),
            Some(account.clone())
        );
        assert_eq!(h.store.load(&account).unwrap().health, Health::Ready);
        let rows: Vec<_> = h
            .ledger()
            .into_iter()
            .filter(|row| row.get("req").is_some())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["account"], account);
        assert_eq!(rows[0]["attempts"], 2);
        assert_eq!(rows[0]["rate_retries"], 1);
        assert_eq!(rows[0]["relayed_rate_429s"], 1);
        assert_eq!(rows[0]["outcome"], "rate_hold_relayed_429");
        assert!(rows[0]["rate_wait_ms"].as_u64().unwrap() >= 900);
    }
}

#[tokio::test]
async fn r1_http_late_inflight_rate_does_not_raise_a_new_window_step() {
    let h = Harness::new(Provider::Codex, 1, 0, 1024).await;
    let id = h.ids()[0].clone();
    let raw = "{\"detail\":\"Rate limit exceeded\"}";
    let mut late = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, raw);
    // Another request opens a two-second window while this older response is
    // still in flight. Its delayed 429 arrives after that window has ended,
    // but before the four-second quiet-step boundary.
    late.chunks[0].0 = Duration::from_secs(3);
    h.fake.state.push(late);
    let mut response = Box::pin(h.send(Provider::Codex, "{}"));
    tokio::select! {
        _ = &mut response => panic!("delayed fixture answered before its first arrival"),
        arrived = tokio::time::timeout(Duration::from_secs(1), async {
            while h.fake.state.requests().is_empty() {
                tokio::task::yield_now().await;
            }
        }) => arrived.unwrap(),
    }
    let other = Instant::now();
    h.state
        .accounts
        .observe_429_at(&id, other, other, None, 1.0);
    let response = tokio::time::timeout(Duration::from_secs(5), response)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.text().await.unwrap(), raw);
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_eq!(h.state.accounts.pacing_snapshot(&id).rate_step, 1);
    assert_eq!(h.store.load(&id).unwrap().health, Health::Ready);
}

// R1-tray.2: core-only event/snapshot contracts; UI bridge is owned separately.
fn tray2_events(events: &mut tokio::sync::broadcast::Receiver<Value>) -> Vec<Value> {
    let mut notices = Vec::new();
    while let Ok(event) = events.try_recv() {
        let text = event.to_string();
        for secret in [
            "synthetic-access",
            "synthetic-refresh",
            "conversation-marker",
        ] {
            assert!(!text.contains(secret));
        }
        if event["event"] == "core_warning" && event["data"]["kind"] == "quota_notice" {
            notices.push(event["data"].clone());
        }
    }
    notices
}
fn tray2_headers(used: u32, reset: i64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-codex-secondary-window-minutes", "10080".to_owned()),
        ("x-codex-secondary-used-percent", used.to_string()),
        ("x-codex-secondary-reset-at", reset.to_string()),
        ("x-codex-active-limit", "secondary".to_owned()),
    ] {
        headers.insert(name, value.parse().unwrap());
    }
    headers
}

#[tokio::test]
async fn tray2_core_quota_failover_is_ready_c7_target_and_flat_event() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let mut events = h.state.accounts.subscribe();
    let reset = Utc::now().timestamp() + 172800;
    let mut limited = quota(reset);
    limited.headers.extend(tray2_headers(100, reset));
    h.fake.state.push(limited);
    h.fake.state.push(ok());
    let response = h.send_with_session(Provider::Codex, "tray2-failover").await;
    assert_eq!(response.status(), 200);
    let target = response.headers()["x-quota-baton-account"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    let notices = tray2_events(&mut events);
    let notice = notices.iter().find(|n| n["action"] == "failover").unwrap();
    assert_eq!(notice["next_account"], target);
    assert_ne!(notice["account"], target);
    assert_eq!(notice["window"], "7d");
    assert_eq!(
        notice["reset_at"],
        json!(chrono::DateTime::from_timestamp(reset, 0).unwrap())
    );
    let views = h.state.accounts.views().await.unwrap();
    let source = views.iter().find(|v| v["id"] == notice["account"]).unwrap();
    assert_eq!(source["quota_notice"]["action"], "failover");
    assert_eq!(source["has_alternative"], true);
    assert_eq!(
        views.iter().find(|v| v["id"] == target).unwrap()["has_alternative"],
        false
    );
    assert!(
        !serde_json::to_string(&views)
            .unwrap()
            .contains("synthetic-access")
    );
}

#[tokio::test]
async fn tray2_core_exhausted_earliest_observed_reset_and_unknown_is_null() {
    for known in [true, false] {
        let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
        let mut events = h.state.accounts.subscribe();
        let reset = Utc::now().timestamp() + 172800;
        h.fake.state.push(if known {
            quota(reset)
        } else {
            ScriptedResponse::json(
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"error":{"type":"usage_limit_reached"}}"#,
            )
        });
        let response = h.send(Provider::Codex, "{}").await;
        assert_eq!(response.status(), 429);
        response.bytes().await.unwrap();
        let notices = tray2_events(&mut events);
        let notice = notices.iter().find(|n| n["action"] == "exhausted").unwrap();
        assert!(notice["next_account"].is_null());
        assert_eq!(notice["reset_at"].is_null(), !known);
        assert_eq!(notice["earliest_account"].is_null(), !known);
        if known {
            assert_eq!(notice["earliest_account"], notice["account"]);
        }
        let views = h.state.accounts.views().await.unwrap();
        assert_eq!(views[0]["quota_notice"]["action"], "exhausted");
        assert_eq!(views[0]["has_alternative"], false);
    }
}

#[tokio::test]
async fn tray2_core_credits_requires_success_and_disable_clears_without_new_toast() {
    let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
    let id = h.ids().remove(0);
    let mut events = h.state.accounts.subscribe();
    h.state
        .accounts
        .change(&id, None, None, None, Some(true))
        .await
        .unwrap();
    assert!(tray2_events(&mut events).is_empty());
    assert!(h.state.accounts.views().await.unwrap()[0]["quota_notice"].is_null());
    let mut response = ok();
    response
        .headers
        .insert("x-codex-credits-serving", "true".parse().unwrap());
    response
        .headers
        .insert("x-codex-credits-has-credits", "true".parse().unwrap());
    h.fake.state.push(response);
    h.send(Provider::Codex, "{}").await.bytes().await.unwrap();
    let notices = tray2_events(&mut events);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["action"], "credits");
    assert_eq!(notices[0]["account"], id);
    assert_eq!(
        h.state.accounts.views().await.unwrap()[0]["quota_notice"]["action"],
        "credits"
    );
    h.state
        .accounts
        .change(&id, None, None, None, Some(false))
        .await
        .unwrap();
    assert!(h.state.accounts.views().await.unwrap()[0]["quota_notice"].is_null());
    assert!(tray2_events(&mut events).is_empty());
    // Re-enabling and an ordinary success cannot resurrect the cached C9 flag.
    h.state
        .accounts
        .change(&id, None, None, None, Some(true))
        .await
        .unwrap();
    h.fake.state.push(ok());
    h.send(Provider::Codex, "{}").await.bytes().await.unwrap();
    assert!(tray2_events(&mut events).is_empty());
    assert!(h.state.accounts.views().await.unwrap()[0]["quota_notice"].is_null());
    let mut response = ok();
    response
        .headers
        .insert("x-codex-credits-serving", "true".parse().unwrap());
    response
        .headers
        .insert("x-codex-credits-has-credits", "true".parse().unwrap());
    h.fake.state.push(response);
    h.send(Provider::Codex, "{}").await.bytes().await.unwrap();
    assert_eq!(tray2_events(&mut events).len(), 1);
    let mut depleted = HeaderMap::new();
    depleted.insert("x-codex-credits-has-credits", "false".parse().unwrap());
    h.state
        .accounts
        .observe_headers(&id, Provider::Codex, &depleted)
        .await;
    assert!(h.state.accounts.views().await.unwrap()[0]["quota_notice"].is_null());
    assert!(tray2_events(&mut events).is_empty());
}

#[tokio::test]
async fn tray2_core_reset_hold_and_successful_retry_never_publish_quota() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    h.fake.state.push(ok());
    let warm = h.send_with_session(Provider::Codex, "tray2-hold").await;
    let original = warm.headers()["x-quota-baton-account"].clone();
    warm.bytes().await.unwrap();
    let mut events = h.state.accounts.subscribe();
    h.fake.state.push(quota(Utc::now().timestamp() + 2));
    h.fake.state.push(ok());
    let retry = h.send_with_session(Provider::Codex, "tray2-hold");
    tokio::pin!(retry);
    tokio::select! {
        response = &mut retry => panic!("reset hold did not wait: {}", response.status()),
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
    }
    assert!(tray2_events(&mut events).is_empty());
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    let response = retry.await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-quota-baton-account"], original);
    response.bytes().await.unwrap();
    assert!(tray2_events(&mut events).is_empty());
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
}

#[tokio::test]
async fn tray2_core_reset_and_disabled_target_invalidate_quota_context() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let reset = Utc::now().timestamp() + 172800;
    let mut response = quota(reset);
    response.headers.extend(tray2_headers(100, reset));
    h.fake.state.push(response);
    h.fake.state.push(ok());
    h.send_with_session(Provider::Codex, "tray2-clear")
        .await
        .bytes()
        .await
        .unwrap();
    let views = h.state.accounts.views().await.unwrap();
    let limited = views
        .iter()
        .find(|v| v["quota_notice"]["action"] == "failover")
        .unwrap();
    let source = limited["id"].as_str().unwrap();
    let target = limited["quota_notice"]["next_account"].as_str().unwrap();
    h.state
        .accounts
        .change(target, Some(UserState::Disabled), None, None, None)
        .await
        .unwrap();
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    h.state
        .accounts
        .observe_headers(source, Provider::Codex, &tray2_headers(10, reset + 86400))
        .await;
    assert_eq!(h.store.load(source).unwrap().health, Health::Ready);
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
}

#[tokio::test]
async fn tray2_core_exhaustion_uses_earliest_account_not_last_failure() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let first_reset = Utc::now().timestamp() + 86400;
    h.fake.state.push(quota(first_reset));
    h.fake.state.push(quota(first_reset + 86400));
    let response = h.send_with_session(Provider::Codex, "tray2-earliest").await;
    assert_eq!(response.status(), 429);
    response.bytes().await.unwrap();
    let views = h.state.accounts.views().await.unwrap();
    let exhausted = views
        .iter()
        .find(|v| v["quota_notice"]["action"] == "exhausted")
        .unwrap();
    let earliest = exhausted["quota_notice"]["earliest_account"]
        .as_str()
        .unwrap();
    assert_ne!(exhausted["id"], earliest);
    assert_eq!(
        exhausted["quota_notice"]["reset_at"],
        json!(chrono::DateTime::from_timestamp(first_reset, 0).unwrap())
    );
    assert!(
        views
            .iter()
            .filter(|v| v["id"] != exhausted["id"])
            .all(|v| v["quota_notice"].is_null())
    );
}

#[tokio::test]
async fn tray2_core_credit_preferences_capability_and_error_are_not_serving_evidence() {
    for (allow, serving, capable, success) in [
        (true, false, true, true),
        (false, true, true, true),
        (true, true, false, true),
        (true, true, true, false),
    ] {
        let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
        h.state
            .accounts
            .change(&h.ids()[0], None, None, None, Some(allow))
            .await
            .unwrap();
        let mut events = h.state.accounts.subscribe();
        let mut response = ScriptedResponse::json(
            if success {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            },
            "{}",
        );
        response.headers.insert(
            "x-codex-credits-has-credits",
            capable.to_string().parse().unwrap(),
        );
        if serving {
            response
                .headers
                .insert("x-codex-credits-serving", "true".parse().unwrap());
        }
        h.fake.state.push(response);
        h.send(Provider::Codex, "{}").await.bytes().await.unwrap();
        assert!(tray2_events(&mut events).is_empty());
        assert!(h.state.accounts.views().await.unwrap()[0]["quota_notice"].is_null());
    }
}

#[tokio::test]
async fn tray2_core_successful_credit_response_at_full_window_still_has_actual_c9_context() {
    let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
    let id = h.ids().remove(0);
    h.state
        .accounts
        .change(&id, None, None, None, Some(true))
        .await
        .unwrap();
    let mut response = ok();
    response
        .headers
        .extend(tray2_headers(100, Utc::now().timestamp() + 172800));
    response
        .headers
        .insert("x-codex-credits-serving", "true".parse().unwrap());
    response
        .headers
        .insert("x-codex-credits-has-credits", "true".parse().unwrap());
    h.fake.state.push(response);
    let mut events = h.state.accounts.subscribe();
    let response = h.send(Provider::Codex, "{}").await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let notices = tray2_events(&mut events);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["action"], "credits");
    assert_eq!(notices[0]["window"], "7d");
    assert_eq!(
        h.state.accounts.views().await.unwrap()[0]["quota_notice"]["action"],
        "credits"
    );
}

#[tokio::test]
async fn tray2_core_postcommit_quota_context_waits_for_next_request_bound_or_unbound() {
    for bound in [false, true] {
        let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
        let first = Bytes::from_static(b"data: {\"type\":\"response.created\"}\n\n");
        let error = Bytes::from_static(b"data: {\"type\":\"error\",\"status\":429,\"error\":{\"type\":\"usage_limit_reached\",\"resets_in_seconds\":600}}\n\n");
        h.fake.state.push(ScriptedResponse::sse(vec![
            (Duration::ZERO, first.clone()),
            (Duration::from_millis(10), error.clone()),
        ]));
        let mut events = h.state.accounts.subscribe();
        let response = if bound {
            h.send_with_session(Provider::Codex, "tray2-postcommit")
                .await
        } else {
            h.send(Provider::Codex, "{}").await
        };
        let source = response.headers()["x-quota-baton-account"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            response.bytes().await.unwrap(),
            [first.as_ref(), error.as_ref()].concat()
        );
        assert!(tray2_events(&mut events).is_empty());
        h.fake.state.push(ok());
        let response = if bound {
            h.send_with_session(Provider::Codex, "tray2-postcommit")
                .await
        } else {
            h.send(Provider::Codex, "{}").await
        };
        assert_eq!(response.status(), 200);
        let target = response.headers()["x-quota-baton-account"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_ne!(source, target);
        response.bytes().await.unwrap();
        let notices = tray2_events(&mut events);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["account"], source);
        assert_eq!(notices[0]["next_account"], target);
        assert_eq!(notices[0]["action"], "failover");
    }
}

#[tokio::test]
async fn tray2_model_scoped_exhaustion_survives_authoritative_views_without_model_name_on_wire() {
    for count in [1, 2] {
        let h = Harness::new(Provider::Claude, count, 384, 1024).await;
        let reset = Utc::now().timestamp() + 172800;
        for _ in 0..count {
            let mut response = ScriptedResponse::json(
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"error":{"type":"rate_limit_error"}}"#,
            );
            response.headers.insert(
                "anthropic-ratelimit-unified-7d_oi-status",
                "rejected".parse().unwrap(),
            );
            response.headers.insert(
                "anthropic-ratelimit-unified-7d_oi-reset",
                reset.to_string().parse().unwrap(),
            );
            h.fake.state.push(response);
        }
        let mut events = h.state.accounts.subscribe();
        let response = h
            .send(Provider::Claude, r#"{"model":"private-synthetic-model"}"#)
            .await;
        assert_eq!(response.status(), 429);
        response.bytes().await.unwrap();
        let notices = tray2_events(&mut events);
        let exhausted = notices.iter().find(|n| n["action"] == "exhausted").unwrap();
        assert_eq!(exhausted["model_limited"], true);
        assert!(!exhausted.to_string().contains("private-synthetic-model"));
        let views = h.state.accounts.views().await.unwrap();
        let source = views
            .iter()
            .find(|v| v["id"] == exhausted["account"])
            .unwrap();
        assert_eq!(source["quota_notice"]["action"], "exhausted");
        assert_eq!(source["quota_notice"]["model_limited"], true);
        assert!(
            !source["quota_notice"]
                .to_string()
                .contains("private-synthetic-model")
        );
        assert!(h.store.list().unwrap().iter().all(|a| {
            !a.eligible_at(Some("private-synthetic-model"), Utc::now())
                && a.eligible_at(Some("other-synthetic-model"), Utc::now())
        }));
        assert_eq!(
            h.state
                .accounts
                .views()
                .await
                .unwrap()
                .iter()
                .find(|v| v["id"] == exhausted["account"])
                .unwrap()["quota_notice"]["action"],
            "exhausted"
        );
    }
}

#[tokio::test]
async fn tray2_exhausted_earliest_recomputes_when_paused_or_drained_account_joins_pool() {
    for paused in [false, true] {
        let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
        let ids = h.ids();
        let b = &ids[1];
        let reset_a = Utc::now().timestamp() + 172800;
        let reset_b = reset_a - 86400;
        let mut b_headers = tray2_headers(100, reset_b);
        b_headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        b_headers.insert("x-codex-primary-used-percent", "100".parse().unwrap());
        b_headers.insert(
            "x-codex-primary-reset-at",
            (reset_b - 43200).to_string().parse().unwrap(),
        );
        h.state
            .accounts
            .observe_headers(b, Provider::Codex, &b_headers)
            .await;
        if paused {
            h.state
                .accounts
                .change(b, Some(UserState::Paused), None, None, None)
                .await
                .unwrap();
        } else {
            h.state
                .accounts
                .change(b, None, None, Some(true), None)
                .await
                .unwrap();
        }
        let mut response = quota(reset_a);
        response.headers.extend(tray2_headers(100, reset_a));
        h.fake.state.push(response);
        let response = h.send(Provider::Codex, "{}").await;
        assert_eq!(response.status(), 429);
        response.bytes().await.unwrap();
        let before = h.state.accounts.views().await.unwrap();
        let source = before
            .iter()
            .find(|v| v["quota_notice"]["action"] == "exhausted")
            .unwrap();
        let a = source["id"].as_str().unwrap();
        assert_ne!(a, b);
        assert_eq!(source["quota_notice"]["earliest_account"], a);
        assert_eq!(
            source["quota_notice"]["reset_at"],
            json!(chrono::DateTime::from_timestamp(reset_a, 0).unwrap())
        );
        if paused {
            h.state
                .accounts
                .change(b, Some(UserState::Active), None, None, None)
                .await
                .unwrap();
        } else {
            h.state
                .accounts
                .change(b, None, None, Some(false), None)
                .await
                .unwrap();
        }
        // No new relay request: the snapshot itself must reconcile provider truth.
        let after = h.state.accounts.views().await.unwrap();
        let updated = &after.iter().find(|v| v["id"] == a).unwrap()["quota_notice"];
        assert_eq!(updated["action"], "exhausted");
        assert_eq!(updated["earliest_account"], b.as_str());
        assert_eq!(
            updated["reset_at"],
            json!(chrono::DateTime::from_timestamp(reset_b, 0).unwrap())
        );
        assert_eq!(updated["model_limited"], false);
        if paused {
            h.state
                .accounts
                .change(b, Some(UserState::Paused), None, None, None)
                .await
                .unwrap();
        } else {
            h.state
                .accounts
                .change(b, None, None, Some(true), None)
                .await
                .unwrap();
        }
        let removed = h.state.accounts.views().await.unwrap();
        let updated = &removed.iter().find(|v| v["id"] == a).unwrap()["quota_notice"];
        assert_eq!(updated["earliest_account"], a);
        assert_eq!(
            updated["reset_at"],
            json!(chrono::DateTime::from_timestamp(reset_a, 0).unwrap())
        );
    }
}

#[tokio::test]
async fn tray2_exhausted_uses_latest_blocking_window_per_account_before_provider_minimum() {
    let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
    let later = Utc::now().timestamp() + 172800;
    let earlier = later - 86400;
    let mut response = quota(later);
    response.headers.extend(tray2_headers(100, later));
    response
        .headers
        .insert("x-codex-primary-window-minutes", "300".parse().unwrap());
    response
        .headers
        .insert("x-codex-primary-used-percent", "100".parse().unwrap());
    response.headers.insert(
        "x-codex-primary-reset-at",
        earlier.to_string().parse().unwrap(),
    );
    // Persisted reducer evidence still outranks a conflicting active-limit alias.
    response
        .headers
        .insert("x-codex-active-limit", "primary".parse().unwrap());
    h.fake.state.push(response);
    let response = h.send(Provider::Codex, "{}").await;
    assert_eq!(response.status(), 429);
    response.bytes().await.unwrap();
    let views = h.state.accounts.views().await.unwrap();
    assert_eq!(views[0]["quota_notice"]["action"], "exhausted");
    assert_eq!(
        views[0]["quota_notice"]["reset_at"],
        json!(chrono::DateTime::from_timestamp(later, 0).unwrap())
    );
    assert_eq!(views[0]["quota_notice"]["window"], "7d");
    assert_eq!(views[0]["quota_notice"]["earliest_account"], views[0]["id"]);
}

#[tokio::test]
async fn tray2_three_accounts_decayed_pacing_names_actual_upstream_successor() {
    let h = Harness::new(Provider::Codex, 3, 384, 1024).await;
    h.fake.state.push(ok());
    let warm = h
        .send_with_session(Provider::Codex, "tray2-pacing-selection")
        .await;
    let source = warm.headers()["x-quota-baton-account"]
        .to_str()
        .unwrap()
        .to_owned();
    warm.bytes().await.unwrap();
    let others: Vec<_> = h.ids().into_iter().filter(|id| id != &source).collect();
    let b = &others[0];
    let mut limited = quota(Utc::now().timestamp() + 172800);
    limited.chunks[0].0 = Duration::from_millis(500);
    h.fake.state.push(limited);
    h.fake.state.push(ok());
    let mut events = h.state.accounts.subscribe();
    let request = h.send_with_session(Provider::Codex, "tray2-pacing-selection");
    tokio::pin!(request);
    tokio::select! {
        _ = &mut request => panic!("quota response did not remain outstanding"),
        _ = async {
            while h.fake.state.requests().len() < 2 { tokio::time::sleep(Duration::from_millis(1)).await; }
        } => {}
    }
    let past = Instant::now() - Duration::from_secs(120);
    h.state.accounts.observe_429_at(b, past, past, None, 1.0);
    // Reproduce stale cached pacing while A's upstream quota body is outstanding.
    h.state.bindings.set_pacing(b, true);
    let response = request.await;
    assert_eq!(response.status(), 200);
    let served = response.headers()["x-quota-baton-account"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    assert_eq!(
        served, *b,
        "production candidate collection must refresh decayed pacing"
    );
    let notices = tray2_events(&mut events);
    let notice = notices.iter().find(|n| n["action"] == "failover").unwrap();
    assert_eq!(notice["account"], source);
    assert_eq!(notice["next_account"], served);
    let account = h.store.load(&served).unwrap();
    let token = h.store.tokens(&account).unwrap();
    assert_eq!(
        h.fake.state.requests().last().unwrap().headers["authorization"],
        format!("Bearer {}", token.access_token)
    );
}

#[tokio::test]
async fn tray2_passive_200_full_window_waits_for_actual_next_placement_or_no_eligible() {
    for count in [1, 2] {
        for bound in [false, true] {
            let h = Harness::new(Provider::Codex, count, 384, 1024).await;
            let mut observed = ok();
            observed
                .headers
                .extend(tray2_headers(100, Utc::now().timestamp() + 172800));
            h.fake.state.push(observed);
            let mut events = h.state.accounts.subscribe();
            let response = if bound {
                h.send_with_session(Provider::Codex, "tray2-passive").await
            } else {
                h.send(Provider::Codex, "{}").await
            };
            assert_eq!(response.status(), 200);
            let source = response.headers()["x-quota-baton-account"]
                .to_str()
                .unwrap()
                .to_owned();
            response.bytes().await.unwrap();
            assert!(
                tray2_events(&mut events).is_empty(),
                "passive usage must not guess a successor"
            );
            assert!(
                h.state
                    .accounts
                    .views()
                    .await
                    .unwrap()
                    .iter()
                    .all(|v| v["quota_notice"].is_null())
            );
            h.fake.state.push(ok());
            let response = if bound {
                h.send_with_session(Provider::Codex, "tray2-passive").await
            } else {
                h.send(Provider::Codex, "{}").await
            };
            let served = response
                .headers()
                .get("x-quota-baton-account")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            assert_eq!(response.status(), if count == 1 { 429 } else { 200 });
            response.bytes().await.unwrap();
            let notices = tray2_events(&mut events);
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0]["account"], source);
            assert_eq!(notices[0]["model_limited"], false);
            if count == 1 {
                assert_eq!(notices[0]["action"], "exhausted");
                assert!(notices[0]["next_account"].is_null());
            } else {
                let served = served.unwrap();
                assert_ne!(served, source);
                assert_eq!(notices[0]["action"], "failover");
                assert_eq!(notices[0]["next_account"], served);
                let target = h.store.load(&served).unwrap();
                let token = h.store.tokens(&target).unwrap();
                assert_eq!(
                    h.fake.state.requests().last().unwrap().headers["authorization"],
                    format!("Bearer {}", token.access_token)
                );
            }
        }
    }
}

#[tokio::test]
async fn tray2_passive_200_full_window_reset_hold_and_same_account_success_have_no_notice() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    let mut observed = ok();
    observed
        .headers
        .extend(tray2_headers(100, Utc::now().timestamp() + 2));
    h.fake.state.push(observed);
    let response = h
        .send_with_session(Provider::Codex, "tray2-passive-hold")
        .await;
    let original = response.headers()["x-quota-baton-account"].to_owned();
    response.bytes().await.unwrap();
    let mut events = h.state.accounts.subscribe();
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    h.fake.state.push(ok());
    let request = h.send_with_session(Provider::Codex, "tray2-passive-hold");
    tokio::pin!(request);
    tokio::select! {
        _ = &mut request => panic!("passive reset hold did not wait"),
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
    }
    assert!(tray2_events(&mut events).is_empty());
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    let response = request.await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-quota-baton-account"], original);
    response.bytes().await.unwrap();
    assert!(tray2_events(&mut events).is_empty());
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
}

#[tokio::test]
async fn tray2_credit_disable_alone_cannot_recreate_passive_quota_notice_on_next_placement() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    for id in h.ids() {
        h.state
            .accounts
            .change(&id, None, None, None, Some(true))
            .await
            .unwrap();
    }
    let mut observed = ok();
    observed
        .headers
        .extend(tray2_headers(100, Utc::now().timestamp() + 172800));
    observed
        .headers
        .insert("x-codex-credits-serving", "true".parse().unwrap());
    observed
        .headers
        .insert("x-codex-credits-has-credits", "true".parse().unwrap());
    h.fake.state.push(observed);
    let mut events = h.state.accounts.subscribe();
    let response = h
        .send_with_session(Provider::Codex, "tray2-toggle-passive")
        .await;
    let source = response.headers()["x-quota-baton-account"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    assert_eq!(tray2_events(&mut events)[0]["action"], "credits");
    h.state
        .accounts
        .change(&source, None, None, None, Some(false))
        .await
        .unwrap();
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    // A new metadata-only observation must not recycle its merged old 100% window.
    let mut capability = HeaderMap::new();
    capability.insert("x-codex-credits-has-credits", "false".parse().unwrap());
    h.state
        .accounts
        .observe_headers(&source, Provider::Codex, &capability)
        .await;
    h.fake.state.push(ok());
    let response = h
        .send_with_session(Provider::Codex, "tray2-toggle-passive")
        .await;
    assert_eq!(response.status(), 200);
    assert_ne!(response.headers()["x-quota-baton-account"], source);
    response.bytes().await.unwrap();
    assert!(tray2_events(&mut events).is_empty());
}

#[tokio::test]
async fn tray2_passive_poll_at_99_resolves_only_when_next_request_actually_moves() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    h.fake.state.push(ok());
    let response = h
        .send_with_session(Provider::Codex, "tray2-passive-poll")
        .await;
    let source = response.headers()["x-quota-baton-account"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    let now = Utc::now();
    let mut events = h.state.accounts.subscribe();
    h.state.accounts.apply_quota_snapshot(&source, json!({
        "source":"synthetic-poll","observed_at":now,
        "windows":[{"name":"5h","used_pct":99.0,"reset_at":now + ChronoDuration::hours(5),"observed_at":now}],
        "credits":{"has_credits":true}, "signals":{}
    }), None).await.unwrap();
    assert!(tray2_events(&mut events).is_empty());
    assert!(
        h.state
            .accounts
            .views()
            .await
            .unwrap()
            .iter()
            .all(|v| v["quota_notice"].is_null())
    );
    h.fake.state.push(ok());
    let response = h
        .send_with_session(Provider::Codex, "tray2-passive-poll")
        .await;
    assert_eq!(response.status(), 200);
    let served = response.headers()["x-quota-baton-account"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_ne!(served, source);
    response.bytes().await.unwrap();
    let notices = tray2_events(&mut events);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["account"], source);
    assert_eq!(notices[0]["action"], "failover");
    assert_eq!(notices[0]["next_account"], served);
    assert_eq!(notices[0]["window"], "5h");
}

#[tokio::test]
async fn r4_credits_in_use_requires_actual_serving_and_preserves_decimal_balance() {
    for (active, serving, expected) in [
        (false, None, false),
        (true, None, true),
        (true, Some(false), false),
    ] {
        // Each case starts eligible. A capability-only full-meter response may
        // legitimately cool the account; do not silently reuse that cooling
        // account as the next case's successful data-plane request.
        let h = Harness::new(Provider::Codex, 1, 384, 1024).await;
        let id = h.ids().remove(0);
        h.state
            .accounts
            .change(&id, None, None, None, Some(true))
            .await
            .unwrap();
        let mut response = ok();
        for (key, value) in [
            ("x-codex-secondary-used-percent", "100"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-after-seconds", "172800"),
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-balance", "2.875"),
        ] {
            response.headers.insert(key, value.parse().unwrap());
        }
        if active {
            response
                .headers
                .insert("x-codex-active-limit", "credits".parse().unwrap());
        }
        if let Some(serving) = serving {
            response.headers.insert(
                "x-codex-credits-serving",
                if serving { "true" } else { "false" }.parse().unwrap(),
            );
        }
        h.fake.state.push(response);
        let response = h.send(Provider::Codex, "{}").await;
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        assert_eq!(h.fake.state.requests().len(), 1);
        let views = h.state.accounts.views().await.unwrap();
        assert_eq!(views[0]["credits"]["balance"], 2.875);
        assert_eq!(views[0]["credits_in_use"], expected);
        if expected {
            h.state
                .accounts
                .change(&id, None, None, None, Some(false))
                .await
                .unwrap();
            assert_eq!(
                h.state.accounts.views().await.unwrap()[0]["credits_in_use"],
                false
            );
            h.state
                .accounts
                .change(&id, None, None, None, Some(true))
                .await
                .unwrap();
            assert_eq!(
                h.state.accounts.views().await.unwrap()[0]["credits_in_use"],
                false
            );
        }
    }
}

#[tokio::test]
async fn full_meter_with_permitted_credits_keeps_rate_retry_on_same_account() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    for id in h.ids() {
        h.store
            .update_metadata(&id, |a| a.allow_credits = true)
            .unwrap();
    }
    let mut r = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, "{}");
    for (key, value) in [
        ("x-codex-primary-used-percent", "100"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-after-seconds", "3600"),
        ("x-codex-credits-has-credits", "true"),
        ("retry-after", "1"),
    ] {
        r.headers.insert(key, value.parse().unwrap());
    }
    h.fake.state.push(r);
    h.fake.state.push(ok());
    let response = h.send(Provider::Codex, "{}").await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 2);
    let suffix = requests[0].headers["chatgpt-account-id"]
        .to_str()
        .unwrap()
        .strip_prefix("synthetic-")
        .unwrap();
    assert_eq!(
        requests[0].headers["authorization"].to_str().unwrap(),
        format!("Bearer synthetic-access-{suffix}")
    );
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        requests[1].headers.get("authorization").unwrap()
    );
    assert_eq!(
        requests[0].headers.get("chatgpt-account-id").unwrap(),
        requests[1].headers.get("chatgpt-account-id").unwrap()
    );
    assert!(
        h.store
            .list()
            .unwrap()
            .iter()
            .all(|a| a.health == Health::Ready)
    );
}

#[tokio::test]
async fn committed_probe_store_failure_preserves_success_and_blocks_later_dispatch() {
    let h = Harness::new(Provider::Claude, 1, 384, 1024).await;
    let id = h.ids()[0].clone();
    h.store
        .update_metadata(&id, |a| {
            a.auth_probe_pending = true;
            a.auth_failures = 1;
        })
        .unwrap();
    let path = h.home.join("accounts").join(format!("{id}.json"));
    let original = fs::metadata(&path).unwrap().permissions();
    let mut readonly = original.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&path, readonly).unwrap();
    let raw=Bytes::from_static(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    h.fake
        .state
        .push(ScriptedResponse::sse(vec![(Duration::ZERO, raw.clone())]));
    let response = h
        .send_with_session(Provider::Claude, "committed-probe")
        .await;
    let status = response.status();
    let body = response.bytes().await.unwrap();
    fs::set_permissions(&path, original).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, raw);
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_eq!(
        h.fake.state.requests()[0].headers["authorization"],
        "Bearer synthetic-access-0"
    );
    assert!(!h.state.accounts.eligible(&id, None).await);
    let next = h
        .send_with_session(Provider::Claude, "committed-probe")
        .await;
    assert_ne!(next.status(), StatusCode::OK);
    assert_eq!(h.fake.state.requests().len(), 1);
    assert!(h.ledger().iter().any(|r|r["event"]=="core_warning" && r["code"]=="probe_store_failed_after_commit"));
}

// Authenticated D22 data-port integration with synthetic accounts only.
async fn r11_http_issue(h: &Harness) -> qb_core::relay::LaunchReply {
    for _ in h.ids() {
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::OK,
            r#"{"models":[{"slug":"synthetic-model"}]}"#,
        ));
    }
    h.state
        .issue_launch(
            qb_core::relay::LaunchRequest {
                model: Some("synthetic-model".into()),
                model_is_explicit: true,
                account_pref: None,
                resume_id: None,
                move_for_model: false,
                urgent: false,
            },
            h.servers.data_addr.port(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn r11_http_entry_c8_adoption_preserves_original_body_and_known_key_precedence() {
    let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    // The body member is deliberately not an identity authority. C8 header wins.
    let raw = r#"{
  "model": "synthetic-model", "session_id": "not-a-c8-key", "input": [] }"#
        .to_owned();
    h.fake.state.push(ok());
    let response = reqwest::Client::new()
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "synthetic-c8")
        .body(raw.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let calls = h.fake.state.requests();
    let sent = calls
        .iter()
        .find(|call| call.path_and_query == "/responses")
        .unwrap();
    assert_eq!(sent.body.as_ref(), raw.as_bytes());
    assert_eq!(
        h.state
            .bindings
            .current(Some("codex:synthetic-c8"))
            .as_deref(),
        Some(launch.account.as_str())
    );
    assert!(
        h.state
            .bindings
            .current(Some("codex:not-a-c8-key"))
            .is_none()
    );
    let unknown = "Z".repeat(43);
    h.fake.state.push(ok());
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/l/{unknown}/v1/responses",
            h.servers.data_addr
        ))
        .bearer_auth("synthetic-client")
        .header("session-id", "synthetic-c8")
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "known C8 binding outranks missing token authority"
    );
    response.bytes().await.unwrap();
    let response = reqwest::Client::new()
        .post(format!(
            "http://{}/l/{unknown}/v1/responses",
            h.servers.data_addr
        ))
        .bearer_auth("synthetic-client")
        .header("session-id", "synthetic-unbound")
        .body(r#"{"model":"synthetic-model","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "launch_expired"
    );
    assert_eq!(
        h.fake
            .state
            .requests()
            .iter()
            .filter(|call| call.path_and_query == "/responses")
            .count(),
        2
    );
    let handle = launch
        .base_url
        .split("/l/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row.to_string().contains(handle) || row.to_string().contains(&unknown))
    );
}

#[tokio::test]
async fn r11_http_entry_unsupported_model_does_not_dispatch_or_replace_current_model() {
    let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    let response = reqwest::Client::new()
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "synthetic-unsupported")
        .body(r#"{"model":"not-advertised","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "model_not_supported"
    );
    assert!(
        !h.fake
            .state
            .requests()
            .iter()
            .any(|call| call.path_and_query == "/responses")
    );
    let info = h
        .state
        .bindings
        .launch_info("codex:synthetic-unsupported", Utc::now())
        .unwrap();
    assert_eq!(info.account, launch.account);
    assert_eq!(info.current_model.as_deref(), Some("synthetic-model"));
}

#[tokio::test]
async fn r11_http_entry_unusable_bound_account_moves_to_compatible_fallback() {
    let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    h.state
        .accounts
        .change(&launch.account, Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    h.fake.state.push(ok());
    let response = reqwest::Client::new()
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "synthetic-forced-seam")
        .body(r#"{"model":"synthetic-model","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    assert_ne!(
        h.state
            .bindings
            .current(Some("codex:synthetic-forced-seam"))
            .as_deref(),
        Some(launch.account.as_str())
    );
    assert_eq!(
        h.fake
            .state
            .requests()
            .iter()
            .filter(|call| call.path_and_query == "/responses")
            .count(),
        1
    );
}

async fn r11_http_warm_distinct(h: &Harness, models: &[&[&str]]) -> Vec<String> {
    let ids = h.ids();
    assert_eq!(ids.len(), models.len());
    // Isolate each fetch so a script can never become a different account's
    // catalog under concurrent issue collection.
    for (index, id) in ids.iter().enumerate() {
        for candidate in &ids {
            h.state
                .accounts
                .change(
                    candidate,
                    Some(if candidate == id {
                        UserState::Active
                    } else {
                        UserState::Paused
                    }),
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        h.fake.state.push(ScriptedResponse::json(StatusCode::OK,
            json!({"models": models[index].iter().map(|slug| json!({"slug": slug})).collect::<Vec<_>>()}).to_string()));
        let warm = h
            .state
            .issue_launch(
                qb_core::relay::LaunchRequest {
                    model: Some(models[index][0].into()),
                    model_is_explicit: true,
                    account_pref: Some(id.clone()),
                    resume_id: None,
                    move_for_model: false,
                    urgent: false,
                },
                h.servers.data_addr.port(),
            )
            .await
            .unwrap();
        assert_eq!(&warm.account, id);
    }
    for id in &ids {
        h.state
            .accounts
            .change(id, Some(UserState::Active), None, None, None)
            .await
            .unwrap();
    }
    ids
}

#[tokio::test]
async fn r11_http_entry_forced_move_filters_model_and_prefers_whole_advertised_catalog() {
    let h = Harness::new(Provider::Codex, 4, 64, 1024).await;
    let ids = r11_http_warm_distinct(
        &h,
        &[
            &["common", "menu"],
            &["different"],
            &["common"],
            &["common", "menu"],
        ],
    )
    .await;
    let launch = h
        .state
        .issue_launch(
            qb_core::relay::LaunchRequest {
                model: Some("common".into()),
                model_is_explicit: true,
                account_pref: Some(ids[0].clone()),
                resume_id: None,
                move_for_model: false,
                urgent: false,
            },
            h.servers.data_addr.port(),
        )
        .await
        .unwrap();
    assert_eq!(launch.account, ids[0]);
    // Resolve a real C8 key before recording the menu, just as /models does.
    h.fake.state.push(ok());
    let client = reqwest::Client::new();
    let send = || {
        client
            .post(format!("{}/responses", launch.base_url))
            .bearer_auth("synthetic-client")
            .header("session-id", "r11-compatible")
            .body(r#"{"model":"common","input":[]}"#)
    };
    let response = send().send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    h.state
        .bindings
        .record_launch_advertised(
            "codex:r11-compatible",
            &ids[0],
            &["common".to_owned(), "menu".to_owned()]
                .into_iter()
                .collect(),
            Utc::now(),
        )
        .unwrap();
    h.state
        .accounts
        .change(&ids[0], Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    h.fake.state.push(ok());
    let response = send().send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    assert_eq!(
        h.state.bindings.current(Some("codex:r11-compatible")),
        Some(ids[3].clone())
    );
    let requests = h.fake.state.requests();
    let responses: Vec<_> = requests
        .iter()
        .filter(|call| call.path_and_query == "/responses")
        .collect();
    assert_eq!(responses.len(), 2);
    assert_ne!(
        responses[0].headers["chatgpt-account-id"],
        responses[1].headers["chatgpt-account-id"]
    );
    assert_eq!(
        responses[1].body.as_ref(),
        br#"{"model":"common","input":[]}"#
    );
    let metadata = h
        .state
        .bindings
        .launch_info("codex:r11-compatible", Utc::now())
        .unwrap();
    assert_eq!(metadata.current_model.as_deref(), Some("common"));
    assert_eq!(metadata.advertised_models.len(), 2);
}

#[tokio::test]
async fn r11_http_entry_forced_move_never_uses_a_healthy_incompatible_account() {
    let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let ids = r11_http_warm_distinct(&h, &[&["common"], &["different"]]).await;
    let launch = h
        .state
        .issue_launch(
            qb_core::relay::LaunchRequest {
                model: Some("common".into()),
                model_is_explicit: true,
                account_pref: Some(ids[0].clone()),
                resume_id: None,
                move_for_model: false,
                urgent: false,
            },
            h.servers.data_addr.port(),
        )
        .await
        .unwrap();
    h.state
        .accounts
        .change(&ids[0], Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "r11-no-compatible")
        .body(r#"{"model":"common","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    response.bytes().await.unwrap();
    assert_eq!(
        h.state.bindings.current(Some("codex:r11-no-compatible")),
        Some(ids[0].clone())
    );
    assert!(
        !h.fake
            .state
            .requests()
            .iter()
            .any(|call| call.path_and_query == "/responses")
    );
}

#[tokio::test]
async fn r11_http_entry_new_model_refreshes_only_bound_catalog_before_dispatch() {
    let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    let before = h.fake.state.requests().len();
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        r#"{"models":[{"slug":"synthetic-model"},{"slug":"newly-added"}]}"#,
    ));
    h.fake.state.push(ok());
    let response = reqwest::Client::new()
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "r11-new-model")
        .body(r#"{"model":"newly-added","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let calls = h.fake.state.requests();
    let added = &calls[before..];
    assert_eq!(added.len(), 2);
    assert!(added[0].path_and_query.starts_with("/models"));
    assert_eq!(added[1].path_and_query, "/responses");
    assert_eq!(
        added[0].headers["chatgpt-account-id"],
        added[1].headers["chatgpt-account-id"]
    );
    let info = h
        .state
        .bindings
        .launch_info("codex:r11-new-model", Utc::now())
        .unwrap();
    assert_eq!(info.account, launch.account);
    assert_eq!(info.current_model.as_deref(), Some("newly-added"));
}

#[tokio::test]
async fn r11_http_entry_upstream_model_denial_refreshes_menu_without_shopping() {
    let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    let before = h.fake.state.requests().len();
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::BAD_REQUEST,
        r#"{"error":{"code":"model_not_found","message":"model is not supported when using Codex with a ChatGPT account"}}"#,
    ));
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        r#"{"models":[{"slug":"replacement"}]}"#,
    ));
    let response = reqwest::Client::new()
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "r11-denied-model")
        .body(r#"{"model":"synthetic-model","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "model_not_supported"
    );
    let calls = h.fake.state.requests();
    let added = &calls[before..];
    assert_eq!(added.len(), 2);
    assert_eq!(added[0].path_and_query, "/responses");
    assert!(added[1].path_and_query.starts_with("/models"));
    assert_eq!(
        added[0].headers["chatgpt-account-id"],
        added[1].headers["chatgpt-account-id"]
    );
    assert_eq!(
        h.state.bindings.current(Some("codex:r11-denied-model")),
        Some(launch.account)
    );
}

#[tokio::test]
async fn r11_http_entry_recovered_original_wins_after_destination_catalog_wait() {
    let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let ids = h.ids();
    h.state
        .accounts
        .change(&ids[1], Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        r#"{"models":[{"slug":"common"}]}"#,
    ));
    let launch = h
        .state
        .issue_launch(
            qb_core::relay::LaunchRequest {
                model: Some("common".into()),
                model_is_explicit: true,
                account_pref: Some(ids[0].clone()),
                resume_id: None,
                move_for_model: false,
                urgent: false,
            },
            h.servers.data_addr.port(),
        )
        .await
        .unwrap();
    assert_eq!(launch.account, ids[0]);
    h.state
        .accounts
        .change(&ids[0], Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    h.state
        .accounts
        .change(&ids[1], Some(UserState::Active), None, None, None)
        .await
        .unwrap();
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        r#"{"models":[{"slug":"common"}]}"#,
    ));
    h.fake.state.push(ok());
    let (arrived, release) = h.fake.state.pause_next_response();
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{}/responses", launch.base_url))
            .bearer_auth("synthetic-client")
            .header("session-id", "r11-recovered")
            .body(r#"{"model":"common","input":[]}"#)
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), arrived)
        .await
        .unwrap()
        .unwrap();
    let calls = h.fake.state.requests();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].path_and_query.starts_with("/models"));
    assert_ne!(
        calls[0].headers["chatgpt-account-id"],
        calls[1].headers["chatgpt-account-id"]
    );
    h.state
        .accounts
        .change(&ids[0], Some(UserState::Active), None, None, None)
        .await
        .unwrap();
    release.send(()).unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    assert_eq!(
        h.state.bindings.current(Some("codex:r11-recovered")),
        Some(ids[0].clone())
    );
    let calls = h.fake.state.requests();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].path_and_query, "/responses");
    assert_eq!(
        calls[0].headers["chatgpt-account-id"],
        calls[2].headers["chatgpt-account-id"]
    );
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "session_move" && row["reason"] == "forced")
    );
}

#[tokio::test]
async fn r11_http_entry_concurrent_launches_adopt_one_real_c8_binding() {
    let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let first = r11_http_issue(&h).await;
    let other = h.ids().into_iter().find(|id| *id != first.account).unwrap();
    let second = h
        .state
        .issue_launch(
            qb_core::relay::LaunchRequest {
                model: Some("synthetic-model".into()),
                model_is_explicit: true,
                account_pref: Some(other.clone()),
                resume_id: None,
                move_for_model: false,
                urgent: false,
            },
            h.servers.data_addr.port(),
        )
        .await
        .unwrap();
    assert_eq!(second.account, other);
    h.fake.state.push(ok());
    h.fake.state.push(ok());
    let client = reqwest::Client::new();
    let send = |base: String| {
        client
            .post(format!("{base}/responses"))
            .bearer_auth("synthetic-client")
            .header("session-id", "r11-simultaneous")
            .body(r#"{"model":"synthetic-model","input":[]}"#)
            .send()
    };
    let (a, b) = tokio::join!(send(first.base_url), send(second.base_url));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.status(), StatusCode::OK);
    assert_eq!(b.status(), StatusCode::OK);
    a.bytes().await.unwrap();
    b.bytes().await.unwrap();
    let canonical = h
        .state
        .bindings
        .launch_info("codex:r11-simultaneous", Utc::now())
        .unwrap();
    assert!(canonical.managed);
    assert_eq!(canonical.canonical_key, "codex:r11-simultaneous");
    let snapshot = h.state.bindings.export().unwrap();
    assert_eq!(snapshot["bindings"].as_array().unwrap().len(), 1);
    let calls = h.fake.state.requests();
    let sent: Vec<_> = calls
        .iter()
        .filter(|call| call.path_and_query == "/responses")
        .collect();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].headers["chatgpt-account-id"],
        sent[1].headers["chatgpt-account-id"]
    );
}

#[tokio::test]
async fn r11_http_entry_search_uses_launch_account_and_preserves_search_schema() {
    let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    let raw = r#"{ "query": "synthetic search", "limit": 3 }"#;
    h.fake.state.push(ok());
    let response = reqwest::Client::new()
        .post(format!("{}/alpha/search", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("x-codex-turn-metadata", r#"{"session_id":"r11-search"}"#)
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let calls = h.fake.state.requests();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].path_and_query, "/alpha/search");
    assert_eq!(calls[1].body.as_ref(), raw.as_bytes());
    assert_eq!(
        calls[0].headers["chatgpt-account-id"],
        calls[1].headers["chatgpt-account-id"]
    );
    assert_eq!(
        h.state.bindings.current(Some("codex:r11-search")),
        Some(launch.account)
    );
}

// Append to crates/qb-core/tests/m4a_http.rs after the existing R11 HTTP tests.
// Synthetic loopback only; uses the existing Harness and shared serial guard.
async fn r11_models_issue_raw(h: &Harness, raw: &Value) -> qb_core::relay::LaunchReply {
    for _ in h.ids() {
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, raw.to_string()));
    }
    h.state
        .issue_launch(
            qb_core::relay::LaunchRequest {
                model: Some("synthetic-model".into()),
                model_is_explicit: true,
                account_pref: None,
                resume_id: None,
                move_for_model: false,
                urgent: false,
            },
            h.servers.data_addr.port(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn r11_models_entry_raw_fields_bound_account_and_no_adoption() {
    let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
    let raw = json!({"models":[{"slug":"synthetic-model", "display_name":"Synthetic",
        "supported_reasoning_levels":[{"effort":"high","description":"fixture"}],
        "future_provider_field":{"nested":[1,true,"keep"]}}], "provider_extension":{"v":7}});
    let launch = r11_models_issue_raw(&h, &raw).await;
    let before = h.fake.state.requests().len();
    let response = reqwest::Client::new()
        .get(format!("{}/models", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "models-not-adopted")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-quota-baton-account"]
            .to_str()
            .unwrap(),
        launch.account
    );
    assert_eq!(response.json::<Value>().await.unwrap(), raw);
    assert_eq!(
        h.fake.state.requests().len(),
        before,
        "shared fresh catalog, no new fetch or shopping"
    );
    assert!(
        h.state
            .bindings
            .current(Some("codex:models-not-adopted"))
            .is_none()
    );
    let models_record = h
        .ledger()
        .into_iter()
        .find(|r| r["route"] == "GET /v1/models")
        .unwrap();
    assert_eq!(models_record["account"], launch.account);
    assert_eq!(models_record["attempts"], 0);
    assert!(models_record["upstream_status"].is_null());
    let token = launch
        .base_url
        .split("/l/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    assert!(!models_record.to_string().contains(token));
}

#[tokio::test]
async fn r11_models_entry_known_c8_outranks_unknown_token_and_records_before_reply() {
    let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
    let raw = json!({"models":[{"slug":"synthetic-model","opaque":{"preserve":true}}]});
    let launch = r11_models_issue_raw(&h, &raw).await;
    h.fake.state.push(ok());
    let client = reqwest::Client::new();
    let adopted = client
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "models-existing-c8")
        .body(r#"{"model":"synthetic-model","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(adopted.status(), StatusCode::OK);
    adopted.bytes().await.unwrap();
    let before = h.fake.state.requests().len();
    let unknown = "Z".repeat(43);
    let response = client
        .get(format!(
            "http://{}/l/{unknown}/v1/models",
            h.servers.data_addr
        ))
        .bearer_auth("synthetic-client")
        .header("session-id", "models-existing-c8")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-quota-baton-account"]
            .to_str()
            .unwrap(),
        launch.account
    );
    let info = h
        .state
        .bindings
        .launch_info("codex:models-existing-c8", Utc::now())
        .unwrap();
    assert_eq!(info.account, launch.account);
    assert!(info.advertised_models.contains("synthetic-model"));
    assert_eq!(response.json::<Value>().await.unwrap(), raw);
    assert_eq!(h.fake.state.requests().len(), before);
    assert!(!h.ledger().iter().any(|r| r.to_string().contains(&unknown)));
}

#[tokio::test]
async fn r11_models_entry_unknown_token_unbound_key_cannot_place_or_shop() {
    let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let unknown = "Z".repeat(43);
    let response = reqwest::Client::new()
        .get(format!(
            "http://{}/l/{unknown}/v1/models",
            h.servers.data_addr
        ))
        .bearer_auth("synthetic-client")
        .header("session-id", "models-never-place")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!response.headers().contains_key("x-quota-baton-account"));
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "launch_expired"
    );
    assert!(
        h.state
            .bindings
            .current(Some("codex:models-never-place"))
            .is_none()
    );
    assert!(h.fake.state.requests().is_empty());
    assert!(!h.ledger().iter().any(|r| r.to_string().contains(&unknown)));
}

#[tokio::test]
async fn r11_models_entry_ordinary_c8_fails_closed_without_managed_promotion() {
    let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
    h.fake.state.push(ok());
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{}/v1/responses", h.servers.data_addr))
        .bearer_auth("synthetic-client")
        .header("session-id", "models-ordinary")
        .body(r#"{"model":"synthetic-model","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let before = h
        .state
        .bindings
        .launch_info("codex:models-ordinary", Utc::now())
        .unwrap();
    assert!(!before.managed);
    let calls = h.fake.state.requests().len();
    let response = client
        .get(format!(
            "http://{}/l/{}/v1/models",
            h.servers.data_addr,
            "Z".repeat(43)
        ))
        .bearer_auth("synthetic-client")
        .header("session-id", "models-ordinary")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.headers().contains_key("x-quota-baton-account"));
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "launch_unavailable"
    );
    let after = h
        .state
        .bindings
        .launch_info("codex:models-ordinary", Utc::now())
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(h.fake.state.requests().len(), calls);
}

#[tokio::test]
async fn r11_models_entry_claude_legacy_route_unchanged() {
    let h = Harness::new(Provider::Claude, 1, 64, 1024).await;
    let raw = json!({"data":[{"id":"synthetic-claude","type":"model"}],"has_more":false});
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, raw.to_string()));
    let response = reqwest::Client::new()
        .get(format!("http://{}/v1/models", h.servers.data_addr))
        .bearer_auth("synthetic-client")
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap(), raw);
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_eq!(h.fake.state.requests()[0].path_and_query, "/v1/models");
}

#[tokio::test]
async fn r11_http_entry_compressed_model_peek_preserves_wire_bytes() {
    use std::io::Write;
    let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    let raw = br#"{ "model":"synthetic-model", "input":[{"role":"user","content":"fixture"}] }"#;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(raw).unwrap();
    let wire = encoder.finish().unwrap();
    h.fake.state.push(ok());
    let response = reqwest::Client::new()
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "r11-gzip")
        .header("content-encoding", "gzip")
        .body(wire.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let calls = h.fake.state.requests();
    let sent = calls
        .iter()
        .find(|call| call.path_and_query == "/responses")
        .unwrap();
    assert_eq!(sent.body.as_ref(), wire);
    assert_eq!(sent.headers["content-encoding"], "gzip");
    assert_eq!(
        h.state.bindings.current(Some("codex:r11-gzip")),
        Some(launch.account)
    );
}

#[tokio::test]
async fn r11_http_entry_invalid_model_metadata_never_dispatches() {
    let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    let before = h.fake.state.requests().len();
    let client = reqwest::Client::new();
    for raw in [
        r#"{"model":"synthetic-model","model":"other","input":[]}"#,
        r#"{"model":null,"input":[]}"#,
        r#"{"model":42,"input":[]}"#,
        r#"{"model":"synthetic-model","input":[],bad}"#,
    ] {
        let response = client
            .post(format!("{}/responses", launch.base_url))
            .bearer_auth("synthetic-client")
            .header("session-id", "r11-invalid-model")
            .body(raw)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        response.bytes().await.unwrap();
    }
    assert_eq!(h.fake.state.requests().len(), before);
}

#[tokio::test]
async fn r11_http_entry_model_denial_keeps_hold_when_catalog_does_not_remove_model() {
    for refresh_fails in [false, true] {
        let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
        let launch = r11_http_issue(&h).await;
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"The 'synthetic-model' model is not supported when using Codex with a ChatGPT account"}}"#,
        ));
        h.fake.state.push(ScriptedResponse::json(
            if refresh_fails {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::OK
            },
            if refresh_fails {
                "{}"
            } else {
                r#"{"models":[{"slug":"synthetic-model"}]}"#
            },
        ));
        let client = reqwest::Client::new();
        let send = || {
            client
                .post(format!("{}/responses", launch.base_url))
                .bearer_auth("synthetic-client")
                .header("session-id", "r11-model-hold")
                .body(r#"{"model":"synthetic-model","input":[]}"#)
                .send()
        };
        let response = send().await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        response.bytes().await.unwrap();
        let account = h.store.load(&launch.account).unwrap();
        assert!(!account.eligible(Some("synthetic-model")));
        assert!(account.eligible(Some("different-model")));
        assert!(matches!(
            account.health,
            Health::Cooling {
                reason: HealthReason::Permission,
                ..
            }
        ));
        let response = tokio::time::timeout(Duration::from_secs(10), send())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        assert_eq!(
            h.fake
                .state
                .requests()
                .iter()
                .filter(|r| r.path_and_query == "/responses")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn r11_http_entry_destination_pause_before_admission_never_publishes_move() {
    let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let ids = r11_http_warm_distinct(&h, &[&["common"], &["common"]]).await;
    let launch = h
        .state
        .issue_launch(
            qb_core::relay::LaunchRequest {
                model: Some("common".into()),
                model_is_explicit: true,
                account_pref: Some(ids[0].clone()),
                resume_id: None,
                move_for_model: false,
                urgent: false,
            },
            h.servers.data_addr.port(),
        )
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let send = || {
        client
            .post(format!("{}/responses", launch.base_url))
            .bearer_auth("synthetic-client")
            .header("session-id", "r11-paused-target")
            .body(r#"{"model":"common","input":[]}"#)
            .send()
    };
    h.fake.state.push(ok());
    let initial = send().await.unwrap();
    assert_eq!(initial.status(), StatusCode::OK);
    initial.bytes().await.unwrap();
    h.state
        .accounts
        .change(&ids[0], Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    let before = h.fake.state.requests().len();
    let (collected, release) = h.state.pause_next_ws_selection();
    let pending = send();
    let mutate = async {
        tokio::time::timeout(Duration::from_secs(5), collected)
            .await
            .unwrap()
            .unwrap();
        h.state
            .accounts
            .change(&ids[1], Some(UserState::Paused), None, None, None)
            .await
            .unwrap();
        release.send(()).unwrap();
    };
    let (response, ()) = tokio::join!(pending, mutate);
    let response = response.unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    assert_eq!(
        h.fake.state.requests().len(),
        before,
        "no dispatch after destination pause"
    );
    assert_eq!(
        h.state.bindings.current(Some("codex:r11-paused-target")),
        Some(ids[0].clone()),
        "a send guard alone must not leave a move to paused B"
    );
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "session_move" && row["reason"] == "forced")
    );
}

// Reconstruct the relay and both listeners, not just a second BindingTable.
// The synthetic account store, ledger writer and upstream remain fixture-owned.
async fn r11_http_restart_fixture() -> (
    Harness,
    qb_core::relay::LaunchReply,
    qb_core::bindings::LaunchInfo,
) {
    let mut h = Harness::new(Provider::Codex, 2, 64, 1024).await;
    let raw = json!({"models":[{"slug":"synthetic-model"},{"slug":"menu-model"}]});
    let launch = r11_models_issue_raw(&h, &raw).await;
    let client = reqwest::Client::new();
    h.fake.state.push(ok());
    let response = client
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "r11-persisted-resume")
        .body(r#"{"model":"synthetic-model","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let response = client
        .get(&launch.catalog_url)
        .bearer_auth("synthetic-client")
        .header("session-id", "r11-persisted-resume")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap(), raw);
    drop(client);
    let persisted = h
        .state
        .bindings
        .launch_info("codex:r11-persisted-resume", Utc::now())
        .unwrap();
    assert_eq!(persisted.account, launch.account);
    assert_eq!(persisted.canonical_key, "codex:r11-persisted-resume");
    assert_eq!(persisted.current_model.as_deref(), Some("synthetic-model"));
    assert_eq!(
        persisted.advertised_models,
        ["synthetic-model".to_owned(), "menu-model".to_owned()]
            .into_iter()
            .collect()
    );
    // Retire the old snapshot writer before constructing the replacement.
    h.state.bindings.flush().unwrap();
    h.state.bindings.freeze().unwrap();
    h.obs.flush().unwrap();
    h.servers.shutdown();
    tokio::time::timeout(Duration::from_secs(5), &mut h.servers.data_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut h.servers.admin_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut config = Config::default();
    // Reuse the data port so the original handle URL is exercised verbatim.
    config.server.data_port = h.servers.data_addr.port();
    config.server.admin_port = 0;
    config.guard.inflight_budget_mib = 64;
    config.guard.memory_hard_limit_mib = 1024;
    config.upstream.codex_base = h.fake.url.clone();
    config.upstream.anthropic_base = h.fake.url.clone();
    h.state = Arc::new(
        RelayState::new(
            h.store.clone(),
            &config,
            h.obs.clone(),
            Arc::new(DebugPins::default()),
        )
        .unwrap()
        .with_token_endpoint(Provider::Codex, h.fake.url.clone()),
    );
    h.servers = http::spawn_with_routes(
        &config,
        LocalSecrets {
            client_key: "synthetic-client".into(),
            admin_token: "synthetic-admin".into(),
        },
        relay::router(h.state.clone()),
        qb_core::admin::runtime_routes(h.state.clone(), 0),
    )
    .await
    .unwrap();
    assert_eq!(
        h.state
            .bindings
            .launch_info("codex:r11-persisted-resume", Utc::now())
            .unwrap(),
        persisted,
        "account, model, menu and canonical identity survive disk reload"
    );
    (h, launch, persisted)
}

#[tokio::test]
async fn r11_http_entry_reconstructed_relay_uses_persisted_c8_without_runtime_handle() {
    let (h, launch, persisted) = r11_http_restart_fixture().await;
    let client = reqwest::Client::new();
    let before = h.fake.state.requests().len();
    let routing = h.state.bindings.routing_view();
    let bindings = h.state.bindings.export().unwrap();
    let moves: Vec<_> = h
        .ledger()
        .into_iter()
        .filter(|row| row["event"] == "session_move")
        .collect();
    // The original handle really lost its runtime authority. Both it and an
    // unknown handle must refuse catalog/new-session requests without known C8.
    for base in [
        launch.base_url.clone(),
        format!("http://{}/l/{}/v1", h.servers.data_addr, "Z".repeat(43)),
    ] {
        for session in [None, Some("r11-not-persisted")] {
            for models in [true, false] {
                let mut request = if models {
                    client.get(format!("{base}/models"))
                } else {
                    client
                        .post(format!("{base}/responses"))
                        .body(r#"{"model":"synthetic-model","input":[]}"#)
                }
                .bearer_auth("synthetic-client");
                if let Some(session) = session {
                    request = request.header("session-id", session);
                }
                let response = request.send().await.unwrap();
                assert_eq!(response.status(), StatusCode::CONFLICT);
                assert_eq!(
                    response.json::<Value>().await.unwrap()["error"]["type"],
                    "launch_expired"
                );
            }
        }
    }
    assert_eq!(h.fake.state.requests().len(), before);
    assert_eq!(h.state.bindings.export().unwrap(), bindings);

    h.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        r#"{"models":[{"slug":"synthetic-model"},{"slug":"menu-model"}]}"#,
    ));
    let response = client
        .get(&launch.catalog_url)
        .bearer_auth("synthetic-client")
        .header("session-id", "r11-persisted-resume")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-quota-baton-account"]
            .to_str()
            .unwrap(),
        launch.account
    );
    assert_eq!(
        response.json::<Value>().await.unwrap()["models"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        h.state
            .bindings
            .launch_info("codex:r11-persisted-resume", Utc::now())
            .unwrap(),
        persisted,
        "/models reads the persisted binding without placing or moving"
    );
    h.fake.state.push(ok());
    let response = client
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "r11-persisted-resume")
        .body(r#"{"model":"synthetic-model","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let calls = h.fake.state.requests();
    assert_eq!(calls.len(), before + 2);
    assert!(calls[before].path_and_query.starts_with("/models"));
    assert_eq!(calls[before + 1].path_and_query, "/responses");
    let original = calls
        .iter()
        .find(|call| call.path_and_query == "/responses")
        .unwrap();
    for call in &calls[before..] {
        assert_eq!(
            call.headers["chatgpt-account-id"],
            original.headers["chatgpt-account-id"]
        );
    }
    assert_eq!(h.state.bindings.routing_view(), routing);
    assert_eq!(
        h.state.bindings.export().unwrap()["bindings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.ledger()
            .into_iter()
            .filter(|row| row["event"] == "session_move")
            .collect::<Vec<_>>(),
        moves
    );
}

#[tokio::test]
async fn r11_http_entry_simultaneous_resume_issuance_preserves_persisted_authority() {
    let (h, _old_launch, persisted) = r11_http_restart_fixture().await;
    let other = h
        .ids()
        .into_iter()
        .find(|id| *id != persisted.account)
        .unwrap();
    let anchors_before = h.state.bindings.export().unwrap()["launch_anchors"]
        .as_object()
        .unwrap()
        .len();
    let routing = h.state.bindings.routing_view();
    let moves: Vec<_> = h
        .ledger()
        .into_iter()
        .filter(|row| row["event"] == "session_move")
        .collect();
    let before = h.fake.state.requests().len();
    // Cold catalogs after reconstruction, with a deterministic upstream gate:
    // both issuance futures are polled while the first fetch is held.
    for _ in h.ids() {
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::OK,
            r#"{"models":[{"slug":"synthetic-model"},{"slug":"menu-model"}]}"#,
        ));
    }
    let (arrived, release) = h.fake.state.pause_next_response();
    let issue = || {
        h.state.issue_launch(
            qb_core::relay::LaunchRequest {
                model: None,
                model_is_explicit: false,
                account_pref: Some(other.clone()),
                resume_id: Some("r11-persisted-resume".into()),
                move_for_model: false,
                urgent: false,
            },
            h.servers.data_addr.port(),
        )
    };
    let (first, second, ()) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(issue(), issue(), async {
            tokio::time::timeout(Duration::from_secs(5), arrived)
                .await
                .unwrap()
                .unwrap();
            release.send(()).unwrap();
        })
    })
    .await
    .unwrap();
    let first = first.unwrap();
    let second = second.unwrap();
    assert_ne!(first.base_url, second.base_url);
    for launch in [&first, &second] {
        assert_eq!(launch.account, persisted.account);
        assert_eq!(launch.model, persisted.current_model);
        h.fake.state.push(ok());
    }
    let client = reqwest::Client::new();
    // No C8 header: each new handle must independently resolve to the same
    // persisted canonical binding, not merely be overridden by a known key.
    let send = |base: &str| {
        client
            .post(format!("{base}/responses"))
            .bearer_auth("synthetic-client")
            .body(r#"{"model":"synthetic-model","input":[]}"#)
            .send()
    };
    let (a, b) = tokio::join!(send(&first.base_url), send(&second.base_url));
    for response in [a.unwrap(), b.unwrap()] {
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
    }
    assert_eq!(
        h.state
            .bindings
            .launch_info("codex:r11-persisted-resume", Utc::now())
            .unwrap(),
        persisted
    );
    let snapshot = h.state.bindings.export().unwrap();
    assert_eq!(snapshot["bindings"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["launches"].as_object().unwrap().len(), 1);
    assert_eq!(
        snapshot["launch_anchors"].as_object().unwrap().len(),
        anchors_before + 2
    );
    for anchor in snapshot["launch_anchors"].as_object().unwrap().values() {
        assert_eq!(anchor["canonical_key"], persisted.canonical_key);
    }
    let calls = h.fake.state.requests();
    let original = calls
        .iter()
        .find(|call| call.path_and_query == "/responses")
        .unwrap();
    let sent: Vec<_> = calls[before..]
        .iter()
        .filter(|call| call.path_and_query == "/responses")
        .collect();
    assert_eq!(sent.len(), 2);
    for call in sent {
        assert_eq!(
            call.headers["chatgpt-account-id"],
            original.headers["chatgpt-account-id"]
        );
    }
    assert_eq!(h.state.bindings.routing_view(), routing);
    assert_eq!(
        h.ledger()
            .into_iter()
            .filter(|row| row["event"] == "session_move")
            .collect::<Vec<_>>(),
        moves
    );
}

fn r11_http_assert_adopted_snapshot(
    h: &Harness,
    provisional: &str,
    canonical: &str,
    account: &str,
) {
    assert!(
        h.state.bindings.current(Some(provisional)).is_none(),
        "a stale selection must not recreate the retired provisional key"
    );
    let current = h
        .state
        .bindings
        .launch_info(provisional, Utc::now())
        .unwrap();
    assert!(current.managed);
    assert_eq!(current.canonical_key, canonical);
    assert_eq!(current.account, account);
    let snapshot = h.state.bindings.export().unwrap();
    let bindings = snapshot["bindings"].as_array().unwrap();
    assert_eq!(
        bindings.len(),
        1,
        "adoption must not recreate a launch group"
    );
    assert_eq!(bindings[0]["key"], canonical);
    assert_eq!(snapshot["launches"].as_object().unwrap().len(), 1);
    for anchor in snapshot["launch_anchors"].as_object().unwrap().values() {
        assert_eq!(anchor["canonical_key"], canonical);
    }
    h.state.bindings.flush().unwrap();
    let restored = qb_core::bindings::BindingTable::new(&h.home, RoutingConfig::default()).unwrap();
    assert_eq!(
        restored.launch_info(provisional, Utc::now()).unwrap(),
        current,
        "the adopted snapshot must reload with its model and identity intact"
    );
}

#[tokio::test]
async fn r11_http_entry_adoption_during_model_refresh_keeps_one_reloadable_binding() {
    let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    let snapshot = h.state.bindings.export().unwrap();
    let provisional = snapshot["bindings"][0]["key"].as_str().unwrap().to_owned();
    assert!(provisional.starts_with("launch:"));
    let canonical = "codex:r11-refresh-adoption";
    // The pending request resolves its provisional Session before this forced
    // catalog refresh. The C8 request below adopts it while that I/O is held.
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        r#"{"models":[{"slug":"synthetic-model"},{"slug":"new-model"}]}"#,
    ));
    h.fake.state.push(ok());
    h.fake.state.push(ok());
    let (arrived, release) = h.fake.state.pause_next_response();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let pending_raw = r#" { "model":"new-model", "input":[], "marker":"pending" } "#;
    let adopting_raw = r#"{"model":"synthetic-model","input":[],"marker":"adopting"}"#;
    let pending = client
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .body(pending_raw)
        .send();
    tokio::pin!(pending);
    tokio::select! {
        _ = &mut pending => panic!("pending request escaped the catalog gate"),
        result = tokio::time::timeout(Duration::from_secs(5), arrived) => {
            result.unwrap().unwrap();
        }
    }
    let calls = h.fake.state.requests();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].path_and_query.starts_with("/models"));
    let adopting = client
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .header("session-id", "r11-refresh-adoption")
        .body(adopting_raw)
        .send();
    tokio::pin!(adopting);
    let adopted = async {
        loop {
            if h.state.bindings.current(Some(canonical)).as_deref() == Some(launch.account.as_str())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        _ = &mut pending => panic!("pending request escaped before adoption"),
        _ = &mut adopting => panic!("C8 request escaped the in-flight catalog refresh"),
        result = tokio::time::timeout(Duration::from_secs(5), adopted) => {
            result.unwrap();
        }
    }
    assert!(h.state.bindings.current(Some(&provisional)).is_none());
    release.send(()).unwrap();
    let (pending, adopting) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(&mut pending, &mut adopting)
    })
    .await
    .unwrap();
    let pending = pending.unwrap();
    let adopting = adopting.unwrap();
    let statuses = [pending.status(), adopting.status()];
    for response in [pending, adopting] {
        tokio::time::timeout(Duration::from_secs(5), response.bytes())
            .await
            .unwrap()
            .unwrap();
    }
    r11_http_assert_adopted_snapshot(&h, &provisional, canonical, &launch.account);
    for status in statuses {
        assert_eq!(status, StatusCode::OK);
    }
    let calls = h.fake.state.requests();
    let sent: Vec<_> = calls
        .iter()
        .filter(|call| call.path_and_query == "/responses")
        .collect();
    assert_eq!(sent.len(), 2);
    for raw in [pending_raw, adopting_raw] {
        assert!(sent.iter().any(|call| call.body.as_ref() == raw.as_bytes()));
    }
    for call in sent {
        assert_eq!(
            call.headers["chatgpt-account-id"],
            calls[0].headers["chatgpt-account-id"]
        );
    }
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn r11_http_entry_adoption_after_selection_snapshot_rejects_stale_publication() {
    let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
    let launch = r11_http_issue(&h).await;
    let snapshot = h.state.bindings.export().unwrap();
    let provisional = snapshot["bindings"][0]["key"].as_str().unwrap().to_owned();
    assert!(provisional.starts_with("launch:"));
    let canonical = "codex:r11-selection-adoption";
    // Managed HTTP shares this selector hook with WS. Pause after the fresh
    // launch snapshot, before the mutation/admission gate, with no lock held.
    let (arrived, release) = h.state.pause_next_ws_selection();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let raw = r#"{"model":"synthetic-model","input":[]}"#;
    let pending = client
        .post(format!("{}/responses", launch.base_url))
        .bearer_auth("synthetic-client")
        .body(raw)
        .send();
    tokio::pin!(pending);
    tokio::select! {
        _ = &mut pending => panic!("request escaped the selection gate"),
        result = tokio::time::timeout(Duration::from_secs(5), arrived) => {
            result.unwrap().unwrap();
        }
    }
    h.fake.state.push(ok());
    let adopting = tokio::time::timeout(
        Duration::from_secs(5),
        client
            .post(format!("{}/responses", launch.base_url))
            .bearer_auth("synthetic-client")
            .header("session-id", "r11-selection-adoption")
            .body(raw)
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(adopting.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(5), adopting.bytes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        h.state.bindings.current(Some(canonical)).as_deref(),
        Some(launch.account.as_str())
    );
    release.send(()).unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), &mut pending)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(5), response.bytes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        h.fake
            .state
            .requests()
            .iter()
            .filter(|call| call.path_and_query == "/responses")
            .count(),
        1,
        "a stale choice cannot dispatch after the binding was adopted"
    );
    r11_http_assert_adopted_snapshot(&h, &provisional, canonical, &launch.account);
}

include!("support/m4a_http_r11_review.inc.rs");

mod n5b_sse {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
        response::Response,
    };
    use futures_util::Stream;
    use hyper::{body::Incoming, server::conn::http1, service::service_fn};
    use hyper_util::rt::TokioIo;
    use std::{
        convert::Infallible,
        io,
        net::Shutdown,
        pin::Pin,
        sync::{Mutex, atomic::Ordering},
        task::{Context, Poll},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, BufReader},
        net::{TcpListener, TcpStream},
        sync::oneshot,
        task::JoinHandle,
        time::timeout,
    };

    const PHASE: Duration = Duration::from_secs(3);
    const UPLOAD: &str = r#"{"input":[],"stream":true}"#;
    const FIRST: &[u8] = b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"synthetic-sse\",\"status\":\"in_progress\"}}\n\n";

    // This stream has no terminal event, EOF, timer, or producer that can finish it.
    // Its Drop signal is owned by the actual upstream HTTP response body.
    struct StalledBody {
        first: Option<Bytes>,
        stalled: Option<oneshot::Sender<()>>,
        dropped: Option<oneshot::Sender<()>>,
    }

    impl Stream for StalledBody {
        type Item = Result<Bytes, io::Error>;

        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if let Some(bytes) = self.first.take() {
                return Poll::Ready(Some(Ok(bytes)));
            }
            if let Some(stalled) = self.stalled.take() {
                let _ = stalled.send(());
            }
            Poll::Pending
        }
    }

    impl Drop for StalledBody {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    struct Signals {
        request: oneshot::Receiver<(Method, String, Bytes)>,
        stalled: oneshot::Receiver<()>,
        dropped: oneshot::Receiver<()>,
        closed: oneshot::Receiver<()>,
    }

    struct StalledUpstream {
        url: String,
        task: JoinHandle<()>,
    }

    impl StalledUpstream {
        async fn start() -> (Self, Signals) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            assert!(address.ip().is_loopback());
            let (request_tx, request) = oneshot::channel();
            let (stalled_tx, stalled) = oneshot::channel();
            let (dropped_tx, dropped) = oneshot::channel();
            let (closed_tx, closed) = oneshot::channel();
            let exchange = Arc::new(Mutex::new(Some((
                request_tx,
                StalledBody {
                    first: Some(Bytes::from_static(FIRST)),
                    stalled: Some(stalled_tx),
                    dropped: Some(dropped_tx),
                },
            ))));
            let task = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let service = service_fn(move |request: Request<Incoming>| {
                    let (seen, stream) = exchange
                        .lock()
                        .unwrap()
                        .take()
                        .expect("the SSE fixture accepts exactly one upstream request");
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = to_bytes(Body::new(body), 4096).await.unwrap();
                        let _ = seen.send((parts.method, parts.uri.path().to_owned(), body));
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "text/event-stream")
                                .header("connection", "close")
                                .body(Body::from_stream(stream))
                                .unwrap(),
                        )
                    }
                });
                // Await the real connection directly: aborting this task is not
                // used to satisfy either closure signal or any assertion.
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
                let _ = closed_tx.send(());
            });
            (
                Self {
                    url: format!("http://{address}"),
                    task,
                },
                Signals {
                    request,
                    stalled,
                    dropped,
                    closed,
                },
            )
        }
    }

    impl Drop for StalledUpstream {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn line(reader: &mut BufReader<TcpStream>, limit: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        for _ in 0..limit {
            bytes.push(
                reader
                    .read_u8()
                    .await
                    .expect("incomplete HTTP fixture line"),
            );
            if bytes.ends_with(b"\r\n") {
                return bytes;
            }
        }
        panic!("HTTP fixture line exceeded its bound");
    }

    async fn first_event(reader: &mut BufReader<TcpStream>) {
        let mut headers = Vec::new();
        loop {
            let next = line(reader, 2048).await;
            headers.extend_from_slice(&next);
            assert!(
                headers.len() <= 16 * 1024,
                "HTTP headers exceeded fixture bound"
            );
            if next == b"\r\n" {
                break;
            }
        }
        let headers = String::from_utf8(headers).unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 "), "{headers}");
        let headers = headers.to_ascii_lowercase();
        assert!(headers.contains("\r\ncontent-type: text/event-stream\r\n"));
        assert!(headers.contains("\r\ntransfer-encoding: chunked\r\n"));
        let mut body = Vec::new();
        while body.len() < FIRST.len() {
            let size = String::from_utf8(line(reader, 64).await).unwrap();
            let size = usize::from_str_radix(size.trim_end().split(';').next().unwrap(), 16)
                .expect("invalid HTTP chunk length");
            assert!(size > 0 && size <= FIRST.len() - body.len());
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..]).await.unwrap();
            let mut delimiter = [0; 2];
            reader.read_exact(&mut delimiter).await.unwrap();
            assert_eq!(delimiter, *b"\r\n");
        }
        assert_eq!(body.as_slice(), FIRST, "SSE body bytes changed");
    }

    #[tokio::test]
    async fn n5b_sse_stalled_body_os_close_drops_upstream_and_releases_native_owners() {
        let (mut upstream, signals) = StalledUpstream::start().await;
        let h = Harness::new_configured(
            Provider::Codex,
            1,
            64,
            1024,
            false,
            RoutingConfig::default(),
            |config| config.upstream.codex_base = upstream.url.clone(),
        )
        .await;
        let account = h.ids().into_iter().next().unwrap();
        assert_eq!(h.state.budget.used(), 0);
        assert_eq!(h.state.inflight_requests.load(Ordering::Acquire), 0);
        assert_eq!(h.state.accounts.inflight_count(&account), 0);
        assert_eq!(h.state.bindings.pending_count(&account), 0);
        assert!(h.servers.data_addr.ip().is_loopback());
        let mut socket = timeout(PHASE, TcpStream::connect(h.servers.data_addr))
            .await
            .unwrap()
            .unwrap();
        let request = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer synthetic-client\r\nContent-Type: application/json\r\nsession-id: synthetic-n5b-sse-close\r\nContent-Length: {}\r\n\r\n{UPLOAD}",
            h.servers.data_addr,
            UPLOAD.len()
        );
        timeout(PHASE, socket.write_all(request.as_bytes()))
            .await
            .unwrap()
            .unwrap();
        let mut reader = BufReader::new(socket);
        timeout(PHASE, first_event(&mut reader))
            .await
            .expect("actual HTTP client must receive the complete nonterminal SSE event");
        let (method, path, body) = timeout(PHASE, signals.request).await.unwrap().unwrap();
        assert_eq!(method, Method::POST);
        assert_eq!(path, "/responses");
        assert_eq!(
            body.as_ref(),
            UPLOAD.as_bytes(),
            "request body bytes changed"
        );
        timeout(PHASE, signals.stalled)
            .await
            .expect("upstream body must be polled into its indefinite stall")
            .unwrap();
        assert_eq!(h.state.inflight_requests.load(Ordering::Acquire), 1);
        assert_eq!(h.state.accounts.inflight_count(&account), 1);
        assert!(
            h.ledger()
                .iter()
                .all(|row| row["route"] != "POST /v1/responses"),
            "nonterminal SSE bytes must not finish the native request"
        );

        // Close the actual OS socket. Keep both servers and the account runtime
        // alive; neither a response Drop shortcut nor server teardown closes it.
        let socket = reader.into_inner().into_std().unwrap();
        socket.shutdown(Shutdown::Both).unwrap();
        drop(socket);
        timeout(PHASE, signals.dropped)
            .await
            .expect("SSE client close must drop upstream before teardown")
            .expect("upstream body Drop signal abandoned");
        timeout(PHASE, signals.closed)
            .await
            .expect("SSE client close must close the real upstream connection")
            .unwrap();
        timeout(PHASE, &mut upstream.task).await.unwrap().unwrap();
        timeout(PHASE, async {
            loop {
                if h.state.inflight_requests.load(Ordering::Acquire) == 0
                    && h.state.accounts.inflight_count(&account) == 0
                    && h.state.bindings.pending_count(&account) == 0
                    && h.state.budget.used() == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("SSE client close must release native owners before teardown");
        assert_eq!(h.state.budget.used(), 0);
        assert_eq!(h.state.inflight_requests.load(Ordering::Acquire), 0);
        assert_eq!(h.state.accounts.inflight_count(&account), 0);
        assert_eq!(h.state.bindings.pending_count(&account), 0);
        let rows = h.ledger();
        let requests = rows
            .iter()
            .filter(|row| row["route"] == "POST /v1/responses")
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 1);
        let row = requests[0];
        assert_eq!(row["transport"], "http");
        assert_eq!(row["account"], account);
        assert_eq!(row["status"], 200);
        assert_eq!(row["upstream_status"], 200);
        assert_eq!(row["attempts"], 1);
        assert_eq!(row["outcome"], "client_drop");
        assert_eq!(row["bytes_in"], UPLOAD.len() as u64);
        assert_eq!(row["bytes_out"], FIRST.len() as u64);
        assert!(h.fake.state.requests().is_empty());
        assert!(
            !h.servers.data_task.is_finished(),
            "relay ended before assertions"
        );
    }
}
