//! Manual, synthetic-only production HWND host. The coordinator builds this once,
//! then r10-native-capture.py runs the resulting test executable (never Cargo).
#![cfg(all(windows, feature = "testkit"))]

#[path = "../src/accessibility.rs"]
mod accessibility;
#[path = "../src/dashboard_persistence.rs"]
mod dashboard_persistence;
#[path = "../src/dashboard_window.rs"]
mod dashboard_window;
#[path = "support/r10_fixtures.rs"]
mod fixture;
#[path = "support/r10_wic.rs"]
mod wic;

use dashboard_window::{Bridge, Event, UiPort};
use qb_tray::{
    dashboard_model as model,
    geometry::{self, Scene, Target},
    render,
    state::{Command, DashboardState, ScrollRegion, Tab},
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    Graphics::Gdi::{ClientToScreen, InvalidateRect, UpdateWindow},
    System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::*,
    },
};

// No runtime, core, real profile, disk persistence, HTTP, or credential provider.
// enqueue does only a bounded nonblocking send, even during reentrant WndProc calls.
struct FakeBridge(SyncSender<Event>);
impl Bridge for FakeBridge {
    fn enqueue(&self, event: Event) {
        self.0
            .try_send(event)
            .expect("R10 bounded bridge overflow/disconnect");
    }
}
struct Run {
    port: Option<UiPort>,
    inbox: Receiver<Event>,
    snapshot: Arc<model::DashboardSnapshot>,
    expected: DashboardState,
    commands: Vec<String>,
    actions: Vec<String>,
    deadline: Instant,
    dark: bool,
    size: [f32; 2],
}
impl Run {
    fn service(&mut self) {
        while let Ok(event) = self.inbox.try_recv() {
            match event {
                Event::Opened { port } => {
                    assert!(port.snapshot(self.snapshot.clone(), fixture::clock()));
                    assert!(port.theme(self.dark));
                    self.port = Some(port);
                }
                Event::Command { command, .. } => {
                    self.commands.push(format!("{command:?}"));
                    if let Command::Detail(ticket) = command {
                        let session = self
                            .snapshot
                            .sessions
                            .sessions
                            .iter()
                            .find(|s| s.id == ticket.id)
                            .expect("synthetic detail id")
                            .clone();
                        let detail = fixture::detail(session);
                        assert!(
                            self.port
                                .as_ref()
                                .unwrap()
                                .detail(ticket.clone(), detail.clone())
                        );
                        assert!(
                            self.expected.publish_detail(&ticket, detail),
                            "detail action diverged"
                        );
                    }
                }
                Event::ClockNeeded { .. } => {
                    if let Some(port) = &self.port {
                        assert!(port.clock(fixture::clock()));
                    }
                }
                // Geometry is deliberately memory-only: never call persistence load/save.
                Event::GeometryDirty { .. } | Event::Closed { .. } => {}
            }
        }
    }
    fn pump(&mut self, millis: u64) {
        let until = Instant::now() + Duration::from_millis(millis);
        loop {
            assert!(Instant::now() < self.deadline, "R10 native host deadline");
            self.service();
            if let Some(port) = &self.port {
                assert!(port.clock(fixture::clock()));
            }
            unsafe {
                let mut msg = MSG::default();
                // Bound each drain so timer/paint activity cannot starve the deadline.
                for _ in 0..128 {
                    if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        break;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if Instant::now() >= until {
                break;
            }
            thread::sleep(Duration::from_millis(8));
        }
    }
    fn scene(&mut self) -> Scene {
        let view = model::project(&self.snapshot, &mut self.expected, &fixture::clock());
        // Same production DirectWrite measurement and Scene::build as the HWND.
        render::Renderer::new(self.dark)
            .unwrap()
            .scene(&view, &self.expected, self.size)
            .unwrap()
    }
    fn click(&mut self, hwnd: HWND, target: Target) {
        let scene = self.scene();
        let hit = scene
            .targets
            .iter()
            .rev()
            .find(|h| h.enabled && h.target == target)
            .expect("requested production hit target must be visible");
        let x = ((hit.rect.left + hit.rect.right) / 2.).round() as i32;
        let y = ((hit.rect.top + hit.rect.bottom) / 2.).round() as i32;
        assert_eq!(scene.hit_test(x as f32, y as f32), Some(&target));
        self.actions
            .push(format!("WM_LBUTTONDOWN/UP {target:?} at {x},{y}"));
        // Mirror only to compute subsequent public geometry. This never sets Host state.
        self.expected.activate(&target);
        unsafe {
            let p = LPARAM(((y as u32) << 16 | (x as u32 & 0xffff)) as isize);
            SendMessageW(hwnd, WM_MOUSEMOVE, Some(WPARAM(0)), Some(p));
            SendMessageW(hwnd, WM_LBUTTONDOWN, Some(WPARAM(1)), Some(p));
            SendMessageW(hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(p));
        }
        self.pump(180);
    }
    fn key(&mut self, hwnd: HWND, key: usize) {
        self.actions.push(format!("WM_KEYDOWN/UP {key:#x}"));
        unsafe {
            SendMessageW(hwnd, WM_KEYDOWN, Some(WPARAM(key)), Some(LPARAM(1)));
            SendMessageW(hwnd, WM_KEYUP, Some(WPARAM(key)), Some(LPARAM(0xc0000001)));
        }
        self.pump(130);
    }
    fn scroll(&mut self, hwnd: HWND) {
        let scene = self.scene();
        let region = scene
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Table)
            .unwrap();
        assert!(region.max_offset >= 96., "fixture must really scroll");
        let x = (region.viewport.left + 100.) as i32;
        let y = (region.viewport.top + 100.) as i32;
        self.expected
            .scroll(ScrollRegion::Table, region.offset + 192., region.max_offset);
        let mut p = POINT { x, y };
        unsafe {
            assert!(ClientToScreen(hwnd, &mut p).as_bool());
            let pos = LPARAM(((p.y as u32) << 16 | (p.x as u32 & 0xffff)) as isize);
            let delta = WPARAM(((-240i16) as u16 as usize) << 16);
            SendMessageW(hwnd, WM_MOUSEWHEEL, Some(delta), Some(pos));
        }
        self.actions
            .push("WM_MOUSEWHEEL -240: real table offset +192 DIP".into());
        self.pump(180);
    }
}
struct Cleanup;
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = dashboard_window::close_now_on_ui_thread();
        unsafe {
            CoUninitialize();
        }
    }
}

fn owned_dashboard() -> HWND {
    unsafe extern "system" fn visit(hwnd: HWND, context: LPARAM) -> windows::core::BOOL {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == std::process::id() {
            let mut class = [0u16; 64];
            let count = unsafe { GetClassNameW(hwnd, &mut class) };
            if count > 0
                && String::from_utf16_lossy(&class[..count as usize]) == "QuotaBatonDashboard"
            {
                unsafe { *(context.0 as *mut HWND) = hwnd };
                return false.into();
            }
        }
        true.into()
    }
    let mut found = HWND::default();
    unsafe {
        // Returning FALSE terminates enumeration once our own window was found.
        let _ = EnumWindows(Some(visit), LPARAM((&mut found as *mut HWND) as isize));
    }
    assert!(
        !found.is_invalid(),
        "this process must own a dashboard HWND"
    );
    found
}

#[test]
#[ignore = "manual production HWND evidence; coordinator must use r10-native-capture.py"]
fn r10_dashboard_native() {
    let output = PathBuf::from(std::env::var_os("R10_STATE_DIR").expect("R10_STATE_DIR"));
    assert!(
        output.is_absolute() && output.is_dir(),
        "collector creates isolated state directory"
    );
    let state: u8 = std::env::var("R10_STATE").unwrap().parse().unwrap();
    assert!((1..=15).contains(&state));
    unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        assert!(!previous.is_invalid(), "per-thread physical DPI awareness");
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
    }
    let _cleanup = Cleanup;
    let (send, recv) = mpsc::sync_channel(256);
    let snapshot = Arc::new(fixture::snapshot(state));
    let dark = matches!(state, 10 | 11);
    let size = if matches!(state, 7 | 15) {
        [1024., 640.]
    } else {
        [1320., 960.]
    };
    let mut run = Run {
        port: None,
        inbox: recv,
        snapshot: snapshot.clone(),
        expected: DashboardState::default(),
        commands: vec![],
        actions: vec![],
        deadline: Instant::now() + Duration::from_secs(55),
        dark,
        size,
    };
    dashboard_window::open_or_focus(
        HWND::default(),
        Arc::new(FakeBridge(send)),
        snapshot,
        fixture::clock(),
        dark,
    )
    .unwrap();
    run.pump(220);
    let hwnd = owned_dashboard();
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    assert_eq!(
        pid,
        std::process::id(),
        "never drive another process's dashboard"
    );
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    assert_eq!(
        dpi, 96,
        "1x requires actual 96 DPI; no synthetic DPI messages or settings changes"
    );
    // Resize OUTER dimensions from the actual nonclient extents to obtain exact client pixels.
    unsafe {
        let mut client = RECT::default();
        let mut window = RECT::default();
        GetClientRect(hwnd, &mut client).unwrap();
        GetWindowRect(hwnd, &mut window).unwrap();
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            size[0] as i32 + window.right - window.left - client.right,
            size[1] as i32 + window.bottom - window.top - client.bottom,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .unwrap();
    }
    run.pump(200);
    match state {
        2 => run.scroll(hwnd),
        3 | 9 | 15 => {
            run.click(hwnd, Target::Session(fixture::SELECTED.into()));
            assert!(
                run.expected.detail.is_some(),
                "real Bridge detail command required"
            );
            if state == 9 {
                // Mouse selection sets focus to this row; keyboard navigation enables its ring.
                run.key(hwnd, 0x09);
                // Walk native focus order to the selected row without mutating Host state.
                let scene = run.scene();
                let order: Vec<_> = scene.focus_order().into_iter().cloned().collect();
                let row = Target::Session(fixture::SELECTED.into());
                let at = order.iter().position(|t| t == &row).unwrap();
                // One initial Tab above moved from selected row to the following control.
                for _ in 1..order.len() {
                    run.key(hwnd, 0x09);
                }
                run.actions.push(format!(
                    "keyboard focus cycle returned to selected row, index {at}"
                ));
            }
        }
        4 => {
            run.click(hwnd, Target::Tab(Tab::Events));
            assert!(run.commands.iter().any(|c| c.starts_with("Activity(")));
        }
        5 => {
            run.click(hwnd, Target::Tab(Tab::Usage));
            assert!(run.commands.iter().any(|c| c == "Usage"));
        }
        12 => run.click(hwnd, Target::Group("cl-mero".into())),
        13 => {
            run.click(hwnd, Target::Search);
            for unit in "zzz-proj".encode_utf16() {
                unsafe {
                    SendMessageW(hwnd, WM_CHAR, Some(WPARAM(unit as usize)), Some(LPARAM(1)));
                }
            }
            run.expected.search = "zzz-proj".into();
            run.key(hwnd, 0x0d);
        }
        _ => {}
    }
    run.pump(250);
    let scene = run.scene();
    let mut interaction = render::Interaction::default();
    if state == 9 {
        interaction.focus = Some(Target::Session(fixture::SELECTED.into()));
        interaction.keyboard_focus = true;
    }
    // A separate native WIC render through exact production paint; never a scaled capture.
    wic::save(
        &output.join("shared-wic-2x.png"),
        &scene,
        &interaction,
        dark,
        size,
    )
    .unwrap();
    let expected: Vec<_> = scene
        .accessibility
        .iter()
        .filter(|n| !n.hidden)
        .map(|n| {
            json!({"name":n.name,"selected":n.selected,"enabled":n.enabled,
            "value":n.value,"expanded":n.expanded,"target":format!("{:?}",n.target)})
        })
        .collect();
    let ready = json!({"schema":1,"state":state,"pid":pid,"hwnd":hwnd.0 as usize,
        "dpi":dpi,"client_dip":size,"synthetic_clock":fixture::clock().now.to_rfc3339(),
        "theme":if dark {"dark"} else {"light"},"actions":run.actions,"commands":run.commands,
        "expected_uia":expected,"focus_required":if state == 9 {Some(fixture::SELECTED)} else {None},
        "wic_2x":{"kind":"shared-production-WIC","dpi":192,"physical_HWND_192DPI":false},
        "scope_gaps":if matches!(state,8|11) {vec!["Reference is a chip/tray comparison sheet; this HWND shows real dashboard account chips, not the comparison artwork or a tray HWND."]} else {vec![]}});
    std::fs::write(
        output.join("ready.tmp"),
        serde_json::to_vec_pretty(&ready).unwrap(),
    )
    .unwrap();
    std::fs::rename(output.join("ready.tmp"), output.join("ready.json")).unwrap();
    // Keep pumping for external PrintWindow and UIA, bounded even if collector disappears.
    while !output.join("release").exists() {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
            let _ = UpdateWindow(hwnd);
        }
        run.pump(40);
    }
}
