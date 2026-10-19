use super::*;

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
async fn simultaneous_rate_wait_expiry_never_forces_a_burst_to_upstream() {
    let h = Harness::with_limits(Provider::Codex, 60, 5, 1).await;
    let headers = [("session-id", "expiry-herd")];
    let first = h.turn("/v1/responses", &headers, RAW).await;
    h.state
        .accounts
        .observe_429(&first, Some(Duration::from_secs(30)));
    let replies =
        futures_util::future::join_all((0..8).map(|_| h.send("/v1/responses", &headers, RAW)))
            .await;
    assert!(replies.iter().all(|r| r.0 == 429));
    assert_eq!(
        h.fake.state.requests().len(),
        1,
        "expired waiters are errors, never admitted wire sends"
    );
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
    h.admin(reqwest::Method::POST,"/admin/v1/routing",Some(json!({"kind":"project","name":"unrelated","settings":{"mode":"rebalance","reset_wait_secs":0}}))).await;
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
async fn full_move_budget_serves_healthy_credential_and_blocks_only_required_move() {
    let h = Harness::new(Provider::Codex).await;
    let headers = [("session-id", "full-move-budget")];
    let a = h.turn("/v1/responses", &headers, RAW).await;
    let key = "codex:full-move-budget";
    for (from, to) in [("cx-z", "cx-x"), ("cx-x", "cx-y"), ("cx-y", a.as_str())] {
        h.state
            .bindings
            .policies
            .dispatch(Some(key), to, Some(from), "forced", Utc::now())
            .unwrap();
    }
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
    assert_eq!(h.turn("/v1/responses", &headers, RAW).await, a);
    assert_credential(&h, 2, &a);
    h.pause(&a).await;
    assert_eq!(h.send("/v1/responses", &headers, RAW).await.0, 409);
    assert_eq!(h.fake.state.requests().len(), 3);
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
    h.pause(&first).await;
    assert_ne!(
        h.turn("/v1/responses", &[("session-id", "recover-root")], RAW)
            .await,
        first
    );
}
