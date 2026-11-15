// Main includes this file directly inside quota::tests. Reuse its native Fixture.
// Source-only worker handoff: no Cargo/test execution, no real provider targets.
#[cfg(debug_assertions)]
mod r28_claude_reset_tests {
    use super::*;
    use crate::reset_credits::Error as ResetError;
    use qb_proto::{reset_credit_action::*, reset_credits::*};
    use serde_json::Value;

    const DETAILS: &str = "/api/oauth/usage?cedar_ember=1&skip_spend=1";
    const USAGE: &str = "/api/oauth/usage";
    const CONSUME: &str = "/api/organizations/synthetic-organization/reset_rate_limits";

    fn reply(value: Value) -> ScriptedResponse {
        ScriptedResponse::json(reqwest::StatusCode::OK, value.to_string())
    }

    #[tokio::test]
    async fn ordinary_claude_poll_preserves_omitted_model_hold_then_recovers_fresh_model_usage() {
        let f = fixture().await;
        let reset = "2099-11-07T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
            (
                "anthropic-ratelimit-unified-reset",
                reset.timestamp().to_string(),
            ),
            (
                "anthropic-ratelimit-unified-7d-status",
                "allowed".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-7d-utilization",
                "0.51".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-7d-reset",
                reset.timestamp().to_string(),
            ),
            (
                "anthropic-ratelimit-unified-7d_oi-status",
                "rejected".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-7d_oi-utilization",
                "1".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-7d_oi-reset",
                reset.timestamp().to_string(),
            ),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        f.relay
            .accounts
            .observe_headers_for_model(
                &f.ids[0],
                Provider::Claude,
                &headers,
                Some("claude-fable-5-2"),
            )
            .await;
        let before = f.account(0);
        assert!(
            matches!(&before.health, Health::Cooling {scope,..} if scope=="model:claude-fable-5-2")
        );
        assert_eq!(before.quota_cooling.as_ref().unwrap().window, "7d_oi");
        let original = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap()["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == "7d_oi")
            .unwrap()
            .clone();
        let mut body = json!({"five_hour":null,"seven_day":{"utilization":51,"resets_at":reset,"status":"allowed"}});
        f.fake.state.push(reply(body.clone()));
        f.fake.state.push(profile_reply(&f));
        f.relay.quota.poll(&f.ids[0]).await.unwrap();
        assert_eq!(f.account(0).health, before.health);
        assert_eq!(f.account(0).quota_cooling, before.quota_cooling);
        let snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
        assert_eq!(
            snapshot["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["name"] == "7d_oi")
                .unwrap(),
            &original,
            "omission cannot invent a new observation or erase rejection attribution"
        );
        assert!(
            f.relay
                .accounts
                .eligible(&f.ids[0], Some("claude-sonnet-5-5"))
                .await
        );
        assert!(
            !f.relay
                .accounts
                .eligible(&f.ids[0], Some("claude-fable-5-2"))
                .await
        );
        for (model, chosen) in [("claude-sonnet-5-5", true), ("claude-fable-5-2", false)] {
            let candidate = crate::bindings::Candidate {
                account: f.account(0),
                quota: Some(snapshot.clone()),
                inflight: 0,
            };
            let decision = f.relay.bindings.choose(
                Provider::Claude,
                Some(&format!("claude:preserved-{model}")),
                None,
                Some(model),
                &[candidate],
                None,
                100,
            );
            assert_eq!(
                matches!(decision, crate::bindings::Decision::Selected { .. }),
                chosen,
                "{model}"
            );
        }
        body["seven_day_opus"] = json!({"utilization":0,"resets_at":reset});
        f.fake.state.push(reply(body));
        f.fake.state.push(profile_reply(&f));
        fresh_poller(&f).poll(&f.ids[0]).await.unwrap();
        assert_eq!(f.account(0).health, Health::Ready);
        assert!(f.account(0).quota_cooling.is_none());
        assert!(
            f.relay
                .accounts
                .eligible(&f.ids[0], Some("claude-fable-5-2"))
                .await
        );
        let recovered = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
        assert!(recovered["signals"]["unified_status"].is_null());
        assert!(
            recovered["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["name"] == "7d_oi")
                .unwrap()["status"]
                .is_null()
        );
        assert!(
            f.fake
                .state
                .requests()
                .iter()
                .all(|request| request.method == reqwest::Method::GET)
        );
    }

    #[tokio::test]
    async fn ordinary_claude_poll_recovers_merged_header_hold_without_reset_post() {
        for (used, partial, rejected, ready) in [
            (0, false, false, true),
            (51, false, false, true),
            (95, false, false, true),
            (97, false, false, false),
            (0, true, false, false),
            (0, false, true, false),
        ] {
            let f = fixture().await;
            let reset = "2099-11-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
            let mut headers = reqwest::header::HeaderMap::new();
            for (name, value) in [
                ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
                (
                    "anthropic-ratelimit-unified-reset",
                    reset.timestamp().to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "five_hour".to_owned(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-status",
                    "rejected".to_owned(),
                ),
                ("anthropic-ratelimit-unified-5h-utilization", "1".to_owned()),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    reset.timestamp().to_string(),
                ),
            ] {
                headers.insert(name, value.parse().unwrap());
            }
            f.relay
                .accounts
                .observe_headers(&f.ids[0], Provider::Claude, &headers)
                .await;
            let before = f.account(0);
            assert!(before.quota_cooling.is_some());
            let mut fresh = usage(used);
            if partial {
                fresh.as_object_mut().unwrap().remove("seven_day");
            }
            if rejected {
                fresh["five_hour"]["status"] = json!("rejected");
            }
            f.fake.state.push(reply(fresh));
            f.fake.state.push(profile_reply(&f));
            f.relay.quota.poll(&f.ids[0]).await.unwrap();
            let after = f.account(0);
            assert_eq!(
                after.health == Health::Ready,
                ready,
                "used={used} partial={partial} rejected={rejected}"
            );
            assert_eq!(f.relay.accounts.eligible(&f.ids[0], None).await, ready);
            let views = f.relay.accounts.typed_views().await.unwrap();
            assert_eq!(
                views
                    .iter()
                    .find(|v| v.id == f.ids[0])
                    .unwrap()
                    .pool_eligible,
                ready
            );
            let merged = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
            assert_eq!(merged["signals"]["unified_status"] != "rejected", ready);
            assert_eq!(after.allow_credits, before.allow_credits);
            assert_eq!(after.drain, before.drain);
            assert_eq!(after.user_state, before.user_state);
            assert_requests_with_profiles(&f, 0, 1);
        }
    }

    #[tokio::test]
    async fn ordinary_claude_poll_preserves_independent_auth_payment_and_model_holds() {
        for reason in [
            HealthReason::Auth,
            HealthReason::Payment,
            HealthReason::Quota,
        ] {
            let f = fixture().await;
            let until = "2099-11-07T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
            let scope = if reason == HealthReason::Quota {
                "model:claude-opus-4"
            } else {
                "account"
            };
            f.relay
                .quota
                .store
                .update_metadata(&f.ids[0], |account| {
                    account.health = Health::Cooling {
                        reason,
                        until,
                        scope: scope.into(),
                    };
                    if reason == HealthReason::Payment {
                        account.payment_until = Some(until);
                    }
                    if reason == HealthReason::Quota {
                        account.quota_cooling = Some(crate::accounts::QuotaCooling {
                            window: "7d_oi".into(),
                            reset_at: until,
                            started_at: Utc::now() - chrono::Duration::seconds(1),
                        });
                    }
                })
                .unwrap();
            let before = f.account(0);
            let mut fresh = usage(0);
            fresh["seven_day_opus"]["utilization"] = json!(100);
            f.fake.state.push(reply(fresh));
            f.fake.state.push(profile_reply(&f));
            f.relay.quota.poll(&f.ids[0]).await.unwrap();
            assert_eq!(f.account(0).health, before.health, "{reason:?}");
            assert_eq!(
                f.account(0).quota_cooling,
                before.quota_cooling,
                "{reason:?}"
            );
            assert_requests_with_profiles(&f, 0, 1);
        }
    }

    #[tokio::test]
    async fn ordinary_claude_poll_recovers_known_weekly_inventory_with_explicit_null_five_hour() {
        for shape in ["null", "omitted", "malformed", "known_five_hour"] {
            let f = fixture().await;
            let reset = "2099-11-07T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
            let mut headers = reqwest::header::HeaderMap::new();
            for (name, value) in [
                ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
                (
                    "anthropic-ratelimit-unified-reset",
                    reset.timestamp().to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "seven_day".to_owned(),
                ),
                (
                    "anthropic-ratelimit-unified-7d-status",
                    "rejected".to_owned(),
                ),
                ("anthropic-ratelimit-unified-7d-utilization", "1".to_owned()),
                (
                    "anthropic-ratelimit-unified-7d-reset",
                    reset.timestamp().to_string(),
                ),
            ] {
                headers.insert(name, value.parse().unwrap());
            }
            if shape == "known_five_hour" {
                headers.insert(
                    "anthropic-ratelimit-unified-5h-status",
                    "rejected".parse().unwrap(),
                );
                headers.insert(
                    "anthropic-ratelimit-unified-5h-utilization",
                    "1".parse().unwrap(),
                );
                headers.insert(
                    "anthropic-ratelimit-unified-5h-reset",
                    (reset - chrono::Duration::days(1))
                        .timestamp()
                        .to_string()
                        .parse()
                        .unwrap(),
                );
            }
            f.relay
                .accounts
                .observe_headers(&f.ids[0], Provider::Claude, &headers)
                .await;
            assert_eq!(f.account(0).quota_cooling.as_ref().unwrap().window, "7d");
            let mut fresh = json!({"five_hour":null,"seven_day":{"utilization":51,"resets_at":reset,"status":"allowed"}});
            if shape == "omitted" {
                fresh.as_object_mut().unwrap().remove("five_hour");
            }
            if shape == "malformed" {
                fresh["five_hour"] = json!({"utilization":0});
            }
            f.fake.state.push(reply(fresh));
            f.fake.state.push(profile_reply(&f));
            f.relay.quota.poll(&f.ids[0]).await.unwrap();
            let ready = shape == "null";
            assert_eq!(f.account(0).health == Health::Ready, ready, "{shape}");
            assert_eq!(
                f.relay.accounts.eligible(&f.ids[0], None).await,
                ready,
                "{shape}"
            );
            assert_eq!(f.account(0).quota_cooling.is_none(), ready, "{shape}");
            let views = f.relay.accounts.typed_views().await.unwrap();
            assert_eq!(
                views
                    .iter()
                    .find(|v| v.id == f.ids[0])
                    .unwrap()
                    .pool_eligible,
                ready,
                "{shape}"
            );
            assert_requests_with_profiles(&f, 0, 1);
        }
    }

    fn usage(used: u64) -> Value {
        json!({
            "five_hour":{"utilization":used,"resets_at":"2099-11-01T00:00:00Z"},
            "seven_day":{"utilization":37,"resets_at":"2099-11-07T00:00:00Z"},
            "seven_day_opus":{"utilization":if used == 100 {100} else {29},"resets_at":"2099-11-07T00:00:00Z"},
            "seven_day_sonnet":{"utilization":11,"resets_at":"2099-11-07T00:00:00Z"}
        })
    }

    fn grant(id: &str, label: &str, clears: Value, left: u64) -> Value {
        json!({"id":id,"label":label,"type":"full","resets_total":2,
            "resets_left":left,"starts_at":"2020-01-01T00:00:00Z",
            "ends_at":"2099-12-01T00:00:00Z","clears":clears,"blocking":[],
            "paused":false,"usable_now":left > 0,"use_requires_limit":true,
            "percent_used":{"five_hour":100,"seven_day_opus":100},"gives_back_weekly":true})
    }

    fn details(fable_left: u64) -> Value {
        let mut value = usage(100);
        // Full expires earlier, but provider recommends Fable. No invented
        // Fable wire enum: the installed client's known windows and label apply.
        let mut full = grant("full", "Full reset", json!(["five_hour", "seven_day"]), 1);
        full["ends_at"] = json!("2099-11-01T00:00:00Z");
        value["cedar_ember"] = json!({"eligible":true,"at_limit":true,
            "exhausted":["five_hour","seven_day_opus"],"next_grant_id":"fable",
            "weekly_resets_at":"2099-11-07T00:00:00Z","cooldown_until":null,
            "grants":[full,grant("fable", "Fable reset", json!(["seven_day_opus"]), fable_left)]});
        value
    }

    async fn fixture() -> Fixture {
        let f = Fixture::with_provider(1, Provider::Claude).await;
        let url = reqwest::Url::parse(&f.fake.url).unwrap();
        assert_eq!(url.scheme(), "http");
        assert!(
            url.host_str()
                .unwrap()
                .parse::<std::net::IpAddr>()
                .unwrap()
                .is_loopback()
        );
        let tokens = f
            .relay
            .quota
            .tokens
            .lock()
            .unwrap()
            .clone()
            .with_token_endpoint(
                Provider::Claude,
                format!("{}/synthetic-oauth/claude", f.fake.url),
            )
            .with_token_endpoint(
                Provider::Codex,
                format!("{}/synthetic-oauth/codex", f.fake.url),
            );
        f.relay.quota.set_tokens(tokens);
        f.relay
            .quota
            .store
            .update_metadata(&f.ids[0], |a| {
                a.plan = Some("max".into());
                a.health = Health::Ready;
            })
            .unwrap();
        f
    }

    fn assert_requests(f: &Fixture, posts: usize) {
        assert_requests_with_profiles(f, posts, 0);
    }

    fn profile_reply(f: &Fixture) -> ScriptedResponse {
        let account = f.account(0);
        let Identity::Claude {
            account_uuid,
            organization_uuid: Some(organization_uuid),
            ..
        } = &account.identity
        else {
            panic!("synthetic Claude account and organization identity");
        };
        reply(json!({
            "account":{"uuid":account_uuid,"has_claude_max":true},
            "organization":{"uuid":organization_uuid,"rate_limit_tier":"default_claude_max_5x"}
        }))
    }

    fn assert_requests_with_profiles(f: &Fixture, posts: usize, profiles: usize) {
        let requests = f.fake.state.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.path_and_query == "/api/oauth/profile")
                .count(),
            profiles
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method == reqwest::Method::POST)
                .count(),
            posts
        );
        for request in requests {
            assert!(
                match request.method {
                    reqwest::Method::GET =>
                        matches!(request.path_and_query.as_str(), DETAILS | USAGE)
                            || (profiles == 1 && request.path_and_query == "/api/oauth/profile"),
                    reqwest::Method::POST => request.path_and_query == CONSUME,
                    _ => false,
                },
                "unexpected synthetic upstream request: {} {}",
                request.method,
                request.path_and_query
            );
            assert_eq!(
                request.headers["authorization"],
                "Bearer synthetic-usage-access"
            );
            assert_eq!(request.headers["accept"], "application/json");
            assert_eq!(request.headers["anthropic-beta"], "oauth-2025-04-20");
            assert_eq!(
                request.headers["user-agent"],
                concat!("quota-baton/", env!("CARGO_PKG_VERSION"))
            );
            assert!(!request.headers.contains_key("chatgpt-account-id"));
            assert!(!request.headers.contains_key("x-app"));
            assert!(!request.headers.contains_key("anthropic-client-platform"));
        }
    }

    fn queue_preview(f: &Fixture, value: Value) {
        f.fake.state.push(reply(value));
        f.fake.state.push(reply(usage(100)));
    }

    async fn preview(f: &Fixture, selected: Option<&str>) -> ResetCreditPreview {
        queue_preview(f, details(2));
        f.relay
            .quota
            .preview_reset_credit_grant(&f.ids[0], selected.map(str::to_owned))
            .await
            .unwrap()
    }

    fn affirmative(wire: &ResetCreditPreview) -> ResetCreditCommitRequest {
        ResetCreditCommitRequest {
            preview_token: wire.preview_token.clone(),
            confirm: true,
        }
    }

    fn queue_commit(f: &Fixture, response: ScriptedResponse, after: Value, fresh: Option<Value>) {
        f.fake.state.push(reply(details(2)));
        f.fake.state.push(reply(usage(100)));
        f.fake.state.push(response);
        f.fake.state.push(reply(after));
        if let Some(fresh) = fresh {
            f.fake.state.push(reply(fresh));
        }
    }

    fn reset_reply() -> ScriptedResponse {
        reply(
            json!({"result":"reset","resets_left":1,"cleared":["seven_day_opus"],
            "weekly_resets_at":"2099-11-07T00:00:00Z"}),
        )
    }

    fn fresh_poller(f: &Fixture) -> Arc<UsagePoller> {
        let old = &f.relay.quota;
        let mut config = Config::default();
        config.upstream.codex_usage_base = f.fake.url.clone();
        config.upstream.claude_usage_base = f.fake.url.clone();
        UsagePoller::new(
            old.store.clone(),
            old.runtime.clone(),
            old.obs.clone(),
            old.client.clone(),
            old.tokens.lock().unwrap().clone(),
            &config,
        )
    }

    fn persisted_fence(f: &Fixture) -> Value {
        let files = std::fs::read_dir(f.relay.quota.store.home().join("state/reset-credits"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>();
        assert_eq!(
            files.len(),
            1,
            "exactly one synthetic account operation fence"
        );
        serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap()
    }

    fn post_body(f: &Fixture) -> Value {
        let requests = f.fake.state.requests();
        let posts = requests
            .iter()
            .filter(|r| r.method == reqwest::Method::POST)
            .collect::<Vec<_>>();
        assert_eq!(posts.len(), 1);
        serde_json::from_slice(&posts[0].body).unwrap()
    }

    #[tokio::test]
    async fn explicit_detail_read_uses_existing_claude_auth_and_ordinary_poll_stays_plain() {
        let f = fixture().await;
        let before = f.account(0);
        f.fake.state.push(reply(usage(83)));
        // Ordinary Claude polling also refreshes the existing profile lane.
        // Return this exact fixture identity, rather than letting an unscripted
        // failure mask a profile/credential regression.
        f.fake.state.push(profile_reply(&f));
        f.relay.quota.poll(&f.ids[0]).await.unwrap();
        assert_eq!(f.account(0).identity, before.identity);
        assert_eq!(f.account(0).plan.as_deref(), Some("max_5x"));
        f.fake.state.push(reply(details(2)));
        let inventory = f
            .relay
            .quota
            .refresh_reset_credits(&f.ids[0])
            .await
            .unwrap();
        assert_eq!(inventory.available_count, Some(3));
        assert_eq!(inventory.state, ResetCreditState::Fresh);
        let facts = inventory.claude.unwrap();
        assert!(facts.eligible);
        assert_eq!(facts.next_grant_id.as_deref(), Some("fable"));
        assert_eq!(facts.grants[1].label, "Fable reset");
        let requests = f.fake.state.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].path_and_query, USAGE);
        assert_eq!(requests[1].path_and_query, "/api/oauth/profile");
        assert_eq!(requests[2].path_and_query, DETAILS);
        for header in ["authorization", "accept", "user-agent", "anthropic-beta"] {
            assert_eq!(
                requests[0].headers.get(header),
                requests[2].headers.get(header)
            );
        }
        // Both ordinary-poll journeys permit exactly one scripted profile GET.
        // Other reset-credit cases retain assert_requests' zero-profile default.
        assert_requests_with_profiles(&f, 0, 1);
        for request in &requests {
            assert_eq!(request.method, reqwest::Method::GET);
            assert!(request.body.is_empty());
        }
    }

    #[tokio::test]
    async fn absent_and_null_programme_are_unknown_never_zero_or_previewable() {
        for null in [false, true] {
            let f = fixture().await;
            let mut absent = usage(100);
            if null {
                absent["cedar_ember"] = Value::Null;
            }
            f.fake.state.push(reply(absent.clone()));
            let inventory = f
                .relay
                .quota
                .refresh_reset_credits(&f.ids[0])
                .await
                .unwrap();
            assert_eq!(inventory.state, ResetCreditState::Unknown);
            assert_eq!(inventory.available_count, None);
            assert!(inventory.claude.is_none());
            f.fake.state.push(reply(absent));
            assert_eq!(
                f.relay.quota.preview_reset_credit(&f.ids[0]).await,
                Err(ResetError::Ineligible)
            );
            assert_requests(&f, 0);
        }
    }

    #[tokio::test]
    async fn ineligible_inventory_retains_provider_reason_without_claiming_eligibility() {
        for reason in ["surface", "cli_version", "tier"] {
            let f = fixture().await;
            let mut denied = details(2);
            denied["cedar_ember"]["eligible"] = json!(false);
            denied["cedar_ember"]["ineligible_reason"] = json!(reason);
            f.fake.state.push(reply(denied.clone()));
            let inventory = f
                .relay
                .quota
                .refresh_reset_credits(&f.ids[0])
                .await
                .unwrap();
            assert_eq!(inventory.refusal, Some(ResetCreditRefusal::Ineligible));
            let facts = inventory.claude.unwrap();
            assert!(!facts.eligible);
            assert_eq!(facts.ineligible_reason.as_deref(), Some(reason));
            f.fake.state.push(reply(denied));
            assert_eq!(
                f.relay.quota.preview_reset_credit(&f.ids[0]).await,
                Err(ResetError::Ineligible)
            );
            assert_requests(&f, 0);
        }
    }

    #[tokio::test]
    async fn multiple_usable_grants_require_explicit_full_or_fable_choice() {
        for expected in ["full", "fable"] {
            let f = fixture().await;
            let wire = preview(&f, Some(expected)).await;
            assert_eq!(wire.grant.id, expected);
            assert_eq!(wire.grant.reset_type, ResetCreditType::ClaudeRateLimits);
            assert_eq!(wire.inventory.available_count, Some(3));
            assert_requests(&f, 0);
        }
        let f = fixture().await;
        // next_grant_id is a UI recommendation, never implicit user choice.
        for old_wrapper in [false, true] {
            f.fake.state.push(reply(details(2)));
            let result = if old_wrapper {
                f.relay.quota.preview_reset_credit(&f.ids[0]).await
            } else {
                f.relay
                    .quota
                    .preview_reset_credit_grant(&f.ids[0], None)
                    .await
            };
            assert_eq!(result, Err(ResetError::Ineligible));
        }
        assert!(
            f.fake
                .state
                .requests()
                .iter()
                .all(|r| r.path_and_query == DETAILS)
        );
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn single_grant_and_zero_inventory_have_unambiguous_authority() {
        for left in [0, 1] {
            let f = fixture().await;
            let mut single = details(left);
            single["cedar_ember"]["grants"]
                .as_array_mut()
                .unwrap()
                .remove(0);
            f.fake.state.push(reply(single.clone()));
            let inventory = f
                .relay
                .quota
                .refresh_reset_credits(&f.ids[0])
                .await
                .unwrap();
            assert_eq!(inventory.available_count, Some(left));
            assert_eq!(inventory.claude.as_ref().unwrap().grants.len(), 1);
            f.fake.state.push(reply(single));
            if left == 0 {
                assert_eq!(
                    f.relay.quota.preview_reset_credit(&f.ids[0]).await,
                    Err(ResetError::Ineligible)
                );
            } else {
                f.fake.state.push(reply(usage(100)));
                assert_eq!(
                    f.relay
                        .quota
                        .preview_reset_credit(&f.ids[0])
                        .await
                        .unwrap()
                        .grant
                        .id,
                    "fable"
                );
            }
            assert_requests(&f, 0);
        }
    }

    #[tokio::test]
    async fn unusable_provider_default_falls_back_to_usable_grant_but_unknown_selection_is_refused()
    {
        let f = fixture().await;
        let mut value = details(2);
        value["cedar_ember"]["grants"][1]["usable_now"] = json!(false);
        value["cedar_ember"]["grants"][1]["unavailable_reason"] = json!("needs_limit");
        queue_preview(&f, value);
        let wire = f.relay.quota.preview_reset_credit(&f.ids[0]).await.unwrap();
        assert_eq!(wire.grant.id, "full");
        f.fake.state.push(reply(details(2)));
        assert_eq!(
            f.relay
                .quota
                .preview_reset_credit_grant(&f.ids[0], Some("not-owned".into()))
                .await,
            Err(ResetError::Ineligible)
        );
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn grant_becoming_unusable_after_preview_refuses_before_post() {
        let f = fixture().await;
        let wire = preview(&f, Some("fable")).await;
        let mut changed = details(2);
        changed["cedar_ember"]["grants"][1]["usable_now"] = json!(false);
        changed["cedar_ember"]["grants"][1]["unavailable_reason"] = json!("cooldown");
        f.fake.state.push(reply(changed));
        let result = f
            .relay
            .quota
            .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
            .await;
        assert!(result.is_err());
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn unusable_grants_never_send_even_if_another_grant_is_usable() {
        for reason in [
            "needs_limit",
            "cooldown",
            "paused",
            "expired",
            "blocked_opus",
        ] {
            let f = fixture().await;
            let mut blocked = details(2);
            let row = &mut blocked["cedar_ember"]["grants"][1];
            row["usable_now"] = json!(false);
            match reason {
                "paused" => row["paused"] = json!(true),
                "expired" => row["ends_at"] = json!("2020-01-01T00:00:00Z"),
                "blocked_opus" => {
                    row["blocking"] = json!(["seven_day_opus"]);
                    row["unavailable_reason"] = json!(reason);
                }
                _ => row["unavailable_reason"] = json!(reason),
            }
            if reason == "cooldown" {
                blocked["cedar_ember"]["cooldown_until"] = json!("2099-12-01T00:00:00Z");
            }
            if reason == "needs_limit" {
                blocked["cedar_ember"]["at_limit"] = json!(false);
            }
            f.fake.state.push(reply(blocked.clone()));
            let inventory = f
                .relay
                .quota
                .refresh_reset_credits(&f.ids[0])
                .await
                .unwrap();
            let facts = inventory.claude.unwrap();
            assert!(!facts.grants[1].usable_now);
            if matches!(reason, "needs_limit" | "cooldown" | "blocked_opus") {
                assert_eq!(facts.grants[1].unavailable_reason.as_deref(), Some(reason));
            }
            f.fake.state.push(reply(blocked));
            assert_eq!(
                f.relay
                    .quota
                    .preview_reset_credit_grant(&f.ids[0], Some("fable".into()))
                    .await,
                Err(ResetError::Ineligible),
                "{reason}"
            );
            assert_requests(&f, 0);
        }
    }

    #[tokio::test]
    async fn selected_grant_posts_once_with_fresh_request_id_and_real_fresh_usage() {
        let mut request_ids = std::collections::HashSet::new();
        for selected in ["full", "fable"] {
            let f = fixture().await;
            f.fake.state.push(reply(usage(94)));
            f.fake.state.push(profile_reply(&f));
            f.relay.quota.poll(&f.ids[0]).await.unwrap();
            let priming_requests = f.fake.state.requests();
            assert_eq!(priming_requests.len(), 2);
            assert_eq!(priming_requests[0].path_and_query, USAGE);
            assert_eq!(priming_requests[1].path_and_query, "/api/oauth/profile");
            assert_requests_with_profiles(&f, 0, 1);
            assert!(matches!(
                f.relay.quota.poll(&f.ids[0]).await,
                Err(PollError::TooSoon { .. })
            ));
            let wire = preview(&f, Some(selected)).await;
            let before = f.account(0);
            let mut after = details(if selected == "fable" { 1 } else { 2 });
            if selected == "full" {
                after["cedar_ember"]["grants"][0]["resets_left"] = json!(0);
                after["cedar_ember"]["grants"][0]["usable_now"] = json!(false);
            }
            let cleared = if selected == "full" {
                json!(["five_hour", "seven_day"])
            } else {
                json!(["seven_day_opus"])
            };
            queue_commit(
                &f,
                reply(
                    json!({"result":"reset","resets_left":if selected == "full" {0} else {1},
                "cleared":cleared}),
                ),
                after,
                Some(usage(17)),
            );
            let request = affirmative(&wire);
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), request.clone())
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Reset);
            assert!(!result.refresh_failed);
            let facts = result.claude.unwrap();
            assert_eq!(facts.result, "reset");
            assert_eq!(serde_json::to_value(&facts.cleared).unwrap(), cleared);
            assert_eq!(
                facts.resets_left,
                Some(if selected == "full" { 0 } else { 1 })
            );
            assert_eq!(result.inventory.available_count, Some(2));
            let body = post_body(&f);
            assert_eq!(body.as_object().unwrap().len(), 3);
            assert_eq!(body["program"], "cedar_ember");
            assert_eq!(body["grant_id"], selected);
            let request_id = body["request_id"].as_str().unwrap();
            assert!(!request_id.is_empty() && request_id.len() <= 64);
            assert!(
                request_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            );
            assert!(
                request_ids.insert(request_id.to_owned()),
                "fresh operation ID"
            );
            assert_eq!(persisted_fence(&f)["request"], request_id);
            let snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
            let windows = snapshot["windows"].as_array().unwrap();
            assert_eq!(
                windows.iter().find(|w| w["name"] == "5h").unwrap()["used_pct"],
                17.0
            );
            assert_eq!(
                windows.iter().find(|w| w["name"] == "7d").unwrap()["used_pct"],
                37.0
            );
            assert_eq!(
                windows.iter().find(|w| w["name"] == "7d_oi").unwrap()["used_pct"],
                29.0
            );
            let account = f.account(0);
            assert_eq!(account.user_state, before.user_state);
            assert_eq!(account.drain, before.drain);
            assert_eq!(account.allow_credits, before.allow_credits);
            let count = f.fake.state.requests().len();
            assert_eq!(
                f.relay
                    .quota
                    .commit_reset_credit(f.ids[0].clone(), request)
                    .await,
                Err(ResetError::Stale)
            );
            assert_eq!(f.fake.state.requests().len(), count);
            assert_requests_with_profiles(&f, 1, 1);
        }
    }

    #[tokio::test]
    async fn full_reset_recovers_polled_native_quota_hold_without_changing_reset_times() {
        let f = fixture().await;
        f.fake.state.push(reply(usage(100)));
        f.fake.state.push(profile_reply(&f));
        f.relay.quota.poll(&f.ids[0]).await.unwrap();
        assert_requests_with_profiles(&f, 0, 1);
        assert!(matches!(
            f.relay.quota.poll(&f.ids[0]).await,
            Err(PollError::TooSoon { .. })
        ));
        let before = f.account(0);
        let cause = before
            .quota_cooling
            .as_ref()
            .expect("ordinary poll owns hold");
        assert_eq!(cause.window, "5h");
        assert!(matches!(before.health, Health::Cooling { .. }));
        assert!(!f.relay.accounts.eligible(&f.ids[0], None).await);
        let before_snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
        let wire = preview(&f, Some("full")).await;
        let mut after_details = details(2);
        after_details["cedar_ember"]["grants"][0]["resets_left"] = json!(0);
        after_details["cedar_ember"]["grants"][0]["usable_now"] = json!(false);
        queue_commit(
            &f,
            reply(json!({"result":"reset","resets_left":0,
                "cleared":["five_hour","seven_day"]})),
            after_details,
            Some(usage(17)),
        );
        let result = f
            .relay
            .quota
            .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
            .await
            .unwrap();
        assert_eq!(result.outcome, ResetCreditOutcome::Reset);
        assert!(!result.refresh_failed);
        let after = f.account(0);
        assert_eq!(after.health, Health::Ready);
        assert!(after.quota_cooling.is_none());
        assert!(!after.quota_credit_releasable);
        assert!(f.relay.accounts.eligible(&f.ids[0], None).await);
        assert_eq!(after.user_state, before.user_state);
        assert_eq!(after.drain, before.drain);
        assert_eq!(after.allow_credits, before.allow_credits);
        let views = f.relay.accounts.typed_views().await.unwrap();
        let view = views.iter().find(|view| view.id == f.ids[0]).unwrap();
        assert!(view.pool_eligible);
        assert!(view.quota_cooling.is_none());
        let snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
        assert_eq!(snapshot["credits"], before_snapshot["credits"]);
        for (name, used) in [
            ("5h", 17.0),
            ("7d", 37.0),
            ("7d_oi", 29.0),
            ("7d_sonnet", 11.0),
        ] {
            let current = snapshot["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|window| window["name"] == name)
                .unwrap();
            let previous = before_snapshot["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|window| window["name"] == name)
                .unwrap();
            assert_eq!(current["used_pct"], json!(used));
            assert_eq!(current["reset_at"], previous["reset_at"]);
        }
        assert_eq!(snapshot["windows"][0]["reset_at"], json!(cause.reset_at));
        assert_eq!(post_body(&f)["grant_id"], "full");
        let requests = f.fake.state.requests();
        assert_eq!(requests.len(), 9);
        assert_eq!(requests.last().unwrap().path_and_query, USAGE);
        assert_requests_with_profiles(&f, 1, 1);
    }

    #[tokio::test]
    async fn reset_without_selected_cleared_scope_requires_ordinary_usage_release_margin() {
        // Without acknowledgement of the causing window, only R29's ordinary
        // observation rule may release: zero qualifies; 97% does not.
        for (cleared, used) in [
            (None, 0),
            (Some(json!(["seven_day_opus"])), 0),
            (None, 97),
            (Some(json!(["seven_day_opus"])), 97),
        ] {
            let f = fixture().await;
            f.fake.state.push(reply(usage(100)));
            f.fake.state.push(profile_reply(&f));
            f.relay.quota.poll(&f.ids[0]).await.unwrap();
            assert_requests_with_profiles(&f, 0, 1);
            assert!(matches!(
                f.relay.quota.poll(&f.ids[0]).await,
                Err(PollError::TooSoon { .. })
            ));
            let before = f.account(0);
            let cause = before
                .quota_cooling
                .as_ref()
                .expect("ordinary poll owns hold");
            assert_eq!(cause.window, "5h");
            assert!(!f.relay.accounts.eligible(&f.ids[0], None).await);
            let before_snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
            let wire = preview(&f, Some("full")).await;
            let mut response = json!({"result":"reset","resets_left":0});
            if let Some(cleared) = cleared {
                response["cleared"] = cleared;
            }
            let mut after_details = details(2);
            after_details["cedar_ember"]["grants"][0]["resets_left"] = json!(0);
            after_details["cedar_ember"]["grants"][0]["usable_now"] = json!(false);
            queue_commit(&f, reply(response), after_details, Some(usage(used)));
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Reset);
            assert!(!result.refresh_failed);
            let after = f.account(0);
            let ordinary_release = used == 0;
            if ordinary_release {
                assert_eq!(after.health, Health::Ready);
                assert!(after.quota_cooling.is_none());
            } else {
                assert_eq!(after.health, before.health);
                assert_eq!(after.quota_cooling, before.quota_cooling);
            }
            assert_eq!(
                f.relay.accounts.eligible(&f.ids[0], None).await,
                ordinary_release
            );
            assert_eq!(after.user_state, before.user_state);
            assert_eq!(after.drain, before.drain);
            assert_eq!(after.allow_credits, before.allow_credits);
            let views = f.relay.accounts.typed_views().await.unwrap();
            let view = views.iter().find(|view| view.id == f.ids[0]).unwrap();
            assert_eq!(view.pool_eligible, ordinary_release);
            assert_eq!(view.quota_cooling.is_none(), ordinary_release);
            let snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
            let current = snapshot["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|window| window["name"] == "5h")
                .unwrap();
            assert_eq!(
                current["used_pct"],
                json!(if ordinary_release { 0.0 } else { 97.0 })
            );
            assert_eq!(current["reset_at"], json!(cause.reset_at));
            assert_eq!(snapshot["credits"], before_snapshot["credits"]);
            assert_eq!(post_body(&f)["grant_id"], "full");
            let requests = f.fake.state.requests();
            assert_eq!(requests.len(), 9);
            assert_eq!(requests.last().unwrap().path_and_query, USAGE);
            assert_requests_with_profiles(&f, 1, 1);
        }
    }

    #[tokio::test]
    async fn weekly_reset_partial_usage_cannot_erase_uncleared_exhausted_five_hour_window() {
        for malformed_five_hour in [false, true] {
            let f = fixture().await;
            let mut initial_usage = usage(100);
            initial_usage["seven_day"]["utilization"] = json!(100);
            initial_usage["seven_day_opus"]["utilization"] = json!(29);
            f.fake.state.push(reply(initial_usage.clone()));
            f.fake.state.push(profile_reply(&f));
            f.relay.quota.poll(&f.ids[0]).await.unwrap();
            assert_requests_with_profiles(&f, 0, 1);
            assert!(matches!(
                f.relay.quota.poll(&f.ids[0]).await,
                Err(PollError::TooSoon { .. })
            ));
            let before = f.account(0);
            let cause = before.quota_cooling.as_ref().expect("poll-created hold");
            assert_eq!(cause.window, "7d");
            let before_snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
            let old_five_hour = before_snapshot["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|window| window["name"] == "5h")
                .unwrap();
            assert_eq!(old_five_hour["used_pct"], json!(100.0));
            assert!(
                old_five_hour["reset_at"]
                    .as_str()
                    .unwrap()
                    .parse::<chrono::DateTime<Utc>>()
                    .unwrap()
                    < cause.reset_at
            );
            assert!(!f.relay.accounts.eligible(&f.ids[0], None).await);

            let mut weekly = grant("weekly", "Weekly reset", json!(["seven_day"]), 1);
            weekly["type"] = json!("weekly");
            weekly["percent_used"] = json!({"seven_day":100});
            let mut inventory = initial_usage.clone();
            inventory["cedar_ember"] = details(2)["cedar_ember"].clone();
            inventory["cedar_ember"]["grants"] = json!([weekly]);
            inventory["cedar_ember"]["next_grant_id"] = json!("weekly");
            queue_preview(&f, inventory.clone());
            let wire = f
                .relay
                .quota
                .preview_reset_credit_grant(&f.ids[0], Some("weekly".into()))
                .await
                .unwrap();
            let mut after_details = inventory.clone();
            after_details["cedar_ember"]["grants"][0]["resets_left"] = json!(0);
            after_details["cedar_ember"]["grants"][0]["usable_now"] = json!(false);
            let mut partial = json!({"seven_day":{
                "utilization":0,"resets_at":initial_usage["seven_day"]["resets_at"]}});
            if malformed_five_hour {
                partial["five_hour"] = json!({"utilization":"invalid","resets_at":"invalid"});
            }
            for value in [
                inventory,
                usage(100),
                json!({"result":"reset","resets_left":0,"cleared":["seven_day"]}),
                after_details,
                partial,
            ] {
                f.fake.state.push(reply(value));
            }
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Reset);
            let after = f.account(0);
            assert!(matches!(after.health, Health::Cooling { .. }));
            assert!(after.quota_cooling.is_some());
            assert!(!f.relay.accounts.eligible(&f.ids[0], None).await);
            let views = f.relay.accounts.typed_views().await.unwrap();
            let view = views.iter().find(|view| view.id == f.ids[0]).unwrap();
            assert!(!view.pool_eligible);
            let projected_five_hour = view
                .windows
                .iter()
                .find(|window| window.name == "5h")
                .expect("uncleared window remains projected");
            assert_eq!(projected_five_hour.used_pct, Some(100.0));
            let snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
            let five_hour = snapshot["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|window| window["name"] == "5h")
                .expect("partial GET cannot erase exhausted 5h");
            assert_eq!(five_hour["used_pct"], old_five_hour["used_pct"]);
            assert_eq!(five_hour["reset_at"], old_five_hour["reset_at"]);
            assert_eq!(after.user_state, before.user_state);
            assert_eq!(after.drain, before.drain);
            assert_eq!(after.allow_credits, before.allow_credits);
            assert_eq!(post_body(&f)["grant_id"], "weekly");
            let requests = f.fake.state.requests();
            assert_eq!(requests.len(), 9);
            assert_eq!(requests.last().unwrap().path_and_query, USAGE);
            assert_requests_with_profiles(&f, 1, 1);
        }
    }

    #[tokio::test]
    async fn full_reset_retires_preexisting_native_header_rejection_at_same_deadline() {
        let f = fixture().await;
        f.fake.state.push(reply(usage(100)));
        f.fake.state.push(profile_reply(&f));
        f.relay.quota.poll(&f.ids[0]).await.unwrap();
        assert_requests_with_profiles(&f, 0, 1);
        assert!(matches!(
            f.relay.quota.poll(&f.ids[0]).await,
            Err(PollError::TooSoon { .. })
        ));
        let poll_cause = f.account(0).quota_cooling.expect("poll-created hold");
        assert_eq!(poll_cause.window, "5h");
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
            (
                "anthropic-ratelimit-unified-reset",
                poll_cause.reset_at.timestamp().to_string(),
            ),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "five_hour".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-5h-status",
                "rejected".to_owned(),
            ),
            ("anthropic-ratelimit-unified-5h-utilization", "1".to_owned()),
            (
                "anthropic-ratelimit-unified-5h-reset",
                poll_cause.reset_at.timestamp().to_string(),
            ),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        // Actual header parsing, cache merge and native reduction happen before
        // preview/commit captures any reset authority; no fabricated cache row.
        let observed = f
            .relay
            .accounts
            .observe_headers(&f.ids[0], Provider::Claude, &headers)
            .await;
        assert_eq!(observed.quota_until, Some(poll_cause.reset_at));
        let before = f.account(0);
        assert_eq!(before.quota_cooling.as_ref().unwrap().window, "5h");
        assert_eq!(
            before.quota_cooling.as_ref().unwrap().reset_at,
            poll_cause.reset_at
        );
        assert!(!f.relay.accounts.eligible(&f.ids[0], None).await);
        let rejected = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
        assert_eq!(rejected["signals"]["unified_status"], "rejected");
        assert_eq!(rejected["signals"]["representative_claim"], "five_hour");
        assert_eq!(
            rejected["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|window| window["name"] == "5h")
                .unwrap()["status"],
            "rejected"
        );

        let wire = preview(&f, Some("full")).await;
        let mut after_details = details(2);
        after_details["cedar_ember"]["grants"][0]["resets_left"] = json!(0);
        after_details["cedar_ember"]["grants"][0]["usable_now"] = json!(false);
        queue_commit(
            &f,
            reply(json!({"result":"reset","resets_left":0,
            "cleared":["five_hour","seven_day"]})),
            after_details,
            Some(usage(0)),
        );
        let result = f
            .relay
            .quota
            .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
            .await
            .unwrap();
        assert_eq!(result.outcome, ResetCreditOutcome::Reset);
        assert!(!result.refresh_failed);
        let after = f.account(0);
        assert_eq!(after.health, Health::Ready);
        assert!(after.quota_cooling.is_none());
        assert!(f.relay.accounts.eligible(&f.ids[0], None).await);
        let views = f.relay.accounts.typed_views().await.unwrap();
        let view = views.iter().find(|view| view.id == f.ids[0]).unwrap();
        assert!(view.pool_eligible);
        assert!(view.quota_cooling.is_none());
        let snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
        let five_hour = snapshot["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|window| window["name"] == "5h")
            .unwrap();
        assert_eq!(five_hour["used_pct"], json!(0.0));
        assert_eq!(five_hour["reset_at"], json!(poll_cause.reset_at));
        assert_ne!(five_hour["status"], "rejected");
        assert_ne!(snapshot["signals"]["unified_status"], "rejected");
        assert_ne!(snapshot["signals"]["limit_reached"], json!(true));
        assert_eq!(snapshot["credits"], rejected["credits"]);
        assert_eq!(after.user_state, before.user_state);
        assert_eq!(after.drain, before.drain);
        assert_eq!(after.allow_credits, before.allow_credits);
        assert_eq!(post_body(&f)["grant_id"], "full");
        let requests = f.fake.state.requests();
        assert_eq!(requests.len(), 9);
        assert_eq!(requests.last().unwrap().path_and_query, USAGE);
        assert_requests_with_profiles(&f, 1, 1);
    }

    #[tokio::test]
    async fn refusal_results_preserve_claude_facts_and_never_retry() {
        for (result_name, reason) in [
            ("already_used", "already_used"),
            ("not_limited", "not_limited"),
            ("cooldown", "cooldown"),
            ("ineligible", "surface"),
            ("unavailable", "paused"),
        ] {
            let f = fixture().await;
            let wire = preview(&f, Some("fable")).await;
            queue_commit(
                &f,
                reply(json!({"result":result_name,"reason":reason,"resets_left":2,
                "cleared":[],"cooldown_until":"2099-12-01T00:00:00Z"})),
                details(2),
                None,
            );
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Refused, "{result_name}");
            let facts = result.claude.unwrap();
            assert_eq!(facts.result, result_name);
            assert_eq!(facts.reason.as_deref(), Some(reason));
            assert!(facts.cleared.is_empty());
            assert_requests(&f, 1);
        }
    }

    #[tokio::test]
    async fn separate_explicit_operations_on_same_home_and_grant_get_distinct_request_ids() {
        let f = fixture().await;
        for left in [2, 1] {
            queue_preview(&f, details(left));
            let wire = f
                .relay
                .quota
                .preview_reset_credit_grant(&f.ids[0], Some("fable".into()))
                .await
                .unwrap();
            for body in [
                details(left),
                usage(100),
                json!({"result":"reset","resets_left":left - 1,"cleared":["seven_day_opus"]}),
                details(left - 1),
                usage(17),
            ] {
                f.fake.state.push(reply(body));
            }
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Reset);
        }
        let posts = f
            .fake
            .state
            .requests()
            .into_iter()
            .filter(|r| r.method == reqwest::Method::POST)
            .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0]["grant_id"], "fable");
        assert_eq!(posts[1]["grant_id"], "fable");
        assert!(
            posts[0]["request_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(
            posts[1]["request_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert_ne!(posts[0]["request_id"], posts[1]["request_id"]);
        assert_eq!(persisted_fence(&f)["request"], posts[1]["request_id"]);
        assert_requests(&f, 2);
    }

    #[tokio::test]
    async fn rate_limited_post_does_not_retry_or_fabricate_reset() {
        let f = fixture().await;
        let wire = preview(&f, Some("fable")).await;
        let mut response = ScriptedResponse::json(reqwest::StatusCode::TOO_MANY_REQUESTS, "{}");
        response.headers.insert("retry-after", "0".parse().unwrap());
        queue_commit(&f, response, details(2), None);
        let result = f
            .relay
            .quota
            .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
            .await
            .unwrap();
        assert_eq!(result.outcome, ResetCreditOutcome::Refused);
        assert_eq!(
            result.claude.unwrap().reason.as_deref(),
            Some("rate_limited")
        );
        assert_eq!(f.fake.state.requests().len(), 6);
        assert_eq!(f.account(0).health, Health::Ready);
        assert_requests(&f, 1);
    }

    #[tokio::test]
    async fn read_auth_errors_revoke_access_without_preview_authority_or_oauth_retry() {
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            for preview_window in [false, true] {
                let f = fixture().await;
                if preview_window {
                    f.fake.state.push(reply(details(2)));
                }
                f.fake.state.push(ScriptedResponse::json(status, "{}"));
                if preview_window {
                    assert_eq!(
                        f.relay
                            .quota
                            .preview_reset_credit_grant(&f.ids[0], Some("fable".into()))
                            .await,
                        Err(ResetError::Ineligible)
                    );
                } else {
                    assert_eq!(
                        f.relay.quota.refresh_reset_credits(&f.ids[0]).await,
                        Err(ResetError::Ineligible)
                    );
                }
                assert!(matches!(f.account(0).health, Health::NeedsRelogin { .. }));
                let requests = f.fake.state.requests();
                assert_eq!(requests.len(), if preview_window { 2 } else { 1 });
                assert_eq!(requests[0].path_and_query, DETAILS);
                if preview_window {
                    assert_eq!(requests[1].path_and_query, USAGE);
                }
                assert!(
                    requests
                        .iter()
                        .all(|request| request.method == reqwest::Method::GET)
                );
                let entry = f.relay.quota.reset_credits.entry(&f.ids[0]).unwrap();
                assert!(entry.lock().await.preview.is_none());
                assert!(
                    f.relay
                        .quota
                        .preview_reset_credit_grant(&f.ids[0], Some("fable".into()))
                        .await
                        .is_err()
                );
                assert!(
                    f.relay
                        .quota
                        .commit_reset_credit(
                            f.ids[0].clone(),
                            ResetCreditCommitRequest {
                                preview_token: "synthetic-no-preview-authority".into(),
                                confirm: true,
                            }
                        )
                        .await
                        .is_err()
                );
                assert_eq!(f.fake.state.requests().len(), requests.len());
                // The strict allowlist and auth checks reject any token refresh,
                // profile lookup, extra read, or redemption POST.
                assert_requests(&f, 0);
            }
        }
    }

    #[tokio::test]
    async fn post_auth_errors_enter_normal_relogin_without_refresh_or_replay() {
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            let f = fixture().await;
            let wire = preview(&f, Some("fable")).await;
            // Auth refusal must end at POST: no inventory read or OAuth refresh.
            f.fake.state.push(reply(details(2)));
            f.fake.state.push(reply(usage(100)));
            f.fake.state.push(ScriptedResponse::json(status, "{}"));
            let request = affirmative(&wire);
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), request.clone())
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Refused);
            assert_eq!(
                result.claude.unwrap().reason.as_deref(),
                Some("authentication_required")
            );
            assert!(result.refresh_failed);
            assert_eq!(f.fake.state.requests().len(), 5);
            assert_eq!(
                f.fake.state.requests().last().unwrap().path_and_query,
                CONSUME
            );
            assert!(
                matches!(f.account(0).health, Health::NeedsRelogin { .. }),
                "HTTP {status}"
            );
            let count = f.fake.state.requests().len();
            assert!(
                f.relay
                    .quota
                    .commit_reset_credit(f.ids[0].clone(), request)
                    .await
                    .is_err()
            );
            assert_eq!(f.fake.state.requests().len(), count);
            assert_requests(&f, 1);
        }
    }

    #[tokio::test]
    async fn truncated_post_remains_fenced_across_coordinator_reconstruction_until_read_only_proof()
    {
        for proof in ["unchanged", "count_drop", "windows_clear"] {
            let f = fixture().await;
            let wire = preview(&f, Some("fable")).await;
            let mut truncated = ScriptedResponse::json(reqwest::StatusCode::OK, "{");
            truncated
                .headers
                .insert("content-length", "4096".parse().unwrap());
            // The failed response is identical before and after application.
            // Immediate reconciliation is deliberately still unchanged.
            queue_commit(&f, truncated, details(2), None);
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Unknown);
            let fence = persisted_fence(&f);
            assert_eq!(fence["outcome"], "unknown");
            assert_eq!(fence["request"], post_body(&f)["request_id"]);
            let restarted = fresh_poller(&f);
            let count = f.fake.state.requests().len();
            assert_eq!(
                restarted.preview_reset_credit(&f.ids[0]).await,
                Err(ResetError::Unknown)
            );
            assert_eq!(f.fake.state.requests().len(), count);
            let mut settled = details(if proof == "count_drop" { 1 } else { 2 });
            if proof == "windows_clear" {
                settled["cedar_ember"]["exhausted"] = json!([]);
                settled["cedar_ember"]["at_limit"] = json!(false);
                settled["cedar_ember"]["grants"][1]["percent_used"] = json!({"seven_day_opus":0});
                settled["seven_day_opus"]["utilization"] = json!(0);
            }
            f.fake.state.push(reply(settled));
            if proof != "unchanged" {
                // Positive settlement also obtains actual quota facts through
                // the forced read; inventory percentages are never published as usage.
                f.fake.state.push(reply(usage(23)));
            }
            let refreshed = restarted.refresh_reset_credits(&f.ids[0]).await;
            if proof == "unchanged" {
                assert_eq!(refreshed, Err(ResetError::Unknown));
                assert_eq!(persisted_fence(&f)["outcome"], "unknown");
                let count = f.fake.state.requests().len();
                assert_eq!(
                    restarted.preview_reset_credit(&f.ids[0]).await,
                    Err(ResetError::Unknown)
                );
                assert_eq!(f.fake.state.requests().len(), count);
            } else {
                assert!(refreshed.is_ok(), "{proof}: {refreshed:?}");
                assert_ne!(persisted_fence(&f)["outcome"], "unknown");
                assert_eq!(persisted_fence(&f)["request"], fence["request"]);
                assert_eq!(
                    f.fake.state.requests().last().unwrap().path_and_query,
                    USAGE
                );
                let snapshot = restarted.obs.quota_snapshot(&f.ids[0]).unwrap();
                let windows = snapshot["windows"].as_array().unwrap();
                assert_eq!(
                    windows.iter().find(|w| w["name"] == "5h").unwrap()["used_pct"],
                    23.0
                );
                assert_eq!(
                    windows.iter().find(|w| w["name"] == "7d_oi").unwrap()["used_pct"],
                    29.0
                );
            }
            assert_requests(&f, 1);
        }
    }

    #[tokio::test]
    async fn durable_request_id_and_unknown_fence_exist_before_post_response() {
        let f = fixture().await;
        let wire = preview(&f, Some("fable")).await;
        queue_commit(&f, reset_reply(), details(1), Some(usage(17)));
        let (mut arrived, mut release) = f.fake.state.pause_next_response();
        let poller = f.relay.quota.clone();
        let id = f.ids[0].clone();
        let sending =
            tokio::spawn(async move { poller.commit_reset_credit(id, affirmative(&wire)).await });
        // Chain gates through both revalidation reads; stop at POST before its
        // response, so inspecting durable evidence cannot see a terminal write.
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(5), arrived)
                .await
                .unwrap()
                .unwrap();
            let (next_arrived, next_release) = f.fake.state.pause_next_response();
            release.send(()).unwrap();
            arrived = next_arrived;
            release = next_release;
        }
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        assert_requests(&f, 1);
        let fence = persisted_fence(&f);
        assert_eq!(fence["grant"], "fable");
        assert_eq!(fence["outcome"], "unknown");
        assert_eq!(fence["request"], post_body(&f)["request_id"]);
        let other = fresh_poller(&f);
        let count = f.fake.state.requests().len();
        assert_eq!(
            other.refresh_reset_credits(&f.ids[0]).await,
            Err(ResetError::Busy)
        );
        assert_eq!(f.fake.state.requests().len(), count);
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), sending)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .outcome,
            ResetCreditOutcome::Reset
        );
        assert_requests(&f, 1);
    }

    #[tokio::test]
    async fn active_and_paused_accounts_preserve_placement_settings_after_use() {
        for state in [UserState::Active, UserState::Paused] {
            let f = fixture().await;
            f.relay
                .quota
                .store
                .update_metadata(&f.ids[0], |a| {
                    a.user_state = state;
                    a.drain = true;
                    a.allow_credits = true;
                })
                .unwrap();
            let wire = preview(&f, Some("fable")).await;
            queue_commit(&f, reset_reply(), details(1), Some(usage(19)));
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Reset);
            assert_eq!(f.account(0).user_state, state);
            assert!(f.account(0).drain);
            assert!(f.account(0).allow_credits);
            assert_requests(&f, 1);
        }
    }

    #[tokio::test]
    async fn disabled_relogin_free_and_no_code_accounts_refuse_without_any_upstream() {
        for restriction in ["disabled", "relogin", "free", "no_code"] {
            let f = fixture().await;
            f.relay
                .quota
                .store
                .update_metadata(&f.ids[0], |a| match restriction {
                    "disabled" => a.user_state = UserState::Disabled,
                    "relogin" => {
                        a.health = Health::NeedsRelogin {
                            reason: "synthetic".into(),
                            since: Utc::now(),
                        }
                    }
                    "free" => a.plan = Some("free".into()),
                    "no_code" => a.health = Health::PlanWithoutClaudeCode,
                    _ => unreachable!(),
                })
                .unwrap();
            let before = f.account(0);
            assert!(
                f.relay
                    .quota
                    .refresh_reset_credits(&f.ids[0])
                    .await
                    .is_err(),
                "{restriction}"
            );
            assert!(
                f.relay.quota.preview_reset_credit(&f.ids[0]).await.is_err(),
                "{restriction}"
            );
            assert!(
                f.relay
                    .quota
                    .commit_reset_credit(
                        f.ids[0].clone(),
                        ResetCreditCommitRequest {
                            preview_token: "synthetic-nonexistent-preview".into(),
                            confirm: true,
                        }
                    )
                    .await
                    .is_err()
            );
            assert_eq!(f.account(0), before);
            assert!(f.fake.state.requests().is_empty(), "{restriction}");
            assert_requests(&f, 0);
        }
    }

    #[tokio::test]
    async fn live_preview_cannot_bypass_later_account_permission_revocation() {
        for restriction in ["disabled", "relogin", "free", "no_code"] {
            let f = fixture().await;
            let wire = preview(&f, Some("fable")).await;
            f.relay
                .quota
                .store
                .update_metadata(&f.ids[0], |a| match restriction {
                    "disabled" => a.user_state = UserState::Disabled,
                    "relogin" => {
                        a.health = Health::NeedsRelogin {
                            reason: "synthetic".into(),
                            since: Utc::now(),
                        }
                    }
                    "free" => a.plan = Some("free".into()),
                    "no_code" => a.health = Health::PlanWithoutClaudeCode,
                    _ => unreachable!(),
                })
                .unwrap();
            let before = f.account(0);
            let count = f.fake.state.requests().len();
            assert!(
                f.relay
                    .quota
                    .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                    .await
                    .is_err()
            );
            assert_eq!(f.account(0), before);
            assert_eq!(f.fake.state.requests().len(), count);
            assert_requests(&f, 0);
        }
    }

    #[tokio::test]
    async fn native_admin_requires_auth_and_passes_selected_grant_to_one_commit() {
        use crate::{admin, http, local_secrets::LocalSecrets};
        let f = fixture().await;
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        let mut servers = http::spawn_with_admin(
            &config,
            LocalSecrets {
                client_key: "synthetic-r28-client".into(),
                admin_token: "synthetic-r28-admin".into(),
            },
            admin::runtime_routes(f.relay.clone(), 0),
        )
        .await
        .unwrap();
        assert!(servers.admin_addr.ip().is_loopback());
        let base = format!(
            "http://{}/admin/v1/accounts/{}/reset-credits",
            servers.admin_addr, f.ids[0]
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let refused = client
            .post(format!("{base}/preview"))
            .json(&json!({"grant_id":"full"}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(f.fake.state.requests().is_empty());
        queue_preview(&f, details(2));
        let wire: ResetCreditPreview = client
            .post(format!("{base}/preview"))
            .bearer_auth("synthetic-r28-admin")
            .json(&json!({"grant_id":"full"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        // Choose against next_grant_id=fable so a route dropping grant_id fails.
        assert_eq!(wire.grant.id, "full");
        assert_requests(&f, 0);
        let cancel = client
            .post(format!("{base}/commit"))
            .bearer_auth("synthetic-r28-admin")
            .json(&json!({"preview_token":wire.preview_token,"confirm":false}))
            .send()
            .await
            .unwrap();
        assert_eq!(cancel.status(), reqwest::StatusCode::CONFLICT);
        assert_requests(&f, 0);
        let mut after = details(2);
        after["cedar_ember"]["grants"][0]["resets_left"] = json!(0);
        after["cedar_ember"]["grants"][0]["usable_now"] = json!(false);
        queue_commit(
            &f,
            reply(json!({"result":"reset","resets_left":0,
            "cleared":["five_hour","seven_day"]})),
            after,
            Some(usage(23)),
        );
        let request = affirmative(&wire);
        let result: ResetCreditResult = client
            .post(format!("{base}/commit"))
            .bearer_auth("synthetic-r28-admin")
            .json(&request)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result.outcome, ResetCreditOutcome::Reset);
        assert_eq!(result.claude.unwrap().cleared, ["five_hour", "seven_day"]);
        assert_eq!(post_body(&f)["grant_id"], "full");
        let replay = client
            .post(format!("{base}/commit"))
            .bearer_auth("synthetic-r28-admin")
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), reqwest::StatusCode::CONFLICT);
        assert_requests(&f, 1);
        servers.shutdown();
        (&mut servers.data_task).await.unwrap().unwrap();
        (&mut servers.admin_task).await.unwrap().unwrap();
    }
}
