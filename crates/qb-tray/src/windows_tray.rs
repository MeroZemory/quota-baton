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
                DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
                DwmSetWindowAttribute,
            },
            Gdi::InvalidateRect,
            Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect},
        },
        System::LibraryLoader::GetModuleHandleW,
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
                IDI_APPLICATION, IsWindowVisible, LoadCursorW, LoadIconW, MSG, PostMessageW,
                PostQuitMessage, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SetForegroundWindow,
                SetTimer, SetWindowPos, ShowWindow, TranslateMessage, WM_APP, WM_DESTROY,
                WM_DPICHANGED, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_PAINT, WM_RBUTTONUP,
                WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
    core::{PCWSTR, w},
};

const TRAY_MESSAGE: u32 = WM_APP + 1;
const ACTION_DONE: u32 = WM_APP + 2;
const WINDOW_CLASS: PCWSTR = w!("QuotaBatonTrayFlyout");
static MODEL: OnceLock<Mutex<(Vec<qb_tray::Row>, String, bool)>> = OnceLock::new();
static VIEW_STATE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SUPERVISOR: OnceLock<Mutex<Supervisor>> = OnceLock::new();
static SUPERVISE_ENABLED: AtomicBool = AtomicBool::new(true);
static FIXTURE_MODE: AtomicBool = AtomicBool::new(false);

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
                            Some(format!("menu:{}", row.id));
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
        WM_RBUTTONUP if IsWindowVisible(hwnd).as_bool() => {
            if let Some(row) = model().lock().unwrap_or_else(|e| e.into_inner()).0.first() {
                *view_state().lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(format!("menu:{}", row.id));
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
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
