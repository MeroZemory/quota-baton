#![cfg(all(feature = "testkit", debug_assertions))]

//! Relay acceptance, not direct BurstTracker tests. Window arithmetic stays in
//! burst.rs's pure clock tests; every fixture and endpoint here is synthetic.
use axum::http::StatusCode;
use bytes::Bytes;
use futures_util::{SinkExt, Stream, StreamExt};
use qb_core::{
    accounts::{AccountStore, Provider, Source, import},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use qb_testkit::{
    ScriptedResponse, ScriptedUpstream,
    websocket::{ConnectionEvent, Message, WebSocketUpstream},
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, pin::Pin, sync::Arc, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};

const LIMIT: Duration = Duration::from_secs(10);
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct CombinedUpstream {
    url: String,
    task: JoinHandle<()>,
}

impl CombinedUpstream {
    // One configured Codex base must serve both POST responses and GET upgrades.
    // Delegate to the existing M4a fakes without changing either harness API.
    async fn start(http: &ScriptedUpstream, ws: &WebSocketUpstream) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        // Routing below is per connection. Close every HTTP response so the
        // relay's shared client cannot reuse a POST connection for a WS GET.
        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_address = http_listener.local_addr().unwrap();
        let http_router = http.state.router().layer(axum::middleware::map_response(
            |mut response: axum::response::Response| async move {
                response.headers_mut().insert(
                    axum::http::header::CONNECTION,
                    axum::http::HeaderValue::from_static("close"),
                );
                response
            },
        ));
        let ws = ws.url.strip_prefix("http://").unwrap().to_owned();
        let task = tokio::spawn(async move {
            // Keep the HTTP listener in this task so Drop aborts it too.
            let http_server = async move {
                let _ = axum::serve(http_listener, http_router).await;
            };
            tokio::pin!(http_server);
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut http_server => break,
                    accepted = listener.accept() => {
                        let Ok((mut incoming, _)) = accepted else { break };
                        let ws = ws.clone();
                        connections.spawn(async move {
                            let _ = tokio::time::timeout(Duration::from_secs(30), async {
                                let mut first = [0];
                                if incoming.peek(&mut first).await? == 0 {
                                    return Ok::<_, std::io::Error>(());
                                }
                                let mut outgoing = if first[0] == b'G' {
                                    tokio::net::TcpStream::connect(ws).await?
                                } else {
                                    tokio::net::TcpStream::connect(http_address).await?
                                };
                                tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await?;
                                Ok(())
                            }).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self { url, task }
    }
}

impl Drop for CombinedUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Harness {
    servers: RunningServers,
    state: Arc<RelayState>,
    obs: Arc<Observability>,
    data: ScriptedUpstream,
    ws: WebSocketUpstream,
    token: ScriptedUpstream,
    _combined: CombinedUpstream,
    client: reqwest::Client,
    home: PathBuf,
    id: String,
    _store: Arc<AccountStore>,
    _root: tempfile::TempDir,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Harness {
    async fn new(provider: Provider, threshold: u32, restored: bool) -> Self {
        let serial = SERIAL.lock().await;
        let base =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r1-burst-fixtures");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let cpa = root.path().join("synthetic-import");
        fs::create_dir_all(&cpa).unwrap();
        fs::write(
            cpa.join("fixture.json"),
            serde_json::to_vec(&json!({
                "type": provider.as_str(), "email": "r1-burst@example.invalid",
                "account_id": "synthetic-burst", "account_uuid": "synthetic-burst",
                "organization_uuid": "synthetic-org", "access_token": "synthetic-access",
                "refresh_token": "synthetic-refresh", "expired": "2099-01-01T00:00:00Z",
                "last_refresh": "2026-01-01T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
        let store = tokio::time::timeout(LIMIT, async {
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
        import::run(
            &store,
            &cpa,
            &[format!("{}:r1-burst@example.invalid", provider.as_str())],
            false,
        )
        .unwrap();
        let mut account = store.list().unwrap().remove(0);
        let tokens = store.tokens(&account).unwrap();
        account.source = Source::Oauth;
        let account = store
            .save(&account, &tokens, Some(account.generation))
            .unwrap();
        let id = account.id;
        if restored {
            fs::create_dir_all(home.join("state")).unwrap();
            fs::write(
                home.join("state/bindings.json"),
                serde_json::to_vec(&json!({
                    "schema": 1, "bindings": [{
                        "key": format!("{}:synthetic-restored", provider.as_str()),
                        "account": id, "last_used": chrono::Utc::now()
                    }]
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let data = ScriptedUpstream::start().await.unwrap();
        let ws = WebSocketUpstream::start().await.unwrap();
        ws.state.set_echo(false);
        let token = ScriptedUpstream::start().await.unwrap();
        let combined = CombinedUpstream::start(&data, &ws).await;
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.tray.burst_new_sessions = threshold;
        config.upstream.codex_base = combined.url.clone();
        config.upstream.anthropic_base = data.url.clone();
        // Even an unexpected usage probe or auth retry can only reach our fakes.
        config.upstream.codex_usage_base = data.url.clone();
        config.upstream.claude_usage_base = data.url.clone();
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap()
            .with_token_endpoint(Provider::Codex, token.url.clone())
            .with_token_endpoint(Provider::Claude, token.url.clone()),
        );
        // Deliberately no refresh, quota, or memory schedulers.
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
            .timeout(LIMIT)
            .build()
            .unwrap();
        Self {
            servers,
            state,
            obs,
            data,
            ws,
            token,
            _combined: combined,
            client,
            home,
            id,
            _store: store,
            _root: root,
            _serial: serial,
        }
    }

    fn request(
        &self,
        provider: Provider,
        session: &str,
        originator: &str,
    ) -> reqwest::RequestBuilder {
        let path = if provider == Provider::Codex {
            "/v1/responses"
        } else {
            "/v1/messages"
        };
        self.client
            .post(format!("http://{}{path}", self.servers.data_addr))
            .bearer_auth("synthetic-client")
            .header("session-id", session)
            .header("x-claude-code-session-id", session)
            .header("originator", originator)
            .header("user-agent", "synthetic-burst/1")
            .header("content-type", "application/json")
            .body(r#"{"model":"synthetic-model","input":"synthetic-body","messages":[]}"#)
    }

    async fn send(&self, provider: Provider, session: &str, originator: &str) {
        let response = self
            .request(provider, session, originator)
            .send()
            .await
            .unwrap();
        self.served(response).await;
    }

    async fn served(&self, response: reqwest::Response) {
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-quota-baton-account"], self.id);
        assert_eq!(response.text().await.unwrap(), r#"{"fake":true}"#);
    }

    async fn websocket(&self, session: &str, originator: &str) {
        let mut request = format!("ws://{}/v1/responses", self.servers.data_addr)
            .into_client_request()
            .unwrap();
        for (name, value) in [
            ("authorization", "Bearer synthetic-client"),
            ("session-id", session),
            ("originator", originator),
            ("user-agent", "synthetic-ws/2"),
        ] {
            request.headers_mut().insert(name, value.parse().unwrap());
        }
        let (mut client, response) = tokio::time::timeout(LIMIT, connect_async(request))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.headers()["x-quota-baton-account"], self.id);
        client
            .send(Message::Text(
                json!({
                    "type": "response.create", "model": "synthetic-model", "input": []
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(LIMIT, client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Message::Text(r#"{"type":"response.completed"}"#.into())
        );
        client.close(None).await.unwrap();
    }

    fn complete_ws(&self) {
        self.ws
            .state
            .push_reply([Message::Text(r#"{"type":"response.completed"}"#.into())]);
    }

    async fn events(&self) -> Events {
        // The admin headers resolve only after the real route subscribed.
        let response = self
            .client
            .get(format!(
                "http://{}/admin/v1/events",
                self.servers.admin_addr
            ))
            .bearer_auth("synthetic-admin")
            .timeout(Duration::from_secs(45))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        Events {
            stream: Box::pin(response.bytes_stream()),
            pending: Vec::new(),
        }
    }

    fn ledger(&self) -> Vec<Value> {
        self.obs.flush().unwrap();
        fs::read_dir(self.home.join("ledger"))
            .unwrap()
            .flat_map(|entry| {
                fs::read_to_string(entry.unwrap().path())
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn assert_ledger(&self, counts: &[u32], new_groups: usize) {
        let ledger = self.ledger();
        let mut burst = Vec::new();
        for mut row in ledger
            .iter()
            .filter(|row| row["event"] == "core_warning")
            .cloned()
        {
            let ts = row.as_object_mut().unwrap().remove("ts").unwrap();
            assert!(chrono::DateTime::parse_from_rfc3339(ts.as_str().unwrap()).is_ok());
            if row["kind"] == "burst" {
                burst.push(row);
            } else {
                // Legacy warnings retain exactly account + code (no burst fields).
                let code = row["code"].as_str().unwrap().to_owned();
                assert!(code.starts_with("synthetic-marker-"));
                assert_eq!(
                    row,
                    json!({"event":"core_warning", "account":"synthetic-account", "code":code})
                );
            }
        }
        assert_eq!(
            burst,
            counts
                .iter()
                .map(|count| json!({
                    "event":"core_warning", "kind":"burst", "burst_new_sessions":count
                }))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            ledger
                .iter()
                .filter(|row| row["event"] == "binding_move" && row["reason"] == "new")
                .count(),
            new_groups
        );
    }
}

struct Events {
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    pending: Vec<u8>,
}

impl Events {
    async fn next(&mut self) -> (String, Value) {
        tokio::time::timeout(LIMIT, async {
            loop {
                if let Some(end) = self.pending.windows(2).position(|pair| pair == b"\n\n") {
                    let frame: Vec<_> = self.pending.drain(..end + 2).collect();
                    let text = std::str::from_utf8(&frame).unwrap();
                    let name = text.lines().find_map(|line| line.strip_prefix("event: "));
                    let data = text.lines().find_map(|line| line.strip_prefix("data: "));
                    if let (Some(name), Some(data)) = (name, data) {
                        return (name.to_owned(), serde_json::from_str(data).unwrap());
                    }
                } else {
                    let chunk = self.stream.next().await.expect("SSE closed").unwrap();
                    self.pending.extend_from_slice(&chunk);
                    assert!(self.pending.len() <= 64 * 1024, "bounded synthetic SSE");
                }
            }
        })
        .await
        .expect("SSE marker deadline")
    }

    async fn checkpoint(&mut self, h: &Harness, marker: u32, counts: &[u32]) {
        let code = format!("synthetic-marker-{marker}");
        h.obs.core_warning("synthetic-account", &code);
        let mut warnings = Vec::new();
        loop {
            let (name, data) = self.next().await;
            if name != "core_warning" {
                continue;
            }
            if data["code"] == code {
                assert_eq!(data, json!({"account":"synthetic-account", "code":code}));
                break;
            }
            warnings.push(data);
        }
        // Exact equality proves count type and payload safety: no identity,
        // session, body, account, code, or hash can be smuggled into a burst.
        assert_eq!(
            warnings,
            counts
                .iter()
                .map(|count| json!({"kind":"burst", "burst_new_sessions":count}))
                .collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn mixed_http_ws_threshold_concurrent_resume_and_originator_isolation() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let h = Harness::new(Provider::Codex, 2, false).await;
        let mut events = h.events().await;
        // Concurrent first placement of one canonical group must reserve once.
        tokio::join!(
            h.send(Provider::Codex, "synthetic-one", "synthetic-origin-a"),
            h.send(Provider::Codex, "synthetic-one", "synthetic-origin-a")
        );
        h.complete_ws();
        h.websocket("synthetic-two", "synthetic-origin-a").await;
        h.send(Provider::Codex, "synthetic-two", "synthetic-origin-a")
            .await;
        h.complete_ws();
        h.websocket("synthetic-one", "synthetic-origin-a").await;
        events.checkpoint(&h, 1, &[]).await; // count exactly two, not attempts.
        h.send(Provider::Codex, "synthetic-other-one", "synthetic-origin-b")
            .await;
        h.complete_ws();
        h.websocket("synthetic-other-two", "synthetic-origin-b")
            .await;
        events.checkpoint(&h, 2, &[]).await;
        h.send(Provider::Codex, "synthetic-three", "synthetic-origin-a")
            .await;
        events.checkpoint(&h, 3, &[3]).await;
        h.complete_ws();
        h.websocket("synthetic-four", "synthetic-origin-a").await;
        h.send(Provider::Codex, "synthetic-five", "synthetic-origin-a")
            .await;
        events.checkpoint(&h, 4, &[]).await; // one warning while above threshold.
        h.complete_ws();
        h.websocket("synthetic-other-three", "synthetic-origin-b")
            .await;
        events.checkpoint(&h, 5, &[3]).await;
        h.assert_ledger(&[3, 3], 8);
        // No SSE subscriber is needed to recover an aggregate while tray is hidden.
        drop(events);
        let state: Value = h
            .client
            .get(format!("http://{}/admin/v1/state", h.servers.admin_addr))
            .bearer_auth("synthetic-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let warnings = state["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["kind"], "burst");
        assert_eq!(warnings[0]["burst_new_sessions"], 3);
        assert_eq!(warnings[0].as_object().unwrap().len(), 3);
        assert!(
            chrono::DateTime::parse_from_rfc3339(warnings[0]["observed_at"].as_str().unwrap())
                .is_ok()
        );

        assert!(h.token.state.requests().is_empty());
    })
    .await
    .expect("mixed relay acceptance deadline");
}

#[tokio::test]
async fn http_401_and_rate_retry_and_ws_same_account_redial_count_one_group() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let h = Harness::new(Provider::Codex, 2, false).await;
        let mut events = h.events().await;
        h.data.state.push(ScriptedResponse::json(StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"synthetic expired token"}}"#));
        h.token.state.push(ScriptedResponse::json(StatusCode::OK,
            r#"{"access_token":"synthetic-refreshed","refresh_token":"synthetic-rotated","expires_in":3600}"#));
        h.send(Provider::Codex, "synthetic-retry", "synthetic-retry-origin").await;
        assert_eq!(h.data.state.requests().len(), 2);
        assert_eq!(h.token.state.requests().len(), 1);
        assert_eq!(h.data.state.requests()[1].headers["authorization"], "Bearer synthetic-refreshed");
        h.ws.state.push_reply([
            Message::Text(r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into()),
            Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: 1012.into(), reason: "synthetic redial".into(),
            })),
        ]);
        h.complete_ws();
        h.websocket("synthetic-retry", "synthetic-retry-origin").await;
        assert_eq!(h.ws.state.requests().len(), 2, "actual upstream redial");
        assert_eq!(h.ws.state.events().iter().filter(|event|
            matches!(event, ConnectionEvent::Opened(_))).count(), 2);
        assert_eq!(h.ws.state.messages().iter().filter(|message|
            matches!(message, Message::Text(_))).count(), 2, "create resent after redial");
        let mut limited = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"code":"rate_limit_exceeded"}}"#);
        limited.headers.insert("retry-after", "1".parse().unwrap());
        h.data.state.push(limited);
        h.send(Provider::Codex, "synthetic-retry", "synthetic-retry-origin").await;
        assert_eq!(h.data.state.requests().len(), 4, "HTTP rate hold retried in place");
        assert!(h.ledger().iter().any(|row|
            row["rate_retries"] == 1 && row["outcome"] == "rate_hold_ok"));
        h.send(Provider::Codex, "synthetic-second", "synthetic-retry-origin").await;
        events.checkpoint(&h, 1, &[]).await;
        h.complete_ws();
        h.websocket("synthetic-third", "synthetic-retry-origin").await;
        events.checkpoint(&h, 2, &[3]).await;
        h.assert_ledger(&[3], 3);
    }).await.expect("retry relay acceptance deadline");
}

#[tokio::test]
async fn claude_children_share_canonical_group_and_strict_crossing() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let h = Harness::new(Provider::Claude, 2, false).await;
        let mut events = h.events().await;
        h.send(
            Provider::Claude,
            "synthetic-root",
            "synthetic-claude-origin",
        )
        .await;
        for (agent, parent) in [
            ("synthetic-child", "main"),
            ("synthetic-grandchild", "synthetic-child"),
        ] {
            let response = h
                .request(
                    Provider::Claude,
                    "synthetic-root",
                    "synthetic-claude-origin",
                )
                .header("x-claude-code-agent-id", agent)
                .header("x-claude-code-parent-agent-id", parent)
                .send()
                .await
                .unwrap();
            h.served(response).await;
        }
        h.send(
            Provider::Claude,
            "synthetic-second",
            "synthetic-claude-origin",
        )
        .await;
        events.checkpoint(&h, 1, &[]).await;
        h.send(
            Provider::Claude,
            "synthetic-third",
            "synthetic-claude-origin",
        )
        .await;
        events.checkpoint(&h, 2, &[3]).await;
        h.send(
            Provider::Claude,
            "synthetic-fourth",
            "synthetic-claude-origin",
        )
        .await;
        events.checkpoint(&h, 3, &[]).await;
        assert_eq!(
            h.state
                .bindings
                .current(Some("claude:synthetic-root:agent:synthetic-grandchild")),
            Some(h.id.clone())
        );
        h.assert_ledger(&[3], 4);
        assert!(h.token.state.requests().is_empty());
    })
    .await
    .expect("Claude relay acceptance deadline");
}

#[tokio::test]
async fn disk_restored_binding_http_and_ws_do_not_create_observations() {
    tokio::time::timeout(Duration::from_secs(45), async {
        let h = Harness::new(Provider::Codex, 0, true).await;
        let mut events = h.events().await;
        assert_eq!(
            h.state.bindings.current(Some("codex:synthetic-restored")),
            Some(h.id.clone())
        );
        h.send(
            Provider::Codex,
            "synthetic-restored",
            "synthetic-restored-origin",
        )
        .await;
        h.complete_ws();
        h.websocket("synthetic-restored", "synthetic-restored-origin")
            .await;
        events.checkpoint(&h, 1, &[]).await;
        h.send(
            Provider::Codex,
            "synthetic-new",
            "synthetic-restored-origin",
        )
        .await;
        events.checkpoint(&h, 2, &[1]).await;
        h.send(
            Provider::Codex,
            "synthetic-next",
            "synthetic-restored-origin",
        )
        .await;
        events.checkpoint(&h, 3, &[]).await;
        h.assert_ledger(&[1], 2);
    })
    .await
    .expect("restored relay acceptance deadline");
}
