//! M7c acceptance harness: seeded R7 A2 distribution and private-memory measurement.
use axum::{
    Router,
    body::Body,
    extract::{Request, ws::WebSocketUpgrade},
    response::Response,
    routing::get,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt, future::join_all};
use qb_core::{
    accounts::{AccountStore, import},
    config::Config,
    debug_pin::DebugPins,
    guard::private_bytes,
    http,
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use serde_json::json;
use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

type IdleSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct CombinedUpstream {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl CombinedUpstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let app = Router::new()
                .route(
                    "/{*path}",
                    get(|ws: WebSocketUpgrade| async move {
                        ws.on_upgrade(
                            |mut socket| async move { while socket.recv().await.is_some() {} },
                        )
                    })
                    .post(|request: Request| async move {
                        let (_, body) = request.into_parts();
                        let mut stream = body.into_data_stream();
                        let _ = stream.next().await;
                        tokio::spawn(async move { while stream.next().await.is_some() {} });
                        Response::new(Body::from("ok"))
                    }),
                )
                .fallback(|request: Request| async move {
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

impl Drop for CombinedUpstream {
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

struct R7Sampler {
    state: u64,
}

impl R7Sampler {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn unit(&mut self) -> f64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.state >> 11) as f64 / ((1u64 << 53) as f64)
    }

    fn next(&mut self) -> (&'static str, A2Shape) {
        let p = self.unit();
        let size = if p < 0.35 {
            800 * 1024
        } else if p < 0.50 {
            1_400 * 1024
        } else if p < 0.90 {
            6 * 1024 * 1024
        } else if p < 0.97 {
            33 * 1024 * 1024
        } else {
            51 * 1024 * 1024
        };
        let compressed = size >= 33 * 1024 * 1024 && self.unit() < 0.35;
        let bucket = if size < 1024 * 1024 {
            "<1 MB"
        } else if size <= 10 * 1024 * 1024 {
            "1-10 MB"
        } else {
            ">10 MB"
        };
        (
            bucket,
            if compressed {
                A2Shape::Zstd(size)
            } else {
                A2Shape::Plain(size)
            },
        )
    }
}

#[test]
fn m7c_r7_sampler_is_seeded_and_has_expected_tail() {
    let mut sampler = R7Sampler::new(0x4d_3743);
    let mut sizes = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let (_, shape) = sampler.next();
        sizes.push(match shape {
            A2Shape::Plain(size) | A2Shape::Zstd(size) => size,
        });
    }
    sizes.sort_unstable();
    assert!(sizes[4_999] <= 6 * 1024 * 1024);
    assert!(sizes[9_499] >= 6 * 1024 * 1024);
    assert!(sizes[9_899] >= 33 * 1024 * 1024);
    assert_eq!(sizes[9_999], 51 * 1024 * 1024);
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

    async fn idle_socket(&self, session: &str) -> IdleSocket {
        let mut request = format!("ws://{}/v1/responses", self.servers.data_addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer synthetic-client".parse().unwrap());
        request
            .headers_mut()
            .insert("session-id", session.parse().unwrap());
        connect_async(request).await.unwrap().0
    }
}

async fn a2_harness() -> (CombinedUpstream, A2Harness) {
    let upstream = CombinedUpstream::start().await;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/tests");
    fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(root).unwrap();
    let home = temp.path().join("home");
    let cpa = temp.path().join("cpa");
    fs::create_dir_all(&cpa).unwrap();
    let mut selectors = Vec::new();
    for provider in ["codex", "claude"] {
        for n in 0..3 {
            let name = format!("{provider}-{n}");
            let email = format!("{name}@example.invalid");
            fs::write(cpa.join(format!("{name}.json")), serde_json::to_vec(&json!({
            "type":provider, "email":email, "account_id":format!("synthetic-{name}"),
            "account_uuid":format!("synthetic-uuid-{n}"), "organization_uuid":"synthetic-org",
            "access_token":format!("synthetic-access-{name}"), "refresh_token":format!("synthetic-refresh-{name}"),
            "id_token":format!("synthetic-id-{name}"), "last_refresh":"2026-01-01T00:00:00Z", "expired":"2030-01-01T00:00:00Z"
            })).unwrap()).unwrap();
            selectors.push(format!("{provider}:{email}"));
        }
    }
    let store = Arc::new(AccountStore::acquire(&home).unwrap());
    import::run(&store, &cpa, &selectors, false).unwrap();
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    config.upstream.codex_base = upstream.url.clone();
    config.upstream.anthropic_base = upstream.url.clone();
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

#[tokio::test]
async fn m7c_synthetic_load_smoke_uses_idle_websockets_and_r7_http() {
    let (_upstream, h) = a2_harness().await;
    let mut sockets = Vec::with_capacity(50);
    for n in 0..50 {
        sockets.push(h.idle_socket(&format!("m7c-idle-{n}")).await);
    }
    let baseline = private_bytes();
    let mut sampler = R7Sampler::new(0x4d_3743);
    let jobs = (0..32).map(|n| {
        let (bucket, shape) = sampler.next();
        let url = h.url();
        let client = h.client.clone();
        async move {
            let (body, compressed) = a2_body(shape, true);
            let mut request = client
                .post(url)
                .bearer_auth("synthetic-client")
                .header("session-id", format!("m7c-http-{n}"))
                .body(body);
            if compressed {
                request = request.header("content-encoding", "zstd");
            }
            let response = request.send().await.unwrap();
            let status = response.status();
            let _ = response.bytes().await.unwrap();
            (bucket, status)
        }
    });
    let results = join_all(jobs).await;
    let failures: Vec<_> = results
        .iter()
        .filter(|(_, status)| !status.is_success())
        .collect();
    assert!(failures.is_empty(), "synthetic load statuses: {failures:?}");
    println!(
        "M7c smoke: idle_ws={} baseline_private_bytes={} final_private_bytes={}",
        sockets.len(),
        baseline,
        private_bytes()
    );
    for socket in &mut sockets {
        let _ = socket.close(None).await;
    }
}

#[tokio::test]
#[ignore = "requires a spawned core process for authoritative private-byte accounting"]
async fn m7c_300_idle_websocket_pairs_measure_private_memory() {
    let (_upstream, h) = a2_harness().await;
    let baseline = private_bytes();
    let mut sockets = Vec::with_capacity(300);
    for n in 0..300 {
        sockets.push(h.idle_socket(&format!("m7c-300-idle-{n}")).await);
    }
    let with_pairs = private_bytes();
    println!(
        "M7c 300 idle WS: baseline_private_bytes={baseline} with_pairs_private_bytes={with_pairs}"
    );
    for socket in &mut sockets {
        let _ = socket.close(None).await;
    }
}

#[test]
#[ignore = "M7c A1 soak; run in the background with M7C_DURATION_SECS=7200"]
fn m7c_a1_seeded_soak_records_private_memory() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (_upstream, h) = a2_harness().await;
        let mut sockets = Vec::with_capacity(50);
        for n in 0..50 {
            sockets.push(h.idle_socket(&format!("m7c-soak-{n}")).await);
        }
        let duration = std::env::var("M7C_DURATION_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(2 * 60 * 60);
        let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/soak");
        fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("m7c-a1.jsonl");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .unwrap();
        let started = Instant::now();
        let mut sampler = R7Sampler::new(0x4d_3743);
        let mut peak = private_bytes();
        let mut rounds = 0u64;
        while started.elapsed() < Duration::from_secs(duration) {
            let jobs = (0..32).map(|n| {
                let (_, shape) = sampler.next();
                let url = h.url();
                let client = h.client.clone();
                async move {
                    let (body, compressed) = a2_body(shape, true);
                    let mut request = client
                        .post(url)
                        .bearer_auth("synthetic-client")
                        .header("session-id", format!("m7c-soak-http-{rounds}-{n}"))
                        .body(body);
                    if compressed {
                        request = request.header("content-encoding", "zstd");
                    }
                    let response = request.send().await.unwrap();
                    response.bytes().await.unwrap();
                }
            });
            join_all(jobs).await;
            // Exercise reconnect-driven resends without retaining transcripts.
            if rounds % 6 == 0 {
                for socket in sockets.iter_mut().take(4) {
                    let _ = socket
                        .send(Message::Binary(Bytes::from(vec![b'w'; 64 * 1024])))
                        .await;
                }
            }
            peak = peak.max(private_bytes());
            let line = json!({
                "elapsed_secs": started.elapsed().as_secs(),
                "private_bytes": private_bytes(),
                "peak_private_bytes": peak,
                "http_concurrency": 32,
                "active_websocket_pairs": sockets.len(),
                "round": rounds,
                "seed": "0x4d3743"
            });
            use std::io::Write;
            writeln!(log, "{line}").unwrap();
            rounds += 1;
        }
        for socket in &mut sockets {
            let _ = socket.close(None).await;
        }
        println!("M7c A1 soak: rounds={rounds} peak_private_bytes={peak}");
    });
}

#[test]
#[ignore = "A2 acceptance harness; run explicitly on the implementer's machine"]
fn a2_benchmark_200_requests_per_shape_and_private_memory() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (_upstream, h) = a2_harness().await;
        let sample_count = std::env::var("M7C_SAMPLES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(200);
        let mut sampler = R7Sampler::new(0x4d_3743);
        let mut buckets: std::collections::BTreeMap<&'static str, Vec<Duration>> =
            std::collections::BTreeMap::new();
        for _ in 0..sample_count {
            let (bucket, shape) = sampler.next();
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
            buckets
                .entry(bucket)
                .or_default()
                .push(proxy_start.elapsed().saturating_sub(direct_ttfb));
            proxy.bytes().await.unwrap();
        }
        for (bucket, mut added) in buckets {
            added.sort_unstable();
            let p50 = added[added.len() / 2];
            let p95 = added[(added.len() * 95 / 100).min(added.len() - 1)];
            println!(
                "A2 {bucket}: samples={} added TTFB p50={p50:?} p95={p95:?}",
                added.len()
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
