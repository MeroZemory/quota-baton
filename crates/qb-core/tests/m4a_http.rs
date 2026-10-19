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
    for session in ["billing", "untouched"] {
        c6_4_assert_served(
            h.send_with_session(Provider::Claude, session).await,
            &h,
            &ids[0],
        )
        .await;
    }
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
async fn c6_3_admission_wait_exhausted_returns_local_429_without_a_send() {
    let h = Harness::new(Provider::Claude, 1, 0, 1024).await;
    let id = h.ids()[0].clone();
    h.state.accounts.set_rate_hold_secs(1);
    h.state
        .accounts
        .observe_429(&id, Some(Duration::from_secs(60)));
    h.fake.state.push(ok());
    let response = h.send(Provider::Claude, "{}").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(h.fake.state.requests().len(), 0);
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
        // C7 keeps unbound traffic on its last account; each round deliberately
        // starts a new binding to exercise the first account's refresh throttle.
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
    assert_eq!(headers["x-quota-baton-account"], account);
    if initial_budget_exhausted {
        let advice = headers["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!((28..=30).contains(&advice));
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["type"],
            "rate_wait_timeout"
        );
    } else {
        assert_eq!(headers["retry-after"], "30");
        assert_eq!(body, first_body);
    }
    let expected_attempts = usize::from(!initial_budget_exhausted);
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), expected_attempts);
    if !initial_budget_exhausted {
        let first_send_wait = requests[0].received_at.saturating_duration_since(started);
        assert!(
            (Duration::from_millis(450)..=Duration::from_millis(900)).contains(&first_send_wait),
            "initial wait must spend the original budget"
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
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(
        row["relayed_rate_429s"],
        u64::from(!initial_budget_exhausted)
    );
    assert_eq!(row["retained_copy"], true);
    assert_eq!(row["account"], account);
    assert_eq!(row["status"], 429);
    assert_eq!(
        row["outcome"],
        if initial_budget_exhausted {
            "rate_wait_timeout"
        } else {
            "rate_hold_relayed_429"
        }
    );
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
async fn r1_http_total_hold_initial_admission_expiry_never_forces_a_send() {
    r1_http_total_hold_case(true).await;
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
