use crate::{
    notices::{self, Controller},
    tray_icon::{pixels::Badge, win_icon::NotifyIcon},
    viewport::{Viewport, pixel_ceil},
};
use anyhow::Result;
use std::{
    cell::RefCell,
    ffi::c_void,
    mem::size_of,
    path::PathBuf,
    process::Command,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Instant, SystemTime},
};
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{
            Direct2D::{
                Common::D2D_SIZE_U, D2D1_FACTORY_TYPE_SINGLE_THREADED,
                D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
                D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1CreateFactory,
                ID2D1Factory, ID2D1HwndRenderTarget,
            },
            Dwm::{
                DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
                DWMWCP_ROUND, DwmSetWindowAttribute,
            },
            Gdi::{
                BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST,
                MONITORINFO, MonitorFromRect, MonitorFromWindow, PAINTSTRUCT,
            },
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
                SetProcessDpiAwarenessContext,
            },
            Shell::{
                NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD,
                NIM_DELETE, NIM_MODIFY, NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT, NIN_BALLOONUSERCLICK,
                NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
                DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW,
                GetWindowRect, HICON, HMENU, IDC_ARROW, IDC_HAND, IsWindow, IsWindowVisible,
                LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SW_HIDE,
                SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, SetCursor, SetForegroundWindow,
                SetTimer, SetWindowPos, ShowWindow, TranslateMessage, WM_APP, WM_CLOSE, WM_DESTROY,
                WM_DPICHANGED, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
                WM_PAINT, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_SHOWWINDOW, WM_SIZE, WM_TIMER,
                WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
    core::{PCWSTR, w},
};
use windows_numerics::Matrix3x2;

const TRAY_MESSAGE: u32 = WM_APP + 1;
const ACTION_DONE: u32 = WM_APP + 2;
const STATE_CHANGED: u32 = WM_APP + 3;
// Narrow confirmed-restart hook; main owns final notification integration.
const CORE_RESTART_CONFIRMED: u32 = WM_APP + 4;
// WPARAM: validated positive u32 observed session count; LPARAM: zero.
// Main owns final debounced notification integration.
const CORE_BURST_OBSERVED: u32 = WM_APP + 5;
const WM_MOUSELEAVE_MESSAGE: u32 = 0x02A3;
const WINDOW_CLASS: PCWSTR = w!("QuotaBatonTrayFlyout");
static MODEL: OnceLock<Mutex<(Vec<qb_tray::Row>, String, bool)>> = OnceLock::new();
static VIEW_STATE: OnceLock<Mutex<view_ownership::ViewOwnership>> = OnceLock::new();
#[path = "tray_worker.rs"]
mod tray_worker;
#[path = "view_ownership.rs"]
mod view_ownership;
static WORKER: OnceLock<tray_worker::Worker> = OnceLock::new();
static LOGIN_GENERATION: Mutex<u64> = Mutex::new(0);
static SUPERVISE_ENABLED: AtomicBool = AtomicBool::new(true);
static FIXTURE_MODE: AtomicBool = AtomicBool::new(false);
static LOGIN_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static LOGIN_URL: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static NOTICES: OnceLock<Mutex<Controller>> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();
static DARK: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct PaintedFrame {
    rows: Vec<qb_tray::Row>,
    state: Option<String>,
    owner: usize,
    pixels: (u32, u32),
    dpi: u32,
    offset: f32,
}

thread_local! {
    static RENDER_TARGET: RefCell<Option<ID2D1HwndRenderTarget>> = const { RefCell::new(None) };
    static VIEWPORT: RefCell<Viewport> = const { RefCell::new(Viewport { offset: 0.0, content: 0.0, extent: 0.0 }) };
    static PAINTED: RefCell<Option<PaintedFrame>> = const { RefCell::new(None) };
    static SHELL_TOOLTIP: RefCell<String> = const { RefCell::new(String::new()) };
    static SHELL_ICON: RefCell<Option<(u32, bool, Badge, NotifyIcon)>> = const { RefCell::new(None) };
}

fn notices() -> &'static Mutex<Controller> {
    NOTICES.get_or_init(|| Mutex::new(Controller::default()))
}
fn elapsed() -> std::time::Duration {
    STARTED.get_or_init(Instant::now).elapsed()
}

fn publish_read(read: super::client::TrayRead) -> Result<()> {
    let now = SystemTime::now();
    let rows = super::client::layout_rows(&read, now)?;
    let core_ok = matches!(&read.health, super::client::CoreHealth::Healthy(_));
    let attention = qb_tray::badge(&rows, core_ok) != qb_tray::Badge::Normal;
    notices()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .snapshot(
            &read.snapshot,
            core_ok,
            attention,
            rows.len(),
            now,
            elapsed(),
        );
    *model().lock().unwrap_or_else(|e| e.into_inner()) = (rows, read.snapshot.version, core_ok);
    Ok(())
}

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
    Surface,
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

fn painted_frame(hwnd: HWND) -> Option<PaintedFrame> {
    let frame = PAINTED.with(|slot| slot.borrow().clone())?;
    let view = view_state().lock().unwrap_or_else(|e| e.into_inner());
    if view.owner() != frame.owner || view.as_ref() != frame.state.as_ref() {
        return None;
    }
    drop(view);
    let mut client = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut client).ok()?;
    }
    let size = client_pixel_size(client);
    let offset = VIEWPORT.with(|v| v.borrow().offset);
    if frame.pixels != (size.width, size.height)
        || frame.dpi != unsafe { GetDpiForWindow(hwnd).max(96) }
        || (frame.offset - offset).abs() > 0.01
    {
        return None;
    }
    Some(frame)
}

fn hit_target(hwnd: HWND, x: f32, y: f32) -> Option<HitTarget> {
    let frame = painted_frame(hwnd)?;
    if x < 0.0 || y < 0.0 || x >= frame.pixels.0 as f32 || y >= frame.pixels.1 as f32 {
        return None;
    }
    let scale = frame.dpi as f32 / 96.0;
    let y = VIEWPORT.with(|slot| slot.borrow().to_content_y(y / scale));
    let x = x / scale;
    let rows = frame.rows;
    let state = frame.state;
    let targets = qb_tray::layout_targets(&rows, false, state.as_deref());
    if let Some(target) = qb_tray::hit_test(&targets, x, y).cloned() {
        return Some(match target {
            qb_tray::HitTarget::AddChoice { provider, .. } => HitTarget::AddChoice(provider),
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
            qb_tray::HitTarget::MenuSurface { .. } => HitTarget::Surface,
            qb_tray::HitTarget::Row { .. } => return None,
        });
    }
    None
}

unsafe fn notify_balloon(
    hwnd: HWND,
    title: &str,
    message: &str,
    kind: crate::tray_icon::signals::Kind,
) -> bool {
    let icon = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO,
        dwInfoFlags: if kind == crate::tray_icon::signals::Kind::CoreRestart {
            NIIF_INFO
        } else {
            NIIF_WARNING
        },
        szInfoTitle: notices::shell_text(title),
        szInfo: notices::shell_text(message),
        ..Default::default()
    };
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &icon).as_bool() }
}

fn icon_size(hwnd: HWND) -> u32 {
    let desired = 16.0 * window_scale(hwnd);
    [16u32, 20, 24, 32]
        .into_iter()
        .min_by(|a, b| ((*a as f32 - desired).abs()).total_cmp(&(*b as f32 - desired).abs()))
        .unwrap_or(16)
}

unsafe fn apply_window_theme(hwnd: HWND, dark: bool) {
    let dark_mode = i32::from(dark);
    let border_color: u32 = if dark { 0x003A3A3A } else { 0x00D9D9D9 };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark_mode as *const i32).cast(),
            size_of::<i32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&border_color as *const u32).cast(),
            size_of::<u32>() as u32,
        );
    }
}

fn refresh_shell(hwnd: HWND) {
    let presentation = notices()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .presentation();
    let dark = DARK.load(Ordering::Acquire);
    let size = icon_size(hwnd);
    let changed = SHELL_ICON.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_none_or(|(s, d, b, _)| (*s, *d, *b) != (size, dark, presentation.badge))
    });
    let candidate = if changed {
        NotifyIcon::new(size, dark, presentation.badge).ok()
    } else {
        None
    };
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_TIP,
        szTip: notices::shell_text(&presentation.tooltip),
        ..Default::default()
    };
    if let Some(icon) = &candidate {
        data.uFlags |= NIF_ICON;
        data.hIcon = HICON(icon.as_raw());
    }
    if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data).as_bool() } {
        SHELL_TOOLTIP.with(|slot| *slot.borrow_mut() = presentation.tooltip);
        if let Some(icon) = candidate {
            SHELL_ICON
                .with(|slot| *slot.borrow_mut() = Some((size, dark, presentation.badge, icon)));
        }
        unsafe {
            apply_window_theme(hwnd, dark);
        }
    }
}

// Synthetic live acceptance observes only typed shell state, never core payloads.
#[cfg(all(debug_assertions, feature = "testkit"))]
fn test_shell(hwnd: HWND, event: &str, kind: Option<crate::tray_icon::signals::Kind>) {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let Some(path) = std::env::var_os("QB_TRAY_TEST_SHELL_LOG").map(PathBuf::from) else {
        return;
    };
    let Some((size, dark, badge)) =
        SHELL_ICON.with(|slot| slot.borrow().as_ref().map(|(s, d, b, _)| (*s, *d, *b)))
    else {
        return;
    };
    let tooltip = SHELL_TOOLTIP.with(|slot| slot.borrow().clone());
    let view = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let rows = model().lock().unwrap_or_else(|e| e.into_inner()).0.clone();
    let state = view
        .as_deref()
        .map(|s| s.split(':').next().unwrap_or("list"))
        .unwrap_or("list");
    let menu_checked = view
        .as_deref()
        .and_then(|s| qb_tray::menu_target(&rows, s))
        .and_then(|(row, index)| qb_tray::actions::menu_items(row)[index].checked(row));
    let menu_pending = view.as_deref().is_some_and(|s| s.ends_with(":pending"));
    let count = rows.len();
    let payload = serde_json::json!({
        "sequence": SEQUENCE.fetch_add(1, Ordering::Relaxed), "event": event,
        "badge": match badge { Badge::Normal => "normal", Badge::Attention => "attention", Badge::Error => "error" },
        "tooltip": tooltip, "dark": dark,
        "icon_size": size, "visible": unsafe { IsWindowVisible(hwnd).as_bool() },
        "state": state, "accounts": count, "kind": kind.map(|k| format!("{k:?}")),
        "menu_checked": menu_checked, "menu_pending": menu_pending,
    });
    if let Some(worker) = WORKER.get() {
        let _ = worker.submit(async move {
            use std::io::Write;
            let Some(parent) = path.parent().and_then(|p| p.canonicalize().ok()) else {
                return;
            };
            if !parent.components().any(|c| c.as_os_str() == ".sandbox") {
                return;
            }
            let Some(name) = path.file_name() else { return };
            let path = parent.join(name);
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{payload}");
            }
        });
    }
}

fn flush_notices(hwnd: HWND) {
    refresh_shell(hwnd);
    #[cfg(all(debug_assertions, feature = "testkit"))]
    test_shell(hwnd, "shell", None);
    let notice = notices()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .next(elapsed());
    if let Some(notice) = notice {
        let shown = unsafe { notify_balloon(hwnd, notice.title, notice.text, notice.kind) };
        #[cfg(all(debug_assertions, feature = "testkit"))]
        if shown {
            test_shell(hwnd, "balloon", Some(notice.kind));
        }
        if !shown {
            notices().lock().unwrap_or_else(|e| e.into_inner()).finish();
        }
    }
}

fn refresh_theme(hwnd: HWND) {
    if let Some(worker) = WORKER.get() {
        let handle = hwnd.0 as isize;
        let _ = worker.submit(async move {
            DARK.store(crate::theme::system_dark(), Ordering::Release);
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(handle as *mut c_void)),
                    STATE_CHANGED,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        });
    } else {
        DARK.store(crate::theme::system_dark(), Ordering::Release);
    }
}

fn redraw_now(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd).as_bool() {
            let _ = InvalidateRect(Some(hwnd), None, true);
            let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
        }
    }
}

fn model() -> &'static Mutex<(Vec<qb_tray::Row>, String, bool)> {
    MODEL.get_or_init(|| Mutex::new((Vec::new(), "v0.1.0".to_owned(), false)))
}

fn view_state() -> &'static Mutex<view_ownership::ViewOwnership> {
    VIEW_STATE.get_or_init(|| Mutex::new(view_ownership::ViewOwnership::default()))
}

fn set_view(value: Option<String>) -> usize {
    view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set(value)
}
fn begin_operation() -> usize {
    view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .begin()
}
// Error publication and owner validation share the exact UI view mutex.
fn action_done(hwnd: isize, owner: usize, error: Option<String>) {
    let preserve_error = error.is_some();
    let owner = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .completion(owner, error);
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut c_void)),
            ACTION_DONE,
            WPARAM(owner),
            LPARAM(isize::from(preserve_error)),
        );
    }
    // Refresh regardless of stale completion or failed message posting.
    update_status(false, None);
}
fn admission_error() {
    set_view(Some("error:코어 작업 스레드를 사용할 수 없습니다".into()));
}

fn full_height() -> f32 {
    let rows = model().lock().unwrap_or_else(|e| e.into_inner()).0.clone();
    let state = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    qb_tray::native_render::state_height(&rows, false, state.as_deref())
}
fn flyout_height() -> i32 {
    full_height().ceil() as i32
}
fn window_scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd).max(96) as f32 / 96.0 }
}
fn client_pixel_size(client: RECT) -> D2D_SIZE_U {
    D2D_SIZE_U {
        width: (client.right - client.left).max(1) as u32,
        height: (client.bottom - client.top).max(1) as u32,
    }
}
unsafe fn work_area(hwnd: HWND) -> Option<RECT> {
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(monitor, &mut info)
            .as_bool()
            .then_some(info.rcWork)
    }
}
fn sync_viewport(hwnd: HWND, reveal: bool) {
    let mut client = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut client).is_err() } {
        return;
    }
    let rows = model().lock().unwrap_or_else(|e| e.into_inner()).0.clone();
    let state = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let height = qb_tray::native_render::state_height(&rows, false, state.as_deref());
    let popup = state
        .as_deref()
        .and_then(|s| qb_tray::popup_rect(&rows, false, s, height));
    let extent = (client.bottom - client.top).max(0) as f32 / window_scale(hwnd);
    VIEWPORT.with(|slot| {
        let mut viewport = slot.borrow_mut();
        viewport.set_geometry(height, extent);
        if reveal && let Some(popup) = popup {
            viewport.reveal_popup((popup.top, popup.bottom), 0.0);
        }
    });
}
unsafe fn resize_flyout(hwnd: HWND) {
    let dpi = unsafe { GetDpiForWindow(hwnd).max(96) } as f32;
    let width = pixel_ceil(360.0, dpi).max(1) as i32;
    let desired = pixel_ceil(full_height(), dpi).max(1) as i32;
    let area = unsafe { work_area(hwnd) };
    let height = area.map_or(desired, |area| desired.min((area.bottom - area.top).max(1)));
    let mut window = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut window) }.is_ok() {
        let (x, y) = area.map_or((window.left, window.top), |area| {
            (
                window
                    .left
                    .clamp(area.left, (area.right - width).max(area.left)),
                window
                    .top
                    .clamp(area.top, (area.bottom - height).max(area.top)),
            )
        });
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    sync_viewport(hwnd, true);
}
unsafe fn position_flyout(hwnd: HWND) {
    let identifier = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    let icon = unsafe { Shell_NotifyIconGetRect(&identifier) }.unwrap_or_else(|_| {
        let mut cursor = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut cursor);
        }
        RECT {
            left: cursor.x,
            top: cursor.y,
            right: cursor.x + 1,
            bottom: cursor.y + 1,
        }
    });
    let monitor = unsafe { MonitorFromRect(&icon, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() } {
        return;
    }
    let dpi = unsafe { GetDpiForWindow(hwnd).max(96) } as f32;
    let width = pixel_ceil(360.0, dpi).max(1) as i32;
    let height = (pixel_ceil(full_height(), dpi).max(1) as i32)
        .min((info.rcWork.bottom - info.rcWork.top).max(1));
    let x = (icon.right - width).clamp(
        info.rcWork.left,
        (info.rcWork.right - width).max(info.rcWork.left),
    );
    let y = (icon.top - height).clamp(
        info.rcWork.top,
        (info.rcWork.bottom - height).max(info.rcWork.top),
    );
    unsafe {
        let _ = SetWindowPos(hwnd, None, x, y, width, height, SWP_NOACTIVATE);
    }
    sync_viewport(hwnd, true);
}

// UI calls only enqueue; health and state requests run on the dedicated worker.
fn update_status(_supervise: bool, _notification_hwnd: Option<HWND>) {
    if let Some(worker) = WORKER.get() {
        worker.refresh();
    }
}

fn submit_job(job: impl std::future::Future<Output = ()> + Send + 'static) -> bool {
    let accepted = WORKER.get().is_some_and(|worker| worker.submit(job));
    if !accepted {
        admission_error();
    }
    accepted
}

fn paint_window(hwnd: HWND) -> Result<()> {
    if !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return Ok(());
    }
    sync_viewport(hwnd, false);
    let (rows, version, core_ok) = model().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (state, owner) = {
        let view = view_state().lock().unwrap_or_else(|e| e.into_inner());
        (view.clone(), view.owner())
    };
    unsafe {
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client)?;
        let pixels = client_pixel_size(client);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let target = RENDER_TARGET.with(|slot| -> Result<ID2D1HwndRenderTarget> {
            if let Some(target) = slot.borrow().as_ref() {
                target.Resize(&pixels)?;
                target.SetDpi(dpi as f32, dpi as f32);
                return Ok(target.clone());
            }
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let properties = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                dpiX: dpi as f32,
                dpiY: dpi as f32,
                ..Default::default()
            };
            let hwnd_properties = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: pixels,
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let target = factory.CreateHwndRenderTarget(&properties, &hwnd_properties)?;
            *slot.borrow_mut() = Some(target.clone());
            Ok(target)
        })?;
        let offset = VIEWPORT.with(|v| v.borrow().offset);
        target.SetTransform(&Matrix3x2::translation(0.0, -offset));
        let result = qb_tray::native_render::paint_with_state(
            &target,
            &rows,
            DARK.load(Ordering::Acquire),
            &version,
            SystemTime::now(),
            false,
            core_ok,
            state.as_deref(),
        );
        target.SetTransform(&Matrix3x2::identity());
        match result {
            Ok(()) => PAINTED.with(|slot| {
                *slot.borrow_mut() = Some(PaintedFrame {
                    rows,
                    state,
                    owner,
                    pixels: (pixels.width, pixels.height),
                    dpi,
                    offset,
                })
            }),
            Err(error) => {
                PAINTED.with(|slot| *slot.borrow_mut() = None);
                if error.code().0 as u32 == 0x8899000C {
                    RENDER_TARGET.with(|slot| *slot.borrow_mut() = None);
                } else {
                    return Err(error.into());
                }
            }
        }
    }
    #[cfg(all(debug_assertions, feature = "testkit"))]
    test_shell(hwnd, "paint", None);
    Ok(())
}

fn menu_target() -> Option<(qb_tray::Row, usize)> {
    let view = view_state().lock().unwrap_or_else(|e| e.into_inner());
    let state = view.clone()?;
    let owner = view.owner();
    drop(view);
    let frame = PAINTED.with(|slot| slot.borrow().clone())?;
    if frame.owner != owner {
        return None;
    }
    let (row, index) = qb_tray::menu_target(&frame.rows, &state)?;
    Some((row.clone(), index))
}

fn change_menu_selection(delta: isize) {
    let Some((row, index)) = menu_target() else {
        return;
    };
    if view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        .is_some_and(|s| s.ends_with(":pending"))
    {
        return;
    }
    let count = qb_tray::actions::menu_items(&row).len();
    if count == 0 {
        return;
    }
    let next = (index as isize + delta).rem_euclid(count as isize) as usize;
    let mut state = view_state().lock().unwrap_or_else(|e| e.into_inner());
    state.set(Some(format!("menu:{}:{next}", row.id)));
}

fn activate_menu(hwnd: HWND) {
    let Some((row, index)) = menu_target() else {
        return;
    };
    activate_menu_item(hwnd, row, index);
}

fn activate_menu_item(hwnd: HWND, row: qb_tray::Row, index: usize) {
    let Some(item) = qb_tray::actions::menu_items(&row).get(index).copied() else {
        return;
    };
    if view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        .is_some_and(|s| s.ends_with(":pending"))
    {
        return;
    }
    if item.is_destructive() {
        set_view(Some(format!("confirm-delete:{}", row.id)));
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
    let Some(owner) = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .begin_menu(&row.id, index, item.is_toggle())
    else {
        return;
    };
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }

    submit_job(async move {
        let result: anyhow::Result<()> = async {
            let client = super::client::AdminClient::from_home()?;
            async {
                match operation {
                    Some(qb_tray::actions::AdminOperation::Account(action)) => client
                        .account_action(
                            &id,
                            super::client::AccountAction::from_str(action).expect("typed action"),
                        )
                        .await
                        .map(|_| ()),
                    Some(qb_tray::actions::AdminOperation::RefreshTokens) => {
                        client.refresh_tokens(&id).await
                    }
                    Some(qb_tray::actions::AdminOperation::Drain(value)) => client
                        .patch_account(
                            &id,
                            super::client::AccountPatch {
                                drain: Some(value),
                                ..Default::default()
                            },
                        )
                        .await
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
            }
            .await?;
            // Publish the refreshed checked value before ending its pending state.
            if item.is_toggle() {
                publish_read(client.read().await?)?;
            }
            Ok(())
        }
        .await;
        if let (Err(error), Some(path)) = (&result, std::env::var_os("QB_TRAY_TEST_ERROR_LOG")) {
            let _ = std::fs::write(path, error.to_string());
        }
        let show_error = result.is_err();
        let error = if show_error {
            let message = result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<super::client::ClientError>())
                .map(super::client::short_error)
                .unwrap_or("계정 갱신을 마치지 못했습니다");
            Some(format!("error:{message}"))
        } else {
            None
        };
        action_done(hwnd_value, owner, error);
    });
}

// Serialize generation checks with publishing, so cancellation cannot race a
// late start/status response into restoring a dismissed login dialog.
fn with_current_login(generation: u64, publish: impl FnOnce()) -> bool {
    let current = LOGIN_GENERATION.lock().unwrap_or_else(|e| e.into_inner());
    if *current != generation {
        return false;
    }
    publish();
    true
}

fn launch_login(hwnd: HWND, provider: String) {
    if !cancel_login() {
        return;
    }
    let generation = *LOGIN_GENERATION.lock().unwrap_or_else(|e| e.into_inner());
    let owner = set_view(Some(format!("login:{provider}")));
    let hwnd_value = hwnd.0 as isize;
    submit_job(async move {
        let result: anyhow::Result<()> = async {
            let client = super::client::AdminClient::from_home()?;
            let start = client
                .start_login(&provider)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            let current = with_current_login(generation, || {
                *login_id().lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(start.login_id.clone());
                *login_url().lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(start.auth_url.clone());
                if let Some(path) = std::env::var_os("QB_TRAY_TEST_LOGIN_LOG") {
                    let _ = std::fs::write(path, format!("pending {}\n", start.login_id));
                }
                view_state()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .publish(owner, Some(format!("login:{provider}")));
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(hwnd_value as *mut c_void)),
                        STATE_CHANGED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            });
            if !current {
                let _ = client.cancel_login(&start.login_id).await;
                return Ok(());
            }
            // The browser is a product action; tests set this hook to avoid opening one.
            if std::env::var_os("QB_TRAY_TEST_NO_BROWSER").is_none() {
                let _ = open_browser(&start.auth_url);
            }
            for _ in 0..600 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if *LOGIN_GENERATION.lock().unwrap_or_else(|e| e.into_inner()) != generation {
                    return Ok(());
                }
                let status = client
                    .login_status(&start.login_id)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                if *LOGIN_GENERATION.lock().unwrap_or_else(|e| e.into_inner()) != generation {
                    return Ok(());
                }
                if matches!(
                    status.status.as_str(),
                    "completed" | "failed" | "expired" | "cancelled"
                ) {
                    with_current_login(generation, || {
                        view_state()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .publish(owner, Some(format!("login:{provider}:{}", status.status)));
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(hwnd_value as *mut c_void)),
                                STATE_CHANGED,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    });
                    break;
                }
            }
            Ok(())
        }
        .await;
        with_current_login(generation, || {
            *login_id().lock().unwrap_or_else(|e| e.into_inner()) = None;
            *login_url().lock().unwrap_or_else(|e| e.into_inner()) = None;
        });
        // Always refresh, even when generation/view is stale.
        action_done(
            hwnd_value,
            owner,
            result
                .err()
                .map(|_| "error:로그인을 마치지 못했습니다".into()),
        );
    });
}

// Invalidate the old poller even on rejection, but retain its ID for retry.
// A start response not yet assigned an ID cancels itself inside its admitted job.
fn cancel_login() -> bool {
    let mut generation = LOGIN_GENERATION.lock().unwrap_or_else(|e| e.into_inner());
    *generation = generation
        .checked_add(1)
        .expect("login generation exhausted");
    let accepted = view_ownership::admit_cancellation(
        &mut login_id().lock().unwrap_or_else(|e| e.into_inner()),
        |id| {
            WORKER.get().is_some_and(|worker| {
                worker.submit_cancel(async move {
                    if let Ok(client) = super::client::AdminClient::from_home() {
                        let _ = client.cancel_login(&id).await;
                    }
                })
            })
        },
    );
    *login_url().lock().unwrap_or_else(|e| e.into_inner()) = None;
    if !accepted {
        admission_error();
        return false;
    }
    if let Some(path) = std::env::var_os("QB_TRAY_TEST_LOGIN_LOG") {
        let _ = std::fs::write(path, "cancelled\n");
    }
    true
}

fn activate_hit(hwnd: HWND, target: HitTarget) {
    match target {
        HitTarget::RowMenu(id) => {
            set_view(Some(format!("menu:{id}:0")));
        }
        HitTarget::RowRelogin(id) => {
            let provider = model()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.provider.to_ascii_lowercase());
            if let Some(provider) = provider {
                launch_login(hwnd, provider);
            }
        }
        HitTarget::MenuItem(id, index) => {
            let row = PAINTED.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .and_then(|frame| frame.rows.iter().find(|row| row.id == id).cloned())
            });
            if let Some(row) = row {
                activate_menu_item(hwnd, row, index);
            }
        }
        HitTarget::Surface => {}
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
                    let _ = row;
                    let hwnd_value = hwnd.0 as isize;
                    let owner = begin_operation();
                    submit_job(async move {
                        let result: anyhow::Result<()> = async {
                            let client = super::client::AdminClient::from_home()?;
                            client.delete_account(&id).await?;
                            Ok(())
                        }
                        .await;
                        action_done(
                            hwnd_value,
                            owner,
                            result
                                .err()
                                .map(|_| "error:계정을 삭제하지 못했습니다".into()),
                        );
                    });
                }
            } else {
                set_view(None);
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
            if !cancel_login() {
                unsafe {
                    resize_flyout(hwnd);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                return;
            }
            set_view(Some("list".into()));
            if std::env::var_os("QB_TRAY_TEST_KEEP_OPEN").is_some() {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
            }
        }
        HitTarget::FooterAdd => {
            set_view(Some("add-menu".into()));
        }
        HitTarget::FooterSettings => {
            open_folder(".");
        }
        HitTarget::FooterLogs => {
            open_folder("logs");
        }
        HitTarget::FooterQuit => {
            set_view(Some("quit".into()));
        }
        HitTarget::QuitChoice(tray_only) => {
            if tray_only {
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            } else {
                let hwnd_value = hwnd.0 as isize;
                if let Some(worker) = WORKER.get() {
                    worker.disable_supervision();
                }
                submit_job(async move {
                    if let Ok(client) = super::client::AdminClient::from_home() {
                        let _ = client.shutdown(30).await;
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
                set_view(None);
            }
        }
        HitTarget::QuitCancel => {
            set_view(None);
        }
        HitTarget::Disclosure(provider) => {
            let provider = provider.to_ascii_lowercase();
            if !matches!(provider.as_str(), "claude" | "codex") {
                return;
            }
            let current = view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let rows = model().lock().unwrap_or_else(|e| e.into_inner()).0.clone();
            let expanded = qb_tray::disabled_expanded(&rows, false, current.as_deref(), &provider);
            set_view(Some(if expanded {
                "list".into()
            } else {
                format!("expanded:{provider}")
            }));
        }
    }
    unsafe {
        resize_flyout(hwnd);
    }
    redraw_now(hwnd);
}

fn open_folder(relative: &str) {
    let home = qb_core::home().unwrap_or_else(|_| PathBuf::from("."));
    let path = if relative == "." {
        home
    } else {
        home.join(relative)
    };
    if let Some(log) = std::env::var_os("QB_TRAY_TEST_EXPLORER_LOG") {
        let _ = std::fs::write(log, path.to_string_lossy().as_bytes());
    } else if std::env::var_os("QB_TRAY_TEST_NO_EXPLORER").is_none() {
        let _ = Command::new("explorer.exe").arg(path).spawn();
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            window: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> *mut c_void;
    }
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if result as isize <= 32 {
        Err(std::io::Error::other("브라우저를 열지 못했습니다."))
    } else {
        Ok(())
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
                set_view(None);
                let _ = ShowWindow(hwnd, SW_HIDE);
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
        TRAY_MESSAGE
            if matches!(
                lparam.0 as u32,
                NIN_BALLOONHIDE | NIN_BALLOONTIMEOUT | NIN_BALLOONUSERCLICK
            ) =>
        {
            let target = notices().lock().unwrap_or_else(|e| e.into_inner()).finish();
            if lparam.0 as u32 == NIN_BALLOONUSERCLICK {
                let state = match target {
                    Some(notices::Target::Account(id))
                        if model()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .0
                            .iter()
                            .any(|row| row.id == id) =>
                    {
                        Some(format!("menu:{id}:0"))
                    }
                    _ => None,
                };
                set_view(state);
                position_flyout(hwnd);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = SetForegroundWindow(hwnd);
                redraw_now(hwnd);
            }
            flush_notices(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP if IsWindowVisible(hwnd).as_bool() => {
            let (x, y) = point_from_lparam(lparam);
            if let Some(target) = hit_target(hwnd, x, y) {
                activate_hit(hwnd, target);
            } else if painted_frame(hwnd).is_some() {
                set_view(None);
                resize_flyout(hwnd);
                redraw_now(hwnd);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP if IsWindowVisible(hwnd).as_bool() => {
            let (x, y) = point_from_lparam(lparam);
            if let Some(HitTarget::RowMenu(id)) = hit_target(hwnd, x, y) {
                set_view(Some(format!("menu:{id}:0")));
                resize_flyout(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, true);
            } else {
                set_view(None);
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
            let cursor = if hit_target(hwnd, x, y).is_some() {
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
            if std::env::var_os("QB_TRAY_TEST_KEEP_OPEN").is_none()
                && view_state()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
            {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u32 == 0x1b => {
            let empty = view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .is_none();
            if empty {
                let _ = ShowWindow(hwnd, SW_HIDE);
            } else {
                resize_flyout(hwnd);
                redraw_now(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL if IsWindowVisible(hwnd).as_bool() => {
            let delta = ((wparam.0 >> 16) as u16 as i16) as f32;
            VIEWPORT.with(|slot| slot.borrow_mut().wheel_delta(delta, 72.0));
            redraw_now(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN if matches!(wparam.0 as u32, 0x21..=0x24) => {
            VIEWPORT.with(|slot| {
                let mut v = slot.borrow_mut();
                match wparam.0 as u32 {
                    0x21 => v.offset -= v.extent * 0.8,
                    0x22 => v.offset += v.extent * 0.8,
                    0x24 => v.offset = 0.0,
                    _ => v.offset = v.max_offset(),
                }
                v.clamp();
            });
            redraw_now(hwnd);
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
            redraw_now(hwnd);
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
            redraw_now(hwnd);
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
            view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .consume(wparam.0, lparam.0 != 0);
            resize_flyout(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        CORE_BURST_OBSERVED => {
            notices()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .burst(wparam.0 as u64, elapsed());
            flush_notices(hwnd);
            LRESULT(0)
        }
        CORE_RESTART_CONFIRMED => {
            notices()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .restarted(elapsed());
            flush_notices(hwnd);
            LRESULT(0)
        }
        STATE_CHANGED => {
            let state = view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref()
                .map(str::to_owned);
            if state.as_deref().is_some_and(|s| {
                s.starts_with("menu:")
                    && qb_tray::menu_target(&model().lock().unwrap_or_else(|e| e.into_inner()).0, s)
                        .is_none()
            }) {
                set_view(None);
            }
            resize_flyout(hwnd);
            flush_notices(hwnd);
            redraw_now(hwnd);
            LRESULT(0)
        }
        WM_SHOWWINDOW => {
            if let Some(worker) = WORKER.get() {
                worker.visible(wparam.0 != 0);
            }
            #[cfg(all(debug_assertions, feature = "testkit"))]
            test_shell(hwnd, "visibility", None);
            if wparam.0 == 0 {
                RENDER_TARGET.with(|slot| *slot.borrow_mut() = None);
                PAINTED.with(|slot| *slot.borrow_mut() = None);
            }
            // WM_SHOWWINDOW precedes the visibility change; publish after it.
            let _ = PostMessageW(Some(hwnd), STATE_CHANGED, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        #[cfg(all(debug_assertions, feature = "testkit"))]
        message if message == WM_APP + 6 && wparam.0 <= 1 => {
            crate::theme::set_test_dark(wparam.0 == 1);
            // WM_SETTINGCHANGE carries marshalled system-message parameters;
            // dispatch synchronously on this same UI thread (handler only queues I/O).
            let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                hwnd,
                WM_SETTINGCHANGE,
                Some(WPARAM(0)),
                Some(LPARAM(0)),
            );
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            refresh_theme(hwnd);
            flush_notices(hwnd);
            redraw_now(hwnd);
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
            resize_flyout(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_SIZE => {
            let mut client = RECT::default();
            if GetClientRect(hwnd, &mut client).is_ok() {
                RENDER_TARGET.with(|slot| {
                    if let Some(target) = slot.borrow().as_ref() {
                        let _ = target.Resize(&client_pixel_size(client));
                        let dpi = GetDpiForWindow(hwnd).max(96) as f32;
                        target.SetDpi(dpi, dpi);
                    }
                });
                sync_viewport(hwnd, true);
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == 1 {
                update_status(SUPERVISE_ENABLED.load(Ordering::Relaxed), Some(hwnd));
            }
            flush_notices(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut paint);
            let _ = paint_window(hwnd);
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(worker) = WORKER.get() {
                worker.close();
            }
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
    STARTED.get_or_init(Instant::now);
    DARK.store(crate::theme::system_dark(), Ordering::Release);
    if let Some(fixture) = fixture {
        let json = std::fs::read_to_string(fixture)?;
        let state: qb_tray::State = serde_json::from_str(&json)?;
        let rows = qb_tray::parse_state_at(&json, now)?;
        let snapshot: super::client::TraySnapshot = serde_json::from_str(&json)?;
        notices()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot(
                &snapshot,
                true,
                qb_tray::badge(&rows, true) != qb_tray::Badge::Normal,
                rows.len(),
                now,
                elapsed(),
            );
        *model().lock().unwrap_or_else(|e| e.into_inner()) = (rows, state.version, true);
        FIXTURE_MODE.store(true, Ordering::Relaxed);
    } else {
        FIXTURE_MODE.store(false, Ordering::Relaxed);
        // Worker starts once the HWND is ready; no admin I/O on the UI thread.
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
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
        apply_window_theme(hwnd, DARK.load(Ordering::Acquire));
        let presentation = notices()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .presentation();
        let size = icon_size(hwnd);
        let dark = DARK.load(Ordering::Acquire);
        let generated = NotifyIcon::new(size, dark, presentation.badge)?;
        let mut icon = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: TRAY_MESSAGE,
            hIcon: HICON(generated.as_raw()),
            ..Default::default()
        };
        icon.szTip = notices::shell_text(&presentation.tooltip);
        if !Shell_NotifyIconW(NIM_ADD, &icon).as_bool() {
            let _ = DestroyWindow(hwnd);
            anyhow::bail!("트레이 아이콘을 만들 수 없습니다");
        }
        SHELL_TOOLTIP.with(|slot| *slot.borrow_mut() = presentation.tooltip);
        SHELL_ICON
            .with(|slot| *slot.borrow_mut() = Some((size, dark, presentation.badge, generated)));
        if !FIXTURE_MODE.load(Ordering::Relaxed) {
            let worker = match tray_worker::Worker::start(hwnd.0 as isize, supervise) {
                Ok(worker) => worker,
                Err(error) => {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
                    let _ = DestroyWindow(hwnd);
                    return Err(error);
                }
            };
            if let Err(worker) = WORKER.set(worker) {
                // Outside all callbacks: even a failed installation owns a thread.
                worker.finish();
                let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
                let _ = DestroyWindow(hwnd);
                anyhow::bail!("tray worker already started");
            }
        }
        SetTimer(Some(hwnd), 1, 60_000, None);
        SetTimer(Some(hwnd), 2, 10_000, None);
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
        if let Some(worker) = WORKER.get() {
            worker.close();
        }
        let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
        SHELL_ICON.with(|slot| *slot.borrow_mut() = None);
        let destroyed = if IsWindow(Some(hwnd)).as_bool() {
            DestroyWindow(hwnd)
        } else {
            Ok(())
        };
        if let Some(worker) = WORKER.get() {
            worker.finish();
        }
        destroyed?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_target_size_is_the_current_client_rect() {
        for (width, height) in [(360, 192), (360, 412), (540, 700), (720, 552)] {
            let client = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            let pixels = client_pixel_size(client);
            assert_eq!((pixels.width, pixels.height), (width as u32, height as u32));
        }
    }
}
