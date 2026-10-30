//! Manual only. Use the identical source/binary feature set for baseline and fixed runs.
//! From IMPL, in PowerShell (choose a NEW child of MAIN's R20-evidence directory):
//! $env:R20_EVIDENCE_DIR='C:/_projects/SampleUser/quota-baton/.sandbox/reports/R20-evidence/baseline-01'
//! $env:R20_RUN_LABEL='baseline' # or fixed; never overwrite a previous run
//! cargo test -p quota-baton --features testkit --test takeover r20_stress::r20_spawned_core_200_handovers -- --ignored --exact --nocapture --test-threads=1
//!
//! Native ledger timestamps are wall clock; harness observations use one monotonic
//! clock. Health polling is readiness discovery, never a retry of a data request.
//! A reused HTTP client permits keepalive but cannot guarantee reuse across exit.
#![cfg(feature = "testkit")]

use super::*;
use futures_util::FutureExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    panic::AssertUnwindSafe,
    sync::atomic::{AtomicBool, Ordering},
};

const HANDOVERS: usize = 200;
const OVERALL: Duration = Duration::from_secs(18 * 60);
const PERIOD: Duration = Duration::from_millis(10);
const BODY_LIMIT: usize = 16 * 1024;
const SYNTHETIC_SSE: &str = concat!(
    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r20-synthetic\",\"status\":\"in_progress\",\"model\":\"fixture\",\"output\":[]}}\n\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r20-synthetic\",\"status\":\"completed\",\"model\":\"fixture\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
    "data: [DONE]\n\n"
);

fn nonreserved(port: u16) {
    assert!(port >= 1024 && !matches!(port, 10100 | 10101 | 10200 | 10201));
}

#[derive(Default)]
struct Counts {
    attempts: [u64; 3], // connect, keepalive, fresh
    successes: [u64; 3],
    failures: [u64; 3],
    timeouts: [u64; 3],
    non_2xx: u64,
    transport: u64,
    io_errors: u64,
    completed: usize,
    timeline_checks: usize,
    unavailable_timelines: usize,
    malformed_lifecycle_lines: usize,
    probe_close_failures: u64,
}

struct Journal {
    start: Instant,
    file: fs::File,
    counts: Counts,
}
impl Journal {
    fn emit(&mut self, mut row: Value) {
        row["observed_us"] = json!(self.start.elapsed().as_micros());
        if serde_json::to_writer(&mut self.file, &row).is_err()
            || self.file.write_all(b"\n").is_err()
        {
            self.counts.io_errors += 1;
        }
    }
    fn outcome(&mut self, lane: usize, row: Value, ok: bool, timeout: bool) {
        self.counts.attempts[lane] += 1;
        if ok {
            self.counts.successes[lane] += 1;
        } else {
            self.counts.failures[lane] += 1;
        }
        self.counts.timeouts[lane] += u64::from(timeout);
        self.emit(row);
    }
}
type Shared = Arc<Mutex<Journal>>;

// Each worker owns its socket/client/runtime. Drop signals and joins all owners,
// including on timeout/unwind; requests have a total deadline of two seconds.
struct Owners {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    journal: Shared,
}
impl Owners {
    fn new(journal: Shared) -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            journal,
        }
    }
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for owner in self.threads.drain(..) {
            if owner.join().is_err() {
                let mut j = self.journal.lock().unwrap();
                j.counts.transport += 1;
                j.emit(json!({"event":"worker_panicked"}));
            }
        }
    }
    fn probe(&mut self, address: SocketAddr) {
        let stop = self.stop.clone();
        let journal = self.journal.clone();
        self.threads.push(std::thread::spawn(move || {
            let mut due = Instant::now();
            while !stop.load(Ordering::Acquire) {
                let start = Instant::now();
                let late = start.saturating_duration_since(due).as_micros();
                let result = TcpStream::connect_timeout(&address, Duration::from_secs(1));
                let mut close_error = None;
                let (ok, timed_out, kind, os_code) = match result {
                    Ok(socket) => {
                        // The connect-only probe sends no HTTP. Abortive close
                        // avoids exhausting ephemeral ports with TIME_WAIT.
                        if let Err(error) =
                            socket2::SockRef::from(&socket).set_linger(Some(Duration::ZERO))
                        {
                            close_error =
                                Some((format!("{:?}", error.kind()), error.raw_os_error()));
                        }
                        drop(socket);
                        (true, false, "success".to_owned(), None)
                    }
                    Err(e) => (
                        false,
                        e.kind() == std::io::ErrorKind::TimedOut,
                        format!("{:?}", e.kind()),
                        e.raw_os_error(),
                    ),
                };
                let started_us = start.duration_since(journal_start(&journal)).as_micros();
                let mut j = journal.lock().unwrap();
                if close_error.is_some() {
                    j.counts.probe_close_failures += 1;
                }
                j.outcome(
                    0,
                    json!({"event":"connect",
                    "started_us":started_us,
                    "elapsed_us":start.elapsed().as_micros(),"schedule_late_us":late,
                    "ok":ok,"kind":kind,"os_code":os_code,
                    "close_ok":if ok {Some(close_error.is_none())} else {None},
                    "close_error_kind":close_error.as_ref().map(|e| &e.0),
                    "close_os_code":close_error.as_ref().and_then(|e| e.1)}),
                    ok,
                    timed_out,
                );
                drop(j);
                // No catch-up burst after a blocked connect; record scheduling lag.
                due += PERIOD;
                if due > Instant::now() {
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                } else {
                    due = Instant::now();
                }
            }
        }));
    }
    fn traffic(&mut self, lane: usize, url: String, key: String) {
        let stop = self.stop.clone();
        let journal = self.journal.clone();
        self.threads.push(std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let http = Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .http1_only()
                    .pool_max_idle_per_host(if lane == 1 { 1 } else { 0 })
                    .timeout(Duration::from_secs(2))
                    .build()
                    .unwrap();
                while !stop.load(Ordering::Acquire) {
                    let start = Instant::now();
                    let request_body = if lane == 1 {
                        json!({"model":"fixture","input":[],"stream":true})
                    } else {
                        json!({"model":"fixture","input":[]})
                    };
                    let mut row = json!({"event":"request", "lane":lane,
                        "method":"POST","path":"/v1/responses","shape":request_body,
                        "started_us":start.duration_since(journal_start(&journal)).as_micros()});
                    let mut request = http
                        .post(&url)
                        .bearer_auth(&key)
                        .header(
                            "session-id",
                            if lane == 1 {
                                "r20-keepalive"
                            } else {
                                "r20-fresh"
                            },
                        )
                        .json(&request_body);
                    if lane == 2 {
                        request = request.header("connection", "close");
                    }
                    let mut ok = false;
                    let mut timed_out = false;
                    match request.send().await {
                        Err(e) => {
                            timed_out = e.is_timeout();
                            row["transport"] = error_class(&e);
                            journal.lock().unwrap().counts.transport += 1;
                        }
                        Ok(mut response) => {
                            let status = response.status();
                            row["status"] = json!(status.as_u16());
                            row["response_headers"] = json!({});
                            for name in ["content-type", "content-length", "connection"] {
                                if let Some(value) =
                                    response.headers().get(name).and_then(|v| v.to_str().ok())
                                {
                                    row["response_headers"][name] = json!(
                                        value
                                            .chars()
                                            .take(128)
                                            .filter(|c| !c.is_control())
                                            .collect::<String>()
                                    );
                                }
                            }
                            if !status.is_success() {
                                journal.lock().unwrap().counts.non_2xx += 1;
                            }
                            let mut body = Vec::new();
                            let mut body_ok = true;
                            loop {
                                match response.chunk().await {
                                    Ok(Some(chunk)) if body.len() + chunk.len() <= BODY_LIMIT => {
                                        body.extend_from_slice(&chunk)
                                    }
                                    Ok(Some(_)) => {
                                        row["body_error"] = json!("exceeds_16KiB");
                                        body_ok = false;
                                        break;
                                    }
                                    Ok(None) => break,
                                    Err(e) => {
                                        timed_out = e.is_timeout();
                                        row["transport"] = error_class(&e);
                                        journal.lock().unwrap().counts.transport += 1;
                                        body_ok = false;
                                        break;
                                    }
                                }
                            }
                            if !status.is_success() {
                                row["error"] = safe_error(&body, &key);
                            }
                            row["body_bytes"] = json!(body.len());
                            let expected_body = if lane == 1 {
                                body.as_slice() == SYNTHETIC_SSE.as_bytes()
                            } else {
                                serde_json::from_slice::<Value>(&body)
                                    .is_ok_and(|value| value == json!({"fake":true}))
                            };
                            ok = status.is_success() && body_ok && expected_body;
                            if status.is_success() && body_ok && !ok {
                                row["body_error"] = json!(if lane == 1 {
                                    "unexpected_synthetic_sse"
                                } else {
                                    "unexpected_synthetic_json"
                                });
                            }
                        }
                    }
                    row["elapsed_us"] = json!(start.elapsed().as_micros());
                    row["ok"] = json!(ok);
                    journal.lock().unwrap().outcome(lane, row, ok, timed_out);
                    tokio::time::sleep(PERIOD).await;
                }
            });
        }));
    }
}
impl Drop for Owners {
    fn drop(&mut self) {
        self.stop();
    }
}
fn journal_start(journal: &Shared) -> Instant {
    journal.lock().unwrap().start
}
fn error_class(e: &reqwest::Error) -> Value {
    // Never serialize reqwest Display/Debug, URLs, headers, or credentials.
    json!({"timeout":e.is_timeout(),"connect":e.is_connect(),
        "body":e.is_body(),"decode":e.is_decode(),"request":e.is_request()})
}
fn safe_error(bytes: &[u8], key: &str) -> Value {
    let value: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let error = value.get("error").unwrap_or(&value);
    let mut result = json!({});
    // Only these three reply fields, with bounded printable synthetic text.
    for field in ["type", "code", "message"] {
        if let Some(value) = error.get(field) {
            if let Some(text) = value.as_str() {
                let text: String = text.chars().take(256).filter(|c| !c.is_control()).collect();
                let lower = text.to_ascii_lowercase();
                let sensitive = (!key.is_empty() && text.contains(key))
                    || [
                        "bearer ",
                        "synthetic-access",
                        "synthetic-refresh",
                        "token",
                        "secret",
                        "authorization",
                        "cookie",
                    ]
                    .iter()
                    .any(|word| lower.contains(word));
                result[field] = json!(if sensitive { "[redacted]" } else { &text });
            } else if value.is_number() || value.is_null() {
                result[field] = value.clone();
            }
        }
    }
    result
}

// Unscripted, unlimited synthetic replies: every one of the 201 cores can fetch
// startup catalog and quota without consuming inference responses or credentials.
async fn fake() -> (String, AbortTask<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    nonreserved(listener.local_addr().unwrap().port());
    let url = format!("http://{}", listener.local_addr().unwrap());
    safe_url(&url);
    let app = axum::Router::new().fallback(|request: axum::extract::Request| async move {
        use axum::response::IntoResponse;
        match (request.method().as_str(), request.uri().path()) {
            ("GET", path) if path.ends_with("/models") => axum::Json(json!({"models":[{"slug":"fixture"}]})).into_response(),
            ("GET", _) => axum::Json(json!({"plan_type":"plus","rate_limit":{"allowed":true,
                "limit_reached":false,"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_at":4102444800_i64},
                "secondary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_at":4102444800_i64}}})).into_response(),
            ("POST", path) if path.ends_with("/responses") => {
                // Drain a bounded valid request so keepalive really remains usable.
                let Ok(body) = axum::body::to_bytes(request.into_body(), BODY_LIMIT).await else {
                    return StatusCode::BAD_REQUEST.into_response();
                };
                let Ok(body) = serde_json::from_slice::<Value>(&body) else {
                    return StatusCode::BAD_REQUEST.into_response();
                };
                if body["stream"] == true {
                    ([("content-type", "text/event-stream")], SYNTHETIC_SSE).into_response()
                } else {
                    axum::Json(json!({"fake":true})).into_response()
                }
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    });
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, AbortTask(Some(task)))
}

struct Run {
    directory: PathBuf,
    journal: Shared,
    owners: Owners,
    cores: Vec<Core>,
    home: Option<Home>,
    upstream: Option<AbortTask<()>>,
    finalized: bool,
}
impl Run {
    fn finish(&mut self, reason: &str) -> Value {
        self.owners.stop();
        // Core Drop kills, joins its watchdog and reaps before Home can disappear.
        for core in &self.cores {
            let mut child = core.child.lock().unwrap();
            let _ = child.kill();
            let reaped = child.wait().is_ok();
            let mut j = self.journal.lock().unwrap();
            if !reaped {
                j.counts.transport += 1;
            }
            j.emit(json!({"event":"cleanup_core","pid":core.pid,"reaped":reaped}));
        }
        // Capture stderr before clearing Core or deleting its disposable home.
        for core in &self.cores {
            if let Some(home) = &self.home {
                record_census(core, home, false, &self.journal);
            }
        }
        self.cores.clear();
        if let Some(home) = &self.home {
            let phases: Vec<Value> = ledger(home).iter().filter_map(native_phase).collect();
            let mut j = self.journal.lock().unwrap();
            for phase in phases {
                j.emit(phase);
            }
        }
        self.upstream.take();
        let removed_home = self
            .home
            .take()
            .map(|home| {
                let path = home.path.clone();
                drop(home);
                !path.exists()
            })
            .unwrap_or(true);
        let mut j = self.journal.lock().unwrap();
        if j.file.flush().and_then(|_| j.file.sync_all()).is_err() {
            j.counts.io_errors += 1;
        }
        let c = &j.counts;
        let summary = json!({"reason":reason,"completed":c.completed,"required":HANDOVERS,
            "attempts":c.attempts,"successes":c.successes,"failures":c.failures,"timeouts":c.timeouts,
            "non_2xx":c.non_2xx,"transport_failures":c.transport,"evidence_io_errors":c.io_errors,
            "timeline_checks":c.timeline_checks,"unavailable_timelines":c.unavailable_timelines,
            "malformed_lifecycle_lines":c.malformed_lifecycle_lines,
            "probe_close_failures":c.probe_close_failures,
            "home_removed":removed_home,"elapsed_us":j.start.elapsed().as_micros(),
            "gate_passed":reason == "complete" && c.completed >= HANDOVERS && c.failures == [0;3]
                && c.timeouts == [0;3] && c.non_2xx == 0 && c.transport == 0 && c.io_errors == 0
                && c.timeline_checks == c.completed + 1 && c.unavailable_timelines == 0
                && c.malformed_lifecycle_lines == 0 && c.probe_close_failures == 0 && removed_home});
        let file = fs::File::create(self.directory.join("summary.json")).unwrap();
        serde_json::to_writer_pretty(&file, &summary).unwrap();
        file.sync_all().unwrap();
        self.finalized = true;
        summary
    }
}
impl Drop for Run {
    fn drop(&mut self) {
        if !self.finalized {
            // Best effort even during a setup/assertion panic; normal path checks IO.
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| self.finish("unwind")));
        }
    }
}
fn native_phase(row: &Value) -> Option<Value> {
    if row["event"] != "takeover" {
        return None;
    }
    let phase = row["phase"].as_str()?;
    if phase.len() > 64
        || !phase
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    Some(
        json!({"event":"native_phase","phase":phase,"pid":row["pid"].as_u64()?,
        "ts":row["ts"].as_str()?.chars().take(64).collect::<String>()}),
    )
}

// Parse only the product's two static lifecycle messages. Read at most 8 MiB
// per snapshot, accept at most 2 KiB per line and at most 64 lifecycle rows.
// Never retain the source line, target, message text, or any unlisted fields.
fn lifecycle_snapshot(core: &Core) -> Value {
    const MAX_BYTES: usize = 8 * 1024 * 1024;
    let mut bytes = Vec::new();
    let read_ok = fs::File::open(&core.log)
        .and_then(|file| {
            let mut limited = std::io::Read::take(file, (MAX_BYTES + 1) as u64);
            std::io::Read::read_to_end(&mut limited, &mut bytes)
        })
        .is_ok();
    let truncated = bytes.len() > MAX_BYTES;
    bytes.truncate(MAX_BYTES);
    let mut rows = Vec::new();
    let mut malformed = 0;
    for bytes in bytes.split(|b| *b == b'\n') {
        let Ok(line) = std::str::from_utf8(bytes) else {
            continue;
        };
        let http = line.contains("HTTP listener lifecycle");
        let handover = line.contains("Core handover lifecycle");
        if !http && !handover {
            continue;
        }
        let parsed = (|| {
            if line.len() > 2048 || rows.len() >= 64 {
                return None;
            }
            let native_ts = line.split_whitespace().next()?;
            let ts = chrono::DateTime::parse_from_rfc3339(native_ts).ok()?;
            let field = |name: &str| -> Option<&str> {
                line.split_whitespace().find_map(|token| {
                    let (key, value) = token.split_once('=')?;
                    (key == name).then(|| value.trim_matches('"'))
                })
            };
            let pid: u32 = field("pid")?.parse().ok()?;
            if pid != core.pid {
                return None;
            }
            let phase = field("phase")?;
            let mut row = json!({"event":"native_stderr_phase","pid":pid,
                "ts":ts.with_timezone(&chrono::Utc).to_rfc3339(),"phase":phase});
            if http {
                if !matches!(phase, "bound" | "first_accept" | "closed") {
                    return None;
                }
                let role = field("role")?;
                if !matches!(role, "data" | "admin") {
                    return None;
                }
                let port: u16 = field("port")?.parse().ok()?;
                row["role"] = json!(role);
                row["port"] = json!(port);
            } else if !matches!(
                phase,
                "drain_requested" | "drain_acknowledged" | "ready" | "listener_shutdown_requested"
            ) {
                return None;
            }
            Some(row)
        })();
        match parsed {
            Some(row) => rows.push(row),
            None => malformed += 1,
        }
    }
    json!({"rows":rows,"read_ok":read_ok,"truncated":truncated,"malformed_lines":malformed})
}

fn timeline_coverage(snapshot: &Value, home: &Home, retired: bool, successor: bool) -> Value {
    let rows = snapshot["rows"].as_array().unwrap();
    let mut missing = Vec::new();
    for (role, port) in [("data", home.data), ("admin", home.admin)] {
        for phase in ["bound", "first_accept", "closed"] {
            if phase == "closed" && !retired {
                continue;
            }
            if !rows
                .iter()
                .any(|r| r["role"] == role && r["port"] == port && r["phase"] == phase)
            {
                missing.push(format!("{role}.{phase}"));
            }
        }
    }
    for phase in [
        "ready",
        "listener_shutdown_requested",
        "drain_requested",
        "drain_acknowledged",
    ] {
        if phase == "listener_shutdown_requested" && !retired {
            continue;
        }
        if matches!(phase, "drain_requested" | "drain_acknowledged") && !successor {
            continue;
        }
        if !rows
            .iter()
            .any(|r| r.get("role").is_none() && r["phase"] == phase)
        {
            missing.push(phase.to_owned());
        }
    }
    json!({"available":missing.is_empty() && snapshot["read_ok"] == true
        && snapshot["truncated"] == false && snapshot["malformed_lines"] == 0,
        "missing":missing})
}

fn record_census(core: &Core, home: &Home, retired: bool, journal: &Shared) {
    let snapshot = lifecycle_snapshot(core);
    let successor = core.log.file_name().and_then(|n| n.to_str()) != Some("core-000.stderr");
    let coverage = timeline_coverage(&snapshot, home, retired, successor);
    let mut j = journal.lock().unwrap();
    j.counts.timeline_checks += 1;
    j.counts.unavailable_timelines += usize::from(coverage["available"] != true);
    j.counts.malformed_lifecycle_lines += snapshot["malformed_lines"].as_u64().unwrap() as usize;
    j.emit(
        json!({"event":"core_timeline_census","pid":core.pid,"retired":retired,
        "lifecycle":snapshot,"timeline":coverage}),
    );
}

fn synthetic_command(home: &Home) -> Command {
    let mut command = home.command();
    let profile = home.path.join("synthetic-profile");
    command
        .current_dir(&home.path)
        .env("HOME", &profile)
        .env("USERPROFILE", &profile)
        .env("APPDATA", profile.join("AppData/Roaming"))
        .env("LOCALAPPDATA", profile.join("AppData/Local"))
        .env("CODEX_HOME", profile.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", profile.join(".claude"));
    // Lifecycle evidence must not depend on the caller's inherited log filter.
    command.env("QB_LOG", "info");
    command
}

// The ordinary ledger also contains every request. Parsing all those JSON
// objects repeatedly makes the stress observer grow with all previous traffic.
// Native ledger serialization writes this literal event tag compactly. Parse
// the much smaller phase subset, and reject older lifetimes of a reused PID.
fn takeover_rows(home: &Home, since: chrono::DateTime<chrono::Utc>) -> Vec<Value> {
    let mut rows = Vec::new();
    for entry in fs::read_dir(home.path.join("ledger")).unwrap() {
        let entry = entry.unwrap();
        let mut reader = BufReader::new(fs::File::open(entry.path()).unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 || !line.ends_with('\n') {
                break; // A concurrent append's partial last line is not a record yet.
            }
            if !line.contains(r#""event":"takeover""#) {
                continue;
            }
            let row: Value = serde_json::from_str(&line).unwrap();
            let at: chrono::DateTime<chrono::Utc> = row["ts"].as_str().unwrap().parse().unwrap();
            if row["event"] == "takeover" && at >= since {
                rows.push(row);
            }
        }
    }
    rows
}

async fn exercise(run: &mut Run) {
    let (url, upstream) = fake().await;
    run.upstream = Some(upstream);
    let home = Home::new(&url).await;
    nonreserved(home.data);
    nonreserved(home.admin);
    // Replace Home's short scripted quota queue with the unlimited fake. The
    // helper's two unused servers remain owned by Home and disappear with it.
    fs::write(home.path.join("config.toml"), format!(
        "[server]\ndata_port={}\nadmin_port={}\ndrain_timeout_secs=20\n[upstream]\nanthropic_base='{url}'\ncodex_base='{url}'\nclaude_usage_base='{url}'\ncodex_usage_base='{url}'\nclaude_token_endpoint='{url}/oauth/token'\ncodex_token_endpoint='{url}/oauth/token'\n",
        home.data, home.admin)).unwrap();
    accounts(&home, 1);
    run.home = Some(home);
    let home = run.home.as_ref().unwrap();
    let http = client();
    let mut command = synthetic_command(home);
    command.arg("serve");
    let mut predecessor_started = chrono::Utc::now();
    run.cores.push(Core::from_command(
        command,
        home.path.join("core-000.stderr"),
    ));
    run.journal
        .lock()
        .unwrap()
        .emit(json!({"event":"core_spawned","core":0,"pid":run.cores[0].pid}));
    let admin_token = token(home).await;
    ready(&http, home, &admin_token, run.cores[0].pid).await;
    run.journal
        .lock()
        .unwrap()
        .emit(json!({"event":"initial_ready","pid":run.cores[0].pid,
        "data_port":home.data,"admin_port":home.admin}));
    run.owners
        .probe(SocketAddr::from(([127, 0, 0, 1], home.data)));
    run.owners.traffic(1, home.data_url(), home.key());
    run.owners.traffic(2, home.data_url(), home.key());
    until(|| {
        run.journal
            .lock()
            .unwrap()
            .counts
            .attempts
            .iter()
            .all(|n| *n >= 5)
    })
    .await;
    for number in 1..=HANDOVERS {
        let before = run.journal.lock().unwrap().counts.attempts;
        let predecessor = run.cores[0].pid;
        let mut command = synthetic_command(home);
        command.args(["serve", "--takeover"]);
        let successor_started = chrono::Utc::now();
        run.journal
            .lock()
            .unwrap()
            .emit(json!({"event":"handover_start","handover":number,"predecessor":predecessor}));
        run.cores.push(Core::from_command(
            command,
            home.path.join(format!("core-{number:03}.stderr")),
        ));
        let successor = run.cores[1].pid;
        run.journal
            .lock()
            .unwrap()
            .emit(json!({"event":"core_spawned","core":number,
            "pid":successor,"predecessor":predecessor}));
        ready(&http, home, &admin_token, successor).await;
        run.journal
            .lock()
            .unwrap()
            .emit(json!({"event":"ready_observed","handover":number,"pid":successor}));
        until(|| run.cores[0].status().is_some()).await;
        run.journal
            .lock()
            .unwrap()
            .emit(json!({"event":"exit_observed","handover":number,
            "pid":predecessor,"success":run.cores[0].status().unwrap().success()}));
        assert!(
            run.cores[0].status().unwrap().success(),
            "predecessor exited unsuccessfully"
        );
        let after_exit = run.journal.lock().unwrap().counts.attempts;
        until(|| {
            let rows = takeover_rows(home, predecessor_started);
            [
                (predecessor, ["drain_started", "lease_released", "exited"]),
                (
                    successor,
                    ["lease_acquired", "bindings_imported", "listening"],
                ),
            ]
            .iter()
            .all(|(pid, names)| {
                names.iter().all(|name| {
                    rows.iter()
                        .any(|row| row["pid"] == *pid && row["phase"] == *name)
                })
            })
        })
        .await;
        until(|| {
            run.journal
                .lock()
                .unwrap()
                .counts
                .attempts
                .iter()
                .zip(after_exit)
                .all(|(now, after_exit)| *now >= after_exit + 2)
        })
        .await;
        let rows = takeover_rows(home, predecessor_started);
        if number == 1 {
            let original: Vec<_> = ledger(home)
                .into_iter()
                .filter(|row| row["event"] == "takeover")
                .filter(|row| {
                    row["ts"]
                        .as_str()
                        .unwrap()
                        .parse::<chrono::DateTime<chrono::Utc>>()
                        .unwrap()
                        >= predecessor_started
                })
                .collect();
            assert_eq!(rows, original, "native phase filter must be lossless");
        }
        let phases: Vec<_> = rows
            .iter()
            .filter(|r| r["pid"] == predecessor || r["pid"] == successor)
            .filter_map(native_phase)
            .collect();
        let after = run.journal.lock().unwrap().counts.attempts;
        let predecessor_stderr = lifecycle_snapshot(&run.cores[0]);
        let successor_stderr = lifecycle_snapshot(&run.cores[1]);
        let predecessor_coverage = timeline_coverage(&predecessor_stderr, home, true, number > 1);
        let successor_coverage = timeline_coverage(&successor_stderr, home, false, true);
        run.journal
            .lock()
            .unwrap()
            .emit(json!({"event":"handover_complete","handover":number,
            "predecessor":predecessor,"successor":successor,"native_phases":phases,
            "predecessor_stderr":predecessor_stderr,"successor_stderr":successor_stderr,
            "predecessor_timeline":predecessor_coverage,"successor_timeline":successor_coverage,
            "attempts_before":before,"attempts_after":after}));
        run.journal.lock().unwrap().counts.completed += 1;
        // The authoritative census is taken at retirement (and final cleanup),
        // so a successor's first accept later in its lifetime remains available.
        record_census(&run.cores[0], home, true, &run.journal);
        drop(run.cores.remove(0));
        predecessor_started = successor_started;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual R20: 201 synthetic spawned cores, explicit evidence directory, up to 18 minutes"]
async fn r20_spawned_core_200_handovers() {
    let directory = PathBuf::from(
        std::env::var_os("R20_EVIDENCE_DIR").expect("set R20_EVIDENCE_DIR explicitly"),
    );
    assert!(directory.is_absolute(), "evidence path must be absolute");
    let allowed = Path::new("C:/_projects/SampleUser/quota-baton/.sandbox/reports/R20-evidence");
    fs::create_dir_all(allowed).unwrap();
    assert_eq!(
        directory.parent().unwrap().canonicalize().unwrap(),
        allowed.canonicalize().unwrap(),
        "choose an immediate child of MAIN .sandbox/reports/R20-evidence"
    );
    let label = std::env::var("R20_RUN_LABEL").expect("set R20_RUN_LABEL=baseline or fixed");
    assert!(matches!(label.as_str(), "baseline" | "fixed"));
    fs::create_dir(&directory).expect("evidence directory must be new; no overwrite");
    let binary = Path::new(env!("CARGO_BIN_EXE_quota-baton"));
    let metadata = json!({"schema":1,"label":label,"handovers":HANDOVERS,
        "started_utc":chrono::Utc::now().to_rfc3339(),"os":std::env::consts::OS,
        "arch":std::env::consts::ARCH,"probe_period_ms":10,"connect_timeout_ms":1000,
        "probe_close_mode":"abortive_linger_zero_no_application_request",
        "ledger_observation_mode":"compact_takeover_tag_prefilter_then_json_current_generation",
        "request_timeout_ms":2000,"overall_timeout_secs":OVERALL.as_secs(),
        "harness_sha256":format!("{:x}",Sha256::digest(include_bytes!("r20_stress.rs"))),
        "binary_sha256":format!("{:x}",Sha256::digest(fs::read(binary).unwrap())),
        "testkit":cfg!(feature="testkit"),"phase0":cfg!(feature="phase0"),
        "shape_capture":cfg!(feature="shape-capture"),"automatic_request_retries":false,
        "lanes":["OS-thread TCP connect","HTTP/1 pooled keepalive","HTTP/1 connection-close fresh"],
        "request_shapes":{"keepalive":{"model":"fixture","input":[],"stream":true},
            "fresh":{"model":"fixture","input":[]}},
        "expected_responses":{"keepalive":"exact synthetic created/completed SSE and DONE",
            "fresh":{"fake":true}},
        "response_header_allowlist":["content-type","content-length","connection"],
        "qualifiers":["synthetic loopback only; no live profile",
            "native ts is wall clock; observed_us is monotonic since journal creation",
            "10ms target cadence; blocking probes record lag; no catch-up burst",
            "keepalive reuse permitted, not guaranteed across takeover",
            "native ledger and allowlisted timestamped stderr lifecycle fields; no inferred timestamps",
            "missing required lifecycle phases make timeline unavailable and fail the gate; last live core close excluded",
            "gate uses final per-core census; per-handover successor snapshots can precede a later first accept",
            "stderr files unique per core, disposable with home; no raw logs or bodies; only three allowlisted response headers retained"]});
    fs::write(
        directory.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(Journal {
        start: Instant::now(),
        file: fs::File::create(directory.join("observations.jsonl")).unwrap(),
        counts: Counts::default(),
    }));
    let mut run = Run {
        directory,
        owners: Owners::new(journal.clone()),
        journal,
        home: None,
        cores: Vec::new(),
        upstream: None,
        finalized: false,
    };
    let outcome = AssertUnwindSafe(tokio::time::timeout(OVERALL, exercise(&mut run)))
        .catch_unwind()
        .await;
    let reason = match outcome {
        Ok(Ok(())) => "complete",
        Ok(Err(_)) => "overall_timeout",
        Err(_) => "panic",
    };
    let summary = run.finish(reason);
    assert_eq!(
        summary["gate_passed"], true,
        "R20 gate failed; preserved summary: {summary}"
    );
}
