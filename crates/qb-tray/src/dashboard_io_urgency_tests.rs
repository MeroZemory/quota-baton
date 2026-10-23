//! Actual loopback mutation: the UI thread never waits on the response.
use super::*;

fn urgent(owner: u64, id: &str, value: bool) -> Event {
    Event::Command {
        owner,
        command: Command::SetSessionUrgent {
            id: id.to_owned(),
            urgent: value,
        },
    }
}

async fn delayed_urgency(
    status: u16,
) -> (
    AdminClient,
    tokio::sync::oneshot::Receiver<serde_json::Value>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(![10100, 10101].contains(&port));
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut wire = Vec::new();
        let mut buffer = [0; 1024];
        let header_end = loop {
            let n = socket.read(&mut buffer).await.unwrap();
            assert!(n > 0 && wire.len() < 8192);
            wire.extend_from_slice(&buffer[..n]);
            if let Some(i) = wire.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let header = std::str::from_utf8(&wire[..header_end])
            .unwrap()
            .to_ascii_lowercase();
        assert!(header.starts_with("post /admin/v1/routing http/1.1"));
        assert!(header.contains("authorization: bearer synthetic-urgent-worker"));
        let size: usize = header
            .lines()
            .find_map(|l| l.strip_prefix("content-length: "))
            .unwrap()
            .parse()
            .unwrap();
        assert!(size < 1024);
        while wire.len() < header_end + size {
            let n = socket.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            wire.extend_from_slice(&buffer[..n]);
        }
        seen_tx
            .send(serde_json::from_slice(&wire[header_end..header_end + size]).unwrap())
            .unwrap();
        release_rx.await.unwrap();
        let body = if status == 200 {
            r#"{"updated":true}"#
        } else {
            "{}"
        };
        socket.write_all(format!("HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        // Keep the listener alive: retries must be absent, not connection-refused.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "urgency POST must never be replayed"
        );
    });
    (
        AdminClient::new(port, "synthetic-urgent-worker").unwrap(),
        seen_rx,
        release_tx,
        task,
    )
}

#[tokio::test]
async fn n5c_dashboard_delayed_urgency_keeps_ui_control_responsive_and_posts_once() {
    for status in [200, 503] {
        let (client, seen, release, server) = delayed_urgency(status).await;
        let coordinator = Coordinator::new(0);
        *coordinator.test_client.lock().unwrap() = Some(client);
        let port = UiPort::test_port(42);
        coordinator.enqueue(Event::Opened { port: port.clone() });
        let id = "a".repeat(64);
        coordinator.enqueue(urgent(42, &id, true));
        let worker = tokio::spawn({
            let c = coordinator.clone();
            async move { c.update_urgency().await }
        });
        let payload = tokio::time::timeout(Duration::from_secs(3), seen)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            payload,
            serde_json::json!({"kind":"urgent", "id":id, "on_5h_limit":"move"})
        );
        assert!(matches!(
            port.test_urgency(),
            Some(qb_tray::state::UrgencyUpdate::Pending { .. })
        ));
        // Synchronous window commands remain runnable while the server is held.
        coordinator.enqueue(urgent(42, &id, false));
        coordinator.enqueue(Event::Command {
            owner: 42,
            command: Command::Sessions(Range::All),
        });
        assert_eq!(coordinator.control.lock().unwrap().range, Range::All);
        assert!(!worker.is_finished());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(port.test_urgency(), Some(qb_tray::state::UrgencyUpdate::Finished { failed, .. }) if failed == (status != 200))
        );
        assert_ne!(coordinator.control.lock().unwrap().dirty & SESSIONS, 0);
        coordinator.update_urgency().await;
        server.await.unwrap();
    }
}

#[tokio::test]
async fn n5c_dashboard_closed_owner_drops_unstarted_mutation_and_inflight_result_stays_old() {
    let (client, seen, release, server) = delayed_urgency(200).await;
    let coordinator = Coordinator::new(0);
    *coordinator.test_client.lock().unwrap() = Some(client);
    let old = UiPort::test_port(42);
    coordinator.enqueue(Event::Opened { port: old.clone() });
    coordinator.enqueue(urgent(42, &"a".repeat(64), true));
    coordinator.enqueue(Event::Closed { owner: 42 });
    coordinator.update_urgency().await; // No request for the closed owner's intent.
    let new = UiPort::test_port(43);
    coordinator.enqueue(Event::Opened { port: new.clone() });
    coordinator.enqueue(urgent(42, &"a".repeat(64), true)); // stale window
    coordinator.enqueue(urgent(43, "abcdef123456", true)); // display id, not authority
    assert!(
        coordinator
            .control
            .lock()
            .unwrap()
            .urgency_pending
            .is_none()
    );
    coordinator.enqueue(urgent(43, &"b".repeat(64), false));
    let worker = tokio::spawn({
        let c = coordinator.clone();
        async move { c.update_urgency().await }
    });
    let payload = tokio::time::timeout(Duration::from_secs(3), seen)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payload["id"], "b".repeat(64));
    coordinator.enqueue(Event::Closed { owner: 43 });
    let newest = UiPort::test_port(44);
    coordinator.enqueue(Event::Opened {
        port: newest.clone(),
    });
    coordinator.enqueue(urgent(44, &"c".repeat(64), true));
    assert!(
        matches!(newest.test_urgency(),
        Some(qb_tray::state::UrgencyUpdate::Finished { id, failed:true }) if id == "c".repeat(64)),
        "busy reopened owner must receive an explicit failure to clear Pending"
    );
    release.send(()).unwrap();
    worker.await.unwrap();
    assert!(
        newest.test_urgency().is_none(),
        "old mutation result must not target reopened window"
    );
    coordinator.update_urgency().await;
    server.await.unwrap();
}
