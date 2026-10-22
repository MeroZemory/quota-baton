//! Real loopback R14 regressions.
//! All homes, credentials, quota observations and upstream replies are synthetic.
#![cfg(feature = "testkit")]

use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use qb_core::{
    accounts::{AccountStore, Health, HealthReason, Provider, import},
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
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(8);
const MODEL: &str = "claude-sonnet-4-5";
const BODY: &str = r#"{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":"r14-body-marker"}]}"#;
const SSE_EVENT_LIMIT: usize = 64 * 1024;
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    // Drop listeners and runtime owners before the synthetic directory.
    servers: RunningServers,
    state: Arc<RelayState>,
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    fake: ScriptedUpstream,
    client: reqwest::Client,
    home: PathBuf,
    started_at: DateTime<Utc>,
    _root: tempfile::TempDir,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

struct Reply {
    status: u16,
    account: String,
    headers: HeaderMap,
    body: Value,
}

impl Harness {
    async fn new(request_timeout_secs: u64) -> Self {
        Self::new_provider(request_timeout_secs, Provider::Claude).await
    }

    async fn new_provider(request_timeout_secs: u64, provider: Provider) -> Self {
        let serial = tokio::time::timeout(Duration::from_secs(30), SERIAL.lock())
            .await
            .unwrap();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r14-http-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let cpa = root.path().join("synthetic-cpa");
        fs::create_dir_all(&cpa).unwrap();
        let mut activate = Vec::new();
        for i in 0..2 {
            let email = format!("r14-{i}@example.invalid");
            let account = json!({
                "type":provider.as_str(), "email":email, "account_id":format!("synthetic-r14-{i}"),
                "account_uuid":format!("uuid-r14-{i}"), "organization_uuid":"org-r14",
                "access_token":format!("synthetic-r14-access-{i}"),
                "refresh_token":format!("synthetic-r14-refresh-{i}"), "id_token":"synthetic-r14-id",
                "expired":"2099-01-01T00:00:00Z", "last_refresh":"2026-01-01T00:00:00Z"
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
                    Err(error) => panic!("synthetic R14 store: {error}"),
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
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.routing.request_timeout_secs = request_timeout_secs;
        config.upstream.anthropic_base = fake.url.clone();
        config.upstream.codex_base = fake.url.clone();
        let started_at = Utc::now();
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
        // Selection confidence only; re-read baselines are seeded exclusively by HTTP turns.
        let mut quota = HeaderMap::new();
        for window in ["5h", "7d"] {
            quota.insert(
                format!("anthropic-ratelimit-unified-{window}-utilization")
                    .parse::<axum::http::HeaderName>()
                    .unwrap(),
                "0.1".parse().unwrap(),
            );
            quota.insert(
                format!("anthropic-ratelimit-unified-{window}-reset")
                    .parse::<axum::http::HeaderName>()
                    .unwrap(),
                (Utc::now() + ChronoDuration::days(1))
                    .timestamp()
                    .to_string()
                    .parse()
                    .unwrap(),
            );
        }
        if provider == Provider::Codex {
            quota.clear();
            for (window, minutes) in [("primary", "300"), ("secondary", "10080")] {
                for (suffix, value) in [
                    ("window-minutes", minutes.to_owned()),
                    ("used-percent", "10".to_owned()),
                    (
                        "reset-at",
                        (Utc::now() + ChronoDuration::days(1))
                            .timestamp()
                            .to_string(),
                    ),
                ] {
                    quota.insert(
                        format!("x-codex-{window}-{suffix}")
                            .parse::<axum::http::HeaderName>()
                            .unwrap(),
                        value.parse().unwrap(),
                    );
                }
            }
        }
        for account in store.list().unwrap() {
            state
                .accounts
                .observe_headers(&account.id, provider, &quota)
                .await;
        }
        let servers = tokio::time::timeout(
            DEADLINE,
            http::spawn_with_routes(
                &config,
                LocalSecrets {
                    client_key: "synthetic-r14-client".into(),
                    admin_token: "synthetic-r14-admin".into(),
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
            started_at,
            _root: root,
            _serial: serial,
        }
    }

    async fn turn(&self, session: &str) -> Reply {
        let (status, account, headers, bytes) =
            self.raw_turn(Provider::Claude, session, BODY).await;
        Reply {
            status,
            account,
            headers,
            body: serde_json::from_slice(&bytes).unwrap(),
        }
    }

    async fn raw_turn(
        &self,
        provider: Provider,
        session: &str,
        body: &str,
    ) -> (u16, String, HeaderMap, bytes::Bytes) {
        let (path, session_header) = match provider {
            Provider::Claude => ("/v1/messages", "x-claude-code-session-id"),
            Provider::Codex => ("/v1/responses", "session-id"),
        };
        let response = self
            .client
            .post(format!("http://{}{path}", self.servers.data_addr))
            .bearer_auth("synthetic-r14-client")
            .header("content-type", "application/json")
            .header(session_header, session)
            .body(body.to_owned())
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let account = headers
            .get("x-quota-baton-account")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = response.bytes().await.unwrap(); // Consume EOF before checking accounting.
        (status, account, headers, bytes)
    }

    async fn admin(&self, method: reqwest::Method, path: &str) -> Value {
        let response = self
            .client
            .request(method, format!("http://{}{path}", self.servers.admin_addr))
            .bearer_auth("synthetic-r14-admin")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        response.json().await.unwrap()
    }

    async fn sessions(&self, known: u64, unknown: u64) -> Value {
        // Completion drop can race the client's final body read; wait for the real DTO.
        tokio::time::timeout(DEADLINE, async {
            loop {
                let view = self
                    .admin(reqwest::Method::GET, "/admin/v1/sessions?range=all")
                    .await;
                if view["move_cost"]["hour"]["known_count"] == known
                    && view["move_cost"]["hour"]["unknown_count"] == unknown
                {
                    break view;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real sessions endpoint did not publish move-cost coverage")
    }

    fn ledger(&self) -> Vec<Value> {
        self.obs.flush().unwrap();
        fs::read_dir(self.home.join("ledger"))
            .unwrap()
            .flat_map(|entry| {
                let text = fs::read_to_string(entry.unwrap().path()).unwrap();
                for private in [
                    "synthetic-r14-access",
                    "synthetic-r14-refresh",
                    "synthetic-r14-client",
                    "synthetic-r14-admin",
                    "r14-body-marker",
                    "r14-known-session",
                    "r14-unknown-session",
                    "r14-pingpong-session",
                ] {
                    assert!(
                        !text.contains(private),
                        "R14 ledger leaked a private fixture marker"
                    );
                }
                text.lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn assert_upstream_accounts(&self, expected: &[&str]) {
        let requests = self.fake.state.requests();
        assert_eq!(
            requests.len(),
            expected.len(),
            "unexpected upstream dispatch (possible guard bypass)"
        );
        for (request, account) in requests.iter().zip(expected) {
            let metadata = self.store.load(account).unwrap();
            let token = self.store.tokens(&metadata).unwrap();
            let authorization = format!("Bearer {}", token.access_token);
            assert!(
                request
                    .headers
                    .get("authorization")
                    .is_some_and(|v| v.as_bytes() == authorization.as_bytes()),
                "upstream request used an unexpected synthetic account"
            );
            assert_eq!(request.path_and_query, "/v1/messages");
            assert_eq!(request.body.as_ref(), BODY.as_bytes());
        }
    }
}

fn usage_reply(input: u64, creation: Option<u64>) -> ScriptedResponse {
    let mut usage = json!({"input_tokens":input, "cache_read_input_tokens":500, "output_tokens":1});
    if let Some(creation) = creation {
        usage["cache_creation_input_tokens"] = creation.into();
    }
    ScriptedResponse::json(
        StatusCode::OK,
        serde_json::to_vec(&json!({
            "id":"msg_r14", "type":"message", "role":"assistant", "model":MODEL,
            "content":[], "stop_reason":"end_turn", "usage":usage
        }))
        .unwrap(),
    )
}

fn payment_reply() -> ScriptedResponse {
    ScriptedResponse::json(
        StatusCode::PAYMENT_REQUIRED,
        r#"{"type":"error","error":{"type":"billing_error","message":"synthetic payment signal"}}"#,
    )
}

fn session_hash(session: &str) -> String {
    Sha256::digest(format!("claude:{session}").as_bytes())[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn codex_usage_reply(input: u64, cached: u64) -> ScriptedResponse {
    ScriptedResponse::json(StatusCode::OK, serde_json::to_vec(&json!({
        "id":"resp_r14", "object":"response", "status":"completed", "model":"r14-synthetic-model",
        "output":[], "usage":{"input_tokens":input, "input_tokens_details":{"cached_tokens":cached}}
    })).unwrap())
}

#[tokio::test]
async fn ordinary_codex_http_model_hint_measures_move_without_routing_model() {
    let h = Harness::new_provider(5, Provider::Codex).await;
    let body = r#"{"model":"r14-synthetic-model","input":[]}"#;
    let session = "r14-ordinary-codex";
    let mut a = String::new();
    for _ in 0..3 {
        h.fake.state.push(codex_usage_reply(100, 80));
        let (status, account, _, _) = h.raw_turn(Provider::Codex, session, body).await;
        assert_eq!(status, 200);
        if a.is_empty() {
            a = account;
        } else {
            assert_eq!(account, a);
        }
    }
    h.fake.state.push(payment_reply());
    h.fake.state.push(codex_usage_reply(200, 0));
    let (status, b, _, _) = h.raw_turn(Provider::Codex, session, body).await;
    assert_eq!(status, 200);
    assert_ne!(a, b);
    let measured = h.sessions(1, 0).await;
    assert_eq!(measured["move_cost"]["hour"]["tokens"], 180);
    assert_eq!(
        h.state.bindings.current(Some(&format!("codex:{session}"))),
        Some(b.clone())
    );
    h.fake.state.push(codex_usage_reply(300, 0));
    let (status, account, _, _) = h.raw_turn(Provider::Codex, session, body).await;
    assert_eq!(status, 200);
    assert_eq!(account, b);
    let rows = h.ledger();
    let turns: Vec<_> = rows
        .iter()
        .filter(|row| row["route"] == "POST /v1/responses")
        .collect();
    assert_eq!(turns.len(), 5);
    assert!(
        turns.iter().all(|row| row["model"].is_null()),
        "accounting hint changed legacy request model metadata"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"] == "binding_move_cost"
                && row["measurement"] == "known"
                && row["reread_tokens"] == 180)
            .count(),
        1
    );
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 6);
    for (request, account) in requests.iter().zip([&a, &a, &a, &a, &b, &b]) {
        assert_eq!(request.path_and_query, "/responses");
        assert_eq!(request.body.as_ref(), body.as_bytes());
        let metadata = h.store.load(account).unwrap();
        let token = h.store.tokens(&metadata).unwrap();
        assert_eq!(
            request.headers["authorization"],
            format!("Bearer {}", token.access_token)
        );
    }
    assert_eq!(h.sessions(1, 0).await["move_cost"]["hour"]["tokens"], 180);
}

fn sse_usage_reply(input: u64, creation: u64, ending: &str) -> ScriptedResponse {
    let start = format!(
        "event: message_start\ndata: {}\n\n",
        json!({
            "type":"message_start", "message":{"id":"msg_r14", "model":MODEL,
            "usage":{"input_tokens":input, "cache_read_input_tokens":500, "cache_creation_input_tokens":creation}}
        })
    );
    let error = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"synthetic\"}}\n\n";
    let stop = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let tail = match ending {
        "error" => error.to_owned(),
        "incomplete" => String::new(),
        "error-stop" => format!("{error}{stop}"),
        "oversized-error-stop" => {
            let error = format!(
                "data: {}\n\n",
                json!({"type":"error","error":{
                    "type":"api_error","message":"x".repeat(SSE_EVENT_LIMIT)
                }})
            );
            format!("{error}{stop}")
        }
        "limit-valid-stop" | "oversized-valid-stop" => {
            let prefix = "data: {\"type\":\"message_delta\",\"padding\":\"";
            let suffix = "\"}\n\n";
            let len = SSE_EVENT_LIMIT + usize::from(ending == "oversized-valid-stop");
            let event = format!(
                "{prefix}{}{suffix}",
                "x".repeat(len - prefix.len() - suffix.len())
            );
            assert_eq!(event.len(), len);
            format!("{event}{stop}")
        }
        "malformed-stop" => format!("data: {{\"type\":\"error\"\n\n{stop}"),
        "stop" => stop.to_owned(),
        _ => unreachable!(),
    };
    ScriptedResponse::sse(vec![
        (Duration::ZERO, start.into()),
        (Duration::from_millis(20), tail.into()),
    ])
}

async fn assert_http_sse_move_accounting(ending: &str, known: bool) {
    let h = Harness::new(5).await;
    let session = "r14-sse-session";
    let mut a = String::new();
    for _ in 0..3 {
        h.fake.state.push(sse_usage_reply(10, 0, "stop"));
        let (status, account, _, bytes) = h.raw_turn(Provider::Claude, session, BODY).await;
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&bytes).contains("message_stop"));
        if a.is_empty() {
            a = account;
        } else {
            assert_eq!(account, a);
        }
    }
    h.fake.state.push(payment_reply());
    let reply = sse_usage_reply(1000, 100, ending);
    let expected: Vec<_> = reply
        .chunks
        .iter()
        .flat_map(|(_, bytes)| bytes.iter().copied())
        .collect();
    h.fake.state.push(reply);
    let (status, b, _, bytes) = h.raw_turn(Provider::Claude, session, BODY).await;
    assert_eq!(status, 200);
    assert_ne!(a, b);
    assert_eq!(bytes.as_ref(), expected.as_slice());
    let (known_count, unknown_count, tokens) = if known { (1, 0, 1090) } else { (0, 1, 0) };
    let view = h.sessions(known_count, unknown_count).await;
    assert_eq!(view["move_cost"]["hour"]["tokens"], tokens);
    assert_eq!(
        view["move_cost"]["rolling24h"]["unknown_count"],
        unknown_count
    );
    // A later complete normal JSON turn must not acquire an already consumed claim.
    h.fake.state.push(usage_reply(2000, Some(0)));
    let later = h.turn(session).await;
    assert_eq!(later.status, 200);
    assert_eq!(later.account, b);
    assert_eq!(
        h.sessions(known_count, unknown_count).await["move_cost"]["hour"]["tokens"],
        tokens
    );
    h.assert_upstream_accounts(&[&a, &a, &a, &a, &b, &b]);
    let rows = h.ledger();
    let turns: Vec<_> = rows
        .iter()
        .filter(|row| row["route"] == "POST /v1/messages")
        .collect();
    assert_eq!(turns.len(), 5);
    assert!(
        turns
            .iter()
            .all(|row| row["status"] == 200 && row["outcome"] == "ok")
    );
    let measurements: Vec<_> = rows
        .iter()
        .filter(|row| row["event"] == "binding_move_cost" && row["measurement"] != "pending")
        .collect();
    assert_eq!(measurements.len(), 1);
    assert_eq!(
        measurements[0]["measurement"],
        if known { "known" } else { "unknown" }
    );
    if known {
        assert_eq!(measurements[0]["reread_tokens"], tokens);
    } else {
        assert!(measurements[0]["reread_tokens"].is_null());
    }
}

#[tokio::test]
async fn real_http_sse_error_eof_consumes_move_as_unknown() {
    assert_http_sse_move_accounting("error", false).await;
}

#[tokio::test]
async fn real_http_sse_incomplete_eof_consumes_move_as_unknown() {
    assert_http_sse_move_accounting("incomplete", false).await;
}

#[tokio::test]
async fn real_http_sse_error_then_stop_still_consumes_move_as_unknown() {
    assert_http_sse_move_accounting("error-stop", false).await;
}

#[tokio::test]
async fn real_http_sse_event_at_limit_still_measures_move() {
    assert_http_sse_move_accounting("limit-valid-stop", true).await;
}

#[tokio::test]
async fn real_http_sse_oversized_data_error_then_stop_consumes_move_as_unknown() {
    assert_http_sse_move_accounting("oversized-error-stop", false).await;
}

#[tokio::test]
async fn real_http_sse_valid_oversized_event_keeps_move_unknown() {
    assert_http_sse_move_accounting("oversized-valid-stop", false).await;
}

#[tokio::test]
async fn real_http_sse_malformed_data_then_stop_consumes_move_as_unknown() {
    assert_http_sse_move_accounting("malformed-stop", false).await;
}

#[tokio::test]
async fn real_http_sse_relayed_successful_stop_measures_move() {
    assert_http_sse_move_accounting("stop", true).await;
}

#[tokio::test]
async fn real_http_move_cost_reaches_admin_and_ledger_with_known_unknown_coverage() {
    let h = Harness::new(5).await;
    let mut a = String::new();
    for _ in 0..3 {
        h.fake.state.push(usage_reply(10, Some(0)));
        let turn = h.turn("r14-known-session").await;
        assert_eq!(turn.status, 200);
        assert_eq!(turn.body["usage"]["input_tokens"], 10);
        if a.is_empty() {
            a = turn.account;
        } else {
            assert_eq!(turn.account, a);
        }
    }
    let b = h
        .store
        .list()
        .unwrap()
        .into_iter()
        .find(|account| account.id != a)
        .unwrap()
        .id;
    // Keep a second real session on A, but give it incomplete usage: no baseline exists.
    h.admin(
        reqwest::Method::POST,
        &format!("/admin/v1/accounts/{b}/pause"),
    )
    .await;
    h.fake.state.push(usage_reply(10, None));
    let unmeasured = h.turn("r14-unknown-session").await;
    assert_eq!(unmeasured.status, 200);
    assert_eq!(unmeasured.account, a);
    h.admin(
        reqwest::Method::POST,
        &format!("/admin/v1/accounts/{b}/resume"),
    )
    .await;

    // Actual A upstream signal; production health/selection/commit dispatches the retry to B.
    h.fake.state.push(payment_reply());
    h.fake.state.push(usage_reply(1000, Some(100)));
    let moved = h.turn("r14-known-session").await;
    assert_eq!(moved.status, 200);
    assert_eq!(moved.account, b);
    assert!(matches!(
        h.store.load(&a).unwrap().health,
        Health::Cooling {
            reason: HealthReason::Payment,
            ..
        }
    ));
    let measured = h.sessions(1, 0).await;
    assert_eq!(measured["move_cost"]["hour"]["tokens"], 1090); // (1000 + 100) - median(10).

    // Its A binding is now unusable: this session's first move stays unknown, even with usage.
    h.fake.state.push(usage_reply(1000, Some(100)));
    let unknown = h.turn("r14-unknown-session").await;
    assert_eq!(unknown.status, 200);
    assert_eq!(unknown.account, b);
    let view = h.sessions(1, 1).await;
    assert_eq!(view["sessions"].as_array().unwrap().len(), 2);
    let observed_until = Utc::now();
    for window in ["hour", "rolling24h"] {
        let cost = &view["move_cost"][window];
        assert_eq!(cost["tokens"], 1090);
        assert_eq!(cost["known_count"], 1);
        assert_eq!(cost["unknown_count"], 1);
        assert_eq!(cost["recording_complete"], true);
        assert_eq!(cost["tokens_saturated"], false);
        let since = DateTime::parse_from_rfc3339(cost["observed_since"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert!(since >= h.started_at && since <= observed_until);
        assert!(
            observed_until - since < ChronoDuration::minutes(1),
            "must expose partial-day observation coverage"
        );
    }
    assert_eq!(
        h.state.bindings.current(Some("claude:r14-known-session")),
        Some(b.clone())
    );
    assert_eq!(
        h.state.bindings.current(Some("claude:r14-unknown-session")),
        Some(b.clone())
    );
    h.assert_upstream_accounts(&[&a, &a, &a, &a, &a, &b, &b]);

    let rows = h.ledger();
    let costs: Vec<_> = rows
        .iter()
        .filter(|row| row["event"] == "binding_move_cost")
        .collect();
    let known = costs
        .iter()
        .find(|row| row["measurement"] == "known")
        .expect("known move cost ledger row");
    let unknown = costs
        .iter()
        .find(|row| row["measurement"] == "unknown")
        .expect("unknown move cost ledger row");
    assert_eq!(known["reread_tokens"], 1090);
    assert!(unknown["reread_tokens"].is_null());
    assert_ne!(known["move_id"], unknown["move_id"]);
    for (result, session) in [
        (known, "r14-known-session"),
        (unknown, "r14-unknown-session"),
    ] {
        assert_eq!(costs.iter().filter(|row| row["move_id"] == result["move_id"]
            && row["measurement"] != "pending").count(), 1);
        assert!(costs.iter().any(|row| row["move_id"] == result["move_id"]
            && row["measurement"] == "pending"
            && row["session"] == session_hash(session)));
        assert!(rows.iter().any(|row| row["event"] == "binding_move"
            && row["session"] == session_hash(session)
            && row["from"] == a
            && row["to"] == b));
    }
    assert!(
        !rows
            .iter()
            .any(|row| row["event"] == "binding_move_refused")
    );
}

#[tokio::test]
async fn real_http_pingpong_is_refused_before_return_dispatch_and_ends_typed_429() {
    let h = Harness::new(1).await;
    h.fake.state.push(usage_reply(10, Some(0)));
    let initial = h.turn("r14-pingpong-session").await;
    assert_eq!(initial.status, 200);
    let a = initial.account;
    h.fake.state.push(payment_reply());
    h.fake.state.push(usage_reply(1000, Some(100)));
    let moved = h.turn("r14-pingpong-session").await;
    assert_eq!(moved.status, 200);
    let b = moved.account;
    assert_ne!(a, b);
    h.sessions(1, 0).await;

    // Fixture-only recovery makes A a real candidate again. The first move's source
    // was excluded by that HTTP failure: missing departure evidence is NOT return proof.
    let recovered = h
        .store
        .update_metadata(&a, |account| {
            account.health = Health::Ready;
            account.payment_until = None;
            account.payment_failures = 0;
            account.quota_cooling = None;
        })
        .unwrap();
    assert!(recovered.eligible(Some(MODEL)));
    assert_eq!(
        h.state
            .bindings
            .current(Some("claude:r14-pingpong-session")),
        Some(b.clone())
    );
    h.fake.state.push(payment_reply()); // B now fails; A is the only usable target.
    // A must never consume this reply. If it does, count/typed-response assertions fail.
    h.fake.state.push(usage_reply(9999, Some(0)));
    let started = Instant::now();
    let refused = h.turn("r14-pingpong-session").await;
    let elapsed = started.elapsed();
    assert_eq!(
        refused.status, 429,
        "move guard must end with a typed limit, not generic timeout"
    );
    assert_eq!(refused.body["type"], "error");
    assert_eq!(refused.body["error"]["type"], "account_move_guard");
    assert!(refused.headers.get("retry-after").is_some());
    assert!(
        elapsed >= Duration::from_millis(700),
        "forced refusal skipped its bounded hold"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "hold exceeded the request budget"
    );
    assert_eq!(
        h.state
            .bindings
            .current(Some("claude:r14-pingpong-session")),
        Some(b.clone())
    );
    h.assert_upstream_accounts(&[&a, &a, &b, &b]);

    let rows = h.ledger();
    let hash = session_hash("r14-pingpong-session");
    let refusal = rows
        .iter()
        .find(|row| {
            row["event"] == "binding_move_refused"
                && row["session"] == hash
                && row["from"] == b
                && row["to"] == a
        })
        .expect("actual attempted return must be ledgered");
    assert_eq!(refusal["reason"], "ping_pong");
    assert_eq!(refusal["forced"], true);
    let until = DateTime::parse_from_rfc3339(refusal["until"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    assert!(until > Utc::now() + ChronoDuration::minutes(20));
    assert!(
        !rows
            .iter()
            .any(|row| row["event"] == "binding_move" && row["from"] == b && row["to"] == a)
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"] == "binding_move_cost" && row["measurement"] == "pending")
            .count(),
        1,
        "refusal must not register another move"
    );
    let view = h.sessions(1, 0).await;
    assert_eq!(view["move_cost"]["hour"]["tokens"], 1090);
}
