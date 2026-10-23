//! Child of entry::n5c_policy_wake; genuine HTTP/poll/admin paths only.
use super::*;

async fn behavior_fixture(
    session: &str,
    mode: &str,
    urgent: &str,
) -> (Harness, String, String, ScriptedUpstream) {
    let (h, a, b, usage) = entry_fixture(session, 60).await;
    timeout(
        PHASE,
        h.admin(
            reqwest::Method::POST,
            "/admin/v1/routing",
            Some(json!({
                "kind":"session", "id":qb_core::routing::session_id(&format!("codex:{session}")),
                "settings":{"mode":mode,"reset_wait_secs":60}
            })),
        ),
    )
    .await
    .unwrap();
    urgency(&h, session, urgent).await;
    let plan = h
        .state
        .bindings
        .policies
        .plan(Some(&format!("codex:{session}")));
    assert_eq!(
        plan.settings.on_5h_limit,
        if urgent == "move" {
            On5hLimit::Move
        } else {
            On5hLimit::Wait
        }
    );
    assert_eq!(plan.settings.reset_wait_secs, 60);
    assert_eq!(
        plan.settings.mode,
        match mode {
            "recover" => Mode::Recover,
            "rebalance" => Mode::Rebalance,
            "strict" => Mode::Strict,
            _ => unreachable!(),
        }
    );
    let alternate = h.store.load(&b).unwrap();
    assert_eq!(alternate.user_state, qb_core::accounts::UserState::Active);
    assert!(matches!(alternate.health, Health::Ready));
    (h, a, b, usage)
}

async fn target(h: &Harness, session: &str) -> (u16, String, Vec<u8>) {
    // Harness.client defaults to 8s. Explicitly match the ORIGINAL configured
    // 30s operation; a cached reset is not permission to restart its budget.
    let response = h
        .client
        .post(format!("http://{}/v1/responses", h.servers.data_addr))
        .bearer_auth("synthetic-client")
        .header("content-type", "application/json")
        .header("session-id", session)
        .timeout(Duration::from_secs(30))
        .body(UPLOAD)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    if status == 429 {
        let reset = response
            .headers()
            .get("x-codex-primary-reset-at")
            .expect("fallback must preserve the actual provider reset header")
            .to_str()
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!(reset > Utc::now().timestamp() + 120);
    }
    let account = response
        .headers()
        .get("x-quota-baton-account")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = timeout(PHASE, response.bytes())
        .await
        .unwrap()
        .unwrap()
        .to_vec();
    (status, account, body)
}

fn assert_source(h: &Harness, index: usize, source: &str) {
    let account = h.store.load(source).unwrap();
    let tokens = h.store.tokens(&account).unwrap();
    let requests = h.fake.state.requests();
    assert_eq!(requests[index].path_and_query, "/responses");
    assert_eq!(requests[index].body.as_ref(), UPLOAD.as_bytes());
    assert!(
        requests[index].headers.get("authorization").is_some_and(
            |v| v.to_str().ok() == Some(format!("Bearer {}", tokens.access_token).as_str())
        ),
        "actual upstream credential must belong to the asserted synthetic source"
    );
}

async fn assert_wait_snapshot(
    h: &Harness,
    session: &str,
    source: &str,
    reset: chrono::DateTime<Utc>,
    waiting: bool,
) {
    let view = timeout(
        PHASE,
        h.admin(reqwest::Method::GET, "/admin/v1/sessions?range=all", None),
    )
    .await
    .unwrap();
    let key = format!("codex:{session}");
    let display = qb_core::dashboard::SessionId::from_key(&key);
    let row = view["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == display.as_str())
        .expect("actual sessions HTTP must retain the request's native row");
    assert_eq!(row["account"], source);
    assert_eq!(
        row["routing"]["action_id"],
        qb_core::routing::session_id(&key)
    );
    assert_eq!(row["quota_wait_known"], true);
    if waiting {
        assert_eq!(
            row["quota_wait"]["waiting"], 1,
            "actual sessions HTTP must show genuine admitted wait"
        );
        let actual =
            chrono::DateTime::parse_from_rfc3339(row["quota_wait"]["reset_at"].as_str().unwrap())
                .unwrap();
        assert_eq!(actual.timestamp(), reset.timestamp());
    } else {
        assert!(
            row["quota_wait"].is_null(),
            "actual sessions HTTP must release completed wait lifetime"
        );
    }
}

async fn clean(h: &Harness, a: &str, b: &str) {
    timeout(PHASE, idle(h, a)).await.unwrap();
    assert_eq!(h.state.accounts.inflight_count(b), 0);
    assert_eq!(h.state.bindings.pending_count(b), 0);
}

fn assert_one_move(h: &Harness, a: &str, b: &str) {
    let ledger = h.ledger();
    let moves: Vec<_> = ledger
        .iter()
        .filter(|row| row["event"] == "binding_move" && !row["from"].is_null())
        .collect();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0]["from"], a);
    assert_eq!(moves[0]["to"], b);
}

fn far_limit() -> ScriptedResponse {
    let (_, mut response) = reset_response();
    response.headers.insert(
        "x-codex-primary-reset-at",
        (Utc::now() + ChronoDuration::hours(1))
            .timestamp()
            .to_string()
            .parse()
            .unwrap(),
    );
    response
}

#[tokio::test]
async fn n5c_http_wait_and_move_keep_native_near_reset_cache_hold_in_recover_and_rebalance() {
    for mode in ["recover", "rebalance"] {
        for urgent in ["wait", "move"] {
            let session = format!("synthetic-n5c-near-{mode}-{urgent}");
            let (h, a, b, usage) = behavior_fixture(&session, mode, urgent).await;
            let reset = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 6, 0).unwrap();
            full_poll_before_request(&h, &usage, &a, reset).await;
            let earliest = Instant::now() + (reset - Utc::now()).to_std().unwrap();
            let mut events = h.state.subscribe_http_quota_wait();
            let mut phases = h.state.subscribe_http_quota_wait_inspections();
            h.fake
                .state
                .push(ScriptedResponse::json(StatusCode::OK, FINAL));
            let (sent, release) = h.fake.state.pause_next_response();
            let observe = async {
                event(
                    &mut phases,
                    "inspection_started",
                    &a,
                    PHASE,
                    "real initial inspection",
                )
                .await;
                if urgent == "wait" {
                    event(
                        &mut events,
                        "admitted",
                        &a,
                        PHASE,
                        "nonurgent full proof must wait even with eligible B",
                    )
                    .await;
                    assert_wait_snapshot(&h, &session, &a, reset, true).await;
                } else {
                    event(
                        &mut phases,
                        "initial_refused",
                        &a,
                        PHASE,
                        "urgent Move must refuse optional proof wait",
                    )
                    .await;
                    assert_no_more_events(&mut events);
                }
                assert_eq!(h.fake.state.requests().len(), 1);
                if urgent == "wait" {
                    event(
                        &mut events,
                        "reselect",
                        &a,
                        (reset - Utc::now()).to_std().unwrap() + PHASE,
                        "Wait must finish at its original reset",
                    )
                    .await;
                }
                timeout(Duration::from_secs(10), sent)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(h.fake.state.requests().len(), 2);
                assert!(
                    h.fake.state.requests()[1].received_at >= earliest,
                    "urgent Move must still honor native near-reset cache hold"
                );
                assert_source(&h, 1, &a);
                release.send(()).unwrap();
            };
            let ((status, account, body), ()) = timeout(Duration::from_secs(15), async {
                tokio::join!(target(&h, &session), observe)
            })
            .await
            .unwrap();
            assert_eq!(
                (status, account.as_str(), body.as_slice()),
                (200, a.as_str(), FINAL.as_bytes())
            );
            clean(&h, &a, &b).await;
            assert_wait_snapshot(&h, &session, &a, reset, false).await;
            assert_exact_attempts(&h, 2);
            assert_eq!(usage.state.requests().len(), 1);
            assert_no_more_events(&mut events);
            // Native account notifications may renew the same receipt. They
            // do not admit a second wait or extend its original reset envelope.
            loop {
                match phases.try_recv() {
                    Ok(phase) => {
                        assert_eq!(phase.source, a);
                        assert_eq!(phase.stage, "inspection_started");
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(error) => panic!("native inspection observer lost: {error}"),
                }
            }
        }
    }
}

#[tokio::test]
async fn n5c_http_long_weekly_limit_moves_for_either_urgency_except_strict() {
    for mode in ["recover", "rebalance", "strict"] {
        for urgent in ["wait", "move"] {
            let session = format!("synthetic-n5c-weekly-{mode}-{urgent}");
            let (h, a, b, usage) = behavior_fixture(&session, mode, urgent).await;
            let reset = Utc::now() + ChronoDuration::days(2);
            usage.state.push(ScriptedResponse::json(
                StatusCode::OK,
                json!({"rate_limit":{
                    "primary_window":{"used_percent":10,"limit_window_seconds":18000,
                        "reset_at":(Utc::now()+ChronoDuration::hours(1)).timestamp()},
                    "secondary_window":{"used_percent":100,"limit_window_seconds":604800,
                        "reset_at":reset.timestamp()},"active_limit":"7d"
                }})
                .to_string(),
            ));
            let account = timeout(PHASE, h.state.quota.poll(&a))
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                account.health,
                Health::Cooling {
                    reason: HealthReason::Quota,
                    ..
                }
            ));
            assert_eq!(account.quota_cooling.as_ref().unwrap().window, "7d");
            assert_eq!(
                account.quota_cooling.as_ref().unwrap().reset_at.timestamp(),
                reset.timestamp()
            );
            let mut events = h.state.subscribe_http_quota_wait();
            let mut phases = h.state.subscribe_http_quota_wait_inspections();
            h.fake
                .state
                .push(ScriptedResponse::json(StatusCode::OK, FINAL));
            let (status, account, body) = timeout(PHASE, target(&h, &session))
                .await
                .expect("long weekly limit must not become an optional/native reset wait");
            event(
                &mut phases,
                "inspection_started",
                &a,
                PHASE,
                "weekly native inspection",
            )
            .await;
            event(
                &mut phases,
                "initial_refused",
                &a,
                PHASE,
                "weekly cannot qualify 5h proof",
            )
            .await;
            assert_no_more_events(&mut events);
            if mode == "strict" {
                assert_eq!(status, 409);
                let error: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(error["error"]["type"], "routing_policy_blocked");
                assert_exact_attempts(&h, 1);
            } else {
                assert_eq!(
                    (status, account.as_str(), body.as_slice()),
                    (200, b.as_str(), FINAL.as_bytes())
                );
                assert_eq!(h.fake.state.requests().len(), 2);
                assert_source(&h, 1, &b);
                assert_one_move(&h, &a, &b);
                assert_eq!(
                    h.state
                        .bindings
                        .current(Some(&format!("codex:{session}")))
                        .as_deref(),
                    Some(b.as_str())
                );
            }
            clean(&h, &a, &b).await;
            assert_eq!(usage.state.requests().len(), 1);
            assert_no_more_events(&mut phases);
        }
    }
}

#[tokio::test]
async fn n5c_http_permitted_serving_credits_continue_on_a_for_either_urgency_and_strict() {
    for mode in ["recover", "rebalance", "strict"] {
        for urgent in ["wait", "move"] {
            let session = format!("synthetic-n5c-paid-{mode}-{urgent}");
            let (h, a, b, usage) = behavior_fixture(&session, mode, urgent).await;
            timeout(
                PHASE,
                h.admin(
                    reqwest::Method::PATCH,
                    &format!("/admin/v1/accounts/{a}"),
                    Some(json!({"allow_credits":true})),
                ),
            )
            .await
            .unwrap();
            usage.state.push(ScriptedResponse::json(StatusCode::OK, json!({
                "credits":{"has_credits":true,"unlimited":false,"balance":"12.50"},
                "rate_limit":{"limit_reached":true,"active_limit":"credits","serving_from_credits":true,
                    "primary_window":{"used_percent":100,"limit_window_seconds":18000,
                        "reset_at":(Utc::now()+ChronoDuration::hours(1)).timestamp()},
                    "secondary_window":{"used_percent":100,"limit_window_seconds":604800,
                        "reset_at":(Utc::now()+ChronoDuration::days(2)).timestamp()}}
            }).to_string()));
            let account = timeout(PHASE, h.state.quota.poll(&a))
                .await
                .unwrap()
                .unwrap();
            assert!(account.allow_credits);
            assert!(matches!(account.health, Health::Ready));
            assert!(account.quota_cooling.is_none());
            let mut events = h.state.subscribe_http_quota_wait();
            let mut phases = h.state.subscribe_http_quota_wait_inspections();
            h.fake
                .state
                .push(ScriptedResponse::json(StatusCode::OK, FINAL));
            let (status, account, body) = timeout(PHASE, target(&h, &session))
                .await
                .expect("permitted credit serving is not a limit");
            assert_eq!(
                (status, account.as_str(), body.as_slice()),
                (200, a.as_str(), FINAL.as_bytes())
            );
            assert_source(&h, 1, &a);
            assert_exact_attempts(&h, 2);
            event(
                &mut phases,
                "inspection_started",
                &a,
                PHASE,
                "credit native inspection",
            )
            .await;
            event(
                &mut phases,
                "initial_refused",
                &a,
                PHASE,
                "credits cannot qualify exhausted proof",
            )
            .await;
            assert_no_more_events(&mut events);
            assert_no_more_events(&mut phases);
            clean(&h, &a, &b).await;
            assert_eq!(usage.state.requests().len(), 1);
        }
    }
}

// D26 fallback acceptance: preserve the actual provider 429; never manufacture
// one from a usage poll. Current N5a over-bound behavior instead selects B.
async fn far_five_hour(urgent: &str) {
    for mode in ["recover", "rebalance"] {
        let session = format!("synthetic-n5c-far-{urgent}-{mode}");
        let (h, a, b, usage) = behavior_fixture(&session, mode, urgent).await;
        h.fake.state.push(far_limit());
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, FINAL)); // catch wrong failover
        let (status, _account, body) = timeout(Duration::from_secs(12), target(&h, &session))
            .await
            .expect("nonurgent fallback must return the actual limit within original budget");
        assert_source(&h, 1, &a);
        if urgent == "wait" {
            assert_eq!(
                status, 429,
                "nonurgent beyond-bound 5h must preserve provider limit instead of moving B"
            );
            assert!(
                body.is_empty(),
                "actual upstream empty limit body must remain verbatim"
            );
            assert_exact_attempts(&h, 2);
            assert_eq!(
                h.state
                    .bindings
                    .current(Some(&format!("codex:{session}")))
                    .as_deref(),
                Some(a.as_str())
            );
        } else {
            assert_eq!(status, 200);
            assert_eq!(body.as_slice(), FINAL.as_bytes());
            assert_eq!(h.fake.state.requests().len(), 3);
            assert_source(&h, 2, &b);
            assert_one_move(&h, &a, &b);
            assert_eq!(
                h.state
                    .bindings
                    .current(Some(&format!("codex:{session}")))
                    .as_deref(),
                Some(b.as_str())
            );
        }
        clean(&h, &a, &b).await;
        assert!(usage.state.requests().is_empty());
    }
}

#[tokio::test]
async fn n5c_http_nonurgent_far_5h_preserves_actual_429_without_spending_b() {
    far_five_hour("wait").await;
}

#[tokio::test]
async fn n5c_http_urgent_far_5h_normally_moves_b_with_verbatim_upload() {
    far_five_hour("move").await;
}
