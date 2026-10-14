use axum::{
    body::{Body, Bytes, to_bytes},
    extract::Request,
    http::StatusCode,
    response::Response,
};
use futures_util::StreamExt;
use qb_core::{
    accounts::{AccountStore, import},
    config::Config,
    debug_pin::DebugPins,
    guard::{MemoryGuard, MemoryGuardTransition, private_bytes},
    http,
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use serde_json::json;
use std::{
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EarlyUpstream {
    url: String,
    bodies: Arc<Mutex<Vec<Bytes>>>,
    first_chunk: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl EarlyUpstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let first_chunk = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let state = (bodies.clone(), first_chunk.clone(), calls.clone());
        let task = tokio::spawn(async move {
            let app = axum::Router::new().fallback(move |request: Request| {
                let (bodies, first_chunk, calls) = state.clone();
                async move {
                    let call = calls.fetch_add(1, Ordering::AcqRel);
                    let (_, body) = request.into_parts();
                    if call == 0 {
                        let mut stream = body.into_data_stream();
                        let _ = stream.next().await;
                        first_chunk.notify_one();
                        tokio::spawn(async move { while stream.next().await.is_some() {} });
                        return Response::builder()
                            .status(StatusCode::TOO_MANY_REQUESTS)
                            .body(Body::from("retry"))
                            .unwrap();
                    }
                    let body = to_bytes(body, usize::MAX).await.unwrap();
                    bodies.lock().unwrap().push(body);
                    Response::new(Body::from("ok"))
                }
            });
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url,
            bodies,
            first_chunk,
            calls,
            task,
        }
    }
}

impl Drop for EarlyUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Harness {
    _root: tempfile::TempDir,
    upstream: EarlyUpstream,
    servers: http::RunningServers,
    client: reqwest::Client,
}

impl Harness {
    async fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/tests");
        fs::create_dir_all(&root).unwrap();
        let temp = tempfile::tempdir_in(root).unwrap();
        let home = temp.path().join("home");
        let cpa = temp.path().join("cpa");
        fs::create_dir_all(&cpa).unwrap();
        for (n, email) in [
            ("one", "one@example.invalid"),
            ("two", "two@example.invalid"),
        ] {
            fs::write(cpa.join(format!("{n}.json")), serde_json::to_vec(&json!({
                "type":"codex", "email":email, "account_id":format!("synthetic-{n}"),
                "account_uuid":format!("synthetic-uuid-{n}"), "organization_uuid":"synthetic-org",
                "access_token":format!("synthetic-access-{n}"), "refresh_token":format!("synthetic-refresh-{n}"),
                "id_token":format!("synthetic-id-{n}"), "last_refresh":"2026-01-01T00:00:00Z", "expired":"2030-01-01T00:00:00Z"
            })).unwrap()).unwrap();
        }
        let store = Arc::new(AccountStore::acquire(&home).unwrap());
        import::run(
            &store,
            &cpa,
            &[
                "codex:one@example.invalid".into(),
                "codex:two@example.invalid".into(),
            ],
            false,
        )
        .unwrap();
        let upstream = EarlyUpstream::start().await;
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.codex_base = upstream.url.clone();
        config.guard.retain_max_per_request_mib = 64;
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let state =
            Arc::new(RelayState::new(store, &config, obs, Arc::new(DebugPins::default())).unwrap());
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-client".into(),
                admin_token: "synthetic-admin".into(),
            },
            relay::router(state),
            axum::Router::new(),
        )
        .await
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        Self {
            _root: temp,
            upstream,
            servers,
            client,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/v1/responses", self.servers.data_addr)
    }
}

#[tokio::test]
async fn early_response_drains_and_retry_is_byte_invariant() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new().await;
    let expected = Bytes::from(
        (0..(2 * 1024 * 1024 + 137))
            .map(|n| (n % 251) as u8)
            .collect::<Vec<_>>(),
    );
    let chunks: Vec<_> = expected
        .chunks(16 * 1024)
        .map(|chunk| Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)))
        .collect();
    let response = h
        .client
        .post(h.url())
        .bearer_auth("synthetic-client")
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
            chunks.into_iter(),
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), "ok");
    assert!(h.upstream.calls.load(Ordering::Acquire) >= 2);
    assert_eq!(
        h.upstream.bodies.lock().unwrap().last().unwrap().as_ref(),
        expected.as_ref()
    );
}

#[tokio::test]
async fn upstream_receives_body_before_client_upload_finishes() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new().await;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
    let body = reqwest::Body::wrap_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let request = tokio::spawn(
        h.client
            .post(h.url())
            .bearer_auth("synthetic-client")
            .body(body)
            .send(),
    );
    let first_seen = h.upstream.first_chunk.notified();
    tx.send(Ok(Bytes::from_static(b"first-chunk")))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), first_seen)
        .await
        .unwrap();
    assert!(!request.is_finished());
    drop(tx);
    let response = request.await.unwrap().unwrap();
    assert!(response.status().is_success() || response.status() == StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn memory_guard_test_hook_has_hysteresis() {
    let guard = MemoryGuard::new(50 * 1024 * 1024);
    assert_eq!(
        guard.force_sample(50 * 1024 * 1024 + 1).transition,
        MemoryGuardTransition::On
    );
    assert!(guard.is_guarded());
    assert_eq!(
        guard.force_sample(45 * 1024 * 1024 - 1).transition,
        MemoryGuardTransition::Off
    );
    assert!(!guard.is_guarded());
}

#[test]
#[ignore = "A2 acceptance harness; run explicitly on the implementer's machine"]
fn a2_benchmark_200_requests_per_shape_and_private_memory() {
    let before = private_bytes();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let h = Harness::new().await;
        let shapes = [
            ("10kb-plain", vec![b'p'; 10 * 1024], false),
            ("30mb-plain", vec![b'p'; 30 * 1024 * 1024], false),
            (
                "30mb-zstd",
                zstd::stream::encode_all(&vec![b'z'; 30 * 1024 * 1024][..], 1).unwrap(),
                true,
            ),
        ];
        for (name, body, zstd) in shapes {
            let mut samples = Vec::with_capacity(200);
            for _ in 0..200 {
                let start = Instant::now();
                let mut request = h.client.post(h.url()).bearer_auth("synthetic-client");
                if zstd {
                    request = request.header("content-encoding", "zstd");
                }
                let response = request.body(body.clone()).send().await.unwrap();
                samples.push(start.elapsed());
                response.bytes().await.unwrap();
            }
            samples.sort_unstable();
            println!("A2 {name}: p50={:?} p95={:?}", samples[99], samples[189]);
        }
        let body = vec![b'm'; 50 * 1024 * 1024];
        let baseline = private_bytes();
        let response = h
            .client
            .post(h.url())
            .bearer_auth("synthetic-client")
            .body(body)
            .send()
            .await
            .unwrap();
        response.bytes().await.unwrap();
        let peak_sample = private_bytes();
        println!(
            "50MB plain private bytes: baseline={baseline} after={peak_sample} delta={}",
            peak_sample.saturating_sub(baseline)
        );
    });
    println!("A2 private bytes baseline={before} peak measurement requires process sampler");
}
