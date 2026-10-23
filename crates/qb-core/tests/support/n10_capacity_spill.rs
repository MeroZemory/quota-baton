use super::*;

#[tokio::test]
async fn n10_four_bound_sessions_spill_only_one_to_a_fresh_reset_account() {
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
    let pending: Vec<_> = sessions
        .iter()
        .map(|session| {
            let request = session_request(&h, session);
            tokio::spawn(async move { connect_async(request).await })
        })
        .collect();
    let mut landed = Vec::new();
    let mut http_fallbacks = 0;
    for request in pending {
        let result = tokio::time::timeout(Duration::from_secs(4), request)
            .await
            .unwrap()
            .unwrap();
        match result {
            Ok((socket, response)) => {
                assert_eq!(response.headers()["x-quota-baton-account"], target.as_str());
                landed.push(socket);
            }
            Err(Error::Http(response)) => {
                assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
                http_fallbacks += 1;
            }
            Err(other) => panic!("unexpected synthetic upgrade result: {other}"),
        }
    }
    assert_eq!(
        landed.len(),
        1,
        "one successful spill, not a vacuous all-refused cap"
    );
    assert_eq!(http_fallbacks, 3);
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
                == Some(target.as_str()))
            .count(),
        1,
        "denied spill admission cannot publish a moved binding"
    );
    assert_eq!(h.store.load(&source).unwrap().health, Health::Ready);
    assert_eq!(h.store.load(&target).unwrap().health, Health::Ready);
    for socket in &mut landed {
        socket.close(None).await.unwrap();
    }
    occupied.close(None).await.unwrap();
}
