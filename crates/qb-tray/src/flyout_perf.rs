//! Opt-in TESTKIT flyout timings. No cache or input policy changes.
use qb_tray::v2_text::TextPhaseStats;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::Write,
    sync::{
        OnceLock,
        mpsc::{SyncSender, sync_channel},
    },
    time::Instant,
};

static LOG: OnceLock<SyncSender<Value>> = OnceLock::new();
#[derive(Default)]
struct Frame {
    start: Option<Instant>,
    end_draw: Option<Instant>,
    phases: BTreeMap<&'static str, u128>,
    wheels: Vec<(u64, Instant, f32, f32)>,
    received: u64,
}
thread_local! { static FRAME: RefCell<Frame> = RefCell::new(Frame::default()); }

pub fn init() -> anyhow::Result<()> {
    if std::env::var_os("QB_TRAY_TEST_FLYOUT_PERF").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Ok(());
    }
    // The direct-fixture route has no WORKER. Use one bounded off-thread writer
    // with the existing shell-log destination; never open a log on the UI thread.
    let path = std::path::PathBuf::from(
        std::env::var_os("QB_TRAY_TEST_SHELL_LOG")
            .ok_or_else(|| anyhow::anyhow!("perf shell log required"))?,
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("perf log parent required"))?
        .canonicalize()?;
    anyhow::ensure!(
        parent.components().any(|c| c.as_os_str() == ".sandbox"),
        "perf log must be in .sandbox"
    );
    let path = parent.join(
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("perf log filename required"))?,
    );
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    let (tx, rx) = sync_channel::<Value>(512);
    std::thread::Builder::new()
        .name("flyout-perf-log".into())
        .spawn(move || {
            while let Ok(value) = rx.recv() {
                writeln!(file, "{value}").expect("perf log write failed");
                file.flush().expect("perf log flush failed");
            }
        })?;
    LOG.set(tx)
        .map_err(|_| anyhow::anyhow!("perf logger already initialized"))?;
    emit(
        json!({"event":"perf_start", "schema":1, "clock":"Instant", "units":"microseconds", "os_queue_included":false}),
    );
    Ok(())
}
pub fn enabled() -> bool {
    LOG.get().is_some()
}
pub fn start() -> Option<Instant> {
    enabled().then(Instant::now)
}
fn emit(value: Value) {
    if let Some(tx) = LOG.get() {
        tx.try_send(value)
            .expect("perf log overflow or writer failure");
    }
}
pub fn begin_frame() {
    if !enabled() {
        return;
    }
    FRAME.with(|f| {
        let mut f = f.borrow_mut();
        f.start = Some(Instant::now());
        f.end_draw = None;
        f.phases.clear();
    });
}
pub fn phase(name: &'static str, at: Option<Instant>) {
    if let Some(at) = at {
        FRAME.with(|f| {
            f.borrow_mut().phases.insert(name, at.elapsed().as_micros());
        });
    }
}
pub fn wheel(at: Option<Instant>, delta: f32, applied: f32, max_offset: f32) {
    let Some(at) = at else {
        return;
    };
    FRAME.with(|f| {
        let mut f = f.borrow_mut();
        f.received += 1;
        let id = f.received;
        assert!(f.wheels.len() < 128, "unpublished perf wheel bound");
        f.wheels.push((id, at, delta, applied));
        emit(json!({"event":"wheel_received", "id":id, "delta":delta,
            "applied_offset":applied, "max_offset":max_offset}));
    });
}
pub fn end_draw() {
    if enabled() {
        FRAME.with(|f| f.borrow_mut().end_draw = Some(Instant::now()));
    }
}
pub fn publish(
    epoch: usize,
    offset: f32,
    max_offset: f32,
    dpi: u32,
    pixels: (u32, u32),
    text: TextPhaseStats,
) {
    if !enabled() {
        return;
    }
    // Capture publication endpoint before formatting or scheduling the log.
    let published = Instant::now();
    FRAME.with(|f| {
        let mut f = f.borrow_mut();
        let end = f.end_draw.expect("successful EndDraw required");
        let wheels: Vec<_> = f.wheels.drain(..).map(|(id, at, delta, applied)| json!({
            "id":id, "delta":delta, "applied_offset":applied,
            "end_draw_us":end.duration_since(at).as_micros(),
            "publish_us":published.duration_since(at).as_micros()
        })).collect();
        emit(json!({"event":"perf_paint", "epoch":epoch, "offset":offset,
            "max_offset":max_offset, "dpi":dpi, "pixels":pixels,
            "received":f.received, "wheels":wheels, "phases_us":f.phases,
            "paint_publish_us":published.duration_since(f.start.expect("frame start required")).as_micros(),
            "text":text}));
    });
}
