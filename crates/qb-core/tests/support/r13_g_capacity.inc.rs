// Included alongside the far-quota tests; do not add another Cargo test target.
#[cfg(feature = "testkit")]
mod r13_g_capacity {
    use super::*;

    #[tokio::test]
    async fn r13_g_capacity_refusal_falls_back_to_same_account_http_without_moving() {
        for (accounts, strict) in [(1, false), (2, false), (2, true)] {
            let t = LaunchHarness::new(accounts).await;
            let launch = t.issue(Some(&t.h.ids[0])).await;
            let session = "r13-g-capacity-http";
            let key = format!("codex:{session}");
            let mut live = t.connect(&launch, Some(session)).await;
            if strict {
                let table = t.h.state.bindings.clone();
                let key = key.clone();
                tokio::task::spawn_blocking(move || {
                    table.set_routing_session(
                        &qb_core::routing::session_id(&key),
                        Some(qb_core::routing::Settings {
                            mode: qb_core::routing::Mode::Strict,
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
            }

            // Learn real socket capacity from an upstream event while keeping
            // the first socket live. Capacity must not become quota/auth health.
            t.h.fake.state.queue_send(Message::text(
                json!({"type":"error","status":400,"error":{
                    "code":"websocket_connection_limit_reached"
                }})
                .to_string(),
            ));
            let mut rejected = t.connect(&launch, Some(session)).await;
            let Message::Text(raw) = receive(&mut rejected).await else {
                panic!("expected actual capacity evidence");
            };
            assert_eq!(
                serde_json::from_str::<Value>(&raw).unwrap()["error"]["code"],
                "websocket_connection_limit_reached"
            );
            rejected.close(None).await.unwrap();
            drop(rejected);
            capacity_count(&t.h, &launch.account, 1).await;
            let upstream_before = t.h.fake.state.requests().len();
            let waits_before = t.h.state.ws_capacity_waits_for_test();

            // Even a caller that repeats upgrades gets a transport refusal,
            // not another connect timeout or a dial beyond the learned cap.
            for _ in 0..3 {
                let began = std::time::Instant::now();
                let error = tokio::time::timeout(
                    Duration::from_secs(1),
                    connect_async(t.request(&launch.base_url, Some(session))),
                )
                .await
                .expect("capacity refusal must precede the two-second upgrade deadline")
                .unwrap_err();
                let Error::Http(response) = error else {
                    panic!("expected HTTP fallback status on the upgrade");
                };
                assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
                assert!(began.elapsed() >= Duration::from_millis(100));
                assert!(!response.headers().contains_key("retry-after"));
                let body: Value =
                    serde_json::from_slice(response.body().as_ref().unwrap()).unwrap();
                assert_eq!(body["error"]["type"], "websocket_capacity");
                assert_eq!(t.h.fake.state.requests().len(), upstream_before);
                assert_eq!(
                    t.h.state.bindings.current(Some(&key)).as_deref(),
                    Some(launch.account.as_str())
                );
            }
            assert!(t.h.state.ws_capacity_waits_for_test() >= waits_before + 3);

            t.http_reply();
            let response = reqwest::Client::new()
                .post(format!("{}/responses", launch.base_url))
                .bearer_auth("synthetic-local-key")
                .header("session-id", session)
                .json(&json!({"model":"synthetic-model","input":[]}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _ = response.bytes().await.unwrap();
            let http = t.h.token_fake.state.requests();
            assert_eq!(http.len(), 1);
            assert_eq!(
                http[0].headers["chatgpt-account-id"],
                t.h.fake.state.requests()[0].headers["chatgpt-account-id"]
            );
            assert_eq!(t.h.fake.state.requests().len(), upstream_before);
            capacity_count(&t.h, &launch.account, 1).await;
            for id in &t.h.ids {
                assert_eq!(t.h.store.load(id).unwrap().health, Health::Ready);
                if id != &launch.account {
                    assert_eq!(t.h.state.ws_reserved_for_test(id), 0);
                }
            }
            let ledger = t.h.ledger();
            let refusals: Vec<_> = ledger.iter().filter(|row| row["status"] == 426).collect();
            assert_eq!(refusals.len(), 3);
            assert!(refusals.iter().all(|row| {
                row.get("timeout_phase").is_none()
                    && row["attempt_details"].as_array().unwrap().is_empty()
            }));
            assert!(!ledger.iter().any(|row| row["status"] == 504));
            assert!(
                !ledger
                    .iter()
                    .any(|row| { row["event"] == "binding_move" && row["from"] == launch.account })
            );
            live.close(None).await.unwrap();
            drop(live);
            capacity_count(&t.h, &launch.account, 0).await;
        }
    }
}
