mod r26_reset_credit_admin_tests {
    use super::*;
    use crate::{admin, http, local_secrets::LocalSecrets};
    use qb_proto::reset_credit_action::{
        ResetCreditOutcome, ResetCreditPreview, ResetCreditResult,
    };

    #[tokio::test]
    async fn authenticated_native_admin_confirmation_has_zero_cancel_and_one_explicit_post() {
        let f = Fixture::new(1).await;
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        let servers = http::spawn_with_admin(
            &config,
            LocalSecrets {
                client_key: "synthetic-r26-client".into(),
                admin_token: "synthetic-r26-admin".into(),
            },
            admin::runtime_routes(f.relay.clone(), 0),
        )
        .await
        .unwrap();
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
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(f.fake.state.requests().is_empty());
        let details = json!({"available_count":1,"credits":[{"id":"synthetic-r26-grant",
            "reset_type":"codex_rate_limits","status":"available","granted_at":"2020-01-01T00:00:00Z",
            "expires_at":"2099-01-01T00:00:00Z"}]});
        let usage = json!({"rate_limit":{"primary_window":{"used_percent":95,"limit_window_seconds":18000}}});
        for value in [&details, &usage] {
            f.fake.state.push(ScriptedResponse::json(
                reqwest::StatusCode::OK,
                value.to_string(),
            ));
        }
        let preview: ResetCreditPreview = client
            .post(format!("{base}/preview"))
            .bearer_auth("synthetic-r26-admin")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(preview.account, f.ids[0]);
        let cancelled = client
            .post(format!("{base}/commit"))
            .bearer_auth("synthetic-r26-admin")
            .json(&json!({"preview_token":preview.preview_token,"confirm":false}))
            .send()
            .await
            .unwrap();
        assert_eq!(cancelled.status(), reqwest::StatusCode::CONFLICT);
        assert!(
            f.fake
                .state
                .requests()
                .iter()
                .all(|request| request.method == reqwest::Method::GET)
        );
        let mut used = details.clone();
        used["available_count"] = json!(0);
        used["credits"][0]["status"] = json!("redeemed");
        for value in [
            &details,
            &usage,
            &json!({"code":"reset","windows_reset":1}),
            &used,
            &usage,
        ] {
            f.fake.state.push(ScriptedResponse::json(
                reqwest::StatusCode::OK,
                value.to_string(),
            ));
        }
        let reply: ResetCreditResult = client
            .post(format!("{base}/commit"))
            .bearer_auth("synthetic-r26-admin")
            .json(&json!({"preview_token":preview.preview_token,"confirm":true}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reply.outcome, ResetCreditOutcome::Reset);
        assert!(!reply.refresh_failed);
        let replay = client
            .post(format!("{base}/commit"))
            .bearer_auth("synthetic-r26-admin")
            .json(&json!({"preview_token":preview.preview_token,"confirm":true}))
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), reqwest::StatusCode::CONFLICT);
        let requests = f.fake.state.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == reqwest::Method::POST)
                .count(),
            1
        );
        assert!(
            requests
                .iter()
                .all(|request| request.path_and_query.starts_with("/backend-api/wham/"))
        );
        let state: serde_json::Value = client
            .get(format!("http://{}/admin/v1/state", servers.admin_addr))
            .bearer_auth("synthetic-r26-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["accounts"][0]["reset_credits"]["available_count"], 0);
        servers.shutdown();
        servers.data_task.await.unwrap().unwrap();
        servers.admin_task.await.unwrap().unwrap();
    }
}
