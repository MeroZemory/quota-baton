#[cfg(feature = "testkit")]
mod r13_h_capacity {
    use super::*;

    #[tokio::test]
    async fn r13_h_three_bound_sessions_keep_account_after_capacity_and_http_fallback() {
        let t = LaunchHarness::new(2).await;
        let source = t.h.ids[0].clone();
        for (id, plan) in [(&source, "pro"), (&t.h.ids[1], "free")] {
            t.h.store
                .update_metadata(id, |account| {
                    account.plan = Some(plan.into());
                })
                .unwrap();
        }
        // A launch preference is advisory. Seed the incident on one account
        // without disabling placement protection, then restore the alternative.
        t.h.state
            .accounts
            .change(&t.h.ids[1], Some(UserState::Paused), None, None, None)
            .await
            .unwrap();
        let mut launches = Vec::new();
        for index in 0..3 {
            let session = format!("synthetic-capacity-cohort-{index}");
            let launch = t.issue(Some(&source)).await;
            assert_eq!(launch.account, source);
            let mut socket = t.connect(&launch, Some(&session)).await;
            socket.close(None).await.unwrap();
            drop(socket);
            capacity_count(&t.h, &source, 0).await;
            launches.push((session, launch));
        }
        t.h.state
            .accounts
            .change(&t.h.ids[1], Some(UserState::Active), None, None, None)
            .await
            .unwrap();
        let mut occupied = t.connect(&launches[0].1, Some(&launches[0].0)).await;
        t.h.fake.state.queue_send(Message::text(
            json!({"type":"error","status":400,
            "error":{"code":"websocket_connection_limit_reached"}})
            .to_string(),
        ));
        let mut rejected = t.connect(&launches[0].1, Some(&launches[0].0)).await;
        let Message::Text(raw) = receive(&mut rejected).await else {
            panic!("capacity event")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["error"]["code"],
            "websocket_connection_limit_reached"
        );
        assert_eq!(t.h.state.ws_reserved_for_test(&source), 2);
        rejected.close(None).await.unwrap();
        drop(rejected);
        capacity_count(&t.h, &source, 1).await;
        let upstream_before = t.h.fake.state.requests().len();
        let moves_before: Vec<_> =
            t.h.ledger()
                .into_iter()
                .filter(|row| row["event"] == "binding_move")
                .collect();
        for (session, launch) in &launches {
            let error = tokio::time::timeout(
                Duration::from_secs(2),
                connect_async(t.request(&launch.base_url, Some(session))),
            )
            .await
            .unwrap()
            .unwrap_err();
            let Error::Http(response) = error else {
                panic!("HTTP fallback required")
            };
            assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
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
            response.bytes().await.unwrap();
            assert_eq!(
                t.h.state
                    .bindings
                    .current(Some(&format!("codex:{session}")))
                    .as_deref(),
                Some(source.as_str())
            );
        }
        let http = t.h.token_fake.state.requests();
        assert_eq!(http.len(), 3);
        let source_header = t.h.fake.state.requests()[0].headers["chatgpt-account-id"].clone();
        assert!(
            http.iter()
                .all(|request| request.headers["chatgpt-account-id"] == source_header)
        );
        assert_eq!(t.h.fake.state.requests().len(), upstream_before);
        assert_eq!(t.h.state.ws_reserved_for_test(&t.h.ids[1]), 0);
        let moves_after: Vec<_> =
            t.h.ledger()
                .into_iter()
                .filter(|row| row["event"] == "binding_move")
                .collect();
        assert_eq!(
            moves_after, moves_before,
            "capacity fallback cannot move an established binding"
        );
        occupied.close(None).await.unwrap();
        drop(occupied);
        capacity_count(&t.h, &source, 0).await;
    }
}
