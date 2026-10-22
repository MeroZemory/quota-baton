//! Native R7 dashboard. Install as a child of windows_tray, not the library.
//! Worker/transport/persistence I/O belongs exclusively to Bridge::enqueue's consumer.
#![cfg(windows)]
#[path = "dashboard_bridge.rs"]
pub mod dashboard_bridge;
#[path = "dashboard_time.rs"]
mod dashboard_time;
#[path = "window_logic.rs"]
mod window_logic;
use super::accessibility;
use crate::dashboard_persistence::Geometry;
pub use dashboard_bridge::{Bridge, Event, UiPort};
use qb_tray::tray_icon::{pixels::Badge, win_icon::NotifyIcon};
use qb_tray::{
    dashboard_model as model,
    geometry::{self, Scene, Target},
    render,
    state::{Command, DashboardState, ScrollRegion, Tab},
};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, HashSet},
    ffi::c_void,
    mem::size_of,
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};
use window_logic::{Editor, Lifecycle, Stamp};
use windows::{
    Win32::{
        Foundation::{FILETIME, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{
            Direct2D::{
                Common::{D2D_SIZE_U, D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT},
                D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
                D2D1_PRESENT_OPTIONS_IMMEDIATELY, D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS,
                D2D1_RENDER_TARGET_PROPERTIES, D2D1CreateFactory, ID2D1Factory,
                ID2D1HwndRenderTarget,
            },
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Gdi::{
                BeginPaint, ClientToScreen, EndPaint, GetMonitorInfoW, HDC, InvalidateRect,
                MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect, MonitorFromWindow,
                PAINTSTRUCT, ScreenToClient,
            },
        },
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{GetCurrentThread, GetThreadTimes},
        },
        UI::{
            HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::{
                GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT,
                TrackMouseEvent,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{Error, PCWSTR, Result, w},
};
use windows_numerics::Matrix3x2;

const CLASS: PCWSTR = w!("QuotaBatonDashboard");
const TITLE: PCWSTR = w!("quota-baton 대시보드");
const TIMER: usize = 0x71;
const WM_MOUSELEAVE_MESSAGE: u32 = 0x02A3;
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
static NEXT_EPOCH: AtomicUsize = AtomicUsize::new(1);
static CLASS_REGISTERED: OnceLock<bool> = OnceLock::new();
static UI_THREAD: OnceLock<u32> = OnceLock::new();
thread_local! {
    static HOST:RefCell<Option<Rc<RefCell<Host>>>>=const{RefCell::new(None)};
    static PREFERENCES:RefCell<Option<DashboardState>>=const{RefCell::new(None)};
    // Class icons outlive every HWND that may borrow them as a default.
    static CLASS_ICONS: RefCell<Option<WindowIcons>> = const { RefCell::new(None) };
    static ICON_STATE: RefCell<(bool, Badge)> = const { RefCell::new((false, Badge::Normal)) };
}

struct WindowIcons {
    small: NotifyIcon,
    big: NotifyIcon,
    key: (u32, bool, Badge),
}
impl WindowIcons {
    fn new(size: u32, dark: bool, badge: Badge) -> Result<Self> {
        Ok(Self {
            small: NotifyIcon::new(size, dark, badge).map_err(|_| invalid())?,
            big: NotifyIcon::new(32, dark, badge).map_err(|_| invalid())?,
            key: (size, dark, badge),
        })
    }
}
/// UI-thread-only publication, sharing the tray's exact theme/badge generator.
/// No TLS or Host borrow crosses SendMessage's reentrant WndProc dispatch.
pub fn set_icon_presentation(dark: bool, badge: Badge) {
    ICON_STATE.with(|s| *s.borrow_mut() = (dark, badge));
    let host = HOST.with(|s| s.borrow().clone());
    let Some(host) = host else { return };
    let hwnd = host.borrow().hwnd;
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let size = match dpi {
        0..=108 => 16,
        109..=132 => 20,
        133..=168 => 24,
        _ => 32,
    };
    if host
        .borrow()
        .icons
        .as_ref()
        .is_some_and(|icons| icons.key == (size, dark, badge))
    {
        return;
    }
    let Ok(icons) = WindowIcons::new(size, dark, badge) else {
        return;
    };
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_SMALL as usize)),
            Some(LPARAM(icons.small.as_raw() as isize)),
        );
        SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_BIG as usize)),
            Some(LPARAM(icons.big.as_raw() as isize)),
        );
    }
    host.borrow_mut().icons = Some(icons);
}

struct Frame {
    scene: render::CachedScene,
    view: Arc<model::DashboardView>,
    stamp: Stamp,
    actions: Vec<Target>,
    elements: Vec<(i32, Target)>,
    focus: Option<Target>,
    keyboard_focus: bool,
    focused: bool,
    next_clock: Option<chrono::DateTime<chrono::Utc>>,
    prepared: Option<accessibility::PreparedSnapshot>,
}
/// Bounded, opt-in timings. EndDraw is submission/present-call time, not GPU completion.
#[derive(Clone, Debug, serde::Serialize)]
pub struct FrameTiming {
    pub input_seq: u64,
    pub rebuild_reason: Option<&'static str>,
    pub tab: String,
    pub hover: Option<String>,
    pub offsets: [f32; 4],
    pub snapshot_us: u64,
    pub project_us: u64,
    pub scene_us: u64,
    pub paint_us: u64,
    pub publish_us: u64,
    pub frame_us: u64,
    pub thread_cpu_us: Option<u64>,
    pub nodes: usize,
    pub rebuilt: bool,
    pub full_scene_build: bool,
    pub retained_target: bool,
    pub partial_scroll: bool,
    pub paint_clip: Option<[f32; 4]>,
    pub draw_commands: usize,
    pub uia: PublicationTiming,
    pub phases: render::PhaseStats,
}
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct PublicationTiming {
    pub ids_us: u64,
    pub elements_us: u64,
    pub validate_us: u64,
    pub swap_us: u64,
}
fn micros(start: Instant) -> u64 {
    start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}
fn thread_cpu_ticks() -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
        .ok()?;
    }
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Some(ticks(kernel).saturating_add(ticks(user)))
}
#[cfg(feature = "testkit")]
#[allow(dead_code)]
pub fn perf_take_frames(hwnd: HWND) -> Vec<FrameTiming> {
    current(hwnd).map_or_else(Vec::new, |h| std::mem::take(&mut h.borrow_mut().timings))
}
#[cfg(feature = "testkit")]
#[allow(dead_code)]
pub fn perf_scene(hwnd: HWND) -> Option<Scene> {
    current(hwnd).and_then(|h| {
        h.borrow()
            .frame
            .as_ref()
            .map(|f| f.scene.as_scene().clone())
    })
}
#[cfg(feature = "testkit")]
#[allow(dead_code)]
pub fn perf_input_seq(hwnd: HWND) -> u64 {
    current(hwnd).map_or(0, |h| h.borrow().input_seq)
}
#[cfg(feature = "testkit")]
#[derive(Debug)]
#[allow(dead_code)]
pub struct PerfCacheState {
    pub epoch: usize,
    pub actions: usize,
    pub elements: usize,
    pub next_clock: Option<chrono::DateTime<chrono::Utc>>,
    pub clock_reconcile: bool,
}
#[cfg(feature = "testkit")]
#[allow(dead_code)]
pub fn perf_cache_state(hwnd: HWND) -> Option<PerfCacheState> {
    let host = current(hwnd)?;
    let h = host.borrow();
    let f = h.frame.as_ref()?;
    Some(PerfCacheState {
        epoch: f.stamp.epoch,
        actions: f.actions.len(),
        elements: f.elements.len(),
        next_clock: h.next_clock,
        clock_reconcile: h.clock_reconcile,
    })
}
#[cfg(feature = "testkit")]
#[allow(dead_code)]
pub fn perf_set_time(hwnd: HWND, clock: model::Clock, uptime_ms: u64) {
    if let Some(host) = current(hwnd) {
        let mut h = host.borrow_mut();
        h.clock = clock;
        h.clock_at = Instant::now();
        if let Some(started) =
            Instant::now().checked_sub(std::time::Duration::from_millis(uptime_ms))
        {
            h.started = started;
        }
    }
}

struct Host {
    hwnd: HWND,
    port: UiPort,
    bridge: Arc<dyn Bridge>,
    life: Lifecycle,
    snapshot: Arc<model::DashboardSnapshot>,
    clock: model::Clock,
    clock_at: Instant,
    started: Instant,
    last_tick: u64,
    last_clock_request: u64,
    clock_reconcile: bool,
    dark: bool,
    state: DashboardState,
    input: render::Interaction,
    edit: Editor,
    editing: Option<Target>,
    factory: ID2D1Factory,
    target: Option<ID2D1HwndRenderTarget>,
    renderer: render::Renderer,
    editor_text: qb_tray::v2_text::Text,
    frame: Option<Frame>,
    tab_frames: Vec<(Tab, Frame)>,
    pending_tab: Option<Frame>,
    scroll_scene: Option<(render::CachedScene, ScrollRegion, Option<qb_tray::Rect>)>,
    root: Option<accessibility::RootProvider>,
    ids: HashMap<String, i32>,
    next_id: i32,
    suppress_geometry: bool,
    closing: bool,
    pointer_region: Option<ScrollRegion>,
    focused: bool,
    profiling: bool,
    timings: Vec<FrameTiming>,
    input_seq: u64,
    dirty_reason: &'static str,
    projected: Option<Arc<model::DashboardView>>,
    next_clock: Option<chrono::DateTime<chrono::Utc>>,
    target_metrics: Option<([u32; 2], u32)>,
    icons: Option<WindowIcons>,
}
fn current(hwnd: HWND) -> Option<Rc<RefCell<Host>>> {
    HOST.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|h| h.borrow().hwnd == hwnd)
            .cloned()
    })
}
fn now(h: &Host) -> u64 {
    h.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}
fn epoch() -> Option<usize> {
    NEXT_EPOCH
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            n.checked_add(1).filter(|v| *v <= isize::MAX as usize)
        })
        .ok()
}
fn owner() -> Option<u64> {
    NEXT_OWNER
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            n.checked_add(1).filter(|n| *n <= usize::MAX as u64)
        })
        .ok()
}
fn invalid() -> Error {
    Error::from_hresult(windows::Win32::Foundation::E_FAIL)
}
unsafe fn monitor(hwnd: HWND) -> MONITORINFO {
    let mut m = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut m) }
        .as_bool()
    {
        m.rcWork = RECT {
            left: 0,
            top: 0,
            right: 1024,
            bottom: 768,
        };
        m.rcMonitor = m.rcWork;
    }
    m
}
fn edges(r: RECT) -> [i32; 4] {
    [r.left, r.top, r.right, r.bottom]
}
unsafe fn metrics(hwnd: HWND) -> Option<([u32; 2], u32)> {
    let mut r = RECT::default();
    unsafe { GetClientRect(hwnd, &mut r) }.ok()?;
    let size = [
        (r.right - r.left).max(0) as u32,
        (r.bottom - r.top).max(0) as u32,
    ];
    Some((size, unsafe { GetDpiForWindow(hwnd) }.max(48)))
}
fn invalidate_frame(h: &mut Host) {
    // Retain the last completed read snapshot until its replacement is ready.
    // Close action admission immediately; reopen only after successful publication.
    h.frame = None;
    h.tab_frames.clear();
    h.pending_tab = None;
    h.scroll_scene = None;
    h.life.invalidate();
}

fn redraw(hwnd: HWND) {
    redraw_for(hwnd, "state");
}
fn redraw_for(hwnd: HWND, reason: &'static str) {
    if let Some(host) = current(hwnd) {
        let mut h = host.borrow_mut();
        invalidate_frame(&mut h);
        h.projected = None;
        h.dirty_reason = reason;
    }
    repaint(hwnd);
}
fn repaint(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}
fn complete_automation_frame(hwnd: HWND) {
    // Win32 can defer a low-priority paint for an occluded client. Dispatch the
    // same production WM_PAINT synchronously, after releasing all Host borrows,
    // so automation can read the completed state it just changed.
    unsafe {
        SendMessageW(hwnd, WM_PAINT, None, None);
    }
}

fn frame_valid(h: &Host) -> bool {
    let Some(f) = h.frame.as_ref() else {
        return false;
    };
    let Some((pixels, dpi)) = (unsafe { metrics(h.hwnd) }) else {
        return false;
    };
    h.life.accepts(h.life.owner, f.stamp.epoch, pixels, dpi)
}

/// Call on the existing tray UI thread. No second message loop or runtime.
pub fn open_or_focus(
    anchor: HWND,
    bridge: Arc<dyn Bridge>,
    initial: Arc<model::DashboardSnapshot>,
    clock: model::Clock,
    dark: bool,
) -> Result<()> {
    let thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    if *UI_THREAD.get_or_init(|| thread) != thread {
        return Err(invalid());
    }
    let old = HOST.with(|s| s.borrow().as_ref().map(|h| h.borrow().hwnd));
    if let Some(hwnd) = old {
        unsafe {
            if IsWindow(Some(hwnd)).as_bool() {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
                return Ok(());
            }
        }
    }
    let module = unsafe { GetModuleHandleW(None) }?;
    let badge = ICON_STATE.with(|s| s.borrow().1);
    if CLASS_REGISTERED.get().is_none() {
        let icons = WindowIcons::new(16, dark, badge)?;
        let registered = unsafe {
            RegisterClassExW(&WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: HINSTANCE(module.0),
                lpszClassName: CLASS,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hIcon: HICON(icons.big.as_raw()),
                hIconSm: HICON(icons.small.as_raw()),
                ..Default::default()
            }) != 0
        };
        let _ = CLASS_REGISTERED.set(registered);
        if registered {
            CLASS_ICONS.with(|s| *s.borrow_mut() = Some(icons));
        }
    }
    if CLASS_REGISTERED.get() != Some(&true) {
        return Err(invalid());
    }
    let token = owner().ok_or_else(invalid)?;
    let factory: ID2D1Factory =
        unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }?;
    let renderer = render::Renderer::new(dark)?;
    let editor_text = qb_tray::v2_text::Text::new()?;
    let m = unsafe { monitor(anchor) };
    let dpi = unsafe { GetDpiForWindow(anchor) }.max(96);
    let r = window_logic::default_rect(edges(m.rcWork), dpi);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS,
            TITLE,
            WS_OVERLAPPEDWINDOW,
            r[0],
            r[1],
            r[2] - r[0],
            r[3] - r[1],
            None,
            None,
            Some(HINSTANCE(module.0)),
            None,
        )
    }?;
    let port = UiPort::new(token, hwnd);
    let mut state = PREFERENCES
        .with(|s| s.borrow_mut().take())
        .unwrap_or_default();
    let startup_commands = window_logic::startup_commands(&mut state);
    let root = accessibility::create_root_provider(hwnd.0 as isize).ok();
    HOST.with(|s| {
        *s.borrow_mut() = Some(Rc::new(RefCell::new(Host {
            hwnd,
            port: port.clone(),
            bridge: bridge.clone(),
            life: Lifecycle::new(token),
            snapshot: initial,
            clock,
            clock_at: Instant::now(),
            started: Instant::now(),
            last_tick: 0,
            last_clock_request: 0,
            clock_reconcile: false,
            dark,
            state,
            input: Default::default(),
            edit: Default::default(),
            editing: None,
            factory,
            target: None,
            renderer,
            editor_text,
            frame: None,
            tab_frames: vec![],
            pending_tab: None,
            scroll_scene: None,
            root,
            ids: HashMap::new(),
            next_id: 1,
            suppress_geometry: true,
            closing: false,
            pointer_region: None,
            focused: false,
            profiling: cfg!(any(debug_assertions, feature = "testkit"))
                && std::env::var_os("QB_DASHBOARD_PROFILE").is_some(),
            timings: Vec::new(),
            input_seq: 0,
            dirty_reason: "initial",
            projected: None,
            next_clock: None,
            target_metrics: None,
            icons: None,
        })))
    });
    set_icon_presentation(dark, badge);
    bridge.enqueue(Event::Opened { port });
    for command in startup_commands {
        bridge.enqueue(Event::Command {
            owner: token,
            command,
        });
    }
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER, 250, None);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
    }
    if let Some(h) = current(hwnd) {
        h.borrow_mut().suppress_geometry = false;
    }
    redraw(hwnd);
    Ok(())
}

/// Shutdown barrier for the tray UI thread. Call BEFORE destroying the flyout
/// or stopping its worker/coordinator. The dashboard's WM_CLOSE handler queues
/// final eligible GeometryDirty and Closed synchronously before this returns.
/// No file/network I/O is performed and no Host/TLS borrow crosses dispatch.
pub fn close_now_on_ui_thread() -> Result<()> {
    let thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    if UI_THREAD.get().is_some_and(|owner| *owner != thread) {
        return Err(invalid());
    }
    // Copy handles only. SendMessage/DestroyWindow reenter the same WndProc,
    // which must be free to borrow and finally remove HOST during teardown.
    let hwnd = HOST.with(|slot| slot.borrow().as_ref().map(|host| host.borrow().hwnd));
    let Some(hwnd) = hwnd else {
        return Ok(());
    };
    if unsafe { GetWindowThreadProcessId(hwnd, None) } != thread {
        return Err(invalid());
    }
    unsafe {
        SendMessageW(hwnd, WM_CLOSE, None, None);
    }
    if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err(invalid());
    }
    Ok(())
}

fn dispatch(hwnd: HWND, commands: Vec<Command>) {
    for command in commands {
        match command {
            Command::FocusSearch => focus_edit(hwnd, Target::Search),
            Command::FocusEventSearch => focus_edit(hwnd, Target::EventSearch),
            command => {
                if let Some(h) = current(hwnd) {
                    let h = h.borrow();
                    h.bridge.enqueue(Event::Command {
                        owner: h.life.owner,
                        command,
                    });
                }
            }
        }
    }
}
fn activate(hwnd: HWND, target: Target) {
    activate_target(hwnd, target, false);
}
fn activate_target(hwnd: HWND, target: Target, enter: bool) {
    let Some(host) = current(hwnd) else { return };
    let previous_tab = host.borrow().state.tab;
    let commands = {
        let mut h = host.borrow_mut();
        if !frame_valid(&h)
            || !h
                .frame
                .as_ref()
                .unwrap()
                .scene
                .targets
                .iter()
                .any(|t| t.enabled && t.target == target)
        {
            return;
        }
        h.input.focus = if target == Target::CloseDetail {
            h.state.selected.clone().map(Target::Session)
        } else {
            Some(target.clone())
        };
        if enter && let Target::Session(id) = &target {
            window_logic::open_session(&mut h.state, id)
        } else {
            h.state.activate(&target)
        }
    };
    let edited = !matches!(target, Target::Search | Target::EventSearch) && finish_edit(hwnd);
    if matches!(target, Target::Tab(_)) && !edited {
        // Projection contains all tabs and does not depend on state.tab. A tab
        // change only changes geometry. Mail/time/filter invalidation still
        // clears this projection through redraw_for before it can be reused.
        if let Some(host) = current(hwnd) {
            let mut h = host.borrow_mut();
            let clock = virtual_clock(&h).utc();
            let expired = h.next_clock.is_some_and(|at| clock >= at)
                || h.tab_frames.iter().any(|(tab, frame)| {
                    *tab == h.state.tab
                        && (clock < frame.view.clock.utc()
                            || frame.next_clock.is_some_and(|at| clock >= at))
                });
            if expired {
                invalidate_frame(&mut h);
                h.projected = None;
            } else {
                if let Some(frame) = h.frame.take()
                    && !frame.scene.modal
                {
                    h.tab_frames.retain(|(tab, _)| *tab != previous_tab);
                    h.tab_frames.push((previous_tab, frame));
                }
                h.pending_tab = h
                    .tab_frames
                    .iter()
                    .position(|(tab, _)| *tab == h.state.tab)
                    .map(|index| h.tab_frames.swap_remove(index).1);
                if h.pending_tab.is_none() {
                    // A newly revealed tab may have earlier time buckets than
                    // the old visible tab. Project it at the current clock.
                    h.projected = None;
                }
                h.scroll_scene = None;
                h.life.invalidate();
            }
            h.dirty_reason = "tab";
        }
        repaint(hwnd);
    } else {
        redraw(hwnd);
    }
    dispatch(hwnd, commands);
}
fn focus_edit(hwnd: HWND, target: Target) {
    let Some(host) = current(hwnd) else { return };
    {
        let mut h = host.borrow_mut();
        if !h.snapshot.connection.core_ok || h.state.popup.is_some() {
            return;
        }
        let text = if target == Target::EventSearch {
            h.state.event_search.clone()
        } else {
            h.state.search.clone()
        };
        h.edit.reset(&text);
        h.editing = Some(target.clone());
        h.input.focus = Some(target);
    }
    unsafe {
        let _ = SetFocus(Some(hwnd));
    }
    // Focus/caret decoration does not change hit geometry. Preserve the current
    // frame for a following UIA Value message admitted from that same frame.
    repaint(hwnd);
}
fn finish_edit(hwnd: HWND) -> bool {
    if let Some(host) = current(hwnd) {
        let mut h = host.borrow_mut();
        h.edit.composition.clear();
        let changed = match h.editing {
            Some(Target::Search) => {
                h.state.search != h.edit.text
                    || h.state.table_offset != 0.
                    || h.state.table_freeze.frozen
            }
            Some(Target::EventSearch) => {
                h.state.event_search != h.edit.text || h.state.events_offset != 0.
            }
            _ => false,
        };
        sync_search(&mut h);
        h.editing = None;
        changed
    } else {
        false
    }
}
fn sync_search(h: &mut Host) {
    if !h.snapshot.connection.core_ok {
        return;
    }
    let displayed = h.edit.displayed();
    match h.editing {
        Some(Target::Search) => {
            h.state.search = displayed;
            h.state.table_offset = 0.;
            h.state.table_freeze.frozen = false;
        }
        Some(Target::EventSearch) => {
            h.state.event_search = displayed;
            h.state.events_offset = 0.;
        }
        _ => {}
    }
}

fn apply_mail(hwnd: HWND) {
    let Some(host) = current(hwnd) else { return };
    let pending = host.borrow().port.take();
    let Some(mut mail) = pending else { return };
    let mut placement = None;
    let mut changed = false;
    {
        let mut h = host.borrow_mut();
        if h.closing {
            return;
        }
        if let Some((snapshot, clock)) = mail.snapshot.take()
            && snapshot.revision >= h.snapshot.revision
        {
            if !snapshot.connection.core_ok {
                h.editing = None;
                h.edit.composition.clear();
                h.input.focus = None;
                h.input.pressed = None;
                h.state.popup = None;
            }
            changed = !Arc::ptr_eq(&h.snapshot, &snapshot);
            h.snapshot = snapshot;
            h.clock = clock;
            h.clock_at = Instant::now();
            h.clock_reconcile = false;
        }
        if let Some(clock) = mail.clock.take() {
            let old_now = virtual_clock(&h).utc();
            changed |= h.clock.day_start != clock.day_start
                || h.clock.day_end != clock.day_end
                || h.clock.now.offset() != clock.now.offset()
                || (old_now - clock.utc()).num_seconds().abs() > 2
                || h.projected
                    .as_ref()
                    .is_some_and(|view| clock.utc() < view.clock.utc())
                || h.tab_frames
                    .iter()
                    .any(|(_, frame)| clock.utc() < frame.view.clock.utc())
                || h.pending_tab
                    .as_ref()
                    .is_some_and(|frame| clock.utc() < frame.view.clock.utc())
                || h.next_clock.is_some_and(|at| clock.utc() >= at);
            h.clock = clock;
            h.clock_at = Instant::now();
            h.clock_reconcile = false;
        }
        if let Some((ticket, detail)) = mail.detail.take() {
            h.state.publish_detail(&ticket, detail);
            changed = true;
        }
        for id in mail.removed {
            h.state.detail_removed(&id);
            changed = true;
        }
        if let Some(dark) = mail.theme.take()
            && h.dark != dark
            && let Ok(renderer) = render::Renderer::new(dark)
        {
            h.renderer = renderer;
            h.dark = dark;
            changed = true;
        }
        if let Some(saved) = mail.geometry.take() {
            let token = h.life.owner;
            placement = h.life.load(token, saved);
        }
    }
    let icon_dark = host.borrow().dark;
    let badge = ICON_STATE.with(|s| s.borrow().1);
    set_icon_presentation(icon_dark, badge);
    if let Some(saved) = placement {
        let saved_rect = RECT {
            left: saved.left,
            top: saved.top,
            right: saved.left.saturating_add(saved.width as i32),
            bottom: saved.top.saturating_add(saved.height as i32),
        };
        let mon = unsafe { MonitorFromRect(&saved_rect, MONITOR_DEFAULTTONEAREST) };
        let mut m = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(mon, &mut m) }.as_bool() {
            m = unsafe { monitor(hwnd) };
        }
        let mut dpi = unsafe { GetDpiForWindow(hwnd) }.max(48);
        let mut dy = dpi;
        unsafe {
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi, &mut dy);
        }
        if let Some(r) = saved.restore(edges(m.rcWork), dpi) {
            host.borrow_mut().suppress_geometry = true;
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r[0],
                    r[1],
                    r[2] - r[0],
                    r[3] - r[1],
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                if saved.maximized {
                    let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
                }
            }
            host.borrow_mut().suppress_geometry = false;
        }
    }
    if changed {
        redraw_for(hwnd, "mail");
    }
}

unsafe fn remember_geometry(hwnd: HWND) {
    let Some(host) = current(hwnd) else { return };
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return;
    }
    let mut r = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
        return;
    }
    let maximized = unsafe { IsZoomed(hwnd) }.as_bool();
    if maximized {
        let mut p = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if unsafe { GetWindowPlacement(hwnd, &mut p) }.is_ok() {
            let m = unsafe { monitor(hwnd) };
            let dx = m.rcWork.left - m.rcMonitor.left;
            let dy = m.rcWork.top - m.rcMonitor.top;
            r = RECT {
                left: p.rcNormalPosition.left + dx,
                top: p.rcNormalPosition.top + dy,
                right: p.rcNormalPosition.right + dx,
                bottom: p.rcNormalPosition.bottom + dy,
            };
        }
    }
    let geometry = Geometry {
        left: r.left,
        top: r.top,
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
        dpi: unsafe { GetDpiForWindow(hwnd) }.max(48),
        maximized,
    };
    let mut h = host.borrow_mut();
    let time = now(&h);
    let user = !h.suppress_geometry;
    h.life.placed(geometry, time, user);
}
fn flush_geometry(hwnd: HWND, force: bool) {
    if let Some(host) = current(hwnd) {
        let mut h = host.borrow_mut();
        let time = now(&h);
        if let Some((sequence, geometry)) = h.life.dirty(time, force) {
            h.bridge.enqueue(Event::GeometryDirty {
                owner: h.life.owner,
                sequence,
                geometry,
            });
        }
    }
}

unsafe fn print_client(hwnd: HWND, dc: HDC) -> Result<()> {
    if dc.is_invalid() {
        return Err(invalid());
    }
    // Refresh the same frame used by hit-testing and UIA before Windows copies it.
    unsafe { paint(hwnd) }?;
    let host = current(hwnd).ok_or_else(invalid)?;
    let mut h = host.borrow_mut();
    let (pixels, dpi) = unsafe { metrics(hwnd) }.ok_or_else(invalid)?;
    let target = unsafe {
        h.factory
            .CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: dpi as f32,
                dpiY: dpi as f32,
                ..Default::default()
            })
    }?;
    unsafe {
        target.BindDC(
            dc,
            &RECT {
                left: 0,
                top: 0,
                right: pixels[0] as i32,
                bottom: pixels[1] as i32,
            },
        )?;
    }
    let mut frame = h.frame.take().ok_or_else(invalid)?;
    let result = paint_decorated(&h, &mut frame.scene, &target, None);
    h.frame = Some(frame);
    result
}

fn virtual_clock(h: &Host) -> model::Clock {
    let mut clock = h.clock.clone();
    clock.now += chrono::Duration::seconds(h.clock_at.elapsed().as_secs().min(120) as i64);
    clock
}

unsafe fn paint(hwnd: HWND) -> Result<()> {
    let mut ps = PAINTSTRUCT::default();
    unsafe { BeginPaint(hwnd, &mut ps) };
    let result = (|| {
        let Some(host) = current(hwnd) else {
            return Ok(());
        };
        let mut h = host.borrow_mut();
        let (pixels, dpi) = unsafe { metrics(hwnd) }.ok_or_else(invalid)?;
        if pixels.contains(&0) {
            return Ok(());
        }
        let profile = h.profiling;
        h.renderer.set_phase_stats_enabled(profile);
        h.renderer.reset_phase_stats();
        h.editor_text.set_phase_stats_enabled(profile);
        h.editor_text.reset_phase_stats();
        let frame_start = profile.then(Instant::now);
        let thread_start = profile.then(thread_cpu_ticks).flatten();
        if h.frame.is_some() && !frame_valid(&h) {
            invalidate_frame(&mut h);
            h.projected = None;
        }
        let retained_target = h.target.is_some() && h.target_metrics == Some((pixels, dpi));
        if h.target.is_none() {
            h.renderer.clear_target_resources();
            h.target = Some(unsafe {
                h.factory.CreateHwndRenderTarget(
                    &D2D1_RENDER_TARGET_PROPERTIES {
                        dpiX: dpi as f32,
                        dpiY: dpi as f32,
                        ..Default::default()
                    },
                    &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                        hwnd,
                        pixelSize: D2D_SIZE_U {
                            width: pixels[0],
                            height: pixels[1],
                        },
                        // Event-driven painting must not block the window thread
                        // for a refresh boundary. Retain pixels for damage clips.
                        presentOptions: D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS
                            | D2D1_PRESENT_OPTIONS_IMMEDIATELY,
                    },
                )
            }?);
            h.target_metrics = Some((pixels, dpi));
        }
        let target = h.target.as_ref().unwrap().clone();
        if h.target_metrics != Some((pixels, dpi)) {
            unsafe {
                target.Resize(&D2D_SIZE_U {
                    width: pixels[0],
                    height: pixels[1],
                })?;
                target.SetDpi(dpi as f32, dpi as f32);
            }
            h.target_metrics = Some((pixels, dpi));
        }
        unsafe {
            target.SetTransform(&Matrix3x2::identity());
        }
        let restored = h.pending_tab.take();
        let republished = restored.is_some();
        let rebuilt = h.frame.is_none() && !republished;
        let (mut snapshot_us, mut project_us, mut scene_us) = (0, 0, 0);
        let mut scroll_damage = None;
        let mut frame = if let Some(frame) = h.frame.take().or(restored) {
            h.next_clock = frame.next_clock;
            h.projected = Some(frame.view.clone());
            frame
        } else {
            let clock = virtual_clock(&h);
            let view = if let Some(view) = &h.projected {
                view.clone()
            } else {
                let part = profile.then(Instant::now);
                let snapshot = h.snapshot.clone();
                snapshot_us = part.map_or(0, micros);
                let part = profile.then(Instant::now);
                let view = Arc::new(model::project(&snapshot, &mut h.state, &clock));
                project_us = part.map_or(0, micros);
                h.projected = Some(view.clone());
                view
            };
            let size = [
                pixels[0] as f32 * 96. / dpi as f32,
                pixels[1] as f32 * 96. / dpi as f32,
            ];
            let part = profile.then(Instant::now);
            let scene = if let Some((previous, region, proposed)) = h.scroll_scene.take() {
                let scene = h
                    .renderer
                    .scene_scroll(&view, &h.state, size, &previous, region)?;
                if retained_target
                    && let Some(proposed) = proposed
                    && scene.scroll_damage_from(&previous) == Some(proposed)
                    && scroll_decorations_contained(&h, &previous, &scene, proposed)
                {
                    scroll_damage = Some(proposed);
                }
                scene
            } else {
                h.renderer.scene_shared(&view, &h.state, size)?
            };
            scene_us = part.map_or(0, micros);
            for region in &scene.content {
                h.state
                    .scroll(region.region, region.offset, region.max_offset);
            }
            if h.input.focus.as_ref().is_some_and(|focus| {
                !scene
                    .targets
                    .iter()
                    .any(|t| t.enabled && &t.target == focus)
            }) {
                h.input.focus = None;
            }
            h.next_clock = dashboard_time::next_visible_time_change(
                &h.snapshot,
                &h.state,
                &scene,
                &view.clock,
                dpi,
            );
            Frame {
                scene,
                view,
                stamp: Stamp {
                    owner: h.life.owner,
                    epoch: epoch().ok_or_else(invalid)?,
                    pixels,
                    dpi,
                },
                actions: vec![],
                elements: vec![],
                focus: h.input.focus.clone(),
                keyboard_focus: h.input.keyboard_focus,
                focused: h.focused,
                next_clock: h.next_clock,
                prepared: None,
            }
        };
        let part = profile.then(Instant::now);
        let clip = ((!rebuilt || scroll_damage.is_some())
            && !republished
            && ps.rcPaint.right > ps.rcPaint.left
            && ps.rcPaint.bottom > ps.rcPaint.top)
            .then(|| qb_tray::Rect {
                left: ps.rcPaint.left as f32 * 96. / dpi as f32,
                top: ps.rcPaint.top as f32 * 96. / dpi as f32,
                right: ps.rcPaint.right as f32 * 96. / dpi as f32,
                bottom: ps.rcPaint.bottom as f32 * 96. / dpi as f32,
            });
        paint_decorated(&h, &mut frame.scene, &target, clip)?;
        let paint_us = part.map_or(0, micros);
        let part = profile.then(Instant::now);
        let mut uia = PublicationTiming::default();
        if rebuilt
            || republished
            || frame.focus != h.input.focus
            || frame.keyboard_focus != h.input.keyboard_focus
            || frame.focused != h.focused
        {
            let reuse = !rebuilt
                && frame.focus == h.input.focus
                && frame.keyboard_focus == h.input.keyboard_focus
                && frame.focused == h.focused;
            if !rebuilt {
                frame.stamp.epoch = epoch().ok_or_else(invalid)?;
            }
            frame.focus = h.input.focus.clone();
            frame.keyboard_focus = h.input.keyboard_focus;
            frame.focused = h.focused;
            uia = publish_accessibility(&mut h, &mut frame, reuse)?;
            h.life.painted(frame.stamp);
        }
        let publish_us = part.map_or(0, micros);
        if let Some(start) = frame_start {
            let mut phases = h.renderer.phase_stats();
            let editor = h.editor_text.phase_stats();
            phases.text_measure_us = phases
                .text_measure_us
                .saturating_add(editor.text_measure_us);
            phases.layout_creations = phases
                .layout_creations
                .saturating_add(editor.layout_creations);
            phases.format_creations = phases
                .format_creations
                .saturating_add(editor.format_creations);
            let timing = FrameTiming {
                input_seq: h.input_seq,
                rebuild_reason: rebuilt.then_some(h.dirty_reason),
                tab: format!("{:?}", h.state.tab),
                hover: h.input.hover.as_ref().map(|v| format!("{v:?}")),
                offsets: [
                    h.state.table_offset,
                    h.state.side_offset,
                    h.state.events_offset,
                    h.state.usage_offset,
                ],
                snapshot_us,
                project_us,
                scene_us,
                paint_us,
                publish_us,
                frame_us: micros(start),
                thread_cpu_us: thread_start.and_then(|before| {
                    thread_cpu_ticks().map(|after| after.saturating_sub(before) / 10)
                }),
                nodes: frame.scene.accessibility.len(),
                rebuilt,
                full_scene_build: rebuilt && frame.scene.rebuilt_region().is_none(),
                retained_target,
                partial_scroll: scroll_damage.is_some(),
                paint_clip: clip.map(|r| [r.left, r.top, r.right, r.bottom]),
                draw_commands: frame.scene.draw.len(),
                uia,
                phases,
            };
            if h.timings.len() < 4096 {
                h.timings.push(timing);
            }
        }
        h.frame = Some(frame);
        Ok(())
    })();
    unsafe {
        let _ = EndPaint(hwnd, &ps);
    }
    if result.is_err()
        && let Some(host) = current(hwnd)
    {
        let mut h = host.borrow_mut();
        h.renderer.clear_target_resources();
        h.target = None;
        h.target_metrics = None;
        invalidate_frame(&mut h);
    }
    result
}

fn paint_decorated(
    h: &Host,
    scene: &mut Scene,
    target: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    clip: Option<qb_tray::Rect>,
) -> Result<()> {
    let original_len = scene.draw.len();
    let inserted = decorate_editor(h, scene)?;
    let result = h.renderer.paint_clipped(target, scene, &h.input, clip);
    if let Some(index) = inserted {
        scene.draw.remove(index);
    }
    scene.draw.truncate(original_len);
    result
}
/// Search selection/caret is appended to the SAME Scene using its exact text
/// style and boxes, measured by the same shared DirectWrite primitive.
fn decorate_editor(h: &Host, scene: &mut Scene) -> Result<Option<usize>> {
    use geometry::{Color, Draw};
    use qb_tray::v2_text::{Align, Face, Style, Wrap};
    use windows::Win32::Graphics::{
        Direct2D::Common::D2D_RECT_F,
        DirectWrite::{DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD},
    };
    if !h.focused {
        return Ok(None);
    }
    let Some(edit) = h.editing.as_ref() else {
        return Ok(None);
    };
    let Some(hit) = scene.targets.iter().find(|t| &t.target == edit) else {
        return Ok(None);
    };
    let Some((index, rect, measure)) = scene.draw.iter().enumerate().find_map(|(i, draw)| {
        if let Draw::Text { rect, measure, .. } = draw
            && measure.face != geometry::Face::Icon
            && geometry::contains(hit.rect, rect.left, rect.top)
        {
            return Some((i, *rect, measure.clone()));
        }
        None
    }) else {
        return Ok(None);
    };
    let style = Style {
        face: match measure.face {
            geometry::Face::Text => Face::Text,
            geometry::Face::Mono => Face::Mono,
            geometry::Face::Icon => Face::Icon,
        },
        size: measure.size,
        line_height: measure.line_height,
        weight: if measure.bold {
            DWRITE_FONT_WEIGHT_SEMI_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        },
        align: Align::Leading,
        wrap: Wrap::SingleLine,
    };
    let text = h.edit.displayed();
    let advance = |count: usize| -> Result<f32> {
        let prefix: String = text.chars().take(count).collect();
        let width = h
            .editor_text
            .layout(
                &prefix,
                D2D_RECT_F {
                    left: 0.,
                    top: 0.,
                    right: 16384.,
                    bottom: 128.,
                },
                style,
            )?
            .metrics
            .natural_width;
        Ok((rect.left + width).min(rect.right - 1.).max(rect.left))
    };
    let from = h.edit.anchor.min(h.edit.cursor);
    let to = h.edit.anchor.max(h.edit.cursor);
    let cursor = if h.edit.composition.is_empty() {
        h.edit.cursor
    } else {
        from + h.edit.composition.chars().count()
    };
    let mut selection = None;
    let mut suffix = Vec::new();
    if from != to && h.edit.composition.is_empty() {
        let selected_rect = qb_tray::Rect {
            left: advance(from)?,
            right: advance(to)?,
            top: rect.top,
            bottom: (rect.top + measure.line_height).min(rect.bottom),
        };
        selection = Some(Draw::Fill {
            rect: selected_rect,
            color: Color::Select,
            radius: 0.,
        });
    }
    if !h.edit.composition.is_empty() {
        suffix.push(Draw::Line {
            from: [advance(from)?, rect.bottom - 1.],
            to: [advance(cursor)?, rect.bottom - 1.],
            color: Color::Accent,
            width: 1.,
            dash: None,
        });
    }
    if now(h) % 1000 < 500 {
        let x = advance(cursor)?;
        suffix.push(Draw::Line {
            from: [x, rect.top],
            to: [x, (rect.top + measure.line_height).min(rect.bottom)],
            color: Color::Text,
            width: 1.,
            dash: None,
        });
    }
    let inserted = selection.map(|draw| {
        scene.draw.insert(index, draw);
        index
    });
    scene.draw.extend(suffix);
    Ok(inserted)
}

fn physical(rect: qb_tray::Rect, origin: POINT, scale: f64) -> [f64; 4] {
    [
        origin.x as f64 + rect.left as f64 * scale,
        origin.y as f64 + rect.top as f64 * scale,
        (rect.right - rect.left).max(0.) as f64 * scale,
        (rect.bottom - rect.top).max(0.) as f64 * scale,
    ]
}
fn accessibility_interactive(
    node: &geometry::AccessibleNode,
    enabled_hits: &BTreeSet<&Target>,
) -> bool {
    !node.hidden
        && node.enabled
        && geometry::width(node.rect) > 0.
        && geometry::height(node.rect) > 0.
        && node
            .target
            .as_ref()
            .is_some_and(|t| enabled_hits.contains(t))
}
fn publish_accessibility(
    h: &mut Host,
    frame: &mut Frame,
    reuse: bool,
) -> Result<PublicationTiming> {
    let mut timing = PublicationTiming::default();
    let started = h.profiling.then(Instant::now);
    if reuse && let Some(prepared) = &frame.prepared {
        h.root
            .as_ref()
            .ok_or_else(invalid)?
            .publish_prepared(prepared, frame.stamp.epoch)?;
        timing.swap_us = started.map_or(0, micros);
        return Ok(timing);
    }
    // A focus-only publication replaces its map just like a new scene.
    frame.actions.clear();
    frame.elements.clear();
    let failure = |reason| Error::new(windows::Win32::Foundation::E_FAIL, reason);
    let mut origin = POINT::default();
    if !unsafe { ClientToScreen(h.hwnd, &mut origin) }.as_bool() {
        return Err(failure("dashboard UIA client origin unavailable"));
    }
    // Allocate every current semantic node before resolving any parent. Hidden
    // chart table/cells remain in the tree even though they have no hit target.
    let mut keys: HashSet<&str> = frame
        .scene
        .accessibility
        .iter()
        .map(|node| node.key.as_str())
        .collect();
    if keys.len() != frame.scene.accessibility.len() {
        return Err(failure("dashboard UIA duplicate semantic key"));
    }
    let required = keys.iter().filter(|key| !h.ids.contains_key(**key)).count();
    for (_, cached) in &h.tab_frames {
        keys.extend(
            cached
                .scene
                .accessibility
                .iter()
                .map(|node| node.key.as_str()),
        );
    }
    h.ids.retain(|key, _| keys.contains(key.as_str()));
    let Ok(required) = i32::try_from(required) else {
        return Err(failure("dashboard UIA element count overflow"));
    };
    if h.next_id <= 0 || h.next_id.checked_add(required).is_none() {
        return Err(failure("dashboard UIA id range exhausted"));
    }
    let mut node_ids = Vec::with_capacity(frame.scene.accessibility.len());
    for node in &frame.scene.accessibility {
        let id = if let Some(id) = h.ids.get(&node.key) {
            *id
        } else {
            let Some(next) = h.next_id.checked_add(1) else {
                return Err(failure("dashboard UIA id allocation overflow"));
            };
            let id = h.next_id;
            h.ids.insert(node.key.clone(), id);
            h.next_id = next;
            id
        };
        node_ids.push(id);
    }
    let enabled_hits: BTreeSet<_> = frame
        .scene
        .targets
        .iter()
        .filter(|hit| hit.enabled)
        .map(|hit| &hit.target)
        .collect();
    let mut elements = Vec::with_capacity(frame.scene.accessibility.len());
    timing.ids_us = started.map_or(0, micros);
    let started = h.profiling.then(Instant::now);
    let mut focused = false;
    for (node, id) in frame.scene.accessibility.iter().zip(node_ids) {
        let parent = match node.parent.as_ref() {
            Some(key) => {
                let Some(id) = h.ids.get(key) else {
                    return Err(failure("dashboard UIA missing semantic parent"));
                };
                Some(*id)
            }
            None => None,
        };
        let kind = match node.role {
            geometry::Role::Tab => accessibility::Kind::Tab,
            geometry::Role::Button => accessibility::Kind::Button,
            geometry::Role::Checkbox => accessibility::Kind::Checkbox,
            geometry::Role::Edit => accessibility::Kind::Edit,
            geometry::Role::ComboBox => accessibility::Kind::ComboBox,
            geometry::Role::Table => accessibility::Kind::Table,
            geometry::Role::ColumnHeader => accessibility::Kind::ColumnHeader,
            geometry::Role::Row => accessibility::Kind::DataRow,
            geometry::Role::Text => accessibility::Kind::Text,
            geometry::Role::Chart => accessibility::Kind::Chart,
            geometry::Role::Group => accessibility::Kind::Group,
        };
        let interactive = accessibility_interactive(node, &enabled_hits);
        let text = kind == accessibility::Kind::Text;
        let action = if interactive && !text {
            node.target.as_ref().map(|t| {
                let index = frame.actions.len();
                frame.actions.push(t.clone());
                frame.elements.push((id, t.clone()));
                index
            })
        } else {
            None
        };
        let is_focused = !focused
            && interactive
            && !text
            && node.target.as_ref() == h.input.focus.as_ref()
            && h.focused;
        focused |= is_focused;
        elements.push(accessibility::Element {
            id,
            parent,
            name: node.name.clone(),
            kind,
            bounds: if node.hidden {
                [0.; 4]
            } else {
                physical(node.rect, origin, frame.stamp.dpi as f64 / 96.)
            },
            enabled: node.enabled,
            action,
            focusable: interactive && !text,
            focused: is_focused,
            value: node.value.clone(),
            selected: Some(node.selected),
            expanded: node.expanded,
        });
    }
    timing.elements_us = started.map_or(0, micros);
    if let Some(root) = h.root.as_ref() {
        #[cfg(all(test, feature = "testkit"))]
        let count = elements.len();
        let started = h.profiling.then(Instant::now);
        let prepared = accessibility::prepare_snapshot(accessibility::Snapshot {
            automation_id: "quota-baton.dashboard",
            name: "quota-baton 대시보드".into(),
            bounds: physical(frame.scene.rect, origin, frame.stamp.dpi as f64 / 96.),
            elements,
        })?;
        timing.validate_us = started.map_or(0, micros);
        let started = h.profiling.then(Instant::now);
        let result = root.publish_prepared(&prepared, frame.stamp.epoch);
        if result.is_ok() {
            frame.prepared = Some(prepared);
        }
        #[cfg(all(test, feature = "testkit"))]
        if !h.profiling {
            eprintln!(
                "dashboard UIA publish epoch={} nodes={} result={result:?}",
                frame.stamp.epoch, count
            );
        }
        timing.swap_us = started.map_or(0, micros);
        result.map(|()| timing)
    } else {
        Err(failure("dashboard UIA provider unavailable"))
    }
}

fn mouse_point(hwnd: HWND, lparam: LPARAM, screen: bool) -> [f32; 2] {
    let mut p = POINT {
        x: lparam.0 as i16 as i32,
        y: (lparam.0 >> 16) as i16 as i32,
    };
    if screen {
        unsafe {
            let _ = ScreenToClient(hwnd, &mut p);
        }
    }
    let scale = unsafe { GetDpiForWindow(hwnd) }.max(48) as f32 / 96.;
    [p.x as f32 / scale, p.y as f32 / scale]
}
fn mouse_move(hwnd: HWND, p: [f32; 2]) {
    let Some(host) = current(hwnd) else { return };
    let dirty;
    let released;
    {
        let mut h = host.borrow_mut();
        if !frame_valid(&h) {
            return;
        }
        let scene = &h.frame.as_ref().unwrap().scene;
        let hover = scene.hit_test(p[0], p[1]).cloned();
        // Tooltips/charts can extend past a control; conservatively repaint the
        // whole client in that case. Plain controls affect only old/new bounds.
        dirty = if !scene.charts.is_empty()
            || scene.tooltip_at(p[0], p[1]).is_some()
            || h.input
                .pointer
                .is_some_and(|p| scene.tooltip_at(p[0], p[1]).is_some())
        {
            None
        } else {
            scene
                .targets
                .iter()
                .filter(|t| {
                    Some(&t.target) == hover.as_ref() || Some(&t.target) == h.input.hover.as_ref()
                })
                .map(|t| t.rect)
                .reduce(|a, b| qb_tray::Rect {
                    left: a.left.min(b.left),
                    top: a.top.min(b.top),
                    right: a.right.max(b.right),
                    bottom: a.bottom.max(b.bottom),
                })
        };
        let region = scene
            .content
            .iter()
            .rev()
            .find(|r| geometry::contains(r.viewport, p[0], p[1]))
            .map(|r| r.region);
        let time = now(&h);
        let frozen = (h.state.table_freeze.frozen, h.state.recent_freeze.frozen);
        if h.pointer_region != region
            && let Some(old) = h.pointer_region
        {
            h.state.pointer(old, false, time);
        }
        if let Some(region) = region {
            h.state.pointer_activity(region, time);
        }
        h.pointer_region = region;
        h.input.hover = hover;
        h.input.pointer = Some(p);
        released = (frozen.0 && !h.state.table_freeze.frozen)
            || (frozen.1 && !h.state.recent_freeze.frozen);
    }
    unsafe {
        let mut tracking = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tracking);
    }
    if released {
        redraw_for(hwnd, "pointer-release");
    } else if let Some(r) = dirty {
        let scale = unsafe { GetDpiForWindow(hwnd) }.max(48) as f32 / 96.;
        let r = RECT {
            left: ((r.left - 4.) * scale).floor() as i32,
            top: ((r.top - 4.) * scale).floor() as i32,
            right: ((r.right + 4.) * scale).ceil() as i32,
            bottom: ((r.bottom + 4.) * scale).ceil() as i32,
        };
        unsafe {
            let _ = InvalidateRect(Some(hwnd), Some(&r), false);
        }
    } else {
        repaint(hwnd);
    }
}
fn scroll(hwnd: HWND, p: [f32; 2], delta: f32) {
    let Some(host) = current(hwnd) else { return };
    {
        let mut h = host.borrow_mut();
        if !frame_valid(&h) {
            return;
        }
        let Host { state, frame, .. } = &mut *h;
        if !window_logic::wheel_scroll(state, &frame.as_ref().unwrap().scene, p, delta) {
            return;
        }
    }
    let mut damage = None;
    if let Some(host) = current(hwnd) {
        let mut h = host.borrow_mut();
        let frame = h.frame.take().unwrap();
        let retained =
            h.target.is_some() && h.target_metrics == Some((frame.stamp.pixels, frame.stamp.dpi));
        let previous = frame.scene;
        let region = previous
            .content
            .iter()
            .rev()
            .find(|r| geometry::contains(r.viewport, p[0], p[1]))
            .map(|r| r.region)
            .unwrap();
        if retained
            && region == ScrollRegion::Table
            && let Some(rect) = previous.table_scroll_damage()
            && scroll_decorations_contained(&h, &previous, &previous, rect)
        {
            damage = Some(rect);
        }
        invalidate_frame(&mut h);
        h.scroll_scene = Some((previous, region, damage));
        h.dirty_reason = "scroll";
    }
    if let Some(r) = damage {
        let scale = unsafe { GetDpiForWindow(hwnd) }.max(48) as f32 / 96.;
        let rect = RECT {
            left: (r.left * scale).floor() as i32,
            top: (r.top * scale).floor() as i32,
            right: (r.right * scale).ceil() as i32,
            bottom: (r.bottom * scale).ceil() as i32,
        };
        unsafe {
            let _ = InvalidateRect(Some(hwnd), Some(&rect), false);
        }
    } else {
        repaint(hwnd);
    }
}

/// Decorations outside the card may remain only when their geometry is identical.
/// Popup/editor/chart effects have wider bounds, so keep their full-paint path.
fn scroll_decorations_contained(h: &Host, old: &Scene, new: &Scene, damage: qb_tray::Rect) -> bool {
    if h.editing.is_some()
        || h.input.tooltip_ready
        || !old.charts.is_empty()
        || !new.charts.is_empty()
        || h.input.pointer.is_some_and(|p| {
            old.tooltip_at(p[0], p[1]).is_some() || new.tooltip_at(p[0], p[1]).is_some()
        })
    {
        return false;
    }
    for target in [&h.input.hover, &h.input.pressed, &h.input.focus]
        .into_iter()
        .flatten()
    {
        let boxes = |scene: &Scene| {
            scene
                .targets
                .iter()
                .filter(|hit| hit.enabled && &hit.target == target)
                .map(|hit| hit.rect)
                .collect::<Vec<_>>()
        };
        let before = boxes(old);
        let after = boxes(new);
        // A vanished focus is cleared during layout, which changes decoration.
        if h.input.focus.as_ref() == Some(target) && after.is_empty() {
            return false;
        }
        if before != after
            && before.iter().chain(&after).any(|r| {
                r.left - 4. < damage.left
                    || r.top - 4. < damage.top
                    || r.right + 4. > damage.right
                    || r.bottom + 4. > damage.bottom
            })
        {
            return false;
        }
    }
    true
}

fn held(vk: i32) -> bool {
    (unsafe { GetKeyState(vk) }) < 0
}
fn keyboard(hwnd: HWND, key: usize) -> bool {
    let Some(host) = current(hwnd) else {
        return false;
    };
    if !host.borrow().snapshot.connection.core_ok {
        return true;
    }
    if matches!(key, 0x09 | 0x25..=0x28) {
        host.borrow_mut().input.keyboard_focus = true;
    }
    let control = held(0x11);
    let shift = held(0x10);
    if key == 0x1b && window_logic::dismiss_popup(&mut host.borrow_mut().state) {
        redraw(hwnd);
        return true;
    }
    if host.borrow().state.popup.is_some() {
        let action = {
            let mut h = host.borrow_mut();
            if !frame_valid(&h) {
                return true;
            }
            let Host { frame, input, .. } = &mut *h;
            window_logic::popup_key(&frame.as_ref().unwrap().scene, &mut input.focus, key, shift)
        };
        if let Some(target) = action {
            activate(hwnd, target);
        } else {
            redraw(hwnd);
        }
        return true;
    }
    if control && key == 0x46 {
        let target = if host.borrow().state.tab == Tab::Events {
            Target::EventSearch
        } else {
            Target::Search
        };
        focus_edit(hwnd, target);
        return true;
    }
    if host.borrow().editing.is_some() {
        let mut h = host.borrow_mut();
        // The IME owns candidate navigation, editing and commit while composing.
        if !h.edit.composition.is_empty() && !control && key != 0x09 {
            return false;
        }
        match key {
            0x41 if control => h.edit.select_all(),
            0x56 if control => {
                if let Some(text) = clipboard_text() {
                    h.edit.insert(&text);
                }
            }
            0x08 => h.edit.erase(true),
            0x2e => h.edit.erase(false),
            0x25 => {
                let n = h.edit.cursor.saturating_sub(1);
                h.edit.move_to(n, shift);
            }
            0x27 => {
                let n = h.edit.cursor + 1;
                h.edit.move_to(n, shift);
            }
            0x24 => h.edit.move_to(0, shift),
            0x23 => {
                let n = h.edit.text.chars().count();
                h.edit.move_to(n, shift);
            }
            0x1b | 0x0d => {
                h.edit.composition.clear();
                sync_search(&mut h);
                h.editing = None;
            }
            0x09 => {
                h.edit.composition.clear();
                sync_search(&mut h);
                h.editing = None;
                drop(h);
                return keyboard(hwnd, key);
            }
            _ => return false,
        }
        sync_search(&mut h);
        drop(h);
        redraw(hwnd);
        return true;
    }
    if matches!(key, 0x26 | 0x28) {
        let navigation = {
            let mut h = host.borrow_mut();
            if !frame_valid(&h) {
                return false;
            }
            let Host {
                state,
                frame,
                input,
                ..
            } = &mut *h;
            window_logic::navigate_session(
                state,
                &frame.as_ref().unwrap().scene,
                input.focus.as_ref(),
                key == 0x26,
                true, // The snapshot connection guard above covers every keyboard path.
            )
            .map(|(focus, commands)| {
                input.focus = Some(focus);
                commands
            })
        };
        if let Some(commands) = navigation {
            redraw(hwnd);
            dispatch(hwnd, commands);
        }
        return true;
    }
    let action = {
        let mut h = host.borrow_mut();
        if !frame_valid(&h) {
            return false;
        }
        let scene = &h.frame.as_ref().unwrap().scene;
        let order: Vec<Target> = scene.focus_order().into_iter().cloned().collect();
        match key {
            0x09 => {
                if order.is_empty() {
                    return true;
                }
                let at = h
                    .input
                    .focus
                    .as_ref()
                    .and_then(|t| order.iter().position(|v| v == t));
                let n = match (at, shift) {
                    (Some(0), true) | (None, true) => order.len() - 1,
                    (Some(i), true) => i - 1,
                    (Some(i), false) => (i + 1) % order.len(),
                    (None, false) => 0,
                };
                h.input.focus = Some(order[n].clone());
                None
            }
            0x0d | 0x20 => h.input.focus.clone(),
            0x1b => Some(Target::CloseDetail),
            0x21 | 0x22 => {
                let region = scene
                    .content
                    .iter()
                    .find(|r| {
                        matches!(
                            r.region,
                            ScrollRegion::Table | ScrollRegion::Events | ScrollRegion::Usage
                        )
                    })
                    .cloned();
                if let Some(r) = region {
                    let sign = if key == 0x21 { -1. } else { 1. };
                    h.state.scroll(
                        r.region,
                        r.offset + sign * geometry::height(r.viewport) * 0.9,
                        r.max_offset,
                    );
                }
                None
            }
            _ => return false,
        }
    };
    if let Some(target) = action {
        if target == Target::CloseDetail {
            let commands = {
                let mut h = host.borrow_mut();
                h.input.focus = h.state.selected.clone().map(Target::Session);
                h.state.activate(&target)
            };
            redraw(hwnd);
            dispatch(hwnd, commands);
        } else {
            activate_target(hwnd, target, key == 0x0d);
        }
    } else {
        let edit = host
            .borrow()
            .input
            .focus
            .clone()
            .filter(|t| matches!(t, Target::Search | Target::EventSearch));
        if let Some(edit) = edit {
            focus_edit(hwnd, edit);
        } else {
            redraw(hwnd);
        }
    }
    true
}

// IMM functions use the system ABI already available in Windows. No feature or
// dependency edits. Custom-painted search receives committed UTF-16 directly.
#[link(name = "imm32")]
unsafe extern "system" {
    fn ImmGetContext(hwnd: *mut c_void) -> *mut c_void;
    fn ImmReleaseContext(hwnd: *mut c_void, context: *mut c_void) -> i32;
    fn ImmGetCompositionStringW(
        context: *mut c_void,
        index: u32,
        buffer: *mut c_void,
        length: u32,
    ) -> i32;
    fn ImmSetCompositionWindow(context: *mut c_void, form: *const CompositionForm) -> i32;
}
#[repr(C)]
struct CompositionForm {
    style: u32,
    point: POINT,
    area: RECT,
}
fn ime_string(context: *mut c_void, index: u32) -> Option<String> {
    let bytes = unsafe { ImmGetCompositionStringW(context, index, std::ptr::null_mut(), 0) };
    if !(0..=2048).contains(&bytes) || bytes % 2 != 0 {
        return None;
    }
    let mut units = vec![0u16; bytes as usize / 2];
    let read = unsafe {
        ImmGetCompositionStringW(context, index, units.as_mut_ptr().cast(), bytes as u32)
    };
    (read == bytes).then(|| String::from_utf16_lossy(&units))
}
fn ime(hwnd: HWND, flags: isize) {
    let Some(host) = current(hwnd) else { return };
    let accepts_text = {
        let h = host.borrow();
        window_logic::text_input_allowed(
            &h.state,
            h.editing.is_some(),
            h.snapshot.connection.core_ok,
        )
    };
    if !accepts_text {
        return;
    }
    let context = unsafe { ImmGetContext(hwnd.0) };
    if context.is_null() {
        return;
    }
    {
        let mut h = host.borrow_mut();
        if flags & 0x800 != 0 {
            if let Some(result) = ime_string(context, 0x800) {
                h.edit.insert(&result);
            }
        } else if flags & 8 != 0 {
            h.edit.composition = ime_string(context, 8).unwrap_or_default();
        }
        sync_search(&mut h);
    }
    unsafe {
        ImmReleaseContext(hwnd.0, context);
    }
    redraw(hwnd);
}
fn position_ime(hwnd: HWND) {
    let Some(host) = current(hwnd) else { return };
    let h = host.borrow();
    let Some(edit) = h.editing.as_ref() else {
        return;
    };
    let Some(frame) = h.frame.as_ref() else {
        return;
    };
    let Some(hit) = frame.scene.targets.iter().find(|t| &t.target == edit) else {
        return;
    };
    let scale = frame.stamp.dpi as f32 / 96.;
    let point = POINT {
        x: ((hit.rect.left + 8.) * scale) as i32,
        y: (hit.rect.bottom * scale) as i32,
    };
    unsafe {
        let context = ImmGetContext(hwnd.0);
        if !context.is_null() {
            ImmSetCompositionWindow(
                context,
                &CompositionForm {
                    style: 2,
                    point,
                    area: RECT::default(),
                },
            );
            ImmReleaseContext(hwnd.0, context);
        }
    }
}
#[link(name = "user32")]
unsafe extern "system" {
    fn OpenClipboard(hwnd: *mut c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn GetClipboardData(format: u32) -> *mut c_void;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalLock(memory: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(memory: *mut c_void) -> i32;
    fn GlobalSize(memory: *mut c_void) -> usize;
}
fn clipboard_text() -> Option<String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let memory = GetClipboardData(13);
        let count = if memory.is_null() {
            0
        } else {
            GlobalSize(memory) / 2
        };
        let mut result = None;
        if count > 0 && count <= 32768 {
            let ptr = GlobalLock(memory).cast::<u16>();
            if !ptr.is_null() {
                let units = std::slice::from_raw_parts(ptr, count);
                let end = units.iter().position(|u| *u == 0).unwrap_or(count);
                result = Some(String::from_utf16_lossy(&units[..end]));
                GlobalUnlock(memory);
            }
        }
        CloseClipboard();
        result
    }
}

fn teardown(hwnd: HWND) {
    let Some(host) = current(hwnd) else { return };
    unsafe {
        remember_geometry(hwnd);
    }
    flush_geometry(hwnd, true);
    let (bridge, token, root) = {
        let mut h = host.borrow_mut();
        if h.closing {
            return;
        }
        h.closing = true;
        h.port.close();
        h.life.close();
        h.frame = None;
        let root = h.root.take();
        h.renderer.clear_target_resources();
        h.target = None;
        let mut preferences = h.state.clone();
        preferences.search.clear();
        preferences.event_search.clear();
        preferences.select(None);
        preferences.table_freeze = Default::default();
        preferences.recent_freeze = Default::default();
        preferences.group_orders.clear();
        preferences.group_headers = Default::default();
        PREFERENCES.with(|s| *s.borrow_mut() = Some(preferences));
        (h.bridge.clone(), h.life.owner, root)
    };
    if let Some(root) = root {
        let _ = root.disconnect_before_destroy();
    }
    bridge.enqueue(Event::Closed { owner: token });
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if matches!(
        message,
        WM_MOUSEMOVE | WM_MOUSEWHEEL | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_KEYDOWN | WM_CHAR
    ) && let Some(host) = current(hwnd)
    {
        let mut h = host.borrow_mut();
        h.input_seq = h.input_seq.saturating_add(1);
    }
    match message {
        WM_ERASEBKGND => return LRESULT(1),
        WM_PRINT | WM_PRINTCLIENT => {
            let result = unsafe { print_client(hwnd, HDC(wparam.0 as *mut c_void)) };
            #[cfg(all(test, feature = "testkit"))]
            eprintln!("dashboard print message {message:#x}: {result:?}");
            if result.is_err() {
                return LRESULT(-1);
            }
            return LRESULT(0);
        }
        WM_PAINT => {
            let _ = unsafe { paint(hwnd) };
            position_ime(hwnd);
            return LRESULT(0);
        }
        dashboard_bridge::WAKE => {
            if current(hwnd).is_some_and(|h| h.borrow().port.owner() == wparam.0 as u64) {
                apply_mail(hwnd);
            }
            return LRESULT(0);
        }
        WM_TIMER if wparam.0 == TIMER => {
            apply_mail(hwnd);
            let mut tick = false;
            let mut caret = false;
            if let Some(host) = current(hwnd) {
                let mut h = host.borrow_mut();
                let time = now(&h);
                if time / 500 != h.last_tick {
                    h.last_tick = time / 500;
                    let frozen = (h.state.table_freeze.frozen, h.state.recent_freeze.frozen);
                    h.state.release_idle(time);
                    tick |= frozen != (h.state.table_freeze.frozen, h.state.recent_freeze.frozen);
                    caret = h.focused && h.editing.is_some();
                }
                let clock = virtual_clock(&h);
                let reconcile = clock.utc() < clock.day_start || clock.utc() >= clock.day_end;
                if !reconcile {
                    tick |= h.next_clock.is_some_and(|at| clock.utc() >= at);
                }
                if (reconcile && !h.clock_reconcile)
                    || time.saturating_sub(h.last_clock_request) >= 15000
                {
                    h.clock_reconcile = reconcile;
                    h.last_clock_request = time;
                    h.bridge.enqueue(Event::ClockNeeded {
                        owner: h.life.owner,
                    });
                }
            }
            flush_geometry(hwnd, false);
            if tick {
                redraw_for(hwnd, "timer");
            } else if caret {
                repaint(hwnd);
            }
            return LRESULT(0);
        }
        WM_SIZE | WM_MOVE => {
            unsafe {
                remember_geometry(hwnd);
            }
            redraw(hwnd);
            return LRESULT(0);
        }
        WM_GETMINMAXINFO => {
            if lparam.0 != 0 {
                let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
                let m = unsafe { monitor(hwnd) };
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as i64;
                let width = (m.rcWork.right - m.rcWork.left).max(100);
                let height = (m.rcWork.bottom - m.rcWork.top).max(100);
                info.ptMinTrackSize = POINT {
                    x: ((1024 * dpi / 96) as i32).min(width),
                    y: ((640 * dpi / 96) as i32).min(height),
                };
                info.ptMaxTrackSize = POINT {
                    x: width,
                    y: height,
                };
                info.ptMaxPosition = POINT {
                    x: m.rcWork.left - m.rcMonitor.left,
                    y: m.rcWork.top - m.rcMonitor.top,
                };
                info.ptMaxSize = POINT {
                    x: width,
                    y: height,
                };
            }
            return LRESULT(0);
        }
        WM_MOVING | WM_SIZING => {
            if lparam.0 != 0 {
                let r = unsafe { &mut *(lparam.0 as *mut RECT) };
                let mon = unsafe { MonitorFromRect(r, MONITOR_DEFAULTTONEAREST) };
                let mut m = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if unsafe { GetMonitorInfoW(mon, &mut m) }.as_bool() {
                    let fit = window_logic::fit_rect(
                        edges(*r),
                        edges(m.rcWork),
                        unsafe { GetDpiForWindow(hwnd) }.max(48),
                    );
                    *r = RECT {
                        left: fit[0],
                        top: fit[1],
                        right: fit[2],
                        bottom: fit[3],
                    };
                }
            }
            return LRESULT(1);
        }
        WM_DISPLAYCHANGE => {
            let mut r = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok()
                && !unsafe { IsZoomed(hwnd) }.as_bool()
            {
                let m = unsafe { monitor(hwnd) };
                let fit = window_logic::fit_rect(
                    edges(r),
                    edges(m.rcWork),
                    unsafe { GetDpiForWindow(hwnd) }.max(48),
                );
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        fit[0],
                        fit[1],
                        fit[2] - fit[0],
                        fit[3] - fit[1],
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                }
            }
            redraw(hwnd);
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            let (dark, badge) = ICON_STATE.with(|s| *s.borrow());
            set_icon_presentation(dark, badge);
            redraw(hwnd);
            if lparam.0 != 0 {
                let r = unsafe { *(lparam.0 as *const RECT) };
                let m = unsafe { monitor(hwnd) };
                let dpi = (wparam.0 & 0xffff) as u32;
                let g = Geometry {
                    left: r.left,
                    top: r.top,
                    width: (r.right - r.left).max(0) as u32,
                    height: (r.bottom - r.top).max(0) as u32,
                    dpi,
                    maximized: false,
                };
                if let Some(r) = g.restore(edges(m.rcWork), dpi) {
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            r[0],
                            r[1],
                            r[2] - r[0],
                            r[3] - r[1],
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
            }
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            mouse_move(hwnd, mouse_point(hwnd, lparam, false));
            return LRESULT(0);
        }
        WM_MOUSELEAVE_MESSAGE => {
            let mut released = false;
            if let Some(host) = current(hwnd) {
                let mut h = host.borrow_mut();
                let time = now(&h);
                let frozen = (h.state.table_freeze.frozen, h.state.recent_freeze.frozen);
                if let Some(region) = h.pointer_region.take() {
                    h.state.pointer(region, false, time);
                }
                h.input.hover = None;
                h.input.pointer = None;
                released = (frozen.0 && !h.state.table_freeze.frozen)
                    || (frozen.1 && !h.state.recent_freeze.frozen);
            }
            if released {
                redraw_for(hwnd, "pointer-release");
            } else {
                repaint(hwnd);
            }
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            if let Some(host) = current(hwnd) {
                host.borrow_mut().input.keyboard_focus = false;
            }
            let target = current(hwnd).and_then(|h| {
                let mut h = h.borrow_mut();
                if !frame_valid(&h) {
                    return None;
                }
                let p = mouse_point(hwnd, lparam, false);
                let target = h.frame.as_ref()?.scene.hit_test(p[0], p[1]).cloned();
                h.input.pressed = target.clone();
                target
            });
            if target.is_some() {
                unsafe {
                    SetCapture(hwnd);
                    let _ = SetFocus(Some(hwnd));
                }
                repaint(hwnd);
            }
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let action = current(hwnd).and_then(|h| {
                let mut h = h.borrow_mut();
                let pressed = h.input.pressed.take()?;
                if !frame_valid(&h) {
                    return None;
                }
                let p = mouse_point(hwnd, lparam, false);
                (h.frame.as_ref()?.scene.hit_test(p[0], p[1]) == Some(&pressed)).then_some(pressed)
            });
            unsafe {
                let _ = ReleaseCapture();
            }
            if let Some(target) = action {
                activate(hwnd, target);
            } else {
                redraw(hwnd);
            }
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            if let Some(h) = current(hwnd) {
                h.borrow_mut().input.pressed = None;
            }
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as i16 as f32;
            scroll(hwnd, mouse_point(hwnd, lparam, true), -delta / 120. * 96.);
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            if keyboard(hwnd, wparam.0) {
                return LRESULT(0);
            }
        }
        WM_CHAR => {
            if let Some(host) = current(hwnd) {
                let mut h = host.borrow_mut();
                if h.state.popup.is_some() {
                    return LRESULT(0);
                }
                if window_logic::text_input_allowed(
                    &h.state,
                    h.editing.is_some(),
                    h.snapshot.connection.core_ok,
                ) && wparam.0 >= 32
                    && !held(0x11)
                {
                    h.edit.utf16(wparam.0 as u16);
                    sync_search(&mut h);
                    drop(h);
                    redraw(hwnd);
                    return LRESULT(0);
                }
            }
        }
        WM_IME_COMPOSITION => {
            if current(hwnd).is_some_and(|h| h.borrow().editing.is_some()) {
                ime(hwnd, lparam.0);
                return LRESULT(0);
            }
        }
        WM_IME_ENDCOMPOSITION => {
            if let Some(host) = current(hwnd) {
                let mut h = host.borrow_mut();
                h.edit.composition.clear();
                if h.state.popup.is_none() {
                    sync_search(&mut h);
                }
            }
            redraw(hwnd);
        }
        WM_SETFOCUS => {
            if let Some(h) = current(hwnd) {
                h.borrow_mut().focused = true;
            }
            repaint(hwnd);
        }
        WM_KILLFOCUS => {
            let changed = finish_edit(hwnd);
            if let Some(h) = current(hwnd) {
                h.borrow_mut().focused = false;
                h.borrow_mut().input.pressed = None;
            }
            if changed {
                redraw(hwnd);
            } else {
                repaint(hwnd);
            }
        }
        WM_GETOBJECT => {
            // An occluded window may not receive WM_PAINT before an automation
            // client reads it. Complete the pending frame on the owning UI thread
            // before returning the provider; never dispatch against stale geometry.
            // No Host borrow spans synchronous WndProc re-entry.
            if lparam.0 as i32 == windows::Win32::UI::Accessibility::UiaRootObjectId
                && current(hwnd).is_some_and(|h| {
                    let h = h.borrow();
                    !h.closing && !frame_valid(&h)
                })
            {
                repaint(hwnd);
                complete_automation_frame(hwnd);
            }
            #[cfg(all(test, feature = "testkit"))]
            eprintln!("dashboard WM_GETOBJECT {:#x}", lparam.0);
            if let Some(host) = current(hwnd) {
                let h = host.borrow();
                // Keep the provider identity stable while a new frame is pending.
                // Snapshot publication and action dispatch retain their epoch guards.
                if !h.closing
                    && let Some(root) = h.root.as_ref()
                    && let Some(result) = root.handle_wm_getobject(wparam, lparam)
                {
                    #[cfg(all(test, feature = "testkit"))]
                    eprintln!("dashboard provider return {:#x}", result.0);
                    return result;
                }
            }
        }
        accessibility::WM_UIA_VALUE => {
            let changed = current(hwnd).is_some_and(|host| {
                let mut h = host.borrow_mut();
                if !frame_valid(&h) || !h.snapshot.connection.core_ok {
                    return false;
                }
                let frame = h.frame.as_ref().unwrap();
                if lparam.0 <= 0 || frame.stamp.epoch != lparam.0 as usize {
                    return false;
                }
                let Some(root) = h.root.as_ref() else {
                    return false;
                };
                let Ok(Some((id, epoch, value))) = root.take_value_request(wparam, lparam) else {
                    return false;
                };
                if epoch != frame.stamp.epoch {
                    return false;
                }
                let Some(target) = frame
                    .elements
                    .iter()
                    .find(|(element, _)| *element == id)
                    .map(|(_, t)| t.clone())
                else {
                    return false;
                };
                if !frame
                    .scene
                    .targets
                    .iter()
                    .any(|hit| hit.enabled && hit.target == target)
                {
                    return false;
                }
                let connected = h.snapshot.connection.core_ok;
                if !window_logic::set_search_value(&mut h.state, &target, value.clone(), connected)
                {
                    return false;
                }
                if h.editing.as_ref() == Some(&target) {
                    h.edit.reset(&value);
                }
                true
            });
            if changed {
                redraw(hwnd);
                // Automation may operate on an occluded window. Publish the
                // completed result before its next property read, even when the
                // compositor would defer an ordinary WM_PAINT.
                complete_automation_frame(hwnd);
            }
            return LRESULT(0);
        }
        accessibility::WM_UIA_INVOKE | accessibility::WM_UIA_FOCUS => {
            let target = current(hwnd).and_then(|h| {
                let h = h.borrow();
                if !frame_valid(&h) {
                    return None;
                }
                let frame = h.frame.as_ref()?;
                if lparam.0 <= 0 || frame.stamp.epoch != lparam.0 as usize {
                    return None;
                }
                if message == accessibility::WM_UIA_INVOKE {
                    frame.actions.get(wparam.0).cloned()
                } else {
                    frame
                        .elements
                        .iter()
                        .find(|(id, _)| *id as usize == wparam.0)
                        .map(|(_, t)| t.clone())
                }
            });
            if let Some(target) = target {
                if message == accessibility::WM_UIA_INVOKE {
                    activate(hwnd, target);
                    complete_automation_frame(hwnd);
                } else if matches!(target, Target::Search | Target::EventSearch) {
                    focus_edit(hwnd, target);
                } else if let Some(h) = current(hwnd) {
                    h.borrow_mut().input.focus = Some(target);
                    unsafe {
                        let _ = SetFocus(Some(hwnd));
                    }
                    // Keep a same-frame UIA invoke valid across its focus step.
                    repaint(hwnd);
                }
            }
            return LRESULT(0);
        }
        WM_CLOSE => {
            teardown(hwnd);
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        WM_NCDESTROY => {
            teardown(hwnd);
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER);
            }
            HOST.with(|s| {
                let mut s = s.borrow_mut();
                if s.as_ref().is_some_and(|h| h.borrow().hwnd == hwnd) {
                    s.take();
                }
            });
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod r1210_uia_hit_index_tests {
    use super::*;
    use qb_tray::{
        Rect,
        geometry::{AccessibleNode, HitTarget, Role},
        state::{EventFilter, ProviderFilter, Range, SortKey},
    };

    fn hit(target: Target, enabled: bool) -> HitTarget {
        HitTarget {
            // The old predicate checks enabled and target, not hit geometry.
            rect: Rect {
                left: 0.,
                top: 0.,
                right: 0.,
                bottom: 0.,
            },
            target,
            enabled,
            tooltip: None,
        }
    }

    fn node(target: Option<Target>) -> AccessibleNode {
        AccessibleNode {
            key: "test".into(),
            parent: None,
            role: Role::Button,
            name: "test".into(),
            rect: Rect {
                left: 2.,
                top: 3.,
                right: 12.,
                bottom: 13.,
            },
            target,
            enabled: true,
            selected: false,
            expanded: None,
            value: None,
            hidden: false,
        }
    }

    fn old_interactive(node: &AccessibleNode, hits: &[HitTarget]) -> bool {
        !node.hidden
            && node.enabled
            && geometry::width(node.rect) > 0.
            && geometry::height(node.rect) > 0.
            && node
                .target
                .as_ref()
                .is_some_and(|t| hits.iter().any(|hit| hit.enabled && &hit.target == t))
    }

    #[test]
    fn r1210_uia_hit_index_matches_legacy_predicate() {
        let mut targets = vec![
            Target::DismissPopup,
            Target::RangeMenu,
            Target::ToggleGrouping,
            Target::Search,
            Target::EventSearch,
            Target::CloseDetail,
            Target::DetailSurface,
            Target::EventDaysMenu,
            Target::AllEvents,
            Target::OpenTrayAdd,
            Target::Today,
        ];
        targets.extend([Tab::Overview, Tab::Events, Tab::Usage].map(Target::Tab));
        targets.extend(
            [
                ProviderFilter::All,
                ProviderFilter::Claude,
                ProviderFilter::Codex,
            ]
            .map(Target::Provider),
        );
        targets.extend(Range::ALL.map(Target::Range));
        targets.extend(
            [
                SortKey::Status,
                SortKey::Session,
                SortKey::Client,
                SortKey::Account,
                SortKey::Model,
                SortKey::Start,
                SortKey::LastActivity,
                SortKey::Requests,
                SortKey::Memo,
            ]
            .map(Target::Sort),
        );
        targets.extend(EventFilter::ALL.map(Target::EventFilter));
        targets.extend([0, 1, 255].map(Target::EventDays));
        for value in ["", "same", "계정🦀"] {
            targets.extend([
                Target::Group(value.into()),
                Target::Session(value.into()),
                Target::SessionLink(value.into()),
                Target::AccountLink(value.into()),
                Target::OpenTrayAccount(value.into()),
                Target::Chart(value.into()),
            ]);
        }
        let mixed: Vec<_> = targets
            .iter()
            .enumerate()
            .flat_map(|(i, t)| {
                [
                    hit(t.clone(), false),
                    hit(t.clone(), i % 3 == 0),
                    hit(t.clone(), i % 5 == 0),
                ]
            })
            .collect();
        let reversed = mixed.iter().rev().cloned().collect();
        let cases = [
            Vec::new(),
            targets.iter().cloned().map(|t| hit(t, false)).collect(),
            mixed,
            reversed,
        ];
        let queries: Vec<_> = targets
            .into_iter()
            .map(Some)
            .chain([None, Some(Target::Chart("missing".into()))])
            .collect();
        for hits in &cases {
            let enabled_hits = hits
                .iter()
                .filter(|hit| hit.enabled)
                .map(|hit| &hit.target)
                .collect();
            for target in &queries {
                for hidden in [false, true] {
                    for enabled in [false, true] {
                        for (width, height) in
                            [(10., 10.), (0., 10.), (10., 0.), (-1., 10.), (10., -1.)]
                        {
                            let mut node = node(target.clone());
                            node.hidden = hidden;
                            node.enabled = enabled;
                            node.rect.right = node.rect.left + width;
                            node.rect.bottom = node.rect.top + height;
                            assert_eq!(
                                accessibility_interactive(&node, &enabled_hits),
                                old_interactive(&node, hits),
                                "target={target:?}, hidden={hidden}, enabled={enabled}, size={width}x{height}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn r1210_uia_hit_index_keeps_duplicate_disabled_and_missing_semantics() {
        for hits in [
            vec![
                hit(Target::Search, false),
                hit(Target::Group("same".into()), false),
                hit(Target::Group("same".into()), true),
                hit(Target::Session("same".into()), false),
            ],
            vec![
                hit(Target::Session("same".into()), false),
                hit(Target::Group("same".into()), true),
                hit(Target::Group("same".into()), false),
                hit(Target::Search, false),
            ],
        ] {
            let enabled_hits: BTreeSet<_> = hits
                .iter()
                .filter(|hit| hit.enabled)
                .map(|hit| &hit.target)
                .collect();
            assert_eq!(enabled_hits.len(), 1);
            assert!(accessibility_interactive(
                &node(Some(Target::Group("same".into()))),
                &enabled_hits
            ));
            for target in [
                None,
                Some(Target::Search),
                Some(Target::Session("same".into())),
                Some(Target::Chart("missing".into())),
            ] {
                assert!(!accessibility_interactive(&node(target), &enabled_hits));
            }
            // Text still passes this membership predicate; the unchanged
            // publication code separately excludes its actions and focus.
            let mut text = node(Some(Target::Group("same".into())));
            text.role = Role::Text;
            assert!(accessibility_interactive(&text, &enabled_hits));
        }
    }
}
