#![cfg(all(feature = "testkit", debug_assertions))]
//! Synthetic loopback only. Test execution is coordinated by the primary worker.
use axum::http::{HeaderMap, StatusCode};
use futures_util::{SinkExt, StreamExt};
use qb_core::{
    accounts::{AccountStore, import},
    admin,
    config::Config,
    dashboard::{SessionId, history::*},
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream, websocket::WebSocketUpstream};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const CLIENT: &str = "r7-synthetic-client";
const ADMIN: &str = "r7-synthetic-admin";
const SID: &str = "12345678-r7-private-tail";
const LABEL: &str = "r7-folder-sentinel";

struct Harness {
    root: tempfile::TempDir,
    home: PathBuf,
    store: Arc<AccountStore>,
    config: Config,
    obs: Arc<Observability>,
    state: Arc<RelayState>,
    servers: RunningServers,
    client: reqwest::Client,
}
impl Harness {
    async fn new(upstream: &str) -> Self {
        let base =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r7-dashboard-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        let home = root.path().join("home");
        let external = root.path().join("synthetic-cpa");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("fixture.json"),serde_json::to_vec(&json!({"type":"codex","email":"r7@example.invalid","account_id":"r7-synthetic-account","access_token":"r7-synthetic-access","refresh_token":"r7-synthetic-refresh","expired":"2099-01-01T00:00:00Z","last_refresh":"2026-10-19T00:00:00Z"})).unwrap()).unwrap();
        let store = Arc::new(AccountStore::acquire(&home).unwrap());
        import::run(
            &store,
            &external,
            &["codex:r7@example.invalid".into()],
            false,
        )
        .unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.codex_base = upstream.into();
        config.upstream.anthropic_base = upstream.into();
        config.upstream.claude_usage_base = upstream.into();
        config.upstream.codex_usage_base = upstream.into();
        config.dashboard.resolve_claude_project = false;
        #[cfg(feature = "shape-capture")]
        let capture = {
            let dir = root.path().join("capture");
            fs::create_dir(&dir).unwrap();
            Some(qb_core::observability::CaptureOptions { dir, limit: 10 })
        };
        #[cfg(not(feature = "shape-capture"))]
        let capture = None;
        let obs = Arc::new(Observability::new(&home, capture).unwrap());
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap(),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: CLIENT.into(),
                admin_token: ADMIN.into(),
            },
            relay::router(state.clone()),
            admin::runtime_routes(state.clone(), 0),
        )
        .await
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self {
            root,
            home,
            store,
            config,
            obs,
            state,
            servers,
            client,
        }
    }
    async fn get(&self, path: &str) -> Value {
        let response = self
            .client
            .get(format!(
                "http://{}/admin/v1/{path}",
                self.servers.admin_addr
            ))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }
    fn id() -> String {
        SessionId::from_key(&format!("codex:{SID}")).as_str().into()
    }
    async fn settled(&self, requests: u64) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let detail = self.get(&format!("sessions/{}", Self::id())).await;
                if detail["requests_today"] == requests && detail["inflight"]["requests"] == 0 {
                    return detail;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
    async fn stop(&self) {
        self.servers.shutdown();
        self.servers.wait_for_listener_close().await;
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.servers.shutdown();
    }
}

#[tokio::test]
async fn r10_relay_quota_extension_keeps_one_activity_and_persists_raw_health_changes() {
    use qb_core::accounts::{Health, HealthReason};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Barrier, Semaphore};

    // Both requests reach upstream before either quota header can cool the pool.
    // Release response two only after response one's health was persisted.
    let admitted = Arc::new(Barrier::new(3));
    let release = Arc::new([Semaphore::new(0), Semaphore::new(0)]);
    let ordinal = Arc::new(AtomicUsize::new(0));
    let reset = chrono::Utc::now().timestamp() + 7200;
    let app = axum::Router::new().fallback({
        let admitted = admitted.clone();
        let release = release.clone();
        move || {
            let admitted = admitted.clone();
            let release = release.clone();
            let index = ordinal.fetch_add(1, Ordering::SeqCst);
            async move {
                assert!(index < 2, "unexpected upstream retry");
                admitted.wait().await;
                release[index].acquire().await.unwrap().forget();
                axum::http::Response::builder()
                    .header("content-type", "application/json")
                    .header("x-codex-limit-reached", "true")
                    .header("x-codex-primary-used-percent", "100")
                    .header("x-codex-primary-window-minutes", "300")
                    .header(
                        "x-codex-primary-reset-at",
                        (reset + index as i64).to_string(),
                    )
                    .body(axum::body::Body::from(r#"{"output":[]}"#))
                    .unwrap()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let h = Harness::new(&url).await;
    let account = h.store.list().unwrap().remove(0);
    let mut requests = Vec::new();
    for session in ["r10-quota-one", "r10-quota-two"] {
        let request = h
            .client
            .post(format!("http://{}/v1/responses", h.servers.data_addr))
            .bearer_auth(CLIENT)
            .header("session-id", session)
            .json(&json!({"model":"synthetic-model", "input":[]}));
        requests.push(tokio::spawn(async move {
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.bytes().await.unwrap();
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), admitted.wait())
        .await
        .unwrap();
    release[0].add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(h.store.load(&account.id).unwrap().health, Health::Cooling { reason: HealthReason::Quota, until, .. } if until.timestamp() == reset) {
                break;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    release[1].add_permits(1);
    for request in requests {
        request.await.unwrap();
    }
    let persisted = h.store.load(&account.id).unwrap();
    assert!(
        matches!(persisted.health, Health::Cooling { reason: HealthReason::Quota, scope, until } if scope == "account" && until.timestamp() == reset + 1)
    );
    let evidence = persisted.quota_cooling.unwrap();
    assert_eq!(evidence.window, "5h");
    assert_eq!(evidence.reset_at.timestamp(), reset + 1);
    let activity = h.get("activity").await;
    let events = activity["events"].as_array().unwrap();
    let reached: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "quota_reached")
        .collect();
    assert_eq!(reached.len(), 1);
    assert!(!events.iter().any(|e| e["kind"] == "quota_reset"));
    // Health.scope is account, not the causing window. Do not invent a window.
    assert!(reached[0]["data"]["window"].is_null());
    h.obs.flush().unwrap();
    let mut changes = Vec::new();
    for entry in fs::read_dir(h.home.join("ledger")).unwrap() {
        for line in fs::read_to_string(entry.unwrap().path()).unwrap().lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            if row["event"] == "health_changed" && row["account"] == account.id {
                changes.push(row);
            }
        }
    }
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0]["from"]["kind"], "ready");
    assert_eq!(changes[1]["from"], changes[0]["to"]);
    assert!(
        changes
            .iter()
            .all(|row| row["to"]["reason"] == "quota" && row["to"]["scope"] == "account")
    );
    for (row, expected) in changes.iter().zip([reset, reset + 1]) {
        let until = chrono::DateTime::parse_from_rfc3339(row["until"].as_str().unwrap()).unwrap();
        assert_eq!(until.timestamp(), expected);
    }
    h.stop().await;
    upstream.abort();
}

#[tokio::test]
async fn r7_first_authenticated_settings_action_after_startup_is_durable() {
    // Each case constructs a new observer AFTER the existing account was saved.
    // No token/health notification or artificial account_changed seeding first.
    for (setting, expected) in [
        ("drain", SettingAction::DrainOn),
        ("allow_credits", SettingAction::CreditsOn),
    ] {
        let fake = ScriptedUpstream::start().await.unwrap();
        let h = Harness::new(&fake.url).await;
        let account = h.store.list().unwrap().remove(0);
        assert!(!account.drain && !account.allow_credits);
        assert_eq!(account.user_state, qb_core::accounts::UserState::Active);
        assert!(
            h.get("activity?days=7").await["events"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let response = h
            .client
            .patch(format!(
                "http://{}/admin/v1/accounts/{}",
                h.servers.admin_addr, account.id
            ))
            .bearer_auth(ADMIN)
            .json(&json!({(setting): true}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let changed: Value = response.json().await.unwrap();
        assert_eq!(changed[setting], true);
        let events = h
            .obs
            .dashboard()
            .activity_page(None, 200, 7)
            .unwrap()
            .events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ActivityKind::AccountChanged);
        assert_eq!(events[0].data.action, Some(expected));
        assert!(events[0].notable);
        assert_eq!(events[0].account.as_ref().unwrap().as_str(), account.id);
        assert!(fake.state.requests().is_empty());
        let before = serde_json::to_value(&events).unwrap();
        assert_eq!(h.get("activity?days=7").await["events"], before);
        h.obs.freeze_quota().unwrap();
        let reopened = Observability::new(&h.home, None).unwrap();
        let after = reopened
            .dashboard()
            .activity_page(None, 200, 7)
            .unwrap()
            .events;
        assert_eq!(serde_json::to_value(after).unwrap(), before);
        drop(reopened);
        h.stop().await;
    }
}

#[tokio::test]
async fn r7_http_authenticated_views_strip_private_label_and_preserve_upstream_body() {
    let fake = ScriptedUpstream::start().await.unwrap();
    let mut response =
        ScriptedResponse::json(StatusCode::OK, r#"{"output":[],"fixture":"unchanged"}"#);
    response
        .headers
        .insert("openai-model", "header-model".parse().unwrap());
    fake.state.push(response);
    let h = Harness::new(&fake.url).await;
    let mut events = h
        .client
        .get(format!("http://{}/admin/v1/events", h.servers.admin_addr))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .bytes_stream();
    let body = r#"{"model":"request-model","input":[],"sentinel":"unchanged"}"#;
    let response = h
        .client
        .post(format!("http://{}/v1/responses", h.servers.data_addr))
        .bearer_auth(CLIENT)
        .header("session-id", SID)
        .header("x-quota-baton-label", LABEL)
        .header("originator", "codex_cli_rs")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.bytes().await.unwrap();
    let forwarded = fake.state.requests();
    assert_eq!(forwarded.len(), 1);
    assert!(!forwarded[0].headers.contains_key("x-quota-baton-label"));
    assert_eq!(forwarded[0].body.as_ref(), body.as_bytes());
    let detail = h.settled(1).await;
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while let Some(Ok(chunk)) = events.next().await {
            bytes.extend_from_slice(&chunk);
            assert!(bytes.len() <= 32768);
            if String::from_utf8_lossy(&bytes).contains("event: sessions_changed") {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    })
    .await
    .unwrap();
    assert!(event.contains("sessions_changed"));
    assert!(!event.contains("original_short_id"));
    assert!(!event.contains(SID));
    drop(events);
    #[cfg(feature = "shape-capture")]
    {
        h.obs.flush().unwrap();
        for file in fs::read_dir(h.root.path().join("capture")).unwrap() {
            let wire = fs::read_to_string(file.unwrap().path()).unwrap();
            assert!(!wire.contains("x-quota-baton-label"));
            assert!(!wire.contains(LABEL));
        }
    }
    assert_eq!(detail["original_short_id"], "12345678");
    assert_eq!(detail["label"]["text"], LABEL);
    assert_eq!(detail["model"], "header-model");
    assert_eq!(detail["client"]["kind"], "codex_cli");
    let list = h.get("sessions?range=all").await;
    let wire = serde_json::to_string(&list).unwrap();
    assert!(!wire.contains("original_short_id"));
    assert!(!wire.contains(SID));
    assert!(wire.contains(LABEL));
    for path in [
        "sessions",
        "sessions/123456789abc",
        "activity",
        "usage-history",
    ] {
        let response = h
            .client
            .get(format!("http://{}/admin/v1/{path}", h.servers.admin_addr))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    for path in [
        "sessions?range=private-invalid",
        "activity?limit=999999",
        "usage-history?hours=49",
    ] {
        let response = h
            .client
            .get(format!("http://{}/admin/v1/{path}", h.servers.admin_addr))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!response.text().await.unwrap().contains("private-invalid"));
    }
    h.stop().await;
}

#[tokio::test]
async fn r7_websocket_socket_is_not_request_and_duplicate_labels_never_reach_fake() {
    let fake = WebSocketUpstream::start().await.unwrap();
    fake.state.set_echo(false);
    let mut headers = HeaderMap::new();
    headers.insert("openai-model", "ws-header-model".parse().unwrap());
    fake.state.set_success_headers(headers);
    // Establish the response ID before its terminal event. An unknown explicit
    // ID must not complete an unrelated turn in the production pump.
    fake.state.push_reply([
        Message::Text(
            r#"{"type":"response.created","response":{"id":"r7-response","status":"in_progress"}}"#
                .into(),
        ),
        Message::Text(
            r#"{"type":"response.completed","response":{"id":"r7-response","status":"completed"}}"#
                .into(),
        ),
    ]);
    let h = Harness::new(&fake.url).await;
    let mut request = format!("ws://{}/v1/responses", h.servers.data_addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {CLIENT}").parse().unwrap());
    request
        .headers_mut()
        .insert("session-id", SID.parse().unwrap());
    request
        .headers_mut()
        .append("x-quota-baton-label", LABEL.parse().unwrap());
    request
        .headers_mut()
        .append("x-quota-baton-label", "qb1:%%%".parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let idle = h.get(&format!("sessions/{}", Harness::id())).await;
    assert_eq!(idle["requests_today"], 0);
    assert_eq!(idle["inflight"]["ws_open"], 1);
    assert_eq!(idle["inflight"]["ws_response_in_flight"], false);
    assert!(idle["label"].is_null());
    socket
        .send(Message::Text(
            r#"{"type":"response.create","model":"body-model","input":[]}"#.into(),
        ))
        .await
        .unwrap();
    let created = tokio::time::timeout(Duration::from_secs(5), async {
        let mut created = false;
        loop {
            if let Some(Ok(Message::Text(text))) = socket.next().await {
                let event: Value = serde_json::from_str(&text).unwrap();
                if event["type"] == "response.created" {
                    assert_eq!(event["response"]["id"], "r7-response");
                    created = true;
                }
                if event["type"] == "response.completed" {
                    assert_eq!(event["response"]["id"], "r7-response");
                    break created;
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(created);
    let completed = h.settled(1).await;
    assert_eq!(completed["model"], "ws-header-model");
    assert_eq!(completed["inflight"]["ws_open"], 1);
    assert_eq!(completed["recent_requests"].as_array().unwrap().len(), 1);
    assert_eq!(completed["recent_requests"][0]["outcome"], "ok");
    let upstream = fake.state.requests();
    assert_eq!(upstream.len(), 1);
    assert!(!upstream[0].headers.contains_key("x-quota-baton-label"));
    socket.close(None).await.unwrap();
    h.stop().await;
}

#[tokio::test]
async fn r7_atomic_archive_restart_keeps_usage_and_activity_but_not_private_memory_evidence() {
    let fake = ScriptedUpstream::start().await.unwrap();
    fake.state
        .push(ScriptedResponse::json(StatusCode::OK, r#"{"output":[]}"#));
    let h = Harness::new(&fake.url).await;
    let response = h
        .client
        .post(format!("http://{}/v1/responses", h.servers.data_addr))
        .bearer_auth(CLIENT)
        .header("session-id", SID)
        .header("x-quota-baton-label", LABEL)
        .json(&json!({"input":[]}))
        .send()
        .await
        .unwrap();
    let _ = response.bytes().await.unwrap();
    h.settled(1).await;
    let account = h.store.list().unwrap().remove(0);
    let now = chrono::Utc::now();
    h.obs.dashboard().usage(UsageObservation {
        account: account.id.clone().try_into().unwrap(),
        observed_at: now,
        window: Some(WindowObservation {
            name: "43200m".to_owned().try_into().unwrap(),
            scope: WindowScope::Account,
            used_pct: Some(42.0),
            reset_at: Some(now + chrono::Duration::hours(1)),
        }),
        credits: None,
    });
    h.obs.record_refresh(&account.id, "manual", "success", None);
    // This fixture deliberately uses linked credentials: selection emits one
    // successful reload in addition to the explicit successful refresh above.
    let expected_events = h
        .obs
        .dashboard()
        .activity_page(None, 200, 7)
        .unwrap()
        .events;
    assert_eq!(expected_events.len(), 2);
    assert_eq!(
        expected_events
            .iter()
            .filter(|event| event.kind == ActivityKind::LinkedReload)
            .count(),
        1
    );
    assert_eq!(
        expected_events
            .iter()
            .filter(|event| event.kind == ActivityKind::TokenRefresh)
            .count(),
        1
    );
    assert!(expected_events.iter().all(|event| {
        event.provider == account.provider
            && event
                .account
                .as_ref()
                .is_some_and(|id| id.as_str() == account.id)
            && event.data.outcome == Some(qb_core::dashboard::RequestOutcome::Ok)
            && !event.notable
    }));
    let expected_events = serde_json::to_value(expected_events).unwrap();
    h.state.bindings.flush().unwrap();
    h.obs.freeze_quota().unwrap();
    for name in ["activity.jsonl", "usage-history.json"] {
        let bytes = fs::read(h.home.join("state").join(name)).unwrap();
        let wire = String::from_utf8(bytes).unwrap();
        for secret in [
            LABEL,
            SID,
            "12345678",
            "original_short_id",
            "x-quota-baton-label",
        ] {
            assert!(!wire.contains(secret));
        }
    }
    // Same synthetic home, old observer frozen. No real profile/home access.
    let reopened = Arc::new(Observability::new(&h.home, None).unwrap());
    let state = RelayState::new(
        h.store.clone(),
        &h.config,
        reopened.clone(),
        Arc::new(DebugPins::default()),
    )
    .unwrap();
    let id: SessionId = Harness::id().try_into().unwrap();
    let detail = reopened.dashboard().detail(&id).unwrap();
    assert!(detail.original_short_id.is_none());
    assert!(detail.session.label.is_none());
    assert!(!detail.session.counters_complete);
    assert_eq!(
        serde_json::to_value(
            reopened
                .dashboard()
                .activity_page(None, 200, 7)
                .unwrap()
                .events
        )
        .unwrap(),
        expected_events
    );
    assert_eq!(
        reopened.dashboard().usage_page(48).unwrap().accounts[0].windows[0]
            .name
            .as_str(),
        "43200m"
    );
    drop(state);
    drop(reopened);
    assert!(h.root.path().exists());
    h.stop().await;
}
