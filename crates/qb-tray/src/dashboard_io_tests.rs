use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn r10_data_timestamp_advances_only_for_successful_data_publications() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-21T06:25:50+09:00").unwrap();
    let first = model::Clock {
        now,
        day_start: now.with_timezone(&Utc) - chrono::Duration::hours(6),
        day_end: now.with_timezone(&Utc) + chrono::Duration::hours(18),
    };
    let mut data = model::DashboardSnapshot::new(
        qb_tray::v2::project(r#"{"accounts":[]}"#, first.utc().into()).unwrap(),
    );
    let mut later = first.clone();
    later.now += chrono::Duration::seconds(30);
    record_data_publication(&mut data, ALL, false, &first);
    assert_eq!(data.data_at, None);
    record_data_publication(&mut data, CLOCK, true, &first);
    assert_eq!(data.data_at, None);
    record_data_publication(&mut data, ALL, true, &first);
    assert_eq!(data.data_at, Some(first.utc()));

    for (dirty, ok) in [
        (CLOCK, true),
        (CLOCK, false),
        (ALL | CLOCK, false),
        (USAGE, false),
        (DETAIL, true),
    ] {
        record_data_publication(&mut data, dirty, ok, &later);
        assert_eq!(data.data_at, Some(first.utc()));
    }
    // A successful fallback poll is fresh even while the stream stays down.
    data.connection.stream_ok = false;
    record_data_publication(&mut data, ALL | DETAIL, true, &later);
    assert_eq!(data.data_at, Some(later.utc()));
    assert!(!data.connection.stream_ok);
    for dirty in [STATE, SESSIONS, ACTIVITY, USAGE] {
        later.now += chrono::Duration::seconds(5);
        record_data_publication(&mut data, dirty, true, &later);
        assert_eq!(data.data_at, Some(later.utc()));
    }
}

struct Fake {
    client: AdminClient,
    hold: Arc<Notify>,
    state_seen: Arc<AtomicUsize>,
    usage_seen: Arc<AtomicUsize>,
    activity_partial: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Fake {
    async fn start(hold_first_state: bool, fail_first_usage: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(![10100, 10101].contains(&port));
        let hold = Arc::new(Notify::new());
        let state_seen = Arc::new(AtomicUsize::new(0));
        let usage_seen = Arc::new(AtomicUsize::new(0));
        let activity_partial = Arc::new(AtomicBool::new(false));
        let partial = activity_partial.clone();
        let (h, s, u) = (hold.clone(), state_seen.clone(), usage_seen.clone());
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (h, s, u) = (h.clone(), s.clone(), u.clone());
                let partial = partial.clone();
                tokio::spawn(async move {
                    let mut wire = Vec::new();
                    let mut buffer = [0; 1024];
                    while !wire.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = socket.read(&mut buffer).await.unwrap();
                        assert!(n > 0 && wire.len() < 8192);
                        wire.extend_from_slice(&buffer[..n]);
                    }
                    let request = String::from_utf8(wire).unwrap();
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("authorization: bearer r7-actor-synthetic")
                    );
                    let path = request.split_whitespace().nth(1).unwrap();
                    let (status, body) = if path == "/admin/v1/state" {
                        let previous = s.fetch_add(1, Ordering::SeqCst);
                        if hold_first_state && previous == 0 {
                            h.notified().await;
                        }
                        (
                            200,
                            r#"{"accounts":[{"id":"synthetic-account","provider":"claude","email":"actor@example.invalid","plan":"pro"}]}"#,
                        )
                    } else if path.starts_with("/admin/v1/sessions?") {
                        if path.ends_with("range=all") {
                            (
                                200,
                                r#"{"sessions":[{"id":"abcdef123456","provider":"claude","account":"synthetic-account"}]}"#,
                            )
                        } else {
                            (200, r#"{"sessions":[]}"#)
                        }
                    } else if path.starts_with("/admin/v1/activity?") {
                        (
                            200,
                            if partial.load(Ordering::SeqCst) {
                                r#"{"truncated":true,"events":[{"seq":7,"kind":"token_refresh","provider":"claude","account":"synthetic-account"}]}"#
                            } else {
                                r#"{"truncated":false,"events":[{"seq":7,"kind":"token_refresh","provider":"claude","account":"synthetic-account"}]}"#
                            },
                        )
                    } else if path.starts_with("/admin/v1/usage-history?") {
                        if u.fetch_add(1, Ordering::SeqCst) == 0 && fail_first_usage {
                            (503, r#"{"error":"synthetic unavailable"}"#)
                        } else {
                            (
                                200,
                                r#"{"accounts":[{"id":"synthetic-account","windows":[] }]}"#,
                            )
                        }
                    } else {
                        (404, "{}")
                    };
                    let header = format!(
                        "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(header.as_bytes()).await.unwrap();
                    socket.write_all(body.as_bytes()).await.unwrap();
                });
            }
        });
        Self {
            client: AdminClient::new(port, "r7-actor-synthetic").unwrap(),
            hold,
            state_seen,
            usage_seen,
            activity_partial,
            task,
        }
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spin_until(mut ready: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(
            std::time::Instant::now() < end,
            "bounded actor observation timed out"
        );
        tokio::task::yield_now().await;
    }
}
fn actor(fake: &Fake) -> (Arc<Coordinator>, UiPort, tokio::task::JoinHandle<()>) {
    let coordinator = Coordinator::new(0);
    *coordinator.test_client.lock().unwrap() = Some(fake.client.clone());
    coordinator.control.lock().unwrap().geometry_known = true;
    let port = UiPort::test_port(42);
    coordinator.enqueue(Event::Opened { port: port.clone() });
    coordinator.stream(true);
    let task = tokio::spawn(coordinator.clone().run());
    (coordinator, port, task)
}

#[tokio::test(start_paused = true)]
async fn dashboard_actor_preserves_and_clears_activity_completeness_with_the_events() {
    let fake = Fake::start(false, false).await;
    fake.activity_partial.store(true, Ordering::SeqCst);
    let (coordinator, port, task) = actor(&fake);
    let mut observed = None;
    spin_until(|| {
        if let Some(snapshot) = port.test_snapshot() {
            observed = Some(snapshot);
        }
        observed.as_ref().is_some_and(|s| s.activity_truncated)
    })
    .await;
    assert_eq!(observed.as_ref().unwrap().activity[0].seq, 7);
    fake.activity_partial.store(false, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(2)).await;
    coordinator.dirty(ACTIVITY);
    spin_until(|| {
        if let Some(snapshot) = port.test_snapshot() {
            observed = Some(snapshot);
        }
        observed.as_ref().is_some_and(|s| !s.activity_truncated)
    })
    .await;
    assert_eq!(observed.unwrap().activity[0].seq, 7);
    coordinator.stop();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn dashboard_actor_refetches_consumed_categories_after_intent_changes_during_http() {
    let fake = Fake::start(true, false).await;
    let (coordinator, port, task) = actor(&fake);
    spin_until(|| fake.state_seen.load(Ordering::SeqCst) == 1).await;
    coordinator.enqueue(Event::Command {
        owner: 42,
        command: Command::Sessions(Range::All),
    });
    fake.hold.notify_one();
    let mut observed = None;
    spin_until(|| {
        if let Some(snapshot) = port.test_snapshot() {
            observed = Some(snapshot);
        }
        observed
            .as_ref()
            .is_some_and(|s| !s.sessions.sessions.is_empty())
    })
    .await;
    let snapshot = observed.unwrap();
    assert_eq!(snapshot.sessions.sessions[0].id, "abcdef123456");
    assert_eq!(snapshot.activity[0].seq, 7);
    assert!(!snapshot.accounts.raw.accounts.is_empty());
    assert!(snapshot.connection.core_ok && snapshot.connection.stream_ok);
    assert!(fake.state_seen.load(Ordering::SeqCst) >= 2);
    coordinator.stop();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn dashboard_actor_retries_failed_usage_without_new_quota_event_at_throttle_boundary() {
    let fake = Fake::start(false, true).await;
    let (coordinator, port, task) = actor(&fake);
    let mut observed = None;
    spin_until(|| {
        if let Some(snapshot) = port.test_snapshot() {
            observed = Some(snapshot);
        }
        observed.as_ref().is_some_and(|s| s.connection.core_ok)
    })
    .await;
    coordinator.enqueue(Event::Command {
        owner: 42,
        command: Command::Usage,
    });
    spin_until(|| fake.usage_seen.load(Ordering::SeqCst) == 1).await;
    spin_until(|| port.test_snapshot().is_some_and(|s| !s.connection.core_ok)).await;
    tokio::time::advance(Duration::from_secs(6)).await;
    // A successful unrelated state refresh must not erase history's retry.
    coordinator.dirty(STATE);
    spin_until(|| port.test_snapshot().is_some_and(|s| s.connection.core_ok)).await;
    assert_eq!(fake.usage_seen.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(23)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fake.usage_seen.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(2)).await;
    spin_until(|| {
        port.test_snapshot()
            .is_some_and(|s| s.usage.accounts.len() == 1)
    })
    .await;
    assert_eq!(fake.usage_seen.load(Ordering::SeqCst), 2);
    coordinator.stop();
    task.await.unwrap();
}

#[path = "dashboard_io_urgency_tests.rs"]
mod urgency_tests;
