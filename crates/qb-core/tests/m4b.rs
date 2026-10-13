//! C7/C8 regressions through loopback HTTP and the runtime admin router.
use axum::http::{HeaderMap, StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use qb_core::{
    accounts::{AccountStore, Health, HealthReason, Provider, QuotaCooling, import},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(8);
const RAW: &str = r#"{ "input": "conversation-marker", "encrypted_content": "verbatim" }"#;
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    servers: RunningServers,
    state: Arc<RelayState>,
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    fake: ScriptedUpstream,
    client: reqwest::Client,
    home: PathBuf,
    _root: tempfile::TempDir,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Harness {
    async fn new(provider: Provider) -> Self {
        Self::with_hold(provider, 60).await
    }

    async fn with_hold(provider: Provider, reset_hold_secs: u64) -> Self {
        // Waiting behind other bounded cases is separate from the HTTP deadline.
        let serial = tokio::time::timeout(Duration::from_secs(120), SERIAL.lock())
            .await
            .unwrap();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m4b");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let cpa = root.path().join("cpa");
        fs::create_dir_all(&cpa).unwrap();
        let mut activate = Vec::new();
        for i in 0..2 {
            let email = format!("m4b-{i}@example.invalid");
            let account = json!({
                "type": provider.as_str(), "email": email,
                "account_id": format!("synthetic-{i}"), "account_uuid": format!("uuid-{i}"),
                "organization_uuid": "org-fixture", "access_token": format!("synthetic-access-{i}"),
                "refresh_token": format!("synthetic-refresh-{i}"), "id_token": "synthetic-id",
                "expired": "2099-01-01T00:00:00Z", "last_refresh": "2026-01-01T00:00:00Z"
            });
            fs::write(
                cpa.join(format!("{i}.json")),
                serde_json::to_vec(&account).unwrap(),
            )
            .unwrap();
            activate.push(format!("{}:{email}", provider.as_str()));
        }
        let store = tokio::time::timeout(DEADLINE, async {
            loop {
                match AccountStore::acquire(&home) {
                    Ok(store) => break Arc::new(store),
                    Err(qb_core::accounts::StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("synthetic store: {error}"),
                }
            }
        })
        .await
        .unwrap();
        import::run(&store, &cpa, &activate, false).unwrap();
        let fake = tokio::time::timeout(DEADLINE, ScriptedUpstream::start())
            .await
            .unwrap()
            .unwrap();
        let mut config = Config::default();
        config.routing.reset_hold_secs = reset_hold_secs;
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.anthropic_base = fake.url.clone();
        config.upstream.codex_base = fake.url.clone();
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
        let servers = tokio::time::timeout(
            DEADLINE,
            http::spawn_with_routes(
                &config,
                LocalSecrets {
                    client_key: "synthetic-client".into(),
                    admin_token: "synthetic-admin".into(),
                },
                relay::router(state.clone()),
                qb_core::admin::runtime_routes(state.clone(), 0),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(DEADLINE)
            .build()
            .unwrap();
        let harness = Self {
            servers,
            state,
            store,
            obs,
            fake,
            client,
            home,
            _root: root,
            _serial: serial,
        };
        for id in harness.ids() {
            harness
                .state
                .accounts
                .observe_headers(&id, provider, &usage(provider, 10))
                .await;
        }
        harness
    }

    async fn restart(self) -> Self {
        self.restart_with_fixture(|_| {}).await
    }

    async fn restart_with_fixture(self, edit: impl FnOnce(&Path)) -> Self {
        self.state.bindings.flush().unwrap();
        self.obs.flush().unwrap();
        let Self {
            mut servers,
            state,
            store,
            obs,
            fake,
            client,
            home,
            _root,
            _serial,
        } = self;
        servers.data_task.abort();
        servers.admin_task.abort();
        let _ = tokio::time::timeout(DEADLINE, &mut servers.data_task)
            .await
            .unwrap();
        let _ = tokio::time::timeout(DEADLINE, &mut servers.admin_task)
            .await
            .unwrap();
        drop(servers);
        // Close accepted keep-alive connections before reacquiring the lease.
        drop(client);
        drop(state);
        drop(store);
        drop(obs);
        // Edit synthetic persisted state only after every writer has stopped.
        edit(&home);
        let store = tokio::time::timeout(DEADLINE, async {
            loop {
                match AccountStore::acquire(&home) {
                    Ok(store) => break Arc::new(store),
                    Err(qb_core::accounts::StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("restarted synthetic store: {error}"),
                }
            }
        })
        .await
        .unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.anthropic_base = fake.url.clone();
        config.upstream.codex_base = fake.url.clone();
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap()
            .with_token_endpoint(Provider::Codex, fake.url.clone()),
        );
        let servers = tokio::time::timeout(
            DEADLINE,
            http::spawn_with_routes(
                &config,
                LocalSecrets {
                    client_key: "synthetic-client".into(),
                    admin_token: "synthetic-admin".into(),
                },
                relay::router(state.clone()),
                qb_core::admin::runtime_routes(state.clone(), 0),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(DEADLINE)
            .build()
            .unwrap();
        Self {
            servers,
            state,
            store,
            obs,
            fake,
            client,
            home,
            _root,
            _serial,
        }
    }

    fn ids(&self) -> Vec<String> {
        self.store
            .list()
            .unwrap()
            .into_iter()
            .map(|account| account.id)
            .collect()
    }

    async fn send(
        &self,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String, Vec<u8>) {
        let mut request = self
            .client
            .post(format!("http://{}{path}", self.servers.data_addr))
            .bearer_auth("synthetic-client")
            .header("content-type", "application/json")
            .body(body.to_owned());
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let account = response
            .headers()
            .get("x-quota-baton-account")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = response.bytes().await.unwrap().to_vec();
        (status, account, body)
    }

    async fn turn(&self, path: &str, headers: &[(&str, &str)], body: &str) -> String {
        let (status, account, _) = self.send(path, headers, body).await;
        assert_eq!(status, 200);
        assert!(!account.is_empty());
        account
    }

    async fn admin(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut request = self
            .client
            .request(method, format!("http://{}{path}", self.servers.admin_addr))
            .bearer_auth("synthetic-admin");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }

    async fn drain(&self, id: &str) {
        let result = self
            .admin(
                reqwest::Method::PATCH,
                &format!("/admin/v1/accounts/{id}"),
                Some(json!({"drain": true})),
            )
            .await;
        assert_eq!(result["drain"], true);
    }

    async fn pause(&self, id: &str) {
        self.admin(
            reqwest::Method::POST,
            &format!("/admin/v1/accounts/{id}/pause"),
            None,
        )
        .await;
    }

    async fn counts(&self, expected: &[(&str, u64)]) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let state = self
                    .admin(reqwest::Method::GET, "/admin/v1/state", None)
                    .await;
                let total: u64 = expected.iter().map(|(_, count)| count).sum();
                if state["bindings"]["count"] == total
                    && expected.iter().all(|(id, count)| {
                        state["accounts"].as_array().unwrap().iter().any(|account| {
                            account["id"] == *id && account["bound_sessions"] == *count
                        })
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("runtime admin binding counts did not converge");
    }

    fn ledger(&self) -> Vec<Value> {
        self.obs.flush().unwrap();
        fs::read_dir(self.home.join("ledger"))
            .unwrap()
            .flat_map(|entry| {
                let text = fs::read_to_string(entry.unwrap().path()).unwrap();
                for secret in [
                    "synthetic-access",
                    "synthetic-refresh",
                    "synthetic-id",
                    "synthetic-client",
                    "synthetic-admin",
                    "conversation-marker",
                    "ledger-session",
                ] {
                    assert!(!text.contains(secret), "ledger leaked {secret}");
                }
                text.lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

fn usage(provider: Provider, percent: u32) -> HeaderMap {
    let mut headers = HeaderMap::new();
    match provider {
        Provider::Claude => {
            for window in ["5h", "7d"] {
                headers.insert(
                    format!("anthropic-ratelimit-unified-{window}-utilization")
                        .parse::<axum::http::HeaderName>()
                        .unwrap(),
                    (f64::from(percent) / 100.0).to_string().parse().unwrap(),
                );
                headers.insert(
                    format!("anthropic-ratelimit-unified-{window}-reset")
                        .parse::<axum::http::HeaderName>()
                        .unwrap(),
                    (Utc::now().timestamp() + 86400)
                        .to_string()
                        .parse()
                        .unwrap(),
                );
            }
        }
        Provider::Codex => {
            for (slot, minutes) in [("primary", 300), ("secondary", 10080)] {
                for (suffix, value) in [
                    ("used-percent", percent.to_string()),
                    ("window-minutes", minutes.to_string()),
                    ("reset-at", (Utc::now().timestamp() + 86400).to_string()),
                ] {
                    headers.insert(
                        format!("x-codex-{slot}-{suffix}")
                            .parse::<axum::http::HeaderName>()
                            .unwrap(),
                        value.parse().unwrap(),
                    );
                }
            }
        }
    }
    headers
}

fn near_reset_limit() -> ScriptedResponse {
    ScriptedResponse::json(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":2,"message":"synthetic quota"}}"#,
    )
}

#[tokio::test]
async fn precommit_quota_reset_hold_retries_same_account_once_with_exact_body() {
    let h = Harness::with_hold(Provider::Codex, 3).await;
    let headers = [("session-id", "in-request-hold")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    h.fake.state.push(near_reset_limit());
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{\"held\":true}"));
    let start = Instant::now();
    let (status, account, body) = h.send("/v1/responses", &headers, RAW).await;
    let elapsed = start.elapsed();
    assert_eq!(status, 200);
    assert_eq!(account, first);
    assert_eq!(body, b"{\"held\":true}");
    assert!(elapsed >= Duration::from_millis(1500), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(3500), "{elapsed:?}");
    let attempts = h.fake.state.requests();
    assert_eq!(attempts.len(), 3);
    for attempt in &attempts {
        assert_eq!(
            attempt.headers["authorization"],
            attempts[0].headers["authorization"]
        );
        assert_eq!(attempt.body.as_ref(), RAW.as_bytes());
    }
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "binding_move" && !row["from"].is_null())
    );
}

#[tokio::test]
async fn second_precommit_429_fails_over_immediately_without_another_hold() {
    let h = Harness::with_hold(Provider::Codex, 3).await;
    let headers = [("session-id", "hold-once")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.fake.state.push(near_reset_limit());
    h.fake.state.push(near_reset_limit());
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        "{\"failover\":true}",
    ));
    let start = Instant::now();
    let (status, account, body) = h.send("/v1/responses", &headers, RAW).await;
    let elapsed = start.elapsed();
    assert_eq!(status, 200);
    assert_eq!(account, other);
    assert_eq!(body, b"{\"failover\":true}");
    assert!(elapsed >= Duration::from_millis(1500), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(3500), "{elapsed:?}");
    let attempts = h.fake.state.requests();
    assert_eq!(attempts.len(), 4);
    assert_eq!(
        attempts[1].headers["authorization"],
        attempts[0].headers["authorization"]
    );
    assert_eq!(
        attempts[2].headers["authorization"],
        attempts[0].headers["authorization"]
    );
    assert_ne!(
        attempts[3].headers["authorization"],
        attempts[0].headers["authorization"]
    );
    assert!(
        attempts
            .iter()
            .all(|attempt| attempt.body.as_ref() == RAW.as_bytes())
    );
    h.counts(&[(&first, 0), (&other, 1)]).await;
}

#[tokio::test]
async fn next_request_can_hold_again_after_failed_hold_without_a_move() {
    let h = Harness::with_hold(Provider::Codex, 3).await;
    let headers = [("session-id", "hold-per-request")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.pause(&other).await;
    for _ in 0..2 {
        h.store
            .update_metadata(&first, |account| {
                account.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: Utc::now() + ChronoDuration::seconds(30),
                    scope: "account".into(),
                };
                account.quota_cooling = Some(QuotaCooling {
                    window: "7d".into(),
                    reset_at: Utc::now() + ChronoDuration::seconds(2),
                    started_at: Utc::now(),
                });
            })
            .unwrap();
        let start = Instant::now();
        let (status, _, body) = h.send("/v1/responses", &headers, RAW).await;
        let elapsed = start.elapsed();
        assert_eq!(status, 429);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"]["type"],
            "usage_limit_reached"
        );
        assert!(elapsed >= Duration::from_millis(1500), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(3500), "{elapsed:?}");
        assert_eq!(h.fake.state.requests().len(), 1);
        h.counts(&[(&first, 1), (&other, 0)]).await;
    }
}

#[tokio::test]
async fn sticky_http_above_gate_keeps_binding_but_new_session_uses_other_account() {
    let h = Harness::new(Provider::Codex).await;
    let mut response = ScriptedResponse::json(StatusCode::OK, "{}");
    response.headers = usage(Provider::Codex, 95);
    h.fake.state.push(response);
    let first = h
        .turn("/v1/responses", &[("session-id", "warm")], RAW)
        .await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "warm")], RAW)
            .await,
        first
    );
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "fresh")], RAW)
            .await,
        other
    );
    h.counts(&[(&first, 1), (&other, 1)]).await;
}

#[tokio::test]
async fn projected_credit_limit_only_spreads_placement_and_never_cools_warm_binding() {
    let h = Harness::new(Provider::Codex).await;
    let headers = [("session-id", "projected-credit-limit")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.counts(&[(&first, 1), (&other, 0)]).await;
    assert!(!h.store.load(&first).unwrap().allow_credits);
    assert_eq!(h.state.accounts.inflight_count(&first), 0);
    assert_eq!(h.state.bindings.pending_count(&first), 0);

    let reset = Utc::now() + ChronoDuration::hours(1);
    let mut observed = usage(Provider::Codex, 89);
    for name in ["x-codex-primary-reset-at", "x-codex-secondary-reset-at"] {
        observed.insert(name, reset.timestamp().to_string().parse().unwrap());
    }
    h.state
        .accounts
        .observe_headers(&first, Provider::Codex, &observed)
        .await;
    assert_eq!(h.store.load(&first).unwrap().health, Health::Ready);
    h.state
        .accounts
        .observe_headers(&other, Provider::Codex, &usage(Provider::Codex, 95))
        .await;
    assert_eq!(
        h.turn(
            "/v1/responses",
            &[("session-id", "projection-control")],
            RAW
        )
        .await,
        first,
        "without pending work the known-passing account wins placement"
    );

    // Reservations alone supply the extra 10% that reaches the no-credits limit.
    let reservations: Vec<_> = (0..10)
        .map(|_| h.state.bindings.begin_request(None, &first))
        .collect();
    assert_eq!(h.state.bindings.pending_count(&first), 10);
    assert_eq!(h.state.accounts.inflight_count(&first), 0);
    let upstream_calls = h.fake.state.requests().len();
    assert_eq!(upstream_calls, 2);
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, first);
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "projection-new")], RAW)
            .await,
        other
    );
    assert_eq!(h.fake.state.requests().len(), upstream_calls + 2);
    assert_eq!(h.state.accounts.inflight_count(&first), 0);
    assert_eq!(h.state.bindings.pending_count(&first), 10);
    assert_eq!(h.store.load(&first).unwrap().health, Health::Ready);
    assert!(h.store.load(&first).unwrap().quota_cooling.is_none());
    drop(reservations);
    assert_eq!(h.state.bindings.pending_count(&first), 0);
    assert_eq!(h.store.load(&first).unwrap().health, Health::Ready);
    assert!(h.store.load(&first).unwrap().quota_cooling.is_none());
    h.counts(&[(&first, 2), (&other, 1)]).await;
}

#[tokio::test]
async fn drain_blocks_new_placements_and_preserves_existing_http_binding() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "existing")], RAW)
        .await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.drain(&first).await;
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "existing")], RAW)
            .await,
        first
    );
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "new")], RAW).await,
        other
    );
    h.counts(&[(&first, 1), (&other, 1)]).await;
}

#[tokio::test]
async fn pause_forces_next_http_turn_to_move_and_admin_counts_follow() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "paused")], RAW)
        .await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.counts(&[(&first, 1), (&other, 0)]).await;
    h.pause(&first).await;
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "paused")], RAW)
            .await,
        other
    );
    h.counts(&[(&first, 0), (&other, 1)]).await;
}

#[tokio::test]
async fn claude_parent_inheritance_and_forced_move_keep_entire_group_together() {
    let h = Harness::new(Provider::Claude).await;
    let root = [("x-claude-code-session-id", "group")];
    let child = [
        ("x-claude-code-session-id", "group"),
        ("x-claude-code-agent-id", "child"),
        ("x-claude-code-parent-agent-id", "main"),
    ];
    let grandchild = [
        ("x-claude-code-session-id", "group"),
        ("x-claude-code-agent-id", "grandchild"),
        ("x-claude-code-parent-agent-id", "child"),
    ];
    let first = h.turn("/v1/messages", &root, "{}").await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.drain(&first).await;
    assert_eq!(h.turn("/v1/messages", &child, "{}").await, first);
    assert_eq!(h.turn("/v1/messages", &grandchild, "{}").await, first);
    h.counts(&[(&first, 3), (&other, 0)]).await;
    h.pause(&first).await;
    assert_eq!(h.turn("/v1/messages", &grandchild, "{}").await, other);
    h.counts(&[(&first, 0), (&other, 3)]).await;
    assert_eq!(h.turn("/v1/messages", &root, "{}").await, other);
    assert_eq!(h.turn("/v1/messages", &child, "{}").await, other);
    let rows = h.ledger();
    let moves: Vec<_> = rows
        .iter()
        .filter(|row| row["event"] == "binding_move" && row["from"] == first)
        .collect();
    assert_eq!(moves.len(), 1, "one event per moved identity group");
    assert_eq!(moves[0]["keys"], 3);
    assert!(
        moves
            .iter()
            .all(|row| row["to"] == other && row["reason"] == "forced")
    );
    assert_eq!(
        moves
            .iter()
            .map(|row| row["bytes_in"].as_u64().unwrap_or(0))
            .sum::<u64>(),
        2,
        "the triggering {{}} request is charged once for the entire three-key group"
    );
}

#[tokio::test]
async fn codex_alpha_search_turn_metadata_shares_response_binding() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("Session-Id", "search-session")], RAW)
        .await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.drain(&first).await;
    let metadata = r#"{"session_id":"search-session","turn_id":"synthetic-turn"}"#;
    assert_eq!(
        h.turn(
            "/v1/alpha/search",
            &[("x-codex-turn-metadata", metadata)],
            RAW
        )
        .await,
        first
    );
    assert_eq!(
        h.turn("/v1/responses", &[("session_id", "search-session")], RAW)
            .await,
        first
    );
    h.counts(&[(&first, 1), (&other, 0)]).await;
    assert_eq!(h.fake.state.requests()[1].path_and_query, "/alpha/search");
    assert!(
        h.fake
            .state
            .requests()
            .iter()
            .all(|request| request.body.as_ref() == RAW.as_bytes())
    );
}

#[tokio::test]
async fn claude_tail_metadata_binds_large_body_without_rewriting_it() {
    let h = Harness::new(Provider::Claude).await;
    let body = json!({"model": "fixture", "messages": [{"role": "user", "content": "x".repeat(70 * 1024)}],
        "metadata": {"user_id": json!({"session_id": "tail-session"}).to_string()}, "max_tokens": 1}).to_string();
    let first = h.turn("/v1/messages", &[], &body).await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.drain(&first).await;
    assert_eq!(
        h.turn(
            "/v1/messages",
            &[("x-claude-code-session-id", "tail-session")],
            "{}"
        )
        .await,
        first
    );
    h.counts(&[(&first, 1), (&other, 0)]).await;
    assert_eq!(h.fake.state.requests()[0].body.as_ref(), body.as_bytes());
}

#[tokio::test]
async fn unbound_http_stays_on_last_account_until_pause() {
    let h = Harness::new(Provider::Codex).await;
    let first = h.turn("/v1/responses", &[], RAW).await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.drain(&first).await;
    assert_eq!(h.turn("/v1/responses", &[], RAW).await, first);
    h.counts(&[(&first, 0), (&other, 0)]).await;
    h.pause(&first).await;
    assert_eq!(h.turn("/v1/responses", &[], RAW).await, other);
    assert_eq!(h.turn("/v1/responses", &[], RAW).await, other);
}

#[tokio::test]
async fn quota_reset_hold_waits_two_seconds_and_keeps_http_binding() {
    let h = Harness::new(Provider::Codex).await;
    let headers = [("session-id", "reset-hold"), ("x-stainless-timeout", "5")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    h.store
        .update_metadata(&first, |account| {
            account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: Utc::now() + ChronoDuration::seconds(2),
                scope: "account".into(),
            }
        })
        .unwrap();
    let start = Instant::now();
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, first);
    assert!(start.elapsed() >= Duration::from_millis(1500));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(h.fake.state.requests().len(), 2);
}

#[tokio::test]
async fn quota_reset_hold_ignores_client_timeout_and_moves_immediately_after_bounded_hold() {
    let h = Harness::with_hold(Provider::Codex, 3).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "hold-timeout")], RAW)
        .await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.store
        .update_metadata(&first, |account| {
            account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: Utc::now() + ChronoDuration::seconds(30),
                scope: "account".into(),
            };
            account.quota_cooling = Some(QuotaCooling {
                window: "7d".into(),
                reset_at: Utc::now() + ChronoDuration::seconds(2),
                started_at: Utc::now(),
            });
        })
        .unwrap();
    let start = Instant::now();
    assert_eq!(
        h.turn(
            "/v1/responses",
            &[
                ("session-id", "hold-timeout"),
                ("x-stainless-timeout", "0.1")
            ],
            RAW
        )
        .await,
        other
    );
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(1500), "{elapsed:?}");
    // Two-second reset plus local HTTP overhead; no second hold or 30s cooling wait.
    assert!(elapsed < Duration::from_millis(3500), "{elapsed:?}");
    assert!(matches!(
        h.store.load(&first).unwrap().health,
        Health::Cooling { .. }
    ));
    assert_eq!(h.fake.state.requests().len(), 2);
    h.counts(&[(&first, 0), (&other, 1)]).await;
}

#[tokio::test]
async fn rn1_successful_reset_hold_then_payment_failover_is_forced() {
    let h = Harness::with_hold(Provider::Codex, 3).await;
    let headers = [("session-id", "hold-then-payment")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    h.store
        .update_metadata(&first, |a| {
            a.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: Utc::now() + ChronoDuration::seconds(2),
                scope: "account".into(),
            };
        })
        .unwrap();
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::PAYMENT_REQUIRED,
        r#"{"error":{"type":"payment_required"}}"#,
    ));
    let next = h.turn("/v1/responses", &headers, RAW).await;
    assert_ne!(next, first);
    h.obs.flush().unwrap();
    let records: Vec<Value> = fs::read_dir(h.home.join("ledger"))
        .unwrap()
        .flat_map(|entry| {
            fs::read_to_string(entry.unwrap().path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    let moved = records
        .iter()
        .find(|row| row["event"] == "binding_move" && row["from"] == first)
        .unwrap();
    assert_eq!(moved["reason"], "forced");
}

#[tokio::test]
async fn forced_move_keeps_body_verbatim_and_request_ledger_session_hashed() {
    let h = Harness::new(Provider::Codex).await;
    let headers = [("session-id", "ledger-session")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    h.pause(&first).await;
    let usage_body = r#"{"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":80,"opaque":"conversation-marker"}},"output":"conversation-marker"}"#;
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, usage_body));
    let (status, second, body) = h.send("/v1/responses", &headers, RAW).await;
    assert_eq!(status, 200);
    assert_eq!(body, usage_body.as_bytes());
    assert_ne!(first, second);
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, second);
    assert!(
        h.fake
            .state
            .requests()
            .iter()
            .all(|request| request.body.as_ref() == RAW.as_bytes())
    );
    let hash: String = Sha256::digest(b"codex:ledger-session")[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let rows = h.ledger();
    let requests: Vec<_> = rows
        .iter()
        .filter(|row| row["transport"] == "http")
        .collect();
    assert_eq!(requests.len(), 3);
    assert_ne!(
        requests[0]["after_move"], true,
        "initial placement is not a move"
    );
    assert_eq!(
        requests[1]["after_move"], true,
        "the first turn on the moved binding is marked"
    );
    assert_ne!(
        requests[2]["after_move"], true,
        "the marker is consumed by one turn"
    );
    for row in &requests {
        assert_eq!(row["session"], hash);
        assert_eq!(row["bytes_in"], RAW.len() as u64);
    }
    let cached: Vec<_> = requests
        .iter()
        .filter(|row| row.get("cache_usage").is_some_and(|usage| !usage.is_null()))
        .collect();
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0]["account"], second);
    assert_eq!(
        cached[0]["cache_usage"],
        json!({
            "input_tokens": 100, "cached_tokens": 80, "cache_creation_input_tokens": null,
        })
    );
    let moves: Vec<_> = rows
        .iter()
        .filter(|row| row["event"] == "binding_move")
        .collect();
    assert_eq!(
        moves.len(),
        2,
        "one placement and one forced move; sticky turn emits none"
    );
    assert_eq!(moves[0]["session"], hash);
    assert_eq!(moves[0]["from"], Value::Null);
    assert_eq!(moves[0]["to"], first);
    assert_eq!(moves[0]["reason"], "new");
    assert_eq!(moves[1]["session"], hash);
    assert_eq!(moves[1]["from"], first);
    assert_eq!(moves[1]["to"], second);
    assert_eq!(moves[1]["reason"], "forced");
    for row in moves {
        assert_eq!(row["bytes_in"], RAW.len() as u64);
    }
}

#[tokio::test]
async fn claude_forced_move_records_only_numeric_cache_usage_and_preserves_response() {
    let h = Harness::new(Provider::Claude).await;
    let headers = [("x-claude-code-session-id", "ledger-session")];
    let first = h.turn("/v1/messages", &headers, RAW).await;
    h.pause(&first).await;
    let usage_body = r#"{"usage":{"input_tokens":100,"cache_read_input_tokens":80,"cache_creation_input_tokens":20,"opaque":"conversation-marker"},"content":[{"type":"text","text":"conversation-marker"}]}"#;
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, usage_body));
    let (status, second, body) = h.send("/v1/messages", &headers, RAW).await;
    assert_eq!(status, 200);
    assert_ne!(first, second);
    assert_eq!(body, usage_body.as_bytes());
    let rows = h.ledger();
    let placements: Vec<_> = rows
        .iter()
        .filter(|row| row["event"] == "binding_move" && row["reason"] == "new")
        .collect();
    assert_eq!(
        placements.len(),
        1,
        "Claude emits the same new-binding event as Codex"
    );
    assert_eq!(placements[0]["from"], Value::Null);
    assert_eq!(placements[0]["to"], first);
    assert_eq!(placements[0]["bytes_in"], RAW.len() as u64);
    let hash: String = Sha256::digest(b"claude:ledger-session")[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(placements[0]["session"], hash);
    let cached: Vec<_> = rows
        .iter()
        .filter(|row| row.get("cache_usage").is_some_and(|usage| !usage.is_null()))
        .collect();
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0]["account"], second);
    assert_eq!(
        cached[0]["cache_usage"],
        json!({
            "input_tokens": 100, "cached_tokens": 80, "cache_creation_input_tokens": 20,
        })
    );
    assert_eq!(h.fake.state.requests().len(), 2);
    assert!(
        h.fake
            .state
            .requests()
            .iter()
            .all(|request| request.body.as_ref() == RAW.as_bytes())
    );
}

#[tokio::test]
async fn explicit_bindings_flush_and_restart_preserve_drained_http_session() {
    let h = Harness::new(Provider::Codex).await;
    let headers = [("session-id", "persisted-session")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    let other = h.ids().into_iter().find(|id| id != &first).unwrap();
    h.drain(&first).await;
    h.counts(&[(&first, 1), (&other, 0)]).await;
    let h = h.restart().await;
    let disk: Value =
        serde_json::from_slice(&fs::read(h.home.join("state/bindings.json")).unwrap()).unwrap();
    assert_eq!(disk["schema"], 1);
    let bindings = disk["bindings"].as_array().unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0]["key"], "codex:persisted-session");
    assert_eq!(bindings[0]["account"], first);
    chrono::DateTime::parse_from_rfc3339(bindings[0]["last_used"].as_str().unwrap()).unwrap();
    // Check startup loading before any request can create a replacement binding.
    h.counts(&[(&first, 1), (&other, 0)]).await;
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, first);
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "after-restart")], RAW)
            .await,
        other
    );
    h.counts(&[(&first, 1), (&other, 1)]).await;
}

// Simulate an hour-idle restart with synthetic persisted metadata. No wall-clock
// hour, live account, or mutable production binding is needed for the cold point.
fn cold_fixture(home: &Path, expired: bool) {
    let path = home.join("state/bindings.json");
    let mut bindings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for binding in bindings["bindings"].as_array_mut().unwrap() {
        binding["last_used"] = json!((Utc::now() - ChronoDuration::hours(2)).to_rfc3339());
    }
    fs::write(path, serde_json::to_vec(&bindings).unwrap()).unwrap();
    let path = home.join("state/quota.json");
    let mut quota: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for account in quota["accounts"].as_object_mut().unwrap().values_mut() {
        for window in account["windows"].as_array_mut().unwrap() {
            window["observed_at"] = json!(
                (Utc::now()
                    - if expired {
                        ChronoDuration::minutes(2)
                    } else {
                        ChronoDuration::hours(2)
                    })
                .to_rfc3339()
            );
            window["used_pct"] = json!(if expired { 95.0 } else { 10.0 });
            window["reset_at"] = json!(
                (Utc::now()
                    + if expired {
                        ChronoDuration::minutes(-1)
                    } else {
                        ChronoDuration::hours(24)
                    })
                .to_rfc3339()
            );
        }
    }
    fs::write(path, serde_json::to_vec(&quota).unwrap()).unwrap();
}

#[tokio::test]
async fn cold_restored_binding_with_old_observations_never_moves_or_ping_pongs() {
    let h = Harness::new(Provider::Codex).await;
    let mut ids = h.ids();
    ids.sort();
    // Bind to the higher id so the old all-stale fallback cannot accidentally
    // select the already-bound account and hide the regression.
    h.state
        .accounts
        .observe_headers(&ids[0], Provider::Codex, &usage(Provider::Codex, 95))
        .await;
    let headers = [("session-id", "cold-old-observation")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    assert_eq!(first, ids[1]);
    let h = h
        .restart_with_fixture(|home| cold_fixture(home, false))
        .await;
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, first);
    let h = h
        .restart_with_fixture(|home| cold_fixture(home, false))
        .await;
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, first);
    h.counts(&[(&first, 1), (&ids[0], 0)]).await;
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "binding_move" && !row["from"].is_null())
    );
}

#[tokio::test]
async fn cold_binding_with_expired_windows_treats_old_usage_as_zero_and_stays() {
    let h = Harness::new(Provider::Codex).await;
    let mut ids = h.ids();
    ids.sort();
    h.state
        .accounts
        .observe_headers(&ids[0], Provider::Codex, &usage(Provider::Codex, 95))
        .await;
    let headers = [("session-id", "cold-expired-window")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    assert_eq!(first, ids[1]);
    let h = h
        .restart_with_fixture(|home| cold_fixture(home, true))
        .await;
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, first);
    assert_eq!(h.store.load(&first).unwrap().health, Health::Ready);
    h.counts(&[(&first, 1), (&ids[0], 0)]).await;
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "binding_move" && !row["from"].is_null())
    );
}
