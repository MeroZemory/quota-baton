//! Opt-in R1 group 4 acceptance against separately spawned synthetic core/tray.
//! No Cargo invocation, real profile, shell-wide window lookup, or Explorer click.
//!
//! REQUIRED MAIN-OWNED HOOKS (not implemented by this test):
//! 1. Debug+testkit only: QB_TRAY_TEST_SHELL_LOG selects an append-only JSONL file.
//!    Emit the complete `Trace` schema below on successful NIM_ADD/NIM_MODIFY,
//!    show/hide, and SSE connect/disconnect, using queued worker I/O. Never dump
//!    raw events, secrets, or bodies. `badge`, `tooltip`, `dark`, `icon_size`
//!    describe the successfully installed shell icon, not desired projection.
//!    `event` is shell|paint|visibility|balloon; `accounts` is the count.
//!    `kind` is null or the Debug notice kind. No extra fields are required.
//!    `state` is "list" for the ordinary flyout, including after it is hidden.
//!    `sequence` increases in capture order; async writes may arrive out of order.
//! 2. Debug+testkit only: WM_APP+6, WPARAM=0 light / 1 dark, LPARAM=0 changes the
//!    cached system theme (only for explicit [tray] theme=system), updates shell/window theme and invalidates the native
//!    renderer. No registry mutation. Native paint is checked through PrintWindow.
//!
//! No core hooks are required: pause/resume use the real admin routes; revoked
//! credentials are produced by our loopback upstream and handled by the relay.
//! Build both binaries with testkit separately. Optional R1_CORE_BIN/R1_TRAY_BIN
//! must resolve inside this workspace's target/. Otherwise use target/debug/.
//! Run ignored tests explicitly, serially. Missing hooks FAIL; they never skip.
//! Every fixture is new under .sandbox/r1-tray-live/, retained with its evidence.
//! Release memory uses R1_RELEASE_CORE_BIN/R1_RELEASE_TRAY_BIN (otherwise the
//! target/release products). Build CORE in release with --features testkit solely
//! to permit numeric-loopback upstream config; build TRAY in release with default
//! features. Both have debug_assertions=false, so debug telemetry remains disabled.
//! Run with no hooks, debug-only arguments, or runtime theme override. The default
//! release core rejects this fixture; do not relax Config validation.

#![cfg(windows)]

use anyhow::{Context, Result, bail, ensure};
use chrono::{TimeZone, Utc};
use qb_core::accounts::{
    Account, AccountStore, Health, Identity, Provider, Source, TokenMeta, Tokens, UserState,
};
use qb_tray::{
    v2,
    v2_geometry::{self as geometry, Target},
    v2_render::Renderer,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::{CloseHandle, HWND, LPARAM, RECT, WPARAM},
    Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS,
        DeleteDC, DeleteObject, GetDC, GetDIBits, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow,
        ReleaseDC, SelectObject,
    },
    Storage::Xps::PrintWindow,
    System::{
        ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        },
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ},
    },
    UI::WindowsAndMessaging::{
        FindWindowExW, GetClientRect, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
        PostMessageW, WM_APP, WM_LBUTTONUP, WM_RBUTTONUP,
    },
};
use windows::core::w;

static SERIAL: Mutex<()> = Mutex::new(());
const STARTUP: Duration = Duration::from_secs(8);
const IMMEDIATE: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug, Deserialize)]
struct Trace {
    event: String,
    kind: Option<String>,
    sequence: u64,
    visible: bool,
    state: String,
    dark: bool,
    icon_size: u32,
    badge: String,
    tooltip: String,
    #[serde(rename = "accounts")]
    account_count: usize,
    scene: Option<SceneTrace>,
}
#[derive(Clone, Debug, Deserialize)]
struct SceneTrace {
    epoch: u64,
    height: f32,
    offset: f32,
    rows: usize,
}

// Nonblocking accept + bounded reads/writes let Drop join even after a partial
// request or failed startup. Only POST /responses can produce revocation; usage
// and token probes remain on this same synthetic listener.
struct Upstream {
    address: SocketAddr,
    revoked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

fn listener() -> Result<TcpListener> {
    loop {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        if !matches!(listener.local_addr()?.port(), 10100 | 10101) {
            return Ok(listener);
        }
    }
}

impl Upstream {
    fn start() -> Result<Self> {
        let listener = listener()?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let revoked = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let revoking = revoked.clone();
        let task = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = respond(&mut stream, &revoking);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            revoked,
            stop,
            task: Some(task),
        })
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn respond(stream: &mut TcpStream, revoked: &AtomicBool) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(300)))?;
    stream.set_write_timeout(Some(Duration::from_millis(300)))?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut bytes = Vec::new();
    loop {
        ensure!(Instant::now() < deadline, "synthetic request deadline");
        ensure!(bytes.len() < 64 * 1024, "synthetic request too large");
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk)?;
        ensure!(count > 0, "synthetic request truncated");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&bytes[..end])?;
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>())
                })
                .transpose()?
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                let is_response = head.lines().next().is_some_and(|line| {
                    line.starts_with("POST /v1/responses ") || line.starts_with("POST /responses ")
                });
                let (status, body) = if is_response && revoked.load(Ordering::Acquire) {
                    (
                        "401 Unauthorized",
                        r#"{"error":{"code":"token_revoked","message":"synthetic invalidated oauth token"}}"#,
                    )
                } else if is_response {
                    ("200 OK", r#"{"fake":true}"#)
                } else {
                    ("200 OK", "{}")
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
                return Ok(());
            }
        }
    }
}

struct Live {
    // Children are registered immediately after spawn, before any fallible wait.
    // Drop kills only these exact handles, including on assertion/panic/startup.
    tray: Option<Child>,
    core: Option<Child>,
    upstream: Upstream,
    home: PathBuf,
    account: String,
    admin: u16,
    data: u16,
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    tray_started: Instant,
}

impl Drop for Live {
    fn drop(&mut self) {
        for child in [&mut self.tray, &mut self.core].into_iter().flatten() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn binary(root: &Path, variable: &str, filename: &str) -> Result<PathBuf> {
    let path = std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/debug").join(filename))
        .canonicalize()
        .with_context(|| {
            format!("build {filename} separately using the fixture build instructions above")
        })?;
    ensure!(
        path.starts_with(root.join("target").canonicalize()?) && path.is_file(),
        "{variable} must be a binary in this workspace target/"
    );
    Ok(path)
}

fn child(binary: &Path, home: &Path, name: &str) -> Result<Command> {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .current_dir(home)
        .creation_flags(0x08000000);
    // Avoid inheriting source discovery, proxy settings, credentials, or old
    // test hooks. The only host environment retained is Windows DLL discovery.
    for key in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for key in [
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "HOME",
        "TEMP",
        "TMP",
    ] {
        command.env(key, home);
    }
    command
        .env("QB_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(
            home.join(format!("{name}.stdout.log")),
        )?))
        .stderr(Stdio::from(fs::File::create(
            home.join(format!("{name}.stderr.log")),
        )?));
    Ok(command)
}

impl Live {
    fn start(name: &str) -> Result<Self> {
        Self::spawn(name, false)
    }

    fn spawn(name: &str, release: bool) -> Result<Self> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let core_bin = binary(
            &root,
            if release {
                "R1_RELEASE_CORE_BIN"
            } else {
                "R1_CORE_BIN"
            },
            if release {
                "../release/quota-baton.exe"
            } else {
                "quota-baton.exe"
            },
        )?;
        let tray_bin = binary(
            &root,
            if release {
                "R1_RELEASE_TRAY_BIN"
            } else {
                "R1_TRAY_BIN"
            },
            if release {
                "../release/quota-baton-tray.exe"
            } else {
                "quota-baton-tray.exe"
            },
        )?;
        let base = root.join(".sandbox/r1-tray-live");
        fs::create_dir_all(&base)?;
        let home = base.join(format!(
            "{name}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&home)?; // Never select, clear, or reuse a supplied home.
        let upstream = Upstream::start()?;
        // Hold both reservations until config is written; release immediately
        // before spawning core. A bind race fails startup, never changes ports.
        let data = listener()?;
        let admin = listener()?;
        let data_port = data.local_addr()?.port();
        let admin_port = admin.local_addr()?.port();
        let identity = Identity::Codex {
            account_id: "r1-live-synthetic".into(),
            email: "r1-live@example.invalid".into(),
            user_id: None,
        };
        let id = identity.id();
        {
            let store = AccountStore::acquire(&home).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let account = Account {
                schema: 1,
                id: id.clone(),
                provider: Provider::Codex,
                label: "synthetic R1 live".into(),
                email: "r1-live@example.invalid".into(),
                plan: Some("synthetic".into()),
                identity,
                source: Source::Oauth,
                user_state: UserState::Active,
                health: Health::Ready,
                auth_failures: 0,
                payment_failures: 0,
                payment_until: None,
                auth_probe_pending: false,
                quota_cooling: None,
                drain: false,
                allow_credits: false,
                quota_credit_releasable: false,
                generation: 0,
                token_meta: TokenMeta {
                    expires_at: Some(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).single().unwrap()),
                    last_refresh_at: None,
                    refresh_uncertain: false,
                },
            };
            store
                .save(
                    &account,
                    &Tokens {
                        access_token: "synthetic-r1-access".into(),
                        refresh_token: "synthetic-r1-refresh".into(),
                        id_token: None,
                    },
                    None,
                )
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            if release {
                for n in 1..8 {
                    let mut extra = account.clone();
                    extra.email = format!("r1-memory-{n}@example.invalid");
                    extra.label = extra.email.clone();
                    extra.identity = Identity::Codex {
                        account_id: format!("r1-memory-{n}"),
                        email: extra.email.clone(),
                        user_id: None,
                    };
                    extra.id = extra.identity.id();
                    store
                        .save(
                            &extra,
                            &Tokens {
                                access_token: format!("synthetic-r1-memory-access-{n}"),
                                refresh_token: format!("synthetic-r1-memory-refresh-{n}"),
                                id_token: None,
                            },
                            None,
                        )
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                }
            }
        }
        fs::write(
            home.join("config.toml"),
            format!(
                "[server]\ndata_port={data_port}\nadmin_port={admin_port}\ndrain_timeout_secs=1\n[upstream]\ncodex_base='{url}/'\nanthropic_base='{url}/'\ncodex_usage_base='{url}/'\nclaude_usage_base='{url}/'\ncodex_token_endpoint='{url}/oauth/token'\nclaude_token_endpoint='{url}/oauth/token'\n[tray]\nburst_new_sessions=2\n",
                url = format_args!("http://{}", upstream.address),
            ),
        )?;
        if name == "theme" && !release {
            use std::io::Write;
            fs::OpenOptions::new()
                .append(true)
                .open(home.join("config.toml"))?
                .write_all(b"theme='system'\n")?;
        }
        let mut live = Self {
            tray: None,
            core: None,
            upstream,
            home,
            account: id,
            admin: admin_port,
            data: data_port,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_millis(400))
                .build()?,
            tray_started: Instant::now(),
        };
        drop(data);
        drop(admin);
        live.core = Some(child(&core_bin, &live.home, "core")?.arg("serve").spawn()?);
        let deadline = Instant::now() + STARTUP;
        loop {
            live.alive()?;
            if live.admin("GET", "/admin/v1/health").is_ok_and(|health| {
                health["ok"] == true && health["pid"] == live.core.as_ref().unwrap().id()
            }) {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "synthetic core startup deadline; inspect project-local core logs"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let mut command = child(&tray_bin, &live.home, "tray")?;
        command.arg("--no-supervise");
        if !release {
            command
                .env("QB_TRAY_TEST_NO_BROWSER", "1")
                .env("QB_TRAY_TEST_NO_EXPLORER", "1")
                .env("QB_TRAY_TEST_NO_CLIPBOARD", "1")
                .env("QB_TRAY_TEST_THEME", "light")
                .env("QB_TRAY_TEST_SHELL_LOG", live.home.join("tray-trace.jsonl"))
                .arg("--open-flyout-live");
        }
        live.tray_started = Instant::now();
        live.tray = Some(command.spawn()?);
        if !release {
            live.wait(
                STARTUP,
                "missing shell hook or initial live presentation",
                |t| t.visible && t.account_count == 1 && t.badge == "normal",
            )?;
        }
        Ok(live)
    }

    fn alive(&mut self) -> Result<()> {
        for (name, child) in [("core", &mut self.core), ("tray", &mut self.tray)] {
            if let Some(child) = child {
                ensure!(
                    child.try_wait()?.is_none(),
                    "owned {name} exited; inspect fixture logs"
                );
            }
        }
        Ok(())
    }

    fn admin(&self, method: &str, path: &str) -> Result<Value> {
        let token = fs::read_to_string(self.home.join("admin.token"))?;
        self.runtime.block_on(async {
            let response = self
                .client
                .request(
                    method.parse()?,
                    format!("http://127.0.0.1:{}{path}", self.admin),
                )
                .bearer_auth(token.trim())
                .send()
                .await?;
            ensure!(
                response.status().is_success(),
                "synthetic admin request failed: {}",
                response.status()
            );
            Ok(response.json().await?)
        })
    }

    fn trace(&self) -> Result<Option<Trace>> {
        self.trace_matching(|_| true)
    }

    fn trace_matching(&self, accepts: impl Fn(&Trace) -> bool) -> Result<Option<Trace>> {
        let path = self.home.join("tray-trace.jsonl");
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "R1 trace exceeded bounded fixture budget"
        );
        // Ignore only an in-flight final line. Complete malformed records fail.
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
            return Ok(None);
        };
        let mut latest: Option<Trace> = None;
        for line in bytes[..end]
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
        {
            let trace: Trace = serde_json::from_slice(line).context("R1 trace schema mismatch")?;
            ensure!(
                matches!(
                    trace.event.as_str(),
                    "shell" | "paint" | "visibility" | "balloon"
                ),
                "unknown shell telemetry event"
            );
            if accepts(&trace)
                && latest
                    .as_ref()
                    .is_none_or(|previous| trace.sequence > previous.sequence)
            {
                latest = Some(trace);
            }
        }
        Ok(latest)
    }

    fn wait(
        &mut self,
        limit: Duration,
        why: &str,
        accepts: impl Fn(&Trace) -> bool,
    ) -> Result<Trace> {
        let deadline = Instant::now() + limit;
        loop {
            self.alive()?;
            if let Some(trace) = self.trace_matching(&accepts)? {
                return Ok(trace);
            }
            if Instant::now() >= deadline {
                bail!("{why}; evidence: {}", self.home.display());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn hwnd(&self) -> Result<HWND> {
        let expected = self.tray.as_ref().context("tray missing")?.id();
        let mut after = None;
        loop {
            let hwnd = unsafe { FindWindowExW(None, after, w!("QuotaBatonTrayFlyout"), None) }
                .context("owned tray window not found")?;
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
            }
            if pid == expected {
                return Ok(hwnd);
            }
            after = Some(hwnd);
        }
    }

    fn post(&self, message: u32, wparam: WPARAM, lparam: LPARAM) -> Result<()> {
        unsafe {
            PostMessageW(Some(self.hwnd()?), message, wparam, lparam)?;
        }
        Ok(())
    }

    fn tray_click(&self, button: u32) -> Result<()> {
        // Same callback Explorer sends. Never force ShowWindow/foreground.
        self.post(WM_APP + 1, WPARAM(0), LPARAM(button as isize))
    }

    fn assert_visible(&self, trace: &Trace, expected: bool) -> Result<()> {
        assert_eq!(trace.visible, expected);
        assert_eq!(unsafe { IsWindowVisible(self.hwnd()?).as_bool() }, expected);
        assert_eq!(trace.state, "list", "tray toggle opened an account menu");
        Ok(())
    }

    fn expected_tooltip(&self) -> Result<String> {
        let state = self.admin("GET", "/admin/v1/state")?;
        Ok(v2::project(&serde_json::to_string(&state)?, SystemTime::now())?.tray_tooltip)
    }
    fn measured_scene(&self, expanded: Option<bool>) -> Result<geometry::Scene> {
        let trace = self
            .trace_matching(|t| t.event == "paint" && t.scene.is_some())?
            .context("actual Scene paint missing")?;
        let frame = trace.scene.context("paint Scene missing")?;
        ensure!(frame.epoch > 0, "paint epoch missing");
        let view = v2::project(
            &serde_json::to_string(&self.admin("GET", "/admin/v1/state")?)?,
            SystemTime::now(),
        )?;
        let hwnd = self.hwnd()?;
        let scale =
            unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) } as f32 / 96.;
        let mut client = RECT::default();
        unsafe {
            GetClientRect(hwnd, &mut client)?;
        }
        let extent = (client.bottom - client.top) as f32 / scale;
        let mut p = geometry::Presentation {
            offset: frame.offset,
            frozen: geometry::capture_order(&view),
            ..Default::default()
        };
        if let Some(expanded) = expanded {
            p.expanded.insert("codex".into(), expanded);
        }
        let renderer = Renderer::new(trace.dark)?;
        renderer.measure(&view, &mut p)?;
        let scene = geometry::Scene::build(&view, p, extent);
        ensure!(
            (frame.height - extent).abs() <= 1.1 / scale
                && (scene.rect.bottom - extent).abs() <= 1.1 / scale,
            "Scene/client DPI extent mismatch"
        );
        Ok(scene)
    }
    fn assert_state(
        &self,
        trace: &Trace,
        expected_state: &str,
        expected_health: &str,
    ) -> Result<()> {
        let state = self.admin("GET", "/admin/v1/state")?;
        let accounts = state["accounts"]
            .as_array()
            .context("authoritative accounts missing")?;
        ensure!(accounts.len() == 1, "authoritative fixture count changed");
        assert_eq!(accounts[0]["id"], self.account);
        assert_eq!(accounts[0]["health"]["kind"], expected_health);
        assert_eq!(accounts[0]["user_state"], expected_state);
        assert_eq!(trace.account_count, accounts.len());
        Ok(())
    }

    fn revoke(&self) -> Result<()> {
        self.upstream.revoked.store(true, Ordering::Release);
        let key = fs::read_to_string(self.home.join("client.key"))?;
        self.runtime.block_on(async {
            let response = self
                .client
                .post(format!("http://127.0.0.1:{}/v1/responses", self.data))
                .timeout(Duration::from_secs(3))
                .bearer_auth(key.trim())
                .header("session-id", "r1-live-revocation")
                .header("originator", "r1-live-synthetic")
                .json(&json!({"model":"synthetic", "input":"synthetic"}))
                .send()
                .await?;
            ensure!(
                !response.status().is_success(),
                "revoked fake unexpectedly served"
            );
            let _ = response.bytes().await?;
            Ok(())
        })
    }
}

#[test]
#[ignore = "spawned synthetic Windows core/tray; requires main shell telemetry hooks"]
fn r1_live_badge_tooltip_follow_real_health_and_core_loss() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut live = Live::start("badge")?;
    let normal = live.wait(IMMEDIATE, "normal shell presentation missing", |t| {
        t.badge == "normal"
    })?;
    assert_eq!(normal.tooltip, live.expected_tooltip()?);
    ensure!(
        matches!(normal.icon_size, 16 | 20 | 24 | 32),
        "invalid shell icon size"
    );
    live.assert_state(&normal, "active", "ready")?;
    live.revoke()?;
    let attention = live.wait(IMMEDIATE, "revocation did not update shell badge", |t| {
        t.badge == "attention" && t.sequence > normal.sequence
    })?;
    assert_eq!(attention.tooltip, live.expected_tooltip()?);
    live.assert_state(&attention, "active", "needs_relogin")?;
    let toast = live.wait(IMMEDIATE, "actionable re-login balloon missing", |t| {
        t.event == "balloon" && t.kind.as_deref() == Some("Relogin")
    })?;
    let before_click = live.admin("GET", "/admin/v1/state")?;
    live.post(
        WM_APP + 1,
        WPARAM(0),
        LPARAM(windows::Win32::UI::Shell::NIN_BALLOONUSERCLICK as isize),
    )?;
    let focused = live.wait(
        IMMEDIATE,
        "balloon click did not reveal the row with menu closed",
        |t| t.sequence > toast.sequence && t.event == "paint" && t.state == "list" && t.visible,
    )?;
    let scene = live.measured_scene(None)?;
    let row = scene
        .row(&live.account)
        .context("notification account row missing")?;
    ensure!(
        row.rect.top >= scene.list.top - 0.5 && row.rect.bottom <= scene.list.bottom + 0.5,
        "notification row not revealed in actual list viewport"
    );
    ensure!(scene.popup.is_none(), "notification opened a menu");
    let after_click = live.admin("GET", "/admin/v1/state")?;
    for field in ["health", "user_state", "drain", "allow_credits", "source"] {
        assert_eq!(
            before_click["accounts"][0][field], after_click["accounts"][0][field],
            "balloon mutated {field}"
        );
    }
    for action in ["pause", "resume"] {
        let prior = live.trace()?.context("shell publication missing")?.sequence;
        live.admin(
            "POST",
            &format!("/admin/v1/accounts/{}/{action}", live.account),
        )?;
        live.wait(IMMEDIATE, "repeated account snapshot did not arrive", |t| {
            t.sequence > prior && t.event == "shell"
        })?;
    }
    let logs = fs::read_to_string(live.home.join("tray-trace.jsonl"))?;
    let relogin_count = logs
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["event"] == "balloon" && v["kind"] == "Relogin")
        .count();
    assert_eq!(
        relogin_count, 1,
        "repeated snapshots bypassed 30-minute debounce"
    );
    live.assert_state(&focused, "active", "needs_relogin")?;
    let owned_core = live.core.as_mut().context("owned core missing")?;
    owned_core.kill()?;
    owned_core.wait()?;
    live.core = None;
    // Core health polling is intentionally 10 s, separate from SSE acceptance.
    let error = live.wait(
        Duration::from_secs(12),
        "core loss did not reach shell",
        |t| t.badge == "error" && t.sequence > attention.sequence,
    )?;
    assert_eq!(error.tooltip, "quota-baton · 코어에 연결할 수 없음");
    assert_eq!(
        error.account_count, 1,
        "core loss discarded last known rows"
    );
    Ok(())
}

#[test]
#[ignore = "spawned synthetic Windows core/tray; requires main shell/theme hooks"]
fn r1_live_runtime_theme_repaints_native_window_and_shell() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut live = Live::start("theme")?;
    let mut before = live.wait(IMMEDIATE, "light initial theme missing", |t| !t.dark)?;
    capture_theme(
        live.hwnd()?,
        &live.home.join("initial-light.bmp"),
        [255, 255, 255],
    )?;
    for (dark, background) in [(true, [32, 32, 32]), (false, [255, 255, 255])] {
        live.post(WM_APP + 6, WPARAM(usize::from(dark)), LPARAM(0))?;
        let after = live.wait(
            IMMEDIATE,
            "runtime theme failed to repaint/update shell",
            |t| t.dark == dark && t.event == "paint" && t.sequence > before.sequence,
        )?;
        capture_theme(
            live.hwnd()?,
            &live.home.join(if dark { "dark.bmp" } else { "light.bmp" }),
            background,
        )?;
        assert_eq!(after.icon_size, before.icon_size);
        assert_eq!(after.badge, before.badge);
        assert_eq!(after.tooltip, before.tooltip);
        live.assert_visible(&after, true)?;
        before = after;
    }
    Ok(())
}

#[test]
#[ignore = "spawned synthetic Windows core/tray; requires main shell telemetry hooks"]
fn r1_live_account_events_immediately_refresh_authoritative_shell_state() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut live = Live::start("events")?;
    for (action, expected, badge) in [
        ("pause", "paused", "normal"),
        ("resume", "active", "normal"),
    ] {
        let before = live.trace()?.context("initial shell snapshot missing")?;
        // Health polls immediately, then every 10 s. Keep both mutations and
        // their publications strictly before that timer can satisfy the test.
        ensure!(
            live.tray_started.elapsed() + IMMEDIATE < Duration::from_secs(10),
            "startup left no pre-health-poll event acceptance window"
        );
        let started = Instant::now();
        live.admin(
            "POST",
            &format!("/admin/v1/accounts/{}/{action}", live.account),
        )?;
        let tooltip = live.expected_tooltip()?;
        let remaining = IMMEDIATE
            .checked_sub(started.elapsed())
            .context("admin action exceeded immediate budget")?;
        let after = live.wait(
            remaining,
            "account_changed did not update shell within 1.5 s",
            |t| t.sequence > before.sequence && t.badge == badge && t.tooltip == tooltip,
        )?;
        ensure!(
            started.elapsed() < IMMEDIATE && live.tray_started.elapsed() < Duration::from_secs(10),
            "event update missed the pre-poll immediate deadline"
        );
        live.assert_state(&after, expected, "ready")?;
        live.assert_visible(&after, true)?;
    }
    let before = live.trace()?.context("shell snapshot missing")?;
    live.admin(
        "POST",
        &format!("/admin/v1/accounts/{}/disable", live.account),
    )?;
    live.wait(IMMEDIATE, "disabled account snapshot missing", |t| {
        t.sequence > before.sequence && t.event == "paint" && t.badge == "normal"
    })?;
    // Open-lifetime membership stays frozen after disable. Reopen before
    // testing the new disabled section; blocked/off providers auto-expand it.
    let before = live.trace()?.context("disabled publication missing")?;
    live.tray_click(WM_LBUTTONUP)?;
    let hidden = live.wait(IMMEDIATE, "disabled list did not hide", |t| {
        t.sequence > before.sequence && !t.visible
    })?;
    live.tray_click(WM_LBUTTONUP)?;
    live.wait(IMMEDIATE, "disabled list did not reopen", |t| {
        t.sequence > hidden.sequence && t.visible && t.event == "paint" && t.state == "list"
    })?;
    let hwnd = live.hwnd()?;
    let mut expanded = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut expanded)?;
    }
    let scene = live.measured_scene(None)?;
    let section = scene
        .sections
        .iter()
        .find(|s| s.provider == "codex")
        .context("Codex section missing")?;
    ensure!(
        section.expanded && section.disabled_rows.len() == 1,
        "off provider failed initial auto-expansion"
    );
    let disclosure = scene
        .targets
        .iter()
        .find(|h| h.target == Target::Disclosure("codex".into()))
        .context("disabled disclosure missing")?
        .rect;
    let scale = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) } as f32 / 96.;
    let before = live.trace()?.context("expanded publication missing")?;
    let x = ((disclosure.left + disclosure.right) / 2. * scale).round() as i16;
    let y = ((disclosure.top + disclosure.bottom) / 2. * scale).round() as i16;
    live.post(
        WM_LBUTTONUP,
        WPARAM(0),
        LPARAM((((y as u16 as u32) << 16) | x as u16 as u32) as isize),
    )?;
    live.wait(IMMEDIATE, "manual collapse did not paint", |t| {
        t.sequence > before.sequence
            && t.event == "paint"
            && t.state == "list"
            && t.scene.as_ref().is_some_and(|s| s.rows == 0)
    })?;
    let mut collapsed = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut collapsed)?;
    }
    ensure!(
        expanded.bottom - expanded.top > collapsed.bottom - collapsed.top,
        "collapse did not reduce disabled section height"
    );
    let scene = live.measured_scene(Some(false))?;
    let disclosure = scene
        .targets
        .iter()
        .find(|h| h.target == Target::Disclosure("codex".into()))
        .context("collapsed disclosure missing")?
        .rect;
    let x = ((disclosure.left + disclosure.right) / 2. * scale).round() as i16;
    let y = ((disclosure.top + disclosure.bottom) / 2. * scale).round() as i16;
    let before = live.trace()?.context("collapsed publication missing")?;
    live.post(
        WM_LBUTTONUP,
        WPARAM(0),
        LPARAM((((y as u16 as u32) << 16) | x as u16 as u32) as isize),
    )?;
    live.wait(IMMEDIATE, "manual expansion did not paint", |t| {
        t.sequence > before.sequence
            && t.event == "paint"
            && t.state == "list"
            && t.scene.as_ref().is_some_and(|s| s.rows == 1)
    })?;
    let scene = live.measured_scene(Some(true))?;
    ensure!(
        scene
            .sections
            .iter()
            .any(|s| s.provider == "codex" && s.expanded && s.disabled_rows.len() == 1),
        "expanded Scene omitted disabled account"
    );
    let after = live.admin("GET", "/admin/v1/state")?;
    assert_eq!(
        after["accounts"][0]["user_state"], "disabled",
        "disclosure activated the account"
    );
    Ok(())
}

#[test]
#[ignore = "spawned synthetic Windows core/tray; requires main shell telemetry hooks"]
fn r1_live_tray_right_click_toggles_closed_and_reopens_without_account_menu() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut live = Live::start("toggle")?;
    let mut before = live.trace()?.context("initial shell snapshot missing")?;
    live.assert_visible(&before, true)?;
    for expected in [false, true, false, true, false] {
        live.tray_click(WM_RBUTTONUP)?;
        let after = live.wait(IMMEDIATE, "right-click toggle failed", |t| {
            t.sequence > before.sequence && t.visible == expected
        })?;
        live.assert_visible(&after, expected)?;
        before = after;
    }
    Ok(())
}

// PrintWindow renders only our owned HWND. Persist an uncompressed BMP and
// verify native palette pixels, independent of the telemetry's dark flag.
fn capture_theme(hwnd: HWND, path: &Path, background: [u8; 3]) -> Result<()> {
    ensure!(
        unsafe { RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW).as_bool() },
        "native repaint before capture failed"
    );
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect)?;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    ensure!(
        (1..=2000).contains(&width) && (1..=2000).contains(&height),
        "invalid capture dimensions"
    );
    let screen = unsafe { GetDC(None) };
    ensure!(!screen.0.is_null(), "GetDC failed");
    let memory = unsafe { CreateCompatibleDC(Some(screen)) };
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    // Cleanup runs even when capture/allocation/serialization/assertion fails.
    let result = (|| -> Result<()> {
        ensure!(
            !memory.0.is_null() && !bitmap.0.is_null(),
            "capture allocation failed"
        );
        let old = unsafe { SelectObject(memory, bitmap.into()) };
        let painted = unsafe { PrintWindow(hwnd, memory, Default::default()).as_bool() };
        unsafe {
            SelectObject(memory, old);
        }
        ensure!(painted, "PrintWindow failed (no screen-copy fallback)");
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
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        ensure!(
            unsafe {
                GetDIBits(
                    memory,
                    bitmap,
                    0,
                    height as u32,
                    Some(pixels.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            } == height,
            "GetDIBits failed"
        );
        let size = pixels.len() as u32;
        let mut bmp = Vec::with_capacity(54 + pixels.len());
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(54 + size).to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&width.to_le_bytes());
        bmp.extend_from_slice(&(-height).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&32u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&size.to_le_bytes());
        bmp.extend_from_slice(&[0; 16]);
        bmp.extend_from_slice(&pixels);
        fs::write(path, bmp)?;
        let matches = pixels
            .chunks_exact(4)
            .filter(|p| [p[2], p[1], p[0]] == background)
            .count();
        ensure!(
            matches > pixels.len() / 4 / 20,
            "native paint did not contain expected theme background; inspect BMP"
        );
        Ok(())
    })();
    unsafe {
        if !bitmap.0.is_null() {
            let _ = DeleteObject(bitmap.into());
        }
        if !memory.0.is_null() {
            let _ = DeleteDC(memory);
        }
        let _ = ReleaseDC(None, screen);
    }
    result
}

fn private_usage(pid: u32) -> Result<u64> {
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )?
    };
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let result = unsafe {
        GetProcessMemoryInfo(
            process,
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    result?;
    Ok(counters.PrivateUsage as u64)
}

// One settle per stage, <=20 s, no trimming/restarting/resampling after failure.
// Retain every raw observation, including all pre-settle high-water values.
fn memory_stage(
    live: &mut Live,
    label: &str,
    visible: bool,
    samples: &mut Vec<Value>,
) -> Result<u64> {
    let started = Instant::now();
    let mut recent = Vec::new();
    loop {
        live.alive()?;
        let hwnd = live.hwnd()?;
        let actual = unsafe { IsWindowVisible(hwnd).as_bool() };
        let pid = live.tray.as_ref().context("owned tray missing")?.id();
        let bytes = private_usage(pid)?;
        samples.push(json!({
            "stage":label, "pid":pid, "private_usage_bytes":bytes,
            "stage_elapsed_ms":started.elapsed().as_millis() as u64,
            "tray_elapsed_ms":live.tray_started.elapsed().as_millis() as u64,
            "visible":actual, "expected_visible":visible,
        }));
        fs::write(
            live.home.join("release-memory-samples.json"),
            serde_json::to_vec_pretty(samples)?,
        )?;
        if actual == visible {
            recent.push(bytes);
            if recent.len() > 8 {
                recent.remove(0);
            }
            if started.elapsed() >= Duration::from_secs(2)
                && recent.len() == 8
                && recent.iter().max().unwrap() - recent.iter().min().unwrap() <= 1024 * 1024
            {
                return Ok(*recent.iter().max().unwrap());
            }
        } else {
            recent.clear();
        }
        ensure!(
            started.elapsed() < Duration::from_secs(20),
            "{label} failed to settle within 20 s"
        );
        thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "spawned synthetic Windows core/tray; exercises retained burst while events are hidden"]
fn r1_live_hidden_burst_reaches_balloon_without_attention_or_health_changes() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut live = Live::start("hidden-burst")?;
    let before = live.trace()?.context("initial shell snapshot missing")?;
    live.tray_click(WM_LBUTTONUP)?;
    live.wait(IMMEDIATE, "flyout did not hide", |t| {
        !t.visible && t.sequence > before.sequence
    })?;
    let key = fs::read_to_string(live.home.join("client.key"))?;
    live.runtime.block_on(async {
        for n in 0..5 {
            let response = live
                .client
                .post(format!("http://127.0.0.1:{}/v1/responses", live.data))
                .bearer_auth(key.trim())
                .header("session-id", format!("r1-hidden-burst-{n}"))
                .header("originator", "r1-hidden-synthetic")
                .json(&json!({"model":"synthetic", "input":"synthetic"}))
                .send()
                .await?;
            ensure!(
                response.status().is_success(),
                "synthetic burst request failed"
            );
            let _ = response.bytes().await?;
        }
        Ok::<(), anyhow::Error>(())
    })?;
    let state = live.admin("GET", "/admin/v1/state")?;
    assert_eq!(
        state["warnings"]
            .as_array()
            .context("recent warning missing")?
            .len(),
        1
    );
    assert_eq!(state["warnings"][0]["burst_new_sessions"], 5);
    let notice = live.wait(
        Duration::from_secs(12),
        "hidden tray lost the burst warning",
        |t| t.event == "balloon" && t.kind.as_deref() == Some("Burst") && !t.visible,
    )?;
    assert_eq!(
        notice.badge, "normal",
        "burst fabricated a persistent health issue"
    );
    live.assert_state(&notice, "active", "ready")?;
    Ok(())
}

#[test]
#[ignore = "RELEASE testkit core + default-feature RELEASE tray; explicit synthetic memory acceptance only"]
fn r1_tray_release_memory() -> Result<()> {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut live = Live::spawn("release-memory", true)?;
    let state = live.admin("GET", "/admin/v1/state")?;
    ensure!(
        state["accounts"]
            .as_array()
            .is_some_and(|accounts| accounts.len() == 8),
        "release memory fixture must expose eight accounts"
    );
    let mut samples = Vec::new();
    let mut settled = Vec::new();
    let result = (|| -> Result<()> {
        let deadline = Instant::now() + STARTUP;
        while live.hwnd().is_err() {
            live.alive()?;
            ensure!(
                Instant::now() < deadline,
                "release tray window startup deadline"
            );
            thread::sleep(Duration::from_millis(25));
        }
        // Initial visibility is observed, never forced by ShowWindow.
        let initial = memory_stage(&mut live, "hidden-initial", false, &mut samples)?;
        settled.push(
            json!({"stage":"hidden-initial", "visible":false, "private_usage_bytes":initial}),
        );
        for cycle in 1..=3 {
            live.tray_click(WM_LBUTTONUP)?;
            let label = format!("open-{cycle}");
            let open = memory_stage(&mut live, &label, true, &mut samples)?;
            settled.push(json!({"stage":label, "visible":true, "private_usage_bytes":open}));
            live.tray_click(WM_LBUTTONUP)?;
            let label = format!("hidden-after-{cycle}");
            let hidden = memory_stage(&mut live, &label, false, &mut samples)?;
            settled.push(json!({"stage":label, "visible":false, "private_usage_bytes":hidden}));
        }
        Ok(())
    })();
    // Write summary before any acceptance error so failed gates keep evidence.
    let limit = 30 * 1024 * 1024u64;
    let hidden_pass = settled.iter().filter(|s| s["visible"] == false).all(|s| {
        s["private_usage_bytes"]
            .as_u64()
            .is_some_and(|n| n <= limit)
    });
    fs::write(
        live.home.join("release-memory-result.json"),
        serde_json::to_vec_pretty(&json!({
            "pid":live.tray.as_ref().context("tray missing")?.id(),
            "metric":"PROCESS_MEMORY_COUNTERS_EX.PrivateUsage", "account_count":8, "hidden_limit_bytes":limit,
            "completed":result.is_ok(), "hidden_pass":hidden_pass, "settled":settled,
            "raw_samples":"release-memory-samples.json",
        }))?,
    )?;
    result?;
    ensure!(
        hidden_pass,
        "release hidden PrivateUsage exceeds 30 MiB; evidence: {}",
        live.home.display()
    );
    Ok(())
}
