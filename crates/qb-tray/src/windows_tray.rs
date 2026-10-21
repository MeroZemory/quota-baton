use crate::{
    notices::{self, Controller},
    tray_icon::{pixels::Badge, win_icon::NotifyIcon},
    viewport::{Viewport, pixel_ceil},
};
use anyhow::Result;
use geometry::{FooterAction, MenuAction, Overlay, Target as HitTarget};
use qb_tray::{
    v2, v2_geometry as geometry,
    v2_render::{LoginStatus, RenderOptions, Renderer},
    v2_session::{ReturnTo, Session, Ticket},
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicUsize;
#[path = "accessibility.rs"]
mod accessibility;
use std::{
    cell::RefCell,
    ffi::c_void,
    mem::size_of,
    os::windows::process::CommandExt,
    path::PathBuf,
    process::Command,
    rc::Rc,
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
        System::{LibraryLoader::GetModuleHandleW, Threading::CREATE_NO_WINDOW},
        UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
                SetProcessDpiAwarenessContext,
            },
            Shell::{
                NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_WARNING,
                NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT,
                NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
                Shell_NotifyIconGetRect, Shell_NotifyIconW,
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
const SHUTDOWN_FAILED: u32 = WM_APP + 7;
const LOGIN_ANIMATION_TIMER: usize = 5;
const WM_MOUSELEAVE_MESSAGE: u32 = 0x02A3;
const WINDOW_CLASS: PCWSTR = w!("QuotaBatonTrayFlyout");
static MODEL: OnceLock<Mutex<(Vec<qb_tray::Row>, String, bool)>> = OnceLock::new();
static VIEW_STATE: OnceLock<Mutex<view_ownership::ViewOwnership>> = OnceLock::new();
#[path = "dashboard_io.rs"]
mod dashboard_io;
#[path = "dashboard_window.rs"]
mod dashboard_window;
#[path = "tray_worker.rs"]
mod tray_worker;
#[path = "view_ownership.rs"]
mod view_ownership;
#[path = "windows_takeover.rs"]
mod windows_takeover;
const DASHBOARD_HANDOFF: u32 = WM_APP + 0x4c2;
static DOUBLE_CLICK_RELEASE: Mutex<Option<Instant>> = Mutex::new(None);
#[link(name = "user32")]
unsafe extern "system" {
    fn GetDoubleClickTime() -> u32;
}
static WORKER: OnceLock<tray_worker::Worker> = OnceLock::new();
static LOGIN_GENERATION: Mutex<u64> = Mutex::new(0);
// The worker spawn gate must also check this while a shutdown is in flight.
static SHUTDOWN_PENDING: AtomicBool = AtomicBool::new(false);
static SUPERVISE_ENABLED: AtomicBool = AtomicBool::new(true);
static FIXTURE_MODE: AtomicBool = AtomicBool::new(false);
static LOGIN_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static LOGIN_URL: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static NOTICES: OnceLock<Mutex<Controller>> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();
static DARK: AtomicBool = AtomicBool::new(false);
static THEME_SYSTEM: AtomicBool = AtomicBool::new(false);
static REVISION: AtomicUsize = AtomicUsize::new(1);
static FRAME_EPOCH: AtomicUsize = AtomicUsize::new(1);
static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();
static LOGIN_DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_SUCCESS: Mutex<Option<SystemTime>> = Mutex::new(None);
fn session() -> &'static Mutex<Session> {
    SESSION.get_or_init(|| Mutex::new(Session::default()))
}
fn invalidate_model() {
    REVISION.fetch_add(1, Ordering::AcqRel);
}

#[derive(Clone)]
struct PaintedFrame {
    #[cfg(all(debug_assertions, feature = "testkit"))]
    tooltip_rect: Option<qb_tray::Rect>,
    view: v2::View,
    scene: geometry::Scene,
    state: Option<String>,
    owner: usize,
    revision: usize,
    epoch: usize,
    pixels: (u32, u32),
    dpi: u32,
    offset: f32,
    actions: Vec<HitTarget>,
    elements: Vec<(i32, HitTarget)>,
}
#[derive(Default)]
struct Interaction {
    focus: Option<HitTarget>,
    hover: Option<HitTarget>,
    pointer_mode: bool,
    focus_visible: bool,
    pressed: Option<HitTarget>,
    tooltip: Option<(String, qb_tray::Rect)>,
    since: Option<Instant>,
    popup_offset: f32,
    reveal: Option<String>,
    reveal_disclosure: Option<String>,
    ids: BTreeMap<String, i32>,
    next_id: i32,
}
thread_local! {
    #[cfg(all(debug_assertions, feature = "testkit"))]
    static FRAME_TIMINGS: RefCell<BTreeMap<&'static str, u128>> = const { RefCell::new(BTreeMap::new()) };
    static LOGIN_ANIMATION_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static RENDER_TARGET: RefCell<Option<ID2D1HwndRenderTarget>> = const { RefCell::new(None) };
    static VISIBLE_RENDERER: RefCell<Option<(bool, Rc<Renderer>)>> = const { RefCell::new(None) };
    static VIEWPORT: RefCell<Viewport> = const { RefCell::new(Viewport { offset: 0.0, content: 0.0, extent: 0.0 }) };
    static PAINTED: RefCell<Option<PaintedFrame>> = const { RefCell::new(None) };
    static INPUT: RefCell<Interaction> = RefCell::new(Interaction::default());
    static UIA: RefCell<Option<accessibility::RootProvider>> = const { RefCell::new(None) };
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
    *LAST_SUCCESS.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
    invalidate_model();
    Ok(())
}

fn login_id() -> &'static Mutex<Option<String>> {
    LOGIN_ID.get_or_init(|| Mutex::new(None))
}

fn login_url() -> &'static Mutex<Option<String>> {
    LOGIN_URL.get_or_init(|| Mutex::new(None))
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
    if frame.revision != REVISION.load(Ordering::Acquire)
        || frame.pixels != (size.width, size.height)
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
    let (x, y) = (x / scale, y / scale);
    frame.scene.hit_test(x, y).cloned().or_else(|| {
        frame
            .scene
            .targets
            .iter()
            .rev()
            .find(|h| contains(h.rect, x, y))
            .filter(|h| !h.enabled)
            .map(|_| HitTarget::Blocked)
    })
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
        dwInfoFlags: match kind {
            crate::tray_icon::signals::Kind::StoreError => NIIF_ERROR,
            crate::tray_icon::signals::Kind::CoreRestart => NIIF_INFO,
            _ => NIIF_WARNING,
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
    if !changed && SHELL_TOOLTIP.with(|slot| *slot.borrow() == presentation.tooltip) {
        return;
    }
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
fn scene_stamp(scene: &geometry::Scene) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    for value in [
        scene.rect.bottom,
        scene.offset,
        scene.list.top,
        scene.list.bottom,
    ] {
        value.to_bits().hash(&mut hash);
    }
    for hit in &scene.targets {
        format!("{:?}", hit.target).hash(&mut hash);
        hit.enabled.hash(&mut hash);
        for value in [hit.rect.left, hit.rect.top, hit.rect.right, hit.rect.bottom] {
            value.to_bits().hash(&mut hash);
        }
    }
    hash.finish()
}

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
    let (menu_checked,menu_pending,scene_trace)=PAINTED.with(|p|p.borrow().as_ref().map(|f| {
        let checked=f.scene.popup.as_ref().and_then(|p|p.items.iter().find_map(|i|i.checked));
        let pending=!session().lock().unwrap_or_else(|e|e.into_inner()).pending_ids().is_empty();
        (checked,pending,serde_json::json!({"epoch":f.epoch,"owner":f.owner,"revision":f.revision,"stamp":scene_stamp(&f.scene),"height":f.scene.rect.bottom,"offset":f.scene.offset,"targets":f.scene.targets.len(),"rows":f.scene.sections.iter().map(|s|s.rows.len()+s.disabled_rows.len()).sum::<usize>(),"expanded":session().lock().unwrap_or_else(|e|e.into_inner()).expansion,"tooltip_rect":f.tooltip_rect.map(|r|[r.left,r.top,r.right,r.bottom])}))
    }).unwrap_or((None,false,serde_json::Value::Null)));
    let count = rows.len();
    let payload = serde_json::json!({
        "sequence": SEQUENCE.fetch_add(1, Ordering::Relaxed), "event": event,
        "clock_ms": elapsed().as_millis(),
        "timing_ms": FRAME_TIMINGS.with(|t| t.borrow().clone()),
        "badge": match badge { Badge::Normal => "normal", Badge::Attention => "attention", Badge::Error => "error" },
        "tooltip": tooltip, "dark": dark,
        "icon_size": size, "visible": unsafe { IsWindowVisible(hwnd).as_bool() },
        "state": state, "accounts": count, "kind": kind.map(|k| format!("{k:?}")),
        "menu_checked": menu_checked, "menu_pending": menu_pending, "scene":scene_trace,
        "file_launch_hook": true,
        "input": INPUT.with(|i| { let i=i.borrow(); serde_json::json!({"pointer":i.pointer_mode,"hover":format!("{:?}",i.hover),"since_ms":i.since.map(|at|at.elapsed().as_millis())}) }),
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
        let shown = unsafe { notify_balloon(hwnd, &notice.title, &notice.text, notice.kind) };
        #[cfg(all(debug_assertions, feature = "testkit"))]
        if shown {
            test_shell(hwnd, "balloon", Some(notice.kind));
        }
        if !shown {
            notices().lock().unwrap_or_else(|e| e.into_inner()).finish();
        }
    }
}

fn refresh_theme(hwnd: HWND, reload: bool) {
    if let Some(worker) = WORKER.get() {
        let handle = hwnd.0 as isize;
        let _ = worker.submit(async move {
            if reload {
                let mode = crate::theme::configured();
                THEME_SYSTEM.store(
                    mode == qb_core::config::TrayTheme::System,
                    Ordering::Release,
                );
                DARK.store(crate::theme::resolve(mode), Ordering::Release);
            } else if THEME_SYSTEM.load(Ordering::Acquire) {
                DARK.store(crate::theme::system_dark(), Ordering::Release);
            }
            if let Some(worker) = WORKER.get() {
                worker.dashboard.theme(DARK.load(Ordering::Acquire));
            }
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(handle as *mut c_void)),
                    STATE_CHANGED,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        });
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
    let mut view = view_state().lock().unwrap_or_else(|e| e.into_inner());
    windows_takeover::invalidate();
    view.set(value)
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

fn projected() -> v2::View {
    let rows = model().lock().unwrap_or_else(|e| e.into_inner()).0.clone();
    v2::project(
        rows.first()
            .and_then(|r| r.view_input.as_deref())
            .unwrap_or("{\"accounts\":[]}"),
        SystemTime::now(),
    )
    .expect("published state was parsed before publication")
}
fn overlay(state: Option<&str>) -> Overlay {
    match state {
        Some("settings") => Overlay::Settings,
        Some("quit") => Overlay::Quit,
        Some("add-menu") => Overlay::Add,
        Some("takeover-providers") => Overlay::TakeoverProviders,
        Some(s) if s.starts_with("takeover-result:") => Overlay::TakeoverCard {
            body: s[16..].into(),
            confirming: false,
            ready: false,
            replace: false,
        },
        Some(s) if s.starts_with("takeover-") => windows_takeover::card(s),
        Some(s) if s.starts_with("confirm-credits:") => Overlay::CreditsConfirm(s[16..].into()),
        Some(s) if s.starts_with("confirm-delete:") => Overlay::DeleteConfirm(s[15..].into()),
        Some(s) if s.starts_with("login:") => {
            Overlay::Login(s[6..].split(':').next().unwrap_or("codex").into())
        }
        Some(s) if s.starts_with("menu:") => {
            let rest = s[5..].strip_suffix(":pending").unwrap_or(&s[5..]);
            Overlay::RowMenu(rest.rsplit_once(':').map_or(rest, |(id, _)| id).into())
        }
        _ => Overlay::None,
    }
}
fn presentation(
    view: &mut v2::View,
    state: Option<&str>,
    core_ok: bool,
) -> (geometry::Presentation, BTreeSet<String>) {
    let mut session = session().lock().unwrap_or_else(|e| e.into_inner());
    if !session.open {
        session.show(view);
    } else {
        session.update(view);
    }
    let mut p = geometry::Presentation {
        expanded: session.expansion.clone(),
        pending_ids: session.pending_ids(),
        core_ok,
        overlay: overlay(state),
        offset: VIEWPORT.with(|v| v.borrow().offset),
        popup_offset: INPUT.with(|i| i.borrow().popup_offset),
        ..Default::default()
    };
    for slot in &session.slots {
        let section = p.frozen.entry(slot.provider.clone()).or_default();
        if slot.disabled_on_open {
            section.disabled_rows.push(slot.id.clone());
        } else {
            section.rows.push(slot.id.clone());
        }
    }
    let mut errors = BTreeSet::new();
    for section in &mut view.sections {
        for row in section.rows.iter_mut().chain(&mut section.disabled_rows) {
            if let Some(error) = session.error(&row.raw.id, elapsed()) {
                row.reason = Some(error.into());
                errors.insert(row.raw.id.clone());
            }
        }
    }
    (p, errors)
}
fn scene_cap(hwnd: HWND) -> f32 {
    unsafe { work_area(hwnd) }.map_or(800., |r| {
        (r.bottom - r.top).max(1) as f32 / window_scale(hwnd) * 0.8
    })
}
fn reveal_bounds(scene: &geometry::Scene, target: &HitTarget) -> Option<qb_tray::Rect> {
    match target {
        HitTarget::Row(id) | HitTarget::Inline { id, .. } => scene.row(id).map(|row| row.rect),
        HitTarget::Disclosure(provider) => {
            scene
                .sections
                .iter()
                .find(|section| &section.provider == provider)?
                .disclosure
        }
        _ => None,
    }
}
fn reveal_offset(scene: &geometry::Scene, rect: qb_tray::Rect) -> f32 {
    let delta = if rect.top < scene.list.top {
        rect.top - scene.list.top
    } else if rect.bottom > scene.list.bottom {
        rect.bottom - scene.list.bottom
    } else {
        0.
    };
    (scene.offset + delta).clamp(0., scene.max_offset)
}
fn repeated_activation(key: u32, flags: isize) -> bool {
    matches!(key, 0x0d | 0x20 | 0x79 | 0x5d) && (flags as usize & (1usize << 30)) != 0
}
fn modal_default_focus(scene: &geometry::Scene) -> Option<HitTarget> {
    if scene.targets.iter().any(|h| {
        h.enabled
            && matches!(
                h.target,
                HitTarget::ConfirmDelete(_) | HitTarget::TakeoverNext | HitTarget::TakeoverCommit
            )
    }) {
        return scene
            .targets
            .iter()
            .find(|h| h.enabled && h.target == HitTarget::CancelDialog)
            .map(|h| h.target.clone());
    }
    scene
        .targets
        .iter()
        .find(|h| h.enabled && !matches!(h.target, HitTarget::Blocked | HitTarget::DismissOverlay))
        .map(|h| h.target.clone())
}
fn disclosure_intent(scene: &geometry::Scene, provider: &str) -> Option<bool> {
    scene
        .sections
        .iter()
        .find(|section| section.provider == provider)
        .map(|section| !section.expanded)
}
fn drain_intent(view: &v2::View, id: &str) -> Option<bool> {
    row_view(view, id).map(|row| !row.raw.drain)
}
fn login_animation_needed(visible: bool, state: Option<&str>) -> bool {
    visible && state.is_some_and(|state| state.starts_with("login:"))
}
fn sync_login_animation(hwnd: HWND, visible: bool) {
    let active = login_animation_needed(
        visible,
        view_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref(),
    );
    LOGIN_ANIMATION_ACTIVE.with(|running| {
        if active == running.get() {
            return;
        }
        if active {
            running.set(unsafe { SetTimer(Some(hwnd), LOGIN_ANIMATION_TIMER, 50, None) } != 0);
        } else {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(
                    Some(hwnd),
                    LOGIN_ANIMATION_TIMER,
                );
            }
            running.set(false);
        }
    });
}
fn measured_scene(
    hwnd: HWND,
) -> Result<(Rc<Renderer>, v2::View, geometry::Scene, BTreeSet<String>)> {
    #[cfg(all(debug_assertions, feature = "testkit"))]
    let started = Instant::now();
    let state = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let core_ok = model().lock().unwrap_or_else(|e| e.into_inner()).2;
    let mut view = projected();
    let (mut p, errors) = presentation(&mut view, state.as_deref(), core_ok);
    let dark = DARK.load(Ordering::Acquire);
    let renderer = VISIBLE_RENDERER.with(|slot| -> Result<Rc<Renderer>> {
        if let Some((theme, renderer)) = slot.borrow().as_ref()
            && *theme == dark
        {
            #[cfg(all(debug_assertions, feature = "testkit"))]
            FRAME_TIMINGS.with(|t| {
                t.borrow_mut().insert("renderer_reused", 1);
            });
            return Ok(renderer.clone());
        }
        let renderer = Rc::new(Renderer::new(dark)?);
        *slot.borrow_mut() = Some((dark, renderer.clone()));
        #[cfg(all(debug_assertions, feature = "testkit"))]
        FRAME_TIMINGS.with(|t| {
            t.borrow_mut().insert("renderer_reused", 0);
        });
        Ok(renderer)
    })?;
    #[cfg(all(debug_assertions, feature = "testkit"))]
    let created = started.elapsed().as_millis();
    renderer.measure(&view, &mut p)?;
    #[cfg(all(debug_assertions, feature = "testkit"))]
    FRAME_TIMINGS.with(|t| {
        let mut t = t.borrow_mut();
        t.insert("renderer_create", created);
        t.insert("measure", started.elapsed().as_millis() - created);
    });
    let mut scene = geometry::Scene::build(&view, p.clone(), scene_cap(hwnd));
    let reveal = INPUT.with(|i| {
        let mut i = i.borrow_mut();
        i.reveal
            .take()
            .map(HitTarget::Row)
            .or_else(|| i.reveal_disclosure.take().map(HitTarget::Disclosure))
    });
    if let Some(target) = reveal
        && let Some(rect) = reveal_bounds(&scene, &target)
    {
        p.offset = reveal_offset(&scene, rect);
        scene = geometry::Scene::build(&view, p, scene_cap(hwnd));
    }
    VIEWPORT.with(|v| {
        let mut v = v.borrow_mut();
        v.content = scene.content_height;
        v.extent = scene.list.bottom - scene.list.top;
        v.offset = scene.offset;
    });
    {
        let pending = session().lock().unwrap_or_else(|e| e.into_inner());
        pending_toggle_labels(&mut scene, &pending);
    }
    Ok((renderer, view, scene, errors))
}
fn toggle_key(action: &MenuAction) -> Option<&'static str> {
    match action {
        MenuAction::ToggleCredits => Some("credits"),
        MenuAction::ToggleDrain => Some("drain"),
        _ => None,
    }
}
fn pending_toggle_labels(scene: &mut geometry::Scene, pending: &Session) {
    if let Some(popup) = &mut scene.popup {
        for item in &mut popup.items {
            if let HitTarget::Menu {
                id: Some(id),
                action,
            } = &item.target
                && let Some(key) = toggle_key(action)
                && pending.pending_action(id) == Some(key)
            {
                item.text = "변경 중…".into();
            }
        }
    }
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
fn resize_target(
    target: &ID2D1HwndRenderTarget,
    pixels: D2D_SIZE_U,
) -> windows::core::Result<bool> {
    unsafe {
        let current = target.GetPixelSize();
        if current.width == pixels.width && current.height == pixels.height {
            return Ok(false);
        }
        target.Resize(&pixels)?;
    }
    Ok(true)
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
unsafe fn resize_flyout(hwnd: HWND) {
    if !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return;
    }
    let Ok((_, _, scene, _)) = measured_scene(hwnd) else {
        return;
    };
    let dpi = unsafe { GetDpiForWindow(hwnd).max(96) } as f32;
    let width = pixel_ceil(360., dpi).max(1) as i32;
    let height = pixel_ceil(scene.rect.bottom, dpi).max(1) as i32;
    let mut window = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut window) }.is_ok() {
        let area = unsafe { work_area(hwnd) };
        let x = area.map_or(window.left, |r| {
            window.left.clamp(r.left, (r.right - width).max(r.left))
        });
        let bottom = area.map_or(window.bottom, |r| window.bottom.min(r.bottom));
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                bottom - height,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
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
    let height = measured_scene(hwnd)
        .map(|(_, _, scene, _)| pixel_ceil(scene.rect.bottom, dpi).max(1) as i32)
        .unwrap_or(298)
        .min(
            ((info.rcWork.bottom - info.rcWork.top) as f32 * 0.8)
                .floor()
                .max(1.) as i32,
        );
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
    #[cfg(all(debug_assertions, feature = "testkit"))]
    let started = Instant::now();
    sync_login_animation(hwnd, unsafe { IsWindowVisible(hwnd).as_bool() });
    if !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return Ok(());
    }
    let (_, version, _) = model().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (state, owner) = {
        let view = view_state().lock().unwrap_or_else(|e| e.into_inner());
        (view.clone(), view.owner())
    };
    let revision = REVISION.load(Ordering::Acquire);
    let (renderer, view, scene, errors) = measured_scene(hwnd)?;
    unsafe {
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client)?;
        let pixels = client_pixel_size(client);
        let dpi = GetDpiForWindow(hwnd).max(96);
        #[cfg(all(debug_assertions, feature = "testkit"))]
        let target_started = Instant::now();
        let target = RENDER_TARGET.with(|slot| -> Result<ID2D1HwndRenderTarget> {
            if let Some(target) = slot.borrow().as_ref() {
                let resized = resize_target(target, pixels)?;
                #[cfg(all(debug_assertions, feature = "testkit"))]
                FRAME_TIMINGS.with(|t| {
                    t.borrow_mut().insert("target_reused", u128::from(!resized));
                });
                #[cfg(not(all(debug_assertions, feature = "testkit")))]
                let _ = resized;
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
            #[cfg(all(debug_assertions, feature = "testkit"))]
            FRAME_TIMINGS.with(|t| {
                t.borrow_mut().insert("target_reused", 0);
            });
            *slot.borrow_mut() = Some(target.clone());
            Ok(target)
        })?;
        #[cfg(all(debug_assertions, feature = "testkit"))]
        FRAME_TIMINGS.with(|t| {
            t.borrow_mut()
                .insert("target", target_started.elapsed().as_millis());
        });
        let offset = scene.offset;
        target.SetTransform(&Matrix3x2::identity());
        INPUT.with(|i| {
            let mut i = i.borrow_mut();
            if (scene.popup.is_some() || scene.dialog.is_some())
                && !i
                    .focus
                    .as_ref()
                    .is_some_and(|t| scene.targets.iter().any(|h| h.enabled && &h.target == t))
            {
                i.focus = modal_default_focus(&scene);
            }
        });
        let pointer = if INPUT.with(|i| i.borrow().pointer_mode) {
            let mut point = POINT::default();
            if GetCursorPos(&mut point).is_ok()
                && windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point).as_bool()
            {
                Some((
                    point.x as f32 / (dpi as f32 / 96.),
                    point.y as f32 / (dpi as f32 / 96.),
                ))
            } else {
                None
            }
        } else {
            None
        };
        let (hover, focus, tooltip_source) = INPUT.with(|i| {
            let mut i = i.borrow_mut();
            if i.pointer_mode {
                let target = pointer
                    .and_then(|(x, y)| scene.hit_test(x, y).cloned())
                    .filter(|t| !matches!(t, HitTarget::Blocked));
                let hint = pointer.and_then(|(x, y)| tooltip_content(&view, &scene, x, y));
                if i.hover != target
                    || i.tooltip.as_ref().map(|(_, r)| r) != hint.as_ref().map(|(_, r)| r)
                {
                    i.since = Some(Instant::now());
                }
                i.hover = target;
                i.tooltip = hint;
            } else {
                i.tooltip = focus_tooltip(&view, &scene, i.focus.as_ref());
            }
            (
                i.hover.clone(),
                i.focus.clone(),
                i.tooltip.clone().filter(|_| tooltip_ready(i.since)),
            )
        });
        let tooltip = tooltip_source
            .as_ref()
            .and_then(|(text, anchor)| renderer.tooltip(text, *anchor, scene.rect).ok());
        let keyboard_focus = INPUT.with(|i| i.borrow().focus_visible);
        let login = state
            .as_deref()
            .filter(|s| s.starts_with("login:"))
            .map(|s| LoginStatus {
                message: if s.ends_with(":expired") {
                    "로그인 시간이 만료되었습니다"
                } else if s.ends_with(":failed") {
                    "로그인하지 못했습니다"
                } else {
                    "브라우저에서 로그인을 마치세요"
                }
                .into(),
                countdown: LOGIN_DEADLINE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .map(|deadline| {
                        let seconds = deadline.saturating_duration_since(Instant::now()).as_secs();
                        format!("{}:{:02}", seconds / 60, seconds % 60)
                    }),
                copy_ready: login_url()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some(),
                retry: s.ends_with(":expired") || s.ends_with(":failed"),
            });
        let last_success = *LAST_SUCCESS.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(all(debug_assertions, feature = "testkit"))]
        let draw_started = Instant::now();
        let result = renderer.paint(
            &target,
            &view,
            &scene,
            &RenderOptions {
                version: &version,
                now: SystemTime::now(),
                hover: hover.as_ref(),
                pressed: INPUT.with(|i| i.borrow().pressed.clone()).as_ref(),
                focus: focus.as_ref().filter(|_| keyboard_focus),
                core_note: (!scene.core_ok).then_some("코어 상태를 읽지 못했습니다"),
                last_success,
                errors: &errors,
                login: login.as_ref(),
                tooltip: tooltip.as_ref(),
                draw_window_border: false,
            },
        );
        #[cfg(all(debug_assertions, feature = "testkit"))]
        FRAME_TIMINGS.with(|t| {
            t.borrow_mut()
                .insert("draw", draw_started.elapsed().as_millis());
        });
        match result {
            Ok(()) => {
                let epoch = FRAME_EPOCH.fetch_add(1, Ordering::AcqRel);
                assert!(epoch > 0 && epoch <= isize::MAX as usize);
                let mut frame = PaintedFrame {
                    #[cfg(all(debug_assertions, feature = "testkit"))]
                    tooltip_rect: tooltip.as_ref().map(|tip| tip.rect),
                    view,
                    scene,
                    state,
                    owner,
                    revision,
                    epoch,
                    pixels: (pixels.width, pixels.height),
                    dpi,
                    offset,
                    actions: Vec::new(),
                    elements: Vec::new(),
                };
                publish_accessibility(hwnd, &mut frame);
                PAINTED.with(|slot| *slot.borrow_mut() = Some(frame));
            }
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
    let drop_started = Instant::now();
    drop(renderer);
    #[cfg(all(debug_assertions, feature = "testkit"))]
    {
        FRAME_TIMINGS.with(|t| {
            t.borrow_mut()
                .insert("renderer_drop", drop_started.elapsed().as_millis());
        });
        FRAME_TIMINGS.with(|t| {
            t.borrow_mut()
                .insert("paint_total", started.elapsed().as_millis());
        });
        test_shell(hwnd, "paint", None);
    }
    Ok(())
}

fn row_view<'a>(view: &'a v2::View, id: &str) -> Option<&'a v2::AccountRow> {
    view.sections
        .iter()
        .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
        .find(|r| r.raw.id == id)
}
fn restore_origin(origin: ReturnTo, id: &str) {
    if matches!(origin, ReturnTo::List) {
        INPUT.with(|i| i.borrow_mut().focus = Some(HitTarget::Row(id.into())));
    }
    if let ReturnTo::Menu(state) = &origin
        && let Overlay::RowMenu(id) = overlay(Some(state))
    {
        INPUT.with(|i| {
            i.borrow_mut().focus = Some(HitTarget::Menu {
                id: Some(id),
                action: MenuAction::ToggleCredits,
            })
        });
    }
    set_view(match origin {
        ReturnTo::List => None,
        ReturnTo::Menu(s) => Some(s),
    });
}
fn ask_credits(id: &str, origin: ReturnTo) {
    let view = projected();
    if session()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .ask_credits(&view, id, origin)
    {
        set_view(Some(format!("confirm-credits:{id}")));
    }
}
fn dispatch_account(hwnd: HWND, id: String, action: MenuAction, ticket: Option<Ticket>) {
    let drain_target = if action == MenuAction::ToggleDrain {
        let Some(target) = painted_frame(hwnd).and_then(|frame| drain_intent(&frame.view, &id))
        else {
            return;
        };
        Some(target)
    } else {
        None
    };
    let current = projected();
    if !model().lock().unwrap_or_else(|e| e.into_inner()).2 || row_view(&current, &id).is_none() {
        if let Some(ticket) = ticket {
            session()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .complete(&ticket, true, elapsed());
        }
        return;
    }
    let row = row_view(&current, &id).expect("checked account");
    if action == MenuAction::Relogin {
        if row.menu.show_relogin {
            launch_login(hwnd, row.raw.provider.clone());
        }
        return;
    }
    if action == MenuAction::Delete && ticket.is_none() {
        set_view(Some(format!("confirm-delete:{id}")));
        return;
    }
    if action == MenuAction::ToggleCredits && !row.raw.allow_credits && ticket.is_none() {
        let state = view_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        ask_credits(
            &id,
            state
                .filter(|s| s.starts_with("menu:"))
                .map_or(ReturnTo::List, ReturnTo::Menu),
        );
        return;
    }
    if ticket.is_none() && !matches!(action, MenuAction::ToggleCredits | MenuAction::ToggleDrain) {
        set_view(None);
    }
    let confirmed = ticket.is_some() && action == MenuAction::ToggleCredits;
    let ticket = match ticket.or_else(|| {
        session()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin(&id)
    }) {
        Some(t) => t,
        None => return,
    };
    if let Some(key) = toggle_key(&action) {
        session()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tag_action(&ticket, key);
    }
    let handle = hwnd.0 as isize;
    let completion = ticket.clone();
    let accepted = WORKER.get().is_some_and(|worker| {
        worker.submit(async move {
            let result: Result<()> = async {
                let client = super::client::AdminClient::from_home()?;
                let read = client.read().await?;
                let rows = super::client::layout_rows(&read, SystemTime::now())?;
                let view = v2::project(
                    rows.first()
                        .and_then(|r| r.view_input.as_deref())
                        .unwrap_or("{\"accounts\":[]}"),
                    SystemTime::now(),
                )?;
                let row = row_view(&view, &id).ok_or_else(|| anyhow::anyhow!("account removed"))?;
                anyhow::ensure!(
                    matches!(read.health, super::client::CoreHealth::Healthy(_)),
                    "core unavailable"
                );
                use super::client::{AccountAction as A, AccountPatch};
                match action {
                    MenuAction::RefreshUsage => {
                        anyhow::ensure!(row.menu.show_refresh_usage, "unavailable");
                        client.account_action(&id, A::RefreshUsage).await?;
                    }
                    MenuAction::RefreshToken => {
                        anyhow::ensure!(row.menu.show_refresh_token, "unavailable");
                        client.refresh_tokens(&id).await?;
                    }
                    MenuAction::ToggleCredits => {
                        anyhow::ensure!(row.raw.provider == "codex", "not Codex");
                        if confirmed {
                            anyhow::ensure!(!row.raw.allow_credits, "already enabled");
                        } else {
                            anyhow::ensure!(row.raw.allow_credits, "confirmation required");
                        }
                        client
                            .patch_account(
                                &id,
                                AccountPatch {
                                    allow_credits: Some(confirmed),
                                    ..Default::default()
                                },
                            )
                            .await?;
                    }
                    MenuAction::ToggleDrain => {
                        anyhow::ensure!(
                            matches!(row.raw.user_state.as_str(), "active" | "paused"),
                            "unavailable"
                        );
                        client
                            .patch_account(
                                &id,
                                AccountPatch {
                                    drain: Some(
                                        drain_target.ok_or_else(|| {
                                            anyhow::anyhow!("missing drain intent")
                                        })?,
                                    ),
                                    ..Default::default()
                                },
                            )
                            .await?;
                    }
                    MenuAction::Pause => {
                        anyhow::ensure!(row.raw.user_state == "active", "unavailable");
                        client.account_action(&id, A::Pause).await?;
                    }
                    MenuAction::Resume => {
                        anyhow::ensure!(row.raw.user_state == "paused", "unavailable");
                        client.account_action(&id, A::Resume).await?;
                    }
                    MenuAction::Disable => {
                        anyhow::ensure!(row.raw.user_state != "disabled", "unavailable");
                        client.account_action(&id, A::Disable).await?;
                    }
                    MenuAction::Enable => {
                        anyhow::ensure!(row.raw.user_state == "disabled", "unavailable");
                        client.account_action(&id, A::Enable).await?;
                    }
                    MenuAction::Delete => {
                        client.delete_account(&id).await?;
                    }
                    _ => anyhow::bail!("not an account action"),
                }
                publish_read(client.read().await?)?;
                Ok(())
            }
            .await;
            if result.is_err()
                && let Ok(client) = super::client::AdminClient::from_home()
                && let Ok(read) = client.read().await
            {
                let _ = publish_read(read);
            }
            session()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .complete(&ticket, result.is_err(), elapsed());
            invalidate_model();
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(handle as *mut c_void)),
                    STATE_CHANGED,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        })
    });
    if !accepted {
        session()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .complete(&completion, true, elapsed());
    }
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
        let mut retry = false;
        let result: anyhow::Result<()> = async {
            let client = super::client::AdminClient::from_home()?;
            let start = client
                .start_login(&provider)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            let current = with_current_login(generation, || {
                *LOGIN_DEADLINE.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(Instant::now() + std::time::Duration::from_secs(600));
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
                    retry = matches!(status.status.as_str(), "failed" | "expired");
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
        if retry {
            update_status(false, None);
            return;
        }
        // Serialize completion publication with cancellation just like start/status.
        // A dismissed login must not publish an error, even before the UI changes owner.
        if !with_current_login(generation, || {
            action_done(
                hwnd_value,
                owner,
                result
                    .err()
                    .map(|_| "error:로그인을 마치지 못했습니다".into()),
            );
        }) {
            update_status(false, None);
        }
    });
}

// Invalidate the old poller even on rejection, but retain its ID for retry.
// A start response not yet assigned an ID cancels itself inside its admitted job.
fn cancel_login() -> bool {
    let mut generation = LOGIN_GENERATION.lock().unwrap_or_else(|e| e.into_inner());
    #[cfg(all(debug_assertions, feature = "testkit"))]
    let had_login = login_id()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
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
    *LOGIN_DEADLINE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if !accepted {
        admission_error();
        return false;
    }
    #[cfg(all(debug_assertions, feature = "testkit"))]
    if had_login && let Some(path) = std::env::var_os("QB_TRAY_TEST_LOGIN_LOG") {
        submit_job(async move {
            let _ = std::fs::write(path, "cancelled\n");
        });
    }
    true
}

fn shutdown_outcome(previous_supervision: bool, accepted: bool) -> (bool, bool) {
    (
        if accepted {
            false
        } else {
            previous_supervision
        },
        accepted,
    )
}
fn quit_all(hwnd: HWND) {
    if SHUTDOWN_PENDING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let previous = SUPERVISE_ENABLED.swap(false, Ordering::AcqRel);
    let owner = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .owner();
    let handle = hwnd.0 as isize;
    let accepted = WORKER.get().is_some_and(|worker| {
        worker.submit(async move {
            let result: Result<()> = async {
                let client = super::client::AdminClient::from_home()?;
                client.shutdown(30).await?;
                Ok(())
            }
            .await;
            let (supervise, close) = shutdown_outcome(previous, result.is_ok());
            SUPERVISE_ENABLED.store(supervise, Ordering::Release);
            if close && let Some(worker) = WORKER.get() {
                worker.disable_supervision();
            }
            SHUTDOWN_PENDING.store(false, Ordering::Release);
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(handle as *mut c_void)),
                    if close { WM_CLOSE } else { SHUTDOWN_FAILED },
                    WPARAM(owner),
                    LPARAM(0),
                );
            }
        })
    });
    if !accepted {
        SUPERVISE_ENABLED.store(previous, Ordering::Release);
        SHUTDOWN_PENDING.store(false, Ordering::Release);
        unsafe {
            let _ = PostMessageW(Some(hwnd), SHUTDOWN_FAILED, WPARAM(owner), LPARAM(0));
        }
    }
}
fn open_dashboard(hwnd: HWND) {
    let Some(worker) = WORKER.get() else { return };
    refresh_theme(hwnd, true);
    let initial =
        std::sync::Arc::new(qb_tray::dashboard_model::DashboardSnapshot::new(projected()));
    let bridge: std::sync::Arc<dyn dashboard_window::Bridge> = worker.dashboard.clone();
    if dashboard_window::open_or_focus(
        hwnd,
        bridge,
        initial,
        dashboard_io::clock(),
        DARK.load(Ordering::Acquire),
    )
    .is_ok()
    {
        set_view(None);
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    } else {
        set_view(Some(
            "error:대시보드 창을 열지 못했습니다. 잠시 후 다시 시도해 주세요.".into(),
        ));
        redraw_now(hwnd);
    }
}
fn dashboard_handoff(hwnd: HWND) {
    let Some(action) = WORKER
        .get()
        .and_then(|worker| worker.dashboard.take_handoff())
    else {
        return;
    };
    let view = projected();
    {
        let mut s = session().lock().unwrap_or_else(|e| e.into_inner());
        if s.open {
            s.update(&view);
        } else {
            s.show(&view);
        }
        if let dashboard_io::Handoff::Account(id) = &action
            && let Some(provider) = s
                .slots
                .iter()
                .find(|slot| &slot.id == id && slot.disabled_on_open)
                .map(|slot| slot.provider.clone())
        {
            s.expansion.insert(provider, true);
        }
    }
    match action {
        dashboard_io::Handoff::Account(id) => {
            set_view(None);
            INPUT.with(|i| i.borrow_mut().reveal = Some(id));
        }
        dashboard_io::Handoff::Add => {
            set_view(Some("add".into()));
        }
    }
    unsafe {
        position_flyout(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
    }
    redraw_now(hwnd);
}
fn activate_hit(hwnd: HWND, target: HitTarget) {
    let Some(frame) = painted_frame(hwnd) else {
        return;
    };
    if !frame
        .scene
        .targets
        .iter()
        .any(|h| h.enabled && h.target == target)
    {
        return;
    }
    if matches!(target, HitTarget::LoginCopy) && !login_copy_ready(frame.state.as_deref()) {
        return;
    }
    match target {
        HitTarget::Row(id) => {
            INPUT.with(|i| i.borrow_mut().focus = Some(HitTarget::Row(id.clone())));
            set_view(Some(format!("menu:{id}:0")));
        }
        HitTarget::Inline { id, action } => match action {
            v2::ActionKind::AllowCredits => ask_credits(&id, ReturnTo::List),
            v2::ActionKind::DisableCredits => {
                dispatch_account(hwnd, id, MenuAction::ToggleCredits, None)
            }
            v2::ActionKind::Enable => dispatch_account(hwnd, id, MenuAction::Enable, None),
            v2::ActionKind::Relogin => dispatch_account(hwnd, id, MenuAction::Relogin, None),
            v2::ActionKind::OpenLogs => open_folder("logs"),
        },
        HitTarget::Menu { id, action } => match action {
            MenuAction::OpenDashboard => open_dashboard(hwnd),
            MenuAction::OpenConfig => open_config(),
            MenuAction::OpenLogs => open_folder("logs"),
            MenuAction::AddClaude => launch_login(hwnd, "claude".into()),
            MenuAction::AddCodex => launch_login(hwnd, "codex".into()),
            MenuAction::Takeover => {
                set_view(Some("takeover-providers".into()));
            }
            MenuAction::TakeoverCodex => windows_takeover::select(hwnd, "codex"),
            MenuAction::TakeoverClaude => windows_takeover::select(hwnd, "claude"),
            MenuAction::QuitTray => unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            MenuAction::QuitAll => quit_all(hwnd),
            action => {
                if let Some(id) = id {
                    dispatch_account(hwnd, id, action, None);
                }
            }
        },
        HitTarget::TakeoverNext => windows_takeover::next(),
        HitTarget::TakeoverCommit => windows_takeover::commit(hwnd),
        HitTarget::ConfirmCredits(id) => {
            let confirmed = session()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .confirm_credits(&projected());
            if let Some((dialog, ticket)) = confirmed {
                if dialog.id == id {
                    restore_origin(dialog.return_to, &dialog.id);
                    dispatch_account(hwnd, id, MenuAction::ToggleCredits, Some(ticket));
                }
            } else {
                set_view(None);
            }
        }
        HitTarget::ConfirmDelete(id) => {
            let ticket = session()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .begin(&id);
            if let Some(ticket) = ticket {
                set_view(None);
                dispatch_account(hwnd, id, MenuAction::Delete, Some(ticket));
            }
        }
        HitTarget::CancelDialog => {
            let dialog = session()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cancel_credits();
            if let Some(dialog) = dialog {
                restore_origin(dialog.return_to, &dialog.id);
            } else {
                set_view(None);
            }
        }
        HitTarget::LoginCopy => {
            let state = view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if state
                .as_deref()
                .is_some_and(|s| s.ends_with(":expired") || s.ends_with(":failed"))
            {
                if let Overlay::Login(provider) = overlay(state.as_deref()) {
                    launch_login(hwnd, provider);
                }
                return;
            }
            let url = login_url()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(url) = url {
                submit_job(async move {
                    if std::env::var_os("QB_TRAY_TEST_NO_CLIPBOARD").is_none() {
                        let _ = Command::new("powershell.exe")
                            .creation_flags(CREATE_NO_WINDOW.0)
                            .args(["-NoProfile", "-Command", "Set-Clipboard -Value $args[0]"])
                            .arg(url)
                            .spawn();
                    }
                });
            }
        }
        HitTarget::LoginCancel => {
            if cancel_login() {
                set_view(None);
            }
        }
        HitTarget::Footer(action) => {
            set_view(Some(
                match action {
                    FooterAction::Add => "add-menu",
                    FooterAction::Settings => "settings",
                    FooterAction::Quit => "quit",
                }
                .into(),
            ));
        }
        HitTarget::AddProvider(provider) => launch_login(hwnd, provider),
        HitTarget::Disclosure(provider) => {
            let Some(requested) = disclosure_intent(&frame.scene, &provider) else {
                return;
            };
            session()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_expanded(&provider, requested);
        }
        HitTarget::DismissOverlay => {
            dismiss_overlay();
        }
        HitTarget::Blocked => return,
    }
    unsafe {
        resize_flyout(hwnd);
    }
    redraw_now(hwnd);
}
fn dismiss_overlay() {
    let state = view_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if matches!(overlay(state.as_deref()), Overlay::CreditsConfirm(_)) {
        let dialog = session()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel_credits();
        if let Some(dialog) = dialog {
            restore_origin(dialog.return_to, &dialog.id);
            return;
        }
    }
    if let Overlay::RowMenu(id) = overlay(state.as_deref()) {
        INPUT.with(|i| i.borrow_mut().focus = Some(HitTarget::Row(id)));
    }
    set_view(None);
    INPUT.with(|i| i.borrow_mut().popup_offset = 0.);
}
fn open_folder(relative: &str) {
    let relative = relative.to_owned();
    submit_job(async move {
        let Ok(home) = qb_core::home() else { return };
        let path = home.join(relative);
        if let Some(log) = std::env::var_os("QB_TRAY_TEST_EXPLORER_LOG") {
            let _ = std::fs::write(log, path.to_string_lossy().as_bytes());
        } else if std::env::var_os("QB_TRAY_TEST_NO_EXPLORER").is_none() {
            let _ = Command::new("explorer.exe").arg(path).spawn();
        }
    });
}
fn open_config() {
    submit_job(async move {
        let result: Result<()> = (|| {
            let path = qb_core::home()?.join("config.toml");
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    f.write_all(crate::theme::COMMENTED_DEFAULTS.as_bytes())?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            #[cfg(all(debug_assertions, feature = "testkit"))]
            if let Some(log) = std::env::var_os("QB_TRAY_TEST_FILE_LAUNCH_LOG") {
                use std::io::Write;
                let log = PathBuf::from(log);
                let parent = log
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("test hook parent"))?
                    .canonicalize()?;
                anyhow::ensure!(
                    parent.components().any(|c| c.as_os_str() == ".sandbox"),
                    "test hook must stay in sandbox"
                );
                let name = log
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("test hook name"))?;
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(parent.join(name))?;
                file.write_all(&serde_json::to_vec(&serde_json::json!({
                    "action":"open_config", "path":path
                }))?)?;
                return Ok(());
            }
            if open_browser(
                path.to_str()
                    .ok_or_else(|| anyhow::anyhow!("config path"))?,
            )
            .is_err()
            {
                Command::new("notepad.exe").arg(path).spawn()?;
            }
            Ok(())
        })();
        let _ = result;
    });
}

fn tooltip_delay() -> std::time::Duration {
    let mut millis = 400u32;
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
            windows::Win32::UI::WindowsAndMessaging::SPI_GETMOUSEHOVERTIME,
            0,
            Some((&mut millis as *mut u32).cast()),
            Default::default(),
        );
    }
    std::time::Duration::from_millis(millis as u64)
}
fn tooltip_ready(since: Option<Instant>) -> bool {
    since.is_some_and(|t| t.elapsed() >= tooltip_delay())
}
fn contains(r: qb_tray::Rect, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}
fn tooltip_content(
    view: &v2::View,
    scene: &geometry::Scene,
    x: f32,
    y: f32,
) -> Option<(String, qb_tray::Rect)> {
    if scene.popup.is_some() || scene.dialog.is_some() {
        return None;
    }
    for (r, summary) in scene.header.summaries.iter().zip(&view.summaries) {
        if contains(*r, x, y) {
            return Some((summary.tooltip.clone(), *r));
        }
    }
    if !contains(scene.list, x, y) {
        return None;
    }
    for boxrow in scene
        .sections
        .iter()
        .flat_map(|s| s.rows.iter().chain(&s.disabled_rows))
    {
        let Some(row) = row_view(view, &boxrow.id) else {
            continue;
        };
        for (rect, text) in [
            (boxrow.chip, row.chip_tooltip.clone()),
            (
                boxrow.credits,
                row.credits.as_ref().map(|c| c.tooltip.clone()),
            ),
            (boxrow.plan, row.plan_tooltip.clone()),
            (Some(boxrow.name), row.name_tooltip.clone()),
            (boxrow.usage, row.usage.as_ref().map(|u| u.tooltip.clone())),
            (boxrow.bar, row.usage.as_ref().map(|u| u.tooltip.clone())),
        ] {
            if let (Some(rect), Some(text)) = (rect, text)
                && contains(rect, x, y)
            {
                return Some((text, rect));
            }
        }
    }
    None
}
fn focus_tooltip(
    view: &v2::View,
    scene: &geometry::Scene,
    focus: Option<&HitTarget>,
) -> Option<(String, qb_tray::Rect)> {
    if scene.popup.is_some() || scene.dialog.is_some() || scene.login.is_some() {
        return None;
    }
    let HitTarget::Row(id) = focus? else {
        return None;
    };
    Some((
        row_view(view, id)?.usage.as_ref()?.tooltip.clone(),
        scene.row(id)?.usage?,
    ))
}
fn hover_pointer(hwnd: HWND, x: f32, y: f32) {
    let Some(frame) = painted_frame(hwnd) else {
        return;
    };
    let scale = frame.dpi as f32 / 96.;
    let target = frame
        .scene
        .hit_test(x / scale, y / scale)
        .cloned()
        .filter(|t| !matches!(t, HitTarget::Blocked));
    let tooltip = tooltip_content(&frame.view, &frame.scene, x / scale, y / scale);
    let changed = INPUT.with(|i| {
        let mut i = i.borrow_mut();
        i.pointer_mode = true;
        i.focus_visible = false;
        if i.hover == target && i.tooltip == tooltip {
            return false;
        }
        i.hover = target;
        i.tooltip = tooltip;
        i.since = Some(Instant::now());
        true
    });
    if changed {
        unsafe {
            SetTimer(
                Some(hwnd),
                4,
                tooltip_delay().as_millis().clamp(1, u32::MAX as u128) as u32,
                None,
            );
        }
        redraw_now(hwnd);
    }
}
fn focus_target(hwnd: HWND, target: HitTarget) {
    INPUT.with(|i| {
        let mut i = i.borrow_mut();
        i.pointer_mode = false;
        i.focus_visible = true;
    });
    let frame = painted_frame(hwnd);
    if let Some(popup) = frame.as_ref().and_then(|f| f.scene.popup.as_ref())
        && let Some(item) = popup.items.iter().find(|i| i.target == target)
    {
        let delta = if item.rect.top < popup.clip.top {
            item.rect.top - popup.clip.top
        } else if item.rect.bottom > popup.clip.bottom {
            item.rect.bottom - popup.clip.bottom
        } else {
            0.
        };
        INPUT.with(|i| {
            let mut i = i.borrow_mut();
            i.popup_offset = (i.popup_offset + delta).clamp(
                0.,
                (popup.content_height - (popup.clip.bottom - popup.clip.top)).max(0.),
            );
        });
    }
    INPUT.with(|i| {
        let mut i = i.borrow_mut();
        i.focus = Some(target.clone());
        i.since = Some(Instant::now());
        i.tooltip = frame.as_ref().and_then(|f| {
            if let HitTarget::Row(id) = &target {
                Some((
                    row_view(&f.view, id)?.usage.as_ref()?.tooltip.clone(),
                    f.scene.row(id)?.hover,
                ))
            } else {
                None
            }
        });
        if let HitTarget::Row(id) | HitTarget::Inline { id, .. } = &target {
            i.reveal = Some(id.clone());
        } else if let HitTarget::Disclosure(provider) = &target {
            i.reveal_disclosure = Some(provider.clone());
        }
    });
    redraw_now(hwnd);
}
fn keyboard(hwnd: HWND, key: u32) -> bool {
    let Some(frame) = painted_frame(hwnd) else {
        return false;
    };
    if let Some(d) = frame
        .scene
        .dialog
        .as_ref()
        .filter(|d| d.takeover_body.is_some())
        && matches!(key, 0x21..=0x24)
    {
        INPUT.with(|i| {
            let mut i = i.borrow_mut();
            let page = geometry::h(d.body).max(20.);
            let max = (d.body_height - page).max(0.);
            i.popup_offset = match key {
                0x21 => (i.popup_offset - page).max(0.),
                0x22 => (i.popup_offset + page).min(max),
                0x23 => max,
                _ => 0.,
            };
        });
        redraw_now(hwnd);
        return true;
    }
    let current = INPUT.with(|i| i.borrow().focus.clone());
    if key == 0x1b {
        if matches!(overlay(frame.state.as_deref()), Overlay::None) {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        } else if matches!(overlay(frame.state.as_deref()), Overlay::Login(_)) {
            if cancel_login() {
                set_view(None);
            }
        } else {
            dismiss_overlay();
        }
        unsafe {
            resize_flyout(hwnd);
        }
        redraw_now(hwnd);
        return true;
    }
    let modal = frame.scene.popup.is_some() || frame.scene.dialog.is_some();
    let mut targets: Vec<_> = frame
        .scene
        .targets
        .iter()
        .filter(|h| h.enabled)
        .map(|h| h.target.clone())
        .filter(|t| !matches!(t, HitTarget::Blocked | HitTarget::DismissOverlay))
        .collect();
    if let Some(popup) = &frame.scene.popup {
        targets = popup
            .items
            .iter()
            .filter(|i| i.enabled)
            .map(|i| i.target.clone())
            .collect();
    }
    if !modal && frame.scene.login.is_none() {
        targets.clear();
        for section in &frame.scene.sections {
            if let Some(button) = &section.add_button
                && button.enabled
            {
                targets.push(button.target.clone());
            }
            for row in &section.rows {
                targets.push(HitTarget::Row(row.id.clone()));
                if let Some(b) = &row.button
                    && b.enabled
                {
                    targets.push(b.target.clone());
                }
            }
            if section.disclosure.is_some() {
                targets.push(HitTarget::Disclosure(section.provider.clone()));
            }
            for row in &section.disabled_rows {
                targets.push(HitTarget::Row(row.id.clone()));
                if let Some(b) = &row.button
                    && b.enabled
                {
                    targets.push(b.target.clone());
                }
            }
        }
        if let Some(empty) = &frame.scene.empty {
            targets.extend(
                empty
                    .buttons
                    .iter()
                    .filter(|b| b.enabled)
                    .map(|b| b.target.clone()),
            );
        }
        targets.extend(
            frame
                .scene
                .footer
                .buttons
                .iter()
                .filter(|b| b.enabled)
                .map(|b| b.target.clone()),
        );
    }
    if matches!(key, 0x09 | 0x26 | 0x28 | 0x24 | 0x23) {
        let list: Vec<_> = targets
            .iter()
            .filter(|&t| {
                key == 0x09
                    || modal
                    || if matches!(key, 0x24 | 0x23) {
                        matches!(t, HitTarget::Row(_))
                    } else {
                        matches!(t, HitTarget::Row(_) | HitTarget::Disclosure(_))
                    }
            })
            .cloned()
            .collect();
        if list.is_empty() {
            return true;
        }
        let nav_current = if key != 0x09 {
            current.as_ref().map(|t| {
                if let HitTarget::Inline { id, .. } = t {
                    HitTarget::Row(id.clone())
                } else {
                    t.clone()
                }
            })
        } else {
            current.clone()
        };
        let position = nav_current
            .as_ref()
            .and_then(|t| list.iter().position(|a| a == t));
        let backwards = key == 0x26
            || (key == 0x09
                && unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x10) } < 0);
        let index = match key {
            0x24 => 0,
            0x23 => list.len() - 1,
            _ => position.map_or(if backwards { list.len() - 1 } else { 0 }, |i| {
                if backwards {
                    (i + list.len() - 1) % list.len()
                } else {
                    (i + 1) % list.len()
                }
            }),
        };
        focus_target(hwnd, list[index].clone());
        return true;
    }
    if let Some(HitTarget::Disclosure(provider)) = &current
        && matches!(key, 0x25 | 0x27)
    {
        session()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_expanded(provider, key == 0x27);
        unsafe {
            resize_flyout(hwnd);
        }
        redraw_now(hwnd);
        return true;
    }
    if matches!(key, 0x0d | 0x20 | 0x79 | 0x5d) {
        if let Some(target) = current
            && targets.contains(&target)
        {
            activate_hit(hwnd, target);
        }
        return true;
    }
    false
}
fn element_name(frame: &PaintedFrame, target: &HitTarget) -> String {
    match target {
        HitTarget::Row(id) => row_view(&frame.view, id)
            .map(|r| r.accessible_name.clone())
            .unwrap_or_default(),
        HitTarget::Inline { id, .. } => row_view(&frame.view, id)
            .and_then(|r| r.action.as_ref())
            .map(|a| a.label.to_owned())
            .unwrap_or_default(),
        HitTarget::Disclosure(p) => frame
            .view
            .sections
            .iter()
            .find(|s| &s.provider == p)
            .and_then(|s| s.disclosure.clone())
            .unwrap_or_default(),
        HitTarget::Footer(a) => match a {
            FooterAction::Add => "계정 추가",
            FooterAction::Settings => "설정",
            FooterAction::Quit => "종료",
        }
        .into(),
        HitTarget::AddProvider(p) => format!("{} 계정 추가", v2::provider_label(p)),
        HitTarget::Menu { .. } => frame
            .scene
            .popup
            .as_ref()
            .and_then(|p| p.items.iter().find(|i| &i.target == target))
            .map(|i| {
                format!(
                    "{}{}{}",
                    i.text,
                    i.checked
                        .map(|b| if b { " · 켜짐" } else { " · 꺼짐" })
                        .unwrap_or(""),
                    i.detail
                        .as_ref()
                        .map(|s| format!(" · {s}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default(),
        HitTarget::TakeoverNext | HitTarget::TakeoverCommit => frame
            .scene
            .dialog
            .as_ref()
            .and_then(|d| d.buttons.iter().find(|b| &b.target == target))
            .map(|b| b.label.clone())
            .unwrap_or_default(),
        HitTarget::ConfirmCredits(_) => "크레딧 사용".into(),
        HitTarget::ConfirmDelete(_) => "계정 삭제".into(),
        HitTarget::CancelDialog | HitTarget::LoginCancel => "취소".into(),
        HitTarget::LoginCopy => "주소 복사".into(),
        _ => String::new(),
    }
}
fn physical(rect: qb_tray::Rect, origin: POINT, scale: f64) -> [f64; 4] {
    [
        origin.x as f64 + rect.left as f64 * scale,
        origin.y as f64 + rect.top as f64 * scale,
        (rect.right - rect.left).max(0.) as f64 * scale,
        (rect.bottom - rect.top).max(0.) as f64 * scale,
    ]
}
fn stable_id(key: String) -> i32 {
    INPUT.with(|i| {
        let mut i = i.borrow_mut();
        if let Some(id) = i.ids.get(&key) {
            return *id;
        }
        i.next_id = i.next_id.checked_add(1).expect("UIA identity exhausted");
        let id = i.next_id;
        i.ids.insert(key, id);
        id
    })
}
fn login_copy_ready(state: Option<&str>) -> bool {
    state.is_some_and(|s| s.ends_with(":expired") || s.ends_with(":failed"))
        || login_url()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
}
fn publish_accessibility(hwnd: HWND, frame: &mut PaintedFrame) {
    use accessibility::{Element, Kind, Snapshot};
    let mut origin = POINT::default();
    unsafe {
        if !windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut origin).as_bool() {
            return;
        }
    }
    let scale = frame.dpi as f64 / 96.;
    let mut elements = Vec::new();
    if let Some(d) = frame.scene.dialog.as_ref()
        && let Some(body) = &d.takeover_body
    {
        elements.push(Element {
            parent: None,
            id: stable_id("takeover:body".into()),
            name: body.clone(),
            kind: Kind::Text,
            bounds: physical(d.body, origin, scale),
            enabled: true,
            action: None,
            focusable: false,
            focused: false,
            value: None,
            selected: None,
            expanded: None,
        });
    }
    for (index, (rect, summary)) in frame
        .scene
        .header
        .summaries
        .iter()
        .zip(&frame.view.summaries)
        .enumerate()
    {
        elements.push(Element {
            parent: None,
            id: stable_id(format!("summary:{index}")),
            name: summary.accessible_name.clone(),
            kind: Kind::Text,
            bounds: physical(*rect, origin, scale),
            enabled: true,
            action: None,
            focusable: false,
            focused: false,
            value: None,
            selected: None,
            expanded: None,
        });
    }
    let focus = INPUT.with(|i| i.borrow().focus.clone());
    for h in &frame.scene.targets {
        if matches!(h.target, HitTarget::Blocked | HitTarget::DismissOverlay) {
            continue;
        }
        let id = stable_id(format!("target:{:?}", h.target));
        let index = frame.actions.len();
        frame.actions.push(h.target.clone());
        frame.elements.push((id, h.target.clone()));
        let enabled = h.enabled
            && (!matches!(h.target, HitTarget::LoginCopy)
                || login_copy_ready(frame.state.as_deref()));
        let checked = frame.scene.popup.as_ref().and_then(|p| {
            p.items
                .iter()
                .find(|i| i.target == h.target)
                .and_then(|i| i.checked)
        });
        elements.push(Element {
            parent: None,
            id,
            name: element_name(frame, &h.target),
            kind: if matches!(h.target, HitTarget::Row(_)) {
                Kind::Row
            } else if checked.is_some() {
                Kind::Checkbox
            } else {
                Kind::Button
            },
            bounds: physical(h.rect, origin, scale),
            enabled,
            action: Some(index),
            focusable: enabled,
            focused: enabled && focus.as_ref() == Some(&h.target),
            value: None,
            selected: checked,
            expanded: None,
        });
    }
    UIA.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = accessibility::create_root_provider(hwnd.0 as isize).ok();
        }
        if let Some(root) = slot.as_ref() {
            let _ = root.publish(
                Snapshot {
                    automation_id: "quota-baton.flyout",
                    name: "quota-baton".into(),
                    bounds: physical(frame.scene.rect, origin, scale),
                    elements,
                },
                frame.epoch,
            );
        }
    });
}
fn disconnect_accessibility() {
    UIA.with(|slot| {
        if let Some(root) = slot.borrow_mut().take() {
            let _ = root.disconnect_before_destroy();
        }
    });
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
        TRAY_MESSAGE
            if lparam.0 as u32 == windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK =>
        {
            *DOUBLE_CLICK_RELEASE
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(
                Instant::now()
                    + std::time::Duration::from_millis(unsafe { GetDoubleClickTime() } as u64),
            );
            open_dashboard(hwnd);
            LRESULT(0)
        }
        TRAY_MESSAGE if lparam.0 as u32 == WM_LBUTTONUP || lparam.0 as u32 == WM_RBUTTONUP => {
            let suppressed = DOUBLE_CLICK_RELEASE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if lparam.0 as u32 == WM_LBUTTONUP
                && suppressed.is_some_and(|until| Instant::now() <= until)
            {
                return LRESULT(0);
            }
            if IsWindowVisible(hwnd).as_bool() {
                set_view(None);
                let _ = ShowWindow(hwnd, SW_HIDE);
                return LRESULT(0);
            }
            if !FIXTURE_MODE.load(Ordering::Relaxed) {
                update_status(SUPERVISE_ENABLED.load(Ordering::Relaxed), Some(hwnd));
            }
            refresh_theme(hwnd, true);
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
                        let view = projected();
                        {
                            let mut session = session().lock().unwrap_or_else(|e| e.into_inner());
                            if !session.open {
                                session.show(&view);
                            } else {
                                session.update(&view);
                            }
                            if let Some(provider) = session
                                .slots
                                .iter()
                                .find(|s| s.id == id && s.disabled_on_open)
                                .map(|s| s.provider.clone())
                            {
                                session.expansion.insert(provider, true);
                            }
                        }
                        INPUT.with(|i| i.borrow_mut().reveal = Some(id));
                        None
                    }
                    _ => None,
                };
                VIEWPORT.with(|v| v.borrow_mut().offset = 0.);
                set_view(state);
                position_flyout(hwnd);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = SetForegroundWindow(hwnd);
                redraw_now(hwnd);
            }
            flush_notices(hwnd);
            LRESULT(0)
        }
        DASHBOARD_HANDOFF => {
            dashboard_handoff(hwnd);
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN
            if IsWindowVisible(hwnd).as_bool() =>
        {
            let (x, y) = point_from_lparam(lparam);
            let target = hit_target(hwnd, x, y);
            INPUT.with(|i| i.borrow_mut().pressed = target);
            redraw_now(hwnd);
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_CANCELMODE
        | windows::Win32::UI::WindowsAndMessaging::WM_CAPTURECHANGED => {
            INPUT.with(|i| i.borrow_mut().pressed = None);
            redraw_now(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP if IsWindowVisible(hwnd).as_bool() => {
            INPUT.with(|i| {
                let mut i = i.borrow_mut();
                i.pressed = None;
                i.pointer_mode = true;
                i.focus_visible = false;
            });
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
            INPUT.with(|i| {
                let mut i = i.borrow_mut();
                i.pointer_mode = true;
                i.focus_visible = false;
            });
            let (x, y) = point_from_lparam(lparam);
            if let Some(HitTarget::Row(id) | HitTarget::Inline { id, .. }) = hit_target(hwnd, x, y)
            {
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
            hover_pointer(hwnd, x, y);
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
            INPUT.with(|i| {
                let mut i = i.borrow_mut();
                i.hover = None;
                i.pointer_mode = false;
                i.pressed = None;
                i.tooltip = None;
                i.since = None;
            });
            redraw_now(hwnd);
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
        WM_MOUSEWHEEL if IsWindowVisible(hwnd).as_bool() => {
            let delta = ((wparam.0 >> 16) as u16 as i16) as f32;
            if let Some(frame) = painted_frame(hwnd) {
                if let Some(popup) = frame.scene.popup {
                    INPUT.with(|i| {
                        let mut i = i.borrow_mut();
                        i.popup_offset = (i.popup_offset - delta / 120. * 48.).clamp(
                            0.,
                            (popup.content_height - (popup.clip.bottom - popup.clip.top)).max(0.),
                        );
                    });
                } else if let Some(d) = frame.scene.dialog.filter(|d| d.takeover_body.is_some()) {
                    INPUT.with(|i| {
                        let mut i = i.borrow_mut();
                        i.popup_offset = (i.popup_offset - delta / 120. * 48.)
                            .clamp(0., (d.body_height - geometry::h(d.body)).max(0.));
                    });
                } else {
                    VIEWPORT.with(|slot| slot.borrow_mut().wheel_delta(delta, 72.));
                }
            }
            redraw_now(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN if repeated_activation(wparam.0 as u32, lparam.0) => LRESULT(0),
        WM_KEYDOWN if keyboard(hwnd, wparam.0 as u32) => LRESULT(0),
        windows::Win32::UI::WindowsAndMessaging::WM_SYSKEYDOWN
            if repeated_activation(wparam.0 as u32, lparam.0) =>
        {
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_SYSKEYDOWN
            if wparam.0 == 0x79 && keyboard(hwnd, 0x79) =>
        {
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_GETOBJECT => {
            if painted_frame(hwnd).is_some()
                && let Some(result) = UIA.with(|p| {
                    p.borrow()
                        .as_ref()
                        .and_then(|p| p.handle_wm_getobject(wparam, lparam))
                })
            {
                return result;
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        accessibility::WM_UIA_INVOKE | accessibility::WM_UIA_FOCUS => {
            if let Some(frame) = painted_frame(hwnd)
                && lparam.0 > 0
                && lparam.0 as usize == frame.epoch
            {
                let target = if message == accessibility::WM_UIA_INVOKE {
                    frame.actions.get(wparam.0).cloned()
                } else {
                    frame
                        .elements
                        .iter()
                        .find(|(id, _)| *id as usize == wparam.0)
                        .map(|(_, t)| t.clone())
                };
                if let Some(target) = target
                    && frame
                        .scene
                        .targets
                        .iter()
                        .any(|h| h.enabled && h.target == target)
                {
                    if message == accessibility::WM_UIA_INVOKE {
                        activate_hit(hwnd, target);
                    } else {
                        focus_target(hwnd, target);
                    }
                }
            }
            LRESULT(0)
        }
        SHUTDOWN_FAILED => {
            if view_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .owner()
                == wparam.0
            {
                set_view(Some("quit".into()));
            }
            // Fixed local copy only; no transport details, credentials, or bodies.
            let _ = windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                Some(hwnd),
                w!("코어를 종료하지 못했습니다. 트레이를 유지합니다. 다시 시도하세요."),
                w!("quota-baton"),
                windows::Win32::UI::WindowsAndMessaging::MB_OK
                    | windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
            redraw_now(hwnd);
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
            sync_login_animation(hwnd, IsWindowVisible(hwnd).as_bool());
            resize_flyout(hwnd);
            flush_notices(hwnd);
            redraw_now(hwnd);
            LRESULT(0)
        }
        WM_SHOWWINDOW => {
            sync_login_animation(hwnd, wparam.0 != 0);
            if let Some(worker) = WORKER.get() {
                worker.visible(wparam.0 != 0);
            }
            #[cfg(all(debug_assertions, feature = "testkit"))]
            test_shell(hwnd, "visibility", None);
            if wparam.0 == 0 {
                // Release the view guard before set_view takes it again.
                let takeover_open = view_state()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_deref()
                    .is_some_and(|s| s.starts_with("takeover-"));
                if takeover_open {
                    set_view(None);
                }
                disconnect_accessibility();
                session().lock().unwrap_or_else(|e| e.into_inner()).hide();
                INPUT.with(|i| {
                    let mut i = i.borrow_mut();
                    i.focus = None;
                    i.focus_visible = false;
                    i.hover = None;
                    i.pointer_mode = false;
                    i.pressed = None;
                    i.tooltip = None;
                    i.since = None;
                    i.popup_offset = 0.;
                    i.reveal_disclosure = None;
                });
                VIEWPORT.with(|v| v.borrow_mut().offset = 0.);
                RENDER_TARGET.with(|slot| *slot.borrow_mut() = None);
                VISIBLE_RENDERER.with(|slot| *slot.borrow_mut() = None);
                PAINTED.with(|slot| *slot.borrow_mut() = None);
            }
            // WM_SHOWWINDOW precedes the visibility change; publish after it.
            let _ = PostMessageW(Some(hwnd), STATE_CHANGED, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        #[cfg(all(debug_assertions, feature = "testkit"))]
        message if message == WM_APP + 8 && wparam.0 == 0 && lparam.0 == 0 => {
            // Readiness only: no paths, config, credentials or I/O in this hook.
            LRESULT(if windows_takeover::synthetic_picker_ready() {
                0x5236
            } else {
                0
            })
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
            refresh_theme(hwnd, false);
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
                        let _ = resize_target(target, client_pixel_size(client));
                        let dpi = GetDpiForWindow(hwnd).max(96) as f32;
                        target.SetDpi(dpi, dpi);
                    }
                });
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == LOGIN_ANIMATION_TIMER {
                let visible = IsWindowVisible(hwnd).as_bool();
                sync_login_animation(hwnd, visible);
                if LOGIN_ANIMATION_ACTIVE.with(|active| active.get()) {
                    redraw_now(hwnd);
                }
                return LRESULT(0);
            }
            if wparam.0 == 4 {
                let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(Some(hwnd), 4);
                redraw_now(hwnd);
            }
            if wparam.0 == 3 && IsWindowVisible(hwnd).as_bool() {
                // Refresh countdowns/error expiry and delayed tooltips without I/O.
                redraw_now(hwnd);
            }
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
        windows::Win32::UI::WindowsAndMessaging::WM_NCDESTROY => {
            disconnect_accessibility();
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_CLOSE => {
            let _ = dashboard_window::close_now_on_ui_thread();
            sync_login_animation(hwnd, false);
            disconnect_accessibility();
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_DESTROY => {
            let _ = dashboard_window::close_now_on_ui_thread();
            sync_login_animation(hwnd, false);
            if let Some(worker) = WORKER.get() {
                worker.close();
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

struct UiApartment(std::marker::PhantomData<std::rc::Rc<()>>);
impl UiApartment {
    fn enter() -> Result<Self> {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
            .ok()?;
        }
        Ok(Self(std::marker::PhantomData))
    }
}
impl Drop for UiApartment {
    fn drop(&mut self) {
        disconnect_accessibility();
        RENDER_TARGET.with(|p| *p.borrow_mut() = None);
        VISIBLE_RENDERER.with(|p| *p.borrow_mut() = None);
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}
pub fn run(
    supervise: bool,
    open_flyout: bool,
    fixture: Option<PathBuf>,
    now: SystemTime,
) -> Result<()> {
    let _apartment = UiApartment::enter()?;
    SUPERVISE_ENABLED.store(supervise, Ordering::Relaxed);
    STARTED.get_or_init(Instant::now);
    DARK.store(false, Ordering::Release);
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
            298,
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
            disconnect_accessibility();
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
                    disconnect_accessibility();
                    let _ = DestroyWindow(hwnd);
                    return Err(error);
                }
            };
            if let Err(worker) = WORKER.set(worker) {
                // Outside all callbacks: even a failed installation owns a thread.
                worker.finish();
                let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
                disconnect_accessibility();
                let _ = DestroyWindow(hwnd);
                anyhow::bail!("tray worker already started");
            }
        }
        refresh_theme(hwnd, true);
        SetTimer(Some(hwnd), 3, 1_000, None);
        SetTimer(Some(hwnd), 1, 60_000, None);
        SetTimer(Some(hwnd), 2, 10_000, None);
        if open_flyout {
            refresh_theme(hwnd, true);
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
        disconnect_accessibility();
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
    fn cancelled_login_generation_cannot_publish_a_late_completion() {
        let generation = *LOGIN_GENERATION.lock().unwrap_or_else(|e| e.into_inner());
        let mut view = view_ownership::ViewOwnership::default();
        let owner = view.set(Some("login:codex".into()));
        // The generation guard must reject even before cancellation changes the view owner.
        assert!(!with_current_login(generation.wrapping_sub(1), || {
            view.completion(owner, Some("error:late".into()));
        }));
        assert_eq!(view.as_deref(), Some("login:codex"));
        assert!(with_current_login(generation, || {
            view.completion(owner, Some("error:current".into()));
        }));
        assert_eq!(view.as_deref(), Some("error:current"));
    }

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
    #[test]
    fn state_adapter_uses_frozen_modal_and_login_variants() {
        assert_eq!(
            overlay(Some("menu:codex-id:3:pending")),
            Overlay::RowMenu("codex-id".into())
        );
        assert_eq!(
            overlay(Some("confirm-credits:codex-id")),
            Overlay::CreditsConfirm("codex-id".into())
        );
        assert_eq!(
            overlay(Some("confirm-delete:codex-id")),
            Overlay::DeleteConfirm("codex-id".into())
        );
        assert_eq!(
            overlay(Some("login:codex:expired")),
            Overlay::Login("codex".into())
        );
        assert_eq!(overlay(Some("settings")), Overlay::Settings);
    }
    #[test]
    fn native_accessibility_uses_physical_screen_bounds_and_stable_distinct_entities() {
        let r = qb_tray::Rect {
            left: 4.,
            top: 10.,
            right: 356.,
            bottom: 54.,
        };
        assert_eq!(
            physical(r, POINT { x: -1920, y: 100 }, 1.5),
            [-1914., 115., 528., 66.]
        );
        let row = stable_id("unrun-test:row:a".into());
        let button = stable_id("unrun-test:inline:a".into());
        assert_ne!(row, button);
        assert_eq!(stable_id("unrun-test:row:a".into()), row);
    }
    #[test]
    fn full_scene_row_button_precedence_and_offline_settings_use_the_same_targets() {
        let view=v2::project(r#"{"version":"test","pools":{"codex":{"eligible":0}},"accounts":[{"id":"native-test","provider":"codex","user_state":"disabled","email":"test@example.invalid"}]}"#,SystemTime::now()).unwrap();
        let scene = geometry::Scene::build(&view, geometry::Presentation::default(), 900.);
        let row = scene.row("native-test").unwrap();
        assert_eq!(
            scene.hit_test(8., row.rect.top + 2.),
            Some(&HitTarget::Row("native-test".into()))
        );
        let b = row.button.as_ref().unwrap();
        assert_eq!(
            scene.hit_test(
                (b.hit.left + b.hit.right) / 2.,
                (b.hit.top + b.hit.bottom) / 2.
            ),
            Some(&b.target)
        );
        let offline = geometry::Scene::build(
            &view,
            geometry::Presentation {
                core_ok: false,
                overlay: Overlay::Settings,
                ..Default::default()
            },
            900.,
        );
        let config = offline
            .targets
            .iter()
            .find(|h| {
                matches!(
                    h.target,
                    HitTarget::Menu {
                        action: MenuAction::OpenConfig,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(config.enabled);
    }
    #[test]
    fn only_the_requested_checkbox_label_becomes_pending() {
        let view = v2::project(
            r#"{"pools":{"codex":{"eligible":1}},"accounts":[{"id":"synthetic-checkbox","provider":"codex","user_state":"active","source":"oauth","health":{"kind":"ready"}}]}"#,
            SystemTime::now(),
        ).unwrap();
        let mut scene = geometry::Scene::build(
            &view,
            geometry::Presentation {
                overlay: Overlay::RowMenu("synthetic-checkbox".into()),
                ..Default::default()
            },
            900.,
        );
        let mut pending = Session::default();
        let ticket = pending.begin("synthetic-checkbox").unwrap();
        assert!(pending.tag_action(&ticket, "credits"));
        pending_toggle_labels(&mut scene, &pending);
        let items = &scene.popup.unwrap().items;
        for (action, expected) in [
            (MenuAction::ToggleCredits, "변경 중…"),
            (MenuAction::ToggleDrain, "새 대화 받지 않기"),
        ] {
            let item = items
                .iter()
                .find(|item| {
                    matches!(
                        &item.target, HitTarget::Menu { action: actual, .. } if actual == &action
                    )
                })
                .unwrap();
            assert_eq!(item.text, expected);
        }
    }
    #[test]
    fn stationary_and_focused_tooltips_read_the_latest_snapshot() {
        let build = |used| {
            let view = v2::project(&serde_json::json!({
                "pools":{"codex":{"eligible":1}},"accounts":[{
                    "id":"synthetic-tip","provider":"codex","user_state":"active","pool_eligible":true,
                    "health":{"kind":"ready"},"windows":[{"name":"7d","used_pct":used,"reset_at":"2099-01-01T00:00:00Z"}]
                }]
            }).to_string(),SystemTime::now()).unwrap();
            let scene = geometry::Scene::build(&view, geometry::Presentation::default(), 900.);
            (view, scene)
        };
        let (old, old_scene) = build(25.);
        let (new, new_scene) = build(50.);
        let rect = old_scene.row("synthetic-tip").unwrap().usage.unwrap();
        let point = ((rect.left + rect.right) / 2., (rect.top + rect.bottom) / 2.);
        let old_hint = tooltip_content(&old, &old_scene, point.0, point.1).unwrap();
        let new_hint = tooltip_content(&new, &new_scene, point.0, point.1).unwrap();
        assert!(old_hint.0.contains("75% 남음"));
        assert!(new_hint.0.contains("50% 남음"));
        assert_eq!(old_hint.1, new_hint.1);
        let focus = HitTarget::Row("synthetic-tip".into());
        assert!(
            focus_tooltip(&new, &new_scene, Some(&focus))
                .unwrap()
                .0
                .contains("50% 남음")
        );
    }
    #[test]
    fn disclosure_click_inverts_the_painted_auto_default_without_a_manual_entry() {
        let view = v2::project(r#"{"pools":{"codex":{"eligible":0}},"accounts":[{"id":"off","provider":"codex","user_state":"disabled"}]}"#, SystemTime::now()).unwrap();
        let scene = geometry::Scene::build(&view, geometry::Presentation::default(), 900.);
        assert_eq!(disclosure_intent(&scene, "codex"), Some(false));
        let mut p = geometry::Presentation::default();
        p.expanded.insert("codex".into(), false);
        let scene = geometry::Scene::build(&view, p, 900.);
        assert_eq!(disclosure_intent(&scene, "codex"), Some(true));
        assert_eq!(disclosure_intent(&scene, "missing"), None);
    }
    #[test]
    fn held_activation_cannot_confirm_a_new_dialog_but_navigation_can_repeat() {
        let repeated = 1isize << 30;
        for key in [0x0d, 0x20, 0x79, 0x5d] {
            assert!(!repeated_activation(key, 1));
            assert!(repeated_activation(key, repeated | 1));
        }
        for key in [0x09, 0x26, 0x28] {
            assert!(!repeated_activation(key, repeated | 1));
        }
    }
    #[test]
    fn delete_dialog_starts_on_cancel_and_credits_remains_explicit() {
        let view = v2::project(
            r#"{"accounts":[{"id":"delete-test","provider":"codex","user_state":"active"}]}"#,
            SystemTime::now(),
        )
        .unwrap();
        let scene = geometry::Scene::build(
            &view,
            geometry::Presentation {
                overlay: Overlay::DeleteConfirm("delete-test".into()),
                ..Default::default()
            },
            800.,
        );
        assert_eq!(modal_default_focus(&scene), Some(HitTarget::CancelDialog));
        let scene = geometry::Scene::build(
            &view,
            geometry::Presentation {
                overlay: Overlay::CreditsConfirm("delete-test".into()),
                ..Default::default()
            },
            800.,
        );
        assert_eq!(
            modal_default_focus(&scene),
            Some(HitTarget::ConfirmCredits("delete-test".into()))
        );
    }
    #[test]
    fn drain_intent_keeps_the_clicked_target_when_fresh_state_has_already_changed() {
        let view = |drain| {
            v2::project(&serde_json::json!({"accounts":[{"id":"drain-test","provider":"codex","user_state":"active","drain":drain}]}).to_string(),SystemTime::now()).unwrap()
        };
        for displayed in [false, true] {
            let intended = drain_intent(&view(displayed), "drain-test").unwrap();
            let fresh = view(!displayed);
            assert_eq!(row_view(&fresh, "drain-test").unwrap().raw.drain, intended);
            assert_eq!(intended, !displayed);
            assert_ne!(Some(intended), drain_intent(&fresh, "drain-test"));
        }
        assert_eq!(drain_intent(&view(false), "missing"), None);
    }
    #[test]
    fn shutdown_failure_restores_each_prior_mode_and_only_acceptance_closes() {
        for prior in [false, true] {
            assert_eq!(shutdown_outcome(prior, false), (prior, false));
            assert_eq!(shutdown_outcome(prior, true), (false, true));
        }
    }
    #[test]
    fn focused_offscreen_disclosure_is_revealed_inside_the_list() {
        let mut accounts:Vec<_>=(0..20).map(|n|serde_json::json!({"id":format!("row-{n:02}"),"provider":"codex","user_state":"active"})).collect();
        accounts.push(
            serde_json::json!({"id":"disabled-test","provider":"codex","user_state":"disabled"}),
        );
        let view = v2::project(
            &serde_json::json!({"accounts":accounts}).to_string(),
            SystemTime::now(),
        )
        .unwrap();
        let scene = geometry::Scene::build(&view, geometry::Presentation::default(), 400.);
        let target = HitTarget::Disclosure("codex".into());
        let rect = reveal_bounds(&scene, &target).unwrap();
        assert!(rect.top >= scene.list.bottom);
        let scene = geometry::Scene::build(
            &view,
            geometry::Presentation {
                offset: reveal_offset(&scene, rect),
                ..Default::default()
            },
            400.,
        );
        let rect = reveal_bounds(&scene, &target).unwrap();
        assert!(rect.top >= scene.list.top && rect.bottom <= scene.list.bottom);
        assert_eq!(
            scene.hit_test((rect.left + rect.right) / 2., (rect.top + rect.bottom) / 2.),
            Some(&target)
        );
        assert_eq!(reveal_offset(&scene, rect), scene.offset);
    }
    #[test]
    fn login_animation_is_scoped_to_a_visible_login_panel() {
        for state in [
            None,
            Some("list"),
            Some("quit"),
            Some("settings"),
            Some("login:codex"),
            Some("login:claude:failed"),
        ] {
            assert!(!login_animation_needed(false, state));
        }
        assert!(login_animation_needed(true, Some("login:codex")));
        assert!(!login_animation_needed(true, Some("list")));
    }
}
