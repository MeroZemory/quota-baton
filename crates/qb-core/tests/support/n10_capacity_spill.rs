use super::*;

#[tokio::test]
async fn r13_h_four_bound_sessions_keep_source_and_all_fall_back_at_cap() {
    let h = Harness::new_with_options(2, false, false, None, Some((30, 2))).await;
    h.fake.state.keep_old_connections();
    let source = h.ids[0].clone();
    let target = h.ids[1].clone();
    // Establish four distinct actual source bindings before learning the cap.
    h.pins.set(Provider::Codex, None, source.clone());
    let sessions: Vec<_> = (0..4)
        .map(|index| format!("synthetic-n10-{index}"))
        .collect();
    for session in &sessions {
        let (mut socket, response) = connect_async(session_request(&h, session)).await.unwrap();
        assert_eq!(response.headers()["x-quota-baton-account"], source.as_str());
        socket.close(None).await.unwrap();
        drop(socket);
        capacity_count(&h, &source, 0).await;
    }
    h.pins.clear();
    let mut occupied = learn_one_socket_cap(&h, &source, &sessions[0]).await;
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-codex-primary-used-percent", "0"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-after-seconds", "18000"),
        ("x-codex-secondary-used-percent", "10"),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-reset-after-seconds", "604800"),
    ] {
        headers.insert(name, value.parse().unwrap());
    }
    h.state
        .accounts
        .observe_headers(&target, Provider::Codex, &headers)
        .await;
    let waits_before = h.state.ws_capacity_waits_for_test();
    let dials_before = h.fake.state.requests().len();
    let moves_before = h
        .ledger()
        .iter()
        .filter(|row| row["event"] == "binding_move")
        .count();
    let pending: Vec<_> = sessions
        .iter()
        .map(|session| {
            let request = session_request(&h, session);
            tokio::spawn(async move { connect_async(request).await })
        })
        .collect();
    let mut http_fallbacks = 0;
    for request in pending {
        let result = tokio::time::timeout(Duration::from_secs(4), request)
            .await
            .unwrap()
            .unwrap();
        match result {
            Ok((mut socket, _)) => {
                let _ = socket.close(None).await;
                panic!("bound capped session must return HTTP fallback without migration");
            }
            Err(Error::Http(response)) => {
                assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
                let body: Value =
                    serde_json::from_slice(response.body().as_ref().unwrap()).unwrap();
                assert_eq!(body["error"]["type"], "websocket_capacity");
                http_fallbacks += 1;
            }
            Err(other) => panic!("unexpected synthetic upgrade result: {other}"),
        }
    }
    assert_eq!(http_fallbacks, sessions.len());
    assert_eq!(
        h.fake.state.requests().len(),
        dials_before,
        "known bound cap must not dial any destination"
    );
    assert_eq!(
        h.ledger()
            .iter()
            .filter(|row| row["event"] == "binding_move")
            .count(),
        moves_before,
        "capacity fallback must not publish binding_move"
    );
    assert!(
        h.state.ws_capacity_waits_for_test() >= waits_before + 4,
        "each existing session must first wait on its source slot"
    );
    assert_eq!(
        sessions
            .iter()
            .filter(|session| h
                .state
                .bindings
                .current(Some(&format!("codex:{session}")))
                .as_deref()
                == Some(source.as_str()))
            .count(),
        sessions.len(),
        "every existing session must preserve its original binding"
    );
    assert_eq!(h.store.load(&source).unwrap().health, Health::Ready);
    assert_eq!(h.store.load(&target).unwrap().health, Health::Ready);
    assert_eq!(h.state.ws_reserved_for_test(&target), 0);
    occupied.close(None).await.unwrap();
}
