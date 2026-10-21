//! Opt-in, synthetic real-HWND release-profile baseline/fix workload.
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
mod fixture;

use anyhow::{Context, Result, ensure};
use dashboard_window::{Bridge, Event, FrameTiming, UiPort};
use qb_tray::{
    geometry::{Scene, Target},
    state::{Command, ScrollRegion, Tab},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{FILETIME, HWND, LPARAM, POINT, RECT, WPARAM},
        Graphics::Gdi::{ClientToScreen, UpdateWindow},
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
                SetThreadDpiAwarenessContext,
            },
            WindowsAndMessaging::*,
        },
    },
    core::BOOL,
};

// State encodings match host-instrumented-v2.rs: Debug tab/target names;
// offsets are Table, Side, Events, Usage. Hooks are called on the owning UI thread.
enum ExpectedState {
    Hover { tab: String, target: String },
    Scroll { tab: String, offset: f32 },
    Tab(String),
}
impl ExpectedState {
    fn matches(&self, frame: &FrameTiming) -> bool {
        match self {
            Self::Hover { tab, target } => {
                &frame.tab == tab && frame.hover.as_ref() == Some(target)
            }
            Self::Scroll { tab, offset } => {
                &frame.tab == tab && (frame.offsets[0] - offset).abs() <= 1.
            }
            Self::Tab(tab) => &frame.tab == tab,
        }
    }
    fn description(&self) -> Value {
        match self {
            Self::Hover { tab, target } => json!({"tab":tab,"hover":target}),
            Self::Scroll { tab, offset } => {
                json!({"tab":tab,"table_offset":offset,"tolerance_dip":1})
            }
            Self::Tab(tab) => json!({"tab":tab}),
        }
    }
}
fn selected_tab(scene: &Scene) -> Result<String> {
    scene
        .accessibility
        .iter()
        .find_map(|node| {
            if !node.selected {
                return None;
            }
            if let Some(Target::Tab(tab)) = &node.target {
                Some(format!("{tab:?}"))
            } else {
                None
            }
        })
        .context("selected tab missing from cached production scene")
}
fn latency_summary(samples: &[Value]) -> Value {
    let mut values: Vec<_> = samples
        .iter()
        .filter_map(|s| s["input_to_observed_frame_us"].as_u64())
        .collect();
    let max = values.iter().max().copied();
    let p50 = percentile(&mut values, 0.50);
    let p95 = percentile(&mut values, 0.95);
    json!({"n":values.len(),"p50":p50,"p95":p95,"max":max,
        "includes_clock_due_samples":true,"kind":"synchronous HWND input-to-production-paint upper bound; no OS queue latency"})
}
fn cause_counts(frames: &[Value]) -> Value {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for frame in frames.iter().filter(|f| f["rebuilt"] == true) {
        let reason = frame["rebuild_reason"].as_str().unwrap_or("unknown");
        *counts.entry(reason.to_owned()).or_default() += 1;
    }
    json!(counts)
}
fn independent_rebuild(frame: &Value) -> bool {
    matches!(
        frame["rebuild_reason"].as_str(),
        Some("clock" | "data" | "timer" | "mail")
    )
}

struct FakeBridge {
    sender: mpsc::SyncSender<Event>,
    fault: Arc<AtomicU8>,
}
impl Bridge for FakeBridge {
    fn enqueue(&self, event: Event) {
        let code = match self.sender.try_send(event) {
            Ok(()) => return,
            Err(mpsc::TrySendError::Full(_)) => 1,
            Err(mpsc::TrySendError::Disconnected(_)) => 2,
        };
        // Called from WndProc: record failure without unwinding across the ABI.
        let _ = self
            .fault
            .compare_exchange(0, code, Ordering::Relaxed, Ordering::Relaxed);
    }
}
struct Cleanup {
    cursor: POINT,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = dashboard_window::close_now_on_ui_thread();
        unsafe {
            let _ = SetCursorPos(self.cursor.x, self.cursor.y);
        }
        unsafe {
            CoUninitialize();
        }
    }
}
struct Run {
    inbox: mpsc::Receiver<Event>,
    port: Option<UiPort>,
    started: Instant,
    deadline: Instant,
    read_commands: Vec<String>,
    bridge_fault: Arc<AtomicU8>,
}
impl Run {
    fn service(&mut self) -> Result<()> {
        match self.bridge_fault.load(Ordering::Relaxed) {
            0 => {}
            1 => anyhow::bail!("performance bridge overflow"),
            _ => anyhow::bail!("performance bridge disconnected"),
        }
        while let Ok(event) = self.inbox.try_recv() {
            match event {
                Event::Opened { port } => self.port = Some(port),
                Event::ClockNeeded { .. } => {
                    let mut clock = fixture::clock();
                    // Monotonic synthetic wall time: never reset to frozen base.
                    clock.now +=
                        chrono::Duration::milliseconds(self.started.elapsed().as_millis() as i64);
                    ensure!(
                        self.port.as_ref().context("missing port")?.clock(clock),
                        "clock mailbox closed"
                    );
                }
                Event::Command { command, .. } => match command {
                    Command::Sessions(_) | Command::Activity(_) | Command::Usage => {
                        // The initial complete synthetic snapshot already holds
                        // these read results. No extra mail/rebuild is fabricated.
                        self.read_commands.push(format!("{command:?}"));
                    }
                    _ => anyhow::bail!("unexpected action in input-only performance workload"),
                },
                Event::GeometryDirty { .. } | Event::Closed { .. } => {}
            }
        }
        Ok(())
    }
    fn dispatch(&mut self) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline,
            "600-second workload deadline"
        );
        self.service()?;
        let dispatch_started = Instant::now();
        unsafe {
            let mut msg = MSG::default();
            let batch = Instant::now();
            for _ in 0..128 {
                if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    break;
                }
                ensure!(msg.message != WM_QUIT, "unexpected window-thread shutdown");
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                // A slow paint must return control to the measurement owner;
                // timer traffic can otherwise replenish this queue forever.
                if msg.message == WM_PAINT || batch.elapsed() >= Duration::from_millis(5) {
                    break;
                }
                if dispatch_started.elapsed() >= Duration::from_millis(20) {
                    break;
                }
            }
        }
        Ok(())
    }
    fn wait_message(&self, until: Instant) {
        let ms = until
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 1000) as u32;
        unsafe {
            MsgWaitForMultipleObjectsEx(None, ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }
    fn pump_for(&mut self, duration: Duration) -> Result<()> {
        let until = Instant::now() + duration;
        loop {
            self.dispatch()?;
            if Instant::now() >= until {
                return Ok(());
            }
            self.wait_message(until);
        }
    }
    fn sample(
        &mut self,
        hwnd: HWND,
        expected: ExpectedState,
        action: impl FnOnce() -> Result<()>,
    ) -> Result<Value> {
        self.dispatch()?;
        // Complete any pending production paint before admitting this input.
        // This is outside latency timing; no measured slow frame is discarded.
        unsafe {
            let _ = UpdateWindow(hwnd);
        }
        let _ = dashboard_window::perf_take_frames(hwnd);
        let before_seq = dashboard_window::perf_input_seq(hwnd);
        let started = Instant::now();
        action()?;
        let input_seq = dashboard_window::perf_input_seq(hwnd);
        ensure!(
            input_seq > before_seq,
            "injection did not advance Host input sequence"
        );
        // Paint the production HWND immediately after the synchronous input.
        // No queued desktop input may supersede this action before its frame.
        // This measures the real Host paint, not OS input-queue latency.
        unsafe {
            let _ = UpdateWindow(hwnd);
        }
        let observed = dashboard_window::perf_take_frames(hwnd);
        let observed_us = started.elapsed().as_micros() as u64;
        let matched = observed
            .iter()
            .position(|f| f.input_seq >= input_seq && expected.matches(f));
        ensure!(
            matched.is_some(),
            "no synchronous correlated frame; expected={} injected_seq={} observed={}",
            expected.description(),
            input_seq,
            serde_json::to_value(&observed)?
        );
        // Keep all frames and slow samples. No retries or latency filtering.
        Ok(json!({"input_seq_before":before_seq,"input_seq":input_seq,
            "expected_state":expected.description(),"matched_frame_index":matched.unwrap(),
            "input_to_observed_frame_us":observed_us,"frames":observed}))
    }
}

fn owned_dashboard() -> Result<HWND> {
    unsafe extern "system" fn visit(hwnd: HWND, context: LPARAM) -> BOOL {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == std::process::id() {
            let mut class = [0u16; 64];
            let n = unsafe { GetClassNameW(hwnd, &mut class) };
            if n > 0 && String::from_utf16_lossy(&class[..n as usize]) == "QuotaBatonDashboard" {
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
fn scene(hwnd: HWND) -> Result<Scene> {
    dashboard_window::perf_scene(hwnd).context("completed production scene unavailable")
}
fn point(scene: &Scene, target: &Target) -> Result<[i32; 2]> {
    let hit = scene
        .targets
        .iter()
        .rev()
        .find(|h| h.enabled && &h.target == target)
        .context("cached scene target unavailable")?;
    let p = [
        ((hit.rect.left + hit.rect.right) / 2.).round() as i32,
        ((hit.rect.top + hit.rect.bottom) / 2.).round() as i32,
    ];
    ensure!(
        scene.hit_test(p[0] as f32, p[1] as f32) == Some(target),
        "target center clipped/covered"
    );
    Ok(p)
}
fn packed(x: i32, y: i32) -> LPARAM {
    LPARAM((((y as u32 & 0xffff) << 16) | (x as u32 & 0xffff)) as isize)
}
fn move_to(hwnd: HWND, p: [i32; 2]) -> Result<()> {
    let p = pixels(hwnd, p);
    unsafe {
        // Inject the actual window message without moving the user's cursor.
        // sample() completes the production paint before queued mouse-leave or
        // external input can supersede it. Natural queue latency is out of scope.
        SendMessageW(
            hwnd,
            WM_MOUSEMOVE,
            Some(WPARAM(0)),
            Some(packed(p[0], p[1])),
        );
    }
    Ok(())
}
fn pixels(hwnd: HWND, p: [i32; 2]) -> [i32; 2] {
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.;
    [
        (p[0] as f32 * scale).round() as i32,
        (p[1] as f32 * scale).round() as i32,
    ]
}
fn click(hwnd: HWND, p: [i32; 2]) -> Result<()> {
    move_to(hwnd, p)?;
    let p = pixels(hwnd, p);
    unsafe {
        SendMessageW(
            hwnd,
            WM_LBUTTONDOWN,
            Some(WPARAM(1)),
            Some(packed(p[0], p[1])),
        );
        SendMessageW(
            hwnd,
            WM_LBUTTONUP,
            Some(WPARAM(0)),
            Some(packed(p[0], p[1])),
        );
    }
    Ok(())
}
fn wheel(hwnd: HWND, p: [i32; 2], delta: i16) -> Result<()> {
    let p = pixels(hwnd, p);
    let mut p = POINT { x: p[0], y: p[1] };
    unsafe {
        ensure!(
            ClientToScreen(hwnd, &mut p).as_bool(),
            "wheel coordinate conversion failed"
        );
        SendMessageW(
            hwnd,
            WM_MOUSEWHEEL,
            Some(WPARAM((delta as u16 as usize) << 16)),
            Some(packed(p.x, p.y)),
        );
    }
    Ok(())
}
fn process_cpu_seconds() -> Result<f64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )?;
    }
    let ticks = |v: FILETIME| (u64::from(v.dwHighDateTime) << 32) | u64::from(v.dwLowDateTime);
    Ok((ticks(kernel) + ticks(user)) as f64 / 10_000_000.)
}
fn percentile(values: &mut [u64], fraction: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[((values.len() as f64 * fraction).ceil() as usize).saturating_sub(1)])
}
fn summary(frames: &[Value]) -> Value {
    let keys = [
        "snapshot_us",
        "project_us",
        "scene_us",
        "paint_us",
        "publish_us",
        "frame_us",
    ];
    let mut result = json!({"frames":frames.len(),"rebuilt":frames.iter().filter(|f|f["rebuilt"]==true).count()});
    for key in keys.into_iter().chain([
        "scene_geometry_us",
        "text_measure_us",
        "draw_us",
        "present_us",
        "layout_creations",
        "format_creations",
    ]) {
        let mut values: Vec<_> = frames
            .iter()
            .filter_map(|f| {
                f.get(key)
                    .or_else(|| f["phases"].get(key))
                    .and_then(Value::as_u64)
            })
            .collect();
        let max = values.iter().max().copied();
        let p50 = percentile(&mut values, 0.50);
        let p95 = percentile(&mut values, 0.95);
        result[key] = json!({"n":values.len(),"p50":p50,"p95":p95,"max":max});
    }
    result
}

fn measure(output: &Path, report: &mut Value) -> Result<()> {
    ensure!(
        !cfg!(debug_assertions),
        "must use the same release profile for baseline/fix"
    );
    ensure!(
        std::env::var_os("QB_DASHBOARD_PROFILE").is_some(),
        "QB_DASHBOARD_PROFILE=1 required"
    );
    let mode = std::env::var("R1210_MODE").unwrap_or_else(|_| "fixed".into());
    ensure!(
        matches!(mode.as_str(), "baseline" | "fixed"),
        "R1210_MODE must be baseline or fixed"
    );
    report["mode"] = json!(mode);
    let raw = fixture::input();
    let bytes = serde_json::to_vec(&raw)?;
    report["fixture"] = fixture::manifest(&bytes);
    std::fs::write(output.join("fixture.json"), &bytes)?;
    let snapshot = Arc::new(fixture::snapshot(&raw));
    let mut cursor = POINT::default();
    unsafe {
        GetCursorPos(&mut cursor)?;
    }
    unsafe {
        ensure!(
            !SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_invalid(),
            "DPI context failed"
        );
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }
    let (send, inbox) = mpsc::sync_channel(256);
    let bridge_fault = Arc::new(AtomicU8::new(0));
    let started = Instant::now();
    let mut run = Run {
        inbox,
        port: None,
        started,
        deadline: started + Duration::from_secs(600),
        read_commands: vec![],
        bridge_fault: bridge_fault.clone(),
    };
    // Locals drop in reverse declaration order. Close the HWND while Run still
    // owns the receiver: shutdown synchronously enqueues GeometryDirty/Closed,
    // including when a measurement returns early with an error.
    let _cleanup = Cleanup { cursor };
    dashboard_window::open_or_focus(
        HWND::default(),
        Arc::new(FakeBridge {
            sender: send,
            fault: bridge_fault,
        }),
        snapshot,
        fixture::clock(),
        false,
    )?;
    run.pump_for(Duration::from_millis(100))?;
    let hwnd = owned_dashboard()?;
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    unsafe {
        let mut c = RECT::default();
        let mut w = RECT::default();
        GetClientRect(hwnd, &mut c)?;
        GetWindowRect(hwnd, &mut w)?;
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            (1320. * dpi as f32 / 96.).round() as i32 + w.right - w.left - c.right,
            (960. * dpi as f32 / 96.).round() as i32 + w.bottom - w.top - c.bottom,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
        )?;
        ensure!(
            IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool(),
            "dashboard must remain visible"
        );
    }
    run.pump_for(Duration::from_secs(2))?;
    let warmup = dashboard_window::perf_take_frames(hwnd);
    report["warmup_frames"] = serde_json::to_value(warmup)?;
    report["cold_first_full_scene"] = report["warmup_frames"]
        .as_array()
        .and_then(|a| a.iter().find(|f| f["rebuilt"] == true))
        .cloned()
        .unwrap_or(Value::Null);
    ensure!(
        !report["cold_first_full_scene"].is_null(),
        "no cold full-scene timing"
    );
    let mut actual = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut actual)?;
    }
    let actual_dpi = unsafe { GetDpiForWindow(hwnd) };
    ensure!(actual_dpi > 0, "invalid readback DPI");
    let actual_pixels = [actual.right - actual.left, actual.bottom - actual.top];
    let requested_pixels = [
        (1320. * actual_dpi as f32 / 96.).round() as i32,
        (960. * actual_dpi as f32 / 96.).round() as i32,
    ];
    let actual_dip = [
        actual_pixels[0] as f32 * 96. / actual_dpi as f32,
        actual_pixels[1] as f32 * 96. / actual_dpi as f32,
    ];
    std::fs::write(
        output.join("warmup.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    report["window"] = json!({"pid":std::process::id(),"hwnd":hwnd.0 as usize,
        "dpi":actual_dpi,"requested_client_dip":[1320,960],
        "requested_client_pixels":requested_pixels,"actual_client_pixels":actual_pixels,
        "client_dip":actual_dip,"theme":"light","client_bounds_verified":actual_pixels==requested_pixels});
    ensure!(
        actual_pixels == requested_pixels,
        "actual client viewport differs from requested workload"
    );
    let _ = scene(hwnd)?; // Outside CPU interval: ensure production frame is available.
    let cpu_start = process_cpu_seconds()?;
    let idle_start = Instant::now();
    run.pump_for(Duration::from_secs(30))?;
    let elapsed = idle_start.elapsed().as_secs_f64();
    let cpu = process_cpu_seconds()? - cpu_start;
    let idle_frames = serde_json::to_value(dashboard_window::perf_take_frames(hwnd))?;
    report["idle"] = json!({"elapsed_seconds":elapsed,"cpu_seconds":cpu,
        "one_core_cpu_percent":100.*cpu/elapsed,"frames":idle_frames,"timer_running":true});
    std::fs::write(
        output.join("idle.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    report["actions"] = json!({"hover":[],"scroll":[],"tabs":[]});
    for i in 0..100 {
        let cached = scene(hwnd)?;
        let target = Target::Tab(if i % 2 == 0 { Tab::Usage } else { Tab::Events });
        let p = point(&cached, &target)?;
        let expected = ExpectedState::Hover {
            tab: selected_tab(&cached)?,
            target: format!("{target:?}"),
        };
        let mut sample = run.sample(hwnd, expected, || {
            move_to(hwnd, p)?;
            Ok(())
        })?;
        sample["index"] = json!(i);
        report["actions"]["hover"]
            .as_array_mut()
            .unwrap()
            .push(sample);
    }
    for i in 0..100 {
        let cached = scene(hwnd)?;
        let region = cached
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Table)
            .context("table scroll region missing")?;
        ensure!(region.max_offset >= 96., "fixture does not scroll");
        let p = [
            (region.viewport.left + 30.) as i32,
            (region.viewport.top + 30.) as i32,
        ];
        let before = region.offset;
        let expected_offset =
            (before + if i % 2 == 0 { 96. } else { -96. }).clamp(0., region.max_offset);
        let expected = ExpectedState::Scroll {
            tab: selected_tab(&cached)?,
            offset: expected_offset,
        };
        let mut sample = run.sample(hwnd, expected, || {
            wheel(hwnd, p, if i % 2 == 0 { -120 } else { 120 })
        })?;
        let after = scene(hwnd)?
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Table)
            .context("post-scroll table missing")?
            .offset;
        ensure!((after - before).abs() > 1., "wheel was clamped/no-op");
        sample["index"] = json!(i);
        sample["offsets"] = json!([before, after]);
        report["actions"]["scroll"]
            .as_array_mut()
            .unwrap()
            .push(sample);
    }
    for i in 0..100 {
        let tab = [Tab::Usage, Tab::Events, Tab::Overview][i % 3];
        let cached = scene(hwnd)?;
        let p = point(&cached, &Target::Tab(tab))?;
        let mut sample = run.sample(hwnd, ExpectedState::Tab(format!("{tab:?}")), || {
            click(hwnd, p)
        })?;
        ensure!(
            scene(hwnd)?
                .accessibility
                .iter()
                .any(|n| n.target == Some(Target::Tab(tab)) && n.selected),
            "tab input did not select requested tab"
        );
        sample["index"] = json!(i);
        sample["to_tab"] = json!(format!("{tab:?}"));
        report["actions"]["tabs"]
            .as_array_mut()
            .unwrap()
            .push(sample);
    }
    let mut warm_full = vec![];
    report["phase_stats"] = json!({});
    report["matched_frame_phase_stats"] = json!({});
    report["input_latency_us"] = json!({});
    report["rebuild_causes"] = json!({});
    for kind in ["hover", "scroll", "tabs"] {
        let samples = report["actions"][kind].as_array().unwrap();
        ensure!(samples.len() == 100, "incomplete action workload");
        let frames: Vec<Value> = samples
            .iter()
            .flat_map(|s| s["frames"].as_array().unwrap().iter().cloned())
            .collect();
        let matched: Vec<Value> = samples
            .iter()
            .map(|sample| {
                let index = sample["matched_frame_index"].as_u64().unwrap() as usize;
                sample["frames"][index].clone()
            })
            .collect();
        let latency = latency_summary(samples);
        ensure!(latency["n"] == 100, "missing input-latency samples");
        let matched_stats = summary(&matched);
        let causes = cause_counts(&frames);
        warm_full.extend(frames.iter().filter(|f| f["rebuilt"] == true).cloned());
        report["phase_stats"][kind] = summary(&frames);
        report["matched_frame_phase_stats"][kind] = matched_stats;
        report["input_latency_us"][kind] = latency;
        report["rebuild_causes"][kind] = causes;
    }
    report["warm_full_scene"] = summary(&warm_full);
    report["read_commands"] = json!(run.read_commands);
    ensure!(!warm_full.is_empty(), "no warm full-scene samples");
    let mut violations = vec![];
    if 100. * cpu / elapsed >= 2. {
        violations.push("idle CPU >=2% of one core");
    }
    for kind in ["hover", "scroll", "tabs"] {
        if report["matched_frame_phase_stats"][kind]["frame_us"]["p95"]
            .as_u64()
            .context("missing frame_us timing")?
            >= 16_000
        {
            violations.push(match kind {
                "hover" => "hover p95 >=16ms",
                "scroll" => "scroll p95 >=16ms",
                _ => "tab p95 >=16ms",
            });
        }
    }
    for kind in ["hover", "scroll", "tabs"] {
        if report["input_latency_us"][kind]["p95"]
            .as_u64()
            .context("missing observed input latency")?
            >= 16_000
        {
            violations.push(match kind {
                "hover" => "hover input-to-observed-frame p95 >=16ms",
                "scroll" => "scroll input-to-observed-frame p95 >=16ms",
                _ => "tab input-to-observed-frame p95 >=16ms",
            });
        }
    }
    if report["warm_full_scene"]["frame_us"]["p95"]
        .as_u64()
        .unwrap()
        >= 50_000
    {
        violations.push("warm full rebuild p95 >=50ms");
    }
    let hover_frames: Vec<_> = report["actions"]["hover"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["frames"].as_array().unwrap().iter())
        .collect();
    let hover_unattributed_rebuilds = hover_frames
        .iter()
        .filter(|f| f["rebuilt"] == true && !independent_rebuild(f))
        .count();
    report["hover_state_or_unknown_rebuilds"] = json!(hover_unattributed_rebuilds);
    if hover_unattributed_rebuilds != 0 {
        violations.push("hover rebuilt for state/unknown cause; clock/data separately attributed");
    }
    report["acceptance"] = json!({"violations":violations,"enforced":mode=="fixed"});
    ensure!(
        mode == "baseline" || violations.is_empty(),
        "fixed acceptance thresholds failed"
    );
    drop(_cleanup);
    // Surface a shutdown callback failure only on the success path. An earlier
    // measurement error returns unchanged while Cleanup runs without panicking.
    run.service()?;
    ensure!(
        !unsafe { IsWindow(Some(hwnd)) }.as_bool(),
        "owned HWND survived cleanup"
    );
    report["window_closed"] = json!(true);
    Ok(())
}

#[test]
#[ignore = "real HWND release-profile measurement; coordinator runner only"]
fn r1210_dashboard_perf() {
    let output = PathBuf::from(std::env::var_os("R1210_OUTPUT").expect("R1210_OUTPUT"));
    assert!(
        output.is_absolute() && output.is_dir(),
        "existing absolute output directory required"
    );
    let sandbox = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.sandbox")
        .canonicalize()
        .unwrap();
    let output = output.canonicalize().unwrap();
    assert!(
        output.starts_with(&sandbox),
        "output must stay inside impl .sandbox"
    );
    assert!(
        !output.join("completion.json").exists(),
        "use a fresh output directory"
    );
    let mut report = json!({"schema":2,"status":"running","profile":"release",
        "workload":"r1210-perf-v1","measurement":"r1210-sync-production-paint-v3","idle_seconds":30,"actions_per_kind":100,
        "input_latency_kind":"synchronous HWND input through UpdateWindow production paint; excludes OS input queue latency",
        "marker_encoding":{"tab_hover":"Host Debug strings","offsets":["Table","Side","Events","Usage"]},
        "cause_limit":"Host v2 uses timer/mail/state; raw labels retained. During timed actions this harness publishes only clock mail, never snapshot/detail/theme mail.",
        "scope_limits":["single light desktop viewport; no physical display completion timestamp",
            "synchronous injected HWND messages; not an end-to-end physical mouse latency measurement",
            "cold first scene reported separately; threshold uses warm full rebuilds",
            "timer no-visible-label-change rebuild regression needs controlled clock test"]});
    let result = measure(&output, &mut report);
    report["status"] = json!(if result.is_ok() {
        "completed"
    } else {
        "failed"
    });
    report["error"] = result
        .as_ref()
        .err()
        .map(|e| json!(format!("{e:#}")))
        .unwrap_or(Value::Null);
    let temporary = output.join("completion.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    std::fs::rename(temporary, output.join("completion.json")).unwrap();
    result.unwrap();
}
