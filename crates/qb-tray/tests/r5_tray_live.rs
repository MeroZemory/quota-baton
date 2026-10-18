//! R5 live acceptance with adjacent synthetic fixtures/tray_v2.
//! Only opt-in debug+testkit; real spawned core, native HWND, real admin/data API.
#![cfg(all(windows, debug_assertions, feature = "testkit"))]

#[path = "support/r5_uia.rs"]
mod uia;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use qb_core::accounts::{Account, AccountStore, HealthSignal, Identity, Tokens};
use qb_tray::{Rect, v2, v2_geometry as g, v2_render::Renderer, v2_session::Session};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BITMAPINFO, BITMAPINFOHEADER, ClientToScreen, CreateCompatibleBitmap,
            CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits,
            RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow, ReleaseDC, SelectObject, UpdateWindow,
        },
        Storage::Xps::PrintWindow,
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
                SetThreadDpiAwarenessContext,
            },
            WindowsAndMessaging::{
                ClipCursor, FindWindowExW, GWL_EXSTYLE, GetClientRect, GetClipCursor, GetCursorPos,
                GetWindowLongPtrW, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST,
                SMTO_ABORTIFHUNG, SPI_GETMOUSEHOVERTIME, SWP_NOACTIVATE, SWP_NOMOVE,
                SWP_NOOWNERZORDER, SWP_NOSIZE, SendMessageTimeoutW, SetCursorPos, SetWindowPos,
                SystemParametersInfoW, WM_APP, WM_KEYDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
                WM_MOUSEWHEEL, WS_EX_TOPMOST, WindowFromPoint,
            },
        },
    },
    core::w,
};

static SERIAL: Mutex<()> = Mutex::new(());
const WAIT: Duration = Duration::from_secs(10);
const STARTUP: Duration = Duration::from_secs(30);
const CASE_BUDGET: Duration = Duration::from_secs(40);
const INPUT: Duration = Duration::from_secs(4);
const CASES: [(&str, &str); 9] = [
    ("01", "overview"),
    ("02", "relogin"),
    ("03", "exhausted"),
    ("04", "credits"),
    ("05", "menu"),
    ("06", "empty"),
    ("07", "quit"),
    ("08", "settings"),
    ("09", "tooltip"),
];

fn fixture(name: &str) -> Result<Value> {
    // These paths become ordinary tracked assets at integration; no ignored include.
    let text = match name {
        "overview" | "credit-actions" => include_str!("fixtures/tray_v2/overview.json"),
        "relogin" => include_str!("fixtures/tray_v2/relogin.json"),
        "exhausted" => include_str!("fixtures/tray_v2/exhausted.json"),
        "credits" => include_str!("fixtures/tray_v2/credits.json"),
        "menu" => include_str!("fixtures/tray_v2/menu.json"),
        "empty" => include_str!("fixtures/tray_v2/empty.json"),
        "quit" => include_str!("fixtures/tray_v2/quit.json"),
        "settings" => include_str!("fixtures/tray_v2/settings.json"),
        "tooltip" => include_str!("fixtures/tray_v2/tooltip.json"),
        _ => bail!("unknown R5 fixture"),
    };
    Ok(serde_json::from_str(text)?)
}

fn same_number(actual: &Value, expected: &Value) -> bool {
    actual
        .as_f64()
        .zip(expected.as_f64())
        .is_some_and(|(a, e)| a == e)
}

#[test]
fn r5_numeric_equality_accepts_float_integer_and_rejects_missing() {
    assert!(same_number(&json!(62.0), &json!(62)));
    assert!(same_number(&json!(0), &json!(0.0)));
    assert!(!same_number(&Value::Null, &Value::Null));
    assert!(!same_number(&json!(62), &Value::Null));
    assert!(!same_number(&json!("62"), &json!(62)));
    assert!(!same_number(&json!(50), &json!(62)));
}

fn shift_days(now: DateTime<Utc>, earliest: DateTime<Utc>) -> i64 {
    let horizon = now + chrono::Duration::hours(1);
    if earliest > horizon {
        0
    } else {
        (horizon - earliest).num_days() + 1
    }
}
#[derive(Clone)]
struct FixtureClock {
    days: i64,
    evaluated_at: DateTime<Utc>,
}
fn fixture_clock() -> Result<&'static FixtureClock> {
    // One decision for the entire serial run, including cases started after
    // midnight. No per-account/window shifts, machine clock or display bypass.
    static CLOCK: OnceLock<std::result::Result<FixtureClock, String>> = OnceLock::new();
    CLOCK
        .get_or_init(|| {
            (|| -> Result<FixtureClock> {
                let mut earliest = None;
                for (_, name) in CASES {
                    for a in fixture(name)?["accounts"]
                        .as_array()
                        .context("fixture accounts")?
                    {
                        if let Some(until) = a["health"]["until"].as_str() {
                            let until = until.parse::<DateTime<Utc>>()?;
                            earliest =
                                Some(earliest.map_or(until, |old: DateTime<Utc>| old.min(until)));
                        }
                    }
                }
                let now = Utc::now();
                Ok(FixtureClock {
                    days: shift_days(now, earliest.context("fixture cooling deadline")?),
                    evaluated_at: now,
                })
            })()
            .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!(e.clone()))
}
fn shift_timestamps(value: &mut Value, days: i64) -> Result<()> {
    match value {
        Value::String(s) => {
            if let Ok(timestamp) = DateTime::parse_from_rfc3339(s) {
                *s = timestamp
                    .checked_add_signed(chrono::Duration::days(days))
                    .context("fixture timestamp overflow")?
                    .to_rfc3339();
            }
        }
        Value::Array(values) => {
            for v in values {
                shift_timestamps(v, days)?;
            }
        }
        Value::Object(values) => {
            for v in values.values_mut() {
                shift_timestamps(v, days)?;
            }
        }
        _ => {}
    }
    Ok(())
}
#[test]
fn r5_fixture_shift_keeps_deadlines_future_and_relative_times() -> Result<()> {
    let earliest = "2026-10-19T00:10:00+09:00".parse::<DateTime<Utc>>()?;
    assert_eq!(
        shift_days(earliest - chrono::Duration::hours(2), earliest),
        0
    );
    assert_eq!(
        shift_days(earliest - chrono::Duration::minutes(10), earliest),
        1
    );
    assert_eq!(
        shift_days(earliest + chrono::Duration::days(3), earliest),
        4
    );
    let mut input = json!({"a":"2026-10-19T00:10:00+09:00","b":"2026-10-19T01:40:00+09:00","email":"synthetic@example.invalid"});
    shift_timestamps(&mut input, 1)?;
    assert_eq!(input["a"], "2026-10-20T00:10:00+09:00");
    assert_eq!(input["b"], "2026-10-20T01:40:00+09:00");
    assert_eq!(input["email"], "synthetic@example.invalid");
    Ok(())
}

fn listener() -> Result<TcpListener> {
    loop {
        let l = TcpListener::bind("127.0.0.1:0")?;
        if !matches!(l.local_addr()?.port(), 10100 | 10101) {
            return Ok(l);
        }
    }
}

// Bounded HTTP/1 parser: no proxy, DNS, remote redirect, unbounded accept/read.
fn read_request(stream: &mut TcpStream) -> Result<String> {
    // Windows can inherit the listening socket's nonblocking mode on accept.
    // A connection may be accepted before its HTTP bytes arrive.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut bytes = Vec::new();
    loop {
        ensure!(
            Instant::now() < deadline && bytes.len() < 65536,
            "fake request budget"
        );
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf)?;
        ensure!(n > 0, "truncated fake request");
        bytes.extend_from_slice(&buf[..n]);
        if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            let head = std::str::from_utf8(&bytes[..end])?;
            let length = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>())
                })
                .transpose()?
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return Ok(head.into());
            }
        }
    }
}

#[test]
fn r5_fake_accept_waits_for_http_bytes_instead_of_failing_would_block() -> Result<()> {
    let server = listener()?;
    server.set_nonblocking(true)?;
    let mut client = TcpStream::connect(server.local_addr()?)?;
    let (mut accepted, _) = server.accept()?;
    accepted.set_nonblocking(true)?;
    let mut probe = [0u8; 1];
    ensure!(
        accepted
            .peek(&mut probe)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock),
        "precondition: accepted stream has no request yet"
    );
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        let _ = started_tx.send(());
        let _ = result_tx.send(read_request(&mut accepted));
    });
    started_rx.recv_timeout(Duration::from_secs(1))?;
    let premature = result_rx.recv_timeout(Duration::from_millis(100));
    client.write_all(b"GET /synthetic HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")?;
    let result = match premature {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            result_rx.recv_timeout(Duration::from_secs(1))?
        }
        Err(error) => return Err(error.into()),
    };
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("fake reader panicked"))?;
    ensure!(
        result?.starts_with("GET /synthetic HTTP/1.1"),
        "expected complete fake request"
    );
    Ok(())
}

struct Upstream {
    port: u16,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
    secondary_used: Arc<AtomicU32>,
}
impl Upstream {
    fn start(accounts: BTreeMap<String, Value>, metadata: PathBuf) -> Result<Self> {
        let l = listener()?;
        l.set_nonblocking(true)?;
        let port = l.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        // u32::MAX = ordinary fixture response. Override affects only the
        // credit-bearing synthetic Codex identity's successful DATA headers.
        let secondary_used = Arc::new(AtomicU32::new(u32::MAX));
        let used_override = secondary_used.clone();
        let task = thread::spawn(move || {
            let mut paid_started = false;
            while !signal.load(Ordering::Acquire) {
                match l.accept() {
                    Ok((mut s, _)) => {
                        let request_started = Instant::now();
                        let mut request_metadata = json!({"route_class":"unparsed","account_ref":null,"response_status":null});
                        let result = (|| -> Result<()> {
                            let head = read_request(&mut s)?;
                            let route = head.lines().next().context("request line")?;
                            let bearer = head.lines().find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("authorization")
                                    .then(|| v.trim().strip_prefix("Bearer "))
                                    .flatten()
                            });
                            let a = bearer
                                .and_then(|b| b.strip_prefix("synthetic-r5-access-"))
                                .and_then(|id| accounts.get(id));
                            let profile = route.contains("/api/oauth/profile ");
                            let data = route.starts_with("POST /responses ")
                                || route.starts_with("POST /v1/responses ")
                                || route.starts_with("POST /v1/messages ")
                                || route.starts_with("POST /messages ");
                            request_metadata["route_class"] = json!(if profile {
                                "profile"
                            } else if data {
                                "data"
                            } else {
                                "usage"
                            });
                            request_metadata["account_ref"] =
                                a.map_or(Value::Null, |a| a["id"].clone());
                            let (status, body, headers) = if profile {
                                // 404 must retain seeded synthetic plan metadata.
                                ("404 Not Found", json!({}), String::new())
                            } else if let Some(a) = a {
                                if data {
                                    // Only the synthetic credit-bearing Codex account
                                    // returns paid evidence, on real successful data.
                                    let codex_mero = a["provider"] == "codex"
                                        && a["credits"]["has_credits"] == true;
                                    let used = used_override.load(Ordering::Acquire);
                                    // A normal subscription update is NOT paid-serving
                                    // evidence. Do not emit a serving flag in this branch.
                                    let headers = if codex_mero && used <= 100 {
                                        format!(
                                            "x-codex-secondary-window-minutes: 10080\r\nx-codex-secondary-used-percent: {used}\r\n"
                                        )
                                    } else if codex_mero {
                                        paid_started = true;
                                        // Actual C9 paid evidence at 99% keeps the
                                        // strict C5 <100 credit gate valid. The
                                        // reference's 0% remaining becomes 1%.
                                        "x-codex-primary-window-minutes: 300\r\nx-codex-primary-used-percent: 99\r\nx-codex-active-limit: credits\r\nx-codex-credits-serving: true\r\nx-codex-credits-has-credits: true\r\nx-codex-credits-balance: 19395.23\r\n".into()
                                    } else {
                                        String::new()
                                    };
                                    (
                                        "200 OK",
                                        json!({"id":"synthetic-r5-response","object":"response","output":[],"usage":{"input_tokens":1,"output_tokens":1}}),
                                        headers,
                                    )
                                } else {
                                    let mut current = a.clone();
                                    if paid_started
                                        && current["provider"] == "codex"
                                        && current["credits"]["has_credits"] == true
                                    {
                                        for w in current["windows"]
                                            .as_array_mut()
                                            .context("fake windows")?
                                        {
                                            if w["name"] == "5h" {
                                                w["used_pct"] = json!(99);
                                            }
                                        }
                                    }
                                    ("200 OK", usage(&current), String::new())
                                }
                            } else {
                                ("404 Not Found", json!({}), String::new())
                            };
                            let body = body.to_string();
                            request_metadata["response_status"] = json!(
                                status
                                    .split_whitespace()
                                    .next()
                                    .context("fake status")?
                                    .parse::<u16>()?
                            );
                            write!(
                                s,
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )?;
                            Ok(())
                        })();
                        // Whitelist only: never persist the request, bearer,
                        // response body, or arbitrary parser error strings.
                        request_metadata["completed"] = json!(result.is_ok());
                        request_metadata["io_error_kind"] = json!(
                            result
                                .as_ref()
                                .err()
                                .and_then(|e| e.downcast_ref::<std::io::Error>())
                                .map(|e| format!("{:?}", e.kind()))
                        );
                        request_metadata["elapsed_ms"] =
                            json!(request_started.elapsed().as_millis());
                        if let Ok(mut log) = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&metadata)
                        {
                            let _ = writeln!(log, "{request_metadata}");
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            port,
            stop,
            task: Some(task),
            secondary_used,
        })
    }
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.task.take() {
            let _ = t.join();
        }
    }
}
fn usage(a: &Value) -> Value {
    let mut out = json!({});
    if a["provider"] == "claude" {
        for w in a["windows"].as_array().unwrap() {
            let key = match w["name"].as_str().unwrap() {
                "5h" => "five_hour",
                "7d" => "seven_day",
                _ => continue,
            };
            out[key] =
                json!({"utilization":w["used_pct"],"resets_at":w["reset_at"],"status":w["status"]});
        }
    } else {
        out = json!({"plan_type":a["plan"],"credits":a["credits"],"rate_limit":{}});
        for w in a["windows"].as_array().unwrap() {
            let (key, seconds) = match w["name"].as_str().unwrap() {
                "5h" => ("primary_window", 18000),
                "7d" => ("secondary_window", 604800),
                _ => continue,
            };
            out["rate_limit"][key] = json!({"used_percent":w["used_pct"],"reset_at":w["reset_at"],"limit_window_seconds":seconds});
        }
        // Do NOT return serving_from_credits/active_limit=credits in polls.
        // Actual successful relay C9 headers are the sole paid-serving source.
    }
    out
}

struct Live {
    tray: Option<Child>,
    core: Option<Child>,
    _upstream: Upstream,
    home: PathBuf,
    admin: u16,
    data: u16,
    desired: Value,
    expected_health: BTreeMap<String, Value>,
    fixture_clock: Value,
    ids: BTreeMap<String, String>,
    rt: tokio::runtime::Runtime,
    client: reqwest::Client,
    started: Instant,
    session: Session,
    overlay: g::Overlay,
}
impl Drop for Live {
    fn drop(&mut self) {
        // Only handles returned by this test's own spawn; no name/PID sweeps.
        for c in [&mut self.tray, &mut self.core].into_iter().flatten() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
fn child(bin: &Path, home: &Path, name: &str) -> Result<Command> {
    let mut c = Command::new(bin);
    c.env_clear().current_dir(home).stdin(Stdio::null());
    if name == "core" {
        c.creation_flags(0x08000000);
    }
    for k in ["SystemRoot", "WINDIR"] {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }
    for k in [
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "HOME",
        "TEMP",
        "TMP",
    ] {
        c.env(k, home);
    }
    c.env("QB_HOME", home)
        .stdout(fs::File::create(home.join(format!("{name}.stdout.log")))?)
        .stderr(fs::File::create(home.join(format!("{name}.stderr.log")))?);
    Ok(c)
}
fn binary(root: &Path, key: &str, name: &str) -> Result<PathBuf> {
    let p = std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/debug").join(name))
        .canonicalize()?;
    ensure!(
        p.is_file() && p.starts_with(root.join("target").canonicalize()?),
        "{key} must be in workspace target/"
    );
    Ok(p)
}
impl Live {
    fn start(name: &str) -> Result<Self> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let base = root.join(".sandbox/r5-tray-live");
        fs::create_dir_all(&base)?;
        let home = base.join(format!(
            "{name}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&home)?;
        let clock = fixture_clock()?;
        let mut desired = fixture(name)?;
        shift_timestamps(&mut desired, clock.days)?;
        let reference_now = "2026-10-18T21:05:00+09:00".parse::<DateTime<chrono::FixedOffset>>()?;
        let clock_record = json!({
            "reference_now": reference_now.to_rfc3339(),
            "day_shift": clock.days,
            "shifted_reference_now": (reference_now + chrono::Duration::days(clock.days)).to_rfc3339(),
            "decision_at": clock.evaluated_at,
            "minimum_cooling_margin_seconds": 3600,
            "calendar_text": "real runtime clock; reference timestamps shifted uniformly; no historical clock claim",
            "credit_bootstrap": if matches!(name, "credits" | "credit-actions") { "5h 99% pre-request and actual successful paid C9 response; strict C5 gate preserved; 1% remaining differs from reference 0%" } else { "none" }
        });
        fs::write(
            home.join("fixture-clock.json"),
            serde_json::to_vec_pretty(&clock_record)?,
        )?;
        if matches!(name, "credits" | "credit-actions") {
            // Core's real 99% credit precondition permits the subsequent
            // successful paid request at 99%. 100% cooling cannot be bypassed
            // by allowance; record the truthful reference difference above.
            let a = desired["accounts"]
                .as_array_mut()
                .context("accounts")?
                .iter_mut()
                .find(|a| a["id"] == "codex-acct-alpha")
                .context("credit bootstrap account")?;
            let w = a["windows"]
                .as_array_mut()
                .context("windows")?
                .iter_mut()
                .find(|w| w["name"] == "5h")
                .context("primary bootstrap window")?;
            w["used_pct"] = json!(99);
        }
        let mut ids = BTreeMap::new();
        let mut expected_health = BTreeMap::new();
        let mut upstream_accounts = BTreeMap::new();
        let mut bindings = vec![];
        {
            let store = AccountStore::acquire(&home).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            for a in desired["accounts"].as_array().unwrap() {
                let ref_id = a["id"].as_str().unwrap();
                let email = a["email"].as_str().unwrap();
                ensure!(
                    email.ends_with("@example.invalid"),
                    "non-synthetic fixture email"
                );
                let identity = if a["provider"] == "claude" {
                    Identity::Claude {
                        account_uuid: format!("synthetic-r5-{ref_id}"),
                        organization_uuid: Some("synthetic-r5-org".into()),
                        email: email.into(),
                    }
                } else {
                    Identity::Codex {
                        account_id: format!("synthetic-r5-{ref_id}"),
                        email: email.into(),
                        user_id: None,
                    }
                };
                let id = identity.id();
                ids.insert(ref_id.into(), id.clone());
                let account: Account = serde_json::from_value(
                    json!({"schema":1,"id":id,"provider":a["provider"],
                    "identity":identity,"email":email,"label":a["label"].as_str().unwrap_or(""),"plan":a["plan"],
                    "source":{"kind":"oauth"},"user_state":a["user_state"],"health":a["health"],
                    "quota_cooling":a.get("quota_cooling"),"allow_credits":a["allow_credits"],"drain":false,
                    "generation":0,"token_meta":{"expires_at":"2099-01-01T00:00:00Z","last_refresh_at":null,"refresh_uncertain":false}}),
                )?;
                // A uniform recorded shift provides runway without faking now.
                if let Some(until) = a["health"]["until"].as_str() {
                    ensure!(
                        until.parse::<DateTime<Utc>>()? > Utc::now(),
                        "shifted reference reset expired unexpectedly"
                    );
                }
                // Derive expected post-poll health with the actual public core
                // reducer. Never substitute render-only health for core health.
                let mut expected = account.clone();
                if a["user_state"] != "disabled" && a["health"]["kind"] != "needs_relogin" {
                    // Match usage(): Codex polls omit per-window status. In
                    // particular the 99% bootstrap must not inherit rejected.
                    let mut windows = a["windows"].clone();
                    if a["provider"] == "codex" {
                        for w in windows.as_array_mut().context("expected windows")? {
                            w.as_object_mut()
                                .context("expected window")?
                                .remove("status");
                        }
                    }
                    expected.apply_health_signal(HealthSignal::UsageSnapshot {
                        snapshot: json!({"windows":windows,"credits":a["credits"],"signals":{}}),
                        model: None,
                        projected_allowance: 0.0,
                    }, Utc::now());
                }
                expected_health.insert(id.clone(), serde_json::to_value(&expected.health)?);
                store
                    .save(
                        &account,
                        &Tokens {
                            access_token: format!("synthetic-r5-access-{id}"),
                            refresh_token: format!("synthetic-r5-refresh-{id}"),
                            id_token: None,
                        },
                        None,
                    )
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                upstream_accounts.insert(id.clone(), a.clone());
                // Actual documented schema-1 persistence, BEFORE core lease.
                // Three independent session groups, two recent and one stale.
                for n in 0..3 {
                    let recent = n < a["active_sessions"].as_u64().unwrap_or(0);
                    let last = if recent {
                        Utc::now()
                    } else {
                        Utc::now() - chrono::Duration::minutes(20)
                    };
                    bindings.push(json!({"key":format!("{}:r5-{id}-{n}",a["provider"].as_str().unwrap()),"account":id,"last_used":last}));
                }
            }
        } // Release AccountStore before spawned core acquires it.
        fs::create_dir_all(home.join("state"))?;
        fs::write(
            home.join("state/bindings.json"),
            serde_json::to_vec(&json!({"schema":1,"bindings":bindings}))?,
        )?;
        let upstream = Upstream::start(upstream_accounts, home.join("upstream-metadata.jsonl"))?;
        let data = listener()?;
        let admin = listener()?;
        let dp = data.local_addr()?.port();
        let ap = admin.local_addr()?.port();
        let up = upstream.port;
        fs::write(
            home.join("config.toml"),
            format!(
                "[server]\ndata_port={dp}\nadmin_port={ap}\ndrain_timeout_secs=1\n[upstream]\nanthropic_base='http://127.0.0.1:{up}'\ncodex_base='http://127.0.0.1:{up}'\nclaude_usage_base='http://127.0.0.1:{up}'\ncodex_usage_base='http://127.0.0.1:{up}'\n[tray]\ntheme='light'\n"
            ),
        )?;
        let mut live = Self {
            tray: None,
            core: None,
            _upstream: upstream,
            home,
            admin: ap,
            data: dp,
            desired,
            expected_health,
            fixture_clock: clock_record,
            ids,
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()?,
            started: Instant::now(),
            session: Session::default(),
            overlay: g::Overlay::None,
        };
        let cb = binary(&root, "R5_CORE_BIN", "quota-baton.exe")?;
        let tb = binary(&root, "R5_TRAY_BIN", "quota-baton-tray.exe")?;
        drop(data);
        drop(admin);
        live.core = Some(
            child(&cb, &live.home, "core")?
                .env("QB_TEST_LOGIN_BASE", format!("http://127.0.0.1:{up}"))
                .arg("serve")
                .spawn()?,
        );
        live.until(STARTUP, "core PID health", |s| {
            Ok(s.api("GET", "/admin/v1/health", None)
                .is_ok_and(|v| v["ok"] == true && v["pid"] == s.core.as_ref().unwrap().id()))
        })?;
        // Use the real administrative poll rather than depending on which
        // accounts the startup scheduler reaches before its work budget ends.
        // 429 means a startup poll is running or its real interval is active;
        // readiness below must still observe all exact expected quota fields.
        let mut polls = vec![];
        for a in live.desired["accounts"].as_array().context("accounts")? {
            if a["user_state"] == "disabled" || a["health"]["kind"] == "needs_relogin" {
                continue;
            }
            let id = &live.ids[a["id"].as_str().context("fixture id")?];
            let token = fs::read_to_string(live.home.join("admin.token"))?;
            let status = live.rt.block_on(async {
                live.client
                    .post(format!(
                        "http://127.0.0.1:{}/admin/v1/accounts/{id}/refresh-usage",
                        live.admin
                    ))
                    .bearer_auth(token.trim())
                    .send()
                    .await
                    .map(|r| r.status())
            })?;
            polls.push(json!({"account_ref":a["id"],"status":status.as_u16()}));
            fs::write(
                live.home.join("startup-polls.json"),
                serde_json::to_vec_pretty(&polls)?,
            )?;
            ensure!(
                status.is_success() || status.as_u16() == 429,
                "startup refresh-usage account {id}: status {}; metadata: {}",
                status.as_u16(),
                live.home.display()
            );
        }
        // Wait complete expected poll results BEFORE launching the visible tray.
        let mut readiness = String::new();
        live.until(
            WAIT,
            "all provider usage and seeded metadata",
            |s| match s.ready() {
                Ok(()) => Ok(true),
                Err(e) => {
                    readiness = e.to_string();
                    Ok(false)
                }
            },
        )
        .with_context(|| {
            format!(
                "readiness metadata: {readiness}; fixture clock: {}",
                live.fixture_clock
            )
        })?;
        if name == "credits" {
            let id = live.ids["codex-acct-alpha"].clone();
            ensure!(
                live.account(&id)?["health"]["kind"] == "ready",
                "99% allowed bootstrap must be core-ready before real paid request"
            );
            live.data_request("r5-paid", "codex")?;
            live.until(INPUT, "real C9 paid-serving state", |s| {
                let a = s.account(&id)?;
                Ok(a["credits_in_use"] == true
                    && a["windows"].as_array().is_some_and(|ws| {
                        ws.iter()
                            .any(|w| w["name"] == "5h" && w["used_pct"].as_f64() == Some(99.))
                    }))
            })?;
        }
        let state = live.state()?;
        let view = v2::project(&state.to_string(), SystemTime::now())?;
        live.session.show(&view);
        let mut cmd = child(&tb, &live.home, "tray")?;
        cmd.arg("--no-supervise")
            .arg("--open-flyout-live")
            .env("QB_TRAY_TEST_NO_BROWSER", "1")
            .env("QB_TRAY_TEST_NO_EXPLORER", "1")
            .env("QB_TRAY_TEST_NO_CLIPBOARD", "1")
            .env("QB_TRAY_TEST_KEEP_OPEN", "1")
            .env("QB_TRAY_TEST_SHELL_LOG", live.home.join("trace.jsonl"))
            .env("QB_TRAY_TEST_LOGIN_LOG", live.home.join("login.log"))
            .env("QB_TRAY_TEST_DELAY_FILE", live.home.join("action-delay"))
            .env(
                "QB_TRAY_TEST_FILE_LAUNCH_LOG",
                live.home.join("file-launch.json"),
            )
            .env("QB_TRAY_TEST_EXPLORER_LOG", live.home.join("launch.log"));
        live.tray = Some(cmd.spawn()?);
        live.wait_paint(0, "list")?;
        Ok(live)
    }
    fn alive(&mut self) -> Result<()> {
        ensure!(
            self.started.elapsed() < CASE_BUDGET,
            "R5 case watchdog exceeded; {}",
            self.home.display()
        );
        for c in [&mut self.core, &mut self.tray].into_iter().flatten() {
            ensure!(
                c.try_wait()?.is_none(),
                "owned child exited; {}",
                self.home.display()
            );
        }
        Ok(())
    }
    fn until(
        &mut self,
        duration: Duration,
        why: &str,
        mut check: impl FnMut(&Self) -> Result<bool>,
    ) -> Result<()> {
        let end = Instant::now() + duration;
        loop {
            self.alive()?;
            if check(self)? {
                return Ok(());
            }
            ensure!(
                Instant::now() < end,
                "timeout: {why}; {}",
                self.home.display()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn api(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let token = fs::read_to_string(self.home.join("admin.token"))?;
        self.rt.block_on(async {
            let mut r = self
                .client
                .request(
                    method.parse()?,
                    format!("http://127.0.0.1:{}{path}", self.admin),
                )
                .bearer_auth(token.trim());
            if let Some(body) = body {
                r = r.json(&body);
            }
            let response = r.send().await?;
            ensure!(
                response.status().is_success(),
                "admin {method} {path}: {}",
                response.status()
            );
            Ok(response.json().await?)
        })
    }
    fn state(&self) -> Result<Value> {
        self.api("GET", "/admin/v1/state", None)
    }
    fn account(&self, id: &str) -> Result<Value> {
        self.state()?["accounts"]
            .as_array()
            .context("accounts")?
            .iter()
            .find(|a| a["id"] == id)
            .cloned()
            .context("account missing")
    }
    fn ready(&self) -> Result<()> {
        let state = self.state().context("readiness state unavailable")?;
        let actual = state["accounts"]
            .as_array()
            .context("readiness accounts missing")?;
        ensure!(
            actual.len() == self.ids.len(),
            "account count actual={} expected={}",
            actual.len(),
            self.ids.len()
        );
        for want in self.desired["accounts"].as_array().unwrap() {
            let id = &self.ids[want["id"].as_str().unwrap()];
            let a = actual
                .iter()
                .find(|a| a["id"] == *id)
                .with_context(|| format!("missing synthetic account {id}"))?;
            if a["user_state"] != want["user_state"]
                || a["plan"] != want["plan"]
                || a["health"] != self.expected_health[id]
                || a["active_sessions"]
                    .as_u64()
                    .zip(want["active_sessions"].as_u64())
                    .is_none_or(|(a, e)| a != e)
                || a["bound_sessions"].as_u64() != Some(3)
            {
                bail!(
                    "account {id}: user_state actual={} expected={}; plan actual={} expected={}; health actual={} expected={}; active_sessions actual={} expected={}; bound_sessions actual={} expected=3",
                    a["user_state"],
                    want["user_state"],
                    a["plan"],
                    want["plan"],
                    a["health"],
                    self.expected_health[id],
                    a["active_sessions"],
                    want["active_sessions"],
                    a["bound_sessions"]
                );
            }
            if want["user_state"] == "disabled" || want["health"]["kind"] == "needs_relogin" {
                continue;
            }
            for w in want["windows"].as_array().unwrap() {
                let reset = w["reset_at"]
                    .as_str()
                    .context("expected window reset missing")?
                    .parse::<DateTime<Utc>>()
                    .context("expected window reset invalid")?;
                if !a["windows"].as_array().is_some_and(|ws| {
                    ws.iter().any(|v| {
                        v["name"] == w["name"]
                            && same_number(&v["used_pct"], &w["used_pct"])
                            && v["reset_at"]
                                .as_str()
                                .and_then(|s| s.parse::<DateTime<Utc>>().ok())
                                == Some(reset)
                    })
                }) {
                    let got = a["windows"]
                        .as_array()
                        .and_then(|ws| ws.iter().find(|v| v["name"] == w["name"]));
                    bail!(
                        "account {id} window {}: used_pct actual={} expected={}; reset_at actual={} expected={}",
                        w["name"],
                        got.map_or(&Value::Null, |v| &v["used_pct"]),
                        w["used_pct"],
                        got.map_or(&Value::Null, |v| &v["reset_at"]),
                        w["reset_at"]
                    );
                }
            }
            if want["credits"].is_object()
                && !same_number(&a["credits"]["balance"], &want["credits"]["balance"])
            {
                bail!(
                    "account {id}: credits balance actual={} expected={}",
                    a["credits"]["balance"],
                    want["credits"]["balance"]
                );
            }
        }
        Ok(())
    }
    fn data_request(&self, session: &str, provider: &str) -> Result<()> {
        let key = fs::read_to_string(self.home.join("client.key"))?;
        self.rt.block_on(async{
            let path=if provider=="codex"{"/v1/responses"}else{"/v1/messages"};
            let r=self.client.post(format!("http://127.0.0.1:{}{path}",self.data)).bearer_auth(key.trim())
                .header("session-id",session).header("x-claude-code-session-id",session)
                .json(&json!({"model":"synthetic-r5","input":"synthetic","messages":[{"role":"user","content":"synthetic"}],"max_tokens":1})).send().await?;
            ensure!(r.status().is_success(),"real relay request failed: {}",r.status()); let _=r.bytes().await?; Ok(())
        })
    }
    fn trace(&self) -> Result<Value> {
        let path = self.home.join("trace.jsonl");
        if !path.exists() {
            return Ok(Value::Null);
        }
        let bytes = fs::read(path)?;
        ensure!(bytes.len() < 4 * 1024 * 1024, "trace budget");
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
            return Ok(Value::Null);
        };
        let mut latest = Value::Null;
        for line in bytes[..end]
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
        {
            let v: Value = serde_json::from_slice(line)?;
            if v["event"] == "paint" && v["sequence"].as_u64() > latest["sequence"].as_u64() {
                latest = v;
            }
        }
        Ok(latest)
    }
    fn wait_paint(&mut self, epoch: u64, state: &str) -> Result<Value> {
        self.until(WAIT, "new painted epoch/state", |s| {
            let t = s.trace()?;
            Ok(t["visible"] == true
                && t["state"] == state
                && t["scene"]["epoch"].as_u64().unwrap_or(0) > epoch)
        })?;
        self.trace()
    }
    fn hwnd(&self) -> Result<HWND> {
        let mut after = None;
        let expected = self.tray.as_ref().context("tray missing")?.id();
        loop {
            let h = unsafe { FindWindowExW(None, after, w!("QuotaBatonTrayFlyout"), None) }?;
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(h, Some(&mut pid));
            }
            if pid == expected {
                return Ok(h);
            }
            after = Some(h);
        }
    }
    fn repaint(&mut self) -> Result<Value> {
        self.repaint_within(INPUT)
    }
    fn repaint_within(&mut self, budget: Duration) -> Result<Value> {
        let epoch = self.trace()?["scene"]["epoch"].as_u64().unwrap_or(0);
        let h = self.hwnd()?;
        unsafe {
            ensure!(
                RedrawWindow(Some(h), None, None, RDW_INVALIDATE | RDW_UPDATENOW).as_bool(),
                "redraw"
            );
            let _ = UpdateWindow(h);
        }
        self.until(budget, "WM repaint epoch", |s| {
            Ok(s.trace()?["scene"]["epoch"].as_u64().unwrap_or(0) > epoch)
        })?;
        self.trace()
    }
    fn scene(&mut self) -> Result<g::Scene> {
        let deadline = Instant::now() + INPUT;
        loop {
            self.alive()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.scene_attempt(remaining) {
                Ok(scene) => return Ok(scene),
                Err(error) => {
                    ensure!(
                        Instant::now() < deadline,
                        "timeout: coherent scene snapshot; last diagnostic: {error:#}; {}",
                        self.home.display()
                    );
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn scene_attempt(&mut self, budget: Duration) -> Result<g::Scene> {
        let trace = self.repaint_within(budget)?;
        let state = self.state()?;
        let view = v2::project(&state.to_string(), SystemTime::now())?;
        // Failed observations must not remove/reappend frozen slots or clear
        // pending state. Stage only slot reconciliation, then commit on success.
        let mut candidate = Session::default();
        candidate.slots.clone_from(&self.session.slots);
        candidate.open = self.session.open;
        candidate.update(&view);
        let mut p = g::Presentation {
            expanded: self.session.expansion.clone(),
            overlay: self.overlay.clone(),
            offset: trace["scene"]["offset"].as_f64().context("trace offset")? as f32,
            ..Default::default()
        };
        for slot in &candidate.slots {
            let sec = p.frozen.entry(slot.provider.clone()).or_default();
            if slot.disabled_on_open {
                sec.disabled_rows.push(slot.id.clone())
            } else {
                sec.rows.push(slot.id.clone())
            }
        }
        Renderer::new(false)?.measure(&view, &mut p)?;
        let mut client = RECT::default();
        let h = self.hwnd()?;
        unsafe {
            GetClientRect(h, &mut client)?;
        }
        let dpi = unsafe { GetDpiForWindow(h) };
        let scale = dpi as f32 / 96.;
        let height = (client.bottom - client.top) as f32 / scale;
        ensure!(
            (height - trace["scene"]["height"].as_f64().context("trace height")? as f32).abs()
                <= 1.,
            "client/trace height mismatch: client={height} trace={} dpi={} epoch={} sequence={}",
            trace["scene"]["height"],
            dpi,
            trace["scene"]["epoch"],
            trace["sequence"]
        );
        let scene = g::Scene::build(&view, p, height);
        ensure!(
            trace["scene"]["expanded"] == serde_json::to_value(&self.session.expansion)?,
            "painted expansion disagrees with intended clicks: actual={} expected={:?} input={} epoch={}",
            trace["scene"]["expanded"],
            self.session.expansion,
            trace["input"],
            trace["scene"]["epoch"]
        );
        ensure!(
            scene.targets.len()
                == trace["scene"]["targets"]
                    .as_u64()
                    .context("trace targets")? as usize,
            "rebuilt target count mismatch: actual={} rebuilt={} expansion={:?} overlay={:?} offset={} height={height} epoch={} actual_expansion={} input={}; snapshot/prefs/overlay stale",
            trace["scene"]["targets"],
            scene.targets.len(),
            self.session.expansion,
            self.overlay,
            trace["scene"]["offset"],
            trace["scene"]["epoch"],
            trace["scene"]["expanded"],
            trace["input"]
        );
        let latest = self.trace()?;
        let mut latest_client = RECT::default();
        unsafe {
            GetClientRect(h, &mut latest_client)?;
        }
        ensure!(
            ["owner", "revision", "stamp"]
                .iter()
                .all(|key| trace["scene"][*key].as_u64().is_some()
                    && latest["scene"][*key] == trace["scene"][*key])
                && (client.left, client.top, client.right, client.bottom)
                    == (
                        latest_client.left,
                        latest_client.top,
                        latest_client.right,
                        latest_client.bottom
                    )
                && unsafe { GetDpiForWindow(h) } == dpi,
            "scene changed during reconstruction: sequence {}->{} epoch {}->{} client ({},{},{},{})->({},{},{},{})",
            trace["sequence"],
            latest["sequence"],
            trace["scene"]["epoch"],
            latest["scene"]["epoch"],
            client.left,
            client.top,
            client.right,
            client.bottom,
            latest_client.left,
            latest_client.top,
            latest_client.right,
            latest_client.bottom
        );
        self.session.update(&view);
        Ok(scene)
    }
    fn hit(&mut self, target: g::Target, left_edge: bool) -> Result<()> {
        let scene = self.scene()?;
        let hit = scene
            .targets
            .iter()
            .find(|h| h.target == target && h.enabled)
            .context("enabled current Scene target missing")?;
        let x = if left_edge {
            hit.rect.left + 2.
        } else {
            (hit.rect.left + hit.rect.right) / 2.
        };
        let y = (hit.rect.top + hit.rect.bottom) / 2.;
        let resolved = scene.hit_test(x, y).context("scene target clipped")?;
        ensure!(*resolved == target, "button precedence mismatch");
        mouse(self.hwnd()?, WM_LBUTTONUP, x, y)
    }
    fn scroll(&mut self, down: bool) -> Result<()> {
        send(
            self.hwnd()?,
            WM_MOUSEWHEEL,
            WPARAM(((if down { -120i16 } else { 120i16 }) as u16 as usize) << 16),
            LPARAM(0),
        )?;
        self.repaint()?;
        Ok(())
    }
    fn top(&mut self) -> Result<()> {
        for _ in 0..24 {
            if self.scene()?.offset < 0.1 {
                return Ok(());
            }
            self.scroll(false)?;
        }
        bail!("list did not return to top within bounded wheel inputs")
    }
    fn reveal(&mut self, target: &g::Target) -> Result<()> {
        let deadline = Instant::now() + INPUT;
        let mut scrolls = 0;
        loop {
            let scene = self.scene()?;
            if scene
                .targets
                .iter()
                .any(|hit| &hit.target == target && hit.enabled)
            {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline && scrolls < 24,
                "target {target:?} did not become visible/enabled; scrolls={scrolls}, offset={}, targets={:?}",
                scene.offset,
                scene
                    .targets
                    .iter()
                    .map(|h| (&h.target, h.enabled))
                    .collect::<Vec<_>>()
            );
            // A visible disabled target is pending, not outside the viewport.
            if scene.targets.iter().any(|hit| &hit.target == target) {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            let rect = match target {
                g::Target::Row(id) => scene.row(id).map(|r| r.rect),
                g::Target::Inline { id, .. } => scene
                    .row(id)
                    .and_then(|r| r.button.as_ref())
                    .filter(|b| &b.target == target)
                    .map(|b| b.hit),
                g::Target::Disclosure(provider) => scene
                    .sections
                    .iter()
                    .find(|s| &s.provider == provider)
                    .and_then(|s| s.disclosure),
                _ => None,
            };
            if let Some(rect) = rect
                && (rect.top < scene.list.top || rect.bottom > scene.list.bottom)
            {
                self.scroll(rect.bottom > scene.list.bottom)?;
                scrolls += 1;
                continue;
            }
            // Backend success can precede the worker's checked-state frame.
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn click(&mut self, target: g::Target, overlay: g::Overlay, state: &str) -> Result<()> {
        self.reveal(&target)?;
        let epoch = self.trace()?["scene"]["epoch"].as_u64().unwrap_or(0);
        let disclosure = if let g::Target::Disclosure(provider) = &target {
            Some((
                provider.clone(),
                !self
                    .session
                    .expansion
                    .get(provider)
                    .copied()
                    .unwrap_or(false),
            ))
        } else {
            None
        };
        self.hit(target, false)?;
        if let Some((provider, expanded)) = disclosure {
            self.session.set_expanded(&provider, expanded);
        }
        self.overlay = overlay;
        self.wait_paint(epoch, state)?;
        Ok(())
    }
    fn esc(&mut self, overlay: g::Overlay, state: &str) -> Result<()> {
        let epoch = self.trace()?["scene"]["epoch"].as_u64().unwrap_or(0);
        send(self.hwnd()?, WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
        self.overlay = overlay;
        self.wait_paint(epoch, state)?;
        Ok(())
    }
    fn capture(&mut self, path: &Path) -> Result<()> {
        let trace = self.repaint()?;
        let h = self.hwnd()?;
        let (width, height, pixels) = pixels(h)?;
        let dpi = unsafe { GetDpiForWindow(h) };
        ensure!(
            width == (360 * dpi).div_ceil(96),
            "live pixel width {width}, DPI {dpi}"
        );
        ensure!(
            trace["visible"] == true && trace["scene"]["epoch"].as_u64().unwrap_or(0) > 0,
            "missing painted live epoch"
        );
        let i = (8 * width as usize + 8) * 4;
        ensure!(
            pixels[i..i + 3] == [255, 255, 255],
            "PrintWindow did not capture the painted light surface"
        );
        ensure!(
            pixels
                .chunks_exact(4)
                .any(|p| p[0] < 128 && p[1] < 128 && p[2] < 128),
            "captured window has no painted text"
        );
        fs::write(path, png(width, height, &pixels))?;
        fs::write(
            path.with_extension("json"),
            serde_json::to_vec_pretty(
                &json!({"kind":"live-PrintWindow-client","pid":self.tray.as_ref().unwrap().id(),"dpi":dpi,"width":width,"height":height,"trace":trace,"state":self.state()?,"fixture_home":self.home,"fixture_clock":self.fixture_clock}),
            )?,
        )?;
        Ok(())
    }
}

fn send(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Result<()> {
    let mut result = 0;
    let started = Instant::now();
    let sent =
        unsafe { SendMessageTimeoutW(h, msg, wp, lp, SMTO_ABORTIFHUNG, 1000, Some(&mut result)) }.0;
    let error = unsafe { windows::Win32::Foundation::GetLastError() }.0;
    ensure!(
        sent != 0,
        "owned window input failed: msg={msg:#x} duration_ms={} win32={error}",
        started.elapsed().as_millis()
    );
    Ok(())
}
fn mouse(h: HWND, msg: u32, x: f32, y: f32) -> Result<()> {
    let scale = unsafe { GetDpiForWindow(h) } as f32 / 96.;
    let x = i16::try_from((x * scale).round() as i32)?;
    let y = i16::try_from((y * scale).round() as i32)?;
    send(
        h,
        msg,
        WPARAM(0),
        LPARAM((((y as u16 as u32) << 16) | x as u16 as u32) as isize),
    )
}
struct Dpi(DPI_AWARENESS_CONTEXT);
impl Dpi {
    fn enter() -> Result<Self> {
        let old =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        ensure!(!old.0.is_null(), "DPI context");
        Ok(Self(old))
    }
}
impl Drop for Dpi {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

fn cursor() -> Result<POINT> {
    let mut p = POINT::default();
    unsafe {
        GetCursorPos(&mut p)?;
    }
    Ok(p)
}
struct CursorPin(RECT);
impl CursorPin {
    fn enter(point: POINT) -> Result<Self> {
        let mut previous = RECT::default();
        unsafe {
            GetClipCursor(&mut previous)?;
        }
        ensure!(
            point.x >= previous.left
                && point.x < previous.right
                && point.y >= previous.top
                && point.y < previous.bottom,
            "owned hover point lies outside existing cursor constraints"
        );
        let fixed = RECT {
            left: point.x,
            top: point.y,
            right: point.x + 1,
            bottom: point.y + 1,
        };
        unsafe {
            ClipCursor(Some(&fixed))?;
        }
        Ok(Self(previous))
    }
}
impl Drop for CursorPin {
    fn drop(&mut self) {
        unsafe {
            let _ = ClipCursor(Some(&self.0));
        }
    }
}
struct CursorRestore(POINT);
impl Drop for CursorRestore {
    fn drop(&mut self) {
        unsafe {
            let _ = SetCursorPos(self.0.x, self.0.y);
        }
    }
}
struct OwnedTopmost {
    hwnd: HWND,
    was_topmost: bool,
}
impl OwnedTopmost {
    fn enter(live: &Live) -> Result<Self> {
        // hwnd() checks the spawned tray PID. Never activate or send input to
        // whichever unrelated app happens to cover the desktop test window.
        let hwnd = live.hwnd()?;
        let guard = Self {
            hwnd,
            was_topmost: unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST.0
                != 0,
        };
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER,
            )?;
        }
        Ok(guard)
    }
}
impl Drop for OwnedTopmost {
    fn drop(&mut self) {
        if !self.was_topmost {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER,
                );
            }
        }
    }
}
fn assert_owned_hover(hwnd: HWND, point: POINT) -> Result<()> {
    assert_cursor(point)?;
    ensure!(
        unsafe { WindowFromPoint(point) } == hwnd,
        "physical hover is not over the owned flyout at ({}, {}); no outside-window input sent",
        point.x,
        point.y
    );
    Ok(())
}
fn assert_cursor(p: POINT) -> Result<()> {
    let now = cursor()?;
    let mut clip = RECT::default();
    unsafe {
        GetClipCursor(&mut clip)?;
    }
    ensure!(
        (now.x, now.y) == (p.x, p.y),
        "actual cursor moved during stationary-tooltip regression: expected=({},{}) actual=({},{}) clip=({},{},{},{})",
        p.x,
        p.y,
        now.x,
        now.y,
        clip.left,
        clip.top,
        clip.right,
        clip.bottom
    );
    Ok(())
}
fn assert_tooltip_rect(trace: &Value, expected: Rect) -> Result<()> {
    let wanted = [expected.left, expected.top, expected.right, expected.bottom];
    ensure!(
        trace["scene"]["tooltip_rect"]
            .as_array()
            .is_some_and(|actual| {
                actual.len() == wanted.len()
                    && actual
                        .iter()
                        .zip(wanted)
                        .all(|(a, e)| a.as_f64().is_some_and(|a| (a - e as f64).abs() <= 1.))
            }),
        "painted tooltip ROI mismatch: actual={} expected={wanted:?} input={} epoch={}",
        trace["scene"]["tooltip_rect"],
        trace["input"],
        trace["scene"]["epoch"]
    );
    Ok(())
}
fn changed_roi(
    width: u32,
    height: u32,
    before: &[u8],
    after: &[u8],
    roi: Rect,
    scale: f32,
) -> Result<usize> {
    ensure!(
        before.len() == after.len() && before.len() == width as usize * height as usize * 4,
        "ROI capture extent changed"
    );
    let (left, top, right, bottom) = (
        (roi.left * scale).ceil() as i32,
        (roi.top * scale).ceil() as i32,
        (roi.right * scale).floor() as i32,
        (roi.bottom * scale).floor() as i32,
    );
    ensure!(
        left >= 0
            && top >= 0
            && right <= width as i32
            && bottom <= height as i32
            && right > left
            && bottom > top,
        "tooltip ROI outside actual client"
    );
    let mut changed = 0;
    for y in top..bottom {
        for x in left..right {
            let i = (y as usize * width as usize + x as usize) * 4;
            if before[i..i + 3] != after[i..i + 3] {
                changed += 1;
            }
        }
    }
    Ok(changed)
}

// PrintWindow/GDI mechanics adapted from m6c_live. No desktop capture fallback,
// external encoder, fabricated image, render-only bypass, or extra dependency.
fn pixels(h: HWND) -> Result<(u32, u32, Vec<u8>)> {
    let mut r = RECT::default();
    unsafe {
        GetClientRect(h, &mut r)?;
    }
    let (width, height) = (r.right - r.left, r.bottom - r.top);
    ensure!(width > 0 && height > 0, "client extent");
    unsafe {
        let dc = GetDC(None);
        ensure!(!dc.0.is_null(), "GetDC");
        let mem = CreateCompatibleDC(Some(dc));
        let bmp = CreateCompatibleBitmap(dc, width, height);
        let old = SelectObject(mem, bmp.into());
        let result = (|| -> Result<Vec<u8>> {
            ensure!(!mem.0.is_null() && !bmp.0.is_null(), "GDI allocation");
            ensure!(
                PrintWindow(h, mem, Default::default()).as_bool(),
                "live PrintWindow failed"
            );
            SelectObject(mem, old);
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut p = vec![0; width as usize * height as usize * 4];
            ensure!(
                GetDIBits(
                    mem,
                    bmp,
                    0,
                    height as u32,
                    Some(p.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS
                ) == height,
                "GetDIBits"
            );
            for p in p.chunks_exact_mut(4) {
                p.swap(0, 2);
                p[3] = 255;
            }
            Ok(p)
        })();
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(None, dc);
        Ok((width as u32, height as u32, result?))
    }
}
// PNG with filter 0 and RFC1950 stored DEFLATE blocks. CRC32 + Adler32.
fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = !0u32;
        for b in kind.iter().chain(data) {
            crc ^= *b as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320u32.wrapping_mul(crc & 1));
            }
        }
        out.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut raw = Vec::new();
    for row in rgba.chunks_exact(width as usize * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let count = raw.len().div_ceil(65535);
    for (i, b) in raw.chunks(65535).enumerate() {
        z.push(u8::from(i + 1 == count));
        let n = b.len() as u16;
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(!n).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for x in raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn evidence() -> Result<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let p = std::env::var_os("R5_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".sandbox/reports/R5-evidence"));
    ensure!(
        p.is_absolute()
            && !p
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
        "absolute evidence path required"
    );
    // Validate existing ancestor BEFORE creating a supplied path (junction safe).
    let ancestor = p
        .ancestors()
        .find(|p| p.exists())
        .context("evidence ancestor")?
        .canonicalize()?;
    ensure!(
        ancestor.starts_with(root.join(".sandbox").canonicalize()?),
        "evidence must stay under project .sandbox"
    );
    fs::create_dir_all(&p)?;
    let p = p.canonicalize()?;
    ensure!(
        p.starts_with(root.join(".sandbox").canonicalize()?),
        "evidence escaped sandbox"
    );
    Ok(p)
}

#[test]
#[ignore = "manual Windows desktop; spawned synthetic debug core/tray; coordinator serial only"]
fn r5_live_nine_reference_scenes() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _dpi = Dpi::enter()?;
    let out = evidence()?;
    let mut failures = vec![];
    // Continue independent scenes after a failure; nine live successes required.
    for (number, name) in CASES {
        let result = (|| -> Result<()> {
            let mut live = Live::start(name)?;
            if name == "overview" {
                let scene = live.scene()?;
                ensure!(
                    scene.sections.iter().all(|s| s.expanded),
                    "blocked providers must initially auto-expand"
                );
                live.click(
                    g::Target::Disclosure("codex".into()),
                    g::Overlay::None,
                    "list",
                )?;
                live.top()?;
                fs::write(
                    out.join("01-interaction.txt"),
                    "Both blocked providers auto-expanded. Clicked the live Codex disclosure once to match reference overview.\n",
                )?;
            }
            if name == "menu" {
                // S.menu intentionally collapses BOTH blocked sections.
                for provider in ["claude", "codex"] {
                    live.click(
                        g::Target::Disclosure(provider.into()),
                        g::Overlay::None,
                        "list",
                    )?;
                }
                live.top()?;
                fs::write(
                    out.join("05-interaction.txt"),
                    "Clicked both live disabled-group disclosures closed before opening the Codex acct-alpha row menu, matching S.menu.\n",
                )?;
                let id = live.ids["codex-acct-alpha"].clone();
                live.click(g::Target::Row(id.clone()), g::Overlay::RowMenu(id), "menu")?;
            }
            if name == "quit" {
                live.click(
                    g::Target::Footer(g::FooterAction::Quit),
                    g::Overlay::Quit,
                    "quit",
                )?;
            }
            if name == "settings" {
                live.click(
                    g::Target::Footer(g::FooterAction::Settings),
                    g::Overlay::Settings,
                    "settings",
                )?;
            }
            if name == "tooltip" {
                let id = live.ids["codex-acct-alpha"].clone();
                live.reveal(&g::Target::Row(id.clone()))?;
                let scene = live.scene()?;
                let anchor = scene
                    .row(&id)
                    .context("tooltip row")?
                    .usage
                    .context("usage line")?;
                let mut delay = 400u32;
                unsafe {
                    SystemParametersInfoW(
                        SPI_GETMOUSEHOVERTIME,
                        0,
                        Some((&mut delay as *mut u32).cast()),
                        Default::default(),
                    )?;
                }
                ensure!(
                    delay <= 3000,
                    "system hover delay exceeds bounded acceptance budget"
                );
                let view = v2::project(&live.state()?.to_string(), SystemTime::now())?;
                let text = &view
                    .sections
                    .iter()
                    .flat_map(|s| &s.rows)
                    .find(|r| r.raw.id == id)
                    .context("tooltip model")?
                    .usage
                    .as_ref()
                    .context("tooltip usage")?
                    .tooltip;
                let tip = Renderer::new(false)?.tooltip(text, anchor, scene.rect)?;
                ensure!(
                    tip.rect.top >= anchor.bottom + 13.9,
                    "stationary tooltip needs room below usage, not a flipped/overlapping ROI"
                );
                let _restore_cursor = CursorRestore(cursor()?);
                let topmost = OwnedTopmost::enter(&live)?;
                fs::write(
                    out.join("09-hover-provenance.json"),
                    serde_json::to_vec_pretty(&json!({
                        "scope":"test-only owned spawned tray HWND", "pid":live.tray.as_ref().context("tray")?.id(),
                        "was_topmost":topmost.was_topmost, "requested_topmost":true,
                        "activate":false, "restore":"Drop restores original topmost membership",
                        "physical_hover_verified":false
                    }))?,
                )?;
                let scale = unsafe { GetDpiForWindow(live.hwnd()?) } as f32 / 96.;
                // Establish the baseline before arming hover. Painting/capture
                // can exceed SPI delay even on a correct native implementation.
                let mut away = POINT {
                    x: (2. * scale).round() as i32,
                    y: (2. * scale).round() as i32,
                };
                unsafe {
                    ensure!(
                        ClientToScreen(live.hwnd()?, &mut away).as_bool(),
                        "baseline cursor mapping"
                    );
                    SetCursorPos(away.x, away.y)?;
                }
                mouse(live.hwnd()?, WM_MOUSEMOVE, 2., 2.)?;
                let baseline_trace = live.repaint()?;
                ensure!(
                    baseline_trace["scene"]["tooltip_rect"].is_null(),
                    "cursor-away baseline still has painted tooltip: rect={} input={}",
                    baseline_trace["scene"]["tooltip_rect"],
                    baseline_trace["input"]
                );
                let (width, height, before) = pixels(live.hwnd()?)?;
                assert_owned_hover(topmost.hwnd, away)?;
                let mut screen = POINT {
                    x: ((anchor.left + 4.) * scale).round() as i32,
                    y: ((anchor.top + anchor.bottom) / 2. * scale).round() as i32,
                };
                unsafe {
                    ensure!(
                        ClientToScreen(live.hwnd()?, &mut screen).as_bool(),
                        "cursor client mapping"
                    );
                    SetCursorPos(screen.x, screen.y)?;
                }
                assert_owned_hover(topmost.hwnd, screen)?;
                fs::write(
                    out.join("09-hover-provenance.json"),
                    serde_json::to_vec_pretty(&json!({
                        "scope":"test-only owned spawned tray HWND", "pid":live.tray.as_ref().context("tray")?.id(),
                        "was_topmost":topmost.was_topmost, "requested_topmost":true,
                        "activate":false, "restore":"Drop restores original topmost membership",
                        "physical_hover_verified":true, "cursor_constraint":"scoped ClipCursor at owned hover point, prior clip and cursor restored", "verification":"WindowFromPoint(cursor) == owned HWND",
                        "cursor_screen":{"x":screen.x,"y":screen.y}, "fixture_clock":live.fixture_clock
                    }))?,
                )?;
                let _pin = CursorPin::enter(screen)?;
                mouse(
                    live.hwnd()?,
                    WM_MOUSEMOVE,
                    anchor.left + 4.,
                    (anchor.top + anchor.bottom) / 2.,
                )?;
                // Real hover input + OS delay; never set renderer review state.
                thread::sleep(Duration::from_millis(delay as u64 + 350));
                let hover_trace = live.repaint()?;
                assert_owned_hover(topmost.hwnd, screen)?;
                assert_tooltip_rect(&hover_trace, tip.rect)?;
                let (after_width, after_height, after) = pixels(live.hwnd()?)?;
                ensure!(
                    (width, height) == (after_width, after_height),
                    "tooltip appearance client extent changed"
                );
                let appearance_roi = Rect {
                    left: tip.rect.left + 3.,
                    top: tip.rect.top + 3.,
                    right: tip.rect.right - 3.,
                    bottom: tip.rect.bottom - 3.,
                };
                let changed = changed_roi(width, height, &before, &after, appearance_roi, scale)?;
                ensure!(
                    changed >= 64,
                    "real hover timer did not paint measured tooltip region: changed={changed} required=64 delay_ms={delay} baseline_cursor=({}, {}) hover_cursor=({}, {}) ROI={appearance_roi:?}",
                    away.x,
                    away.y,
                    screen.x,
                    screen.y
                );
                // Preserve reference PNG09 BEFORE changing its reference data.
                assert_cursor(screen)?;
                live.capture(&out.join("09-tooltip.png"))?;
                let state_before = live.state()?;
                let account = state_before["accounts"]
                    .as_array()
                    .context("accounts")?
                    .iter()
                    .find(|a| a["id"] == id)
                    .context("Codex mero")?;
                ensure!(
                    account["windows"]
                        .as_array()
                        .context("windows")?
                        .iter()
                        .any(|w| w["name"] == "7d" && w["used_pct"].as_f64() == Some(62.)),
                    "expected actual weekly usage 62 before regression"
                );
                ensure!(
                    account["allow_credits"] == false && account["credits_in_use"] == false,
                    "tooltip regression must use subscription, not paid-serving"
                );
                let view_before = v2::project(&state_before.to_string(), SystemTime::now())?;
                ensure!(
                    view_before
                        .summaries
                        .iter()
                        .find(|s| s.provider == "codex")
                        .context("Codex summary")?
                        .eligible
                        == Some(1),
                    "Codex mero must be sole eligible relay target"
                );
                // First tooltip TEXT LINE only. This excludes underlying bar,
                // summary, session counts, footer and observation-clock lines.
                let roi = Rect {
                    left: tip.rect.left + 10.,
                    top: tip.rect.top + 8.,
                    right: tip.rect.right - 10.,
                    bottom: tip.rect.top + 24.,
                };
                ensure!(
                    roi.top >= anchor.bottom + 14.,
                    "ROI includes underlying usage"
                );
                let (rw, rh, stationary_before) = pixels(live.hwnd()?)?;
                let epoch = live.trace()?["scene"]["epoch"]
                    .as_u64()
                    .context("pre-request paint epoch")?;
                // No cursor move or WM_MOUSEMOVE from here until all assertions
                // and post-update capture finish. No admin write or poll bypass.
                live._upstream.secondary_used.store(50, Ordering::Release);
                live.data_request("r5-stationary-tooltip-50", "codex")?;
                live.until(INPUT, "real C9 weekly usage 50", |s| {
                    let a = s.account(&id)?;
                    Ok(a["windows"].as_array().is_some_and(|ws| {
                        ws.iter()
                            .any(|w| w["name"] == "7d" && w["used_pct"].as_f64() == Some(50.))
                    }))
                })?;
                live.wait_paint(epoch, "list")?;
                let state_after = live.state()?;
                let latest = v2::project(&state_after.to_string(), SystemTime::now())?;
                let latest_text = &latest
                    .sections
                    .iter()
                    .flat_map(|s| &s.rows)
                    .find(|r| r.raw.id == id)
                    .context("latest Codex row")?
                    .usage
                    .as_ref()
                    .context("latest usage")?
                    .tooltip;
                ensure!(
                    latest_text.contains("50% 사용") && latest_text.contains("50% 남음"),
                    "shared latest tooltip did not project 50/50"
                );
                let latest_scene = live.scene()?;
                let latest_anchor = latest_scene
                    .row(&id)
                    .context("latest row")?
                    .usage
                    .context("latest usage box")?;
                let latest_tip =
                    Renderer::new(false)?.tooltip(latest_text, latest_anchor, latest_scene.rect)?;
                assert_tooltip_rect(&live.trace()?, latest_tip.rect)?;
                ensure!(
                    (tip.rect.left - latest_tip.rect.left).abs() < 0.1
                        && (tip.rect.top - latest_tip.rect.top).abs() < 0.1,
                    "tooltip moved; first-line ROI comparison invalid"
                );
                let mut current_screen = POINT {
                    x: ((latest_anchor.left + 4.) * scale).round() as i32,
                    y: ((latest_anchor.top + latest_anchor.bottom) / 2. * scale).round() as i32,
                };
                unsafe {
                    ensure!(
                        ClientToScreen(live.hwnd()?, &mut current_screen).as_bool(),
                        "post-request cursor mapping"
                    );
                }
                ensure!(
                    (screen.x, screen.y) == (current_screen.x, current_screen.y),
                    "window/usage anchor moved under stationary cursor"
                );
                assert_cursor(screen)?;
                // SSE/model publication may trail the first post-request paint.
                // Bounded repaint only: stale cached native text never satisfies.
                live.until(
                    INPUT,
                    "stationary live tooltip first-line pixels changed",
                    |s| {
                        assert_owned_hover(topmost.hwnd, screen)?;
                        let (w, h, p) = pixels(s.hwnd()?)?;
                        ensure!(
                            (w, h) == (rw, rh),
                            "client extent changed during stationary comparison"
                        );
                        Ok(changed_roi(w, h, &stationary_before, &p, roi, scale)? >= 8)
                    },
                )?;
                let (_, _, stationary_after) = pixels(live.hwnd()?)?;
                let updated_pixels =
                    changed_roi(rw, rh, &stationary_before, &stationary_after, roi, scale)?;
                ensure!(
                    live.account(&id)?["credits_in_use"] == false,
                    "normal usage update falsely claimed paid serving"
                );
                live.capture(&out.join("09-tooltip-after-usage50.png"))?;
                assert_owned_hover(topmost.hwnd, screen)?;
                fs::write(
                    out.join("09-stationary-regression.json"),
                    serde_json::to_vec_pretty(
                        &json!({"status":"assertions completed; visual review pending","used_before":62,"used_after":50,"header":"x-codex-secondary-used-percent","cursor_screen":{"x":screen.x,"y":screen.y},"roi_dips":{"left":roi.left,"top":roi.top,"right":roi.right,"bottom":roi.bottom},"changed_pixels":updated_pixels,"epoch_before":epoch,"trace_after":live.trace()?}),
                    )?,
                )?;
                fs::write(
                    out.join("09-interaction.txt"),
                    format!(
                        "Actual cursor + initial WM_MOUSEMOVE on usage; SPI delay {delay} ms; {changed} tooltip appearance pixels. Then actual relay C9 weekly 62->50; cursor unchanged, no additional WM_MOUSEMOVE; {updated_pixels} changed first-line tooltip pixels, excluding usage bar/statistics/clock.\n"
                    ),
                )?;
            }
            if name != "tooltip" {
                live.capture(&out.join(format!("{number}-{name}.png")))?;
            }
            if name == "relogin" {
                let scene = live.scene()?;
                let view = v2::project(&live.state()?.to_string(), SystemTime::now())?;
                let mut rows = vec![];
                for hit in &scene.targets {
                    if let g::Target::Row(id) = &hit.target {
                        let row = view
                            .sections
                            .iter()
                            .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
                            .find(|r| &r.raw.id == id)
                            .context("visible UIA row missing from fresh projection")?;
                        rows.push(row.accessible_name.clone());
                    }
                }
                let summaries: Vec<_> = view
                    .summaries
                    .iter()
                    .map(|s| s.accessible_name.clone())
                    .collect();
                let result = uia::assert_names(live.hwnd()?, &rows, &summaries)?;
                fs::write(
                    out.join("02-accessibility.json"),
                    serde_json::to_vec_pretty(&result)?,
                )?;
            }
            if name == "empty" {
                ensure!(live.scene()?.empty.is_some(), "real empty Scene absent");
            }
            Ok(())
        })();
        if let Err(e) = result {
            failures.push(format!("{number}-{name}: {e:#}"));
        }
    }
    fs::write(
        out.join("capture-results.json"),
        serde_json::to_vec_pretty(
            &json!({"failures":failures,"required":9,"status":if failures.is_empty(){"live captures complete; visual review pending"}else{"FAILED"}}),
        )?,
    )?;
    ensure!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

fn inline(id: &str, action: v2::ActionKind) -> g::Target {
    g::Target::Inline {
        id: id.into(),
        action,
    }
}
fn menu_credit(id: &str) -> g::Target {
    g::Target::Menu {
        id: Some(id.into()),
        action: g::MenuAction::ToggleCredits,
    }
}
fn assert_allow(live: &mut Live, id: &str, value: bool) -> Result<()> {
    live.until(
        INPUT,
        "authoritative allow_credits and pending completion",
        |s| Ok(s.account(id)?["allow_credits"] == value && s.trace()?["menu_pending"] == false),
    )
}
fn assert_checked(live: &mut Live, value: bool) -> Result<()> {
    live.until(INPUT, "painted checkbox check state", |s| {
        let t = s.trace()?;
        Ok(t["state"] == "menu" && t["menu_checked"] == value && t["menu_pending"] == false)
    })
}
fn unchanged_after_cancel(live: &mut Live, id: &str) -> Result<()> {
    // Check a bounded quiet interval, not just the state before queued work runs.
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(100));
        live.alive()?;
        ensure!(
            live.account(id)?["allow_credits"] == false,
            "cancel/Esc sent an unexpected credit enable"
        );
    }
    Ok(())
}

#[test]
#[ignore = "manual synthetic core/tray interactions, sequential coordinator run"]
fn r5_live_credits_every_enable_confirms_and_checkbox_left_edge() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _dpi = Dpi::enter()?;
    let mut live = Live::start("credit-actions")?;
    let id = live.ids["codex-acct-alpha"].clone();
    live.click(
        inline(&id, v2::ActionKind::AllowCredits),
        g::Overlay::CreditsConfirm(id.clone()),
        "confirm-credits",
    )?;
    assert_allow(&mut live, &id, false)?;
    live.click(g::Target::CancelDialog, g::Overlay::None, "list")?;
    unchanged_after_cancel(&mut live, &id)?;
    live.click(
        inline(&id, v2::ActionKind::AllowCredits),
        g::Overlay::CreditsConfirm(id.clone()),
        "confirm-credits",
    )?;
    live.esc(g::Overlay::None, "list")?;
    unchanged_after_cancel(&mut live, &id)?;
    live.click(
        inline(&id, v2::ActionKind::AllowCredits),
        g::Overlay::CreditsConfirm(id.clone()),
        "confirm-credits",
    )?;
    live.click(
        g::Target::ConfirmCredits(id.clone()),
        g::Overlay::None,
        "list",
    )?;
    assert_allow(&mut live, &id, true)?;
    live.data_request("r5-paid-after-confirm", "codex")?;
    live.until(INPUT, "C9 after confirmed enable", |s| {
        Ok(s.account(&id)?["credits_in_use"] == true)
    })?;
    live.click(
        inline(&id, v2::ActionKind::DisableCredits),
        g::Overlay::None,
        "list",
    )?;
    assert_allow(&mut live, &id, false)?;
    live.click(
        inline(&id, v2::ActionKind::AllowCredits),
        g::Overlay::CreditsConfirm(id.clone()),
        "confirm-credits",
    )?;
    assert_allow(&mut live, &id, false)?;
    live.esc(g::Overlay::None, "list")?;
    live.click(
        g::Target::Row(id.clone()),
        g::Overlay::RowMenu(id.clone()),
        "menu",
    )?;
    let epoch = live.trace()?["scene"]["epoch"].as_u64().unwrap();
    live.hit(menu_credit(&id), true)?;
    live.overlay = g::Overlay::CreditsConfirm(id.clone());
    live.wait_paint(epoch, "confirm-credits")?;
    assert_allow(&mut live, &id, false)?;
    live.click(
        g::Target::CancelDialog,
        g::Overlay::RowMenu(id.clone()),
        "menu",
    )?;
    unchanged_after_cancel(&mut live, &id)?;
    let epoch = live.trace()?["scene"]["epoch"].as_u64().unwrap();
    live.hit(menu_credit(&id), true)?;
    live.overlay = g::Overlay::CreditsConfirm(id.clone());
    live.wait_paint(epoch, "confirm-credits")?;
    live.click(
        g::Target::ConfirmCredits(id.clone()),
        g::Overlay::RowMenu(id.clone()),
        "menu",
    )?;
    assert_allow(&mut live, &id, true)?;
    assert_checked(&mut live, true)?;
    ensure!(
        live.trace()?["state"] == "menu",
        "checkbox confirmation must preserve menu"
    );
    let epoch = live.trace()?["scene"]["epoch"].as_u64().unwrap();
    live.hit(menu_credit(&id), true)?;
    live.wait_paint(epoch, "menu")?;
    assert_allow(&mut live, &id, false)?;
    assert_checked(&mut live, false)?;
    let epoch = live.trace()?["scene"]["epoch"].as_u64().unwrap();
    live.hit(menu_credit(&id), true)?;
    live.overlay = g::Overlay::CreditsConfirm(id.clone());
    live.wait_paint(epoch, "confirm-credits")?;
    assert_allow(&mut live, &id, false)?;
    live.esc(g::Overlay::RowMenu(id.clone()), "menu")?;
    unchanged_after_cancel(&mut live, &id)?;
    Ok(())
}

#[test]
#[ignore = "manual synthetic core/tray Enable order-freeze test"]
fn r5_live_mouse_enable_stays_in_disabled_slot_until_reopen() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _dpi = Dpi::enter()?;
    let mut live = Live::start("overview")?;
    let id = live.ids["claude-sample-acct-c"].clone();
    ensure!(
        live.scene()?.row(&id).context("off row")?.disabled_slot,
        "initial disabled slot"
    );
    let before = live.trace()?["scene"]["epoch"].as_u64().unwrap();
    let delay_file = live.home.join("action-delay");
    fs::write(&delay_file, b"delay")?;
    live.click(
        inline(&id, v2::ActionKind::Enable),
        g::Overlay::None,
        "list",
    )?;
    live.until(INPUT, "visible pending action frame", |s| {
        let bytes = fs::read(s.home.join("trace.jsonl"))?;
        Ok(bytes
            .split(|b| *b == b'\n')
            .filter_map(|l| serde_json::from_slice::<Value>(l).ok())
            .any(|t| {
                t["event"] == "paint"
                    && t["menu_pending"] == true
                    && t["scene"]["epoch"].as_u64().unwrap_or(0) > before
            }))
    })?;
    fs::remove_file(&delay_file)?;
    live.until(
        WAIT,
        "mouse enable reached real core after controlled five-second worker delay",
        |s| Ok(s.account(&id)?["user_state"] == "active"),
    )?;
    live.until(INPUT, "pending settled", |s| {
        Ok(s.trace()?["menu_pending"] == false)
    })?;
    let scene = live.scene()?;
    let row = scene.row(&id).context("enabled frozen row")?;
    ensure!(
        row.disabled_slot && row.button.is_none(),
        "enabled row must keep old slot and lose button"
    );
    let h = live.hwnd()?;
    send(h, WM_APP + 1, WPARAM(0), LPARAM(WM_LBUTTONUP as isize))?;
    thread::sleep(Duration::from_millis(100));
    send(h, WM_APP + 1, WPARAM(0), LPARAM(WM_LBUTTONUP as isize))?;
    let view = v2::project(&live.state()?.to_string(), SystemTime::now())?;
    live.session.hide();
    live.session.show(&view);
    live.repaint()?;
    ensure!(
        !live
            .scene()?
            .row(&id)
            .context("reopened enabled row")?
            .disabled_slot,
        "enabled row did not move on reopen"
    );
    Ok(())
}

#[test]
#[ignore = "manual synthetic relogin panel; NO_BROWSER hook mandatory"]
fn r5_live_relogin_panel_cancel_cleans_core_login() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _dpi = Dpi::enter()?;
    let mut live = Live::start("relogin")?;
    let id = live.ids["codex-hello.sample-acct"].clone();
    live.click(
        inline(&id, v2::ActionKind::Relogin),
        g::Overlay::Login("codex".into()),
        "login",
    )?;
    live.until(INPUT, "login pending id", |s| {
        Ok(fs::read_to_string(s.home.join("login.log")).is_ok_and(|v| v.starts_with("pending ")))
    })?;
    let text = fs::read_to_string(live.home.join("login.log"))?;
    let login = text
        .trim()
        .strip_prefix("pending ")
        .context("login id")?
        .to_owned();
    ensure!(live.scene()?.login.is_some(), "login panel not painted");
    live.click(g::Target::LoginCancel, g::Overlay::None, "list")?;
    live.until(INPUT, "core login cancelled", |s| {
        Ok(
            s.api("GET", &format!("/admin/v1/accounts/login/{login}"), None)?["status"]
                == "cancelled",
        )
    })?;
    live.until(INPUT, "tray cancellation hook", |s| {
        Ok(fs::read_to_string(s.home.join("login.log"))?.trim() == "cancelled")
    })?;
    ensure!(
        live.account(&id)?["health"]["kind"] == "needs_relogin",
        "cancel changed account"
    );
    Ok(())
}

#[test]
#[ignore = "manual native render-target reuse against a synthetic spawned core"]
fn r5_live_same_size_repaints_reuse_native_render_target() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _dpi = Dpi::enter()?;
    let mut live = Live::start("empty")?;
    for _ in 0..3 {
        let trace = live.repaint()?;
        ensure!(
            trace["timing_ms"]["target_reused"] == 1,
            "same-size paint recreated or resized its render target"
        );
    }
    Ok(())
}

#[test]
#[ignore = "manual settings and safe folder launch hook"]
fn r5_live_settings_log_action_works_in_empty_state() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _dpi = Dpi::enter()?;
    let mut live = Live::start("empty")?;
    live.click(
        g::Target::Footer(g::FooterAction::Settings),
        g::Overlay::Settings,
        "settings",
    )?;
    ensure!(
        live.scene()?
            .popup
            .as_ref()
            .context("settings popup")?
            .items
            .iter()
            .any(|i| i.target
                == g::Target::Menu {
                    id: None,
                    action: g::MenuAction::OpenConfig
                }),
        "config action missing"
    );
    live.hit(
        g::Target::Menu {
            id: None,
            action: g::MenuAction::OpenLogs,
        },
        false,
    )?;
    live.until(INPUT, "logs launch hook", |s| {
        Ok(fs::read_to_string(s.home.join("launch.log"))
            .is_ok_and(|v| Path::new(&v) == s.home.join("logs")))
    })?;
    // OpenConfig must NOT be activated until main adds the missing interception hook.
    Ok(())
}

#[test]
#[ignore = "requires coordinator's guarded config-launch hook; fails safely before ShellExecute when absent"]
fn r5_live_settings_config_action_is_intercepted() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _dpi = Dpi::enter()?;
    let mut live = Live::start("empty")?;
    // A hook env var alone is not proof an old product binary understands it.
    // Capability advertisement must precede any potentially external launch.
    ensure!(
        live.trace()?["file_launch_hook"] == true,
        "BLOCKED config action: add debug+testkit file_launch_hook telemetry and guarded QB_TRAY_TEST_FILE_LAUNCH_LOG interception in windows_tray::open_config; see NOTES"
    );
    live.click(
        g::Target::Footer(g::FooterAction::Settings),
        g::Overlay::Settings,
        "settings",
    )?;
    live.hit(
        g::Target::Menu {
            id: None,
            action: g::MenuAction::OpenConfig,
        },
        false,
    )?;
    live.until(INPUT, "config launch hook", |s| {
        let Ok(bytes) = fs::read(s.home.join("file-launch.json")) else {
            return Ok(false);
        };
        let v: Value = serde_json::from_slice(&bytes)?;
        Ok(v["action"] == "open_config"
            && v["path"]
                .as_str()
                .is_some_and(|p| Path::new(p) == s.home.join("config.toml")))
    })?;
    Ok(())
}
