//! Deterministic real-relay regression for drain racing an unread HTTP body.
//! No native process, production profile, OS-specific fixture, or testkit hook.
use super::*;
use crate::{
    accounts::{Identity, Tokens},
    config::LinuxSecretBackend,
};
use axum::body::{Body, to_bytes};
use futures_util::{FutureExt, StreamExt, stream};
use serde_json::{Value, json};
use std::{io::Write, panic::AssertUnwindSafe, path::Path};
use tokio::sync::oneshot;

const LIMIT: Duration = Duration::from_secs(10);
const CLIENT_KEY: &str = "r20-synthetic-client-key";
const PAYLOAD: &[u8] = br#"{"model":"fixture","input":[]}"#;

struct Captured {
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

struct Successor {
    port: u16,
    url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl Successor {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port >= 1024 && !matches!(port, 10100 | 10101 | 10200 | 10201));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let received = captured.clone();
        let app = Router::new().route(
            "/v1/responses",
            axum::routing::post(move |request: Request| {
                let received = received.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let body = to_bytes(body, 1024).await.unwrap();
                    received.lock().unwrap().push(Captured {
                        method: parts.method,
                        path: parts.uri.to_string(),
                        headers: parts.headers,
                        body,
                    });
                    Json(json!({"fake":true}))
                }
            }),
        );
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        Self {
            port,
            url: format!("http://127.0.0.1:{port}"),
            captured,
            stop: Some(stop),
            task,
        }
    }
    async fn close(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(LIMIT, &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
impl Drop for Successor {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

struct Fixture {
    state: Arc<RelayState>,
    obs: Arc<Observability>,
    successor: Successor,
    // Drop relay/store, observer and listener owners before removing their home.
    _home: tempfile::TempDir,
}
impl Fixture {
    async fn new(seed_account: bool, retain_mib: u64) -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/tests/r20-handover-http");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let successor = Successor::new().await;
        let mut config = Config::default();
        config.secrets.linux_backend = LinuxSecretBackend::File0600;
        config.server.data_port = successor.port;
        config.server.admin_port = 0;
        config.guard.retain_max_per_request_mib = retain_mib;
        config.upstream.codex_base = successor.url.clone();
        config.upstream.anthropic_base = successor.url.clone();
        config.upstream.codex_usage_base = successor.url.clone();
        config.upstream.claude_usage_base = successor.url.clone();
        config.upstream.codex_token_endpoint = Some(format!("{}/oauth/token", successor.url));
        config.upstream.claude_token_endpoint = Some(format!("{}/oauth/token", successor.url));
        // NamedTempFile creates the private config without an inline Unix mode
        // branch; File0600 also prevents Secret Service/DPAPI fixture coupling.
        let mut config_file = tempfile::NamedTempFile::new_in(home.path()).unwrap();
        config_file
            .write_all(toml::to_string(&config).unwrap().as_bytes())
            .unwrap();
        config_file
            .persist(home.path().join("config.toml"))
            .unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        if seed_account {
            let identity = Identity::Codex {
                account_id: "r20-synthetic-account".into(),
                user_id: None,
                email: "r20@example.invalid".into(),
            };
            let account: Account = serde_json::from_value(json!({
                "schema":1,"id":identity.id(),"provider":"codex","label":"R20 synthetic",
                "email":"r20@example.invalid","identity":identity,"source":{"kind":"oauth"},
                "user_state":"active","health":{"kind":"ready"},"generation":0,
                "token_meta":{"expires_at":"2099-01-01T00:00:00Z","refresh_uncertain":false}
            }))
            .unwrap();
            store
                .save(
                    &account,
                    &Tokens {
                        access_token: "r20-synthetic-access".into(),
                        refresh_token: "r20-synthetic-refresh".into(),
                        id_token: None,
                    },
                    None,
                )
                .unwrap();
        }
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let state = Arc::new(
            RelayState::new(store, &config, obs.clone(), Arc::new(DebugPins::default())).unwrap(),
        );
        Self {
            state,
            obs,
            successor,
            _home: home,
        }
    }
    fn request(&self, body: Body) -> Request {
        Request::builder()
            .method(Method::POST)
            .uri("/v1/responses")
            .header("host", format!("127.0.0.1:{}", self.successor.port))
            .header("authorization", format!("Bearer {CLIENT_KEY}"))
            .header("session-id", "r20-late-upload")
            .header("content-type", "application/json")
            .header("content-length", PAYLOAD.len())
            .header("user-agent", "r20-synthetic-client")
            .header(
                crate::routing::PROJECT_HEADER,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .body(body)
            .unwrap()
    }
    async fn close(&mut self) {
        tokio::time::timeout(LIMIT, self.state.shutdown_refreshes())
            .await
            .unwrap()
            .unwrap();
        self.obs.flush().unwrap();
        self.successor.close().await;
    }
}

#[tokio::test]
async fn drain_after_initial_check_forwards_held_multichunk_body_without_retention() {
    // The zero case proves forwarding cannot depend on a retained replay copy;
    // both variants drive the same actual relay and actual drain implementation.
    for retain_mib in [0, 64] {
        let mut fixture = Fixture::new(true, retain_mib).await;
        let result = AssertUnwindSafe(tokio::time::timeout(LIMIT, async {
            assert!(!fixture.state.is_draining());
            assert_eq!(fixture.state.store.list().unwrap().len(), 1);
            let (release, held) = oneshot::channel();
            let (entered, entered_rx) = oneshot::channel();
            let first = stream::once(async move {
                let _ = entered.send(());
                held.await.unwrap();
                Ok::<_, std::io::Error>(Bytes::from_static(&PAYLOAD[..8]))
            });
            let rest = stream::iter([
                Ok::<_, std::io::Error>(Bytes::from_static(&PAYLOAD[8..19])),
                Ok::<_, std::io::Error>(Bytes::from_static(&PAYLOAD[19..])),
            ]);
            let request = fixture.request(Body::from_stream(first.chain(rest)));
            let original_headers = request.headers().clone();
            let mut response = Box::pin(relay(State(fixture.state.clone()), request));
            // No spawned request driver: the first poll crosses the initial
            // drain check and suspends on upload's still-withheld prefix.
            assert!(futures_util::poll!(&mut response).is_pending());
            assert!(!fixture.state.is_draining());
            assert_eq!(fixture.state.inflight_requests.load(Ordering::Acquire), 1);
            entered_rx.await.unwrap();
            fixture.state.begin_drain().await.unwrap();
            fixture.state.release_lease_after_drain();
            assert!(fixture.state.is_draining());
            assert!(fixture.state.store.writes_stopped());
            release.send(()).unwrap();
            let response = response.await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"fake":true})
            );
            let captured = fixture.successor.captured.lock().unwrap();
            assert_eq!(
                captured.len(),
                1,
                "original request must be forwarded exactly once"
            );
            let received = &captured[0];
            assert_eq!(received.method, Method::POST);
            assert_eq!(received.path, "/v1/responses");
            assert_eq!(received.body.as_ref(), PAYLOAD);
            for (name, value) in &original_headers {
                assert_eq!(
                    received.headers.get(name),
                    Some(value),
                    "original client header changed: {name}"
                );
            }
        }))
        .catch_unwind()
        .await;
        fixture.close().await;
        match result {
            Ok(result) => result.expect("held-upload handover exceeded deadline"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}

#[tokio::test]
async fn undrained_empty_pool_keeps_genuine_no_account_400() {
    let mut fixture = Fixture::new(false, 0).await;
    let result = AssertUnwindSafe(tokio::time::timeout(LIMIT, async {
        assert!(!fixture.state.is_draining());
        assert!(fixture.state.store.list().unwrap().is_empty());
        let mut request = fixture.request(Body::from(PAYLOAD));
        // This control checks no-account behavior, not an unregistered project's
        // independent admission conflict. The held-upload case keeps the hint.
        assert!(request.headers_mut().remove(crate::routing::PROJECT_HEADER).is_some());
        let response = relay(State(fixture.state.clone()), request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert_eq!(value["error"]["message"],
            "quota-baton: no usable codex account — run quota-baton accounts login codex --no-browser");
        assert!(fixture.successor.captured.lock().unwrap().is_empty());
    })).catch_unwind().await;
    fixture.close().await;
    match result {
        Ok(result) => result.expect("empty-pool control exceeded deadline"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
