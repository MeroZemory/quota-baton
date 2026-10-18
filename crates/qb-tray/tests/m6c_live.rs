//! Manual Windows acceptance harness for M6c.
//!
//! It is ignored by default because it starts two real GUI processes. Set
//! `M6C_CORE_BIN`, `M6C_TRAY_BIN`, and `M6C_HOME` to synthetic test fixtures
//! before running it. Build the debug binaries with testkit for safe loopback
//! upstreams and typed shell telemetry. No real profile is selected by this test.

#![cfg(windows)]

use chrono::{TimeZone, Utc};
use qb_core::accounts::{
    Account, AccountStore, Health, Identity, Provider, Source, TokenMeta, Tokens, UserState,
};
use qb_tray::{
    v2,
    v2_geometry::{self as geometry, FooterAction, MenuAction, Overlay, Target},
    v2_render::Renderer,
    v2_session::Session,
};
use std::{
    cell::RefCell,
    time::{Instant, SystemTime},
};
use std::{
    fs,
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        Graphics::Gdi::{
            BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, RDW_INVALIDATE,
            RDW_UPDATENOW, RedrawWindow, ReleaseDC, SRCCOPY, SelectObject,
        },
        Storage::Xps::PrintWindow,
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
                SetThreadDpiAwarenessContext,
            },
            WindowsAndMessaging::{
                FindWindowExW, GetClientRect, GetWindowRect, GetWindowThreadProcessId,
                SMTO_ABORTIFHUNG, SW_SHOW, SendMessageTimeoutW, SetForegroundWindow, ShowWindow,
                WM_KEYDOWN, WM_LBUTTONUP, WM_MOUSEWHEEL, WM_NULL, WM_RBUTTONUP,
            },
        },
    },
    core::w,
};

struct DpiContext(DPI_AWARENESS_CONTEXT);
impl DpiContext {
    fn enter() -> anyhow::Result<Self> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        anyhow::ensure!(!previous.0.is_null(), "fixture thread DPI awareness failed");
        Ok(Self(previous))
    }
}
impl Drop for DpiContext {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

fn window(tray_pid: u32) -> Option<HWND> {
    let mut after = None;
    loop {
        let hwnd = unsafe { FindWindowExW(None, after, w!("QuotaBatonTrayFlyout"), None).ok()? };
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == tray_pid {
            return Some(hwnd);
        }
        after = Some(hwnd);
    }
}

fn send(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> anyhow::Result<()> {
    let mut result = 0usize;
    anyhow::ensure!(
        unsafe {
            SendMessageTimeoutW(
                hwnd,
                message,
                wparam,
                lparam,
                SMTO_ABORTIFHUNG,
                1000,
                Some(&mut result),
            )
            .0 != 0
        },
        "fixture window input timed out"
    );
    Ok(())
}

fn painted(hwnd: HWND) -> anyhow::Result<()> {
    responsive(hwnd)?;
    anyhow::ensure!(
        unsafe { RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW).as_bool() },
        "fixture repaint failed"
    );
    Ok(())
}

fn click(hwnd: HWND, x: i16, y: i16, message: u32) -> anyhow::Result<()> {
    let packed = ((y as u16 as u32) << 16) | x as u16 as u32;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    send(hwnd, message, WPARAM(0), LPARAM(packed as isize))
}

// Mirror only the live opening's presentation, never account data. Every scene
// projects /state from the spawned core and uses the actual painted scroll epoch.
#[derive(Default)]
struct Navigation {
    home: PathBuf,
    log: PathBuf,
    session: Session,
    popup_offset: f32,
}
thread_local! { static NAVIGATION: RefCell<Navigation> = RefCell::new(Navigation::default()); }
fn reset_navigation() {
    NAVIGATION.with(|n| {
        let mut n = n.borrow_mut();
        n.session.hide();
        n.popup_offset = 0.;
    });
}
fn latest_paint(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let bytes = fs::read(path)?;
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter(|v| v["event"] == "paint" && v["scene"]["epoch"].as_u64().is_some())
        .max_by_key(|v| v["sequence"].as_u64())
        .ok_or_else(|| anyhow::anyhow!("actual Scene paint telemetry missing"))
}
fn repaint_trace(hwnd: HWND) -> anyhow::Result<serde_json::Value> {
    let path = NAVIGATION.with(|n| n.borrow().log.clone());
    let before = latest_paint(&path)
        .ok()
        .and_then(|v| v["scene"]["epoch"].as_u64())
        .unwrap_or(0);
    painted(hwnd)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Ok(trace) = latest_paint(&path)
            && trace["scene"]["epoch"].as_u64().is_some_and(|e| e > before)
        {
            return Ok(trace);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "fresh actual Scene publication missing after repaint"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn live_scene(hwnd: HWND, state: &str) -> anyhow::Result<geometry::Scene> {
    let trace = repaint_trace(hwnd)?;
    let expected = state.split(':').next().unwrap_or("list");
    anyhow::ensure!(
        trace["state"] == expected,
        "actual overlay differs from target request: {trace}"
    );
    let home = NAVIGATION.with(|n| n.borrow().home.clone());
    let snapshot = admin_json(&home, "/admin/v1/state")?;
    let view = v2::project(&serde_json::to_string(&snapshot)?, SystemTime::now())?;
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.;
    let mut client = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut client)?;
    }
    let extent = (client.bottom - client.top) as f32 / scale;
    let mut p = geometry::Presentation {
        offset: trace["scene"]["offset"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("paint offset missing"))? as f32,
        ..Default::default()
    };
    if trace["menu_pending"] == true
        && let Some(id) = state.strip_prefix("menu:")
    {
        p.pending_ids.insert(id.into());
    }
    p.overlay = match state {
        "settings" => Overlay::Settings,
        "quit" => Overlay::Quit,
        "add-menu" => Overlay::Add,
        s if s.starts_with("menu:") => Overlay::RowMenu(s[5..].into()),
        s if s.starts_with("confirm-credits:") => Overlay::CreditsConfirm(s[16..].into()),
        s if s.starts_with("login:") => Overlay::Login(s[6..].into()),
        _ => Overlay::None,
    };
    NAVIGATION.with(|n| {
        let mut n = n.borrow_mut();
        if !n.session.open {
            n.session.show(&view);
        } else {
            n.session.update(&view);
        }
        p.expanded = n.session.expansion.clone();
        p.popup_offset = n.popup_offset;
        for slot in &n.session.slots {
            let section = p.frozen.entry(slot.provider.clone()).or_default();
            if slot.disabled_on_open {
                section.disabled_rows.push(slot.id.clone());
            } else {
                section.rows.push(slot.id.clone());
            }
        }
    });
    let renderer = Renderer::new(trace["dark"].as_bool().unwrap_or(false))?;
    renderer.measure(&view, &mut p)?;
    // The HWND has already been resized by native Scene construction. Its exact
    // client extent is the cap for this input lookup; never translate header/footer.
    let scene = geometry::Scene::build(&view, p, extent);
    anyhow::ensure!(
        (scene.rect.bottom - extent).abs() <= 1.1 / scale,
        "measured scene/client height mismatch"
    );
    anyhow::ensure!(
        (trace["scene"]["height"].as_f64().unwrap_or(-1.) as f32 - extent).abs() <= 1.1 / scale,
        "paint telemetry/client height mismatch"
    );
    Ok(scene)
}
fn target(
    hwnd: HWND,
    _rows: &[qb_tray::Row],
    state: &str,
    accepts: impl Fn(&Target) -> bool,
) -> anyhow::Result<Target> {
    let scene = live_scene(hwnd, state)?;
    // Logical rows/menu items remain in Scene when clipped out of the target
    // vector. Resolve identity here; click_target scrolls then rechecks hits.
    let mut candidates = scene
        .targets
        .iter()
        .map(|h| h.target.clone())
        .chain(
            scene
                .sections
                .iter()
                .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
                .map(|r| Target::Row(r.id.clone())),
        )
        .chain(
            scene
                .popup
                .iter()
                .flat_map(|p| p.items.iter())
                .map(|i| i.target.clone()),
        );
    candidates
        .find(accepts)
        .ok_or_else(|| anyhow::anyhow!("expected shared Scene target missing in {state}"))
}

fn footer(_rows: &[qb_tray::Row], action: FooterAction) -> anyhow::Result<Target> {
    Ok(Target::Footer(action))
}
fn click_target(
    hwnd: HWND,
    rows: &[qb_tray::Row],
    state: &str,
    target: Target,
    message: u32,
) -> anyhow::Result<()> {
    click_target_part(hwnd, rows, state, target, message, false)
}
fn click_target_part(
    hwnd: HWND,
    _rows: &[qb_tray::Row],
    state: &str,
    target: Target,
    message: u32,
    icon: bool,
) -> anyhow::Result<()> {
    let mut scene = live_scene(hwnd, state)?;
    // Scroll only when a row or popup item is outside its own viewport.
    let raw = match &target {
        Target::Row(id) => scene.row(id).map(|r| r.hover),
        Target::Menu { .. } => scene
            .popup
            .as_ref()
            .and_then(|p| p.items.iter().find(|i| i.target == target).map(|i| i.rect)),
        _ => None,
    };
    if let Some(rect) = raw {
        let clip = scene.popup.as_ref().map_or(scene.list, |p| p.clip);
        let delta = if rect.top < clip.top {
            rect.top - clip.top
        } else if rect.bottom > clip.bottom {
            rect.bottom - clip.bottom
        } else {
            0.
        };
        if delta != 0. {
            let step = if scene.popup.is_some() { 48. } else { 72. };
            let wheel = (-delta * 120. / step).round();
            anyhow::ensure!(wheel.abs() <= i16::MAX as f32, "scroll exceeds wheel range");
            send(
                hwnd,
                WM_MOUSEWHEEL,
                WPARAM((wheel as i16 as u16 as usize) << 16),
                LPARAM(0),
            )?;
            if let Some(p) = &scene.popup {
                NAVIGATION.with(|n| {
                    n.borrow_mut().popup_offset = (p.offset - wheel / 120. * 48.).clamp(
                        0.,
                        (p.content_height - (p.clip.bottom - p.clip.top)).max(0.),
                    )
                });
            }
            scene = live_scene(hwnd, state)?;
        }
    }
    let hit = scene
        .targets
        .iter()
        .rev()
        .find(|h| h.target == target)
        .ok_or_else(|| anyhow::anyhow!("typed target not visible: {target:?}"))?;
    // Disabled controls remain clickable for the repeated-pending absorption test.
    let mut rect = hit.rect;
    if icon {
        let item = scene
            .popup
            .as_ref()
            .and_then(|p| p.items.iter().find(|i| i.target == target))
            .ok_or_else(|| anyhow::anyhow!("checkbox item missing"))?;
        if hit.enabled {
            anyhow::ensure!(
                scene.hit_test(
                    (item.label.left + item.label.right) / 2.,
                    (item.label.top + item.label.bottom) / 2.
                ) == Some(&target),
                "checkbox label is not in full item hit scope"
            );
        }
        rect = scene
            .popup
            .as_ref()
            .and_then(|p| p.items.iter().find(|i| i.target == target))
            .and_then(|i| i.lead)
            .ok_or_else(|| anyhow::anyhow!("checkbox icon box missing"))?;
    }
    let x = if matches!(target, Target::Row(_)) {
        rect.left + 2.
    } else {
        (rect.left + rect.right) / 2.
    };
    let y = (rect.top + rect.bottom) / 2.;
    if hit.enabled {
        anyhow::ensure!(
            scene.hit_test(x, y) == Some(&target),
            "shared hit test/button precedence mismatch"
        );
    } else {
        anyhow::ensure!(
            scene.hit_test(x, y).is_none(),
            "disabled target fell through"
        );
    }
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.;
    click(
        hwnd,
        i16::try_from((x * scale).round() as i32)?,
        i16::try_from((y * scale).round() as i32)?,
        message,
    )
}

fn latest_shell(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let bytes = fs::read(path)?;
    let mut latest = serde_json::Value::Null;
    for line in bytes.split(|b| *b == b'\n') {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line)
            && value["sequence"].as_u64() > latest["sequence"].as_u64()
        {
            latest = value;
        }
    }
    Ok(latest)
}

fn responsive(hwnd: HWND) -> anyhow::Result<()> {
    let mut result = 0usize;
    unsafe {
        let quick = SendMessageTimeoutW(
            hwnd,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            200,
            Some(&mut result),
        )
        .0 != 0;
        if !quick {
            let slow = SendMessageTimeoutW(
                hwnd,
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1000,
                Some(&mut result),
            )
            .0 != 0;
            anyhow::ensure!(slow, "tray window did not respond within 1000 ms");
        }
    }
    Ok(())
}

fn capture(hwnd: HWND, path: &std::path::Path) -> anyhow::Result<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect)?;
    }
    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);
    let screen = unsafe { GetDC(None) };
    anyhow::ensure!(!screen.0.is_null(), "GetDC failed");
    let memory = unsafe { CreateCompatibleDC(Some(screen)) };
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    anyhow::ensure!(
        !memory.0.is_null() && !bitmap.0.is_null(),
        "bitmap allocation failed"
    );
    unsafe {
        let _old = SelectObject(memory, bitmap.into());
        anyhow::ensure!(
            PrintWindow(hwnd, memory, Default::default()).as_bool()
                || BitBlt(
                    memory,
                    0,
                    0,
                    width,
                    height,
                    Some(screen),
                    rect.left,
                    rect.top,
                    SRCCOPY
                )
                .is_ok(),
            "window capture failed"
        );
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
        anyhow::ensure!(
            GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS
            ) == height,
            "GetDIBits failed"
        );
        let image_size = pixels.len() as u32;
        let file_size = 14 + 40 + image_size;
        let mut bytes = Vec::with_capacity(file_size as usize);
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&file_size.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&(54u32).to_le_bytes());
        bytes.extend_from_slice(&(40u32).to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&(-height).to_le_bytes());
        bytes.extend_from_slice(&(1u16).to_le_bytes());
        bytes.extend_from_slice(&(32u16).to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&image_size.to_le_bytes());
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&pixels);
        if path.extension().and_then(|value| value.to_str()) == Some("png") {
            let bmp = path.with_extension("bmp.tmp");
            fs::write(&bmp, bytes)?;
            let status = Command::new("ffmpeg.exe")
                .args(["-y", "-loglevel", "error"])
                .arg("-i")
                .arg(&bmp)
                .arg("-frames:v")
                .arg("1")
                .arg(path)
                .status()?;
            anyhow::ensure!(status.success(), "PNG conversion failed");
            let _ = fs::remove_file(bmp);
        } else {
            fs::write(path, bytes)?;
        }
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        let _ = ReleaseDC(None, screen);
    }
    Ok((width, height))
}

fn capture_checked(hwnd: HWND, path: &std::path::Path) -> anyhow::Result<()> {
    let trace = repaint_trace(hwnd)?;
    anyhow::ensure!(
        trace["scene"]["epoch"].as_u64().is_some(),
        "actual painted Scene missing before capture"
    );
    let (width, height) = capture(hwnd, path)?;
    let mut client = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut client)?;
    }
    anyhow::ensure!(
        (width, height) == (client.right - client.left, client.bottom - client.top),
        "capture size {width}x{height} != client {}x{}",
        client.right - client.left,
        client.bottom - client.top
    );
    Ok(())
}

struct Children {
    core: Child,
    tray: Child,
}
impl Drop for Children {
    fn drop(&mut self) {
        for child in [&mut self.tray, &mut self.core] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn free_port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

struct SyntheticUpstream {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
    port: u16,
}

impl SyntheticUpstream {
    fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let join = thread::spawn(move || {
            while !signal.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => respond(&mut stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            stop,
            join: Some(join),
            port,
        })
    }
}

impl Drop for SyntheticUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn respond(stream: &mut TcpStream) {
    use std::io::{Read, Write};
    let mut request = [0u8; 4096];
    let _ = stream.read(&mut request);
    let body = b"{}";
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn prepare_home(home: &std::path::Path) -> anyhow::Result<SyntheticUpstream> {
    let sandbox = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.sandbox")
        .canonicalize()?;
    let parent = home
        .parent()
        .ok_or_else(|| anyhow::anyhow!("fixture home needs a parent"))?
        .canonicalize()?;
    anyhow::ensure!(
        parent.starts_with(&sandbox),
        "fixture home must stay inside this workspace's .sandbox"
    );
    if home.exists() {
        anyhow::ensure!(
            home.canonicalize()?.starts_with(&sandbox),
            "fixture home resolves outside .sandbox"
        );
    }
    if home.exists() {
        fs::remove_dir_all(home)?;
    }
    fs::create_dir_all(home)?;
    let upstream = SyntheticUpstream::start()?;
    let data = free_port()?;
    let admin = free_port()?;
    let store = AccountStore::acquire(home).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let row_count = std::env::var("M6C_ROWS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2);
    for n in 0..row_count {
        let email = format!("m6c-{n}@example.invalid");
        let identity = Identity::Codex {
            account_id: format!("m6c-{n}"),
            email: email.clone(),
            user_id: None,
        };
        let account = Account {
            schema: 1,
            id: identity.id(),
            provider: Provider::Codex,
            label: email.clone(),
            email,
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
                    access_token: format!("synthetic-access-{n}"),
                    refresh_token: format!("synthetic-refresh-{n}"),
                    id_token: None,
                },
                None,
            )
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    }
    fs::write(
        home.join("config.toml"),
        format!(
            "[server]\ndata_port={data}\nadmin_port={admin}\ndrain_timeout_secs=20\n[upstream]\ncodex_base='http://127.0.0.1:{port}/'\nanthropic_base='http://127.0.0.1:{port}/'\nclaude_usage_base='http://127.0.0.1:{port}/'\ncodex_usage_base='http://127.0.0.1:{port}/'\nclaude_token_endpoint='http://127.0.0.1:{port}/oauth/token'\ncodex_token_endpoint='http://127.0.0.1:{port}/oauth/token'\n",
            port = upstream.port
        ),
    )?;
    Ok(upstream)
}

fn admin_json(home: &std::path::Path, path: &str) -> anyhow::Result<serde_json::Value> {
    let config = fs::read_to_string(home.join("config.toml"))?;
    let admin = config
        .lines()
        .find_map(|line| line.strip_prefix("admin_port=")?.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("synthetic admin port missing"))?;
    let token = fs::read_to_string(home.join("admin.token"))?;
    use std::io::{Read, Write};
    let mut stream = TcpStream::connect(("127.0.0.1", admin))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        admin,
        token.trim()
    )?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    let text = String::from_utf8(bytes)?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("admin response missing body"))?;
    anyhow::ensure!(head.starts_with("HTTP/1.1 200"), "admin returned {head}");
    Ok(serde_json::from_str(body)?)
}

#[test]
#[ignore = "starts a real Windows core and tray; run explicitly with synthetic M6C_* variables"]
fn m6c_live_spawned_core_clicks_and_captures() -> anyhow::Result<()> {
    let _dpi = DpiContext::enter()?;
    let core_bin = std::env::var_os("M6C_CORE_BIN")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("M6C_CORE_BIN is required"))?;
    let tray_bin = std::env::var_os("M6C_TRAY_BIN")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("M6C_TRAY_BIN is required"))?;
    let home = std::env::var_os("M6C_HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("M6C_HOME is required"))?;
    let _upstream = prepare_home(&home)?;
    let explorer_log = home.join("explorer-hook.log");
    let login_log = home.join("login-hook.log");
    let delay_file = home.join("admin-delay");
    let shell_log = home.join("tray-shell.jsonl");
    let error_log = home.join("tray-error.log");
    NAVIGATION.with(|n| {
        let mut n = n.borrow_mut();
        n.home = home.clone();
        n.log = shell_log.clone();
    });
    unsafe {
        std::env::set_var("QB_TRAY_TEST_EXPLORER_LOG", &explorer_log);
    }
    unsafe {
        std::env::set_var("QB_TRAY_TEST_LOGIN_LOG", &login_log);
    }
    unsafe {
        std::env::set_var("QB_TRAY_TEST_ERROR_LOG", &error_log);
    }
    let evidence = std::env::var_os("M6C_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/reports/M6c-evidence")
        });
    let parent = evidence
        .parent()
        .ok_or_else(|| anyhow::anyhow!("evidence needs a parent"))?
        .canonicalize()?;
    anyhow::ensure!(
        parent.components().any(|c| c.as_os_str() == ".sandbox"),
        "evidence must stay in a project .sandbox"
    );
    if evidence.exists() {
        anyhow::ensure!(
            evidence
                .canonicalize()?
                .components()
                .any(|c| c.as_os_str() == ".sandbox"),
            "evidence resolves outside .sandbox"
        );
    }
    let _ = fs::remove_dir_all(&evidence);
    fs::create_dir_all(&evidence)?;
    let core = Command::new(&core_bin)
        .env("QB_HOME", &home)
        .arg("serve")
        .stdout(Stdio::from(fs::File::create(home.join("core.stdout.log"))?))
        .stderr(Stdio::from(fs::File::create(home.join("core.stderr.log"))?))
        .spawn()?;
    let core_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if admin_json(&home, "/admin/v1/health").is_ok() {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < core_deadline,
            "spawned core did not become healthy: {}",
            fs::read_to_string(home.join("core.stderr.log")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(100));
    }
    let tray = Command::new(&tray_bin)
        .env("QB_HOME", &home)
        .env("QB_TRAY_TEST_NO_BROWSER", "1")
        .env("QB_TRAY_TEST_KEEP_OPEN", "1")
        .env("QB_TRAY_TEST_DELAY_FILE", &delay_file)
        .env("QB_TRAY_TEST_SHELL_LOG", &shell_log)
        .args([
            "--no-supervise",
            "--open-flyout-live",
            "--now",
            "2030-01-01T00:00:00Z",
        ])
        .stdout(Stdio::from(fs::File::create(home.join("tray.stdout.log"))?))
        .stderr(Stdio::from(fs::File::create(home.join("tray.stderr.log"))?))
        .spawn()?;
    let mut children = Children { core, tray };
    let hwnd = (0..100)
        .find_map(|_| {
            let found = window(children.tray.id());
            if found.is_none() {
                thread::sleep(Duration::from_millis(100));
            }
            found
        })
        .ok_or_else(|| anyhow::anyhow!("flyout window not found"))?;
    anyhow::ensure!(
        !home.join("tray-error.log").exists(),
        "tray failed to read the live core state: {}",
        fs::read_to_string(home.join("tray-error.log")).unwrap_or_default()
    );
    thread::sleep(Duration::from_millis(200));
    capture_checked(hwnd, &evidence.join("list.png"))?;
    let state = admin_json(&home, "/admin/v1/state")
        .map_err(|error| anyhow::anyhow!("initial state: {error}"))?;
    let accounts = state["accounts"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("state accounts missing"))?;
    let row_count = std::env::var("M6C_ROWS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2);
    anyhow::ensure!(
        accounts.len() == row_count,
        "live tray decoded {} accounts",
        accounts.len()
    );
    let mut rows = qb_tray::parse_state(&serde_json::to_string(&state)?)?;
    let last_id = rows.last().map(|row| row.id.clone()).unwrap();
    let settings = footer(&rows, FooterAction::Settings)?;
    click_target(hwnd, &rows, "list", settings, WM_LBUTTONUP)?;
    let logs = target(hwnd, &rows, "settings", |t| {
        matches!(
            t,
            Target::Menu {
                action: MenuAction::OpenLogs,
                ..
            }
        )
    })?;
    click_target(hwnd, &rows, "settings", logs, WM_LBUTTONUP)?;
    thread::sleep(Duration::from_millis(150));
    anyhow::ensure!(
        explorer_log.exists(),
        "settings log-folder test hook was not called"
    );
    send(hwnd, WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
    let row_menu = target(
        hwnd,
        &rows,
        "list",
        |t| matches!(t, Target::Row(id) if id == &last_id),
    )?;
    click_target(hwnd, &rows, "list", row_menu, WM_RBUTTONUP)?;
    let menu_state = format!("menu:{last_id}");
    if rows
        .iter()
        .any(|row| row.id == last_id && row.provider == "Codex")
    {
        let checkbox = target(
            hwnd,
            &rows,
            &menu_state,
            |t| matches!(t,Target::Menu{id:Some(id),action:MenuAction::ToggleCredits} if id==&last_id),
        )?;
        // Every off-to-on attempt asks first. Cancel returns to the same menu,
        // changes no backend flag, and supplies no remembered consent.
        click_target_part(
            hwnd,
            &rows,
            &menu_state,
            checkbox.clone(),
            WM_LBUTTONUP,
            true,
        )?;
        let dialog_state = format!("confirm-credits:{last_id}");
        let cancel = target(hwnd, &rows, &dialog_state, |t| {
            matches!(t, Target::CancelDialog)
        })?;
        click_target(hwnd, &rows, &dialog_state, cancel, WM_LBUTTONUP)?;
        let cancelled = admin_json(&home, "/admin/v1/state")?;
        anyhow::ensure!(
            cancelled["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == last_id && a["allow_credits"] == false),
            "cancel enabled credits"
        );
        anyhow::ensure!(
            repaint_trace(hwnd)?["state"] == "menu",
            "cancel lost the originating menu"
        );
        click_target_part(
            hwnd,
            &rows,
            &menu_state,
            checkbox.clone(),
            WM_LBUTTONUP,
            true,
        )?;
        let confirm = target(
            hwnd,
            &rows,
            &dialog_state,
            |t| matches!(t,Target::ConfirmCredits(id) if id==&last_id),
        )?;
        fs::write(&delay_file, b"delay")?;
        click_target(hwnd, &rows, &dialog_state, confirm, WM_LBUTTONUP)?;
        painted(hwnd)?;
        thread::sleep(Duration::from_millis(250));
        responsive(hwnd)?;
        let pending = latest_shell(&shell_log)?;
        anyhow::ensure!(
            pending["state"] == "menu" && pending["menu_pending"] == true,
            "checkbox pending state was not painted: {pending}"
        );
        // A repeated glyph click while pending must not enqueue a second toggle.
        click_target_part(
            hwnd,
            &rows,
            &menu_state,
            checkbox.clone(),
            WM_LBUTTONUP,
            true,
        )?;
        capture_checked(hwnd, &evidence.join("credits-pending.png"))?;
        fs::remove_file(&delay_file)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            responsive(hwnd)?;
            let state = admin_json(&home, "/admin/v1/state")?;
            let changed = state["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == last_id && a["allow_credits"] == true);
            let shell = latest_shell(&shell_log)?;
            if changed
                && shell["state"] == "menu"
                && shell["menu_checked"] == true
                && shell["menu_pending"] == false
            {
                rows = qb_tray::parse_state(&serde_json::to_string(&state)?)?;
                break;
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "glyph toggle failed or menu closed: {shell}"
            );
            thread::sleep(Duration::from_millis(100));
        }
        capture_checked(hwnd, &evidence.join("credits-checked-menu-open.png"))?;
        click_target_part(hwnd, &rows, &menu_state, checkbox, WM_LBUTTONUP, true)?;
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            responsive(hwnd)?;
            let state = admin_json(&home, "/admin/v1/state")?;
            let shell = latest_shell(&shell_log)?;
            anyhow::ensure!(
                shell["state"] != "confirm-credits",
                "off incorrectly requested confirmation"
            );
            if state["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == last_id && a["allow_credits"] == false)
                && shell["state"] == "menu"
                && shell["menu_checked"] == false
                && shell["menu_pending"] == false
            {
                rows = qb_tray::parse_state(&serde_json::to_string(&state)?)?;
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "direct off failed/menu closed: {shell}"
            );
            thread::sleep(Duration::from_millis(100));
        }
        capture_checked(hwnd, &evidence.join("credits-off-menu-open.png"))?;
    }
    capture_checked(hwnd, &evidence.join("row-menu.png"))?;
    fs::write(&delay_file, b"delay")?;
    let pause = target(
        hwnd,
        &rows,
        &menu_state,
        |t| matches!(t,Target::Menu{id:Some(id),action:MenuAction::Pause} if id==&last_id),
    )?;
    click_target(hwnd, &rows, &menu_state, pause, WM_LBUTTONUP)?;
    // Preserve the six-second delayed-admin acceptance and keep checking that
    // the UI responds while its synthetic network action is still pending.
    for _ in 0..6 {
        thread::sleep(Duration::from_secs(1));
        responsive(hwnd)?;
    }
    let _ = fs::remove_file(&delay_file);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state = admin_json(&home, "/admin/v1/state")
            .map_err(|error| anyhow::anyhow!("pause poll: {error}"))?;
        if state["accounts"]
            .as_array()
            .and_then(|a| a.get(row_count.saturating_sub(1)))
            .and_then(|a| a["user_state"].as_str())
            == Some("paused")
        {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "row 2 pause action did not reach admin API"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let updated = admin_json(&home, "/admin/v1/state")?;
    rows = qb_tray::parse_state(&serde_json::to_string(&updated)?)?;
    // The artificial five-second delay may expire a concurrently read state.
    // Dismiss its safe error view, then require a fresh healthy publication;
    // never accept a transient empty/error flyout as the quit-menu evidence.
    reset_navigation();
    let prior = latest_shell(&shell_log)?["sequence"].as_u64().unwrap_or(0);
    send(hwnd, WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        responsive(hwnd)?;
        let shell = latest_shell(&shell_log)?;
        if shell["sequence"].as_u64().is_some_and(|n| n > prior)
            && shell["accounts"].as_u64() == Some(row_count as u64)
            && shell["badge"].as_str().is_some_and(|s| s != "error")
            && shell["state"] == "list"
        {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "tray did not recover its account list after delayed admin"
        );
        thread::sleep(Duration::from_millis(50));
    }
    painted(hwnd)?;
    // Exercise settings and tray-only quit on the recovered live list.
    let settings = footer(&rows, FooterAction::Settings)?;
    click_target(hwnd, &rows, "list", settings, WM_LBUTTONUP)?;
    thread::sleep(Duration::from_millis(150));
    send(hwnd, WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
    let quit_footer = footer(&rows, FooterAction::Quit)?;
    // Compare against the same paused list/scroll position, rather than the
    // initial unpaused list (which already differs before opening any popup).
    painted(hwnd)?;
    // Footer is sticky; no Home/End scrolling or whole-window translation.
    let before_quit = evidence.join("before-quit.png");
    capture_checked(hwnd, &before_quit)?;
    let list_bytes = fs::read(&before_quit)?;
    click_target(hwnd, &rows, "list", quit_footer, WM_LBUTTONUP)?;
    let quit_path = evidence.join("quit-menu.png");
    capture_checked(hwnd, &quit_path)?;
    anyhow::ensure!(
        fs::read(&quit_path)? != list_bytes,
        "quit menu did not repaint"
    );
    let tray_only = target(hwnd, &rows, "quit", |t| {
        matches!(
            t,
            Target::Menu {
                action: MenuAction::QuitTray,
                ..
            }
        )
    })?;
    click_target(hwnd, &rows, "quit", tray_only, WM_LBUTTONUP)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if children.tray.try_wait()?.is_some() {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "tray-only quit did not exit"
        );
        thread::sleep(Duration::from_millis(100));
    }
    anyhow::ensure!(
        children.core.try_wait()?.is_none(),
        "tray-only quit stopped the core"
    );
    for _ in 0..50 {
        if window(children.tray.id()).is_none() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    anyhow::ensure!(
        window(children.tray.id()).is_none(),
        "owned quit tray window still exists"
    );
    fs::rename(&shell_log, evidence.join("first-tray-shell.jsonl"))?;
    let tray = Command::new(&tray_bin)
        .env("QB_HOME", &home)
        .env("QB_TRAY_TEST_NO_BROWSER", "1")
        .env("QB_TRAY_TEST_KEEP_OPEN", "1")
        .env("QB_TRAY_TEST_DELAY_FILE", &delay_file)
        .env("QB_TRAY_TEST_SHELL_LOG", &shell_log)
        .args([
            "--no-supervise",
            "--open-flyout-live",
            "--now",
            "2030-01-01T00:00:00Z",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    children.tray = tray;
    reset_navigation();
    let hwnd = (0..100)
        .find_map(|_| {
            let found = window(children.tray.id());
            if found.is_none() {
                thread::sleep(Duration::from_millis(100));
            }
            found
        })
        .ok_or_else(|| anyhow::anyhow!("login flyout window not found"))?;
    thread::sleep(Duration::from_secs(1));
    responsive(hwnd)?;
    responsive(hwnd)?;
    let add = footer(&rows, FooterAction::Add)?;
    click_target(hwnd, &rows, "list", add, WM_LBUTTONUP)?;
    let codex = target(hwnd, &rows, "add-menu", |t| {
        matches!(
            t,
            Target::Menu {
                action: MenuAction::AddCodex,
                ..
            }
        )
    })?;
    click_target(hwnd, &rows, "add-menu", codex, WM_LBUTTONUP)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if fs::read_to_string(&login_log)
            .map(|value| value.starts_with("pending "))
            .unwrap_or(false)
        {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "add-account did not start a pending login"
        );
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_millis(200));
    capture_checked(hwnd, &evidence.join("login-panel.png"))?;
    responsive(hwnd)?;
    let cancel = target(hwnd, &rows, "login:codex", |t| {
        matches!(t, Target::LoginCancel)
    })?;
    click_target(hwnd, &rows, "login:codex", cancel, WM_LBUTTONUP)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if fs::read_to_string(&login_log)
            .map(|value| value.trim() == "cancelled")
            .unwrap_or(false)
        {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "login cancel did not reach the live tray"
        );
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
