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
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        Graphics::{
            Dwm::{
                DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
                DwmSetWindowAttribute,
            },
            Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT, TextOutW},
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
                RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOSIZE, SetForegroundWindow,
                SetTimer, SetWindowPos, ShowWindow, TranslateMessage, WM_APP, WM_DESTROY,
                WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_PAINT, WM_RBUTTONUP, WM_SETTINGCHANGE,
                WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
    core::{PCWSTR, w},
};

const TRAY_MESSAGE: u32 = WM_APP + 1;
const WINDOW_CLASS: PCWSTR = w!("QuotaBatonTrayFlyout");
static STATUS: OnceLock<Mutex<Vec<u16>>> = OnceLock::new();
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

fn status_text() -> &'static Mutex<Vec<u16>> {
    STATUS.get_or_init(|| {
        Mutex::new(
            "코어 상태를 확인하는 중..."
                .encode_utf16()
                .chain([0])
                .collect(),
        )
    })
}

fn update_status(supervise: bool) {
    let result = (|| -> Result<String> {
        let client = super::client::AdminClient::from_home()?;
        let runtime = tokio::runtime::Runtime::new()?;
        let read = runtime
            .block_on(client.read())
            .map_err(|e| anyhow::anyhow!(e))?;
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
    let mut value = Vec::new();
    for line in text.lines() {
        value.extend(line.encode_utf16());
        value.push(0);
    }
    value.truncate(2048);
    if value.last() != Some(&0) {
        value.push(0);
    }
    *status_text().lock().unwrap_or_else(|e| e.into_inner()) = value;
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
                point.y - 430,
                360,
                430,
                SWP_NOSIZE,
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
        WM_TIMER => {
            update_status(SUPERVISE_ENABLED.load(Ordering::Relaxed));
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let text = status_text()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for (row, line) in text.split(|value| *value == 0).enumerate() {
                if !line.is_empty() {
                    let _ = TextOutW(dc, 20, 20 + row as i32 * 24, line);
                }
            }
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

pub fn run(supervise: bool) -> Result<()> {
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
            430,
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
