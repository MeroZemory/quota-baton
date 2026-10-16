use anyhow::Result;
use std::os::windows::process::CommandExt;
use std::{
    ffi::c_void,
    mem::size_of,
    path::PathBuf,
    process::Command,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{
            Direct2D::{
                Common::D2D_SIZE_U, D2D1_FACTORY_TYPE_SINGLE_THREADED,
                D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
                D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1CreateFactory,
                ID2D1Factory,
            },
            Dwm::{
                DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
                DWMWCP_ROUND, DwmSetWindowAttribute,
            },
            Gdi::InvalidateRect,
            Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect},
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            Shell::{
                NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
                NIM_MODIFY, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
                DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, HMENU, IDC_ARROW,
                IDC_HAND, IDI_APPLICATION, IsWindowVisible, LoadCursorW, LoadIconW, MSG,
                PostMessageW, PostQuitMessage, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE,
                SetCursor, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow,
                TranslateMessage, WM_APP, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN,
                WM_KILLFOCUS, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WM_SETTINGCHANGE,
                WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
    core::{PCWSTR, w},
};

const TRAY_MESSAGE: u32 = WM_APP + 1;
const ACTION_DONE: u32 = WM_APP + 2;
const STATE_CHANGED: u32 = WM_APP + 3;
const WM_MOUSELEAVE_MESSAGE: u32 = 0x02A3;
const WINDOW_CLASS: PCWSTR = w!("QuotaBatonTrayFlyout");
static MODEL: OnceLock<Mutex<(Vec<qb_tray::Row>, String, bool)>> = OnceLock::new();
static VIEW_STATE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SUPERVISOR: OnceLock<Mutex<Supervisor>> = OnceLock::new();
static SUPERVISE_ENABLED: AtomicBool = AtomicBool::new(true);
static FIXTURE_MODE: AtomicBool = AtomicBool::new(false);
static LOGIN_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static LOGIN_URL: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn login_id() -> &'static Mutex<Option<String>> {
    LOGIN_ID.get_or_init(|| Mutex::new(None))
}

fn login_url() -> &'static Mutex<Option<String>> {
    LOGIN_URL.get_or_init(|| Mutex::new(None))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HitTarget {
    RowMenu(String),
    RowRelogin(String),
    MenuItem(String, usize),
    ConfirmDelete(String, bool),
    LoginCopy,
    LoginCancel,
    FooterAdd,
    FooterSettings,
    FooterLogs,
    FooterQuit,
    QuitChoice(bool),
    QuitCancel,
    AddChoice(Option<String>),
    Disclosure(String),
}

fn point_from_lparam(value: LPARAM) -> (f32, f32) {
    let raw = value.0 as u32;
    ((raw as i16) as f32, (((raw >> 16) as u16) as i16) as f32)
}

fn hit_target(x: f32, y: f32) -> Option<HitTarget> {
    let rows = model().lock().unwrap_or_else(|e| e.into_inner()).0.clone();
    let state = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let targets = qb_tray::layout_targets(
        &rows,
        state.as_deref() == Some("expanded"),
        state.as_deref(),
    );
    if let Some(target) = qb_tray::hit_test(&targets, x, y).cloned() {
        return Some(match target {
            qb_tray::HitTarget::RowMenu { id, .. } => HitTarget::RowMenu(id),
            qb_tray::HitTarget::RowRelogin { id, .. } => HitTarget::RowRelogin(id),
            qb_tray::HitTarget::MenuItem { id, index, .. } => HitTarget::MenuItem(id, index),
            qb_tray::HitTarget::Disclosure { provider, .. } => HitTarget::Disclosure(provider),
            qb_tray::HitTarget::Footer { action, .. } => match action {
                qb_tray::FooterAction::AddAccount => HitTarget::FooterAdd,
                qb_tray::FooterAction::Settings => HitTarget::FooterSettings,
                qb_tray::FooterAction::Logs => HitTarget::FooterLogs,
                qb_tray::FooterAction::Quit => HitTarget::FooterQuit,
            },
            qb_tray::HitTarget::Login { action, .. } => match action {
                qb_tray::LoginAction::CopyAddress => HitTarget::LoginCopy,
                qb_tray::LoginAction::Cancel => HitTarget::LoginCancel,
            },
            qb_tray::HitTarget::Quit { action, .. } => match action {
                qb_tray::QuitAction::TrayOnly => HitTarget::QuitChoice(true),
                qb_tray::QuitAction::TrayAndProxy => HitTarget::QuitChoice(false),
                qb_tray::QuitAction::Cancel => HitTarget::QuitCancel,
            },
            qb_tray::HitTarget::Dialog { action, .. } => match action {
                qb_tray::DialogAction::ConfirmDelete => HitTarget::ConfirmDelete(
                    state
                        .as_deref()
                        .and_then(|s| s.strip_prefix("confirm-delete:"))
                        .unwrap_or_default()
                        .to_owned(),
                    true,
                ),
                qb_tray::DialogAction::Cancel => HitTarget::ConfirmDelete(
                    state
                        .as_deref()
                        .and_then(|s| s.strip_prefix("confirm-delete:"))
                        .unwrap_or_default()
                        .to_owned(),
                    false,
                ),
            },
            qb_tray::HitTarget::Row { .. } => return None,
        });
    }
    if let Some(value) = state.as_deref() {
        if let Some(rest) = value.strip_prefix("menu:") {
            let mut pieces = rest.split(':');
            let id = pieces.next()?.to_owned();
            let _selected = pieces.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            if (136.0..=352.0).contains(&x) && (126.0..312.0).contains(&y) {
                let count = rows
                    .iter()
                    .find(|row| row.id == id)
                    .map(|row| qb_tray::actions::menu_items(row).len())
                    .unwrap_or(0);
                let index = ((y - 164.0) / 27.0).floor().max(0.0) as usize;
                if index < count
                    && y >= 164.0 + index as f32 * 27.0
                    && y < 190.0 + index as f32 * 27.0
                {
                    return Some(HitTarget::MenuItem(id, index));
                }
            }
            return None;
        }
        if let Some(id) = value.strip_prefix("confirm-delete:") {
            if (16.0..344.0).contains(&x) && y > flyout_height() as f32 - 64.0 {
                return Some(HitTarget::ConfirmDelete(id.to_owned(), x < 180.0));
            }
            return None;
        }
        if value.starts_with("login:") {
            if (176.0..208.0).contains(&y) {
                return Some(if x < 180.0 {
                    HitTarget::LoginCopy
                } else {
                    HitTarget::LoginCancel
                });
            }
            return None;
        }
        if value == "add-menu" {
            if (128.0..352.0).contains(&x)
                && (flyout_height() as f32 - 180.0..flyout_height() as f32 - 70.0).contains(&y)
            {
                let top = flyout_height() as f32 - 180.0;
                return Some(if y < top + 36.0 {
                    HitTarget::AddChoice(Some("claude".into()))
                } else if y < top + 72.0 {
                    HitTarget::AddChoice(Some("codex".into()))
                } else {
                    HitTarget::AddChoice(None)
                });
            }
            return None;
        }
        if value == "quit" {
            let top = flyout_height() as f32 - 124.0;
            if (top..top + 72.0).contains(&y) {
                return Some(HitTarget::QuitChoice(y < top + 32.0));
            }
            return None;
        }
    }
    let mut cursor = 52.0f32;
    for provider in ["Claude", "Codex"] {
        let active = rows
            .iter()
            .filter(|row| row.provider == provider && row.user_chip != "비활성")
            .collect::<Vec<_>>();
        let disabled = rows
            .iter()
            .filter(|row| row.provider == provider && row.user_chip == "비활성")
            .collect::<Vec<_>>();
        if active.is_empty() && disabled.is_empty() {
            continue;
        }
        cursor += 36.0;
        for row in active {
            let height = qb_tray::row_height(row);
            if (cursor..cursor + height).contains(&y) {
                if (320.0..360.0).contains(&x) {
                    return Some(HitTarget::RowMenu(row.id.clone()));
                }
                if row.reason_button && x >= 258.0 && y >= cursor + height - 32.0 {
                    return Some(HitTarget::RowRelogin(row.id.clone()));
                }
            }
            cursor += height;
        }
        if !disabled.is_empty() {
            if (cursor..cursor + 32.0).contains(&y) {
                return Some(HitTarget::Disclosure(provider.to_owned()));
            }
            cursor += 32.0;
        }
    }
    let footer = flyout_height() as f32 - 48.0;
    if (footer + 8.0..footer + 40.0).contains(&y) {
        return if (88.0..169.0).contains(&x) {
            Some(HitTarget::FooterAdd)
        } else if (169.0..227.0).contains(&x) {
            Some(HitTarget::FooterSettings)
        } else if (227.0..283.0).contains(&x) {
            Some(HitTarget::FooterLogs)
        } else if (283.0..360.0).contains(&x) {
            Some(HitTarget::FooterQuit)
        } else {
            None
        };
    }
    None
}

struct Supervisor {
    last_start: Option<Instant>,
    delay: Duration,
    unreachable_since: Option<Instant>,
}

fn supervisor() -> &'static Mutex<Supervisor> {
    SUPERVISOR.get_or_init(|| {
        Mutex::new(Supervisor {
            last_start: None,
            delay: Duration::from_secs(5),
            unreachable_since: None,
        })
    })
}

unsafe fn notify_balloon(hwnd: HWND, title: &str, message: &str) {
    let mut icon = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_INFO,
        ..Default::default()
    };
    let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let message: Vec<u16> = message.encode_utf16().chain([0]).collect();
    let title_len = title.len().min(icon.szInfoTitle.len());
    let message_len = message.len().min(icon.szInfo.len());
    icon.szInfoTitle[..title_len].copy_from_slice(&title[..title_len]);
    icon.szInfo[..message_len].copy_from_slice(&message[..message_len]);
    let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &icon) };
}

fn model() -> &'static Mutex<(Vec<qb_tray::Row>, String, bool)> {
    MODEL.get_or_init(|| Mutex::new((Vec::new(), "v0.1.0".to_owned(), false)))
}

fn view_state() -> &'static Mutex<Option<String>> {
    VIEW_STATE.get_or_init(|| Mutex::new(None))
}

fn flyout_height() -> i32 {
    let model = model().lock().unwrap_or_else(|e| e.into_inner());
    let state = view_state().lock().unwrap_or_else(|e| e.into_inner());
    qb_tray::native_render::state_height(&model.0, false, state.as_deref())
        .ceil()
        .clamp(120.0, 700.0) as i32
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn position_flyout(hwnd: HWND) {
    let identifier = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    let icon = Shell_NotifyIconGetRect(&identifier).unwrap_or_else(|_| {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        RECT {
            left: cursor.x,
            top: cursor.y,
            right: cursor.x + 1,
            bottom: cursor.y + 1,
        }
    });
    let monitor = MonitorFromRect(&icon, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = GetMonitorInfoW(monitor, &mut info);
    let height = flyout_height();
    let width = 360;
    let x = (icon.right - width).clamp(info.rcWork.left, info.rcWork.right - width);
    let y = (icon.top - height).max(info.rcWork.top);
    let _ = SetWindowPos(hwnd, None, x, y, width, height, Default::default());
}

fn update_status(supervise: bool, notification_hwnd: Option<HWND>) {
    let result = (|| -> Result<String> {
        let client = super::client::AdminClient::from_home()?;
        let runtime = tokio::runtime::Runtime::new()?;
        let read = runtime
            .block_on(client.read())
            .map_err(|e| anyhow::anyhow!(e))?;
        let rows = super::client::layout_rows(&read, std::time::SystemTime::now())?;
        *model().lock().unwrap_or_else(|e| e.into_inner()) =
            (rows, read.snapshot.version.clone(), true);
        Ok(super::client::render_flyout(&read))
    })();
    let text = match result {
        Ok(text) => {
            let mut state = supervisor().lock().unwrap_or_else(|e| e.into_inner());
            state.last_start = None;
            state.delay = Duration::from_secs(5);
            state.unreachable_since = None;
            text
        }
        Err(_) => {
            if supervise {
                let mut state = supervisor().lock().unwrap_or_else(|e| e.into_inner());
                let since = *state.unreachable_since.get_or_insert_with(Instant::now);
                let due = state
                    .last_start
                    .is_none_or(|at| at.elapsed() >= state.delay);
                if since.elapsed() >= Duration::from_secs(30) && due {
                    // Detached and hidden; an existing core is never terminated.
                    let started = Command::new("quota-baton")
                        .args(["serve"])
                        .creation_flags(0x0800_0000)
                        .spawn()
                        .is_ok();
                    if started {
                        state.last_start = Some(Instant::now());
                        state.delay = (state.delay * 2).min(Duration::from_secs(300));
                        unsafe {
                            if let Some(hwnd) = notification_hwnd {
                                notify_balloon(
                                    hwnd,
                                    "quota-baton",
                                    "코어가 예기치 않게 종료되어 다시 시작했습니다",
                                );
                            }
                        }
                    }
                }
            }
            {
                let mut model = model().lock().unwrap_or_else(|e| e.into_inner());
                model.0.clear();
                model.1 = "error:코어 상태를 읽지 못했습니다 · 로그 열기".to_owned();
                model.2 = false;
            }
            "코어에 연결할 수 없습니다".to_owned()
        }
    };
    if text == "코어에 연결할 수 없습니다" {
        let mut model = model().lock().unwrap_or_else(|e| e.into_inner());
        model.0.clear();
        model.2 = false;
    }
}

fn paint_window(hwnd: HWND) -> Result<()> {
    let (rows, version, core_ok) = model().lock().unwrap_or_else(|e| e.into_inner()).clone();
    unsafe {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U {
                width: 360,
                height: flyout_height() as u32,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let target = factory.CreateHwndRenderTarget(&props, &hwnd_props)?;
        let review_state = view_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        qb_tray::native_render::paint_with_state(
            &target,
            &rows,
            false,
            &version,
            SystemTime::now(),
            false,
            core_ok,
            review_state.as_deref(),
        )?;
    }
    Ok(())
}

fn menu_target() -> Option<(qb_tray::Row, usize)> {
    let state = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let rest = state.strip_prefix("menu:")?;
    let mut parts = rest.split(':');
    let id = parts.next()?;
    let index = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let row = model()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .iter()
        .find(|row| row.id == id)
        .cloned()?;
    Some((row, index))
}

fn change_menu_selection(delta: isize) {
    let Some((row, index)) = menu_target() else {
        return;
    };
    let count = qb_tray::actions::menu_items(&row).len();
    if count == 0 {
        return;
    }
    let next = (index as isize + delta).rem_euclid(count as isize) as usize;
    let mut state = view_state().lock().unwrap_or_else(|e| e.into_inner());
    *state = Some(format!("menu:{}:{next}", row.id));
}

fn activate_menu(hwnd: HWND) {
    let Some((row, index)) = menu_target() else {
        return;
    };
    let Some(item) = qb_tray::actions::menu_items(&row).get(index).copied() else {
        return;
    };
    if item.is_destructive() {
        *view_state().lock().unwrap_or_else(|e| e.into_inner()) =
            Some(format!("confirm-delete:{}", row.id));
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
        return;
    }
    let operation = qb_tray::actions::admin_operation(item, &row);
    let provider = row.provider.clone();
    let id = row.id.clone();
    if item == qb_tray::actions::MenuItem::Relogin {
        launch_login(hwnd, provider.to_ascii_lowercase());
        return;
    }
    let hwnd_value = hwnd.0 as isize;
    let _ = std::thread::Builder::new()
        .name("quota-baton-tray-action".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let client = super::client::AdminClient::from_home()?;
                let runtime = tokio::runtime::Runtime::new()?;
                runtime.block_on(async {
                    match operation {
                        Some(qb_tray::actions::AdminOperation::Account(action)) => client
                            .account_action(
                                &id,
                                super::client::AccountAction::from_str(action)
                                    .expect("typed action"),
                            )
                            .await
                            .map(|_| ()),
                        Some(qb_tray::actions::AdminOperation::Drain(value)) => client
                            .patch_account(
                                &id,
                                super::client::AccountPatch {
                                    drain: Some(value),
                                    ..Default::default()
                                },
                            )
                            .await
                            .map(|_| ())
                            .map(|_| ()),
                        Some(qb_tray::actions::AdminOperation::Credits(value)) => client
                            .patch_account(
                                &id,
                                super::client::AccountPatch {
                                    allow_credits: Some(value),
                                    ..Default::default()
                                },
                            )
                            .await
                            .map(|_| ())
                            .map(|_| ()),
                        Some(qb_tray::actions::AdminOperation::Relogin) => {
                            client.start_login(&provider).await.map(|_| ())
                        }
                        Some(qb_tray::actions::AdminOperation::Delete) => {
                            client.delete_account(&id).await
                        }
                        None => Ok(()),
                    }
                    .map_err(|e| anyhow::anyhow!(e))
                })
            })();
            if result.is_ok() {
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(hwnd_value as *mut c_void)),
                        ACTION_DONE,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        });
}

fn launch_login(hwnd: HWND, provider: String) {
    let hwnd_value = hwnd.0 as isize;
    let _ = std::thread::Builder::new()
        .name("quota-baton-tray-login".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let client = super::client::AdminClient::from_home()?;
                let runtime = tokio::runtime::Runtime::new()?;
                let start = runtime
                    .block_on(client.start_login(&provider))
                    .map_err(|e| anyhow::anyhow!(e))?;
                *login_id().lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(start.login_id.clone());
                *login_url().lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(start.auth_url.clone());
                *view_state().lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(format!("login:{provider}"));
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(hwnd_value as *mut c_void)),
                        STATE_CHANGED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
                // The browser is a product action; tests set this hook to avoid opening one.
                if std::env::var_os("QB_TRAY_TEST_NO_BROWSER").is_none() {
                    let _ = Command::new("cmd")
                        .args(["/C", "start", "", &start.auth_url])
                        .spawn();
                }
                for _ in 0..600 {
                    std::thread::sleep(Duration::from_secs(1));
                    let status = runtime
                        .block_on(client.login_status(&start.login_id))
                        .map_err(|e| anyhow::anyhow!(e))?;
                    if matches!(
                        status.status.as_str(),
                        "completed" | "failed" | "expired" | "cancelled"
                    ) {
                        *view_state().lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("login:{provider}:{}", status.status));
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(hwnd_value as *mut c_void)),
                                STATE_CHANGED,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                        break;
                    }
                }
                *login_id().lock().unwrap_or_else(|e| e.into_inner()) = None;
                Ok(())
            })();
            if result.is_ok() {
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(hwnd_value as *mut c_void)),
                        ACTION_DONE,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        });
}

fn cancel_login() {
    let id = login_id().lock().unwrap_or_else(|e| e.into_inner()).take();
    *login_url().lock().unwrap_or_else(|e| e.into_inner()) = None;
    if let Some(id) = id {
        let _ = std::thread::Builder::new()
            .name("quota-baton-tray-login-cancel".into())
            .spawn(move || {
                if let Ok(client) = super::client::AdminClient::from_home()
                    && let Ok(runtime) = tokio::runtime::Runtime::new()
                {
                    let _ = runtime.block_on(client.cancel_login(&id));
                }
            });
    }
}

fn activate_hit(hwnd: HWND, target: HitTarget) {
    match target {
        HitTarget::RowMenu(id) => {
            *view_state().lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("menu:{id}:0"));
        }
        HitTarget::RowRelogin(id) => {
            let provider = model()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.provider.to_ascii_lowercase())
                .unwrap_or_else(|| "codex".into());
            launch_login(hwnd, provider);
        }
        HitTarget::MenuItem(id, index) => {
            let row = model()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
                .iter()
                .find(|row| row.id == id)
                .cloned();
            if let Some(row) = row
                && let Some(item) = qb_tray::actions::menu_items(&row).get(index).copied()
            {
                if item.is_destructive() {
                    *view_state().lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("confirm-delete:{id}"));
                } else if item == qb_tray::actions::MenuItem::Relogin {
                    launch_login(hwnd, row.provider.to_ascii_lowercase());
                } else {
                    *view_state().lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("menu:{id}:{index}"));
                    activate_menu(hwnd);
                }
            }
        }
        HitTarget::ConfirmDelete(id, yes) => {
            if yes {
                let row = model()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .0
                    .iter()
                    .find(|row| row.id == id)
                    .cloned();
                if let Some(row) = row {
                    let provider = row.provider.clone();
                    let hwnd_value = hwnd.0 as isize;
                    let _ = std::thread::Builder::new()
                        .name("quota-baton-tray-delete".into())
                        .spawn(move || {
                            let _ = (|| -> anyhow::Result<()> {
                                let client = super::client::AdminClient::from_home()?;
                                let runtime = tokio::runtime::Runtime::new()?;
                                runtime
                                    .block_on(client.delete_account(&id))
                                    .map_err(|e| anyhow::anyhow!(e))?;
                                Ok(())
                            })();
                            unsafe {
                                let _ = PostMessageW(
                                    Some(HWND(hwnd_value as *mut c_void)),
                                    ACTION_DONE,
                                    WPARAM(0),
                                    LPARAM(0),
                                );
                            };
                            let _ = provider;
                        });
                }
            } else {
                *view_state().lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        HitTarget::LoginCopy => {
            if std::env::var_os("QB_TRAY_TEST_NO_CLIPBOARD").is_none()
                && let Some(url) = login_url()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            {
                let _ = Command::new("powershell.exe")
                    .args(["-NoProfile", "-Command", "Set-Clipboard -Value $args[0]"])
                    .arg(url)
                    .spawn();
            }
        }
        HitTarget::LoginCancel => {
            cancel_login();
            *view_state().lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        HitTarget::FooterAdd => {
            *view_state().lock().unwrap_or_else(|e| e.into_inner()) = Some("add-menu".into());
        }
        HitTarget::FooterSettings => {
            open_folder(".");
        }
        HitTarget::FooterLogs => {
            open_folder("logs");
        }
        HitTarget::FooterQuit => {
            *view_state().lock().unwrap_or_else(|e| e.into_inner()) = Some("quit".into());
        }
        HitTarget::QuitChoice(tray_only) => {
            if tray_only {
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            } else {
                let hwnd_value = hwnd.0 as isize;
                let _ = std::thread::Builder::new()
                    .name("quota-baton-tray-shutdown".into())
                    .spawn(move || {
                        if let Ok(client) = super::client::AdminClient::from_home()
                            && let Ok(runtime) = tokio::runtime::Runtime::new()
                        {
                            let _ = runtime.block_on(client.shutdown(30));
                        }
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(hwnd_value as *mut c_void)),
                                WM_CLOSE,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    });
            }
        }
        HitTarget::AddChoice(provider) => {
            if let Some(provider) = provider {
                launch_login(hwnd, provider);
            } else {
                *view_state().lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        HitTarget::QuitCancel => {
            *view_state().lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        HitTarget::Disclosure(provider) => {
            let _ = provider;
            let mut state = view_state().lock().unwrap_or_else(|e| e.into_inner());
            *state = Some(if state.as_deref() == Some("expanded") {
                "list".into()
            } else {
                "expanded".into()
            });
        }
    }
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

fn open_folder(relative: &str) {
    let home = qb_core::home().unwrap_or_else(|_| PathBuf::from("."));
    let path = if relative == "." {
        home
    } else {
        home.join(relative)
    };
    if std::env::var_os("QB_TRAY_TEST_NO_EXPLORER").is_none() {
        let _ = Command::new("explorer.exe").arg(path).spawn();
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        TRAY_MESSAGE if lparam.0 as u32 == WM_LBUTTONUP || lparam.0 as u32 == WM_RBUTTONUP => {
            if IsWindowVisible(hwnd).as_bool() {
                if lparam.0 as u32 == WM_RBUTTONUP {
                    if let Some(row) = model().lock().unwrap_or_else(|e| e.into_inner()).0.first() {
                        *view_state().lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("menu:{}:0", row.id));
                        let _ = InvalidateRect(Some(hwnd), None, true);
                    }
                } else {
                    *view_state().lock().unwrap_or_else(|e| e.into_inner()) = None;
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                return LRESULT(0);
            }
            if !FIXTURE_MODE.load(Ordering::Relaxed) {
                update_status(SUPERVISE_ENABLED.load(Ordering::Relaxed), Some(hwnd));
            }
            position_flyout(hwnd);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetForegroundWindow(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP if IsWindowVisible(hwnd).as_bool() => {
            let (x, y) = point_from_lparam(lparam);
            if let Some(target) = hit_target(x, y) {
                activate_hit(hwnd, target);
            } else if view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
            {
                *view_state().lock().unwrap_or_else(|e| e.into_inner()) = None;
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP if IsWindowVisible(hwnd).as_bool() => {
            let (x, y) = point_from_lparam(lparam);
            if let Some(HitTarget::RowMenu(id)) = hit_target(x, y) {
                *view_state().lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(format!("menu:{id}:0"));
                let _ = InvalidateRect(Some(hwnd), None, true);
            } else {
                *view_state().lock().unwrap_or_else(|e| e.into_inner()) = None;
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE if IsWindowVisible(hwnd).as_bool() => {
            let mut tracking = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                ..Default::default()
            };
            let _ = TrackMouseEvent(&mut tracking);
            let (x, y) = point_from_lparam(lparam);
            let cursor = if hit_target(x, y).is_some() {
                LoadCursorW(None, IDC_HAND)
            } else {
                LoadCursorW(None, IDC_ARROW)
            };
            if let Ok(cursor) = cursor {
                let _ = SetCursor(Some(cursor));
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MESSAGE => {
            let _ = SetCursor(LoadCursorW(None, IDC_ARROW).ok());
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            if view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u32 == 0x1b => {
            let mut state = view_state().lock().unwrap_or_else(|e| e.into_inner());
            if state.take().is_none() {
                let _ = ShowWindow(hwnd, SW_HIDE);
            } else {
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_KEYDOWN
            if view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
                .is_some_and(|s| s.starts_with("menu:"))
                && wparam.0 as u32 == 0x26 =>
        {
            change_menu_selection(-1);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_KEYDOWN
            if view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
                .is_some_and(|s| s.starts_with("menu:"))
                && wparam.0 as u32 == 0x28 =>
        {
            change_menu_selection(1);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_KEYDOWN
            if view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
                .is_some_and(|s| s.starts_with("menu:"))
                && (wparam.0 as u32 == 0x0d || wparam.0 as u32 == 0x20) =>
        {
            activate_menu(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        ACTION_DONE => {
            update_status(false, Some(hwnd));
            *view_state().lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        STATE_CHANGED => {
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            if lparam.0 != 0 {
                let suggested = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    Default::default(),
                );
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_TIMER => {
            update_status(SUPERVISE_ENABLED.load(Ordering::Relaxed), Some(hwnd));
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_PAINT => {
            let _ = paint_window(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

pub fn run(
    supervise: bool,
    open_flyout: bool,
    fixture: Option<PathBuf>,
    now: SystemTime,
) -> Result<()> {
    SUPERVISE_ENABLED.store(supervise, Ordering::Relaxed);
    if let Some(fixture) = fixture {
        let json = std::fs::read_to_string(fixture)?;
        let state: qb_tray::State = serde_json::from_str(&json)?;
        let rows = qb_tray::parse_state_at(&json, now)?;
        *model().lock().unwrap_or_else(|e| e.into_inner()) = (rows, state.version, true);
        FIXTURE_MODE.store(true, Ordering::Relaxed);
    } else {
        FIXTURE_MODE.store(false, Ordering::Relaxed);
        update_status(supervise, None);
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hIcon: LoadIconW(None, IDI_APPLICATION)?,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            WINDOW_CLASS,
            w!("quota-baton"),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            360,
            flyout_height(),
            None,
            Some(HMENU::default()),
            Some(instance),
            None::<*const c_void>,
        )?;
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            size_of_val(&corner) as u32,
        );
        let dark_mode: i32 = 0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark_mode as *const _ as *const c_void,
            size_of_val(&dark_mode) as u32,
        );
        let border_color: u32 = 0x00D9D9D9;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border_color as *const _ as *const c_void,
            size_of_val(&border_color) as u32,
        );
        SetTimer(Some(hwnd), 1, 60_000, None);
        let mut icon = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: TRAY_MESSAGE,
            hIcon: class.hIcon,
            ..Default::default()
        };
        let tip: Vec<u16> = "quota-baton".encode_utf16().chain([0]).collect();
        icon.szTip[..tip.len()].copy_from_slice(&tip);
        if !Shell_NotifyIconW(NIM_ADD, &icon).as_bool() {
            anyhow::bail!("트레이 아이콘을 만들 수 없습니다");
        }
        if open_flyout {
            position_flyout(hwnd);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetForegroundWindow(hwnd);
        }
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
        DestroyWindow(hwnd)?;
    }
    Ok(())
}
