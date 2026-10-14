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
    state: Arc<RelayState>,
    observability: Arc<Observability>,
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
        let state = Arc::new(
            RelayState::new(store, &config, obs.clone(), Arc::new(DebugPins::default())).unwrap(),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-client".into(),
                admin_token: "synthetic-admin".into(),
            },
            relay::router(state.clone()),
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
            state,
            observability: obs,
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
async fn m7b_guard_503_retry_after_new_requests_existing_stream_continues() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new().await;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
    let body = reqwest::Body::wrap_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let existing = tokio::spawn(
        h.client
            .post(h.url())
            .bearer_auth("synthetic-client")
            .body(body)
            .send(),
    );
    tx.send(Ok(Bytes::from_static(b"existing"))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), h.upstream.first_chunk.notified())
        .await
        .unwrap();
    assert!(h.state.force_memory_sample_for_test(u64::MAX).guarded);
    let rejected = h
        .client
        .post(h.url())
        .bearer_auth("synthetic-client")
        .body("new")
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rejected.headers()["retry-after"], "5");
    drop(tx);
    let existing = existing.await.unwrap().unwrap();
    assert_ne!(existing.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn m7b_guard_ledger_events() {
    let _serial = SERIAL.lock().await;
    let h = Harness::new().await;
    assert_eq!(
        h.state.force_memory_sample_for_test(u64::MAX).transition,
        MemoryGuardTransition::On
    );
    assert_eq!(
        h.state.force_memory_sample_for_test(0).transition,
        MemoryGuardTransition::Off
    );
    h.observability.flush().unwrap();
    let mut ledger = Vec::new();
    for entry in fs::read_dir(h._root.path().join("home").join("ledger")).unwrap() {
        let path = entry.unwrap().path();
        ledger.extend(
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()),
        );
    }
    assert!(ledger.iter().any(|row| row["event"] == "guard_on"));
    assert!(ledger.iter().any(|row| row["event"] == "guard_off"));
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

struct A2Upstream {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl A2Upstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let app = axum::Router::new().fallback(|request: Request| async move {
                let (_, body) = request.into_parts();
                let mut stream = body.into_data_stream();
                let _ = stream.next().await;
                tokio::spawn(async move { while stream.next().await.is_some() {} });
                Response::new(Body::from("ok"))
            });
            let _ = axum::serve(listener, app).await;
        });
        Self { url, task }
    }
}

impl Drop for A2Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct A2Reader {
    remaining: usize,
}

impl std::io::Read for A2Reader {
    fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
        let count = self.remaining.min(target.len());
        target[..count].fill(b'z');
        self.remaining -= count;
        Ok(count)
    }
}

#[derive(Clone, Copy)]
enum A2Shape {
    Plain(usize),
    Zstd(usize),
}

fn a2_body(shape: A2Shape, paced: bool) -> (reqwest::Body, bool) {
    const CHUNK: usize = 64 * 1024;
    match shape {
        A2Shape::Plain(size) => {
            let stream = futures_util::stream::unfold(0usize, move |offset| async move {
                if offset >= size {
                    return None;
                }
                if paced {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                let count = (size - offset).min(CHUNK);
                Some((
                    Ok::<_, std::io::Error>(Bytes::from(vec![b'p'; count])),
                    offset + count,
                ))
            });
            (reqwest::Body::wrap_stream(stream), false)
        }
        A2Shape::Zstd(size) => {
            let encoder =
                zstd::stream::read::Encoder::new(A2Reader { remaining: size }, 1).unwrap();
            let stream = futures_util::stream::unfold(encoder, |mut encoder| async move {
                let mut chunk = vec![0; CHUNK];
                let count = std::io::Read::read(&mut encoder, &mut chunk).unwrap();
                if count == 0 {
                    None
                } else {
                    chunk.truncate(count);
                    Some((Ok::<_, std::io::Error>(Bytes::from(chunk)), encoder))
                }
            });
            (reqwest::Body::wrap_stream(stream), true)
        }
    }
}

struct A2Harness {
    _root: tempfile::TempDir,
    upstream_url: String,
    servers: http::RunningServers,
    client: reqwest::Client,
}

impl A2Harness {
    fn url(&self) -> String {
        format!("http://{}/v1/responses", self.servers.data_addr)
    }
}

async fn a2_harness() -> (A2Upstream, A2Harness) {
    let upstream = A2Upstream::start().await;
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
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    config.upstream.codex_base = upstream.url.clone();
    let obs = Arc::new(Observability::new(&home, None).unwrap());
    let state = Arc::new(
        RelayState::new(store, &config, obs.clone(), Arc::new(DebugPins::default())).unwrap(),
    );
    let servers = http::spawn_with_routes(
        &config,
        LocalSecrets {
            client_key: "synthetic-client".into(),
            admin_token: "synthetic-admin".into(),
        },
        relay::router(state.clone()),
        axum::Router::new(),
    )
    .await
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let harness = A2Harness {
        _root: temp,
        upstream_url: upstream.url.clone(),
        servers,
        client,
    };
    (upstream, harness)
}

#[test]
#[ignore = "A2 acceptance harness; run explicitly on the implementer's machine"]
fn a2_benchmark_200_requests_per_shape_and_private_memory() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (_upstream, h) = a2_harness().await;
        let shapes = [
            ("10kb-plain", A2Shape::Plain(10 * 1024)),
            ("30mb-plain", A2Shape::Plain(30 * 1024 * 1024)),
            ("30mb-zstd", A2Shape::Zstd(30 * 1024 * 1024)),
        ];
        for (name, shape) in shapes {
            let mut added = Vec::with_capacity(200);
            for _ in 0..200 {
                let (direct_body, compressed) = a2_body(shape, false);
                let direct_start = Instant::now();
                let mut direct_request = h.client.post(&h.upstream_url).body(direct_body);
                if compressed {
                    direct_request = direct_request.header("content-encoding", "zstd");
                }
                let direct = direct_request.send().await.unwrap();
                let direct_ttfb = direct_start.elapsed();
                direct.bytes().await.unwrap();

                let (proxy_body, _) = a2_body(shape, false);
                let mut request = h
                    .client
                    .post(h.url())
                    .bearer_auth("synthetic-client")
                    .body(proxy_body);
                if compressed {
                    request = request.header("content-encoding", "zstd");
                }
                let proxy_start = Instant::now();
                let proxy = request.send().await.unwrap();
                added.push(proxy_start.elapsed().saturating_sub(direct_ttfb));
                proxy.bytes().await.unwrap();
            }
            added.sort_unstable();
            println!(
                "A2 {name}: added TTFB p50={:?} p95={:?}",
                added[99], added[189]
            );
        }

        let baseline = private_bytes();
        let (body, _) = a2_body(A2Shape::Plain(50 * 1024 * 1024), true);
        let request = h
            .client
            .post(h.url())
            .bearer_auth("synthetic-client")
            .body(body)
            .send();
        tokio::pin!(request);
        let mut peak = baseline;
        let mut samples = 0usize;
        let mut ticker = tokio::time::interval(Duration::from_millis(5));
        let response = loop {
            tokio::select! {
                result = &mut request => break result.unwrap(),
                _ = ticker.tick() => { peak = peak.max(private_bytes()); samples += 1; }
            }
        };
        response.bytes().await.unwrap();
        peak = peak.max(private_bytes());
        println!(
            "A2 50MiB plain upload: baseline={baseline} peak={peak} delta={} samples={samples}",
            peak.saturating_sub(baseline)
        );
    });
}
