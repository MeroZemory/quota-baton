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
async fn local_rate_wait_expiry_has_typed_json_retry_advice_for_both_families() {
    for provider in [Provider::Claude, Provider::Codex] {
        let h = Harness::with_limits(provider, 60, 5, 1).await;
        let (path, header) = if provider == Provider::Claude {
            ("/v1/messages", "x-claude-code-session-id")
        } else {
            ("/v1/responses", "session-id")
        };
        let a = h.turn(path, &[(header, "typed-rate-expiry")], RAW).await;
        h.state
            .accounts
            .observe_429(&a, Some(Duration::from_secs(30)));
        let response = h
            .client
            .post(format!("http://{}{path}", h.servers.data_addr))
            .bearer_auth("synthetic-client")
            .header(header, "typed-rate-expiry")
            .body(RAW)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            response.headers()["retry-after"]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                >= 28
        );
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["type"],
            "rate_wait_timeout"
        );
        assert_eq!(h.fake.state.requests().len(), 1);
        assert!(matches!(h.store.load(&a).unwrap().health, Health::Ready));
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
async fn started_rate_budget_does_not_reopen_after_later_slow_auth_retry() {
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
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{}"));
    let result = h
        .send(
            "/v1/responses",
            &[("session-id", "rate-then-slow-auth")],
            RAW,
        )
        .await;
    assert_eq!(result.0, 429);
    assert!(String::from_utf8_lossy(&result.2).contains("rate_wait_timeout"));
    let requests = h.fake.state.requests();
    assert_eq!(
        requests.len(),
        3,
        "a spent rate budget sent one more data request"
    );
    assert_eq!(
        requests[0].headers["chatgpt-account-id"],
        requests[1].headers["chatgpt-account-id"]
    );
    assert!(String::from_utf8_lossy(&requests[2].body).contains("refresh_token"));
}

#[tokio::test]
async fn legacy_global_strict_corrupt_restore_refuses_core_start_before_any_b_credential() {
    let h = Harness::new(Provider::Codex).await;
    let a = h
        .turn("/v1/responses", &[("session-id", "legacy-corrupt")], RAW)
        .await;
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
    config.routing.mode = qb_core::routing::Mode::Strict;
    config.upstream.codex_base = fake.url.clone();
    config.upstream.anthropic_base = fake.url.clone();
    for raw in ["{".to_owned(),json!({"schema":99,"bindings":[]}).to_string(),json!({"schema":1,"bindings":[{"key":"codex:legacy-corrupt","account":a,"last_used":Utc::now()},{"key":"codex:legacy-corrupt","account":a,"last_used":Utc::now()}]}).to_string()] {
        let path=home.join("state/bindings.json");fs::write(&path,raw.as_bytes()).unwrap();
        assert!(RelayState::new(store.clone(),&config,obs.clone(),Arc::new(DebugPins::default())).is_err(),"legacy strict core started with no trustworthy anchor");
        assert_eq!(fs::read(&path).unwrap(),raw.as_bytes());
        assert_eq!(fake.state.requests().len(),1,"corrupt restore dispatched an alternate credential");
    }
}
