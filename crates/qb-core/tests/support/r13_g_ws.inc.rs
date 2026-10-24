include!("r13_g_capacity.inc.rs");
include!("r13_h_capacity.inc.rs");

#[cfg(feature = "testkit")]
mod r13_g {
    use super::*;

    const KEY: &str = "codex:r13-g-actual-upgrade";

    async fn bound(t: &LaunchHarness, strict: bool, seconds: i64) -> relay::LaunchReply {
        let launch = t.issue(Some(&t.h.ids[0])).await;
        let mut first = t.connect(&launch, Some("r13-g-actual-upgrade")).await;
        first.close(None).await.unwrap();
        drop(first);
        capacity_count(&t.h, &launch.account, 0).await;
        let table = t.h.state.bindings.clone();
        tokio::task::spawn_blocking(move || {
            table.set_routing_session(
                &qb_core::routing::session_id(KEY),
                Some(qb_core::routing::Settings {
                    mode: if strict {
                        qb_core::routing::Mode::Strict
                    } else {
                        qb_core::routing::Mode::Recover
                    },
                    reset_wait_secs: 60,
                    on_5h_limit: qb_core::routing::On5hLimit::Wait,
                }),
                None,
                false,
            )
        })
        .await
        .unwrap()
        .unwrap();
        if strict {
            // set_routing_session's None is the optional project, not an
            // anchor override. The existing classified binding supplies it.
            let view = t.h.state.bindings.routing_view();
            let observed = view["observed_sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == qb_core::routing::session_id(KEY))
                .unwrap();
            assert_eq!(observed["policy"]["settings"]["mode"], "strict");
            assert_eq!(observed["policy"]["anchor"], launch.account);
        }
        let now = Utc::now();
        let reset = now + ChronoDuration::seconds(seconds);
        t.h.store
            .update_metadata(&launch.account, |account| {
                account.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: reset,
                    scope: "account".into(),
                };
                account.quota_cooling = Some(qb_core::accounts::QuotaCooling {
                    window: "5h".into(),
                    reset_at: reset,
                    started_at: now,
                });
            })
            .unwrap();
        launch
    }

    // Keep a real source catalog request stalled while the upgrade reuses its
    // proved current model. Destination catalogs were warmed by issue_launch.
    async fn stall_source(
        t: &LaunchHarness,
        launch: &relay::LaunchReply,
    ) -> (tokio::task::JoinHandle<()>, oneshot::Sender<()>) {
        assert!(
            t.h.state
                .clear_codex_catalog_for_test(&launch.account)
                .unwrap()
        );
        let (arrived, release) = t.catalogs.pause();
        let url = launch.catalog_url.clone();
        let task = tokio::spawn(async move {
            let _ = reqwest::Client::new()
                .get(url)
                .bearer_auth("synthetic-local-key")
                .send()
                .await;
        });
        tokio::time::timeout(Duration::from_secs(2), arrived)
            .await
            .expect("source catalog must actually be stalled")
            .unwrap();
        (task, release)
    }

    #[tokio::test]
    async fn r13_g_eight_actual_upgrades_far_wait_first_moves_despite_stalled_source_catalog() {
        let t = LaunchHarness::new(2).await;
        let launch = bound(&t, false, 4 * 60 * 60).await;
        let (catalog_task, release) = stall_source(&t, &launch).await;
        let catalog_requests = t.catalogs.requests.lock().unwrap().len();
        for index in 0..8 {
            let (mut client, response) = tokio::time::timeout(
                Duration::from_secs(1),
                connect_async(t.request(&launch.base_url, Some("r13-g-actual-upgrade"))),
            )
            .await
            .expect("each upgrade must decide before its timeout envelope")
            .unwrap();
            assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
            assert_eq!(
                response.headers()["x-quota-baton-account"],
                t.h.ids[1].as_str()
            );
            assert_eq!(
                t.h.state.bindings.current(Some(KEY)).as_deref(),
                Some(t.h.ids[1].as_str())
            );
            assert_eq!(t.h.fake.state.requests().len(), index + 2);
            // Catalogs use a separate fake recorder, never the WS dial count.
            assert_eq!(t.catalogs.requests.lock().unwrap().len(), catalog_requests);
            if index == 0 {
                let moves: Vec<_> =
                    t.h.ledger()
                        .into_iter()
                        .filter(|row| {
                            row["event"] == "binding_move"
                                && row["from"] == launch.account
                                && row["to"] == t.h.ids[1]
                        })
                        .collect();
                assert_eq!(moves.len(), 1, "first upgrade publishes exactly one move");
                assert_eq!(moves[0]["reason"], "five_hour_far_move");
            }
            client.close(None).await.unwrap();
            drop(client);
            capacity_count(&t.h, &t.h.ids[1], 0).await;
        }
        // Preserve the incident's HTTP fallback shape after all eight upgrades.
        t.http_reply();
        let response = reqwest::Client::new()
            .post(format!("{}/responses", launch.base_url))
            .bearer_auth("synthetic-local-key")
            .header("session-id", "r13-g-actual-upgrade")
            .json(&json!({"model":"synthetic-model","input":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = release.send(());
        catalog_task.await.unwrap();
    }

    #[tokio::test]
    async fn r13_g_strict_far_upgrade_never_moves_or_waits_for_source_catalog() {
        let t = LaunchHarness::new(2).await;
        let launch = bound(&t, true, 4 * 60 * 60).await;
        let (catalog_task, release) = stall_source(&t, &launch).await;
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            connect_async(t.request(&launch.base_url, Some("r13-g-actual-upgrade"))),
        )
        .await
        .expect("strict limit decision must precede upgrade timeout")
        .unwrap_err();
        let Error::Http(response) = error else {
            panic!("strict upgrade must return an HTTP refusal")
        };
        let body: Value = serde_json::from_slice(
            response
                .body()
                .as_ref()
                .expect("strict refusal includes provider-shaped JSON"),
        )
        .unwrap();
        // An authorized strict anchor with applicable quota returns its limit;
        // unrelated strict policy failures remain conflicts.
        assert_eq!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "actual body: {body}"
        );
        assert_eq!(
            body["error"]["type"], "usage_limit_reached",
            "actual body: {body}"
        );
        assert!(body["error"]["resets_at"].as_i64().unwrap() > Utc::now().timestamp());
        assert!(body["error"]["resets_in_seconds"].as_i64().unwrap() > 0);
        assert!(response.headers().contains_key("retry-after"));
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
        assert_eq!(
            t.h.state.bindings.current(Some(KEY)).as_deref(),
            Some(launch.account.as_str())
        );
        assert_eq!(t.h.fake.state.requests().len(), 1);
        let _ = release.send(());
        catalog_task.await.unwrap();
    }

    #[tokio::test]
    async fn r13_g_five_second_reset_within_ten_second_upgrade_holds_without_moving() {
        // Preserve the production WS bound (<15s) and reset_fits_transport.
        // The 30-second near-reset requirement belongs to HTTP coverage;
        // WS exercises the same hold decision with a reset inside its budget.
        let t = LaunchHarness::new_with_upgrade_deadline(2, None, 60, 10).await;
        let launch = bound(&t, false, 5).await;
        let upgrade = connect_async(t.request(&launch.base_url, Some("r13-g-actual-upgrade")));
        tokio::pin!(upgrade);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut upgrade)
                .await
                .is_err(),
            "inside-bound reset must enter a hold, not immediately move"
        );
        assert_eq!(
            t.h.state.bindings.current(Some(KEY)).as_deref(),
            Some(launch.account.as_str())
        );
        assert_eq!(t.h.fake.state.requests().len(), 1);
        // Cancellation ends this observation before the five-second wall reset.
    }
}
