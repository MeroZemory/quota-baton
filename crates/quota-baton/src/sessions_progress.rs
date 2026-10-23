//! Scoped move progress. Signals request cancellation; journal boundaries own it.
use anyhow::Result;
use std::{
    cell::RefCell,
    io::{IsTerminal, Write},
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Searching,
    Header,
    Checking,
    Preview,
    Confirmation,
    Linking,
    Records,
    Starting,
}
impl Phase {
    fn name(self, english: bool) -> &'static str {
        match (self, english) {
            (Self::Searching, false) => "홈 검색",
            (Self::Header, false) => "대화 머리말 읽기",
            (Self::Checking, false) => "조상과 잠금 확인",
            (Self::Preview, false) => "미리보기 준비",
            (Self::Confirmation, false) => "move 입력 대기",
            (Self::Linking, false) => "파일 연결",
            (Self::Records, false) => "대화 항목 기록",
            (Self::Starting, false) => "Codex 시작",
            (Self::Searching, true) => "searching homes",
            (Self::Header, true) => "reading conversation header",
            (Self::Checking, true) => "checking ancestors and locks",
            (Self::Preview, true) => "building preview",
            (Self::Confirmation, true) => "waiting for move",
            (Self::Linking, true) => "linking files",
            (Self::Records, true) => "writing records",
            (Self::Starting, true) => "starting Codex",
        }
    }
}

struct State {
    phase: Phase,
    changed: Instant,
    bytes: Option<u64>,
    commit: bool,
    mutated: bool,
    stopped: bool,
    reported: bool,
}
impl State {
    fn update_phase(&mut self, phase: Phase, bytes: Option<u64>, now: Instant) -> bool {
        let report = !self.reported
            || self.phase != phase
            || now.duration_since(self.changed) >= PROGRESS_INTERVAL;
        self.phase = phase;
        self.bytes = bytes;
        if report {
            self.changed = now;
            self.reported = true;
        }
        report
    }
}
struct Inner {
    english: bool,
    start: Instant,
    state: Mutex<State>,
    wake: Condvar,
    interrupts: AtomicUsize,
    cancel: AtomicBool,
    verbose: AtomicBool,
    terminal: bool,
}
impl Inner {
    fn request_cancel(&self) {
        self.interrupts.fetch_add(1, Ordering::AcqRel);
        self.cancel.store(true, Ordering::Release);
        self.wake.notify_all();
    }
}
thread_local! { static CURRENT: RefCell<Option<Arc<Inner>>> = const { RefCell::new(None) }; }
static ACTIVE: Mutex<Option<Weak<Inner>>> = Mutex::new(None);

#[derive(Debug)]
pub(crate) struct Interrupted {
    pub(crate) changed: bool,
    english: bool,
}
impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match (self.changed, self.english) {
            (false, false) => "취소했습니다. 아무것도 바뀌지 않았습니다.",
            (false, true) => "Cancelled. Nothing was changed.",
            (true, false) => "안전한 단계에서 멈췄습니다. 다음 실행에서 이동 복구를 선택해 주세요.",
            (true, true) => "Stopped at a safe boundary. Choose move recovery on the next run.",
        })
    }
}
impl std::error::Error for Interrupted {}

pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    error.downcast_ref::<Interrupted>().map(ToString::to_string)
}

fn current() -> Option<Arc<Inner>> {
    CURRENT.with(|c| c.borrow().clone())
}
fn write_line(inner: &Inner, line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = write_progress(&mut stderr, inner.terminal, line);
    let _ = stderr.flush();
}
fn write_progress(writer: &mut impl Write, terminal: bool, line: &str) -> std::io::Result<()> {
    if terminal {
        write!(writer, "\r\x1b[2K{line}")
    } else {
        writeln!(writer, "{line}")
    }
}
fn pending_warnings(inner: &Inner, state: &State, seen: &mut usize) -> usize {
    let count = inner.interrupts.load(Ordering::Acquire);
    let pending = count.wrapping_sub(*seen);
    *seen = count;
    if state.commit { pending } else { 0 }
}
fn render(inner: &Inner, state: &State, heartbeat: bool) -> String {
    let phase = state.phase.name(inner.english);
    let elapsed = inner.start.elapsed().as_secs();
    let size = state
        .bytes
        .map(|b| format!(" · {:.1} MB", b as f64 / 1048576.))
        .unwrap_or_default();
    match (inner.english, heartbeat) {
        (false, false) => format!("{phase}{size} · {elapsed}초"),
        (false, true) => format!("여전히 진행 중: {phase}{size} · {elapsed}초"),
        (true, false) => format!("{phase}{size} · {elapsed}s"),
        (true, true) => format!("Still working: {phase}{size} · {elapsed}s"),
    }
}
fn warning(english: bool) -> &'static str {
    if english {
        "This step must finish safely. Stopping as soon as it finishes."
    } else {
        "지금 단계는 중단하면 안 되는 단계입니다. 끝나는 대로 멈춥니다."
    }
}

pub(crate) struct Operation {
    inner: Option<Arc<Inner>>,
    worker: Option<JoinHandle<()>>,
}
impl Operation {
    pub(crate) fn begin(id: &str, english: bool) -> Result<Self> {
        if current().is_some() {
            return Ok(Self {
                inner: None,
                worker: None,
            });
        }
        let short = id
            .chars()
            .filter(|c| !c.is_control())
            .take(8)
            .collect::<String>();
        let first = if english {
            format!(
                "Preparing to bring conversation {short} into quota-baton. Large conversations can take 1-2 minutes. Keep this window open."
            )
        } else {
            format!(
                "대화 {short} 를 qb로 가져올 준비를 합니다. 큰 대화는 1~2분 걸릴 수 있습니다. 창을 닫지 마세요."
            )
        };
        // Flush before starting a worker, registering signals, or any scan.
        writeln!(std::io::stderr().lock(), "{first}")?;
        std::io::stderr().flush()?;
        install_handler()?;
        let now = Instant::now();
        let inner = Arc::new(Inner {
            english,
            start: now,
            state: Mutex::new(State {
                phase: Phase::Searching,
                changed: now,
                bytes: None,
                commit: false,
                mutated: false,
                stopped: false,
                reported: false,
            }),
            wake: Condvar::new(),
            interrupts: AtomicUsize::new(0),
            cancel: AtomicBool::new(false),
            verbose: AtomicBool::new(false),
            terminal: std::io::stderr().is_terminal(),
        });
        let observed = inner.clone();
        let worker = std::thread::Builder::new()
            .name("qb-move-progress".into())
            .spawn(move || {
                let mut seen = 0;
                let mut state = observed.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if state.stopped {
                        break;
                    }
                    for _ in 0..pending_warnings(&observed, &state, &mut seen) {
                        write_line(&observed, warning(observed.english));
                    }
                    if state.changed.elapsed() >= PROGRESS_INTERVAL {
                        write_line(&observed, &render(&observed, &state, true));
                        state.changed = Instant::now();
                        state.reported = true;
                    }
                    state = observed
                        .wake
                        .wait_timeout(state, Duration::from_millis(100))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            })?;
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&inner));
        CURRENT.with(|c| *c.borrow_mut() = Some(inner.clone()));
        Ok(Self {
            inner: Some(inner),
            worker: Some(worker),
        })
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            CURRENT.with(|c| *c.borrow_mut() = None);
            *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
            inner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .stopped = true;
            inner.wake.notify_all();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            if inner.terminal {
                let _ = writeln!(std::io::stderr().lock());
            }
        }
    }
}

pub(crate) fn phase(phase: Phase, bytes: Option<u64>) -> Result<()> {
    check()?;
    if let Some(inner) = current() {
        let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.update_phase(phase, bytes, Instant::now()) {
            write_line(&inner, &render(&inner, &state, false));
        }
    }
    Ok(())
}
pub(crate) fn english() -> bool {
    current().is_some_and(|inner| inner.english)
}
pub(crate) fn set_verbose(value: bool) {
    if let Some(inner) = current() {
        inner.verbose.store(value, Ordering::Release);
    }
}
pub(crate) fn verbose() -> bool {
    current().is_some_and(|inner| inner.verbose.load(Ordering::Acquire))
}

/// Read terminal confirmation without leaving a blocked stdin worker behind.
/// Native children have not started yet, so this input handle belongs to us.
pub(crate) fn read_line(line: &mut String) -> std::io::Result<usize> {
    #[cfg(windows)]
    if current().is_some() {
        use windows::Win32::{
            Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::{
                Console::{
                    GetStdHandle, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE,
                },
                Threading::WaitForSingleObject,
            },
        };
        let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.map_err(std::io::Error::other)?;
        let mut units = Vec::new();
        loop {
            check().map_err(|_| std::io::Error::from(std::io::ErrorKind::Interrupted))?;
            match unsafe { WaitForSingleObject(input, 100) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => continue,
                WAIT_FAILED => return Err(std::io::Error::last_os_error()),
                _ => return Err(std::io::Error::other("Unexpected console wait result")),
            }
            let mut events = [INPUT_RECORD::default()];
            let mut count = 0;
            unsafe { ReadConsoleInputW(input, &mut events, &mut count) }
                .map_err(std::io::Error::other)?;
            if count == 0 || events[0].EventType != KEY_EVENT as u16 {
                continue;
            }
            let event = unsafe { events[0].Event.KeyEvent };
            if !event.bKeyDown.as_bool() {
                continue;
            }
            let ch = unsafe { event.uChar.UnicodeChar };
            if ch == 13 {
                let _ = writeln!(std::io::stdout().lock());
                let text = String::from_utf16_lossy(&units);
                let length = text.len() + 1;
                line.push_str(&text);
                line.push('\n');
                return Ok(length);
            }
            if ch == 8 {
                if units.pop().is_some() {
                    let _ = write!(std::io::stdout().lock(), "\x08 \x08");
                }
            } else if ch >= 32 && units.len() < 256 {
                for _ in 0..event.wRepeatCount.max(1) {
                    if units.len() == 256 {
                        break;
                    }
                    units.push(ch);
                    let _ = write!(
                        std::io::stdout().lock(),
                        "{}",
                        String::from_utf16_lossy(&[ch])
                    );
                }
            }
            let _ = std::io::stdout().flush();
        }
    }
    use std::io::BufRead;
    std::io::stdin().lock().read_line(line)
}
pub(crate) fn check() -> Result<()> {
    if let Some(inner) = current() {
        let state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if inner.cancel.load(Ordering::Acquire) && !state.commit {
            return Err(Interrupted {
                changed: state.mutated,
                english: inner.english,
            }
            .into());
        }
    }
    Ok(())
}
pub(crate) fn boundary() -> Result<()> {
    if let Some(inner) = current()
        && inner.cancel.load(Ordering::Acquire)
    {
        let state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        // The operation can stop before the progress worker observes the signal.
        if state.mutated {
            write_line(&inner, warning(inner.english));
        }
        return Err(Interrupted {
            changed: state.mutated,
            english: inner.english,
        }
        .into());
    }
    Ok(())
}
pub(crate) struct Commit(Option<Arc<Inner>>);
pub(crate) fn commit() -> Result<Commit> {
    check()?;
    let inner = current();
    if let Some(inner) = &inner {
        let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.commit = true;
        state.mutated = true;
    }
    Ok(Commit(inner))
}
impl Drop for Commit {
    fn drop(&mut self) {
        if let Some(inner) = &self.0 {
            inner.state.lock().unwrap_or_else(|e| e.into_inner()).commit = false;
        }
    }
}
#[cfg(windows)]
unsafe extern "system" fn interrupt(kind: u32) -> windows::core::BOOL {
    use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    if matches!(kind, CTRL_C_EVENT | CTRL_BREAK_EVENT)
        && let Ok(active) = ACTIVE.lock()
        && let Some(inner) = active.as_ref().and_then(Weak::upgrade)
    {
        inner.request_cancel();
        return true.into();
    }
    false.into()
}
fn install_handler() -> Result<()> {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    let ok = *REGISTERED.get_or_init(|| {
        #[cfg(windows)]
        {
            unsafe {
                windows::Win32::System::Console::SetConsoleCtrlHandler(Some(interrupt), true)
                    .is_ok()
            }
        }
        #[cfg(not(windows))]
        {
            true
        }
    });
    anyhow::ensure!(ok, "이동 중단 처리를 준비하지 못했습니다");
    Ok(())
}

/// Thread-local cancellation scope without a worker, console handler, or output.
/// Keep the guard alive while exercising move code; dropping it restores the prior scope.
#[cfg(test)]
pub(crate) struct TestCancellation {
    inner: Arc<Inner>,
    previous: Option<Arc<Inner>>,
    // CURRENT must be restored on the thread that installed this scope.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(test)]
pub(crate) fn test_cancellation(english: bool) -> TestCancellation {
    let now = Instant::now();
    let inner = Arc::new(Inner {
        english,
        start: now,
        state: Mutex::new(State {
            phase: Phase::Searching,
            changed: now,
            bytes: None,
            commit: false,
            mutated: false,
            stopped: false,
            reported: false,
        }),
        wake: Condvar::new(),
        interrupts: AtomicUsize::new(0),
        cancel: AtomicBool::new(false),
        verbose: AtomicBool::new(false),
        terminal: false,
    });
    let previous = CURRENT.with(|c| c.replace(Some(inner.clone())));
    TestCancellation {
        inner,
        previous,
        _thread: std::marker::PhantomData,
    }
}

/// Install an English test scope for same-thread move checkpoint hooks.
#[cfg(test)]
pub(crate) fn test_scope() -> TestCancellation {
    test_cancellation(true)
}

/// May be passed directly to a `'static` hook; the hook must run on the scope's thread.
#[cfg(test)]
pub(crate) fn test_request_cancel() {
    current()
        .expect("test cancellation scope must be active")
        .request_cancel();
}

#[cfg(test)]
impl TestCancellation {
    pub(crate) fn request_cancel(&self) {
        self.inner.request_cancel();
    }
}

#[cfg(test)]
impl Drop for TestCancellation {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.previous.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precommit_cancel_leaves_everything_unchanged() {
        let cancellation = test_cancellation(true);
        cancellation.request_cancel();
        let error = check().unwrap_err();
        assert!(!error.downcast_ref::<Interrupted>().unwrap().changed);
        assert_eq!(
            friendly_error(&error).as_deref(),
            Some("Cancelled. Nothing was changed.")
        );
        assert!(commit().is_err());
        assert!(phase(Phase::Header, None).is_err());
        assert!(!cancellation.inner.state.lock().unwrap().mutated);
    }

    #[test]
    fn commit_defers_checks_but_boundary_stops() {
        let cancellation = test_cancellation(true);
        let guard = commit().unwrap();
        cancellation.request_cancel();
        check().unwrap();
        let error = boundary().unwrap_err();
        assert!(error.downcast_ref::<Interrupted>().unwrap().changed);
        assert_eq!(
            friendly_error(&error).as_deref(),
            Some("Stopped at a safe boundary. Choose move recovery on the next run.")
        );
        drop(guard);
        assert!(check().is_err());
    }

    #[test]
    fn repeated_commit_interrupts_each_warn_once() {
        let cancellation = test_cancellation(true);
        let _guard = commit().unwrap();
        let mut seen = 0;
        cancellation.request_cancel();
        cancellation.request_cancel();
        let state = cancellation.inner.state.lock().unwrap();
        assert_eq!(pending_warnings(&cancellation.inner, &state, &mut seen), 2);
        assert_eq!(pending_warnings(&cancellation.inner, &state, &mut seen), 0);
        drop(state);
        cancellation.request_cancel();
        let state = cancellation.inner.state.lock().unwrap();
        assert_eq!(pending_warnings(&cancellation.inner, &state, &mut seen), 1);
        assert_eq!(
            warning(true),
            "This step must finish safely. Stopping as soon as it finishes."
        );
    }

    #[test]
    fn nonterminal_progress_uses_plain_complete_lines() {
        let mut output = Vec::new();
        write_progress(
            &mut output,
            false,
            "reading conversation header · 1.0 MB · 5s",
        )
        .unwrap();
        write_progress(&mut output, false, warning(true)).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "reading conversation header · 1.0 MB · 5s\nThis step must finish safely. Stopping as soon as it finishes.\n"
        );
        let mut terminal = Vec::new();
        write_progress(&mut terminal, true, "progress").unwrap();
        assert_eq!(terminal, b"\r\x1b[2Kprogress");
    }

    #[test]
    fn repeated_headers_are_throttled_without_losing_latest_size() {
        let cancellation = test_cancellation(true);
        let mut state = cancellation.inner.state.lock().unwrap();
        let now = state.changed;
        assert!(state.update_phase(Phase::Header, Some(0), now));
        for bytes in 1..10_000 {
            assert!(!state.update_phase(Phase::Header, Some(bytes), now + Duration::from_secs(4)));
        }
        assert_eq!(state.bytes, Some(9_999));
        assert_eq!(state.changed, now);
        assert!(state.update_phase(Phase::Header, Some(10_000), now + PROGRESS_INTERVAL));
        assert!(!state.update_phase(Phase::Header, None, now + PROGRESS_INTERVAL));
        assert!(state.update_phase(Phase::Checking, None, now + PROGRESS_INTERVAL));
    }

    #[test]
    fn cancellation_scope_restores_previous_scope() {
        let outer = test_cancellation(true);
        {
            let inner = test_cancellation(false);
            inner.request_cancel();
            assert!(check().is_err());
            assert!(!english());
        }
        check().unwrap();
        assert!(english());
        outer.request_cancel();
        assert!(check().is_err());
    }

    #[test]
    fn static_hook_cancels_current_scope() {
        let _scope = test_scope();
        let hook: Box<dyn Fn() + Send + Sync + 'static> = Box::new(test_request_cancel);
        check().unwrap();
        hook();
        assert!(check().is_err());
    }
}
