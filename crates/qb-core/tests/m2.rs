use axum::http::StatusCode;
use bytes::Bytes;
use qb_core::{
    accounts::{AccountStore, Provider, UserState, import},
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
    time::Duration,
};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    root: tempfile::TempDir,
    home: PathBuf,
    cpa: PathBuf,
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    pins: Arc<DebugPins>,
    claude: ScriptedUpstream,
    codex: ScriptedUpstream,
    servers: RunningServers,
    client: reqwest::Client,
    #[cfg(feature = "phase0")]
    captures: PathBuf,
}

fn fixture(provider: &str, version: u8, expiry: Option<String>) -> Value {
    json!({"type":provider,"email":format!("{provider}@example.invalid"),"account_id":format!("synthetic-{provider}"),"account_uuid":"synthetic-uuid","organization_uuid":"synthetic-org","access_token":format!("fake-access-{provider}-{version}"),"refresh_token":format!("fake-refresh-{provider}-{version}"),"id_token":format!("fake-id-{provider}-{version}"),"last_refresh":format!("2026-01-0{version}T00:00:00Z"),"expired":expiry.unwrap_or_else(||format!("2030-01-0{version}T00:00:00Z"))})
}

impl Harness {
    async fn new(cap: u64, budget: u64) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/tests");
        fs::create_dir_all(&root).unwrap();
        let root = tempfile::tempdir_in(root).unwrap();
        let home = root.path().join("home");
        let cpa = root.path().join("cpa");
        fs::create_dir_all(&cpa).unwrap();
        for provider in ["claude", "codex"] {
            fs::write(
                cpa.join(format!("{provider}.json")),
                serde_json::to_vec(&fixture(provider, 1, None)).unwrap(),
            )
            .unwrap();
        }
        let store = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match AccountStore::acquire(&home) {
                    Ok(store) => break Arc::new(store),
                    Err(qb_core::accounts::StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => panic!("synthetic account store failed: {error}"),
                }
            }
        })
        .await
        .expect("previous synthetic core did not release lease");
        import::run(
            &store,
            &cpa,
            &[
                "claude:claude@example.invalid".into(),
                "codex:codex@example.invalid".into(),
            ],
            false,
        )
        .unwrap();
        let claude = ScriptedUpstream::start().await.unwrap();
        let codex = ScriptedUpstream::start().await.unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.guard.retain_max_per_request_mib = cap;
        config.guard.inflight_budget_mib = budget;
        config.upstream.anthropic_base = claude.url.clone();
        config.upstream.codex_base = codex.url.clone();
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let pins = Arc::new(DebugPins::default());
        let state = RelayState::new(store.clone(), &config, obs.clone(), pins.clone()).unwrap();
        #[cfg(feature = "phase0")]
        let captures = root.path().join("body-captures");
        #[cfg(feature = "phase0")]
        fs::create_dir(&captures).unwrap();
        #[cfg(feature = "phase0")]
        let state = state.with_body_capture(Arc::new(
            qb_core::phase0::BodyCapture::new(captures.clone(), 20).unwrap(),
        ));
        let state = Arc::new(state);
        let admin = qb_core::debug_pin::routes(store.clone(), pins.clone());
        #[cfg(feature = "phase0")]
        let admin = admin.merge(relay::phase0_routes(state.clone()));
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "fake-local-client".into(),
                admin_token: "fake-local-admin".into(),
            },
            relay::router(state),
            admin,
        )
        .await
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        Self {
            root,
            home,
            cpa,
            store,
            obs,
            pins,
            claude,
            codex,
            servers,
            client,
            #[cfg(feature = "phase0")]
            captures,
        }
    }
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.servers.data_addr)
    }
    fn admin(&self, path: &str) -> String {
        format!("http://{}/admin/v1{path}", self.servers.admin_addr)
    }
    async fn send(&self, path: &str, body: impl Into<reqwest::Body>) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .bearer_auth("fake-local-client")
            .body(body)
            .send()
            .await
            .unwrap()
    }
    fn ledger(&self) -> Vec<Value> {
        self.obs.flush().unwrap();
        let mut result = Vec::new();
        for entry in fs::read_dir(self.home.join("ledger")).unwrap() {
            let text = fs::read_to_string(entry.unwrap().path()).unwrap();
            for token in [
                "fake-access-",
                "fake-refresh-",
                "fake-id-",
                "fake-local-client",
                "fake-local-admin",
            ] {
                assert!(!text.contains(token));
            }
            result.extend(
                text.lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap()),
            );
        }
        result
    }
}

#[tokio::test]
async fn every_route_preserves_path_query_and_c3_headers() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new(64, 384).await;
    for (method, path, claude, upstream) in [
        (
            "POST",
            "/v1/messages?a=%2F&x=1",
            true,
            "/v1/messages?a=%2F&x=1",
        ),
        (
            "POST",
            "/v1/messages/count_tokens?q=yes",
            true,
            "/v1/messages/count_tokens?q=yes",
        ),
        ("GET", "/v1/models?extra=a", true, "/v1/models?extra=a"),
        ("POST", "/v1/responses?q=a", false, "/responses?q=a"),
        ("POST", "/v1/responses/compact", false, "/responses/compact"),
        ("POST", "/v1/alpha/search", false, "/alpha/search"),
        (
            "POST",
            "/v1/images/generations",
            false,
            "/images/generations",
        ),
        ("POST", "/v1/images/edits", false, "/images/edits"),
        (
            "GET",
            "/v1/models?client_version=0.162.1",
            false,
            "/models?client_version=0.162.1",
        ),
    ] {
        let mut request = h
            .client
            .request(method.parse().unwrap(), h.url(path))
            .bearer_auth("fake-local-client")
            .header("x-api-key", "fake-local-client")
            .header("user-agent", "codex_cli_rs/0.162.1 extra/value")
            .header("originator", "codex_cli_rs")
            .header("chatgpt-account-id", "forged")
            .header("connection", "keep-alive, x-hop")
            .header("x-hop", "remove-me")
            .header("proxy-custom", "remove-me")
            .header("sec-websocket-key", "remove-me")
            .header("accept-encoding", "gzip, zstd")
            .header("anthropic-beta", "existing-beta")
            .header("session-id", "synthetic-session")
            .body("{ \"input\": [] }");
        if claude {
            request = request.header("anthropic-version", "2023-06-01");
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.headers().contains_key("x-quota-baton-account"));
        response.bytes().await.unwrap();
        let requests = if claude {
            h.claude.state.requests()
        } else {
            h.codex.state.requests()
        };
        let request = requests.last().unwrap();
        assert_eq!(request.path_and_query, upstream);
        assert_eq!(
            request.headers["authorization"],
            format!(
                "Bearer fake-access-{}-1",
                if claude { "claude" } else { "codex" }
            )
        );
        for removed in ["x-api-key", "x-hop", "proxy-custom", "sec-websocket-key"] {
            assert!(!request.headers.contains_key(removed));
        }
        assert_eq!(request.headers["accept-encoding"], "gzip, zstd");
        assert_eq!(
            request.headers["user-agent"],
            "codex_cli_rs/0.162.1 extra/value"
        );
        if claude {
            assert_eq!(
                request.headers["anthropic-beta"],
                "existing-beta,oauth-2025-04-20"
            );
        } else {
            assert_eq!(request.headers["chatgpt-account-id"], "synthetic-codex");
        }
        assert_eq!(request.body.as_ref(), b"{ \"input\": [] }");
    }
    for path in ["/v1/memories/trace_summarize", "/unknown"] {
        let response = h.send(path, "{}").await;
        assert_eq!(response.status(), 404);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["message"], "Unsupported route");
    }
    assert_eq!(h.claude.state.requests().len(), 3);
    assert_eq!(h.codex.state.requests().len(), 6);
    let rows = h.ledger();
    let request = rows
        .iter()
        .find(|row| row["route"] == "POST /v1/responses")
        .unwrap();
    for key in [
        "ts",
        "req",
        "family",
        "route",
        "transport",
        "account",
        "session",
        "model",
        "originator",
        "ua",
        "status",
        "upstream_status",
        "ttfb_ms",
        "dur_ms",
        "bytes_in",
        "bytes_out",
        "attempts",
        "outcome",
    ] {
        assert!(request.get(key).is_some(), "missing {key}");
    }
    assert_eq!(request["ua"], "codex_cli_rs/0.162.1");
    assert_eq!(request["originator"], "codex_cli_rs");
    assert!(
        !serde_json::to_string(&rows)
            .unwrap()
            .contains("synthetic-session")
    );
}

#[tokio::test]
async fn body_encodings_response_headers_usage_and_beta_merge() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new(64, 384).await;
    let body = b"{\n \"model\": \"synthetic\", \"input\": [1,2,3] }";
    let compressed = zstd::stream::encode_all(&body[..], 1).unwrap();
    let mut reply = ScriptedResponse::json(StatusCode::OK, compressed.clone());
    reply
        .headers
        .insert("content-encoding", "zstd".parse().unwrap());
    reply.headers.append("set-cookie", "a=1".parse().unwrap());
    reply.headers.append("set-cookie", "b=2".parse().unwrap());
    reply.headers.insert("connection", "x-hop".parse().unwrap());
    reply.headers.insert("x-hop", "remove-me".parse().unwrap());
    reply
        .headers
        .insert("x-codex-primary-used-percent", "12.5".parse().unwrap());
    reply
        .headers
        .insert("x-codex-primary-window-minutes", "300".parse().unwrap());
    reply
        .headers
        .insert("x-codex-primary-reset-after-seconds", "60".parse().unwrap());
    h.codex.state.push(reply);
    let response = h
        .client
        .post(h.url("/v1/responses"))
        .bearer_auth("fake-local-client")
        .header("content-encoding", "zstd")
        .body(compressed.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-encoding"], "zstd");
    assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
    assert!(!response.headers().contains_key("x-hop"));
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        compressed.as_slice()
    );
    assert_eq!(
        h.codex.state.requests()[0].body.as_ref(),
        compressed.as_slice()
    );
    for beta in [None, Some("oauth-2025-04-20,existing-beta")] {
        let mut reply = ScriptedResponse::json(StatusCode::OK, "{}");
        reply.headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.25".parse().unwrap(),
        );
        reply.headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            "2000000000".parse().unwrap(),
        );
        reply.headers.insert(
            "anthropic-ratelimit-unified-5h-status",
            "allowed".parse().unwrap(),
        );
        h.claude.state.push(reply);
        let mut request = h
            .client
            .post(h.url("/v1/messages"))
            .bearer_auth("fake-local-client")
            .body(body.to_vec());
        if let Some(beta) = beta {
            request = request.header("anthropic-beta", beta);
        }
        request.send().await.unwrap().bytes().await.unwrap();
        let requests = h.claude.state.requests();
        assert_eq!(
            requests.last().unwrap().headers["anthropic-beta"],
            beta.unwrap_or("oauth-2025-04-20")
        );
    }
    h.obs.flush().unwrap();
    let quota: Value =
        serde_json::from_slice(&fs::read(h.home.join("state/quota.json")).unwrap()).unwrap();
    let quota_text = quota.to_string();
    assert!(quota_text.contains("12.5"));
    assert!(quota_text.contains("25.0") || quota_text.contains("25"));
    assert!(quota_text.contains("5h"));
}

#[tokio::test]
async fn commit_hold_waits_for_normal_data_and_relays_first_error() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new(64, 384).await;
    let first = Bytes::from_static(b": heartbeat\n\n");
    let normal =
        Bytes::from_static(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n");
    let last = Bytes::from_static(b"data: {\"done\":true}\n\n");
    h.claude.state.push(ScriptedResponse::sse(vec![
        (Duration::ZERO, first.clone()),
        (Duration::from_millis(200), normal.clone()),
        (Duration::from_millis(200), last.clone()),
    ]));
    let started = std::time::Instant::now();
    let response = h.send("/v1/messages", "{}").await;
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert!(started.elapsed() < Duration::from_secs(2));
    let expected = [first.as_ref(), normal.as_ref(), last.as_ref()].concat();
    assert_eq!(response.bytes().await.unwrap().as_ref(), expected);
    let error = Bytes::from_static(
        b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}\n\n",
    );
    h.claude
        .state
        .push(ScriptedResponse::sse(vec![(Duration::ZERO, error.clone())]));
    assert_eq!(
        h.send("/v1/messages", "{}").await.bytes().await.unwrap(),
        error
    );
    assert_eq!(h.claude.state.requests().len(), 2);
    let rows = h.ledger();
    assert!(rows.iter().any(|row| {
        row["outcome"]
            .as_str()
            .is_some_and(|value| value.contains("error"))
    }));
}

#[tokio::test]
async fn budget_overflow_forwards_without_retry_copy() {
    let _serial = SERIAL.lock().await;
    for (cap, budget) in [(0, 384), (64, 0)] {
        let mut h = Harness::new(cap, budget).await;
        let body = vec![b'x'; 1024];
        assert_eq!(
            h.send("/v1/responses", body.clone())
                .await
                .bytes()
                .await
                .unwrap(),
            "{\"fake\":true}"
        );
        assert_eq!(h.codex.state.requests()[0].body.as_ref(), body.as_slice());
        let rows = h.ledger();
        assert!(rows.iter().any(|row| row["retained_copy"] == false));
        h.servers.data_task.abort();
        h.servers.admin_task.abort();
        let _ = (&mut h.servers.data_task).await;
        let _ = (&mut h.servers.admin_task).await;
        drop(h);
    }
}

#[tokio::test]
async fn over_cap_chunked_upload_streams_identical_bytes_without_retention() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new(1, 384).await;
    let expected = vec![b'x'; 3 * 1024 * 1024 + 17];
    let chunks: Vec<_> = expected.chunks(8192).map(Bytes::copy_from_slice).collect();
    let body = reqwest::Body::wrap_stream(futures_util::stream::iter(
        chunks.into_iter().map(Ok::<_, std::io::Error>),
    ));
    let response = h.send("/v1/responses", body).await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    assert_eq!(h.codex.state.requests()[0].body.as_ref(), expected);
    let rows = h.ledger();
    let row = rows
        .iter()
        .find(|row| row["route"] == "POST /v1/responses")
        .unwrap();
    assert_eq!(row["retained_copy"], false);
    assert_eq!(row["bytes_in"], expected.len());
    assert_eq!(row["attempts"], 1);
}

#[tokio::test]
async fn expiry_reload_is_throttled_stale_events_limited_and_401_bypasses_throttle() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new(64, 384).await;
    let mut account = h
        .store
        .list()
        .unwrap()
        .into_iter()
        .find(|a| a.provider == Provider::Claude)
        .unwrap();
    let tokens = h.store.tokens(&account).unwrap();
    account.token_meta.expires_at = Some(chrono::Utc::now() + chrono::Duration::minutes(5));
    account.token_meta.last_refresh_at = Some("2031-01-01T00:00:00Z".parse().unwrap());
    let mut source = fixture(
        "claude",
        1,
        Some(account.token_meta.expires_at.unwrap().to_rfc3339()),
    );
    source["last_refresh"] = json!("2031-01-01T00:00:00Z");
    h.store
        .save(&account, &tokens, Some(account.generation))
        .unwrap();
    fs::write(
        h.cpa.join("claude.json"),
        serde_json::to_vec(&source).unwrap(),
    )
    .unwrap();
    for _ in 0..3 {
        h.send("/v1/messages", "{}").await.bytes().await.unwrap();
    }
    let rows = h.ledger();
    assert_eq!(
        rows.iter()
            .filter(|r| r["event"] == "linked_reload")
            .count(),
        1
    );
    assert_eq!(
        rows.iter().filter(|r| r["outcome"] == "link_stale").count(),
        1
    );
    source["last_refresh"] = json!("2031-01-02T00:00:00Z");
    source["access_token"] = json!("fake-access-claude-2");
    source["expired"] = json!("2030-01-02T00:00:00Z");
    fs::write(
        h.cpa.join("claude.json"),
        serde_json::to_vec(&source).unwrap(),
    )
    .unwrap();
    h.claude
        .state
        .push(ScriptedResponse::json(StatusCode::UNAUTHORIZED, "{}"));
    h.claude
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{}"));
    let response = h.send("/v1/messages", "{}").await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let requests = h.claude.state.requests();
    assert_eq!(
        requests[3].headers["authorization"],
        "Bearer fake-access-claude-1"
    );
    assert_eq!(
        requests[4].headers["authorization"],
        "Bearer fake-access-claude-2"
    );
    h.claude
        .state
        .push(ScriptedResponse::json(StatusCode::UNAUTHORIZED, "{}"));
    assert_eq!(h.send("/v1/messages", "{}").await.status(), 401);
    let rows = h.ledger();
    assert_eq!(
        rows.iter().filter(|r| r["outcome"] == "link_stale").count(),
        1
    );
    assert!(rows.iter().any(|r| r["event"] == "linked_reload"
        && r["trigger"] == "upstream_401"
        && r["outcome"] == "updated"));
}

#[tokio::test]
async fn linked_reload_before_expiry_and_401_once_without_health_changes() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new(64, 384).await;
    let old = h
        .store
        .list()
        .unwrap()
        .into_iter()
        .find(|account| account.provider == Provider::Claude)
        .unwrap();
    let tokens = h.store.tokens(&old).unwrap();
    let mut near = old.clone();
    near.token_meta.expires_at = Some(chrono::Utc::now() + chrono::Duration::minutes(5));
    // 합성 원본과 저장 레코드의 시각을 맞춘다.
    let mut source = fixture(
        "claude",
        1,
        Some(near.token_meta.expires_at.unwrap().to_rfc3339()),
    );
    source["last_refresh"] = json!("2031-01-01T00:00:00Z");
    near.token_meta.last_refresh_at = Some("2031-01-01T00:00:00Z".parse().unwrap());
    h.store.save(&near, &tokens, Some(old.generation)).unwrap();
    source["last_refresh"] = json!("2031-01-02T00:00:00Z");
    source["access_token"] = json!("fake-access-claude-2");
    source["expired"] = json!("2032-01-02T00:00:00Z");
    fs::write(
        h.cpa.join("claude.json"),
        serde_json::to_vec(&source).unwrap(),
    )
    .unwrap();
    h.send("/v1/messages", "{}").await.bytes().await.unwrap();
    assert_eq!(
        h.claude.state.requests()[0].headers["authorization"],
        "Bearer fake-access-claude-2"
    );
    fs::write(
        h.cpa.join("codex.json"),
        serde_json::to_vec(&fixture("codex", 2, None)).unwrap(),
    )
    .unwrap();
    h.codex.state.push(ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        "{\"error\":\"synthetic unauthorized\"}",
    ));
    h.codex
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{\"ok\":true}"));
    assert_eq!(h.send("/v1/responses", "{}").await.status(), 200);
    let requests = h.codex.state.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer fake-access-codex-1"
    );
    assert_eq!(
        requests[1].headers["authorization"],
        "Bearer fake-access-codex-2"
    );
    h.codex.state.push(ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        "{\"error\":\"synthetic unauthorized\"}",
    ));
    let response = h.send("/v1/responses", "{}").await;
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.text().await.unwrap(),
        "{\"error\":\"synthetic unauthorized\"}"
    );
    assert_eq!(h.codex.state.requests().len(), 3);
    assert!(
        h.store
            .list()
            .unwrap()
            .iter()
            .all(|account| account.health == qb_core::accounts::Health::Ready)
    );
    let rows = h.ledger();
    assert!(
        rows.iter()
            .any(|row| row.to_string().contains("link_stale"))
    );
}

#[tokio::test]
async fn global_codex_pin_leaves_claude_active_account_usable() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new(64, 384).await;
    let accounts = h.store.list().unwrap();
    let codex = accounts
        .iter()
        .find(|a| a.provider == Provider::Codex)
        .unwrap();
    let claude = accounts
        .iter()
        .find(|a| a.provider == Provider::Claude)
        .unwrap();
    assert_eq!(
        h.client
            .post(h.admin("/debug/pin"))
            .bearer_auth("fake-local-admin")
            .json(&json!({"account_id":codex.id}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let response = h.send("/v1/messages", "{}").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-quota-baton-account"], claude.id);
    response.bytes().await.unwrap();
    assert_eq!(h.send("/v1/responses", "{}").await.status(), 200);
    assert_eq!(h.claude.state.requests().len(), 1);
    assert_eq!(h.codex.state.requests().len(), 1);
}

#[tokio::test]
async fn local_machine_errors_are_english_without_secrets() {
    let _serial = SERIAL.lock().await;
    let mut h = Harness::new(64, 384).await;
    for (name, value, expected, message) in [
        (
            "authorization",
            "Bearer fake-wrong",
            401,
            "Invalid local credential",
        ),
        ("host", "evil.invalid", 421, "Invalid Host"),
        ("origin", "null", 403, "Origin is forbidden"),
    ] {
        let response = h
            .client
            .post(h.url("/v1/messages"))
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["message"], message);
        assert!(!body.to_string().contains("fake-wrong"));
    }
    h.claude.state.push(ScriptedResponse::json(
        StatusCode::BAD_GATEWAY,
        r#"{"error":{"type":"api_error","message":"Synthetic upstream failure"}}"#,
    ));
    let response = h.send("/v1/messages", "{}").await;
    assert_eq!(response.status(), 502);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("Synthetic upstream failure")
    );
    let old = std::mem::replace(&mut h.codex, ScriptedUpstream::start().await.unwrap());
    drop(old);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let response = h.send("/v1/responses", "{}").await;
    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["message"], "Could not connect to upstream");
    assert!(!body.to_string().contains("fake-access"));
}

#[cfg(feature = "phase0")]
#[tokio::test]
async fn phase0_authenticated_replay_uses_forced_account_and_linked_401_path() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new(64, 384).await;
    let body = r#"{"input":[{"type":"reasoning","encrypted_content":"synthetic-encrypted"},{"type":"message","content":[{"type":"input_text","text":"synthetic conversation"}]}]}"#;
    h.send("/v1/responses", body).await.bytes().await.unwrap();
    let capture = fs::read_dir(&h.captures)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut second = fixture("codex", 1, None);
    second["email"] = json!("second@example.invalid");
    second["account_id"] = json!("synthetic-second");
    fs::write(
        h.cpa.join("second.json"),
        serde_json::to_vec(&second).unwrap(),
    )
    .unwrap();
    import::run(&h.store, &h.cpa, &[], false).unwrap();
    let account = h
        .store
        .list()
        .unwrap()
        .into_iter()
        .find(|a| {
            a.identity
                .canonical_key()
                .contains("second@example.invalid")
        })
        .unwrap();
    second["last_refresh"] = json!("2026-01-02T00:00:00Z");
    second["access_token"] = json!("fake-access-second-2");
    fs::write(
        h.cpa.join("second.json"),
        serde_json::to_vec(&second).unwrap(),
    )
    .unwrap();
    let request = json!({"capture_path":capture,"account_id":account.id,"strip_encrypted":"none","tamper_encrypted":false});
    assert_eq!(
        h.client
            .post(h.admin("/phase0/replay"))
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    h.codex
        .state
        .push(ScriptedResponse::json(StatusCode::UNAUTHORIZED, "{}"));
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("content-type", "text/event-stream".parse().unwrap());
    h.codex.state.push(ScriptedResponse {status:StatusCode::OK,headers,chunks:vec![(Duration::ZERO,Bytes::from_static(b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":42,\"input_tokens_details\":{\"cached_tokens\":12}},\"output\":[{\"text\":\"synthetic conversation\"}]}}\n\n"))]});
    let response = h
        .client
        .post(h.admin("/phase0/replay"))
        .bearer_auth("fake-local-admin")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let summary: Value = response.json().await.unwrap();
    assert_eq!(summary["status"], 200);
    assert_eq!(summary["account"], account.id);
    assert_eq!(summary["strip_encrypted"], "none");
    let text = summary.to_string();
    assert!(text.contains("42"));
    assert!(text.contains("12"));
    assert!(!text.contains("synthetic conversation"));
    assert!(!text.contains("synthetic-encrypted"));
    assert!(!text.contains("fake-access"));
    let sent = h.codex.state.requests();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1].headers["chatgpt-account-id"], "synthetic-second");
    assert_eq!(
        sent[2].headers["authorization"],
        "Bearer fake-access-second-2"
    );
    assert_eq!(&sent[2].body[..], body.as_bytes());
    assert_eq!(fs::read_dir(&h.captures).unwrap().count(), 1);
    let invalid = json!({"capture_path":Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),"account_id":account.id});
    assert_eq!(
        h.client
            .post(h.admin("/phase0/replay"))
            .bearer_auth("fake-local-admin")
            .json(&invalid)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn no_account_and_debug_pin_routes() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new(64, 384).await;
    for mut account in h.store.list().unwrap() {
        let tokens = h.store.tokens(&account).unwrap();
        account.user_state = UserState::Disabled;
        h.store
            .save(&account, &tokens, Some(account.generation))
            .unwrap();
    }
    let response = h.send("/v1/messages", "{}").await;
    assert_eq!(response.status(), 400);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert!(error["error"]["message"].as_str().unwrap().contains("계정"));
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no usable claude account")
    );
    let account = h
        .store
        .list()
        .unwrap()
        .into_iter()
        .find(|account| account.provider == Provider::Codex)
        .unwrap();
    let pin = json!({"account_id":account.id,"session_key":"codex:synthetic-session"});
    assert_eq!(
        h.client
            .post(h.admin("/debug/pin"))
            .json(&pin)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        h.client
            .post(h.admin("/debug/pin"))
            .bearer_auth("fake-local-admin")
            .json(&pin)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let response = h
        .client
        .post(h.url("/v1/responses"))
        .bearer_auth("fake-local-client")
        .header("session_id", "synthetic-session")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    assert_eq!(h.send("/v1/responses", "{}").await.status(), 400);
    assert_eq!(
        h.client
            .delete(h.admin("/debug/pin"))
            .bearer_auth("fake-local-admin")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        h.pins
            .get(Provider::Codex, Some("codex:synthetic-session"))
            .is_none()
    );
    assert!(h.root.path().exists());
}
