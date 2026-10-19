use super::*;

#[tokio::test]
async fn local_upgrade_rate_wait_expiry_has_typed_json_and_advice_without_a_dial() {
    let h = Harness::new_with_rate_options(1, false, false, Some((1, 3))).await;
    h.state
        .accounts
        .observe_429(&h.ids[0], Some(Duration::from_secs(30)));
    let Error::Http(response) = connect_async(h.request()).await.unwrap_err() else {
        panic!("expected local HTTP error");
    };
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 28
    );
    let json: Value = serde_json::from_slice(response.body().as_ref().unwrap()).unwrap();
    assert_eq!(json["error"]["type"], "rate_wait_timeout");
    assert!(h.fake.state.requests().is_empty());
    assert!(
        h.ledger()
            .iter()
            .any(|row| row["outcome"] == "rate_wait_timeout" && row["attempts"] == 0)
    );
}

#[tokio::test]
async fn large_model_first_payload_cannot_escape_old_socket_after_strict_move() {
    let h = Harness::new(2).await;
    h.fake.state.keep_old_connections();
    h.fake.state.push_reply([Message::Text(
        r#"{"type":"response.created","response":{"id":"still-active"}}"#.into(),
    )]);
    let (mut old, a) = h.connect().await;
    create(&mut old, "original").await;
    assert!(matches!(receive(&mut old).await, Message::Text(_)));
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
    let large = format!(
        r#"{{"model":"original","type":"response.create","input":"{}"}}"#,
        "x".repeat(1024 * 1024 + 128)
    );
    old.send(Message::Text(large.into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3),async {
        tokio::select! {
            message = old.next() => assert_close(message.unwrap().unwrap(),1000),
            _ = async { loop {
                if h.fake.state.messages().iter().filter(|m|matches!(m,Message::Text(_))).count()>1 { break; }
                tokio::task::yield_now().await;
            }} => panic!("large unclassified payload escaped on the old credential"),
        }
    }).await.unwrap();
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count(),
        1
    );
    assert_credential(&h, 0, &a);
    assert_credential(&h, 1, &b);
    next.close(None).await.unwrap();
}

#[tokio::test]
async fn upgrade_uses_its_own_ingress_deadline_independent_of_http_timeout() {
    for (http, ws, expected) in [
        (1, 3, StatusCode::BAD_REQUEST),
        (3, 1, StatusCode::GATEWAY_TIMEOUT),
    ] {
        let h = Harness::new_with_deadlines(http, ws).await;
        h.fake.state.push_handshake_delayed(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"type":"invalid_request_error","message":"synthetic delayed handshake"}}"#,
            HeaderMap::new(),
            Duration::from_secs(2),
        );
        let start = std::time::Instant::now();
        assert_eq!(h.error().await.0, expected);
        assert!(start.elapsed() < Duration::from_secs(4));
        if http == 1 {
            assert!(start.elapsed() >= Duration::from_millis(1800));
        } else {
            assert!(start.elapsed() < Duration::from_millis(1800));
        }
    }
}

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

#[tokio::test]
async fn delivered_large_unparsed_output_never_replays_the_create_on_later_rate_error() {
    let h = Harness::new(1).await;
    let large = format!(
        r#"{{"type":"response.output_text.delta","delta":"{}"}}"#,
        "x".repeat(1024 * 1024 + 1)
    );
    let rate = r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#;
    h.fake.state.push_reply([
        Message::Text(large.clone().into()),
        Message::Text(rate.into()),
    ]);
    let (mut client, a) = h.connect().await;
    create(&mut client, "fixture-model").await;
    assert_eq!(receive(&mut client).await, Message::Text(large.into()));
    assert_eq!(receive(&mut client).await, Message::Text(rate.into()));
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_eq!(
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count(),
        1
    );
    assert_credential(&h, 0, &a);
    let _ = client.close(None).await;
}

#[tokio::test]
async fn large_field_reordered_create_obeys_model_cooling_and_rate_wait() {
    for cooling in [true, false] {
        let h = Harness::new_with_rate_options(1, false, false, Some((1, 3))).await;
        let (mut client, a) = h.connect().await;
        if cooling {
            h.store
                .update_metadata(&a, |account| {
                    account.health = Health::Cooling {
                        reason: HealthReason::Permission,
                        until: Utc::now() + ChronoDuration::hours(1),
                        scope: "model:blocked".into(),
                    }
                })
                .unwrap();
        } else {
            h.state
                .accounts
                .observe_429(&a, Some(Duration::from_secs(30)));
        }
        let large = format!(
            r#"{{"input":"{}","model":"blocked","type":"response.create"}}"#,
            "x".repeat(1024 * 1024 + 128)
        );
        client.send(Message::Text(large.into())).await.unwrap();
        assert_close(
            receive(&mut client).await,
            if cooling { 1000 } else { 1013 },
        );
        assert!(
            h.fake.state.messages().is_empty(),
            "large create bypassed final model/pacing admission"
        );
        assert_eq!(h.fake.state.requests().len(), 1);
        assert_credential(&h, 0, &a);
    }
}
