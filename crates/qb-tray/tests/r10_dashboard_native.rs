//! Manual, synthetic-only production HWND host. The coordinator builds this once,
//! then r10-native-capture.py runs the resulting test executable (never Cargo).
#![cfg(all(windows, feature = "testkit"))]

#[path = "../src/accessibility.rs"]
mod accessibility;
#[path = "../src/dashboard_persistence.rs"]
#[allow(dead_code)] // Only Geometry is used; this synthetic host never loads/saves a profile.
mod dashboard_persistence;
#[path = "../src/dashboard_window.rs"]
#[allow(dead_code)] // The FakeBridge intentionally uses a subset of the product bridge API.
mod dashboard_window;
#[path = "support/r10_fixtures.rs"]
mod fixture;
#[path = "support/r10_hwnd_capture.rs"]
mod hwnd_capture;
#[path = "support/r10_wic.rs"]
mod wic;

use dashboard_window::{Bridge, Event, UiPort};
use qb_tray::{
    dashboard_model as model,
    geometry::{Scene, Target},
    render,
    state::{Command, DashboardState, ScrollRegion, Tab},
};
use serde_json::json;
use std::{
    cell::RefCell,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{ClientToScreen, InvalidateRect, UpdateWindow},
    System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetThreadDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState, VK_SHIFT},
        WindowsAndMessaging::*,
    },
};

// No runtime, core, real profile, disk persistence, HTTP, or credential provider.
// enqueue does only a bounded nonblocking send, even during reentrant WndProc calls.
struct FakeBridge(SyncSender<Event>);

#[test]
fn r10_chip_cases_really_render_account_chips_instead_of_the_empty_screen() {
    for state in [8, 11] {
        let snapshot = fixture::snapshot(state);
        let mut ui = DashboardState::default();
        let view = model::project(&snapshot, &mut ui, &fixture::clock());
        assert!(matches!(view.empty, model::EmptyState::None));
        assert_eq!(view.groups.len(), 11);
        assert!(
            view.groups
                .iter()
                .any(|g| g.tone == qb_tray::v2::Tone::Limit)
        );
    }
}
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
// Count actual WM_PAINT dispatch without altering the production WndProc.
thread_local! {
    static PAINT_PROBE: RefCell<Option<(WNDPROC, u64)>> = const { RefCell::new(None) };
}
unsafe extern "system" fn observe_paints(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let old = PAINT_PROBE.with(|slot| {
        let mut slot = slot.borrow_mut();
        slot.as_mut().and_then(|(old, count)| {
            if msg == WM_PAINT {
                *count = count.saturating_add(1);
            }
            *old
        })
    });
    unsafe { CallWindowProcW(old, hwnd, msg, w, l) }
}
struct PaintProbe {
    hwnd: HWND,
    previous: isize,
}
impl PaintProbe {
    fn new(hwnd: HWND) -> Self {
        let previous = unsafe {
            SetWindowLongPtrW(
                hwnd,
                GWLP_WNDPROC,
                observe_paints as *const () as usize as isize,
            )
        };
        assert_ne!(previous, 0);
        let old = unsafe { std::mem::transmute::<isize, WNDPROC>(previous) };
        PAINT_PROBE.with(|slot| assert!(slot.borrow_mut().replace((old, 0)).is_none()));
        Self { hwnd, previous }
    }
    fn count(&self) -> u64 {
        PAINT_PROBE.with(|slot| slot.borrow().as_ref().unwrap().1)
    }
}
impl Drop for PaintProbe {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_WNDPROC, self.previous);
        }
        PAINT_PROBE.with(|slot| *slot.borrow_mut() = None);
    }
}
fn root_encoding_probes(hwnd: HWND, run: &mut Run) {
    let probe = PaintProbe::new(hwnd);
    let root = windows::Win32::UI::Accessibility::UiaRootObjectId;
    for object in [root as isize, root as u32 as isize] {
        unsafe {
            SendMessageW(hwnd, WM_SIZE, None, None);
        }
        let before = probe.count();
        let result = unsafe { SendMessageW(hwnd, WM_GETOBJECT, None, Some(LPARAM(object))) };
        assert_ne!(
            result.0, 0,
            "real provider returned for root encoding {object:#x}"
        );
        assert!(
            probe.count() > before,
            "pending root encoding {object:#x} must complete a real paint"
        );
        run.actions.push(format!(
            "pending WM_GETOBJECT {object:#x}: actual WM_PAINT observed"
        ));
    }
    unsafe {
        SendMessageW(hwnd, WM_SIZE, None, None);
    }
    let before = probe.count();
    unsafe {
        SendMessageW(hwnd, WM_GETOBJECT, None, Some(LPARAM(1)));
    }
    assert_eq!(
        probe.count(),
        before,
        "non-root must not force production paint"
    );
    run.actions
        .push("non-root WM_GETOBJECT: no forced WM_PAINT".into());
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
                unsafe { (*(context.0 as *mut Vec<HWND>)).push(hwnd) };
            }
        }
        true.into()
    }
    let mut found = Vec::<HWND>::new();
    unsafe {
        EnumWindows(Some(visit), LPARAM((&mut found as *mut Vec<HWND>) as isize)).unwrap();
    }
    assert_eq!(
        found.len(),
        1,
        "this process must own exactly one dashboard"
    );
    found[0]
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
    let theme = std::env::var("R10_THEME").expect("R10_THEME");
    assert!(matches!(theme.as_str(), "light" | "dark"));
    let run_id = std::env::var("R10_RUN_ID").expect("R10_RUN_ID");
    assert!(run_id.len() == 32 && run_id.bytes().all(|c| c.is_ascii_hexdigit()));
    unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        assert!(!previous.is_invalid(), "per-thread physical DPI awareness");
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
    }
    let _cleanup = Cleanup;
    let (send, recv) = mpsc::sync_channel(256);
    let snapshot = Arc::new(fixture::snapshot(state));
    let dark = theme == "dark";
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
    if state == 1 && !dark {
        root_encoding_probes(hwnd, &mut run);
        run.pump(80);
    }
    match state {
        2 => run.scroll(hwnd),
        3 | 9 | 15 => {
            run.click(hwnd, Target::Session(fixture::SELECTED.into()));
            assert!(
                run.expected.detail.is_some(),
                "real Bridge detail command required"
            );
            if state == 9 {
                // Focus order can change as navigation reveals rows. A forward/backward
                // pair returns to this row without traversing a changing scroll viewport.
                run.key(hwnd, 0x09);
                let mut keys = [0u8; 256];
                unsafe { GetKeyboardState(&mut keys).unwrap() };
                struct RestoreKeys([u8; 256]);
                impl Drop for RestoreKeys {
                    fn drop(&mut self) {
                        unsafe { SetKeyboardState(&self.0).unwrap() };
                    }
                }
                let restore = RestoreKeys(keys);
                keys[VK_SHIFT.0 as usize] |= 0x80;
                unsafe { SetKeyboardState(&keys).unwrap() };
                run.key(hwnd, 0x09);
                drop(restore);
                run.actions
                    .push("Tab then Shift+Tab returned to selected row".into());
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
            json!({"name":n.name,"role":format!("{:?}",n.role),"selected":n.selected,"enabled":n.enabled,
            "value":n.value,"expanded":n.expanded,"target":format!("{:?}",n.target)})
        })
        .collect();
    // Freeze only this synthetic host's periodic timer after expensive WIC work.
    // External UIA reads one completed fixture frame instead of racing clock repaints.
    // Production clock behavior is unchanged; no system clock or global input is altered.
    assert!(run.port.as_ref().unwrap().clock(fixture::clock()));
    run.pump(80);
    unsafe {
        KillTimer(Some(hwnd), 0x71).unwrap();
        let _ = InvalidateRect(Some(hwnd), None, false);
        let _ = UpdateWindow(hwnd);
    }
    run.pump(40);
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client).unwrap() };
    assert_eq!(
        [client.right, client.bottom],
        [size[0] as i32, size[1] as i32]
    );
    hwnd_capture::save(hwnd, &output.join("hwnd-client-1x.bgra")).unwrap();
    let ready = json!({"schema":1,"state":state,"run_id":run_id,"pid":pid,"hwnd":hwnd.0 as usize,
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
        run.pump(40);
    }
    assert_eq!(
        std::fs::read_to_string(output.join("release")).unwrap(),
        run_id
    );
    drop(_cleanup);
    assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
    let completion = json!({"schema":1,"state":state,"theme":theme,"pid":pid,
        "run_id":run_id,"status":"completed","window_closed":true});
    std::fs::write(
        output.join("completion.tmp"),
        serde_json::to_vec_pretty(&completion).unwrap(),
    )
    .unwrap();
    std::fs::rename(
        output.join("completion.tmp"),
        output.join("completion.json"),
    )
    .unwrap();
}
