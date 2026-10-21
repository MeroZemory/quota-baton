//! Isolated actual-HWND cache regressions. One ignored test owns the UI thread.
//! Synthetic fixture + memory-only bridge; no config, HTTP, native CLI or worker.
//! Requires QB_DASHBOARD_PROFILE=1 for actual production frame counters.
#![cfg(all(windows, feature = "testkit"))]

#[path = "../src/accessibility.rs"]
mod accessibility;
#[path = "../src/dashboard_persistence.rs"]
#[allow(dead_code)]
mod dashboard_persistence;
#[path = "../src/dashboard_window.rs"]
#[allow(dead_code)]
mod dashboard_window;
#[path = "support/r1210_fixtures.rs"]
#[allow(dead_code)]
mod fixture;

use anyhow::{Context, Result, ensure};
use dashboard_window::{Bridge, Event, UiPort};
use qb_tray::{
    dashboard_model::Clock,
    geometry::{Draw, Scene, Target},
    state::{Command, ScrollRegion, Tab},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT, WPARAM},
        Graphics::Gdi::{ClientToScreen, UpdateWindow},
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    core::BOOL,
};

const TIMER: usize = 0x71; // Production dashboard timer; dispatch its real WndProc.
const MOUSE_LEAVE: u32 = 0x02A3;

struct MemoryBridge {
    sender: mpsc::SyncSender<Event>,
    fault: Arc<AtomicBool>,
}
impl Bridge for MemoryBridge {
    fn enqueue(&self, event: Event) {
        // Never panic/unwind across WndProc if this bounded channel fails.
        if self.sender.try_send(event).is_err() {
            self.fault.store(true, Ordering::Relaxed);
        }
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
struct Run {
    inbox: mpsc::Receiver<Event>,
    fault: Arc<AtomicBool>,
    port: Option<UiPort>,
    clocks: usize,
    commands: Vec<Command>,
    until: Instant,
}
impl Run {
    fn collect(&mut self) -> Result<()> {
        ensure!(
            !self.fault.load(Ordering::Relaxed),
            "synthetic bridge overflow/disconnect"
        );
        while let Ok(event) = self.inbox.try_recv() {
            match event {
                Event::Opened { port } => self.port = Some(port),
                Event::ClockNeeded { .. } => self.clocks += 1, // Deliberately held.
                Event::Command { command, .. } => self.commands.push(command),
                Event::GeometryDirty { .. } | Event::Closed { .. } => {}
            }
        }
        Ok(())
    }
    fn drain(&mut self) -> Result<()> {
        unsafe {
            let mut message = MSG::default();
            for _ in 0..4096 {
                ensure!(
                    Instant::now() < self.until,
                    "bounded HWND regression deadline"
                );
                if !PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    return self.collect();
                }
                ensure!(message.message != WM_QUIT, "unexpected UI thread shutdown");
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
                self.collect()?;
            }
        }
        anyhow::bail!("message queue did not settle within bounded drain")
    }
    fn frame(&mut self, hwnd: HWND) -> Result<()> {
        self.drain()?;
        // A retained frame must tolerate real paint messages without rebuilding.
        // Explicit paint also removes compositor/occlusion scheduling variability.
        send(hwnd, WM_PAINT, 0, 0);
        self.drain()?;
        ensure!(
            dashboard_window::perf_cache_state(hwnd).is_some(),
            "missing completed frame"
        );
        Ok(())
    }
    fn tick(&mut self, hwnd: HWND, clock: Clock, uptime_ms: u64) -> Result<()> {
        // This hook rebases clocks only: it must not invalidate or clear deadline.
        dashboard_window::perf_set_time(hwnd, clock, uptime_ms);
        send(hwnd, WM_TIMER, TIMER, 0);
        self.frame(hwnd)
    }
    fn no_rebuild(
        &mut self,
        hwnd: HWND,
        stage: &str,
    ) -> Result<Vec<dashboard_window::FrameTiming>> {
        let frames = dashboard_window::perf_take_frames(hwnd);
        ensure!(
            !frames.is_empty(),
            "{stage}: frame instrumentation unavailable"
        );
        ensure!(frames.iter().all(|f| !f.rebuilt), "{stage}: scene rebuilt");
        ensure!(
            frames.iter().all(|f| f.project_us == 0 && f.scene_us == 0),
            "{stage}: retained path projected or measured a scene"
        );
        Ok(frames)
    }
}
fn send(hwnd: HWND, message: u32, wparam: usize, lparam: isize) {
    unsafe {
        SendMessageW(hwnd, message, Some(WPARAM(wparam)), Some(LPARAM(lparam)));
    }
}
fn dashboard() -> Result<HWND> {
    unsafe extern "system" fn visit(hwnd: HWND, context: LPARAM) -> BOOL {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == std::process::id() {
            let mut name = [0u16; 64];
            let n = unsafe { GetClassNameW(hwnd, &mut name) };
            if n > 0 && String::from_utf16_lossy(&name[..n as usize]) == "QuotaBatonDashboard" {
                unsafe {
                    (*(context.0 as *mut Vec<HWND>)).push(hwnd);
                }
            }
        }
        true.into()
    }
    let mut found = Vec::<HWND>::new();
    unsafe {
        EnumWindows(Some(visit), LPARAM((&mut found as *mut Vec<HWND>) as isize))?;
    }
    ensure!(
        found.len() == 1,
        "expected exactly one process-owned dashboard"
    );
    Ok(found[0])
}
fn cache(hwnd: HWND) -> Result<dashboard_window::PerfCacheState> {
    dashboard_window::perf_cache_state(hwnd).context("missing cache state")
}
fn selected_tab(hwnd: HWND) -> Result<Tab> {
    dashboard_window::perf_scene(hwnd)
        .context("missing scene")?
        .accessibility
        .iter()
        .find_map(|n| match &n.target {
            Some(Target::Tab(tab)) if n.selected => Some(*tab),
            _ => None,
        })
        .context("selected tab missing")
}
fn hover(hwnd: HWND, target: Target) -> Result<()> {
    let scene = dashboard_window::perf_scene(hwnd).context("missing hover scene")?;
    let hit = scene
        .targets
        .iter()
        .rev()
        .find(|h| h.enabled && h.target == target)
        .context("hover target absent")?;
    let x = (hit.rect.left + hit.rect.right) / 2.;
    let y = (hit.rect.top + hit.rect.bottom) / 2.;
    ensure!(
        scene.hit_test(x, y) == Some(&target),
        "hover target covered"
    );
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.;
    let x = (x * scale).round() as i32;
    let y = (y * scale).round() as i32;
    send(
        hwnd,
        WM_MOUSEMOVE,
        0,
        (((y as u32 & 0xffff) << 16) | (x as u32 & 0xffff)) as isize,
    );
    Ok(())
}
fn invoke_tab(hwnd: HWND, tab: Tab, epoch: usize) -> Result<()> {
    // Tabs are the first three actionable scene nodes. Check this production
    // scene precondition explicitly instead of guessing an element/runtime ID.
    let scene = dashboard_window::perf_scene(hwnd).context("missing invoke scene")?;
    let first: Vec<_> = scene
        .accessibility
        .iter()
        .filter_map(|n| {
            if n.hidden || !n.enabled || n.rect.right <= n.rect.left || n.rect.bottom <= n.rect.top
            {
                return None;
            }
            n.target.as_ref().filter(|target| {
                scene
                    .targets
                    .iter()
                    .any(|h| h.enabled && &h.target == *target)
            })
        })
        .take(3)
        .cloned()
        .collect();
    let tabs = [Tab::Overview, Tab::Events, Tab::Usage];
    ensure!(
        first == tabs.map(Target::Tab),
        "first action slots no longer match tab nodes"
    );
    let slot = tabs.iter().position(|t| *t == tab).unwrap();
    send(
        hwnd,
        accessibility::WM_UIA_INVOKE,
        slot,
        isize::try_from(epoch)?,
    );
    Ok(())
}

fn sync_paint(hwnd: HWND) -> Result<()> {
    ensure!(
        unsafe { UpdateWindow(hwnd) }.as_bool(),
        "synchronous paint failed"
    );
    Ok(())
}

fn switch_tab(run: &mut Run, hwnd: HWND, tab: Tab) -> Result<Vec<dashboard_window::FrameTiming>> {
    let _ = dashboard_window::perf_take_frames(hwnd);
    invoke_tab(hwnd, tab, cache(hwnd)?.epoch)?;
    // UIA invoke completes the production paint synchronously. Observe it before
    // unrelated desktop messages; this path never answers a worker request.
    run.collect()?;
    ensure!(selected_tab(hwnd)? == tab, "tab did not dispatch: {tab:?}");
    let frames = dashboard_window::perf_take_frames(hwnd);
    ensure!(!frames.is_empty(), "tab frame instrumentation absent");
    Ok(frames)
}

fn fresh_tab(frames: &[dashboard_window::FrameTiming], stage: &str) -> Result<()> {
    ensure!(
        frames.iter().filter(|f| f.rebuilt).count() == 1
            && frames
                .iter()
                .any(|f| f.rebuilt && f.full_scene_build && f.project_us > 0),
        "{stage}: uncached tab must build full scene with fresh projection: {frames:?}"
    );
    Ok(())
}

fn warm_tab(frames: &[dashboard_window::FrameTiming], stage: &str) -> Result<()> {
    ensure!(
        frames.iter().all(|f| !f.rebuilt
            && !f.full_scene_build
            && f.scene_us == 0
            && f.project_us == 0
            && f.snapshot_us == 0),
        "{stage}: warm tab rebuilt scene/projection/snapshot: {frames:?}"
    );
    Ok(())
}

fn tab_invalidation_regressions(run: &mut Run, hwnd: HWND) -> Result<()> {
    let port = run.port.as_ref().context("missing mailbox")?.clone();
    let mut revision = 10;
    for reason in ["data", "filter", "scroll"] {
        if selected_tab(hwnd)? != Tab::Overview {
            let _ = switch_tab(run, hwnd, Tab::Overview)?;
        }
        revision += 1;
        let input = fixture::input();
        publish_fixture(hwnd, &port, &input, revision)?;
        dashboard_window::perf_set_time(hwnd, fixture::clock(), 40_000);
        fresh_tab(&switch_tab(run, hwnd, Tab::Usage)?, "seed Usage")?;
        dashboard_window::perf_set_time(hwnd, fixture::clock(), 40_000);
        warm_tab(&switch_tab(run, hwnd, Tab::Overview)?, "seed Overview")?;
        match reason {
            "data" => {
                let mut changed = input;
                changed["accounts"]["accounts"][0]["label"] =
                    serde_json::json!("cache revision changed");
                revision += 1;
                publish_fixture(hwnd, &port, &changed, revision)?;
            }
            "filter" => {
                let scene = dashboard_window::perf_scene(hwnd).context("missing filter scene")?;
                let grouped = scene
                    .accessibility
                    .iter()
                    .find(|n| n.target == Some(Target::ToggleGrouping))
                    .context("grouping state missing")?
                    .selected;
                let slot = scene
                    .accessibility
                    .iter()
                    .filter(|n| {
                        !n.hidden
                            && n.enabled
                            && n.role != qb_tray::geometry::Role::Text
                            && n.rect.right > n.rect.left
                            && n.rect.bottom > n.rect.top
                            && n.target.as_ref().is_some_and(|t| {
                                scene.targets.iter().any(|h| h.enabled && &h.target == t)
                            })
                    })
                    .position(|n| n.target == Some(Target::ToggleGrouping))
                    .context("grouping action missing")?;
                send(
                    hwnd,
                    accessibility::WM_UIA_INVOKE,
                    slot,
                    isize::try_from(cache(hwnd)?.epoch)?,
                );
                run.collect()?;
                ensure!(
                    dashboard_window::perf_scene(hwnd)
                        .context("missing regrouped scene")?
                        .accessibility
                        .iter()
                        .any(|n| n.target == Some(Target::ToggleGrouping) && n.selected != grouped),
                    "grouping action did not change state"
                );
            }
            "scroll" => {
                let scene = dashboard_window::perf_scene(hwnd).context("missing scroll scene")?;
                let region = scene
                    .content
                    .iter()
                    .find(|r| r.region == ScrollRegion::Table)
                    .context("scroll Table missing")?;
                ensure!(
                    region.max_offset > region.offset,
                    "scroll fixture already at bottom"
                );
                let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.;
                let mut point = POINT {
                    x: ((region.viewport.left + 10.) * scale) as i32,
                    y: ((region.viewport.top + 10.) * scale) as i32,
                };
                ensure!(
                    unsafe { ClientToScreen(hwnd, &mut point) }.as_bool(),
                    "scroll coordinate conversion failed"
                );
                send(
                    hwnd,
                    WM_MOUSEWHEEL,
                    usize::from((-120_i16) as u16) << 16,
                    (((point.y as u32 & 0xffff) << 16) | (point.x as u32 & 0xffff)) as isize,
                );
                sync_paint(hwnd)?;
                let scrolled =
                    dashboard_window::perf_scene(hwnd).context("missing scrolled scene")?;
                ensure!(
                    scrolled
                        .content
                        .iter()
                        .any(|r| r.region == ScrollRegion::Table && r.offset > region.offset),
                    "synthetic wheel did not scroll Table"
                );
            }
            _ => unreachable!(),
        }
        // Isolate invalidation from target deadline expiry, regardless of the
        // machine's time spent parsing the synthetic snapshot or painting.
        dashboard_window::perf_set_time(hwnd, fixture::clock(), 40_000);
        fresh_tab(&switch_tab(run, hwnd, Tab::Usage)?, reason)?;
    }

    // Let only the INACTIVE Overview deadline expire while Events remains valid.
    // No timer or mail may invalidate the cache before the restore attempt.
    let _ = switch_tab(run, hwnd, Tab::Overview)?;
    revision += 1;
    publish_fixture(hwnd, &port, &fixture::input(), revision)?;
    let overview = cache(hwnd)?;
    let deadline = overview.next_clock.context("Overview expiry missing")?;
    fresh_tab(
        &switch_tab(run, hwnd, Tab::Events)?,
        "first Events after data",
    )?;
    ensure!(
        cache(hwnd)?.next_clock.is_some_and(|at| at > deadline),
        "active Events deadline does not isolate inactive Overview expiry"
    );
    let mut expired = fixture::clock();
    expired.now = deadline.with_timezone(expired.now.offset());
    dashboard_window::perf_set_time(hwnd, expired, 40_000);
    fresh_tab(
        &switch_tab(run, hwnd, Tab::Overview)?,
        "expired Overview restore",
    )?;
    let restored = cache(hwnd)?;
    ensure!(
        restored.epoch > overview.epoch && restored.next_clock.is_some_and(|at| at > deadline),
        "expired target did not refresh publication/deadline: old={overview:?}, new={restored:?}"
    );
    let before = run.commands.len();
    invoke_tab(hwnd, Tab::Usage, overview.epoch)?;
    run.collect()?;
    ensure!(
        selected_tab(hwnd)? == Tab::Overview && run.commands.len() == before,
        "expired cached epoch dispatched an action"
    );
    Ok(())
}

fn cached_future_clock_regression(run: &mut Run, hwnd: HWND) -> Result<()> {
    let port = run.port.as_ref().context("missing mailbox")?.clone();
    // Active Usage is rendered at T; inactive Overview is rendered at T+100ms.
    // A correction to T+50ms is forward relative to the active completed view,
    // below the >2s jump threshold, but backward relative to the cached target.
    for (revision, via_mail) in [(16, false), (17, true)] {
        if selected_tab(hwnd)? != Tab::Usage {
            let _ = switch_tab(run, hwnd, Tab::Usage)?;
        }
        let clock = fixture::clock();
        publish_fixture(hwnd, &port, &fixture::input(), revision)?;
        let older = cache(hwnd)?;
        let mut future = clock.clone();
        future.now += chrono::Duration::milliseconds(100);
        ensure!(
            older.next_clock.is_some_and(|at| at > future.utc()),
            "Usage deadline must not confound cached-clock lower bound: {older:?}"
        );
        dashboard_window::perf_set_time(hwnd, future.clone(), 40_000);
        fresh_tab(
            &switch_tab(run, hwnd, Tab::Overview)?,
            "future Overview first visit",
        )?;
        let future_overview = cache(hwnd)?;
        dashboard_window::perf_set_time(hwnd, future, 40_000);
        warm_tab(
            &switch_tab(run, hwnd, Tab::Usage)?,
            "older Usage restoration",
        )?;
        let active = cache(hwnd)?;
        let mut correction = clock;
        correction.now += chrono::Duration::milliseconds(50);
        if via_mail {
            let _ = dashboard_window::perf_take_frames(hwnd);
            ensure!(
                port.clock(correction.clone()),
                "cached-future clock mailbox closed"
            );
            sync_mail(hwnd, &port)?;
            let frames = dashboard_window::perf_take_frames(hwnd);
            fresh_tab(&frames, "clock mail must invalidate future inactive view")?;
            ensure!(
                cache(hwnd)?.epoch > active.epoch,
                "future cached clock mail did not publish a new active epoch"
            );
        } else {
            // No timer/mail invalidation: exercise the requested-frame lower
            // bound guard itself, separately from apply_mail's cache scan.
            dashboard_window::perf_set_time(hwnd, correction.clone(), 40_000);
        }
        dashboard_window::perf_set_time(hwnd, correction, 40_000);
        fresh_tab(
            &switch_tab(run, hwnd, Tab::Overview)?,
            "reject future cached Overview",
        )?;
        let restored = cache(hwnd)?;
        ensure!(
            restored.epoch > future_overview.epoch,
            "future cached target did not receive a fresh publication: via_mail={via_mail}"
        );
        let before = run.commands.len();
        invoke_tab(hwnd, Tab::Events, future_overview.epoch)?;
        run.collect()?;
        ensure!(
            selected_tab(hwnd)? == Tab::Overview
                && cache(hwnd)?.epoch == restored.epoch
                && run.commands.len() == before,
            "future cached target epoch dispatched after correction: via_mail={via_mail}"
        );
    }
    Ok(())
}

fn sync_mail(hwnd: HWND, port: &UiPort) -> Result<()> {
    // Consume the real mailbox without pumping unrelated desktop input. The
    // posted wake can safely drain later, when the mailbox is already empty.
    send(
        hwnd,
        dashboard_window::dashboard_bridge::WAKE,
        port.owner() as usize,
        0,
    );
    sync_paint(hwnd)
}

fn region_pointer(hwnd: HWND, region: ScrollRegion) -> Result<()> {
    let scene = dashboard_window::perf_scene(hwnd).context("missing pointer scene")?;
    let viewport = scene
        .content
        .iter()
        .find(|r| r.region == region)
        .context("required pointer region missing")?
        .viewport;
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.;
    let x = ((viewport.left + 10.) * scale).round() as i32;
    let y = ((viewport.top + 10.) * scale).round() as i32;
    send(
        hwnd,
        WM_MOUSEMOVE,
        0,
        (((y as u32 & 0xffff) << 16) | (x as u32 & 0xffff)) as isize,
    );
    sync_paint(hwnd)
}

fn region_order(scene: &Scene, region: ScrollRegion) -> Vec<String> {
    match region {
        ScrollRegion::Table => scene.session_rows.iter().map(|r| r.id.clone()).collect(),
        ScrollRegion::Side => scene
            .accessibility
            .iter()
            .filter(|n| {
                n.key
                    .strip_prefix("event:")
                    .is_some_and(|s| s.parse::<u64>().is_ok())
            })
            .map(|n| n.key.clone())
            .collect(),
        _ => unreachable!("only Table and Recent are exercised"),
    }
}

fn publish_fixture(
    hwnd: HWND,
    port: &UiPort,
    input: &serde_json::Value,
    revision: u64,
) -> Result<()> {
    let mut snapshot = fixture::snapshot(input);
    snapshot.revision = revision;
    ensure!(
        port.snapshot(Arc::new(snapshot), fixture::clock()),
        "snapshot mailbox closed"
    );
    sync_mail(hwnd, port)
}

fn pointer_release_regressions(run: &mut Run, hwnd: HWND) -> Result<()> {
    // Ensure the actual Recent card/Side viewport exists rather than silently
    // testing the narrow layout's non-scrollable recent strip.
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.;
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            (1500. * scale) as i32,
            (1000. * scale) as i32,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )?;
    }
    run.frame(hwnd)?;
    let port = run.port.as_ref().context("missing mailbox")?.clone();
    let mut revision = 1;
    for (region, move_exit) in [
        (ScrollRegion::Table, false),
        (ScrollRegion::Table, true),
        (ScrollRegion::Side, false),
        (ScrollRegion::Side, true),
    ] {
        send(hwnd, MOUSE_LEAVE, 0, 0);
        sync_paint(hwnd)?;
        let mut input = fixture::input();
        revision += 1;
        publish_fixture(hwnd, &port, &input, revision)?;
        let scene = dashboard_window::perf_scene(hwnd).context("missing initial ordering")?;
        ensure!(
            scene.content.iter().any(|r| r.region == ScrollRegion::Side),
            "wide Recent viewport missing"
        );
        let old = region_order(&scene, region);
        ensure!(
            old.len() >= 2,
            "ordering fixture needs two rows: {region:?}"
        );
        region_pointer(hwnd, region)?; // Freeze the production region.
        let clocks_before = run.clocks;
        let commands_before = run.commands.len();
        if region == ScrollRegion::Table {
            let sessions = input["sessions"]["sessions"].as_array_mut().unwrap();
            let first = sessions
                .iter()
                .find(|s| s["id"].as_str() == Some(old[0].as_str()))
                .unwrap();
            let second = sessions
                .iter()
                .find(|s| s["id"].as_str() == Some(old[1].as_str()))
                .unwrap();
            ensure!(
                first["account"] == second["account"],
                "first two fixture rows must share account"
            );
            for s in sessions {
                if s["id"].as_str() == Some(old[1].as_str()) {
                    s["last_used_at"] =
                        serde_json::json!(fixture::clock().utc() - chrono::Duration::seconds(5));
                }
            }
        } else {
            let first: u64 = old[0].strip_prefix("event:").unwrap().parse()?;
            let second: u64 = old[1].strip_prefix("event:").unwrap().parse()?;
            for event in input["activity"].as_array_mut().unwrap() {
                if event["seq"].as_u64() == Some(first) {
                    event["ts"] =
                        serde_json::json!(fixture::clock().utc() - chrono::Duration::seconds(30));
                } else if event["seq"].as_u64() == Some(second) {
                    event["ts"] =
                        serde_json::json!(fixture::clock().utc() - chrono::Duration::seconds(5));
                }
            }
        }
        revision += 1;
        publish_fixture(hwnd, &port, &input, revision)?;
        let frozen = dashboard_window::perf_scene(hwnd).context("missing frozen ordering")?;
        ensure!(
            region_order(&frozen, region) == old,
            "frozen {region:?} lost retained order"
        );
        let _ = dashboard_window::perf_take_frames(hwnd);
        let before = cache(hwnd)?;
        if move_exit {
            region_pointer(
                hwnd,
                if region == ScrollRegion::Table {
                    ScrollRegion::Side
                } else {
                    ScrollRegion::Table
                },
            )?;
        } else {
            send(hwnd, MOUSE_LEAVE, 0, 0);
            sync_paint(hwnd)?;
        }
        let released = dashboard_window::perf_scene(hwnd).context("missing released ordering")?;
        let order = region_order(&released, region);
        ensure!(
            order.first() == Some(&old[1]),
            "pointer exit did not sort {region:?}: move_exit={move_exit}, old={old:?}, new={order:?}"
        );
        let frames = dashboard_window::perf_take_frames(hwnd);
        ensure!(
            frames.iter().any(|f| f.rebuilt
                && f.rebuild_reason == Some("pointer-release")
                && f.project_us > 0),
            "pointer release must invalidate stale projection: region={region:?}, move_exit={move_exit}, frames={frames:?}"
        );
        ensure!(
            cache(hwnd)?.epoch > before.epoch,
            "pointer release did not publish fresh UIA"
        );
        run.collect()?;
        ensure!(
            run.clocks == clocks_before && run.commands.len() == commands_before,
            "pointer release unexpectedly depended on worker command/clock reply"
        );
    }
    Ok(())
}

fn backward_clock_regression(run: &mut Run, hwnd: HWND) -> Result<()> {
    send(hwnd, MOUSE_LEAVE, 0, 0);
    sync_paint(hwnd)?;
    let port = run.port.as_ref().context("missing mailbox")?.clone();
    let clock = fixture::clock();
    let last = clock.utc() - chrono::Duration::seconds(60);
    let mut input = fixture::input();
    // Isolate the session age deadline from an event at exactly clock.utc(),
    // which would become a future-event deadline after the backward sample.
    input["activity"] = serde_json::json!([]);
    for s in input["sessions"]["sessions"].as_array_mut().unwrap() {
        s["last_used_at"] = serde_json::json!(last);
    }
    publish_fixture(hwnd, &port, &input, 10)?;
    let before = cache(hwnd)?;
    ensure!(
        before.next_clock == Some(clock.utc() + chrono::Duration::seconds(60)),
        "60-second fixture deadline unexpected: {before:?}"
    );
    let old = dashboard_window::perf_scene(hwnd).context("missing age scene")?;
    let rendered_age = |scene: &Scene, clock: &Clock| {
        let age = clock.age(Some(last));
        scene
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Table)
            .is_some_and(|region| {
                scene.draw.iter().any(|d| {
                    matches!(d, Draw::Text { rect, measure, .. }
                if measure.text == age
                    && rect.left < region.viewport.right && rect.right > region.viewport.left
                    && rect.top < region.viewport.bottom && rect.bottom > region.viewport.top)
                })
            })
    };
    ensure!(rendered_age(&old, &clock), "60-second age text absent");
    let _ = dashboard_window::perf_take_frames(hwnd);
    // Rebase only the virtual-clock anchor, retaining the completed projection.
    // The incoming sample differs by one second, below the old >2s jump rule.
    dashboard_window::perf_set_time(hwnd, clock.clone(), 40_000);
    let mut corrected = clock.clone();
    corrected.now -= chrono::Duration::seconds(1);
    ensure!(
        port.clock(corrected.clone()),
        "backward clock mailbox closed"
    );
    sync_mail(hwnd, &port)?;
    let after = cache(hwnd)?;
    let scene = dashboard_window::perf_scene(hwnd).context("missing corrected age scene")?;
    ensure!(
        after.epoch > before.epoch && after.next_clock == Some(clock.utc()),
        "one-second backward correction did not republish/rearm age boundary: before={before:?}, after={after:?}"
    );
    ensure!(
        rendered_age(&scene, &corrected) && !rendered_age(&scene, &clock),
        "backward age bucket still renders stale text"
    );
    ensure!(
        scene
            .accessibility
            .iter()
            .any(|n| n.key.starts_with("session:") && n.rect.bottom > n.rect.top),
        "corrected frame lost visible session UIA"
    );
    let frames = dashboard_window::perf_take_frames(hwnd);
    ensure!(
        frames.iter().filter(|f| f.rebuilt).count() == 1 && frames.iter().any(|f| f.project_us > 0),
        "backward correction must reproject/rebuild once: frames={frames:?}"
    );
    Ok(())
}

#[test]
#[ignore = "isolated actual HWND cache regressions; coordinator runner only"]
fn r1210_host_cache() -> Result<()> {
    ensure!(
        std::env::var_os("QB_DASHBOARD_PROFILE").is_some(),
        "QB_DASHBOARD_PROFILE=1 required"
    );
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }
    let (sender, inbox) = mpsc::sync_channel(256);
    let fault = Arc::new(AtomicBool::new(false));
    let mut run = Run {
        inbox,
        fault: fault.clone(),
        port: None,
        clocks: 0,
        commands: vec![],
        until: Instant::now() + Duration::from_secs(180),
    };
    // Drop the Host while its bridge receiver is still alive, including errors.
    let _cleanup = Cleanup;
    dashboard_window::open_or_focus(
        HWND::default(),
        Arc::new(MemoryBridge { sender, fault }),
        Arc::new(fixture::snapshot(&fixture::input())),
        fixture::clock(),
        false,
    )?;
    let hwnd = dashboard()?;
    unsafe {
        KillTimer(Some(hwnd), TIMER)?;
    }
    dashboard_window::perf_set_time(hwnd, fixture::clock(), 0);
    run.frame(hwnd)?;
    ensure!(run.port.is_some(), "missing synthetic mailbox");
    ensure!(
        selected_tab(hwnd)? == Tab::Overview,
        "isolated test did not start on Overview"
    );
    send(hwnd, MOUSE_LEAVE, 0, 0);
    send(hwnd, WM_KILLFOCUS, 0, 0);
    run.frame(hwnd)?;
    let _ = dashboard_window::perf_take_frames(hwnd);

    // Twenty deterministic timer messages, 5s of virtual elapsed time: the
    // fixture's most recent activity remains “just now” through this interval.
    let retained = cache(hwnd)?;
    ensure!(
        retained
            .next_clock
            .is_some_and(|at| at > fixture::clock().utc() + chrono::Duration::seconds(5)),
        "fixture deadline does not isolate no-visible-change ticks"
    );
    for i in 1..=20 {
        let mut clock = fixture::clock();
        clock.now += chrono::Duration::milliseconds(i * 250);
        run.tick(hwnd, clock, (i * 250) as u64)?;
        ensure!(
            cache(hwnd)?.epoch == retained.epoch,
            "no-change timer republished epoch"
        );
    }
    run.no_rebuild(hwnd, "twenty no-change timer ticks")?;

    for i in 0..20 {
        run.drain()?;
        let target = Target::Tab(if i % 2 == 0 { Tab::Usage } else { Tab::Events });
        hover(hwnd, target.clone())?;
        // Complete the invalidated production paint synchronously. Pumping
        // first lets unrelated desktop WM_MOUSEMOVE/WM_MOUSELEAVE replace the
        // synthetic hover, because TrackMouseEvent observes the real cursor.
        // Leave the user's cursor untouched and inspect this frame before drain.
        ensure!(
            unsafe { UpdateWindow(hwnd) }.as_bool(),
            "hover UpdateWindow failed"
        );
        ensure!(
            cache(hwnd)?.epoch == retained.epoch,
            "hover republished epoch"
        );
        let frames = run.no_rebuild(hwnd, "hover")?;
        ensure!(
            frames.last().and_then(|f| f.hover.as_deref()) == Some(format!("{target:?}").as_str()),
            "real mouse message did not update hover decoration: target={target:?}, frames={frames:?}"
        );
    }

    // Focus-only UIA publications replace maps; they must not append entries.
    ensure!(
        retained.actions >= 3 && retained.elements >= 3,
        "missing tab maps"
    );
    let mut previous_epoch = retained.epoch;
    for i in 0..40 {
        send(
            hwnd,
            if i % 2 == 0 {
                WM_SETFOCUS
            } else {
                WM_KILLFOCUS
            },
            0,
            0,
        );
        run.frame(hwnd)?;
        let current = cache(hwnd)?;
        ensure!(
            current.epoch > previous_epoch,
            "focus did not republish UIA"
        );
        ensure!(
            current.actions == retained.actions && current.elements == retained.elements,
            "focus publication grew frame action/element maps"
        );
        previous_epoch = current.epoch;
    }
    run.no_rebuild(hwnd, "forty focus-only publications")?;
    let before = run.commands.len();
    invoke_tab(hwnd, Tab::Usage, retained.epoch)?;
    run.frame(hwnd)?;
    ensure!(
        selected_tab(hwnd)? == Tab::Overview && run.commands.len() == before,
        "stale epoch dispatched an action"
    );
    ensure!(
        cache(hwnd)?.epoch == previous_epoch,
        "stale invoke changed publication"
    );
    run.no_rebuild(hwnd, "stale epoch invocation")?;
    let mut tab_clock = fixture::clock();
    tab_clock.now += chrono::Duration::seconds(5);
    dashboard_window::perf_set_time(hwnd, tab_clock.clone(), 5_000);
    invoke_tab(hwnd, Tab::Usage, previous_epoch)?;
    run.frame(hwnd)?;
    let tab_frames = dashboard_window::perf_take_frames(hwnd);
    ensure!(
        tab_frames.iter().filter(|f| f.rebuilt).count() == 1
            && tab_frames.iter().any(|f| f.rebuilt && f.tab == "Usage"),
        "current epoch tab action must rebuild Usage geometry exactly once: frames={tab_frames:?}"
    );
    fresh_tab(&tab_frames, "first Usage reveals hidden clock labels")?;
    ensure!(
        selected_tab(hwnd)? == Tab::Usage,
        "current epoch positive control did not dispatch"
    );
    ensure!(
        run.commands[before..]
            .iter()
            .any(|c| matches!(c, Command::Usage)),
        "current epoch positive control did not reach the memory bridge"
    );
    let usage = cache(hwnd)?;
    dashboard_window::perf_set_time(hwnd, tab_clock.clone(), 5_000);
    warm_tab(
        &switch_tab(&mut run, hwnd, Tab::Overview)?,
        "first warm Overview",
    )?;
    ensure!(
        cache(hwnd)?.epoch > usage.epoch,
        "warm Overview did not advance epoch"
    );
    dashboard_window::perf_set_time(hwnd, tab_clock.clone(), 5_000);
    fresh_tab(&switch_tab(&mut run, hwnd, Tab::Events)?, "first Events")?;
    let events = cache(hwnd)?;
    let mut latest_epoch = events.epoch;
    // Repeatedly cycle all three completed frames. Map counts must be bounded
    // per tab even though every prepared UIA publication gets a new epoch.
    for _ in 0..6 {
        for (tab, expected) in [
            (Tab::Usage, &usage),
            (Tab::Overview, &retained),
            (Tab::Events, &events),
        ] {
            dashboard_window::perf_set_time(hwnd, tab_clock.clone(), 5_000);
            warm_tab(&switch_tab(&mut run, hwnd, tab)?, "three-tab warm cache")?;
            let current = cache(hwnd)?;
            ensure!(
                current.epoch > latest_epoch
                    && current.actions == expected.actions
                    && current.elements == expected.elements,
                "warm prepared publication epoch/maps invalid: tab={tab:?}, current={current:?}, expected={expected:?}"
            );
            latest_epoch = current.epoch;
        }
    }
    dashboard_window::perf_set_time(hwnd, tab_clock.clone(), 5_000);
    warm_tab(
        &switch_tab(&mut run, hwnd, Tab::Usage)?,
        "warm Usage before stale invoke",
    )?;
    let warm_usage = cache(hwnd)?;
    let command_count = run.commands.len();
    invoke_tab(hwnd, Tab::Overview, usage.epoch)?;
    run.collect()?;
    ensure!(
        selected_tab(hwnd)? == Tab::Usage
            && cache(hwnd)?.epoch == warm_usage.epoch
            && run.commands.len() == command_count,
        "old cached Usage epoch dispatched"
    );
    dashboard_window::perf_set_time(hwnd, tab_clock, 5_000);
    warm_tab(
        &switch_tab(&mut run, hwnd, Tab::Overview)?,
        "warm Overview before midnight",
    )?;

    // Install an authoritative old-day sample through the real mailbox, then
    // withhold all clock replies while advancing past midnight without sleeping.
    let mut last_day = fixture::clock();
    last_day.now =
        (last_day.day_end - chrono::Duration::seconds(5)).with_timezone(last_day.now.offset());
    ensure!(
        run.port.as_ref().unwrap().clock(last_day.clone()),
        "mailbox closed"
    );
    run.tick(hwnd, last_day.clone(), 10_000)?;
    let midnight = cache(hwnd)?;
    ensure!(
        midnight.next_clock == Some(last_day.day_end),
        "fixture did not arm midnight: cache={midnight:?}, clocks={}, frames={:?}",
        run.clocks,
        dashboard_window::perf_take_frames(hwnd)
    );
    let _ = dashboard_window::perf_take_frames(hwnd);
    let requests = run.clocks;
    for i in 0..20 {
        let mut stale = last_day.clone();
        stale.now = (last_day.day_end + chrono::Duration::milliseconds(i * 250))
            .with_timezone(stale.now.offset());
        let uptime_ms = 16_000 + (i * 250) as u64;
        let before = cache(hwnd)?;
        let clocks_before = run.clocks;
        // Same path as Run::tick, with checkpoints to distinguish timer
        // invalidation from queued mail/focus publication during frame drain.
        dashboard_window::perf_set_time(hwnd, stale.clone(), uptime_ms);
        let rebased = dashboard_window::perf_cache_state(hwnd);
        send(hwnd, WM_TIMER, TIMER, 0);
        let after_timer = dashboard_window::perf_cache_state(hwnd);
        run.collect()?;
        let clocks_after_timer = run.clocks;
        run.frame(hwnd)?;
        let current = cache(hwnd)?;
        ensure!(
            current.clock_reconcile && current.epoch == midnight.epoch,
            "held midnight reconciliation rebuilt or lost pending state: \
             tick={i}, uptime_ms={uptime_ms}, input_clock={stale:?}, \
             expected_epoch={}, current_epoch={}, pending={}, \
             clocks_before={clocks_before}, clocks_after_timer={clocks_after_timer}, \
             clocks={}, expected_clocks={}, before={before:?}, rebased={rebased:?}, \
             after_timer={after_timer:?}, current={current:?}, baseline={midnight:?}, \
             retained_frames={:?}",
            midnight.epoch,
            current.epoch,
            current.clock_reconcile,
            run.clocks,
            requests + 1,
            dashboard_window::perf_take_frames(hwnd)
        );
        ensure!(
            run.clocks == requests + 1,
            "midnight requests were not coalesced: tick={i}, uptime_ms={uptime_ms}, \
             input_clock={stale:?}, clocks_before={clocks_before}, \
             clocks_after_timer={clocks_after_timer}, clocks={}, expected_clocks={}, \
             before={before:?}, rebased={rebased:?}, after_timer={after_timer:?}, current={current:?}, \
             baseline={midnight:?}, retained_frames={:?}",
            run.clocks,
            requests + 1,
            dashboard_window::perf_take_frames(hwnd)
        );
    }
    run.no_rebuild(hwnd, "held midnight clock reply")?;
    let mut stale = last_day.clone();
    stale.now =
        (last_day.day_end + chrono::Duration::seconds(16)).with_timezone(stale.now.offset());
    run.tick(hwnd, stale, 32_000)?;
    ensure!(
        run.clocks == requests + 2,
        "pending reconciliation did not retry after 15s"
    );
    run.no_rebuild(hwnd, "midnight retry")?;

    let mut fresh = last_day.clone();
    fresh.day_start = last_day.day_end;
    fresh.day_end = fresh.day_start + chrono::Duration::days(1); // Fixed synthetic non-DST day.
    fresh.now = (fresh.day_start + chrono::Duration::seconds(16)).with_timezone(fresh.now.offset());
    ensure!(
        run.port.as_ref().unwrap().clock(fresh.clone()),
        "reconciliation reply rejected"
    );
    run.tick(hwnd, fresh.clone(), 32_000)?;
    let frames = dashboard_window::perf_take_frames(hwnd);
    ensure!(
        frames.iter().filter(|f| f.rebuilt).count() == 1,
        "valid clock reply must rebuild exactly once: clocks={}, cache={:?}, frames={frames:?}",
        run.clocks,
        dashboard_window::perf_cache_state(hwnd)
    );
    // In contrast to a warm tab restoration, authoritative changed-clock
    // mail must replace the old projection. This large fixture makes projection
    // work measurable with the production microsecond counter.
    ensure!(
        frames.iter().any(|f| f.rebuilt && f.project_us > 0),
        "changed clock reply must reproject: frames={frames:?}"
    );
    let reconciled = cache(hwnd)?;
    ensure!(
        !reconciled.clock_reconcile && reconciled.epoch > midnight.epoch,
        "valid clock reply did not reset reconciliation/publication"
    );
    ensure!(
        reconciled
            .next_clock
            .is_some_and(|at| at > fresh.utc() + chrono::Duration::seconds(5)),
        "reconciled deadline is still stale or fixture interval is too short"
    );
    for i in 1..=20 {
        let mut clock = fresh.clone();
        clock.now += chrono::Duration::milliseconds(i * 250);
        run.tick(hwnd, clock, 32_000 + (i * 250) as u64)?;
        ensure!(
            cache(hwnd)?.epoch == reconciled.epoch,
            "post-reply timer rebuilt again"
        );
    }
    run.no_rebuild(hwnd, "post-reconciliation ticks")?;
    ensure!(
        run.clocks == requests + 2,
        "unexpected post-reconciliation clock request"
    );
    pointer_release_regressions(&mut run, hwnd)?;
    backward_clock_regression(&mut run, hwnd)?;
    tab_invalidation_regressions(&mut run, hwnd)?;
    cached_future_clock_regression(&mut run, hwnd)?;
    dashboard_window::close_now_on_ui_thread()?;
    ensure!(
        !unsafe { IsWindow(Some(hwnd)) }.as_bool(),
        "owned HWND survived close"
    );
    Ok(())
}
