//! Manual Windows acceptance harness for M6c.
//!
//! It is ignored by default because it starts two real GUI processes. Set
//! `M6C_CORE_BIN`, `M6C_TRAY_BIN`, and `M6C_HOME` to synthetic test fixtures
//! before running it. No real profile is ever selected by this test.

#![cfg(windows)]

use chrono::{TimeZone, Utc};
use qb_core::accounts::{
    Account, AccountStore, Health, Identity, Provider, Source, TokenMeta, Tokens, UserState,
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
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY,
            SelectObject,
        },
        Storage::Xps::PrintWindow,
        UI::WindowsAndMessaging::{
            FindWindowW, GetClientRect, GetWindowRect, PostMessageW, SMTO_ABORTIFHUNG, SW_SHOW,
            SendMessageTimeoutW, SetForegroundWindow, ShowWindow, WM_LBUTTONUP, WM_NULL,
            WM_RBUTTONUP,
        },
    },
    core::w,
};

fn window() -> Option<HWND> {
    unsafe { FindWindowW(None, w!("quota-baton")).ok() }
}

fn click(hwnd: HWND, x: i16, y: i16, message: u32) {
    let packed = ((y as u16 as u32) << 16) | x as u16 as u32;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(packed as isize));
    }
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
    let error_log = home.join("tray-error.log");
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
            let found = window();
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
    let mut list_targets = qb_tray::layout_targets(&rows, false, Some("list"));
    let mut footer_center = list_targets
        .iter()
        .find_map(|target| match target {
            qb_tray::HitTarget::Footer {
                action: qb_tray::FooterAction::AddAccount,
                rect,
            } => Some((
                (rect.left + rect.right) / 2.0,
                (rect.top + rect.bottom) / 2.0,
            )),
            _ => None,
        })
        .unwrap();
    let last_id = rows.last().map(|row| row.id.clone()).unwrap();
    let last_y = 52.0
        + 36.0
        + rows[..rows.len() - 1]
            .iter()
            .map(qb_tray::row_height)
            .sum::<f32>()
        + 22.0;
    for y in ((footer_center.1 as i16).saturating_sub(24)
        ..(footer_center.1 as i16).saturating_add(25))
        .step_by(4)
    {
        responsive(hwnd)?;
        click(hwnd, 195, y, WM_LBUTTONUP);
        thread::sleep(Duration::from_millis(35));
        if explorer_log.exists() {
            break;
        }
    }
    anyhow::ensure!(explorer_log.exists(), "settings test hook was not called");
    responsive(hwnd)?;
    click(hwnd, 330, last_y as i16, WM_RBUTTONUP);
    thread::sleep(Duration::from_millis(100));
    thread::sleep(Duration::from_millis(200));
    capture_checked(hwnd, &evidence.join("row-menu.png"))?;
    fs::write(&delay_file, b"delay")?;
    responsive(hwnd)?;
    let menu_height = qb_tray::state_height(&rows, false, Some(&format!("menu:{last_id}")));
    let menu = qb_tray::popup_rect(&rows, false, &format!("menu:{last_id}"), menu_height).unwrap();
    click(hwnd, 220, (menu.top + 20.0) as i16, WM_LBUTTONUP);
    thread::sleep(Duration::from_secs(6));
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
    let list_height = qb_tray::state_height(&rows, false, None)
        .ceil()
        .clamp(120.0, 700.0);
    list_targets = qb_tray::layout_targets(&rows, false, Some("list"));
    footer_center = list_targets
        .iter()
        .find_map(|target| match target {
            qb_tray::HitTarget::Footer {
                action: qb_tray::FooterAction::AddAccount,
                rect,
            } => Some((
                (rect.left + rect.right) / 2.0,
                (rect.top + rect.bottom) / 2.0,
            )),
            _ => None,
        })
        .unwrap();
    // Exercise settings and tray-only quit on the live list.
    thread::sleep(Duration::from_secs(1));
    responsive(hwnd)?;
    let settings_center = list_targets
        .iter()
        .find_map(|target| match target {
            qb_tray::HitTarget::Footer {
                action: qb_tray::FooterAction::Settings,
                rect,
            } => Some((
                (rect.left + rect.right) / 2.0,
                (rect.top + rect.bottom) / 2.0,
            )),
            _ => None,
        })
        .unwrap();
    click(
        hwnd,
        settings_center.0 as i16,
        settings_center.1 as i16,
        WM_LBUTTONUP,
    );
    thread::sleep(Duration::from_millis(150));
    responsive(hwnd)?;
    responsive(hwnd)?;
    let quit_center = list_targets
        .iter()
        .find_map(|target| match target {
            qb_tray::HitTarget::Footer {
                action: qb_tray::FooterAction::Quit,
                rect,
            } => Some((
                (rect.left + rect.right) / 2.0,
                (rect.top + rect.bottom) / 2.0,
            )),
            _ => None,
        })
        .unwrap();
    click(
        hwnd,
        quit_center.0 as i16,
        quit_center.1 as i16,
        WM_LBUTTONUP,
    );
    thread::sleep(Duration::from_millis(300));
    capture_checked(hwnd, &evidence.join("quit-menu.png"))?;
    responsive(hwnd)?;
    let quit_targets = qb_tray::layout_targets(&rows, false, Some("quit"));
    let tray_only_center = quit_targets
        .iter()
        .find_map(|target| match target {
            qb_tray::HitTarget::Quit {
                action: qb_tray::QuitAction::TrayOnly,
                rect,
            } => Some((
                (rect.left + rect.right) / 2.0,
                (rect.top + rect.bottom) / 2.0,
            )),
            _ => None,
        })
        .unwrap();
    click(
        hwnd,
        tray_only_center.0 as i16,
        tray_only_center.1 as i16,
        WM_LBUTTONUP,
    );
    if children.tray.try_wait()?.is_none() {
        let footer = list_targets
            .iter()
            .find_map(|target| match target {
                qb_tray::HitTarget::Footer {
                    action: qb_tray::FooterAction::Quit,
                    rect,
                } => Some(*rect),
                _ => None,
            })
            .unwrap();
        let target = quit_targets
            .iter()
            .find_map(|target| match target {
                qb_tray::HitTarget::Quit {
                    action: qb_tray::QuitAction::TrayOnly,
                    rect,
                } => Some(*rect),
                _ => None,
            })
            .unwrap();
        'retry_quit: for fx in (footer.left as i16..=footer.right as i16).step_by(8) {
            for fy in (footer.top as i16..=footer.bottom as i16).step_by(4) {
                click(hwnd, fx, fy, WM_LBUTTONUP);
                thread::sleep(Duration::from_millis(20));
                for x in (target.left as i16..=target.right as i16).step_by(8) {
                    for y in (target.top as i16..=target.bottom as i16).step_by(4) {
                        click(hwnd, x, y, WM_LBUTTONUP);
                        if children.tray.try_wait()?.is_some() {
                            break 'retry_quit;
                        }
                    }
                }
            }
        }
    }
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
    anyhow::ensure!(
        children.core.try_wait()?.is_none(),
        "tray-only quit stopped the core"
    );
    for _ in 0..50 {
        if window().is_none() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let tray = Command::new(&tray_bin)
        .env("QB_HOME", &home)
        .env("QB_TRAY_TEST_NO_BROWSER", "1")
        .env("QB_TRAY_TEST_KEEP_OPEN", "1")
        .env("QB_TRAY_TEST_DELAY_FILE", &delay_file)
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
    let hwnd = (0..100)
        .find_map(|_| {
            let found = window();
            if found.is_none() {
                thread::sleep(Duration::from_millis(100));
            }
            found
        })
        .ok_or_else(|| anyhow::anyhow!("login flyout window not found"))?;
    thread::sleep(Duration::from_secs(1));
    responsive(hwnd)?;
    responsive(hwnd)?;
    click(
        hwnd,
        footer_center.0 as i16,
        footer_center.1 as i16,
        WM_LBUTTONUP,
    );
    thread::sleep(Duration::from_millis(300));
    responsive(hwnd)?;
    let add_popup = qb_tray::popup_rect(&rows, false, "add-menu", list_height).unwrap();
    click(hwnd, 220, (add_popup.top + 54.0) as i16, WM_LBUTTONUP);
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
    let login_targets = qb_tray::layout_targets(&rows, false, Some("login:codex"));
    let cancel_center = login_targets
        .iter()
        .find_map(|target| match target {
            qb_tray::HitTarget::Login {
                action: qb_tray::LoginAction::Cancel,
                rect,
                ..
            } => Some((
                (rect.left + rect.right) / 2.0,
                (rect.top + rect.bottom) / 2.0,
            )),
            _ => None,
        })
        .unwrap();
    click(
        hwnd,
        cancel_center.0 as i16,
        cancel_center.1 as i16,
        WM_LBUTTONUP,
    );
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
