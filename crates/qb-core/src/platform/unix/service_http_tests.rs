//! Actual admin connection and blocking-writer lifetime regressions.
use super::*;
use axum::routing::post;
use fs2::FileExt;
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};
use tokio::sync::oneshot;

fn synthetic_http_config() -> Config {
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    config
}

fn synthetic_http_secrets() -> crate::local_secrets::LocalSecrets {
    crate::local_secrets::LocalSecrets {
        client_key: "synthetic-held-handler-client".into(),
        admin_token: "synthetic-held-handler-admin".into(),
    }
}

// This deliberately compares the same real axum route in both cases. Aborting
// the accept parent is NOT evidence that its already accepted handler ended.
#[tokio::test]
async fn actual_admin_graceful_parent_waits_but_aborted_parent_detaches_handler() {
    for abort_parent in [false, true] {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (finished_tx, finished_rx) = oneshot::channel();
        let channels = Arc::new(Mutex::new(Some((entered_tx, release_rx, finished_tx))));
        let route = Router::new().route(
            "/admin/v1/synthetic-held",
            post(move || {
                let channels = channels.clone();
                async move {
                    let (entered, release, finished) = channels.lock().unwrap().take().unwrap();
                    entered.send(()).unwrap();
                    release.await.unwrap();
                    finished.send(()).unwrap();
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let mut servers = crate::http::spawn_with_admin(
            &synthetic_http_config(),
            synthetic_http_secrets(),
            route,
        )
        .await
        .unwrap();
        let url = format!("http://{}/admin/v1/synthetic-held", servers.admin_addr);
        let request = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap()
                .post(url)
                .bearer_auth("synthetic-held-handler-admin")
                .send()
                .await
                .unwrap()
                .status()
        });
        timeout(Duration::from_secs(5), entered_rx)
            .await
            .unwrap()
            .unwrap();
        servers.shutdown();
        timeout(Duration::from_secs(5), servers.wait_for_listener_close())
            .await
            .unwrap();
        // Borrow the JoinHandle: timing out must not discard its ownership.
        assert!(
            timeout(Duration::from_millis(100), &mut servers.admin_task)
                .await
                .is_err()
        );
        if abort_parent {
            servers.admin_task.abort();
            assert!((&mut servers.admin_task).await.unwrap_err().is_cancelled());
        }
        // A response after the aborted parent's terminal join proves the
        // accepted handler was still alive independently of that parent.
        release_tx.send(()).unwrap();
        timeout(Duration::from_secs(5), finished_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.await.unwrap(), StatusCode::NO_CONTENT);
        if !abort_parent {
            timeout(Duration::from_secs(5), &mut servers.admin_task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        timeout(Duration::from_secs(5), &mut servers.data_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

struct MarkHandlerDrop(Arc<AtomicBool>);
impl Drop for MarkHandlerDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn cancelled_admin_request_runtime_drop_joins_started_writer_with_lease_retained() {
    let fixture = super::super::private_fs::tests::fixture();
    let lease_path = fixture.path().join("synthetic-http.lock");
    let original = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lease_path)
        .unwrap();
    FileExt::try_lock_exclusive(&original).unwrap();
    // Use the production duplicate primitive, without the process-global bank
    // or managed-mode environment. This is the final native owner's local fd.
    let retained = clone_lease_file(&original).unwrap();
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lease_path)
        .unwrap();
    drop(original);
    let marker_path = fixture.path().join("synthetic-writer-completed");
    let handler_dropped = Arc::new(AtomicBool::new(false));
    let writer_completed = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let channels = Arc::new(Mutex::new(Some((started_tx, release_rx))));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let handler_dropped = handler_dropped.clone();
        let writer_completed = writer_completed.clone();
        let marker_path = marker_path.clone();
        let route = Router::new().route(
            "/admin/v1/synthetic-writer",
            post(move || {
                let channels = channels.clone();
                let handler_dropped = handler_dropped.clone();
                let writer_completed = writer_completed.clone();
                let marker_path = marker_path.clone();
                async move {
                    let _handler_drop = MarkHandlerDrop(handler_dropped);
                    let (started, release) = channels.lock().unwrap().take().unwrap();
                    tokio::task::spawn_blocking(move || {
                        started.send(()).unwrap();
                        // Watchdog only: avoids hanging a broken regression.
                        // Successful test operation always releases this first.
                        release.recv_timeout(Duration::from_secs(5)).unwrap();
                        std::fs::write(marker_path, b"writer finished").unwrap();
                        writer_completed.store(true, Ordering::Release);
                    })
                    .await
                    .unwrap();
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let servers = crate::http::spawn_with_admin(
            &synthetic_http_config(),
            synthetic_http_secrets(),
            route,
        )
        .await
        .unwrap();
        let url = format!("http://{}/admin/v1/synthetic-writer", servers.admin_addr);
        let request = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap()
                .post(url)
                .bearer_auth("synthetic-held-handler-admin")
                .send()
                .await
        });
        // Proves this is an already RUNNING blocking writer, not a queued task
        // which RuntimeDrop could cancel before it starts.
        timeout(Duration::from_secs(5), started_rx)
            .await
            .unwrap()
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        servers.shutdown();
        // Match the unsafe parent-drop case. RuntimeDrop below must still own
        // the remaining connection future and its running blocking writer.
        drop(servers);
    });

    let (dropping_tx, dropping_rx) = std::sync::mpsc::channel();
    let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
    let release_worker = std::thread::spawn(move || {
        dropping_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let runtime_waited = matches!(
            dropped_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        let lease_held = FileExt::try_lock_exclusive(&contender)
            .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock);
        // Release even if an assertion will fail; do not strand the writer.
        release_tx.send(()).unwrap();
        (runtime_waited, lease_held, contender)
    });
    dropping_tx.send(()).unwrap();
    drop(runtime);
    dropped_tx.send(()).ok();
    let (runtime_waited, lease_held, contender) = release_worker.join().unwrap();
    assert!(
        runtime_waited,
        "RuntimeDrop returned while the writer was held"
    );
    assert!(lease_held, "final native owner lost the actual flock");
    assert!(handler_dropped.load(Ordering::Acquire));
    assert!(writer_completed.load(Ordering::Acquire));
    assert_eq!(std::fs::read(&marker_path).unwrap(), b"writer finished");
    assert_eq!(
        FileExt::try_lock_exclusive(&contender).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(retained);
    FileExt::try_lock_exclusive(&contender).unwrap();
}
