//! Manual synthetic HWND gate. No runtime, profiles, persistence, or network.
#![cfg(all(windows, feature = "testkit"))]

#[path = "../src/accessibility.rs"]
mod accessibility;
#[allow(dead_code)]
#[path = "../src/dashboard_persistence.rs"]
mod dashboard_persistence;
#[allow(dead_code)]
#[path = "../src/dashboard_window.rs"]
mod dashboard_window;
#[path = "support/r13_icon_probe.rs"]
mod probe;

use dashboard_window::{Bridge, Event, UiPort};
use qb_tray::{
    dashboard_model as model,
    tray_icon::pixels::{self, Badge},
    v2,
};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT, WPARAM},
    System::{
        Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
        Threading::GetCurrentThreadId,
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::*,
    },
};

struct FakeBridge(SyncSender<Event>);
impl Bridge for FakeBridge {
    fn enqueue(&self, event: Event) {
        self.0
            .try_send(event)
            .expect("synthetic bridge overflow/disconnect");
    }
}

struct Cleanup(DPI_AWARENESS_CONTEXT);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = dashboard_window::close_now_on_ui_thread();
        unsafe {
            CoUninitialize();
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

fn clock() -> model::Clock {
    model::Clock {
        now: chrono::DateTime::parse_from_rfc3339("2026-10-22T12:00:00+09:00").unwrap(),
        day_start: chrono::DateTime::parse_from_rfc3339("2026-10-22T00:00:00+09:00")
            .unwrap()
            .to_utc(),
        day_end: chrono::DateTime::parse_from_rfc3339("2026-10-23T00:00:00+09:00")
            .unwrap()
            .to_utc(),
    }
}

struct Run {
    send: SyncSender<Event>,
    recv: Receiver<Event>,
    port: Option<UiPort>,
    deadline: Instant,
}
impl Run {
    fn pump(&mut self) {
        let until = Instant::now() + Duration::from_millis(100);
        loop {
            assert!(Instant::now() < self.deadline, "synthetic HWND deadline");
            while let Ok(event) = self.recv.try_recv() {
                match event {
                    Event::Opened { port } => self.port = Some(port),
                    Event::ClockNeeded { .. } => {
                        if let Some(port) = &self.port {
                            assert!(port.clock(clock()));
                        }
                    }
                    // No command consumer or persistence backend is installed.
                    Event::Command { .. } | Event::GeometryDirty { .. } | Event::Closed { .. } => {}
                }
            }
            unsafe {
                let mut msg = MSG::default();
                for _ in 0..128 {
                    if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        break;
                    }
                    assert_ne!(msg.message, WM_QUIT, "unexpected UI thread shutdown");
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if Instant::now() >= until {
                break;
            }
            thread::sleep(Duration::from_millis(4));
        }
    }

    fn open(&mut self, dark: bool, badge: Badge) -> HWND {
        dashboard_window::set_icon_presentation(dark, badge);
        let view = v2::project(
            r#"{"version":"R13-synthetic","accounts":[]}"#,
            clock().utc().into(),
        )
        .unwrap();
        dashboard_window::open_or_focus(
            HWND::default(),
            Arc::new(FakeBridge(self.send.clone())),
            Arc::new(model::DashboardSnapshot::new(view)),
            clock(),
            dark,
        )
        .unwrap();
        self.pump();
        assert!(self.port.is_some());
        owned_dashboard()
    }

    fn close(&mut self, hwnd: HWND) {
        dashboard_window::close_now_on_ui_thread().unwrap();
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        self.port = None;
        self.pump();
    }
}

fn owned_dashboard() -> HWND {
    unsafe extern "system" fn visit(hwnd: HWND, context: LPARAM) -> windows::core::BOOL {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == std::process::id() {
            let mut class = [0_u16; 64];
            let count = unsafe { GetClassNameW(hwnd, &mut class) };
            if count > 0
                && String::from_utf16_lossy(&class[..count as usize]) == "QuotaBatonDashboard"
            {
                unsafe { (*(context.0 as *mut Vec<HWND>)).push(hwnd) };
            }
        }
        true.into()
    }
    let mut found = Vec::<HWND>::new();
    unsafe {
        EnumThreadWindows(
            GetCurrentThreadId(),
            Some(visit),
            LPARAM(std::ptr::from_mut(&mut found) as isize),
        )
        .unwrap();
    }
    assert_eq!(found.len(), 1, "one dashboard on this test's own UI thread");
    found[0]
}

fn class_icons(hwnd: HWND) -> [usize; 2] {
    unsafe {
        [
            GetClassLongPtrW(hwnd, GCLP_HICONSM),
            GetClassLongPtrW(hwnd, GCLP_HICON),
        ]
    }
}

fn window_icons(hwnd: HWND) -> [usize; 2] {
    [ICON_SMALL, ICON_BIG].map(|kind| unsafe {
        SendMessageW(hwnd, WM_GETICON, Some(WPARAM(kind as usize)), None).0 as usize
    })
}

fn assert_icon(handle: usize, size: u32, dark: bool, badge: Badge) {
    let (actual_size, actual) = probe::icon(handle as *mut _).expect("live borrowed HICON");
    assert_eq!(actual_size, size);
    let expected = pixels::render(size, dark, badge).unwrap();
    assert_eq!(
        actual, expected.bgra,
        "all premultiplied BGRA pixels: {size}px dark={dark} {badge:?}"
    );
}

fn assert_window_icons(hwnd: HWND, dark: bool, badge: Badge) -> [usize; 2] {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    assert!(dpi > 0);
    let small = match dpi {
        0..=108 => 16,
        109..=132 => 20,
        133..=168 => 24,
        _ => 32,
    };
    let handles = window_icons(hwnd);
    assert_icon(handles[0], small, dark, badge);
    assert_icon(handles[1], 32, dark, badge);
    handles
}

fn assert_retired(handles: [usize; 2]) {
    for handle in handles {
        assert!(
            probe::icon(handle as *mut _).is_err(),
            "a retired dynamic HICON must no longer be live"
        );
    }
}

fn evidence_directory() -> Option<PathBuf> {
    std::env::var_os("R13_ICON_OUT").map(|value| {
        let requested = PathBuf::from(value);
        assert!(
            requested.is_absolute() && requested.is_dir(),
            "R13_ICON_OUT must be an existing directory"
        );
        let output = requested.canonicalize().unwrap();
        let sandbox = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox")
            .canonicalize()
            .unwrap();
        assert!(
            output.starts_with(&sandbox),
            "R13_ICON_OUT must resolve inside this project's .sandbox"
        );
        output
    })
}

#[test]
#[ignore = "manual Windows testkit gate: synthetic production HWND and borrowed HICON decoding"]
fn r13_dashboard_icons_match_pixels_reuse_handles_and_survive_lifecycle() {
    let output = evidence_directory();
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    assert!(!previous.is_invalid());
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
    let (send, recv) = mpsc::sync_channel(1024);
    let mut run = Run {
        send,
        recv,
        port: None,
        deadline: Instant::now() + Duration::from_secs(45),
    };
    // Drop before Run, so teardown can still enqueue into the live FakeBridge.
    let _cleanup = Cleanup(previous);
    let hwnd = run.open(false, Badge::Normal);
    if std::env::var_os("R13_SHELL_REVIEW").is_some() {
        let output = output
            .as_ref()
            .expect("R13_SHELL_REVIEW requires R13_ICON_OUT");
        let ready = json!({"pid": std::process::id(), "hwnd": hwnd.0 as usize});
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join("shell-ready.json"))
            .unwrap()
            .write_all(&serde_json::to_vec(&ready).unwrap())
            .unwrap();
        let deadline = (Instant::now() + Duration::from_secs(30)).min(run.deadline);
        let done = output.join("shell-done.json");
        while !done.exists() {
            assert!(Instant::now() < deadline, "shell review signal timed out");
            run.pump();
        }
    }
    let class = class_icons(hwnd);
    assert_icon(class[0], 16, false, Badge::Normal);
    assert_icon(class[1], 32, false, Badge::Normal);
    assert_window_icons(hwnd, false, Badge::Normal);
    let mut cases = Vec::<Value>::new();

    // Warm both renderers and the production UIA/cache paths before leak samples.
    for round in 0..4 {
        let before = probe::resources();
        for dark in [false, true] {
            assert!(run.port.as_ref().unwrap().theme(dark));
            run.pump();
            for badge in [Badge::Normal, Badge::Attention, Badge::Error] {
                let previous = window_icons(hwnd);
                dashboard_window::set_icon_presentation(dark, badge);
                let handles = assert_window_icons(hwnd, dark, badge);
                if previous != handles {
                    assert_retired(previous);
                }
                for _ in 0..16 {
                    dashboard_window::set_icon_presentation(dark, badge);
                }
                assert_eq!(
                    window_icons(hwnd),
                    handles,
                    "unchanged presentation must reuse both HICONs"
                );
                assert_eq!(
                    class_icons(hwnd),
                    class,
                    "dynamic state never replaces class defaults"
                );
                assert_icon(class[0], 16, false, Badge::Normal);
                assert_icon(class[1], 32, false, Badge::Normal);
                if round == 0 {
                    let capture = output.as_ref().map(|out| {
                        let name = format!("r13-dashboard-{}-{badge:?}.bmp", if dark { "dark" } else { "light" });
                        let dimensions = probe::capture(hwnd, &out.join(&name)).unwrap();
                        json!({"file": name, "dimensions": dimensions, "scope": "owned-full-window-PrintWindow"})
                    });
                    cases.push(json!({"dark": dark, "badge": format!("{badge:?}"),
                        "actual_dpi": unsafe { GetDpiForWindow(hwnd) }, "handles": handles,
                        "pixel_match": true, "unchanged_handles_reused": true, "capture": capture}));
                }
            }
        }
        let after = probe::resources();
        if round >= 2 {
            assert_eq!(
                after, before,
                "resource growth across repeated presentation updates"
            );
        }
        cases.push(json!({"phase":"updates", "round":round, "resources_before":before,"resources_after":after}));
    }

    // Synthetic DPI notifications exercise the actual handler; they do not change
    // monitor DPI. Expectations always use GetDpiForWindow, never the fake value.
    let dpi_resources = probe::resources();
    for announced in [96_u32, 120, 144, 192, 96] {
        let old_dpi = unsafe { GetDpiForWindow(hwnd) };
        let old_icons = window_icons(hwnd);
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut rect).unwrap();
            SendMessageW(
                hwnd,
                WM_DPICHANGED,
                Some(WPARAM((announced | (announced << 16)) as usize)),
                Some(LPARAM(std::ptr::from_ref(&rect) as isize)),
            );
        }
        run.pump();
        let handles = assert_window_icons(hwnd, true, Badge::Error);
        if unsafe { GetDpiForWindow(hwnd) } == old_dpi {
            assert_eq!(handles, old_icons, "unchanged physical DPI reuses icons");
        }
        cases.push(
            json!({"phase":"synthetic-dpi-message","announced_dpi":announced,
            "actual_dpi":unsafe { GetDpiForWindow(hwnd) },"pixel_match":true}),
        );
    }
    assert_eq!(
        probe::resources(),
        dpi_resources,
        "DPI handler resource growth"
    );
    let dynamic = window_icons(hwnd);
    run.close(hwnd);
    assert_retired(dynamic);
    // Class HICONs are deliberately retained after HWND teardown.
    assert_icon(class[0], 16, false, Badge::Normal);
    assert_icon(class[1], 32, false, Badge::Normal);
    let baseline = probe::resources();
    for cycle in 0..8 {
        let dark = cycle % 2 != 0;
        let badge = [Badge::Normal, Badge::Attention, Badge::Error][cycle % 3];
        let hwnd = run.open(dark, badge);
        assert_eq!(class_icons(hwnd), class);
        let dynamic = assert_window_icons(hwnd, dark, badge);
        run.close(hwnd);
        assert_retired(dynamic);
        assert_icon(class[0], 16, false, Badge::Normal);
        assert_icon(class[1], 32, false, Badge::Normal);
        let after = probe::resources();
        assert_eq!(
            after, baseline,
            "HWND reopen/close must release dynamic icons and GDI resources"
        );
        cases.push(
            json!({"phase":"reopen","cycle":cycle,"resources":after,"class_icons_retained":true}),
        );
    }
    let result = json!({"passed":true,"synthetic_only":true,"pid":std::process::id(),
        "class_handles":class,"closed_baseline_gdi_user":baseline,"cases":cases,
        "dpi_scope":"synthetic notifications at actual HWND DPI; no real monitor transition claim",
        "shell_scope":"taskbar, Alt-Tab and Explorer require separate coordinator evidence"});
    if let Some(output) = output {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join("r13-dashboard-icons.json"))
            .unwrap()
            .write_all(&serde_json::to_vec_pretty(&result).unwrap())
            .unwrap();
    }
    println!("{result}");
}
