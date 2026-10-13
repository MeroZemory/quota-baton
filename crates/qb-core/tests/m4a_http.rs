use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, Utc};
use qb_core::{
    accounts::{AccountStore, Health, HealthReason, Provider, UserState, import},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

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
async fn c6_row_3_only_long_rate_limit_fails_over() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    for (seconds, expected) in [(10, 429), (11, 200)] {
        let mut r = ScriptedResponse::json(
            StatusCode::TOO_MANY_REQUESTS,
            "{\"error\":{\"type\":\"rate_limit_error\"}}",
        );
        r.headers
            .insert("retry-after", seconds.to_string().parse().unwrap());
        h.fake.state.push(r);
        if expected == 200 {
            h.fake.state.push(ok());
        }
        assert_eq!(h.send(Provider::Claude, "{}").await.status(), expected);
    }
    assert!(matches!(
        h.store.load(&h.ids()[0]).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Rate,
            ..
        }
    ));
}
#[tokio::test]
async fn c6_row_4_revocation_is_terminal_and_fails_over() {
    let h = Harness::new(Provider::Codex, 2, 384, 1024).await;
    h.fake.state.push(ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"token_revoked","message":"conversation-marker"}}"#,
    ));
    h.fake.state.push(ok());
    assert_eq!(h.send(Provider::Codex, "{}").await.status(), 200);
    assert!(matches!(
        h.store.load(&h.ids()[0]).unwrap().health,
        Health::NeedsRelogin { .. }
    ));
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
async fn c6_row_3_http_date_retry_after_fails_over() {
    let h = Harness::new(Provider::Claude, 2, 384, 1024).await;
    let mut r = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, "{}");
    r.headers.insert(
        "retry-after",
        (Utc::now() + ChronoDuration::seconds(60))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string()
            .parse()
            .unwrap(),
    );
    h.fake.state.push(r);
    h.fake.state.push(ok());
    assert_eq!(h.send(Provider::Claude, "{}").await.status(), 200);
    assert_eq!(h.fake.state.requests().len(), 2);
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
