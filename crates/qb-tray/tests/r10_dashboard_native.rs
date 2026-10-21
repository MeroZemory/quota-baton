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
    geometry::{self, Color, Draw, Face, Measure, Scene, Target, TextAlign},
    render,
    state::{Command, DashboardState, ScrollRegion, Tab},
    v2_text,
};
use serde_json::{Value, json};
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
        Input::KeyboardAndMouse::{GetKeyboardState, SetFocus, SetKeyboardState, VK_SHIFT},
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
    edge: Option<fixture::EdgeCase>,
    output: PathBuf,
    run_id: String,
    edge_step: u8,
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
        if self.edge.is_some() {
            assert_owned(hwnd);
            // The owned window can be occluded by the collector. Complete its
            // pending production paint before dispatching the next input.
            unsafe {
                let _ = SetFocus(Some(hwnd));
                SendMessageW(hwnd, WM_PAINT, None, None);
            }
        }
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

    fn edge_click(&mut self, hwnd: HWND, target: Target) {
        assert_owned(hwnd);
        // Synthetic input also runs when the owned window is occluded. Make
        // the previous production redraw complete before the next hit test.
        unsafe {
            let _ = SetFocus(Some(hwnd));
            SendMessageW(hwnd, WM_PAINT, None, None);
        }
        let scene = self.scene();
        let hit = scene
            .targets
            .iter()
            .rev()
            .find(|h| h.enabled && h.target == target)
            .expect("visible edge target");
        // Use the exposed left edge of a row/header. Its centre can be covered
        // by the overlay or by the separately hit-tested re-login action.
        let x = (hit.rect.left + 6.).round() as i32;
        let y = ((hit.rect.top + hit.rect.bottom) / 2.).round() as i32;
        assert_eq!(scene.hit_test(x as f32, y as f32), Some(&target));
        self.actions
            .push(format!("WM_LBUTTONDOWN/UP {target:?} at {x},{y}"));
        self.expected.activate(&target);
        unsafe {
            let p = LPARAM(((y as u32) << 16 | (x as u32 & 0xffff)) as isize);
            SendMessageW(hwnd, WM_MOUSEMOVE, Some(WPARAM(0)), Some(p));
            SendMessageW(hwnd, WM_LBUTTONDOWN, Some(WPARAM(1)), Some(p));
            SendMessageW(hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(p));
        }
        self.pump(100);
    }

    fn edge_resize(&mut self, hwnd: HWND, size: [f32; 2]) {
        assert_owned(hwnd);
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
        self.size = size;
        self.actions.push(format!(
            "SetWindowPos actual client {}x{}",
            size[0], size[1]
        ));
        self.pump(100);
        let mut client = RECT::default();
        unsafe { GetClientRect(hwnd, &mut client).unwrap() };
        assert_eq!(
            [client.right, client.bottom],
            [size[0] as i32, size[1] as i32]
        );
    }

    fn edge_scroll_to(&mut self, hwnd: HWND, requested: f32) {
        assert_owned(hwnd);
        let scene = self.scene();
        let region = scene
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Table)
            .unwrap();
        assert!(
            requested >= 0. && requested <= region.max_offset,
            "edge must really scroll"
        );
        // Production WM_MOUSEWHEEL converts each delta unit to 0.8 DIP. Work
        // from the rounded delivered delta, never assume the requested offset.
        let delta = ((region.offset - requested) / 0.8).round() as i16;
        let offset = region.offset - f32::from(delta) * 0.8;
        self.expected
            .scroll(ScrollRegion::Table, offset, region.max_offset);
        let mut p = POINT {
            x: region.viewport.left as i32 + 4,
            y: region.viewport.top as i32 + 4,
        };
        unsafe {
            assert!(ClientToScreen(hwnd, &mut p).as_bool());
            let pos = LPARAM(((p.y as u32) << 16 | (p.x as u32 & 0xffff)) as isize);
            SendMessageW(
                hwnd,
                WM_MOUSEWHEEL,
                Some(WPARAM((delta as u16 as usize) << 16)),
                Some(pos),
            );
        }
        self.actions
            .push(format!("WM_MOUSEWHEEL {delta}: table offset {offset} DIP"));
        self.pump(100);
    }

    fn edge_checkpoint(&mut self, hwnd: HWND, stage: &str, mut facts: Value, focus: Option<&str>) {
        assert_owned(hwnd);
        // Reset elapsed wall time as well as the timer. The synthetic fixture
        // is at :50, so otherwise a lengthy UIA traversal crosses a minute and
        // changes duration text despite an unchanged expected fixture clock.
        assert!(self.port.as_ref().unwrap().clock(fixture::clock()));
        self.pump(80);
        // Keep the synthetic clock fixed while external UIA traverses one frame.
        // A timer repaint republishes epoch-bound providers during traversal.
        // Input and admin messages still flow through the production window.
        unsafe {
            let _ = KillTimer(Some(hwnd), 0x71);
        }
        self.edge_step += 1;
        let directory = self.output.join(format!("edge-{:02}", self.edge_step));
        std::fs::create_dir(&directory).unwrap();
        let scene = self.scene();
        if self.edge == Some(fixture::EdgeCase::Overlay) {
            let surface = scene
                .targets
                .iter()
                .find(|h| h.target == Target::DetailSurface);
            let open = matches!(stage, "overlay-open" | "overlay-switch-row");
            assert_eq!(surface.is_some(), open);
            if let Some(surface) = surface {
                assert_eq!(geometry::width(surface.rect), 360.);
                assert_eq!(surface.rect.right, self.size[0] - 20.);
            }
            facts["list_columns"] = json!(column_geometry(&scene));
            facts["overlay_width"] = json!(surface.map(|h| geometry::width(h.rect)));
        }
        if self.edge == Some(fixture::EdgeCase::Sticky) && stage != "sticky-collapsed" {
            let account = if stage == "sticky-next" {
                fixture::NEXT_ACCOUNT
            } else {
                fixture::EDGE_ACCOUNT
            };
            assert!(
                scene
                    .accessibility
                    .iter()
                    .any(|n| n.key.starts_with(&format!("sticky:{account}:control:")))
            );
            assert!(
                scene
                    .draw
                    .iter()
                    .any(|d| matches!(d, Draw::Text {measure,..} if measure.text == "Claude · "))
            );
            facts["production_sticky_provider_prefix"] = json!("Claude · ");
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
            let _ = UpdateWindow(hwnd);
        }
        self.pump(40);
        hwnd_capture::save(hwnd, &directory.join("hwnd-client-1x.bgra")).unwrap();
        // Includes hidden chart history, with explicit zero bounds, and semantic
        // parents. External UIA must corroborate this measured production Scene.
        let expected: Vec<_> = scene.accessibility.iter().map(|n| json!({
            "key":n.key,"parent":n.parent,"name":n.name,"role":format!("{:?}",n.role),
            "selected":n.selected,"enabled":n.enabled,"value":n.value,"expanded":n.expanded,
            "target":format!("{:?}",n.target),"hidden":n.hidden,
            "rect":if n.hidden {[0.;4]} else {[n.rect.left,n.rect.top,geometry::width(n.rect),geometry::height(n.rect)]}
        })).collect();
        let record = json!({"schema":1,"state":self.edge.unwrap().state(),
            "theme":if self.dark {"dark"} else {"light"},"pid":std::process::id(),
            "hwnd":hwnd.0 as usize,"run_id":self.run_id,"dpi":96,"client_dip":self.size,
            "edge_case":self.edge.unwrap().name(),"edge_step":self.edge_step,"stage":stage,
            "actions":self.actions,"commands":self.commands,"expected_uia":expected,
            "focus_required":focus,"source_assertions":facts});
        std::fs::write(
            directory.join("ready.tmp"),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
        std::fs::rename(directory.join("ready.tmp"), directory.join("ready.json")).unwrap();
        // No next input until the collector has checked this exact HWND through
        // external UIA. An expected-state mirror is never execution evidence.
        while !directory.join("ack.json").exists() {
            assert!(
                !self.output.join("release").exists(),
                "collector aborted edge checkpoint"
            );
            self.pump(30);
        }
        let ack: Value =
            serde_json::from_slice(&std::fs::read(directory.join("ack.json")).unwrap()).unwrap();
        assert_eq!(ack["run_id"], self.run_id);
        assert_eq!(ack["hwnd"], hwnd.0 as usize);
        assert_eq!(ack["pid"], std::process::id());
        assert_eq!(ack["edge_step"], self.edge_step);
        assert_eq!(ack["status"], "asserted");
    }
}

fn assert_owned(hwnd: HWND) {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    assert_eq!(
        pid,
        std::process::id(),
        "only the synthetic host's own HWND"
    );
    assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
}

fn text_style(m: &Measure) -> v2_text::Style {
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    };
    v2_text::Style {
        face: match m.face {
            Face::Text => v2_text::Face::Text,
            Face::Mono => v2_text::Face::Mono,
            Face::Icon => v2_text::Face::Icon,
        },
        size: m.size,
        weight: if m.bold {
            DWRITE_FONT_WEIGHT_SEMI_BOLD
        } else {
            DWRITE_FONT_WEIGHT_NORMAL
        },
        line_height: m.line_height,
        align: match m.align {
            TextAlign::Leading => v2_text::Align::Leading,
            TextAlign::Center => v2_text::Align::Center,
        },
        wrap: if m.wrap {
            v2_text::Wrap::KeepAll
        } else {
            v2_text::Wrap::SingleLine
        },
    }
}

fn inside_rect(inner: qb_tray::Rect, outer: qb_tray::Rect) -> bool {
    inner.left >= outer.left - 0.01
        && inner.right <= outer.right + 0.01
        && inner.top >= outer.top - 0.01
        && inner.bottom <= outer.bottom + 0.01
}

fn column_geometry(scene: &Scene) -> Vec<(String, f32)> {
    let table = scene
        .content
        .iter()
        .find(|r| r.region == ScrollRegion::Table)
        .unwrap();
    scene
        .draw
        .iter()
        .filter_map(|draw| match draw {
            Draw::Text { rect, measure, .. }
                if (rect.top - (table.viewport.top - 24.)).abs() < 0.01
                    && [
                        "세션",
                        "클라이언트 · 번호",
                        "모델",
                        "시작",
                        "마지막 활동",
                        "오늘 요청",
                        "메모",
                    ]
                    .contains(&measure.text.as_str()) =>
            {
                Some((measure.text.clone(), geometry::width(*rect)))
            }
            _ => None,
        })
        .collect()
}

fn boundary_assertions(run: &mut Run) -> Value {
    use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
    let view = model::project(&run.snapshot, &mut run.expected, &fixture::clock());
    let g = view
        .groups
        .iter()
        .find(|g| g.id == fixture::EDGE_ACCOUNT)
        .unwrap();
    assert_eq!(g.name, fixture::LONG_ACCOUNT);
    assert!(g.relogin && !g.relogin_reason.is_empty());
    let scene = run.scene();
    let header = scene
        .targets
        .iter()
        .find(|h| h.target == Target::Group(g.id.clone()))
        .unwrap()
        .rect;
    let engine = v2_text::Text::new().unwrap();
    let fixed = [
        g.plan.clone(),
        g.chip.clone(),
        "트레이에서 다시 로그인".into(),
        "활성 세션".into(),
        format!("{} / {}", g.active, g.threshold.unwrap()),
    ];
    let mut measured = vec![];
    let mut boxes = vec![];
    for label in &fixed {
        let (r, m) = scene
            .draw
            .iter()
            .find_map(|draw| match draw {
                Draw::Text { rect, measure, .. }
                    if &measure.text == label && inside_rect(*rect, header) =>
                {
                    Some((*rect, measure))
                }
                _ => None,
            })
            .expect("fixed header text is drawn inside its real header");
        let layout = engine
            .layout(
                &m.text,
                D2D_RECT_F {
                    left: r.left,
                    top: r.top,
                    right: r.right,
                    bottom: r.bottom,
                },
                text_style(m),
            )
            .unwrap();
        assert_eq!(layout.metrics.visible_lines, 1, "fixed field {label}");
        assert!(
            !layout.metrics.trimmed,
            "fixed field must fit actual font: {label}"
        );
        assert!(layout.metrics.natural_width <= geometry::width(r) + 0.1);
        boxes.push(r);
        measured.push(json!({"text":label,"width":geometry::width(r),"natural_width":layout.metrics.natural_width,"trimmed":layout.metrics.trimmed}));
    }
    assert!(
        boxes
            .windows(2)
            .all(|pair| pair[0].right <= pair[1].left + 0.1),
        "fixed fields do not overlap"
    );
    let (name_box, name_measure) = scene
        .draw
        .iter()
        .find_map(|draw| match draw {
            Draw::Text { rect, measure, .. }
                if measure.text == g.name && inside_rect(*rect, header) =>
            {
                Some((*rect, measure))
            }
            _ => None,
        })
        .unwrap();
    let name_layout = engine
        .layout(
            &g.name,
            D2D_RECT_F {
                left: name_box.left,
                top: name_box.top,
                right: name_box.right,
                bottom: name_box.bottom,
            },
            text_style(name_measure),
        )
        .unwrap();
    assert!(
        geometry::width(name_box) > 0. && name_layout.metrics.trimmed,
        "long name trims first"
    );
    assert!(name_box.right <= boxes[0].left);
    let pips = scene
        .draw
        .iter()
        .filter(|draw| match draw {
            Draw::Fill {
                rect,
                color: Color::Accent,
                ..
            }
            | Draw::Stroke {
                rect,
                color: Color::PipOutline,
                ..
            } => {
                inside_rect(*rect, header)
                    && (geometry::width(*rect) - 8.).abs() < 0.01
                    && (geometry::height(*rect) - 8.).abs() < 0.01
            }
            _ => false,
        })
        .count();
    assert_eq!(
        pips as u64,
        g.threshold.unwrap(),
        "fitting T=5 retains every pip"
    );
    let narrow = run.size[0] < 1180.;
    let counts = scene
        .accessibility
        .iter()
        .find(|n| n.key == "header:counts")
        .unwrap();
    assert_eq!(counts.hidden, narrow);
    assert_eq!(
        scene.content.iter().any(|r| r.region == ScrollRegion::Side),
        !narrow
    );
    let list_card = scene
        .accessibility
        .iter()
        .find(|n| n.key == "sessions:table")
        .unwrap()
        .rect;
    let all_events = scene
        .targets
        .iter()
        .find(|h| h.target == Target::AllEvents)
        .unwrap()
        .rect;
    let side_card = scene
        .draw
        .iter()
        .find_map(|d| match d {
            Draw::Stroke {
                rect,
                color: Color::Border,
                ..
            } if inside_rect(all_events, *rect)
                && if narrow {
                    (geometry::height(*rect) - 44.).abs() < 0.01
                } else {
                    (geometry::width(*rect) - 360.).abs() < 0.01
                } =>
            {
                Some(*rect)
            }
            _ => None,
        })
        .expect("production recent strip/sidebar card");
    if narrow {
        assert!((side_card.top - list_card.bottom - 12.).abs() < 0.01);
        assert_eq!(geometry::width(side_card), geometry::width(list_card));
    } else {
        assert!((side_card.left - list_card.right - 16.).abs() < 0.01);
    }
    let table = scene
        .content
        .iter()
        .find(|r| r.region == ScrollRegion::Table)
        .unwrap();
    let columns = column_geometry(&scene);
    let has_column = |label: &str| columns.iter().any(|(name, _)| name == label);
    assert_eq!(has_column("메모"), narrow);
    assert!(has_column("시작") && has_column("모델") && has_column("클라이언트 · 번호"));
    assert_eq!(
        columns
            .iter()
            .find(|(name, _)| name == "클라이언트 · 번호")
            .unwrap()
            .1,
        172.
    );
    json!({"kind":"production-Scene-and-DirectWrite","client_width":run.size[0],
        "font_families":engine.families(),"fixed_fields":measured,"pips":pips,
        "long_name_trimmed":true,"columns":columns,"memo_column":narrow,"start_column":true,"model_column":true,
        "header_counts_visible":!narrow,"recent_strip":narrow,
        "table_viewport":[table.viewport.left,table.viewport.top,geometry::width(table.viewport),geometry::height(table.viewport)]})
}

fn relogin_header_assertions(run: &mut Run) -> Value {
    let view = model::project(&run.snapshot, &mut run.expected, &fixture::clock());
    let g = view
        .groups
        .iter()
        .find(|g| g.id == fixture::EDGE_ACCOUNT)
        .unwrap();
    assert_eq!(g.windows.len(), 2, "retained stale windows are required");
    assert!(!g.relogin_reason.is_empty());
    assert_eq!(
        g.relogin,
        run.edge != Some(fixture::EdgeCase::ReloginLinked)
    );
    let scene = run.scene();
    assert!(
        scene
            .draw
            .iter()
            .any(|d| matches!(d, Draw::Text {measure,..} if measure.text == g.relogin_reason))
    );
    for stale in ["63% 남음", "42% 남음"] {
        assert!(
            !scene
                .draw
                .iter()
                .any(|d| matches!(d, Draw::Text {measure,..} if measure.text == stale))
        );
        assert!(!scene.accessibility.iter().any(|n| n.name.contains(stale)));
    }
    assert_eq!(
        scene
            .targets
            .iter()
            .any(|h| h.target == Target::OpenTrayAccount(g.id.clone())),
        g.relogin
    );
    json!({"kind":"production-Scene","retained_windows":2,"reason":g.relogin_reason,
        "own_relogin_action":g.relogin,"last_observed":fixture::last_observed().to_rfc3339()})
}

fn relogin_chart_assertions(run: &mut Run) -> Value {
    let view = model::project(&run.snapshot, &mut run.expected, &fixture::clock());
    let tile = view
        .usage
        .iter()
        .find(|t| t.id == fixture::EDGE_ACCOUNT)
        .unwrap();
    assert!(tile.unconfirmed);
    assert_eq!(tile.last_observed, Some(fixture::last_observed()));
    assert_eq!(tile.series.len(), 2);
    assert!(
        tile.series
            .iter()
            .all(|s| s.points.len() >= 3 && s.points.last().unwrap().0 == fixture::last_observed())
    );
    let scene = run.scene();
    let chart = scene
        .charts
        .iter()
        .find(|c| c.id == fixture::EDGE_ACCOUNT)
        .unwrap();
    let plot = chart.rect;
    let x = |at: chrono::DateTime<chrono::Utc>| {
        plot.left
            + geometry::width(plot) * at.signed_duration_since(chart.from).num_milliseconds() as f32
                / chart
                    .to
                    .signed_duration_since(chart.from)
                    .num_milliseconds() as f32
    };
    let last_x = x(fixture::last_observed());
    let now_x = x(fixture::clock().utc());
    assert!(now_x > last_x + 1.);
    let mut lines = 0;
    let mut hatches = 0;
    for draw in &scene.draw {
        if let Draw::Line {
            from, to, color, ..
        } = draw
        {
            if matches!(color, Color::BarOk | Color::Series7d) {
                lines += 1;
                assert!(
                    from[0] <= last_x + 0.01 && to[0] <= last_x + 0.01,
                    "no extrapolated series"
                );
            }
            if matches!(color, Color::Track) {
                hatches += 1;
                for p in [from, to] {
                    assert!(
                        p[0] >= last_x - 0.01
                            && p[0] <= now_x + 0.01
                            && p[1] >= plot.top
                            && p[1] <= plot.bottom
                    );
                }
                assert!(((from[0] - to[0]).abs() - (from[1] - to[1]).abs()).abs() < 0.02);
            }
        }
    }
    assert!(
        lines >= 4 && hatches > 2,
        "real stopped history plus nonempty hatch required"
    );
    for label in ["63%", "42%", "63% 남음", "42% 남음"] {
        assert!(
            !scene
                .draw
                .iter()
                .any(|d| matches!(d, Draw::Text {measure,..} if measure.text == label)),
            "no current meter or endpoint label"
        );
    }
    let stamp = fixture::clock().time(Some(fixture::last_observed()));
    assert!(scene.draw.iter().any(|d| matches!(d, Draw::Text {measure,..} if measure.text == format!("{stamp} 이후 확인하지 못함 · 다시 로그인 필요"))));
    json!({"kind":"production-Scene","last_observed":fixture::last_observed().to_rfc3339(),
        "stamp":stamp,"series_lines":lines,"hatch_lines":hatches,"last_x":last_x,"now_x":now_x,
        "stopped_series":true,"no_current_endpoint_labels":true})
}

fn edge_actions(hwnd: HWND, run: &mut Run) {
    use fixture::EdgeCase;
    let edge = run.edge.unwrap();
    match edge {
        EdgeCase::Boundary1179 | EdgeCase::Boundary1180 => {
            let initial = edge.size();
            let other = if initial[0] == 1179. { 1180. } else { 1179. };
            // Cross the production layout breakpoint twice on this same HWND.
            for size in [initial, [other, 960.], initial] {
                run.edge_resize(hwnd, size);
                let facts = boundary_assertions(run);
                run.edge_checkpoint(
                    hwnd,
                    if size[0] == 1179. {
                        "boundary-1179"
                    } else {
                        "boundary-1180"
                    },
                    facts,
                    None,
                );
            }
        }
        EdgeCase::ReloginOwned | EdgeCase::ReloginLinked => {
            let facts = relogin_header_assertions(run);
            run.edge_checkpoint(hwnd, "relogin-header", facts, None);
            run.edge_click(hwnd, Target::Session(fixture::SELECTED.into()));
            let view = model::project(&run.snapshot, &mut run.expected, &fixture::clock());
            let detail = view
                .detail
                .as_ref()
                .expect("actual Detail command published a response");
            let remaining = &detail
                .facts
                .iter()
                .find(|(key, _)| key == "남은 양")
                .unwrap()
                .1;
            let stamp = fixture::clock().time(Some(fixture::last_observed()));
            assert!(
                remaining.contains("확인 불가")
                    && remaining.contains(&format!("{stamp} 기준"))
                    && remaining.contains("5시간 63%")
                    && remaining.contains("주간 42%")
            );
            run.edge_checkpoint(
                hwnd,
                "relogin-detail",
                json!({"kind":"production-Scene-and-actual-Bridge-command","stamp":stamp}),
                Some(fixture::SELECTED),
            );
            run.edge_click(hwnd, Target::Tab(Tab::Usage));
            assert!(run.commands.iter().any(|c| c == "Usage"));
            let facts = relogin_chart_assertions(run);
            run.edge_checkpoint(hwnd, "relogin-history", facts, None);
        }
        EdgeCase::Sticky => {
            let group = Target::Group(fixture::EDGE_ACCOUNT.into());
            run.edge_scroll_to(hwnd, 48.);
            let facts = relogin_header_assertions(run);
            run.edge_checkpoint(hwnd, "sticky-pinned", facts, None);
            let offset = run.expected.table_offset;
            let before = run
                .commands
                .iter()
                .filter(|c| *c == &format!("OpenTrayAccount({:?})", fixture::EDGE_ACCOUNT))
                .count();
            run.edge_click(hwnd, Target::OpenTrayAccount(fixture::EDGE_ACCOUNT.into()));
            let after = run
                .commands
                .iter()
                .filter(|c| *c == &format!("OpenTrayAccount({:?})", fixture::EDGE_ACCOUNT))
                .count();
            assert_eq!(
                after,
                before + 1,
                "owned Host must dispatch exactly one re-login bridge command"
            );
            assert!(!run.expected.collapsed.contains(fixture::EDGE_ACCOUNT));
            assert_eq!(run.expected.table_offset, offset);
            run.edge_checkpoint(
                hwnd,
                "sticky-action",
                json!({"bridge_dispatch_delta":after-before,"table_offset":offset}),
                None,
            );
            run.edge_click(hwnd, group.clone());
            assert!(run.expected.collapsed.contains(fixture::EDGE_ACCOUNT));
            assert_eq!(run.expected.table_offset, offset);
            run.edge_checkpoint(
                hwnd,
                "sticky-collapsed",
                json!({"table_offset":offset}),
                None,
            );
            run.edge_click(hwnd, group);
            run.edge_checkpoint(
                hwnd,
                "sticky-restored",
                json!({"table_offset":offset}),
                None,
            );
            let scene = run.scene();
            let viewport = scene
                .content
                .iter()
                .find(|r| r.region == ScrollRegion::Table)
                .unwrap()
                .viewport;
            // Group semantic bounds are viewport-clipped. Obtain the full bottom
            // from the last production session row, not a hard-coded row count.
            let view = model::project(&run.snapshot, &mut run.expected, &fixture::clock());
            let ids: Vec<_> = view
                .groups
                .iter()
                .find(|g| g.id == fixture::EDGE_ACCOUNT)
                .unwrap()
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect();
            let bottom = scene
                .session_rows
                .iter()
                .filter(|r| ids.contains(&r.id.as_str()))
                .map(|r| r.rect.bottom)
                .max_by(f32::total_cmp)
                .unwrap()
                + 1.;
            let push = run.expected.table_offset + bottom - viewport.top - 60. + 24.;
            run.edge_scroll_to(hwnd, push);
            run.edge_checkpoint(
                hwnd,
                "sticky-push",
                json!({"push_offset":run.expected.table_offset}),
                None,
            );
            run.edge_scroll_to(hwnd, push + 48.);
            run.edge_checkpoint(
                hwnd,
                "sticky-next",
                json!({"push_offset":run.expected.table_offset}),
                None,
            );
        }
        EdgeCase::Overlay => {
            run.edge_checkpoint(hwnd, "overlay-baseline", json!({"client":[1024,640]}), None);
            run.edge_click(hwnd, Target::Session(fixture::SELECTED.into()));
            run.edge_checkpoint(
                hwnd,
                "overlay-open",
                json!({"detail_command_required":true}),
                Some(fixture::SELECTED),
            );
            run.edge_click(hwnd, Target::CloseDetail);
            run.edge_checkpoint(
                hwnd,
                "overlay-x",
                json!({"closed_by":"X"}),
                Some(fixture::SELECTED),
            );
            run.edge_click(hwnd, Target::Session(fixture::SELECTED.into()));
            run.key(hwnd, 0x1b);
            run.expected.activate(&Target::CloseDetail);
            run.edge_checkpoint(
                hwnd,
                "overlay-esc",
                json!({"closed_by":"Esc"}),
                Some(fixture::SELECTED),
            );
            run.edge_click(hwnd, Target::Session(fixture::SELECTED.into()));
            run.edge_click(hwnd, Target::Session(fixture::SELECTED.into()));
            run.edge_checkpoint(
                hwnd,
                "overlay-second-click",
                json!({"closed_by":"selected-row-second-click"}),
                Some(fixture::SELECTED),
            );
            run.edge_click(hwnd, Target::Session(fixture::SELECTED.into()));
            run.edge_click(hwnd, Target::Session(fixture::OTHER_ROW.into()));
            run.edge_checkpoint(
                hwnd,
                "overlay-switch-row",
                json!({"detail_command_required":true}),
                Some(fixture::OTHER_ROW),
            );
        }
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
    let edge = std::env::var("R10_EDGE_CASE")
        .ok()
        .map(|s| fixture::EdgeCase::parse(&s));
    if let Some(edge) = edge {
        assert_eq!(state, edge.state(), "edge uses an existing protocol state");
    }
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
    let snapshot = Arc::new(match edge {
        Some(edge) => fixture::edge_snapshot(edge),
        None => fixture::snapshot(state),
    });
    let dark = theme == "dark";
    let size = if let Some(edge) = edge {
        edge.size()
    } else if matches!(state, 7 | 15) {
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
        edge,
        output: output.clone(),
        run_id: run_id.clone(),
        edge_step: 0,
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
    if edge.is_some() {
        edge_actions(hwnd, &mut run);
    } else {
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
        // Edge checkpoints may already have stopped the synthetic clock.
        let _ = KillTimer(Some(hwnd), 0x71);
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
    let mut ready = json!({"schema":1,"state":state,"run_id":run_id,"pid":pid,"hwnd":hwnd.0 as usize,
        "dpi":dpi,"client_dip":size,"synthetic_clock":fixture::clock().now.to_rfc3339(),
        "theme":if dark {"dark"} else {"light"},"actions":run.actions,"commands":run.commands,
        "expected_uia":expected,"focus_required":if state == 9 {Some(fixture::SELECTED)} else {None},
        "wic_2x":{"kind":"shared-production-WIC","dpi":192,"physical_HWND_192DPI":false},
        "scope_gaps":if matches!(state,8|11) {vec!["Reference is a chip/tray comparison sheet; this HWND shows real dashboard account chips, not the comparison artwork or a tray HWND."]} else {vec![]}});
    if let Some(edge) = edge {
        ready["edge_case"] = json!(edge.name());
        ready["edge_steps"] = json!(run.edge_step);
        ready["focus_required"] = json!(if edge == fixture::EdgeCase::Overlay {
            Some(fixture::OTHER_ROW)
        } else {
            None
        });
    }
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
