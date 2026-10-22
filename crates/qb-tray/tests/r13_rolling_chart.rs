//! Synthetic production-HWND chart evidence; coordinator builds/runs explicitly.
#![cfg(all(windows, feature = "testkit"))]

#[path = "../src/accessibility.rs"]
mod accessibility;
#[path = "support/r10_hwnd_capture.rs"]
mod capture;
#[allow(dead_code)]
#[path = "../src/dashboard_persistence.rs"]
mod dashboard_persistence;
#[allow(dead_code)]
#[path = "../src/dashboard_window.rs"]
mod dashboard_window;

use dashboard_window::{Bridge, Event, UiPort};
use qb_tray::{
    dashboard_model as m,
    geometry::{Draw, Role, Target, width},
    state::Tab,
    v2,
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
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

struct FakeBridge(mpsc::SyncSender<Event>, Arc<AtomicBool>);
impl Bridge for FakeBridge {
    fn enqueue(&self, event: Event) {
        // Called inside WndProc, including window destruction: never unwind
        // across that ABI boundary. The test checks this fault after cleanup.
        if self.0.try_send(event).is_err() {
            self.1.store(true, Ordering::Release);
        }
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

fn pump(inbox: &mpsc::Receiver<Event>, port: &mut Option<UiPort>, deadline: Instant) {
    let until = Instant::now() + Duration::from_millis(120);
    loop {
        assert!(Instant::now() < deadline, "rolling chart HWND deadline");
        while let Ok(event) = inbox.try_recv() {
            if let Event::Opened { port: opened } = event {
                *port = Some(opened);
            }
            // No persistence, command backend, HTTP or real clock provider.
        }
        unsafe {
            let mut msg = MSG::default();
            for _ in 0..128 {
                if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    break;
                }
                assert_ne!(msg.message, WM_QUIT);
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

fn owned_dashboard() -> HWND {
    unsafe extern "system" fn visit(hwnd: HWND, context: LPARAM) -> windows::core::BOOL {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let mut name = [0_u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut name) };
        if pid == std::process::id()
            && len > 0
            && String::from_utf16_lossy(&name[..len as usize]) == "QuotaBatonDashboard"
        {
            unsafe { (*(context.0 as *mut Vec<HWND>)).push(hwnd) };
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
    assert_eq!(found.len(), 1);
    found[0]
}

fn fixture(before_midnight: bool, empty: bool) -> (m::Clock, m::DashboardSnapshot) {
    use chrono::Duration;
    let now = chrono::DateTime::parse_from_rfc3339(if before_midnight {
        "2026-10-22T23:40:00+09:00"
    } else {
        "2026-10-23T00:20:00+09:00"
    })
    .unwrap();
    let day_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(*now.offset())
        .unwrap()
        .to_utc();
    let clock = m::Clock {
        now,
        day_start,
        day_end: day_start + Duration::days(1),
    };
    let reset = clock.utc() + Duration::minutes(30);
    let accounts = v2::project(&json!({"version":"R13-chart-synthetic","accounts":[
        {"id":"rolling","provider":"codex","email":"rolling@example.invalid","plan":"plus",
         "user_state":"active","health":{"kind":"ready"},"active_sessions":2,"spread_threshold":5,
         "windows":[{"name":"5h","used_pct":36,"reset_at":reset},
                    {"name":"7d","used_pct":20,"reset_at":clock.utc()+Duration::days(3)}]},
        {"id":"free","provider":"codex","email":"free@example.invalid","plan":"free",
         "user_state":"active","health":{"kind":"ready"},"active_sessions":0,"spread_threshold":5,
         "windows":[{"name":"43200m","used_pct":10,"reset_at":clock.utc()+Duration::days(20)}]}
    ]}).to_string(), clock.utc().into()).unwrap();
    let mut snapshot = m::DashboardSnapshot::new(accounts);
    let from = m::usage_domain(&clock).0;
    if !empty {
        snapshot.usage.accounts.push(m::AccountHistory {
            id: "rolling".into(),
            windows: vec![m::HistoryWindow {
                name: "5h".into(),
                points: vec![
                    (from, Some(10.)),
                    (clock.utc() - Duration::minutes(40), None),
                    (clock.utc() - Duration::minutes(30), Some(30.)),
                    (clock.utc(), Some(36.)),
                ],
                ..Default::default()
            }],
            ..Default::default()
        });
    }
    (clock, snapshot)
}

#[test]
#[ignore = "manual synthetic HWND capture; set project-local R13_CHART_OUTPUT"]
fn r13_rolling_chart_native_capture() {
    let output = PathBuf::from(std::env::var_os("R13_CHART_OUTPUT").expect("R13_CHART_OUTPUT"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    assert!(output.is_absolute() && output.is_dir());
    assert!(output.canonicalize().unwrap().starts_with(root));
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    assert!(!previous.is_invalid());
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
    let deadline = Instant::now() + Duration::from_secs(45);
    let (send, inbox) = mpsc::sync_channel(256);
    let mut port = None;
    let bridge_fault = Arc::new(AtomicBool::new(false));
    // Reverse drop order keeps the receiver and UiPort alive through WM_DESTROY,
    // even if a scene assertion unwinds before the explicit cleanup below.
    let cleanup = Cleanup(previous);
    let (clock, snapshot) = fixture(false, false);
    dashboard_window::open_or_focus(
        HWND::default(),
        Arc::new(FakeBridge(send, bridge_fault.clone())),
        Arc::new(snapshot),
        clock,
        false,
    )
    .unwrap();
    pump(&inbox, &mut port, deadline);
    let hwnd = owned_dashboard();
    let scene = dashboard_window::perf_scene(hwnd).expect("production frame");
    let tab = scene
        .targets
        .iter()
        .find(|t| t.target == Target::Tab(Tab::Usage))
        .unwrap()
        .rect;
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.;
    let x = ((tab.left + tab.right) / 2. * scale).round() as u32;
    let y = ((tab.top + tab.bottom) / 2. * scale).round() as u32;
    let position = LPARAM((y << 16 | x) as isize);
    unsafe {
        SendMessageW(hwnd, WM_LBUTTONDOWN, Some(WPARAM(1)), Some(position));
        SendMessageW(hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(position));
    }
    let mut records = Vec::new();
    for (case, before, empty) in [
        ("after-midnight", false, false),
        ("before-midnight", true, false),
        ("empty", false, true),
    ] {
        for dark in [false, true] {
            for size in [[1024, 640], [1320, 960]] {
                let (clock, snapshot) = fixture(before, empty);
                assert!(
                    port.as_ref()
                        .unwrap()
                        .snapshot(Arc::new(snapshot), clock.clone())
                );
                assert!(port.as_ref().unwrap().theme(dark));
                let dpi = unsafe { GetDpiForWindow(hwnd) };
                let mut client = RECT::default();
                let mut window = RECT::default();
                unsafe {
                    GetClientRect(hwnd, &mut client).unwrap();
                    GetWindowRect(hwnd, &mut window).unwrap();
                    SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        size[0] * dpi as i32 / 96 + window.right - window.left - client.right,
                        size[1] * dpi as i32 / 96 + window.bottom - window.top - client.bottom,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    )
                    .unwrap();
                }
                pump(&inbox, &mut port, deadline);
                let scene = dashboard_window::perf_scene(hwnd).expect("production chart frame");
                let chart = scene.charts.iter().find(|c| c.id == "rolling").unwrap();
                assert_eq!((chart.from, chart.to), m::usage_domain(&clock));
                assert_eq!(
                    scene.charts.iter().find(|c| c.id == "free").unwrap().series[0].name,
                    "30일"
                );
                assert_eq!(
                    scene.hit_test(chart.rect.left + 1., chart.rect.top + 1.),
                    Some(&Target::Chart("rolling".into()))
                );
                assert!(scene.chart_tooltip("rolling", chart.rect.right).is_none());
                assert!(scene.draw.iter().any(
                    |d| matches!(d, Draw::Text { measure, .. } if measure.text.contains("00:00"))
                ));
                assert!(scene.draw.iter().any(
                    |d| matches!(d, Draw::Text { measure, .. } if measure.text.contains("범위 밖"))
                ));
                if empty {
                    assert!(chart.series.iter().all(|s| s.points.is_empty()));
                } else if !before {
                    assert!(
                        scene
                            .accessibility
                            .iter()
                            .any(|n| n.role == Role::Row && n.name.contains("10/22 23:50:00"))
                    );
                }
                let name = format!(
                    "{case}-{}-{}x{}",
                    if dark { "dark" } else { "light" },
                    size[0],
                    size[1]
                );
                capture::save(hwnd, &output.join(format!("{name}.bgra"))).unwrap();
                unsafe { GetClientRect(hwnd, &mut client).unwrap() };
                assert_eq!(
                    std::fs::metadata(output.join(format!("{name}.bgra")))
                        .unwrap()
                        .len(),
                    (client.right * client.bottom * 4) as u64
                );
                records.push(json!({"file":format!("{name}.bgra"),"pixel_format":"BGRA8-top-down",
                    "width":client.right,"height":client.bottom,"dpi":dpi,"case":case,"dark":dark,
                    "from":chart.from,"to":chart.to,"now":chart.now,"plot_width_dip":width(chart.rect),
                    "scope":"actual production HWND WM_PRINTCLIENT"}));
            }
        }
    }
    std::fs::write(
        output.join("captures.json"),
        serde_json::to_vec_pretty(&records).unwrap(),
    )
    .unwrap();
    drop(cleanup);
    assert!(
        !bridge_fault.load(Ordering::Acquire),
        "synthetic bridge overflow/disconnect during capture or cleanup"
    );
}
