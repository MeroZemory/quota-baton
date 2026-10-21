mod r11_review {
    use super::*;

    async fn issue_http(h: &Harness, request: Value) -> (StatusCode, Value) {
        // The shared Harness intentionally configures admin launch's data port
        // as zero. Give this actual admin endpoint the existing live data port.
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-client".into(),
                admin_token: "synthetic-admin".into(),
            },
            relay::router(h.state.clone()),
            qb_core::admin::runtime_routes(h.state.clone(), h.servers.data_addr.port()),
        )
        .await
        .unwrap();
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
            .post(format!("http://{}/admin/v1/launches", servers.admin_addr))
            .bearer_auth("synthetic-admin")
            .json(&request)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let reply = response.json().await.unwrap();
        servers.shutdown();
        (status, reply)
    }

    fn anchor_targets(snapshot: &Value) -> std::collections::BTreeMap<String, String> {
        snapshot["launch_anchors"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(anchor, metadata)| {
                (
                    anchor.clone(),
                    metadata["canonical_key"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    async fn model_hold_fixture(strict: bool, blocked_model: &str) -> (Harness, String) {
        let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
        let catalog = json!({"models":[{"slug":"synthetic-model"},{"slug":"menu-model"}]});
        let launch = r11_models_issue_raw(&h, &catalog).await;
        h.fake.state.push(ok());
        let response = reqwest::Client::new()
            .post(format!("{}/responses", launch.base_url))
            .bearer_auth("synthetic-client")
            .header("session-id", "r11-review-model-hold")
            .body(r#"{"model":"synthetic-model","input":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        let info = h
            .state
            .bindings
            .launch_info("codex:r11-review-model-hold", Utc::now())
            .unwrap();
        assert_eq!(info.account, launch.account);
        assert_eq!(info.current_model.as_deref(), Some("synthetic-model"));
        if strict {
            let snapshot = h.state.bindings.export().unwrap();
            let policy_key = snapshot["launches"][&info.canonical_key]["policy_key"]
                .as_str()
                .unwrap();
            h.state
                .bindings
                .policies
                .set_session(
                    &qb_core::routing::session_id(policy_key),
                    Some(qb_core::routing::Settings {
                        mode: qb_core::routing::Mode::Strict,
                        reset_wait_secs: 0,
                    }),
                    None,
                    Some(&info.account),
                )
                .unwrap();
        }
        let headers = tray2_headers(0, (Utc::now() + ChronoDuration::days(7)).timestamp());
        for id in h.ids() {
            h.state
                .accounts
                .observe_headers(&id, Provider::Codex, &headers)
                .await;
        }
        h.store
            .update_metadata(&info.account, |account| {
                account.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: Utc::now() + ChronoDuration::minutes(5),
                    scope: format!("model:{blocked_model}"),
                };
            })
            .unwrap();
        (h, info.account)
    }

    #[tokio::test]
    async fn r11_review_implicit_resume_retains_current_model_over_held_default() {
        for strict in [false, true] {
            let (h, source) = model_hold_fixture(strict, "menu-model").await;
            let before = h.fake.state.requests().len();
            let (status, reply) = issue_http(
                &h,
                json!({"model":"menu-model","model_is_explicit":false,
                    "resume_id":"r11-review-model-hold"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{reply}");
            assert_eq!(reply["account"], source);
            assert_eq!(reply["model"], "synthetic-model");
            assert_eq!(h.fake.state.requests().len(), before);
            let info = h
                .state
                .bindings
                .launch_info("codex:r11-review-model-hold", Utc::now())
                .unwrap();
            assert_eq!(info.account, source);
            assert_eq!(info.current_model.as_deref(), Some("synthetic-model"));
            assert!(!h.ledger().iter().any(|row| row["reason"] == "forced"));
        }
    }

    #[tokio::test]
    async fn r11_review_explicit_held_model_requires_consent_before_compatible_move() {
        for strict in [false, true] {
            let (h, source) = model_hold_fixture(strict, "menu-model").await;
            let before = h.state.bindings.export().unwrap();
            let policies_before = h.state.bindings.policies.list();
            let calls_before = h.fake.state.requests().len();
            let request = json!({"model":"menu-model","model_is_explicit":true,
                "resume_id":"r11-review-model-hold"});
            let (status, reply) = issue_http(&h, request.clone()).await;
            assert_eq!(status, StatusCode::CONFLICT, "{reply}");
            assert_eq!(reply["error"]["code"], "model_move_consent_required");
            let refused = h.state.bindings.export().unwrap();
            for field in ["bindings", "launches"] {
                assert_eq!(refused[field], before[field], "{field}");
            }
            // Resolution touches last_used; it must not add or redirect anchors.
            assert_eq!(anchor_targets(&refused), anchor_targets(&before));
            assert_eq!(h.state.bindings.policies.list(), policies_before);
            assert_eq!(h.fake.state.requests().len(), calls_before);
            assert!(!h.ledger().iter().any(|row| row["reason"] == "forced"));

            let mut consented = request;
            consented["move_for_model"] = json!(true);
            let (status, reply) = issue_http(&h, consented).await;
            assert_eq!(status, StatusCode::OK, "{reply}");
            assert_ne!(reply["account"], source);
            assert_eq!(reply["model"], "menu-model");
            let info = h
                .state
                .bindings
                .launch_info("codex:r11-review-model-hold", Utc::now())
                .unwrap();
            assert_eq!(json!(info.account), reply["account"]);
            assert_eq!(info.current_model.as_deref(), Some("menu-model"));
            if strict {
                let policy_key = before["launches"][&info.canonical_key]["policy_key"]
                    .as_str()
                    .unwrap();
                let plan = h.state.bindings.policies.plan(Some(policy_key));
                assert_eq!(plan.settings.mode, qb_core::routing::Mode::Strict);
                assert_eq!(plan.anchor.as_deref(), Some(info.account.as_str()));
            }
            h.fake.state.push(ok());
            let body = r#"{"model":"menu-model","input":[]}"#;
            let response = reqwest::Client::new()
                .post(format!("{}/responses", reply["base_url"].as_str().unwrap()))
                .bearer_auth("synthetic-client")
                .header("session-id", "r11-review-model-hold")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.bytes().await.unwrap();
            let calls = h.fake.state.requests();
            let first = calls
                .iter()
                .find(|call| call.path_and_query == "/responses")
                .unwrap();
            let last = calls.last().unwrap();
            assert_eq!(last.path_and_query, "/responses");
            assert_eq!(last.body.as_ref(), body.as_bytes());
            assert_ne!(
                first.headers["chatgpt-account-id"],
                last.headers["chatgpt-account-id"]
            );
        }
    }

    #[tokio::test]
    async fn r11_review_retained_ineligible_source_cannot_publish_resume() {
        let (h, source) = model_hold_fixture(false, "synthetic-model").await;
        let before = h.state.bindings.export().unwrap();
        let policies_before = h.state.bindings.policies.list();
        let calls_before = h.fake.state.requests().len();
        let (status, reply) = issue_http(
            &h,
            json!({"model":"menu-model","model_is_explicit":false,
                "resume_id":"r11-review-model-hold"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{reply}");
        assert_eq!(reply["error"]["code"], "launch_unavailable");
        let refused = h.state.bindings.export().unwrap();
        for field in ["bindings", "launches"] {
            assert_eq!(refused[field], before[field], "{field}");
        }
        assert_eq!(anchor_targets(&refused), anchor_targets(&before));
        assert_eq!(h.state.bindings.policies.list(), policies_before);
        assert_eq!(
            h.state
                .bindings
                .current(Some("codex:r11-review-model-hold")),
            Some(source)
        );
        assert_eq!(h.fake.state.requests().len(), calls_before);
        assert!(!h.ledger().iter().any(|row| row["reason"] == "forced"));
    }

    #[tokio::test]
    async fn r11_review_legacy_resume_clears_unproved_held_default_before_admission() {
        let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
        let catalog = json!({"models":[{"slug":"synthetic-model"}]});
        let _warm = r11_models_issue_raw(&h, &catalog).await;
        h.fake.state.push(ok());
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/responses", h.servers.data_addr))
            .bearer_auth("synthetic-client")
            .header("session-id", "r11-review-legacy-default")
            .body(r#"{"model":"synthetic-model","input":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        let prior = h
            .state
            .bindings
            .launch_info("codex:r11-review-legacy-default", Utc::now())
            .unwrap();
        assert!(!prior.managed);
        assert!(prior.current_model.is_none());
        let headers = tray2_headers(0, (Utc::now() + ChronoDuration::days(7)).timestamp());
        for id in h.ids() {
            h.state
                .accounts
                .observe_headers(&id, Provider::Codex, &headers)
                .await;
        }
        h.store
            .update_metadata(&prior.account, |account| {
                account.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: Utc::now() + ChronoDuration::minutes(5),
                    scope: "model:menu-model".into(),
                };
            })
            .unwrap();
        let calls_before = h.fake.state.requests().len();
        let (status, reply) = issue_http(
            &h,
            json!({"model":"menu-model","model_is_explicit":false,
                "resume_id":"r11-review-legacy-default"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(reply["account"], prior.account);
        assert!(reply["model"].is_null());
        let resumed = h
            .state
            .bindings
            .launch_info("codex:r11-review-legacy-default", Utc::now())
            .unwrap();
        assert!(resumed.managed);
        assert_eq!(resumed.canonical_key, prior.canonical_key);
        assert_eq!(resumed.account, prior.account);
        assert!(resumed.current_model.is_none());
        assert_eq!(h.fake.state.requests().len(), calls_before);
        assert!(!h.ledger().iter().any(|row| row["reason"] == "forced"));
    }

    #[tokio::test]
    async fn r11_review_probing_prior_cannot_bypass_model_move_consent() {
        let (h, source) = model_hold_fixture(false, "menu-model").await;
        let account = h
            .store
            .update_metadata(&source, |account| account.auth_probe_pending = true)
            .unwrap();
        let probe = h.state.accounts.begin_probe(&account).unwrap().unwrap();
        assert!(h.state.accounts.probing(&source));
        let before = h.state.bindings.export().unwrap();
        let policies_before = h.state.bindings.policies.list();
        let calls_before = h.fake.state.requests().len();
        let (status, reply) = issue_http(
            &h,
            json!({"model":"menu-model","model_is_explicit":true,
                "resume_id":"r11-review-model-hold"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{reply}");
        assert_eq!(reply["error"]["code"], "launch_unavailable");
        let refused = h.state.bindings.export().unwrap();
        for field in ["bindings", "launches"] {
            assert_eq!(refused[field], before[field], "{field}");
        }
        assert_eq!(anchor_targets(&refused), anchor_targets(&before));
        assert_eq!(h.state.bindings.policies.list(), policies_before);
        let info = h
            .state
            .bindings
            .launch_info("codex:r11-review-model-hold", Utc::now())
            .unwrap();
        assert_eq!(info.account, source);
        assert_eq!(info.current_model.as_deref(), Some("synthetic-model"));
        assert!(h.state.accounts.probing(&source));
        assert_eq!(h.fake.state.requests().len(), calls_before);
        assert!(!h.ledger().iter().any(|row| row["reason"] == "forced"));
        drop(probe);
    }

    #[tokio::test]
    async fn r11_review_accepted_model_ids_route_without_logging_validator_restrictions() {
        for model in ["m".repeat(101), "provider/model:revision".to_owned()] {
            let h = Harness::new(Provider::Codex, 1, 64, 1024).await;
            let catalog = json!({"models":[{"slug":model}]});
            h.fake
                .state
                .push(ScriptedResponse::json(StatusCode::OK, catalog.to_string()));
            let launch = h
                .state
                .issue_launch(
                    relay::LaunchRequest {
                        model: Some(model.clone()),
                        model_is_explicit: true,
                        account_pref: None,
                        resume_id: None,
                        move_for_model: false,
                    },
                    h.servers.data_addr.port(),
                )
                .await
                .unwrap();
            let client = reqwest::Client::new();
            let menu = client
                .get(&launch.catalog_url)
                .bearer_auth("synthetic-client")
                .send()
                .await
                .unwrap();
            assert_eq!(menu.status(), StatusCode::OK);
            assert_eq!(menu.json::<Value>().await.unwrap(), catalog);
            let body = json!({"model":model,"input":[]}).to_string();
            h.fake.state.push(ok());
            let response = client
                .post(format!("{}/responses", launch.base_url))
                .bearer_auth("synthetic-client")
                .body(body.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.bytes().await.unwrap();
            let calls = h.fake.state.requests();
            assert_eq!(calls.last().unwrap().body.as_ref(), body.as_bytes());
            assert_eq!(calls.last().unwrap().path_and_query, "/responses");
        }
    }

    #[tokio::test]
    async fn r11_review_issue_catalog_wait_rechecks_paused_and_drained_account() {
        for drained in [false, true] {
            let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
            let ids = h.ids();
            let source = ids[0].clone();
            for _ in &ids {
                h.fake.state.push(ScriptedResponse::json(
                    StatusCode::OK,
                    r#"{"models":[{"slug":"synthetic-model"}]}"#,
                ));
            }
            let (arrived, release) = h.fake.state.pause_next_response();
            let state = h.state.clone();
            let pref = source.clone();
            let port = h.servers.data_addr.port();
            let issuing = tokio::spawn(async move {
                state
                    .issue_launch(
                        relay::LaunchRequest {
                            model: Some("synthetic-model".into()),
                            model_is_explicit: true,
                            account_pref: Some(pref),
                            resume_id: None,
                            move_for_model: false,
                        },
                        port,
                    )
                    .await
            });
            tokio::time::timeout(Duration::from_secs(5), arrived)
                .await
                .unwrap()
                .unwrap();
            h.store
                .update_settings(&source, drained.then_some(true), None, |account| {
                    if drained {
                        account.drain = true;
                    } else {
                        account.user_state = UserState::Paused;
                    }
                })
                .unwrap();
            release.send(()).unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(10), issuing)
                .await
                .unwrap()
                .unwrap();
            if let Ok(reply) = reply {
                assert_ne!(reply.account, source);
            }
            let snapshot = h.state.bindings.export().unwrap();
            assert!(
                snapshot["bindings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|b| b["account"] != source)
            );
        }
    }

    #[tokio::test]
    async fn r11_review_restart_recovers_proved_model_without_source_catalog_and_preserves_strict()
    {
        for strict in [false, true] {
            let (h, launch, persisted) = r11_http_restart_fixture().await;
            if strict {
                let snapshot = h.state.bindings.export().unwrap();
                let policy_key = snapshot["launches"][&persisted.canonical_key]["policy_key"]
                    .as_str()
                    .unwrap();
                h.state
                    .bindings
                    .policies
                    .set_session(
                        &qb_core::routing::session_id(policy_key),
                        Some(qb_core::routing::Settings {
                            mode: qb_core::routing::Mode::Strict,
                            reset_wait_secs: 0,
                        }),
                        None,
                        Some(&persisted.account),
                    )
                    .unwrap();
            }
            h.store
                .update_metadata(&persisted.account, |account| {
                    account.user_state = UserState::Paused
                })
                .unwrap();
            h.fake.state.push(ScriptedResponse::json(
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{}"#,
            ));
            h.fake.state.push(ScriptedResponse::json(
                StatusCode::OK,
                r#"{"models":[{"slug":"synthetic-model"},{"slug":"menu-model"}]}"#,
            ));
            if !strict {
                h.fake.state.push(ok());
            }
            let response = reqwest::Client::new()
                .post(format!("{}/responses", launch.base_url))
                .bearer_auth("synthetic-client")
                .header("session-id", "r11-persisted-resume")
                .body(r#"{"model":"synthetic-model","input":[]}"#)
                .send()
                .await
                .unwrap();
            let status = response.status();
            response.bytes().await.unwrap();
            let data: Vec<_> = h
                .fake
                .state
                .requests()
                .into_iter()
                .filter(|r| r.path_and_query == "/responses")
                .collect();
            if strict {
                assert_ne!(status, StatusCode::OK);
                assert_eq!(data.len(), 1, "only pre-restart source response may exist");
                assert_eq!(
                    h.state
                        .bindings
                        .current(Some(&persisted.canonical_key))
                        .as_deref(),
                    Some(persisted.account.as_str())
                );
            } else {
                assert_eq!(status, StatusCode::OK);
                assert_eq!(data.len(), 2);
                assert_ne!(
                    data[0].headers["chatgpt-account-id"],
                    data[1].headers["chatgpt-account-id"]
                );
                assert_ne!(
                    h.state
                        .bindings
                        .current(Some(&persisted.canonical_key))
                        .as_deref(),
                    Some(persisted.account.as_str())
                );
            }
        }
    }

    #[tokio::test]
    async fn r11_review_restart_never_recovers_an_unproved_new_model_via_other_account() {
        let (h, launch, persisted) = r11_http_restart_fixture().await;
        h.store
            .update_metadata(&persisted.account, |account| {
                account.user_state = UserState::Paused
            })
            .unwrap();
        let before = h.fake.state.requests().len();
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{}"#,
        ));
        let response = reqwest::Client::new()
            .post(format!("{}/responses", launch.base_url))
            .bearer_auth("synthetic-client")
            .header("session-id", "r11-persisted-resume")
            .body(r#"{"model":"not-proved-model","input":[]}"#)
            .send()
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        let calls = h.fake.state.requests();
        assert_eq!(calls.len(), before + 1);
        assert!(calls.last().unwrap().path_and_query.starts_with("/models"));
        assert_eq!(
            h.state
                .bindings
                .current(Some(&persisted.canonical_key))
                .as_deref(),
            Some(persisted.account.as_str())
        );
    }

    // Observe real authenticated admin responses; do not inject dashboard events.
    async fn model_choice_dashboard_get(h: &Harness, path: &str) -> Value {
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
            .get(format!("http://{}/admin/v1/{path}", h.servers.admin_addr))
            .bearer_auth("synthetic-admin")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "dashboard {path}");
        response.json().await.unwrap()
    }

    #[tokio::test]
    async fn r11_http_entry_model_choice_placement_and_move_surface_in_dashboard() {
        let h = Harness::new(Provider::Codex, 2, 64, 1024).await;
        let ids = r11_http_warm_distinct(&h, &[&["common", "a-only"], &["common", "b-only"]]).await;
        // Avoid warmup placements consuming the weak/unobserved-account gate.
        let headers = tray2_headers(0, (Utc::now() + ChronoDuration::days(7)).timestamp());
        for id in &ids {
            h.state
                .accounts
                .observe_headers(id, Provider::Codex, &headers)
                .await;
        }
        let resume = "r11-model-choice-panel";
        let id = qb_core::dashboard::SessionId::from_key(&format!("codex:{resume}"));
        let path = format!("sessions/{}", id.as_str());
        let before_calls = h.fake.state.requests().len();
        let (status, initial) = issue_http(
            &h,
            json!({
                "model":"a-only", "model_is_explicit":true,
                "account_pref":ids[0], "resume_id":resume,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{initial}");
        assert_eq!(initial["account"], ids[0]);
        assert_eq!(h.fake.state.requests().len(), before_calls);

        // This public-JSON assertion fails on the unfixed product before any
        // inference can create a generic placement and mask the issuance gap.
        let placed = model_choice_dashboard_get(&h, &path).await;
        assert_eq!(placed["timeline"].as_array().unwrap().len(), 1);
        let first = &placed["timeline"][0];
        assert_eq!(first["kind"], "placed");
        assert_eq!(first["reason"], "launch_model");
        assert_eq!(first["account"], ids[0]);
        assert!(first["from"].is_null());
        assert!(first["next_turn_cache"].is_null());
        assert_eq!(placed["account"], ids[0]);
        assert_eq!(placed["first_account_today"], ids[0]);
        assert!(placed["moves_today"].as_array().unwrap().is_empty());
        let activity_before = model_choice_dashboard_get(&h, "activity").await;
        assert!(
            !activity_before["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["data"]["session"] == id.as_str())
        );

        let request = json!({"model":"b-only", "model_is_explicit":true, "resume_id":resume});
        let (status, refused) = issue_http(&h, request.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        assert_eq!(refused["error"]["code"], "model_move_consent_required");
        let unchanged = model_choice_dashboard_get(&h, &path).await;
        for field in ["timeline", "moves_today", "account", "first_account_today"] {
            assert_eq!(unchanged[field], placed[field], "refusal changed {field}");
        }
        assert_eq!(
            model_choice_dashboard_get(&h, "activity").await,
            activity_before
        );
        assert_eq!(h.fake.state.requests().len(), before_calls);

        // A first request reporting a cache hit must not label initial placement
        // as an after-move cache result. Send and fully drain the actual HTTP body.
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let body = r#"{ "model":"a-only", "input":[] }"#;
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::OK,
            r#"{"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":5}}}"#,
        ));
        let response = client
            .post(format!(
                "{}/responses",
                initial["base_url"].as_str().unwrap()
            ))
            .bearer_auth("synthetic-client")
            .header("session-id", resume)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        let observed = model_choice_dashboard_get(&h, &path).await;
        assert_eq!(observed["timeline"], placed["timeline"]);
        assert!(observed["moves_today"].as_array().unwrap().is_empty());

        let mut consented = request;
        consented["move_for_model"] = json!(true);
        let (status, moved) = issue_http(&h, consented).await;
        assert_eq!(status, StatusCode::OK, "{moved}");
        assert_eq!(moved["account"], ids[1]);
        assert_eq!(h.fake.state.requests().len(), before_calls + 1);
        let detail = model_choice_dashboard_get(&h, &path).await;
        assert_eq!(detail["account"], ids[1]);
        assert_eq!(detail["first_account_today"], ids[0]);
        assert_eq!(detail["timeline"].as_array().unwrap().len(), 2);
        assert_eq!(detail["timeline"][0], placed["timeline"][0]);
        let mv = &detail["timeline"][1];
        assert_eq!(mv["kind"], "move");
        assert_eq!(mv["reason"], "model_choice");
        assert_eq!(mv["from"], ids[0]);
        assert_eq!(mv["account"], ids[1]);
        assert_eq!(detail["moves_today"].as_array().unwrap().len(), 1);
        assert_eq!(detail["moves_today"][0]["reason"], "model_choice");
        let activity = model_choice_dashboard_get(&h, "activity").await;
        let events: Vec<_> = activity["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["data"]["session"] == id.as_str())
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "move");
        assert_eq!(events[0]["data"]["reason"], "model_choice");
        assert_eq!(events[0]["account"], ids[0]);
        assert_eq!(events[0]["account_to"], ids[1]);
        assert_eq!(events[0]["notable"], true);
        let calls = h.fake.state.requests();
        assert_eq!(calls.last().unwrap().body.as_ref(), body.as_bytes());
        assert_eq!(calls.last().unwrap().path_and_query, "/responses");
    }
}
