//! Isolated service protocol tests; no process environment mutation.
use super::*;

#[test]
fn intent_is_once_and_only_successful_drain_authorizes_reload() {
    let intent = ReloadIntent::new();
    assert!(!intent.ready());
    assert!(intent.claim());
    assert!(!intent.claim());
    assert!(!intent.ready());
    intent.finish(true);
    assert!(intent.ready());
    assert!(!intent.claim());
}

#[test]
fn failed_drain_never_authorizes_exec_or_a_second_attempt() {
    let intent = ReloadIntent::new();
    assert!(intent.claim());
    intent.finish(false);
    assert!(intent.failed());
    assert!(!intent.ready());
    assert!(!intent.claim());
    // A stale successful completion cannot reverse failure.
    intent.finish(true);
    assert!(intent.failed());
    assert!(!intent.ready());
}

#[test]
fn term_before_during_or_after_drain_always_cancels_intent() {
    for phase in [IDLE, DRAINING, DRAINED, FAILED] {
        let intent = ReloadIntent(AtomicU8::new(phase));
        intent.stop();
        intent.finish(true);
        assert!(!intent.ready());
        assert!(!intent.claim());
        assert_ne!(intent.0.load(Ordering::Acquire) & STOPPING, 0);
    }
}

#[test]
fn drain_failure_remains_visible_before_or_after_term() {
    for stop_first in [false, true] {
        let intent = ReloadIntent::new();
        assert!(intent.claim());
        if stop_first {
            intent.stop();
        }
        intent.finish(false);
        intent.stop();
        assert!(intent.failed());
        assert!(!intent.ready());
        assert!(!intent.claim());
    }
}

#[test]
fn concurrent_claim_has_exactly_one_winner() {
    let intent = Arc::new(ReloadIntent::new());
    let winners = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let intent = intent.clone();
                scope.spawn(move || intent.claim())
            })
            .collect();
        threads
            .into_iter()
            .map(|task| task.join().unwrap())
            .filter(|won| *won)
            .count()
    });
    assert_eq!(winners, 1);
}

#[test]
fn stop_racing_drain_completion_cannot_be_overwritten() {
    let intent = ReloadIntent::new();
    assert!(intent.claim());
    std::thread::scope(|scope| {
        scope.spawn(|| intent.stop());
        scope.spawn(|| intent.finish(true));
    });
    assert!(!intent.ready());
    assert!(!intent.claim());
}

fn generation(pid: u32, started_at: &str) -> HealthResponse {
    HealthResponse {
        ok: true,
        version: "synthetic".into(),
        pid,
        started_at: started_at.into(),
    }
}

#[test]
fn readiness_requires_different_generation_and_same_pid() {
    let old = generation(123, "2026-10-25T00:00:00.000001Z");
    assert!(!successor(&old, &old).unwrap());
    assert!(successor(&old, &generation(123, "2026-10-25T00:00:00.000002Z")).unwrap());
    assert!(successor(&old, &generation(124, &old.started_at)).is_err());
    assert!(successor(&old, &generation(124, "2026-10-25T00:00:01Z")).is_err());
}

#[test]
fn only_managed_server_invocation_qualifies_for_signal_preparation() {
    let args = |items: &[&str]| {
        items
            .iter()
            .map(|item| OsString::from(*item))
            .collect::<Vec<_>>()
    };
    assert!(server_invocation(&args(&[
        "/usr/local/bin/quota-baton",
        "serve"
    ])));
    assert!(!server_invocation(&args(&[
        "/usr/local/bin/quota-baton",
        "serve",
        "--takeover"
    ])));
    assert!(!server_invocation(&args(&[
        "/usr/local/bin/quota-baton",
        "status"
    ])));
    assert!(!server_invocation(&args(&[])));
}

#[test]
fn capture_keeps_installed_name_and_exact_serve_arguments() {
    let args: Vec<OsString> = [
        "/usr/local/bin/quota-baton",
        "serve",
        "--capture-limit",
        "7",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let process = capture_process(args.clone()).unwrap();
    assert_eq!(process.executable, Path::new("/usr/local/bin/quota-baton"));
    assert_eq!(process.args, args[1..]);
    for executable in ["quota-baton", "./quota-baton", "/usr/../bin/quota-baton"] {
        assert!(capture_process(vec![executable.into(), "serve".into()]).is_err());
    }
}

#[test]
fn notify_address_encodes_pathname_and_abstract_without_extra_nul() {
    let (path, length) = notify_address(OsStr::new("/run/qb-notify")).unwrap();
    let offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    assert_eq!(length as usize, offset + b"/run/qb-notify".len() + 1);
    assert_eq!(path.sun_path[0] as u8, b'/');
    assert_eq!(path.sun_path[b"/run/qb-notify".len()], 0);
    let (abstract_address, length) = notify_address(OsStr::new("@qb-notify")).unwrap();
    assert_eq!(length as usize, offset + b"@qb-notify".len());
    assert_eq!(abstract_address.sun_path[0], 0);
    assert_eq!(abstract_address.sun_path[1] as u8, b'q');
    for invalid in ["", "@", "relative", "/bad\0path"] {
        assert!(notify_address(OsStr::new(invalid)).is_err());
    }
    assert!(notify_address(OsStr::new(&format!("/{}", "x".repeat(200)))).is_err());
}

#[test]
fn signal_set_covers_both_stop_signals_only() {
    let signals = stop_signals();
    unsafe {
        assert_eq!(libc::sigismember(&signals, libc::SIGTERM), 1);
        assert_eq!(libc::sigismember(&signals, libc::SIGINT), 1);
        assert_eq!(libc::sigismember(&signals, libc::SIGHUP), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn durable_cleanup_finishes_after_deadline_before_failure_is_reported() {
    let failed = AtomicBool::new(false);
    let completed = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(1);
    let cleanup = async {
        sleep(Duration::from_secs(2)).await;
        completed.store(true, Ordering::Release);
        Ok(())
    };
    await_durable_cleanup(cleanup, deadline, &failed)
        .await
        .unwrap();
    assert!(completed.load(Ordering::Acquire));
    assert!(failed.load(Ordering::Acquire));
}

#[tokio::test]
async fn durable_cleanup_error_is_latched_after_cleanup_returns() {
    let failed = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(45);
    assert!(
        await_durable_cleanup(
            async { bail!("synthetic cleanup failure") },
            deadline,
            &failed
        )
        .await
        .is_err()
    );
    assert!(failed.load(Ordering::Acquire));
}

#[tokio::test(start_paused = true)]
async fn graceful_parent_future_is_awaited_past_deadline() {
    let failed = AtomicBool::new(false);
    let completed = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(1);
    let clean = await_connection_drain(
        async {
            sleep(Duration::from_secs(2)).await;
            completed.store(true, Ordering::Release);
            true
        },
        deadline,
        &failed,
    )
    .await;
    assert!(!clean, "late completion must still fail exec eligibility");
    assert!(
        completed.load(Ordering::Acquire),
        "parent future was abandoned"
    );
    assert!(failed.load(Ordering::Acquire));
}

#[tokio::test]
async fn terminal_drain_handle_survives_a_cancelled_settlement_waiter() {
    let (release, released) = tokio::sync::oneshot::channel();
    let completed = Arc::new(AtomicBool::new(false));
    let marker = completed.clone();
    let slot = tokio::sync::Mutex::new(Some(tokio::spawn(async move {
        released.await.unwrap();
        marker.store(true, Ordering::Release);
    })));
    {
        let settlement = await_drain_task(&slot);
        tokio::pin!(settlement);
        assert!(futures_util::poll!(settlement.as_mut()).is_pending());
    }
    assert!(
        slot.lock().await.is_some(),
        "cancelling waiter lost the handle"
    );
    assert!(!completed.load(Ordering::Acquire));
    release.send(()).unwrap();
    assert!(await_drain_task(&slot).await);
    assert!(completed.load(Ordering::Acquire));
    assert!(slot.lock().await.is_none());
}

#[test]
fn retained_clone_keeps_actual_flock_after_original_descriptor_drops() {
    use fs2::FileExt;
    use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};
    let fixture = super::super::private_fs::tests::fixture();
    let path = fixture.path().join("synthetic-service.lock");
    let original = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    FileExt::try_lock_exclusive(&original).unwrap();
    let retained = clone_lease_file(&original).unwrap();
    let flags = unsafe { libc::fcntl(retained.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    drop(original);
    assert_eq!(
        FileExt::try_lock_exclusive(&contender).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(retained);
    FileExt::try_lock_exclusive(&contender).unwrap();
}

#[test]
fn deferred_startup_failure_disarms_exec_and_preserves_error_for_cleanup() {
    let failed = AtomicBool::new(false);
    let armed = AtomicBool::new(true);
    let error = defer_startup_result(
        Err(anyhow!("synthetic startup failure")),
        true,
        &failed,
        &armed,
    )
    .unwrap()
    .unwrap();
    assert_eq!(error.to_string(), "synthetic startup failure");
    assert!(failed.load(Ordering::Acquire));
    assert!(!armed.load(Ordering::Acquire));
    assert!(
        defer_startup_result(
            Err(anyhow!("synthetic standalone failure")),
            false,
            &failed,
            &armed
        )
        .is_err()
    );
}

#[test]
fn retained_observability_outlives_runtime_then_flushes_and_drops_outside_it() {
    let fixture = super::super::private_fs::tests::fixture();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let retained = runtime.block_on(async {
        let original =
            Arc::new(crate::observability::Observability::new(fixture.path(), None).unwrap());
        let retained = original.clone();
        original.record_takeover("synthetic-post-runtime");
        drop(original);
        retained
    });
    let weak = Arc::downgrade(&retained);
    drop(runtime);
    assert!(weak.upgrade().is_some());
    assert!(tokio::runtime::Handle::try_current().is_err());
    let failed = AtomicBool::new(false);
    finish_observability(retained, &failed);
    assert!(weak.upgrade().is_none());
    assert!(!failed.load(Ordering::Acquire));
    let ledger: String = std::fs::read_dir(fixture.path().join("ledger"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(ledger.contains("synthetic-post-runtime"));
}
