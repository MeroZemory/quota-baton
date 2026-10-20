use super::*;

#[tokio::test]
async fn large_streaming_input_is_not_moved_based_on_first_small_chunk() {
    let h = Harness::new(Provider::Codex).await;
    let ids = h.ids();
    h.pause(&ids[1]).await;
    for n in 0..6 {
        assert_eq!(
            h.turn(
                "/v1/responses",
                &[("session-id", &format!("large-cost-{n}"))],
                RAW
            )
            .await,
            ids[0]
        );
    }
    h.admin(
        reqwest::Method::POST,
        &format!("/admin/v1/accounts/{}/resume", ids[1]),
        None,
    )
    .await;
    h.drain(&ids[0]).await;
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "confirmed-target")], RAW)
            .await,
        ids[1]
    );
    h.admin(
        reqwest::Method::PATCH,
        &format!("/admin/v1/accounts/{}", ids[0]),
        Some(json!({"drain":false})),
    )
    .await;
    // The target's warm-up placement consumes its D20 weak-evidence slot.
    // Observe quota after that placement so this test reaches the overload mark
    // and isolates the large-body cost guard rather than the freshness cap.
    let asof = h.state.bindings.begin_usage_observation_for(&ids[1]);
    h.state
        .accounts
        .observe_headers(&ids[1], Provider::Codex, &usage(Provider::Codex, 10))
        .await;
    h.state.bindings.note_usage_observation_asof_token(
        &ids[1],
        &h.obs.quota_snapshot(&ids[1]).unwrap(),
        asof,
    );
    let candidates: Vec<_> = h
        .store
        .list()
        .unwrap()
        .into_iter()
        .map(|account| qb_core::bindings::Candidate {
            quota: h.obs.quota_snapshot(&account.id),
            account,
            inflight: 0,
        })
        .collect();
    for n in 0..3 {
        h.state.bindings.note_client_rate(
            Provider::Codex,
            &ids[0],
            Some(&format!("codex:large-cost-{n}")),
            &candidates,
            Utc::now(),
        );
    }
    let n = (0..6)
        .find(|n| {
            h.state
                .bindings
                .overload_marked(Some(&format!("codex:large-cost-{n}")), Utc::now())
        })
        .expect("confirmed target should create a real mark");
    let body = format!(r#"{{"input":"{}"}}"#, "x".repeat(9 * 1024 * 1024));
    let before = h.fake.state.requests().len();
    assert_eq!(
        h.turn(
            "/v1/responses",
            &[("session-id", &format!("large-cost-{n}"))],
            &body
        )
        .await,
        ids[0]
    );
    assert_credential(&h, before, &ids[0]);
    assert_eq!(h.fake.state.requests()[before].body.len(), body.len());
    assert!(
        h.ledger()
            .iter()
            .any(|row| row["bytes_in"] == body.len() as u64),
        "routing estimate must not double-count ledger bytes"
    );
}

#[tokio::test]
async fn restored_claude_root_cannot_move_on_small_child_cost_evidence() {
    let h = Harness::new(Provider::Claude).await;
    let a = h
        .turn(
            "/v1/messages",
            &[("x-claude-code-session-id", "restored-parent")],
            "{}",
        )
        .await;
    h.drain(&a).await;
    let mut snapshot = h.state.bindings.export().unwrap();
    for b in snapshot["bindings"].as_array_mut().unwrap() {
        b["last_used"] = json!(Utc::now() - ChronoDuration::seconds(3601));
    }
    h.state.bindings.import(snapshot).unwrap();
    let b = h
        .turn(
            "/v1/messages",
            &[("x-claude-code-session-id", "confirmed-target")],
            "{}",
        )
        .await;
    assert_ne!(a, b);
    assert_eq!(
        h.turn(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", "restored-parent"),
                ("x-claude-code-agent-id", "small")
            ],
            "{}"
        )
        .await,
        a
    );
    assert_credential(&h, 2, &a);
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "binding_move" && row["from"] == a && row["to"] == b)
    );
}

#[tokio::test]
async fn initial_rate_wait_expiry_forces_one_same_account_send_for_both_families() {
    for provider in [Provider::Claude, Provider::Codex] {
        let h = Harness::with_limits(provider, 60, 5, 1).await;
        let (path, header) = if provider == Provider::Claude {
            ("/v1/messages", "x-claude-code-session-id")
        } else {
            ("/v1/responses", "session-id")
        };
        let a = h.turn(path, &[(header, "forced-rate-expiry")], RAW).await;
        h.state
            .accounts
            .observe_429(&a, Some(Duration::from_secs(30)));
        let before = h.fake.state.requests().len();
        let started = Instant::now();
        assert_eq!(
            h.turn(path, &[(header, "forced-rate-expiry")], RAW).await,
            a
        );
        assert!(started.elapsed() >= Duration::from_millis(900));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(h.fake.state.requests().len(), before + 1);
        assert_credential(&h, before, &a);
        assert!(matches!(h.store.load(&a).unwrap().health, Health::Ready));
        assert_eq!(
            h.fake.state.requests()[before].body.as_ref(),
            RAW.as_bytes()
        );
    }
}

#[tokio::test]
async fn existing_project_association_keeps_anchor_when_binding_snapshot_is_lost() {
    let h = Harness::new(Provider::Codex).await;
    let a = h
        .turn("/v1/responses", &[("session-id", "project-existing")], RAW)
        .await;
    let project=h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"project","name":"existing","settings":{"mode":"strict","reset_wait_secs":0}}))).await;
    // This is the same atomic table entry point used by project ingress;
    // crash before a later choose can be tested without racing file watchers.
    h.state
        .bindings
        .associate_project("codex:project-existing", project["id"].as_str().unwrap())
        .unwrap();
    let h = h
        .restart_with_fixture(|home| {
            fs::write(
                home.join("state/bindings.json"),
                br#"{"schema":1,"bindings":[]}"#,
            )
            .unwrap();
        })
        .await;
    h.pause(&a).await;
    assert_eq!(
        h.send("/v1/responses", &[("session-id", "project-existing")], RAW)
            .await
            .0,
        409
    );
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn ordinary_delayed_401_refresh_is_not_terminated_by_rate_budget() {
    let h = Harness::with_limits(Provider::Codex, 60, 5, 1).await;
    for id in h.ids() {
        let mut a = h.store.load(&id).unwrap();
        let tokens = h.store.tokens(&a).unwrap();
        a.source = qb_core::accounts::Source::Oauth;
        h.store.save(&a, &tokens, Some(a.generation)).unwrap();
    }
    let mut first = ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"invalid_api_key"}}"#,
    );
    first.chunks[0].0 = Duration::from_millis(1200);
    h.fake.state.push(first);
    h.fake.state.push(ScriptedResponse::json(StatusCode::OK,r#"{"access_token":"synthetic-rotated-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600}"#));
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{}"));
    let result = h
        .send("/v1/responses", &[("session-id", "delayed-auth")], RAW)
        .await;
    assert_eq!(result.0, 200);
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 3);
    assert!(String::from_utf8_lossy(&requests[1].body).contains("refresh_token"));
    assert_eq!(
        requests[2].headers["authorization"],
        "Bearer synthetic-rotated-access"
    );
    assert_eq!(
        requests[0].headers["chatgpt-account-id"],
        requests[2].headers["chatgpt-account-id"]
    );
}

#[tokio::test]
async fn strict_http_payment_and_permission_prioritize_explicit_policy_block() {
    for status in [StatusCode::PAYMENT_REQUIRED, StatusCode::FORBIDDEN] {
        let h = Harness::new(Provider::Codex).await;
        let headers = [("session-id", "strict-denial")];
        let a = h.turn("/v1/responses", &headers, RAW).await;
        h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:strict-denial"),"settings":{"mode":"strict","reset_wait_secs":0}}))).await;
        h.fake.state.push(ScriptedResponse::json(
            status,
            r#"{"error":{"type":"permission_error"}}"#,
        ));
        let result = h.send("/v1/responses", &headers, RAW).await;
        assert_eq!(result.0, 409);
        assert!(String::from_utf8_lossy(&result.2).contains("routing_policy_blocked"));
        assert_eq!(h.fake.state.requests().len(), 2);
        assert_credential(&h, 1, &a);
    }
}

#[tokio::test]
async fn drain_during_new_placement_wait_cannot_create_a_phantom_warm_binding() {
    let h = Harness::new(Provider::Codex).await;
    let ids = h.ids();
    for id in &ids {
        h.state
            .accounts
            .observe_429(id, Some(Duration::from_secs(2)));
    }
    let send = h.send("/v1/responses", &[("session-id", "new-drain")], RAW);
    let edit = async {
        let selected = tokio::time::timeout(DEADLINE, async {
            loop {
                if let Some(id) = ids
                    .iter()
                    .find(|id| h.state.accounts.pacing_snapshot(id).waiting > 0)
                {
                    break id.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        h.drain(&selected).await;
        selected
    };
    let (reply, drained) = tokio::join!(send, edit);
    assert_ne!(reply.0, 200);
    assert!(h.fake.state.requests().is_empty());
    let next = h
        .turn("/v1/responses", &[("session-id", "new-drain")], RAW)
        .await;
    assert_ne!(next, drained);
    assert_credential(&h, 0, &next);
}

#[tokio::test]
async fn total_precommit_deadline_cancels_hold_without_sending_or_changing_account() {
    let h = Harness::with_limits(Provider::Codex, 60, 1, 60).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "total-deadline")], RAW)
        .await;
    let now = Utc::now();
    h.store
        .update_metadata(&first, |a| {
            a.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: now + ChronoDuration::seconds(20),
                scope: "account".into(),
            };
            a.quota_cooling = Some(QuotaCooling {
                window: "5h".into(),
                reset_at: now + ChronoDuration::seconds(20),
                started_at: now,
            });
        })
        .unwrap();
    let start = Instant::now();
    let response = h
        .send("/v1/responses", &[("session-id", "total-deadline")], RAW)
        .await;
    assert_eq!(response.0, 504);
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_eq!(
        h.state
            .bindings
            .current(Some("codex:total-deadline"))
            .as_deref(),
        Some(first.as_str())
    );
}

#[tokio::test]
async fn simultaneous_initial_rate_expiry_sends_once_each_and_relays_actual_429() {
    let h = Harness::with_limits(Provider::Codex, 60, 5, 1).await;
    let headers = [("session-id", "expiry-herd")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    h.state
        .accounts
        .observe_429(&first, Some(Duration::from_secs(30)));
    let raw_rate = r#"{"error":{"type":"rate_limit_error","message":"synthetic-forced-final"}}"#;
    for _ in 0..8 {
        let mut rate = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, raw_rate);
        rate.headers.insert("retry-after", "30".parse().unwrap());
        h.fake.state.push(rate);
    }
    let began = Instant::now();
    let replies =
        futures_util::future::join_all((0..8).map(|_| h.send("/v1/responses", &headers, RAW)))
            .await;
    assert!(began.elapsed() >= Duration::from_millis(900));
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "initial expiry must not renew the rate wait"
    );
    for (status, account, body) in replies {
        assert_eq!(status, 429);
        assert_eq!(account, first);
        assert_eq!(body, raw_rate.as_bytes());
    }
    let requests = h.fake.state.requests();
    assert_eq!(
        requests.len(),
        9,
        "each initial expiry sends once, with no RATE retry"
    );
    for (index, request) in requests.iter().enumerate().skip(1) {
        assert_credential(&h, index, &first);
        assert_eq!(request.body.as_ref(), RAW.as_bytes());
        assert_eq!(request.headers["session-id"], "expiry-herd");
    }
    let rows: Vec<_> = h
        .ledger()
        .into_iter()
        .filter(|row| row["req"].is_string() && row["status"] == 429)
        .collect();
    assert_eq!(rows.len(), 8);
    for row in rows {
        assert_eq!(row["attempts"], 1);
        assert_eq!(row["rate_retries"], 0);
        assert_eq!(row["relayed_rate_429s"], 1);
        assert_eq!(row["outcome"], "rate_hold_relayed_429");
        assert!(row["rate_wait_ms"].as_u64().unwrap() >= 900);
    }
    assert!(matches!(
        h.store.load(&first).unwrap().health,
        Health::Ready
    ));
}

#[tokio::test]
async fn rate_expiry_returns_original_upstream_429_without_final_forced_retry() {
    let h = Harness::with_limits(Provider::Codex, 60, 5, 1).await;
    let mut rate = ScriptedResponse::json(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"type":"rate_limit_error","message":"synthetic-rate-original"}}"#,
    );
    rate.headers.insert("retry-after", "3".parse().unwrap());
    h.fake.state.push(rate);
    let reply = h
        .send("/v1/responses", &[("session-id", "original-429")], RAW)
        .await;
    assert_eq!(reply.0, 429);
    assert!(String::from_utf8_lossy(&reply.2).contains("synthetic-rate-original"));
    assert_eq!(h.fake.state.requests().len(), 1);
}

fn assert_credential(h: &Harness, index: usize, id: &str) {
    let account = h.store.load(id).unwrap();
    let token = h.store.tokens(&account).unwrap();
    let request = &h.fake.state.requests()[index];
    assert_eq!(
        request.headers["authorization"].to_str().unwrap(),
        format!("Bearer {}", token.access_token.as_str())
    );
    if let qb_core::accounts::Identity::Codex { account_id, .. } = account.identity {
        assert_eq!(request.headers["chatgpt-account-id"], account_id);
    }
}

#[tokio::test]
async fn queued_request_cannot_dispatch_old_account_after_move_and_strict_edit() {
    let h = Harness::new(Provider::Codex).await;
    let headers = [("session-id", "dispatch-race")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    assert_credential(&h, 0, &first);
    h.state
        .accounts
        .observe_429(&first, Some(Duration::from_secs(2)));
    let waiting = h.send("/v1/responses", &headers, RAW);
    let edit = async {
        tokio::time::timeout(DEADLINE, async {
            while h.state.accounts.pacing_snapshot(&first).waiting == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        h.pause(&first).await;
        let next = h.turn("/v1/responses", &headers, RAW).await;
        assert_ne!(first, next);
        assert_credential(&h, 1, &next);
        h.admin(reqwest::Method::POST, "/admin/v1/routing", Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:dispatch-race"),"settings":{"mode":"strict","reset_wait_secs":0}}))).await;
        h.admin(
            reqwest::Method::POST,
            &format!("/admin/v1/accounts/{first}/resume"),
            None,
        )
        .await;
        next
    };
    let (result, next) = tokio::join!(waiting, edit);
    assert_eq!(result.0, 409);
    assert_eq!(
        h.fake.state.requests().len(),
        2,
        "old admitted request must never send A after strict B"
    );
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, next);
    assert_credential(&h, 2, &next);
}

#[tokio::test]
async fn legacy_codex_strict_policy_survives_restart_with_independent_agent_key() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "legacy")], RAW)
        .await;
    let anchor = first.clone();
    let h = h
        .restart_with_fixture(move |home| {
            let id = qb_core::routing::session_id("codex:legacy");
            let policy = json!({"schema":1,"revision":4,"projects":{},"sessions":{
            id:{"project":null,"baseline":{"mode":"rebalance","reset_wait_secs":60},
            "explicit":{"mode":"strict","reset_wait_secs":0},"anchor":anchor,"movements":[]}}});
            fs::write(
                home.join("state/routing-policies.json"),
                serde_json::to_vec(&policy).unwrap(),
            )
            .unwrap();
            fs::write(
                home.join("state/bindings.json"),
                br#"{"schema":1,"bindings":[]}"#,
            )
            .unwrap();
        })
        .await;
    assert_eq!(
        h.turn(
            "/v1/responses",
            &[("session-id", "legacy:agent:independent")],
            RAW
        )
        .await,
        first
    );
    assert_credential(&h, 1, &first);
    h.pause(&first).await;
    assert_eq!(
        h.send(
            "/v1/responses",
            &[("session-id", "legacy:agent:independent")],
            RAW
        )
        .await
        .0,
        409
    );
    assert_eq!(h.fake.state.requests().len(), 2);
    // Explicit update writes v2, preserving the legacy alias across another restart.
    let project=h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"project","name":"unrelated","settings":{"mode":"rebalance","reset_wait_secs":0}}))).await;
    h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:legacy:agent:independent"),"project":project["id"]}))).await;
    let h = h.restart().await;
    assert_eq!(
        h.send(
            "/v1/responses",
            &[("session-id", "legacy:agent:independent")],
            RAW
        )
        .await
        .0,
        409
    );
    assert_eq!(h.fake.state.requests().len(), 2);
}

#[tokio::test]
async fn policy_edit_interrupts_reset_hold_instead_of_waiting_for_obsolete_deadline() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "interrupt-hold")], RAW)
        .await;
    let now = Utc::now();
    h.store
        .update_metadata(&first, |a| {
            a.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: now + ChronoDuration::seconds(20),
                scope: "account".into(),
            };
            a.quota_cooling = Some(QuotaCooling {
                window: "5h".into(),
                reset_at: now + ChronoDuration::seconds(20),
                started_at: now,
            });
        })
        .unwrap();
    let waiting = h.send("/v1/responses", &[("session-id", "interrupt-hold")], RAW);
    let edit = async {
        // Select has no reservation while holding. Wait until request tracking
        // proves the real relay entered, then withdraw the hold permission.
        tokio::time::timeout(DEADLINE, async {
            while h
                .state
                .inflight_requests
                .load(std::sync::atomic::Ordering::Relaxed)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:interrupt-hold"),"settings":{"mode":"strict","reset_wait_secs":0}}))).await;
    };
    let start = Instant::now();
    let (reply, ()) = tokio::join!(waiting, edit);
    assert_eq!(reply.0, 409);
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn full_discretionary_pool_budget_preserves_healthy_credential_but_allows_forced_move() {
    let h = Harness::new(Provider::Codex).await;
    let headers = [("session-id", "full-move-budget")];
    let a = h.turn("/v1/responses", &headers, RAW).await;
    let key = "codex:full-move-budget";
    // Fill the discretionary pool cap with distinct groups/targets. Forced
    // history is exempt from all movement caps and cannot seed this condition.
    for n in 0..h.state.bindings.policies.limits().max_pool_moves {
        h.state
            .bindings
            .policies
            .dispatch(
                Some(&format!("codex:budget-seed-{n}")),
                &format!("cx-budget-{n}"),
                Some("cx-origin"),
                "overload",
                Utc::now(),
            )
            .unwrap();
    }
    assert!(
        !h.state
            .bindings
            .policies
            .discretionary_allowed(key, Some(&a), Utc::now())
    );
    let mut snapshot = h.state.bindings.export().unwrap();
    for binding in snapshot["bindings"].as_array_mut().unwrap() {
        binding["last_used"] = json!(Utc::now() - ChronoDuration::seconds(3601));
    }
    h.state.bindings.import(snapshot).unwrap();
    h.drain(&a).await;
    let b = h
        .turn("/v1/responses", &[("session-id", "warm-target")], RAW)
        .await;
    assert_ne!(a, b);
    assert_credential(&h, 1, &b);
    // Keep D20's target cap out of this test: quota observed after the
    // warm-up placement makes B a genuine candidate for a discretionary move.
    let asof = h.state.bindings.begin_usage_observation_for(&b);
    h.state
        .accounts
        .observe_headers(&b, Provider::Codex, &usage(Provider::Codex, 10))
        .await;
    h.state.bindings.note_usage_observation_asof_token(
        &b,
        &h.obs.quota_snapshot(&b).unwrap(),
        asof,
    );
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, a);
    assert_credential(&h, 2, &a);
    assert!(
        !h.state
            .bindings
            .policies
            .discretionary_allowed(key, Some(&b), Utc::now())
    );
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "binding_move" && row["from"] == a && row["to"] == b)
    );
    h.pause(&a).await;
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, b);
    assert_credential(&h, 3, &b);
    assert_eq!(h.fake.state.requests().len(), 4);
    assert_eq!(h.fake.state.requests()[3].body.as_ref(), RAW.as_bytes());
    assert!(h.ledger().iter().any(|row| row["event"] == "binding_move"
        && row["from"] == a
        && row["to"] == b
        && row["reason"] == "forced"));
}

#[tokio::test]
async fn strict_codex_pause_and_restart_never_dispatch_to_other_account() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "strict-root")], RAW)
        .await;
    assert_credential(&h, 0, &first);
    h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:strict-root"),"settings":{"mode":"strict","reset_wait_secs":0}}))).await;
    h.pause(&first).await;
    let count = h.fake.state.requests().len();
    let (status, _, body) = h
        .send("/v1/responses", &[("session-id", "strict-root")], RAW)
        .await;
    assert_eq!(status, 409);
    assert!(String::from_utf8_lossy(&body).contains("routing_policy_blocked"));
    assert_eq!(
        h.fake.state.requests().len(),
        count,
        "paused strict group must not hit a different upstream credential"
    );
    let h = h
        .restart_with_fixture(|home| {
            fs::write(
                home.join("state/bindings.json"),
                br#"{"schema":1,"bindings":[]}"#,
            )
            .unwrap();
        })
        .await;
    assert_eq!(
        h.send("/v1/responses", &[("session-id", "strict-root")], RAW)
            .await
            .0,
        409
    );
    assert_eq!(
        h.fake.state.requests().len(),
        count,
        "durable strict intent outlives binding eviction"
    );
    let other = h
        .turn("/v1/responses", &[("session-id", "new-root")], RAW)
        .await;
    assert_ne!(other, first);
}

#[tokio::test]
async fn registered_project_applies_before_first_dispatch_and_claude_child_inherits() {
    let h = Harness::new(Provider::Claude).await;
    let result=h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"project","name":"합성 프로젝트","settings":{"mode":"strict","reset_wait_secs":0}}))).await;
    let project = result["id"].as_str().unwrap();
    let first = h
        .turn(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", "project-root"),
                ("x-quota-baton-project", project),
            ],
            "{}",
        )
        .await;
    assert_eq!(
        h.turn(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", "project-root"),
                ("x-claude-code-agent-id", "child")
            ],
            "{}"
        )
        .await,
        first
    );
    assert_credential(&h, 0, &first);
    assert_credential(&h, 1, &first);
    for recorded in h.fake.state.requests() {
        assert!(!recorded.headers.contains_key("x-quota-baton-project"));
        assert_eq!(recorded.body.as_ref(), b"{}");
    }
    h.pause(&first).await;
    let before = h.fake.state.requests().len();
    assert_eq!(
        h.send(
            "/v1/messages",
            &[
                ("x-claude-code-session-id", "project-root"),
                ("x-claude-code-agent-id", "child")
            ],
            "{}"
        )
        .await
        .0,
        409
    );
    assert_eq!(h.fake.state.requests().len(), before);
}

#[tokio::test]
async fn recover_mode_moves_only_after_real_account_unavailability() {
    let h = Harness::new(Provider::Codex).await;
    let first = h
        .turn("/v1/responses", &[("session-id", "recover-root")], RAW)
        .await;
    h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:recover-root"),"settings":{"mode":"recover","reset_wait_secs":0}}))).await;
    h.drain(&first).await;
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "recover-root")], RAW)
            .await,
        first
    );
    assert_credential(&h, 0, &first);
    assert_credential(&h, 1, &first);
    h.pause(&first).await;
    let next = h
        .turn("/v1/responses", &[("session-id", "recover-root")], RAW)
        .await;
    assert_credential(&h, 2, &next);
    assert_ne!(next, first);
}

#[tokio::test]
async fn restored_codex_root_continuation_does_not_claim_full_context_cost_or_move() {
    let h = Harness::new(Provider::Codex).await;
    let a = h
        .turn("/v1/responses", &[("session-id", "continued-root")], RAW)
        .await;
    h.drain(&a).await;
    let mut snapshot = h.state.bindings.export().unwrap();
    for binding in snapshot["bindings"].as_array_mut().unwrap() {
        binding["last_used"] = json!(Utc::now() - ChronoDuration::seconds(3601));
    }
    h.state.bindings.import(snapshot).unwrap();
    let b = h
        .turn("/v1/responses", &[("session-id", "continued-target")], RAW)
        .await;
    assert_ne!(a, b);
    let continuation =
        r#"{"model":"fixture-model","previous_response_id":"synthetic-prior","input":"delta"}"#;
    assert_eq!(
        h.turn(
            "/v1/responses",
            &[("session-id", "continued-root")],
            continuation
        )
        .await,
        a
    );
    assert_credential(&h, 2, &a);
    assert_eq!(
        h.fake.state.requests()[2].body.as_ref(),
        continuation.as_bytes()
    );
    assert!(
        !h.ledger()
            .iter()
            .any(|row| row["event"] == "binding_move" && row["from"] == a && row["to"] == b)
    );
}

#[tokio::test]
async fn spent_rate_budget_allows_slow_auth_retry_but_never_another_rate_retry() {
    let h = Harness::with_limits(Provider::Codex, 60, 10, 3).await;
    for id in h.ids() {
        let mut a = h.store.load(&id).unwrap();
        let tokens = h.store.tokens(&a).unwrap();
        a.source = qb_core::accounts::Source::Oauth;
        h.store.save(&a, &tokens, Some(a.generation)).unwrap();
    }
    let mut rate = ScriptedResponse::json(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"type":"rate_limit_error"}}"#,
    );
    rate.headers.insert("retry-after", "1".parse().unwrap());
    h.fake.state.push(rate);
    let mut auth = ScriptedResponse::json(
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"code":"invalid_api_key"}}"#,
    );
    auth.chunks[0].0 = Duration::from_millis(2200);
    h.fake.state.push(auth);
    h.fake.state.push(ScriptedResponse::json(StatusCode::OK,r#"{"access_token":"synthetic-rotated-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600}"#));
    let final_body =
        r#"{"error":{"type":"rate_limit_error","message":"synthetic-after-auth-rate"}}"#;
    let mut final_rate = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, final_body);
    final_rate
        .headers
        .insert("retry-after", "30".parse().unwrap());
    h.fake.state.push(final_rate);
    let result = h
        .send(
            "/v1/responses",
            &[("session-id", "rate-then-slow-auth")],
            RAW,
        )
        .await;
    assert_eq!(result.0, 429);
    assert_eq!(
        result.2,
        final_body.as_bytes(),
        "relay the actual final 429 after independent auth recovery"
    );
    let requests = h.fake.state.requests();
    assert_eq!(
        requests.len(),
        4,
        "one RATE retry, one token refresh, and one independent auth retry only"
    );
    assert_eq!(
        requests[0].headers["chatgpt-account-id"],
        requests[1].headers["chatgpt-account-id"]
    );
    assert!(String::from_utf8_lossy(&requests[2].body).contains("refresh_token"));
    assert_eq!(
        requests[3].headers["authorization"],
        "Bearer synthetic-rotated-access"
    );
    for index in [0, 1, 3] {
        assert_eq!(requests[index].body.as_ref(), RAW.as_bytes());
        assert_eq!(
            requests[index].headers["chatgpt-account-id"],
            requests[0].headers["chatgpt-account-id"]
        );
    }
    let rows: Vec<_> = h
        .ledger()
        .into_iter()
        .filter(|row| row["req"].is_string())
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["attempts"], 3);
    assert_eq!(rows[0]["rate_retries"], 1);
    assert_eq!(rows[0]["relayed_rate_429s"], 1);
    assert_eq!(rows[0]["outcome"], "rate_hold_relayed_429");
}

#[tokio::test]
async fn global_strict_is_rejected_and_explicit_strict_corrupt_restore_preserves_evidence() {
    for provider in [Provider::Codex, Provider::Claude] {
        for global in [true, false] {
            let h = Harness::new(provider).await;
            let (path, header) = if provider == Provider::Codex {
                ("/v1/responses", "session-id")
            } else {
                ("/v1/messages", "x-claude-code-session-id")
            };
            let key = format!("{}:legacy-corrupt", provider.as_str());
            let a = h.turn(path, &[(header, "legacy-corrupt")], RAW).await;
            let b = h.ids().into_iter().find(|id| id != &a).unwrap();
            assert_credential(&h, 0, &a);
            h.pause(&a).await;
            h.state.bindings.freeze().unwrap();
            h.obs.flush().unwrap();
            let Harness {
                mut servers,
                state,
                store,
                obs,
                fake,
                client,
                home,
                _root,
                _serial,
            } = h;
            servers.data_task.abort();
            servers.admin_task.abort();
            let _ = (&mut servers.data_task).await;
            let _ = (&mut servers.admin_task).await;
            drop(client);
            drop(servers);
            drop(state);
            let mut config = Config::default();
            config.server.data_port = 0;
            config.server.admin_port = 0;
            config.routing.mode = if global {
                qb_core::routing::Mode::Strict
            } else {
                qb_core::routing::Mode::Rebalance
            };
            config.upstream.codex_base = fake.url.clone();
            config.upstream.anthropic_base = fake.url.clone();
            if global {
                assert!(
                    config.routing.validate().is_err(),
                    "global strict must not be accepted"
                );
                continue;
            }
            if !global {
                let policy = qb_core::routing::PolicyStore::new(
                    &home,
                    qb_core::routing::Settings::default(),
                )
                .unwrap();
                policy
                    .set_session(
                        &qb_core::routing::session_id(&key),
                        Some(qb_core::routing::Settings {
                            mode: qb_core::routing::Mode::Strict,
                            reset_wait_secs: 0,
                        }),
                        None,
                        None,
                    )
                    .unwrap();
            }
            let stamp = json!(Utc::now());
            let duplicate_field = format!(
                r#"{{"schema":1,"bindings":[{{"key":"{key}","account":"{a}","account":"{b}","last_used":{stamp}}}]}}"#
            );
            let mut cases=vec!["{".to_owned(),json!({"schema":99,"bindings":[]}).to_string(),json!({"schema":1,"bindings":[{"key":key,"account":a,"last_used":stamp},{"key":key,"account":a,"last_used":stamp}]}).to_string(),duplicate_field];
            if provider == Provider::Claude {
                cases.push(json!({"schema":1,"bindings":[{"key":key,"account":a,"last_used":stamp},{"key":format!("{key}:agent:x"),"account":b,"last_used":stamp}]}).to_string());
            }
            for raw in cases {
                let path = home.join("state/bindings.json");
                fs::write(&path, raw.as_bytes()).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                }
                for _ in 0..2 {
                    assert!(
                        RelayState::new(
                            store.clone(),
                            &config,
                            obs.clone(),
                            Arc::new(DebugPins::default())
                        )
                        .is_err(),
                        "legacy strict core started with no trustworthy anchor"
                    );
                    assert_eq!(fs::read(&path).unwrap(), raw.as_bytes());
                    assert_eq!(
                        fake.state.requests().len(),
                        1,
                        "corrupt restore dispatched an alternate credential"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn legacy_alias_anchor_recovery_and_v2_independence_use_the_actual_serving_credentials() {
    for (schema, divergent) in [(1, false), (1, true), (2, true)] {
        let h = Harness::new(Provider::Codex).await;
        let a = h
            .turn("/v1/responses", &[("session-id", "alias-proof")], RAW)
            .await;
        let b = h.ids().into_iter().find(|id| id != &a).unwrap();
        let paused = if divergent { &a } else { &b };
        h.pause(paused).await;
        let child = h
            .turn(
                "/v1/responses",
                &[("session-id", "alias-proof:agent:x")],
                RAW,
            )
            .await;
        assert_eq!(child, if divergent { b.clone() } else { a.clone() });
        assert_credential(&h, 0, &a);
        assert_credential(&h, 1, &child);
        h.admin(
            reqwest::Method::POST,
            &format!("/admin/v1/accounts/{paused}/resume"),
            None,
        )
        .await;
        h.state.bindings.freeze().unwrap();
        h.obs.flush().unwrap();
        let Harness {
            mut servers,
            state,
            store,
            obs,
            fake,
            client,
            home,
            _root,
            _serial,
        } = h;
        servers.data_task.abort();
        servers.admin_task.abort();
        let _ = (&mut servers.data_task).await;
        let _ = (&mut servers.admin_task).await;
        drop(client);
        drop(servers);
        drop(state);
        let id = qb_core::routing::session_id("codex:alias-proof");
        let mut policy = json!({"schema":schema,"revision":1,"projects":{},"sessions":{id:{"project":null,"baseline":{"mode":"rebalance","reset_wait_secs":60},"explicit":{"mode":"strict","reset_wait_secs":0},"anchor":null,"movements":[]}}});
        if schema == 2 {
            policy["legacy_codex_roots"] = json!([]);
        }
        let policy_path = home.join("state/routing-policies.json");
        let policy_bytes = serde_json::to_vec(&policy).unwrap();
        fs::write(&policy_path, &policy_bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&policy_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let bindings_bytes = fs::read(home.join("state/bindings.json")).unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.routing.mode = qb_core::routing::Mode::Rebalance;
        config.upstream.codex_base = fake.url.clone();
        config.upstream.anthropic_base = fake.url.clone();
        if schema == 1 && divergent {
            for _ in 0..2 {
                assert!(
                    RelayState::new(
                        store.clone(),
                        &config,
                        obs.clone(),
                        Arc::new(DebugPins::default())
                    )
                    .is_err()
                );
                assert_eq!(fs::read(&policy_path).unwrap(), policy_bytes);
                assert_eq!(
                    fs::read(home.join("state/bindings.json")).unwrap(),
                    bindings_bytes
                );
                assert_eq!(fake.state.requests().len(), 2);
            }
            continue;
        }
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap(),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-client".into(),
                admin_token: "synthetic-admin".into(),
            },
            relay::router(state.clone()),
            qb_core::admin::runtime_routes(state.clone(), 0),
        )
        .await
        .unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let h = Harness {
            servers,
            state,
            store,
            obs,
            fake,
            client,
            home,
            _root,
            _serial,
        };
        h.pause(&a).await;
        assert_eq!(
            h.send("/v1/responses", &[("session-id", "alias-proof")], RAW)
                .await
                .0,
            409
        );
        assert_eq!(h.fake.state.requests().len(), 2);
        if schema == 1 {
            assert_eq!(
                h.send(
                    "/v1/responses",
                    &[("session-id", "alias-proof:agent:x")],
                    RAW
                )
                .await
                .0,
                409
            );
            assert_eq!(h.fake.state.requests().len(), 2);
        } else {
            assert_eq!(
                h.turn(
                    "/v1/responses",
                    &[("session-id", "alias-proof:agent:x")],
                    RAW
                )
                .await,
                b
            );
            assert_credential(&h, 2, &b);
        }
    }
}

#[tokio::test]
async fn rejected_idle_import_blocks_alias_dispatch_until_successful_recovery() {
    let h = Harness::new(Provider::Codex).await;
    let a = h
        .turn("/v1/responses", &[("session-id", "idle-alias")], RAW)
        .await;
    h.pause(&a).await;
    let b = h
        .turn(
            "/v1/responses",
            &[("session-id", "idle-alias:agent:x")],
            RAW,
        )
        .await;
    assert_ne!(a, b);
    assert_credential(&h, 0, &a);
    assert_credential(&h, 1, &b);
    h.admin(
        reqwest::Method::POST,
        &format!("/admin/v1/accounts/{a}/resume"),
        None,
    )
    .await;
    let snapshot = h.state.bindings.export().unwrap();
    let id = qb_core::routing::session_id("codex:idle-alias");
    let policy = json!({"schema":1,"revision":1,"projects":{},"sessions":{id:{"project":null,"baseline":{"mode":"rebalance","reset_wait_secs":60},"explicit":{"mode":"strict","reset_wait_secs":0},"anchor":null,"movements":[]}}});
    let path = h.home.join("state/routing-policies.json");
    let bytes = serde_json::to_vec(&policy).unwrap();
    fs::write(&path, &bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(
        h.state
            .bindings
            .import(snapshot.clone())
            .unwrap_err()
            .to_string()
            .contains("strict policy alias")
    );
    for session in ["idle-alias:agent:x", "idle-alias"] {
        assert_eq!(
            h.send("/v1/responses", &[("session-id", session)], RAW)
                .await
                .0,
            409
        );
    }
    assert_eq!(h.fake.state.requests().len(), 2);
    let n = h.fake.state.requests().len();
    let normal = h
        .turn(
            "/v1/responses",
            &[("session-id", "fresh-after-rejected-import")],
            RAW,
        )
        .await;
    assert_credential(&h, n, &normal);
    assert_eq!(h.fake.state.requests().len(), n + 1);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let mut repaired = snapshot;
    for binding in repaired["bindings"].as_array_mut().unwrap() {
        binding["account"] = json!(a);
    }
    h.state.bindings.import(repaired).unwrap();
    for session in ["idle-alias", "idle-alias:agent:x"] {
        let n = h.fake.state.requests().len();
        assert_eq!(
            h.turn("/v1/responses", &[("session-id", session)], RAW)
                .await,
            a
        );
        assert_credential(&h, n, &a);
    }
}

#[tokio::test]
async fn disappeared_policy_during_idle_import_preserves_strict_serving_lock() {
    let h = Harness::new(Provider::Codex).await;
    let a = h
        .turn("/v1/responses", &[("session-id", "missing-policy")], RAW)
        .await;
    assert_credential(&h, 0, &a);
    h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:missing-policy"),"settings":{"mode":"strict","reset_wait_secs":0}}))).await;
    let before = h.state.bindings.export().unwrap();
    fs::remove_file(h.home.join("state/routing-policies.json")).unwrap();
    assert!(h.state.bindings.import(before.clone()).is_err());
    assert_eq!(
        h.state
            .bindings
            .policies
            .plan(Some("codex:missing-policy"))
            .anchor
            .as_deref(),
        Some(a.as_str())
    );
    h.pause(&a).await;
    assert_eq!(
        h.send("/v1/responses", &[("session-id", "missing-policy")], RAW)
            .await
            .0,
        409
    );
    assert_eq!(h.fake.state.requests().len(), 1);
    assert_eq!(h.state.bindings.export().unwrap(), before);
    let h = h.restart_with_fixture(|_| {}).await;
    assert_eq!(
        h.send("/v1/responses", &[("session-id", "missing-policy")], RAW)
            .await
            .0,
        409
    );
    assert_eq!(h.fake.state.requests().len(), 1);
    let n = h.fake.state.requests().len();
    let normal = h
        .turn(
            "/v1/responses",
            &[("session-id", "not-locked-after-recovery")],
            RAW,
        )
        .await;
    assert_ne!(normal, a);
    assert_credential(&h, n, &normal);
    assert!(h.ledger().iter().any(|row| row["event"] == "core_warning"
        && row["code"] == "routing_policy_strict_recovery_required"));
}

#[tokio::test]
async fn r8_policy_write_failure_blocks_only_strict_sessions_and_keeps_current_default_credential()
{
    let h = Harness::new(Provider::Codex).await;
    let a = h
        .turn("/v1/responses", &[("session-id", "available-default")], RAW)
        .await;
    h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"session","id":qb_core::routing::session_id("codex:locked"),"settings":{"mode":"strict","reset_wait_secs":0}}))).await;
    let path = h.home.join("state/routing-policies.json");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let response = h.client.post(format!("http://{}/admin/v1/routing",h.servers.admin_addr))
        .bearer_auth("synthetic-admin")
        .json(&json!({"kind":"session","id":qb_core::routing::session_id("codex:available-default"),"settings":{"mode":"recover","reset_wait_secs":20}}))
        .send().await.unwrap();
    assert!(!response.status().is_success());
    let n = h.fake.state.requests().len();
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "available-default")], RAW)
            .await,
        a
    );
    assert_credential(&h, n, &a);
    assert_eq!(
        h.send("/v1/responses", &[("session-id", "locked")], RAW)
            .await
            .0,
        409
    );
    assert_eq!(h.fake.state.requests().len(), n + 1);
    assert!(
        h.ledger()
            .iter()
            .any(|row| row["event"] == "core_warning"
                && row["code"] == "routing_policy_write_failed")
    );
}

#[tokio::test]
async fn r8_corrupt_default_policy_is_quarantined_and_real_relay_keeps_serving() {
    let h = Harness::new(Provider::Codex).await;
    let a = h
        .turn("/v1/responses", &[("session-id", "plain-recovery")], RAW)
        .await;
    let h = h
        .restart_with_fixture(|home| {
            fs::write(home.join("state/routing-policies.json"), b"not a policy").unwrap();
        })
        .await;
    assert_eq!(
        h.turn("/v1/responses", &[("session-id", "plain-recovery")], RAW)
            .await,
        a
    );
    assert_credential(&h, 1, &a);
    assert!(
        h.ledger().iter().any(
            |row| row["event"] == "core_warning" && row["code"] == "routing_policy_quarantined"
        )
    );
    assert!(
        fs::read_dir(h.home.join("state"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("routing-policies.json.corrupt-"))
    );
}

#[tokio::test]
async fn r8_four_forced_recoveries_within_ten_minutes_use_the_actual_alternate_credentials() {
    let h = Harness::new(Provider::Codex).await;
    let key = "unbudgeted-forced";
    let mut current = h.turn("/v1/responses", &[("session-id", key)], RAW).await;
    let mut alternate = h.ids().into_iter().find(|id| id != &current).unwrap();
    let began = Instant::now();
    for _ in 0..4 {
        if h.store.load(&alternate).unwrap().user_state != qb_core::accounts::UserState::Active {
            h.admin(
                reqwest::Method::POST,
                &format!("/admin/v1/accounts/{alternate}/resume"),
                None,
            )
            .await;
        }
        h.pause(&current).await;
        let n = h.fake.state.requests().len();
        assert_eq!(
            h.turn("/v1/responses", &[("session-id", key)], RAW).await,
            alternate
        );
        assert_credential(&h, n, &alternate);
        std::mem::swap(&mut current, &mut alternate);
    }
    assert!(began.elapsed() < Duration::from_secs(600));
    assert_eq!(h.fake.state.requests().len(), 5);
    assert_eq!(
        h.ledger()
            .iter()
            .filter(|row| row["event"] == "binding_move" && row["reason"] == "forced")
            .count(),
        4
    );
    assert!(!h.home.join("state/routing-policies.json").exists());
    assert!(!h.home.join("state/routing-policies.fence.json").exists());
}
