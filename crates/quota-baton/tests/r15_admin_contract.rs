//! One real /state response through the real tray decoder. No spawned client.
//! Source-only candidate: account transport integration may intentionally be red.
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::OriginalUri,
    http::{HeaderMap, StatusCode},
    middleware,
    response::Response,
};
use chrono::{Duration as ChronoDuration, Utc};
use qb_core::{
    accounts::{AccountStore, Provider, import},
    bindings::{MoveGuardEvent, MoveGuardPair},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::{CacheUsage, Observability},
    relay::RelayState,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    net::{Ipv4Addr, TcpListener},
    sync::{Arc, Mutex},
    time::Duration,
};

const ADMIN: &str = "r15-synthetic-admin";
const BODY_CAP: usize = 8 * 1024 * 1024;
type CapturedBodies = Arc<Mutex<Vec<Vec<u8>>>>;

struct Harness {
    servers: RunningServers,
    relay: Arc<RelayState>,
    obs: Arc<Observability>,
    store: Arc<AccountStore>,
    upstream_trap: TcpListener,
    root: tempfile::TempDir,
    captured: CapturedBodies,
}

impl Harness {
    async fn new() -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r15-admin-contract-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        let home = root.path().join("home");
        let external = root.path().join("synthetic-cpa");
        fs::create_dir(&external).unwrap();
        for suffix in ["a", "b"] {
            fs::write(
                external.join(format!("fixture-{suffix}.json")),
                serde_json::to_vec(&json!({
                    "type":"codex", "email":format!("fixture-{suffix}@example.invalid"),
                    "account_id":format!("r15-fixture-{suffix}"),
                    "access_token":"r15-synthetic-access", "refresh_token":"r15-synthetic-refresh",
                    "expired":"2099-01-01T00:00:00Z", "last_refresh":"2026-10-24T00:00:00Z"
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let store = Arc::new(AccountStore::acquire(&home).unwrap());
        import::run(
            &store,
            &external,
            &[
                "codex:fixture-a@example.invalid".into(),
                "codex:fixture-b@example.invalid".into(),
            ],
            false,
        )
        .unwrap();
        let accounts = store.list().unwrap();
        assert_eq!(accounts.len(), 2, "fixture import must be nonempty");
        let ids: Vec<_> = accounts.iter().map(|a| a.id.clone()).collect();

        // No upstream service is started. An accidental connect is observable.
        let upstream_trap = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        upstream_trap.set_nonblocking(true).unwrap();
        let upstream_port = upstream_trap.local_addr().unwrap().port();
        assert!(!matches!(upstream_port, 10100 | 10101));
        let upstream = format!("http://127.0.0.1:{upstream_port}");
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.codex_base = upstream.clone();
        config.upstream.anthropic_base = upstream.clone();
        config.upstream.codex_usage_base = upstream.clone();
        config.upstream.claude_usage_base = upstream.clone();
        #[cfg(debug_assertions)]
        {
            config.upstream.codex_token_endpoint = Some(upstream.clone());
            config.upstream.claude_token_endpoint = Some(upstream);
        }
        config.dashboard.resolve_claude_project = false;
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let relay = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap(),
        );
        // Do not start quota/profile/refresh/dashboard background schedulers.
        // Existing public numeric observers populate the actual account view.
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("x-codex-primary-used-percent", "12.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-after-seconds", "3600"),
            ("x-codex-secondary-used-percent", "25"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-after-seconds", "86400"),
        ] {
            headers.insert(key, value.parse().unwrap());
        }
        for id in &ids {
            obs.observe_headers(id, Provider::Codex, &headers);
        }

        // Populate MoveReread through the existing accounting observer API.
        // This records diagnostic usage, not a placement/selection assertion.
        obs.configure_move_notice(1);
        let at = Utc::now();
        let model = Some("r15-synthetic-model");
        let key = "codex:r15-synthetic-session";
        let baseline = CacheUsage {
            input_tokens: Some(10),
            cached_tokens: Some(0),
            cache_creation_input_tokens: None,
        };
        let first = obs
            .accounting_begin(Provider::Codex, key, &ids[0], model, at)
            .unwrap();
        obs.accounting_end(first, true, Some(&baseline), at)
            .unwrap();
        obs.committed_move(Provider::Codex, key, &ids[1], model, at)
            .unwrap();
        let next = obs
            .accounting_begin(Provider::Codex, key, &ids[1], model, at)
            .unwrap();
        let reread = CacheUsage {
            input_tokens: Some(100),
            cached_tokens: Some(0),
            cache_creation_input_tokens: None,
        };
        obs.accounting_end(next, true, Some(&reread), at).unwrap();
        // Existing public diagnostic event type; no new unchecked constructor.
        obs.record_move_guard(&MoveGuardEvent {
            proposed_reason: "forced".into(),
            move_list: vec![MoveGuardPair {
                from: ids[0].clone(),
                to: ids[1].clone(),
            }],
            key: key.into(),
            from: ids[0].clone(),
            to: ids[1].clone(),
            reason: "global_trip".into(),
            until: at + ChronoDuration::minutes(4),
            newly_tripped: true,
            forced: true,
        });

        let captured: CapturedBodies = Arc::new(Mutex::new(Vec::new()));
        let capture = captured.clone();
        let admin_routes = qb_core::admin::runtime_routes(relay.clone(), 0).route_layer(
            middleware::map_response(move |OriginalUri(uri): OriginalUri, response: Response| {
                let capture = capture.clone();
                async move {
                    if uri.path() != "/admin/v1/state" {
                        return response;
                    }
                    assert_eq!(response.status(), StatusCode::OK);
                    let (parts, body) = response.into_parts();
                    let bytes = to_bytes(body, BODY_CAP)
                        .await
                        .expect("bounded state response");
                    capture.lock().unwrap().push(bytes.to_vec());
                    // Preserve the original parts and EXACT bytes for this client.
                    // Never reconstruct the server response via serde_json here.
                    Response::from_parts(parts, Body::from(bytes))
                }
            }),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "r15-synthetic-client".into(),
                admin_token: ADMIN.into(),
            },
            Router::new(),
            admin_routes,
        )
        .await
        .unwrap();
        for port in [servers.admin_addr.port(), servers.data_addr.port()] {
            assert_ne!(port, 0);
            assert!(!matches!(port, 10100 | 10101));
        }
        Self {
            servers,
            relay,
            obs,
            store,
            upstream_trap,
            root,
            captured,
        }
    }

    async fn close(mut self) {
        self.servers.shutdown();
        tokio::time::timeout(Duration::from_secs(5), async {
            (&mut self.servers.data_task).await.unwrap().unwrap();
            (&mut self.servers.admin_task).await.unwrap().unwrap();
        })
        .await
        .expect("owned HTTP servers must terminate");
        // Outside a Tokio context, Observability::drop joins its writer before
        // the synthetic TempDir is deleted (important for Windows file handles).
        std::thread::spawn(move || {
            drop(self.servers);
            drop(self.relay);
            drop(self.obs);
            drop(self.store);
            drop(self.upstream_trap);
            drop(self.captured);
            drop(self.root);
        })
        .join()
        .expect("owned fixture cleanup");
    }
}

// Exact key sets at EVERY object, lengths/order at arrays, exact scalar values.
// Diagnostics name paths, without dumping response bodies or fixture token text.
fn assert_same_tree(expected: &Value, actual: &Value, path: &str) {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            assert_eq!(
                left.keys().collect::<BTreeSet<_>>(),
                right.keys().collect::<BTreeSet<_>>(),
                "object keys differ at {path}"
            );
            for (key, value) in left {
                assert_same_tree(value, &right[key], &format!("{path}/{key}"));
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            assert_eq!(left.len(), right.len(), "array length differs at {path}");
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                assert_same_tree(left, right, &format!("{path}/{index}"));
            }
        }
        _ => assert!(expected == actual, "JSON type/scalar differs at {path}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_state_same_response_survives_real_tray_transport_without_any_key_loss() {
    let h = Harness::new().await;
    let client = qb_tray::client::AdminClient::new(h.servers.admin_addr.port(), ADMIN).unwrap();
    // This is the ONLY GET. Even a decode failure is examined after cleanup.
    let decoded = client.snapshot().await;
    let bodies = std::mem::take(&mut *h.captured.lock().unwrap());
    let upstream_untouched = match h.upstream_trap.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
        Ok((stream, _)) => {
            drop(stream);
            false
        }
        Err(_) => false,
    };
    drop(client);
    h.close().await;
    assert!(
        upstream_untouched,
        "state fixture attempted an upstream connection"
    );
    assert_eq!(
        bodies.len(),
        1,
        "capture exactly the response consumed by snapshot()"
    );
    let raw: Value = serde_json::from_slice(&bodies[0]).unwrap();
    let accounts = raw["accounts"].as_array().expect("real account array");
    assert_eq!(accounts.len(), 2);
    for account in accounts {
        assert_eq!(account["source"]["kind"], "cpa");
        assert_eq!(account["source"]["mode"], "linked");
        assert!(account["source"]["dir"].is_string());
        assert!(account["source"]["match"].is_object());
        assert!(account["usage_forecast"].is_object());
        assert!(
            !account["usage_forecast"]["windows"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(!account["windows"].as_array().unwrap().is_empty());
    }
    for provider in ["claude", "codex"] {
        assert!(raw["pools"][provider]["eligible"].is_u64());
        assert!(raw["pools"][provider]["unavailable"].is_u64());
        assert!(raw["pools"][provider]["cooling"].is_object());
    }
    let warnings = raw["warnings"].as_array().unwrap();
    assert_eq!(
        warnings.len(),
        2,
        "nonempty typed warning producer fixtures"
    );
    let reread = warnings
        .iter()
        .find(|w| w["kind"] == "move_reread")
        .unwrap();
    assert_eq!(reread["reread_tokens"], 90);
    assert_eq!(reread["known_count"], 1);
    assert_eq!(reread["unknown_count"], 0);
    let breaker = warnings
        .iter()
        .find(|w| w["kind"] == "move_breaker")
        .unwrap();
    for field in [
        "kind",
        "observed_at",
        "move_list",
        "until",
        "session",
        "from",
        "to",
        "reason",
        "forced",
    ] {
        assert!(
            breaker.get(field).is_some(),
            "missing actual warning field {field}"
        );
    }
    assert_eq!(breaker["move_list"].as_array().unwrap().len(), 1);
    let session = breaker["session"].as_str().unwrap();
    assert_eq!(session.len(), 12);
    assert!(session.bytes().all(|b| b.is_ascii_hexdigit()));
    let snapshot = decoded.expect("actual tray decoder must accept the real response");
    let serialized = serde_json::to_vec(&snapshot).unwrap();
    let roundtrip: Value = serde_json::from_slice(&serialized).unwrap();
    assert_same_tree(&raw, &roundtrip, "$");
    assert!(raw == roundtrip, "whole same-response JSON equality");
}
