use anyhow::Result;
use std::os::windows::process::CommandExt;
use std::{
    ffi::c_void,
    mem::size_of,
    process::Command,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
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
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
                DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, HMENU, IDC_ARROW,
                IDI_APPLICATION, IsWindowVisible, LoadCursorW, LoadIconW, MSG, PostQuitMessage,
                RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SetForegroundWindow, SetTimer,
                SetWindowPos, ShowWindow, TranslateMessage, WM_APP, WM_DESTROY, WM_DPICHANGED,
                WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_PAINT, WM_RBUTTONUP, WM_SETTINGCHANGE,
                WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
    core::{PCWSTR, w},
};

const TRAY_MESSAGE: u32 = WM_APP + 1;
const WINDOW_CLASS: PCWSTR = w!("QuotaBatonTrayFlyout");
static MODEL: OnceLock<Mutex<(Vec<qb_tray::Row>, String)>> = OnceLock::new();
static SUPERVISOR: OnceLock<Mutex<Supervisor>> = OnceLock::new();
static SUPERVISE_ENABLED: AtomicBool = AtomicBool::new(true);

struct Supervisor {
    last_start: Option<Instant>,
    delay: Duration,
}

fn supervisor() -> &'static Mutex<Supervisor> {
    SUPERVISOR.get_or_init(|| {
        Mutex::new(Supervisor {
            last_start: None,
            delay: Duration::from_secs(5),
        })
    })
}

fn model() -> &'static Mutex<(Vec<qb_tray::Row>, String)> {
    MODEL.get_or_init(|| Mutex::new((Vec::new(), "v0.1.0".to_owned())))
}

fn update_status(supervise: bool) {
    let result = (|| -> Result<String> {
        let client = super::client::AdminClient::from_home()?;
        let runtime = tokio::runtime::Runtime::new()?;
        let read = runtime
            .block_on(client.read())
            .map_err(|e| anyhow::anyhow!(e))?;
        let rows = super::client::layout_rows(&read, std::time::SystemTime::now())?;
        *model().lock().unwrap_or_else(|e| e.into_inner()) = (rows, read.snapshot.version.clone());
        Ok(super::client::render_flyout(&read))
    })();
    let text = match result {
        Ok(text) => {
            let mut state = supervisor().lock().unwrap_or_else(|e| e.into_inner());
            state.last_start = None;
            state.delay = Duration::from_secs(5);
            text
        }
        Err(_) => {
            if supervise {
                let mut state = supervisor().lock().unwrap_or_else(|e| e.into_inner());
                let due = state
                    .last_start
                    .is_none_or(|at| at.elapsed() >= state.delay);
                if due {
                    // Detached and hidden; an existing core is never terminated.
                    let started = Command::new("quota-baton")
                        .args(["serve"])
                        .creation_flags(0x0800_0000)
                        .spawn()
                        .is_ok();
                    if started {
                        state.last_start = Some(Instant::now());
                        state.delay = (state.delay * 2).min(Duration::from_secs(300));
                    }
                }
            }
            "코어에 연결할 수 없습니다".to_owned()
        }
    };
    if text == "코어에 연결할 수 없습니다" {
        model().lock().unwrap_or_else(|e| e.into_inner()).0.clear();
    }
}

fn paint_window(hwnd: HWND) -> Result<()> {
    let (rows, version) = model().lock().unwrap_or_else(|e| e.into_inner()).clone();
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
                height: 700,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let target = factory.CreateHwndRenderTarget(&props, &hwnd_props)?;
        qb_tray::native_render::paint(
            &target,
            &rows,
            false,
            &version,
            std::time::SystemTime::now(),
            false,
        )?;
    }
    Ok(())
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
                let _ = ShowWindow(hwnd, SW_HIDE);
                return LRESULT(0);
            }
            update_status(SUPERVISE_ENABLED.load(Ordering::Relaxed));
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            let _ = SetWindowPos(
                hwnd,
                None,
                point.x - 360,
                point.y - 700,
                360,
                700,
                Default::default(),
            );
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetForegroundWindow(hwnd);
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u32 == 0x1b => {
            let _ = ShowWindow(hwnd, SW_HIDE);
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
            update_status(SUPERVISE_ENABLED.load(Ordering::Relaxed));
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

pub fn run(supervise: bool, open_flyout: bool) -> Result<()> {
    SUPERVISE_ENABLED.store(supervise, Ordering::Relaxed);
    update_status(supervise);
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
            700,
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
