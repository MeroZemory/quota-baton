use super::*;

async fn strict(h: &Harness) {
    let table = h.state.bindings.clone();
    tokio::task::spawn_blocking(move || {
        table.set_routing_session(
            &qb_core::routing::session_id("codex:synthetic-ws-session"),
            Some(qb_core::routing::Settings {
                mode: qb_core::routing::Mode::Strict,
                reset_wait_secs: 0,
            }),
            None,
            false,
        )
    })
    .await
    .unwrap()
    .unwrap();
}
fn assert_credential(h: &Harness, index: usize, id: &str) {
    let account = h.store.load(id).unwrap();
    let tokens = h.store.tokens(&account).unwrap();
    let request = &h.fake.state.requests()[index];
    assert_eq!(
        request.headers["authorization"].to_str().unwrap(),
        format!("Bearer {}", tokens.access_token.as_str())
    );
    if let qb_core::accounts::Identity::Codex { account_id, .. } = account.identity {
        assert_eq!(request.headers["chatgpt-account-id"], account_id);
    }
}
#[tokio::test]
async fn strict_paused_socket_reconnect_never_dials_a_different_credential() {
    let h = Harness::new(2).await;
    let (mut client, account) = h.connect().await;
    assert_credential(&h, 0, &account);
    strict(&h).await;
    h.state
        .accounts
        .change(&account, Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    assert_close(receive(&mut client).await, 1000);
    assert_eq!(h.error().await.0, StatusCode::CONFLICT);
    assert_eq!(h.fake.state.requests().len(), 1);
    assert!(
        h.fake
            .state
            .messages()
            .iter()
            .all(|m| !matches!(m, Message::Text(_)))
    );
}
#[tokio::test]
async fn queued_create_on_a_cannot_bypass_later_strict_b() {
    let h = Harness::new(2).await;
    h.fake.state.keep_old_connections();
    let (mut old, a) = h.connect().await;
    h.state
        .accounts
        .observe_429(&a, Some(Duration::from_secs(2)));
    create(&mut old, "queued-old-account").await;
    r1_create_wait_for_initial_queue(&h, &a).await;
    h.state
        .accounts
        .change(&a, Some(UserState::Paused), None, None, None)
        .await
        .unwrap();
    let (mut next, b) = h.connect().await;
    assert_ne!(a, b);
    strict(&h).await;
    h.state
        .accounts
        .change(&a, Some(UserState::Active), None, None, None)
        .await
        .unwrap();
    assert_close(receive(&mut old).await, 1000);
    assert!(
        h.fake
            .state
            .messages()
            .iter()
            .all(|m| !matches!(m, Message::Text(_)))
    );
    assert_eq!(h.fake.state.requests().len(), 2);
    assert_credential(&h, 0, &a);
    assert_credential(&h, 1, &b);
    let done = Message::Text(r#"{"type":"response.completed"}"#.into());
    h.fake.state.push_reply([done.clone()]);
    create(&mut next, "allowed-new-account").await;
    assert_eq!(receive(&mut next).await, done);
    next.close(None).await.unwrap();
}
#[tokio::test]
async fn waiting_rate_retry_rechecks_its_model_cooling_before_writing() {
    let h = Harness::new_with_rate_options(1, false, false, Some((5, 3))).await;
    h.fake.state.push_reply([Message::Text(
        r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into(),
    )]);
    let (mut client, account) = h.connect().await;
    create(&mut client, "synthetic-model").await;
    r1_create_wait_for_initial_queue(&h, &account).await;
    h.state
        .accounts
        .transition(
            &account,
            HealthSignal::Cooling {
                reason: HealthReason::Permission,
                until: Utc::now() + ChronoDuration::minutes(1),
                scope: "model:synthetic-model".into(),
            },
        )
        .await
        .unwrap();
    assert_close(receive(&mut client).await, 1000);
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count(),
        1
    );
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_credential(&h, 0, &account);
}
