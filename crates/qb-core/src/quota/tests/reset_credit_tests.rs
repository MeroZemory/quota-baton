// Included inside quota::tests so the existing isolated Fixture is reused.
// Source-only until Main runs the Cargo gate. All HTTP targets are loopback.
// The token-endpoint injection seam exists only with debug assertions.
#[cfg(debug_assertions)]
mod r26_reset_credit_tests {
    use super::*;
    use crate::reset_credits::Error as ResetError;
    use qb_proto::{reset_credit_action::*, reset_credits::*};
    use serde_json::Value;

    const DETAILS: &str = "/backend-api/wham/rate-limit-reset-credits";
    const CONSUME: &str = "/backend-api/wham/rate-limit-reset-credits/consume";
    const USAGE: &str = "/backend-api/wham/usage";

    fn reply(value: Value) -> ScriptedResponse {
        ScriptedResponse::json(reqwest::StatusCode::OK, value.to_string())
    }

    fn grant(id: &str, expires_at: &str, status: &str) -> Value {
        json!({"id":id,"reset_type":"codex_rate_limits","status":status,
            "granted_at":"2020-01-01T00:00:00Z","expires_at":expires_at})
    }

    fn inventory(applied: bool) -> Value {
        // Deliberately unsorted: neither row position nor ID chooses earliest.
        json!({"available_count":if applied { 1 } else { 2 },"credits":[
            grant("a-later", "2099-12-01T00:00:00Z", "available"),
            grant("z-earliest", "2099-11-01T00:00:00Z",
                if applied { "redeemed" } else { "available" })
        ]})
    }

    fn windows(used: u64, count: u64) -> Value {
        // Absolute reset avoids receipt-time noise between preview and commit.
        json!({"rate_limit_reset_credits":{"available_count":count},
            "credits":{"has_credits":true,"balance":"42","unlimited":false},
            "rate_limit":{"primary_window":{"used_percent":used,
                "limit_window_seconds":18000,"reset_at":4070908800_i64}}})
    }

    fn assert_loopback(f: &Fixture) {
        let url = reqwest::Url::parse(&f.fake.url).unwrap();
        assert_eq!(url.scheme(), "http");
        assert!(
            url.host_str()
                .unwrap()
                .parse::<std::net::IpAddr>()
                .unwrap()
                .is_loopback()
        );
        // Even a regression that attempts OAuth refresh stays on the fake.
        // assert_requests then rejects that unexpected token endpoint call.
        let tokens = f
            .relay
            .quota
            .tokens
            .lock()
            .unwrap()
            .clone()
            .with_token_endpoint(
                Provider::Codex,
                format!("{}/synthetic-oauth/codex", f.fake.url),
            )
            .with_token_endpoint(
                Provider::Claude,
                format!("{}/synthetic-oauth/claude", f.fake.url),
            );
        f.relay.quota.set_tokens(tokens);
    }

    fn assert_requests(f: &Fixture, posts: usize) {
        let requests = f.fake.state.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method == reqwest::Method::POST)
                .count(),
            posts
        );
        for request in requests {
            // Also excludes token refresh, profile fetch and generic mutation retry.
            assert!(
                match request.method {
                    reqwest::Method::GET =>
                        matches!(request.path_and_query.as_str(), DETAILS | USAGE),
                    reqwest::Method::POST => request.path_and_query == CONSUME,
                    _ => false,
                },
                "unexpected synthetic upstream request path"
            );
        }
    }

    async fn preview(f: &Fixture, n: usize) -> ResetCreditPreview {
        assert_loopback(f);
        f.fake.state.push(reply(inventory(false)));
        f.fake.state.push(reply(windows(90, 2)));
        f.relay.quota.preview_reset_credit(&f.ids[n]).await.unwrap()
    }

    fn affirmative(preview: &ResetCreditPreview) -> ResetCreditCommitRequest {
        ResetCreditCommitRequest {
            preview_token: preview.preview_token.clone(),
            confirm: true,
        }
    }

    fn queue_commit(f: &Fixture, response: ScriptedResponse, applied: bool) {
        f.fake.state.push(reply(inventory(false)));
        f.fake.state.push(reply(windows(90, 2)));
        f.fake.state.push(response);
        f.fake.state.push(reply(inventory(applied)));
    }

    fn fresh_poller(f: &Fixture) -> Arc<UsagePoller> {
        // Reconstruct only the coordinator. No in-memory preview/hub is reused;
        // the already-held synthetic AccountStore supplies the same private home.
        assert_loopback(f);
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

    #[tokio::test]
    async fn preview_and_cancel_are_read_only_and_leave_account_unchanged() {
        let f = Fixture::new(1).await;
        let before = f.account(0);
        let wire = preview(&f, 0).await;
        assert_eq!(wire.grant.id, "z-earliest");
        assert_eq!(wire.account, f.ids[0]);
        assert_eq!(wire.inventory.available_count, Some(2));
        assert_eq!(wire.effect, "eligible_codex_limits");
        // Dismiss/cancel is represented by dropping the preview without commit.
        drop(wire);
        assert_eq!(f.account(0), before);
        assert_eq!(f.fake.state.requests().len(), 2);
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn nonaffirmative_confirmation_sends_no_mutation() {
        let f = Fixture::new(1).await;
        let wire = preview(&f, 0).await;
        let result = f
            .relay
            .quota
            .commit_reset_credit(
                f.ids[0].clone(),
                ResetCreditCommitRequest {
                    preview_token: wire.preview_token,
                    confirm: false,
                },
            )
            .await;
        assert_eq!(result, Err(ResetError::Stale));
        assert_eq!(f.fake.state.requests().len(), 2);
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn affirmative_sends_exact_selected_id_once_and_replay_is_refused() {
        let f = Fixture::new(1).await;
        assert_loopback(&f);
        // A recent ordinary poll must not suppress the post-redemption GET.
        f.fake.state.push(reply(windows(88, 2)));
        f.relay.quota.poll(&f.ids[0]).await.unwrap();
        assert!(matches!(
            f.relay.quota.poll(&f.ids[0]).await,
            Err(PollError::TooSoon { .. })
        ));
        let wire = preview(&f, 0).await;
        let before = f.account(0);
        queue_commit(&f, reply(json!({"code":"reset","windows_reset":1})), true);
        f.fake.state.push(reply(windows(17, 1)));
        let request = affirmative(&wire);
        let result = f
            .relay
            .quota
            .commit_reset_credit(f.ids[0].clone(), request.clone())
            .await
            .unwrap();
        assert_eq!(result.outcome, ResetCreditOutcome::Reset);
        assert!(!result.refresh_failed);
        assert_eq!(result.inventory.available_count, Some(1));
        let requests = f.fake.state.requests();
        let post = requests
            .iter()
            .find(|r| r.method == reqwest::Method::POST)
            .unwrap();
        let body: Value = serde_json::from_slice(&post.body).unwrap();
        assert_eq!(body["credit_id"], "z-earliest");
        assert_eq!(body.as_object().unwrap().len(), 2);
        assert_eq!(body["redeem_request_id"].as_str().unwrap().len(), 32);
        assert!(
            body["redeem_request_id"]
                .as_str()
                .unwrap()
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        );
        let Identity::Codex { account_id, .. } = &before.identity else {
            panic!("synthetic Codex identity")
        };
        assert_eq!(
            post.headers
                .get("ChatGPT-Account-Id")
                .unwrap()
                .to_str()
                .unwrap(),
            account_id
        );
        let snapshot = f.relay.quota.obs.quota_snapshot(&f.ids[0]).unwrap();
        assert_eq!(snapshot["windows"][0]["used_pct"], 17.0);
        assert_eq!(snapshot["credits"]["balance"], 42.0);
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method == reqwest::Method::GET && r.path_and_query == USAGE)
                .count(),
            4
        );
        assert_eq!(f.account(0).user_state, before.user_state);
        assert_eq!(f.account(0).drain, before.drain);
        assert_eq!(f.account(0).allow_credits, before.allow_credits);
        let count = requests.len();
        assert_eq!(
            f.relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), request)
                .await,
            Err(ResetError::Stale)
        );
        assert_eq!(f.fake.state.requests().len(), count);
        assert_requests(&f, 1);
    }

    #[tokio::test]
    async fn concurrent_confirmation_is_busy_and_cannot_double_spend() {
        let f = Fixture::new(1).await;
        let wire = preview(&f, 0).await;
        queue_commit(
            &f,
            reply(json!({"code":"nothing_to_reset","windows_reset":0})),
            false,
        );
        let (arrived, release) = f.fake.state.pause_next_response();
        let poller = f.relay.quota.clone();
        let id = f.ids[0].clone();
        let request = affirmative(&wire);
        let first_request = request.clone();
        let first =
            tokio::spawn(async move { poller.commit_reset_credit(id, first_request).await });
        tokio::time::timeout(Duration::from_secs(3), arrived)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            f.relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), request)
                .await,
            Err(ResetError::Busy)
        );
        assert_requests(&f, 0);
        release.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), first)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.outcome, ResetCreditOutcome::NothingToReset);
        assert_requests(&f, 1);
    }

    #[tokio::test]
    async fn account_swap_rejects_even_with_another_accounts_live_preview() {
        let f = Fixture::new(2).await;
        let a = preview(&f, 0).await;
        let b = preview(&f, 1).await;
        assert_ne!(a.preview_token, b.preview_token);
        let before = f.fake.state.requests().len();
        assert_eq!(
            f.relay
                .quota
                .commit_reset_credit(f.ids[1].clone(), affirmative(&a))
                .await,
            Err(ResetError::Stale)
        );
        assert_eq!(f.fake.state.requests().len(), before);
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn expired_preview_is_refused_before_revalidation_or_post() {
        let f = Fixture::new(1).await;
        let wire = preview(&f, 0).await;
        let entry = f.relay.quota.reset_credits.entry(&f.ids[0]).unwrap();
        entry.lock().await.preview.as_mut().unwrap().wire.expires_at =
            (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        assert_eq!(
            f.relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await,
            Err(ResetError::Stale)
        );
        assert_eq!(f.fake.state.requests().len(), 2);
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn account_metadata_change_invalidates_preview_without_mutation() {
        let f = Fixture::new(1).await;
        let wire = preview(&f, 0).await;
        let generation = f.account(0).generation;
        f.relay
            .quota
            .store
            .update_metadata(&f.ids[0], |a| a.label.push_str(" changed"))
            .unwrap();
        assert!(f.account(0).generation > generation);
        assert_eq!(
            f.relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await,
            Err(ResetError::Stale)
        );
        assert_eq!(f.fake.state.requests().len(), 2);
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn changed_inventory_including_nonselected_grant_requires_new_preview() {
        for change_count in [false, true] {
            let f = Fixture::new(1).await;
            let wire = preview(&f, 0).await;
            let mut changed = inventory(false);
            if change_count {
                changed["available_count"] = json!(3);
            } else {
                changed["credits"][0]["expires_at"] = json!("2099-12-02T00:00:00Z");
            }
            f.fake.state.push(reply(changed));
            assert_eq!(
                f.relay
                    .quota
                    .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                    .await,
                Err(ResetError::Stale)
            );
            assert_eq!(f.fake.state.requests().len(), 3);
            assert_requests(&f, 0);
        }
    }

    #[tokio::test]
    async fn changed_window_effect_requires_new_preview_without_mutation() {
        for change in ["replace", "add", "remove"] {
            let f = Fixture::new(1).await;
            let wire = preview(&f, 0).await;
            let mut changed = windows(91, 2);
            match change {
                "replace" => {
                    changed["rate_limit"]["primary_window"]["limit_window_seconds"] = json!(604800)
                }
                "add" => {
                    changed["rate_limit"]["secondary_window"] = json!({
                    "used_percent":20,"limit_window_seconds":604800,"reset_at":4070908800_i64})
                }
                "remove" => {
                    changed["rate_limit"]
                        .as_object_mut()
                        .unwrap()
                        .remove("primary_window");
                }
                _ => unreachable!(),
            }
            f.fake.state.push(reply(inventory(false)));
            f.fake.state.push(reply(changed));
            assert_eq!(
                f.relay
                    .quota
                    .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                    .await,
                Err(ResetError::Stale)
            );
            assert_eq!(f.fake.state.requests().len(), 4);
            assert_requests(&f, 0);
        }
    }

    #[tokio::test]
    async fn ordinary_usage_and_observation_generation_changes_preserve_confirmation() {
        let f = Fixture::new(1).await;
        let wire = preview(&f, 0).await;
        let before = f.account(0);
        f.fake.state.push(reply(windows(91, 2)));
        f.relay.quota.poll(&f.ids[0]).await.unwrap();
        f.relay
            .quota
            .store
            .update_metadata(&f.ids[0], |a| {
                a.plan_observed_at = Some(Utc::now());
            })
            .unwrap();
        let observed = f.account(0);
        assert!(observed.generation > before.generation);
        assert_ne!(observed.plan_observed_at, before.plan_observed_at);
        assert_eq!(observed.plan, before.plan);
        let mut traffic = windows(95, 2);
        traffic["rate_limit"]["primary_window"]["reset_at"] = json!(4070908860_i64);
        traffic["rate_limit"]["primary_window"]["status"] = json!("allowed_warning");
        f.fake.state.push(reply(inventory(false)));
        f.fake.state.push(reply(traffic));
        f.fake
            .state
            .push(reply(json!({"code":"nothing_to_reset","windows_reset":0})));
        f.fake.state.push(reply(inventory(false)));
        let result = f
            .relay
            .quota
            .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
            .await
            .unwrap();
        assert_eq!(result.outcome, ResetCreditOutcome::NothingToReset);
        assert_requests(&f, 1);
    }

    #[tokio::test]
    async fn separate_poller_cannot_pass_process_lock_held_by_inflight_commit() {
        let f = Fixture::new(1).await;
        let a = preview(&f, 0).await;
        let other = fresh_poller(&f);
        f.fake.state.push(reply(inventory(false)));
        f.fake.state.push(reply(windows(90, 2)));
        let b = other.preview_reset_credit(&f.ids[0]).await.unwrap();
        queue_commit(
            &f,
            reply(json!({"code":"nothing_to_reset","windows_reset":0})),
            false,
        );
        let (arrived, release) = f.fake.state.pause_next_response();
        let poller = f.relay.quota.clone();
        let id = f.ids[0].clone();
        let first =
            tokio::spawn(async move { poller.commit_reset_credit(id, affirmative(&a)).await });
        tokio::time::timeout(Duration::from_secs(3), arrived)
            .await
            .unwrap()
            .unwrap();
        let count = f.fake.state.requests().len();
        assert_eq!(
            other
                .commit_reset_credit(f.ids[0].clone(), affirmative(&b))
                .await,
            Err(ResetError::Busy)
        );
        assert_eq!(
            other.refresh_reset_credits(&f.ids[0]).await,
            Err(ResetError::Busy)
        );
        assert_eq!(
            other.preview_reset_credit(&f.ids[0]).await,
            Err(ResetError::Busy)
        );
        assert_eq!(f.fake.state.requests().len(), count);
        assert_requests(&f, 0);
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), first)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .outcome,
            ResetCreditOutcome::NothingToReset
        );
        assert_requests(&f, 1);
    }

    #[tokio::test]
    async fn close_joins_retained_commit_before_terminal_persistence_and_rejects_new_admission() {
        let f = Fixture::new(1).await;
        let wire = preview(&f, 0).await;
        queue_commit(
            &f,
            reply(json!({"code":"nothing_to_reset","windows_reset":0})),
            false,
        );
        let (arrived, release) = f.fake.state.pause_next_response();
        let poller = f.relay.quota.clone();
        let id = f.ids[0].clone();
        let request = affirmative(&wire);
        let caller = tokio::spawn(async move { poller.commit_reset_credit(id, request).await });
        // The first revalidation GET is held before the fence and POST.
        tokio::time::timeout(Duration::from_secs(3), arrived)
            .await
            .unwrap()
            .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let closing = f.relay.quota.close_reset_credits();
        tokio::pin!(closing);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut closing)
                .await
                .is_err()
        );
        assert_eq!(
            f.relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await,
            Err(ResetError::Unavailable)
        );
        assert_requests(&f, 0);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), &mut closing)
            .await
            .unwrap();
        assert_requests(&f, 1);
        assert_eq!(
            f.relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await,
            Err(ResetError::Unavailable)
        );
        // A new coordinator can read terminal evidence and acquire the released
        // process lock only after close has joined the retained operation.
        let other = fresh_poller(&f);
        f.fake.state.push(reply(inventory(false)));
        f.fake.state.push(reply(windows(90, 2)));
        assert_eq!(
            other
                .preview_reset_credit(&f.ids[0])
                .await
                .unwrap()
                .grant
                .id,
            "z-earliest"
        );
        assert_requests(&f, 1);
    }

    #[tokio::test]
    async fn http_errors_after_dispatch_are_unknown_without_auth_refresh_or_resend() {
        for status in [401, 403, 429, 500, 503] {
            let f = Fixture::new(1).await;
            let wire = preview(&f, 0).await;
            queue_commit(
                &f,
                ScriptedResponse::json(
                    reqwest::StatusCode::from_u16(status).unwrap(),
                    r#"{"error":"synthetic refusal with no authoritative consumption proof"}"#,
                ),
                false,
            );
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Unknown, "HTTP {status}");
            assert_eq!(result.inventory.state, ResetCreditState::Stale);
            assert_eq!(result.inventory.available_count, Some(2));
            let count = f.fake.state.requests().len();
            assert_eq!(
                f.relay.quota.preview_reset_credit(&f.ids[0]).await,
                Err(ResetError::Unknown)
            );
            assert_eq!(
                f.relay
                    .quota
                    .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                    .await,
                Err(ResetError::Stale)
            );
            assert_eq!(f.fake.state.requests().len(), count);
            assert_eq!(count, 6);
            assert_requests(&f, 1);
        }
    }

    #[tokio::test]
    async fn malformed_success_body_stays_unknown_and_replay_sends_nothing() {
        for body in [r#"{"code":"future_code"}"#, r#"{"code":"reset" broken"#] {
            let f = Fixture::new(1).await;
            let wire = preview(&f, 0).await;
            queue_commit(
                &f,
                ScriptedResponse::json(reqwest::StatusCode::OK, body),
                false,
            );
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Unknown);
            let count = f.fake.state.requests().len();
            assert_eq!(
                f.relay.quota.preview_reset_credit(&f.ids[0]).await,
                Err(ResetError::Unknown)
            );
            assert_eq!(f.fake.state.requests().len(), count);
            assert_requests(&f, 1);
        }
    }

    #[tokio::test]
    async fn transport_ambiguity_survives_new_coordinator_until_exact_grant_reconciliation() {
        for applied in [false, true] {
            let f = Fixture::new(1).await;
            let wire = preview(&f, 0).await;
            // Truncated HTTP body: the fake records dispatch, but the client
            // cannot know whether application preceded the broken response.
            let mut truncated = ScriptedResponse::json(reqwest::StatusCode::OK, "{");
            truncated
                .headers
                .insert("content-length", "4096".parse().unwrap());
            queue_commit(&f, truncated, applied);
            let result = f
                .relay
                .quota
                .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
                .await
                .unwrap();
            assert_eq!(result.outcome, ResetCreditOutcome::Unknown);
            assert_requests(&f, 1);
            let restarted = fresh_poller(&f);
            let count = f.fake.state.requests().len();
            assert_eq!(
                restarted.preview_reset_credit(&f.ids[0]).await,
                Err(ResetError::Unknown)
            );
            assert_eq!(f.fake.state.requests().len(), count);
            f.fake.state.push(reply(inventory(applied)));
            let reconciled = restarted.refresh_reset_credits(&f.ids[0]).await;
            if applied {
                assert_eq!(reconciled.unwrap().available_count, Some(1));
                f.fake.state.push(reply(inventory(true)));
                f.fake.state.push(reply(windows(90, 1)));
                let next = restarted.preview_reset_credit(&f.ids[0]).await.unwrap();
                assert_eq!(next.grant.id, "a-later");
                // Opening a new preview after proof still spends nothing.
            } else {
                assert_eq!(reconciled, Err(ResetError::Unknown));
                let before = f.fake.state.requests().len();
                assert_eq!(
                    restarted.preview_reset_credit(&f.ids[0]).await,
                    Err(ResetError::Unknown)
                );
                assert_eq!(f.fake.state.requests().len(), before);
            }
            assert_requests(&f, 1);
        }
    }

    #[tokio::test]
    async fn successful_redemption_with_failed_refresh_is_still_success_and_cached_stale() {
        let f = Fixture::new(1).await;
        let wire = preview(&f, 0).await;
        f.fake.state.push(reply(inventory(false)));
        f.fake.state.push(reply(windows(90, 2)));
        f.fake
            .state
            .push(reply(json!({"code":"reset","windows_reset":1})));
        f.fake.state.push(ScriptedResponse::json(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "{}",
        ));
        f.fake.state.push(reply(windows(12, 1)));
        let result = f
            .relay
            .quota
            .commit_reset_credit(f.ids[0].clone(), affirmative(&wire))
            .await
            .unwrap();
        assert_eq!(result.outcome, ResetCreditOutcome::Reset);
        assert!(result.refresh_failed);
        assert_eq!(result.inventory.state, ResetCreditState::Stale);
        assert_eq!(result.inventory.available_count, Some(2));
        assert_eq!(result.inventory.refusal, Some(ResetCreditRefusal::Stale));
        assert_requests(&f, 1);
    }

    #[tokio::test]
    async fn read_only_refresh_failure_preserves_count_as_stale() {
        let f = Fixture::new(1).await;
        assert_loopback(&f);
        f.fake.state.push(reply(inventory(false)));
        let first = f
            .relay
            .quota
            .refresh_reset_credits(&f.ids[0])
            .await
            .unwrap();
        f.fake.state.push(ScriptedResponse::json(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "{}",
        ));
        assert_eq!(
            f.relay.quota.refresh_reset_credits(&f.ids[0]).await,
            Err(ResetError::Unavailable)
        );
        let cached = f
            .relay
            .quota
            .reset_credit_cached(&f.ids[0], Provider::Codex);
        assert_eq!(cached.state, ResetCreditState::Stale);
        assert_eq!(cached.available_count, first.available_count);
        assert_eq!(cached.grants, first.grants);
        assert_eq!(cached.observed_at, first.observed_at);
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn unsupported_provider_never_reaches_fake_or_claims_zero() {
        let f = Fixture::with_provider(1, Provider::Claude).await;
        assert_loopback(&f);
        let cached = f
            .relay
            .quota
            .reset_credit_cached(&f.ids[0], Provider::Claude);
        assert_eq!(cached.state, ResetCreditState::Unsupported);
        assert_eq!(cached.available_count, None);
        assert_eq!(
            f.relay.quota.preview_reset_credit(&f.ids[0]).await,
            Err(ResetError::Unsupported)
        );
        assert!(f.fake.state.requests().is_empty());
        assert_requests(&f, 0);
    }

    #[tokio::test]
    async fn zero_unknown_and_capped_inventory_never_produce_a_preview_or_post() {
        let mut capped = inventory(false);
        capped["available_count"] = json!(3);
        for details in [json!({"available_count":0,"credits":[]}), json!({}), capped] {
            let f = Fixture::new(1).await;
            assert_loopback(&f);
            f.fake.state.push(reply(details));
            assert_eq!(
                f.relay.quota.preview_reset_credit(&f.ids[0]).await,
                Err(ResetError::Ineligible)
            );
            assert_eq!(f.fake.state.requests().len(), 1);
            assert_requests(&f, 0);
        }
    }
}
