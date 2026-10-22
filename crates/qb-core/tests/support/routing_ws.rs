use super::*;

#[tokio::test]
async fn upgrade_rate_hold_expiry_sends_once_before_the_overall_deadline() {
    let h = Harness::new_with_rate_options(1, false, false, Some((1, 3))).await;
    let id = &h.ids[0];
    h.state
        .accounts
        .observe_429(id, Some(Duration::from_secs(30)));
    let began = std::time::Instant::now();
    let (mut client, response) =
        tokio::time::timeout(Duration::from_millis(1800), connect_async(h.request()))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(response.headers()["x-quota-baton-account"], id.as_str());
    assert!(
        began.elapsed() >= Duration::from_millis(800),
        "admission cannot skip the one-second hold's final-send boundary"
    );
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_credential(&h, 0, id);
    assert_eq!(
        h.fake.state.requests()[0].headers["session-id"],
        "synthetic-ws-session"
    );
    let row = h
        .ledger()
        .into_iter()
        .find(|row| row["outcome"] == "ws_upgrade")
        .unwrap();
    assert_eq!(row["status"], 101);
    assert_eq!(row["account"], id.as_str());
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(row["relayed_rate_429s"], 0);
    assert!(row["rate_wait_ms"].as_u64().unwrap() >= 800);
    assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
    let _ = client.close(None).await;
}

#[tokio::test]
async fn slow_forced_upgrade_still_expires_at_the_original_overall_deadline() {
    let h = Harness::new_with_rate_options(1, false, false, Some((1, 3))).await;
    let id = &h.ids[0];
    h.state
        .accounts
        .observe_429(id, Some(Duration::from_secs(30)));
    h.fake.state.push_handshake_delayed(
        StatusCode::BAD_REQUEST,
        r#"{"error":{"type":"invalid_request_error","message":"synthetic delayed rejection"}}"#,
        HeaderMap::new(),
        Duration::from_secs(2),
    );
    let began = std::time::Instant::now();
    let Error::Http(response) =
        tokio::time::timeout(Duration::from_millis(1800), connect_async(h.request()))
            .await
            .expect("the forced dial must not renew the overall one-second envelope")
            .unwrap_err()
    else {
        panic!("expected overall upgrade timeout");
    };
    assert!(began.elapsed() >= Duration::from_millis(900));
    assert!(began.elapsed() < Duration::from_millis(1800));
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(!response.headers().contains_key("retry-after"));
    let json: Value = serde_json::from_slice(response.body().as_ref().unwrap()).unwrap();
    assert_eq!(json["error"]["type"], "request_timeout");
    assert!(!json.to_string().contains("synthetic delayed rejection"));
    assert_eq!(
        h.fake.state.requests().len(),
        1,
        "expiry authorizes one dial, not retries or failover"
    );
    assert_credential(&h, 0, id);
    let row = h
        .ledger()
        .into_iter()
        .find(|row| row["outcome"] == "request_timeout")
        .unwrap();
    assert_eq!(row["status"], 504);
    assert_eq!(row["account"], id.as_str());
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["rate_retries"], 0);
    assert_eq!(row["relayed_rate_429s"], 0);
    assert!(row["rate_wait_ms"].as_u64().unwrap() >= 800);
    assert!(row["dur_ms"].as_u64().unwrap() < 1800);
    assert_eq!(h.store.load(id).unwrap().health, Health::Ready);
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
    // The one-second minimum breaker trip must be shorter than the HTTP
    // deadline. Keep the delayed handshake strictly between the two budgets.
    for (http, ws, expected) in [
        (2, 4, StatusCode::BAD_REQUEST),
        (4, 2, StatusCode::GATEWAY_TIMEOUT),
    ] {
        let h = Harness::new_with_deadlines(http, ws).await;
        h.fake.state.push_handshake_delayed(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"type":"invalid_request_error","message":"synthetic delayed handshake"}}"#,
            HeaderMap::new(),
            Duration::from_secs(3),
        );
        let start = std::time::Instant::now();
        assert_eq!(h.error().await.0, expected);
        assert!(start.elapsed() < Duration::from_secs(5));
        if http == 2 {
            assert!(start.elapsed() >= Duration::from_millis(2800));
        } else {
            assert!(start.elapsed() < Duration::from_millis(2800));
        }
    }
}

pub(super) async fn strict(h: &Harness) {
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
        h.fake.state.set_echo(true);
        let began = std::time::Instant::now();
        client
            .send(Message::Text(large.clone().into()))
            .await
            .unwrap();
        if cooling {
            assert_close(receive(&mut client).await, 1000);
            assert!(
                h.fake.state.messages().is_empty(),
                "model-cooled create reached upstream"
            );
        } else {
            assert_eq!(
                receive(&mut client).await,
                Message::Text(large.clone().into())
            );
            assert!(began.elapsed() >= Duration::from_millis(900));
            let messages = h.fake.state.messages();
            assert_eq!(
                messages,
                vec![Message::Text(large.into())],
                "initial expiry must send the exact large create once"
            );
            let _ = client.close(None).await;
        }
        assert_eq!(h.fake.state.requests().len(), 1);
        assert_credential(&h, 0, &a);
    }
}

#[tokio::test]
async fn small_unclassified_json_output_never_allows_post_publication_rate_replay() {
    for output in [
        r#"{"opaque":"published"}"#,
        r#"["published"]"#,
        r#""published""#,
        r#"{"type":"unexpected.delta","text":"published"}"#,
        r#"{"type":"response.output_text.delta","response_id":"unknown","delta":"published"}"#,
        r#"{"type":"response.future_delta","response_id":"unknown","text":"published"}"#,
        r#"{"type":"response.failed","response_id":"unknown","response":{"error":{"code":"synthetic"}}}"#,
        r#"{"type":"response.output_text.delta","type":"metadata","text":"published"}"#,
    ] {
        let h = Harness::new(1).await;
        let rate = r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#;
        h.fake
            .state
            .push_reply([Message::Text(output.into()), Message::Text(rate.into())]);
        let (mut client, a) = h.connect().await;
        create(&mut client, "fixture-model").await;
        assert_eq!(receive(&mut client).await, Message::Text(output.into()));
        assert_eq!(receive(&mut client).await, Message::Text(rate.into()));
        assert_eq!(h.fake.state.requests().len(), 1);
        assert_credential(&h, 0, &a);
        assert_eq!(h.fake.state.messages().len(), 1);
        let _ = client.close(None).await;
    }
}

#[tokio::test]
async fn output_during_rate_wait_cancels_reserved_replay_and_keeps_the_response_alive() {
    let h = Harness::new_with_rate_options(1, false, false, Some((5, 3))).await;
    h.fake.state.push_reply([Message::Text(
        r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into(),
    )]);
    let (mut client, a) = h.connect().await;
    create(&mut client, "fixture-model").await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while h.state.accounts.pacing_snapshot(&a).waiting == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let delta =
        Message::Text(r#"{"type":"response.output_text.delta","delta":"published"}"#.into());
    h.fake.state.send(delta.clone()).unwrap();
    assert_eq!(receive(&mut client).await, delta);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        h.fake.state.messages().len(),
        1,
        "a reserved retry was sent after output became public"
    );
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_credential(&h, 0, &a);
    let done = Message::Text(r#"{"type":"response.completed"}"#.into());
    h.fake.state.send(done.clone()).unwrap();
    assert_eq!(receive(&mut client).await, done);
    let _ = client.close(None).await;
}

#[tokio::test]
async fn unknown_model_is_rechecked_for_initial_retry_and_redial_data() {
    for mode in ["initial", "retry", "redial"] {
        let h = Harness::new_with_rate_options(1, false, false, Some((5, 3))).await;
        if mode != "initial" {
            let mut reply = vec![Message::Text(
                r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded"}}"#.into(),
            )];
            if mode == "redial" {
                reply.push(Message::Close(Some(
                    tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: 1012.into(),
                        reason: "".into(),
                    },
                )));
            }
            h.fake.state.push_reply(reply);
        }
        let (mut client, a) = h.connect().await;
        if mode == "initial" {
            h.state
                .accounts
                .observe_429(&a, Some(Duration::from_secs(2)));
        }
        client
            .send(Message::Text(
                r#"{"type":"response.create","input":[]}"#.into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while h.state.accounts.pacing_snapshot(&a).waiting == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        h.store
            .update_metadata(&a, |account| {
                account.health = Health::Cooling {
                    reason: HealthReason::Permission,
                    until: Utc::now() + ChronoDuration::hours(1),
                    scope: "model:blocked".into(),
                }
            })
            .unwrap();
        assert_close(receive(&mut client).await, 1000);
        assert_eq!(
            h.fake.state.messages().len(),
            usize::from(mode != "initial")
        );
        assert_eq!(
            h.fake.state.requests().len(),
            1,
            "model-unknown redial opened an extra socket"
        );
        assert_credential(&h, 0, &a);
    }
}

#[tokio::test]
async fn retained_create_stays_charged_across_write_and_backpressures_another_socket() {
    let h = Harness::new_with_budget_options(1, false, false, None, None, Some(1)).await;
    h.fake.state.keep_old_connections();
    let (mut first, a) = h.connect().await;
    let message = format!(
        r#"{{"type":"response.create","model":"fixture","input":"{}"}}"#,
        "x".repeat(2 * 1024 * 1024)
    );
    let charged = message.len();
    first.send(Message::Text(message.into())).await.unwrap();
    let texts = || {
        h.fake
            .state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(_)))
            .count()
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while texts() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        h.state.budget.used(),
        charged,
        "upstream flush cannot release the retained allocation charge"
    );
    let upstream_first = h.fake.state.sender().unwrap();
    let (mut second, b) = h.connect().await;
    assert_eq!(a, b);
    assert_credential(&h, 0, &a);
    assert_credential(&h, 1, &b);
    create(&mut second, "fixture").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(texts(), 1, "second socket read must remain backpressured");
    upstream_first
        .send(Message::Text(
            r#"{"type":"response.created","response":{"id":"first"}}"#.into(),
        ))
        .unwrap();
    assert!(matches!(receive(&mut first).await, Message::Text(_)));
    tokio::time::timeout(Duration::from_secs(3), async {
        while texts() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.fake
        .state
        .send(Message::Text(
            r#"{"type":"response.completed","response":{"id":"second"}}"#.into(),
        ))
        .unwrap();
    assert!(matches!(receive(&mut second).await, Message::Text(_)));
    first.close(None).await.unwrap();
    second.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while h.state.budget.used() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn upgrade_skips_reset_beyond_remaining_budget_without_overriding_strict() {
    for mode in [
        qb_core::routing::Mode::Recover,
        qb_core::routing::Mode::Rebalance,
        qb_core::routing::Mode::Strict,
    ] {
        let h = Harness::new_with_options(2, false, false, None, Some((3, 1))).await;
        let (mut first, a) = h.connect().await;
        assert_credential(&h, 0, &a);
        first.close(None).await.unwrap();
        let table = h.state.bindings.clone();
        tokio::task::spawn_blocking(move || {
            table.set_routing_session(
                &qb_core::routing::session_id("codex:synthetic-ws-session"),
                Some(qb_core::routing::Settings {
                    mode,
                    reset_wait_secs: 60,
                }),
                None,
                false,
            )
        })
        .await
        .unwrap()
        .unwrap();
        let until = Utc::now() + ChronoDuration::seconds(20);
        h.store
            .update_metadata(&a, |account| {
                account.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until,
                    scope: "account".into(),
                };
                account.quota_cooling = Some(qb_core::accounts::QuotaCooling {
                    window: "5h".into(),
                    reset_at: until,
                    started_at: Utc::now(),
                });
            })
            .unwrap();
        if mode == qb_core::routing::Mode::Strict {
            assert_eq!(h.error().await.0, StatusCode::CONFLICT);
            assert_eq!(h.fake.state.requests().len(), 1);
        } else {
            let (mut next, b) = h.connect().await;
            assert_ne!(a, b);
            assert_credential(&h, 1, &b);
            next.close(None).await.unwrap();
        }
    }
}

#[tokio::test]
async fn pending_quota_reconnect_skips_reset_outside_upgrade_budget_in_each_mode() {
    for mode in [
        qb_core::routing::Mode::Recover,
        qb_core::routing::Mode::Rebalance,
        qb_core::routing::Mode::Strict,
    ] {
        let h = Harness::new_with_options(2, false, false, None, Some((3, 1))).await;
        let (mut first, a) = h.connect().await;
        assert_credential(&h, 0, &a);
        let table = h.state.bindings.clone();
        tokio::task::spawn_blocking(move || {
            table.set_routing_session(
                &qb_core::routing::session_id("codex:synthetic-ws-session"),
                Some(qb_core::routing::Settings {
                    mode,
                    reset_wait_secs: 60,
                }),
                None,
                false,
            )
        })
        .await
        .unwrap()
        .unwrap();
        h.fake.state.push_reply([Message::Text(r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","resets_in_seconds":20}}"#.into())]);
        create(&mut first, "fixture").await;
        let end = receive(&mut first).await;
        if let Message::Close(_) = end {
        } else {
            assert!(matches!(end, Message::Text(_)));
        }
        assert_eq!(
            h.state
                .accounts
                .pending_quota_count_for_test("codex:synthetic-ws-session"),
            1,
            "actual quota frame must create the pending ticket before reconnect"
        );
        if mode == qb_core::routing::Mode::Strict {
            assert_eq!(h.error().await.0, StatusCode::CONFLICT);
            assert_eq!(h.fake.state.requests().len(), 1);
        } else {
            let (mut next, b) = h.connect().await;
            assert_ne!(a, b);
            assert_credential(&h, 1, &b);
            next.close(None).await.unwrap();
        }
    }
}
