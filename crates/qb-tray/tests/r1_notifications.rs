//! Product relay -> core event/state -> shared tray formatter, synthetic only.
use chrono::{Duration as ChronoDuration, Utc};
use qb_core::{
    accounts::{AccountStore, import},
    config::Config,
    debug_pin::DebugPins,
    http,
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::RelayState,
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

#[tokio::test]
async fn quota_relay_events_render_account_and_grounded_next_action() {
    for scenario in [
        "failover",
        "credits",
        "exhausted",
        "exhausted-disabled",
        "exhausted-credit-offer",
    ] {
        let action = if scenario.starts_with("exhausted") {
            "exhausted"
        } else {
            scenario
        };
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/tray2-bridge");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let cpa = root.path().join("synthetic-cpa");
        fs::create_dir(&cpa).unwrap();
        let n = if action == "failover" || scenario == "exhausted-disabled" {
            2
        } else {
            1
        };
        let mut activate = Vec::new();
        for i in 0..n {
            let email = format!("notification-{i}@example.invalid");
            fs::write(cpa.join(format!("{i}.json")), serde_json::to_vec(&json!({
                "type":"codex","email":email,"account_id":format!("synthetic-notice-{i}"),
                "access_token":"synthetic-notice-access","refresh_token":"synthetic-notice-refresh",
                "id_token":"synthetic-notice-id","expired":"2099-01-01T00:00:00Z",
                "last_refresh":"2026-01-01T00:00:00Z","plan_type":"plus"
            })).unwrap()).unwrap();
            if scenario != "exhausted-disabled" || i == 0 {
                activate.push(format!("codex:{email}"));
            }
        }
        let store = Arc::new(AccountStore::acquire(&home).unwrap());
        import::run(&store, &cpa, &activate, false).unwrap();
        if action == "credits" {
            let id = store.list().unwrap()[0].id.clone();
            store
                .update_settings(&id, None, Some(true), |a| a.allow_credits = true)
                .unwrap();
        }
        let fake = ScriptedUpstream::start().await.unwrap();
        let reset = Utc::now() + ChronoDuration::days(2);
        let mut limited = ScriptedResponse::json(
            if action == "credits" { StatusCode::OK } else { StatusCode::TOO_MANY_REQUESTS },
            if action == "credits" { json!({"ok":true}) } else {
                json!({"error":{"type":"usage_limit_reached","resets_at":reset.timestamp(),"message":"synthetic-notice-raw-body"}})
            }.to_string());
        for (name, value) in [
            ("x-codex-secondary-used-percent", "100".to_owned()),
            ("x-codex-secondary-window-minutes", "10080".to_owned()),
            ("x-codex-secondary-reset-at", reset.timestamp().to_string()),
            ("x-codex-active-limit", "secondary".to_owned()),
            (
                "x-codex-credits-has-credits",
                (action == "credits" || scenario == "exhausted-credit-offer").to_string(),
            ),
            ("x-codex-credits-serving", (action == "credits").to_string()),
        ] {
            limited.headers.insert(name, value.parse().unwrap());
        }
        fake.state.push(limited);
        if action == "failover" {
            fake.state
                .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        }
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.codex_base = fake.url.clone();
        config.upstream.anthropic_base = fake.url.clone();
        config.upstream.codex_usage_base = fake.url.clone();
        config.upstream.claude_usage_base = fake.url.clone();
        #[cfg(debug_assertions)]
        {
            config.upstream.codex_token_endpoint = Some(fake.url.clone());
            config.upstream.claude_token_endpoint = Some(fake.url.clone());
        }
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let state = Arc::new(
            RelayState::new(store.clone(), &config, obs, Arc::new(DebugPins::default())).unwrap(),
        );
        let mut events = state.accounts.subscribe();
        let mut servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-notice-client".into(),
                admin_token: "synthetic-notice-admin".into(),
            },
            qb_core::relay::router(state.clone()),
            qb_core::admin::runtime_routes(state.clone(), 0),
        )
        .await
        .unwrap();
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
            .post(format!("http://{}/v1/responses", servers.data_addr))
            .bearer_auth("synthetic-notice-client")
            .header("session-id", "synthetic-notice-session")
            .body("{}")
            .send()
            .await
            .unwrap();
        if action != "exhausted" {
            assert!(response.status().is_success());
        }
        let _body = response.bytes().await.unwrap();
        let views = state.accounts.views().await.unwrap();
        let account = views
            .iter()
            .find(|a| a["quota_notice"]["action"] == action)
            .expect("core must publish grounded notice");
        let snapshot: Value = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{}/admin/v1/state", servers.admin_addr))
            .bearer_auth("synthetic-notice-admin")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let projected = qb_tray::v2::project(&snapshot.to_string(), SystemTime::now()).unwrap();
        let id = account["id"].as_str().unwrap();
        let context: qb_tray::QuotaNotice =
            serde_json::from_value(account["quota_notice"].clone()).unwrap();
        let name = qb_tray::v2_notices::account_row(&projected, "codex", id)
            .unwrap()
            .name
            .as_str();
        let next = context
            .next_account
            .as_deref()
            .and_then(|next| qb_tray::v2_notices::account_row(&projected, "codex", next))
            .map(|row| row.name.as_str());
        let message = qb_tray::v2_notices::quota_message(
            &projected,
            "codex",
            id,
            &context,
            SystemTime::now(),
        )
        .unwrap();
        if action == "exhausted" {
            assert_eq!(message.title, "Codex 계정 모두 한도 도달");
        } else {
            assert!(message.title.contains(name));
        }
        match action {
            "failover" => {
                assert!(message.text.contains("이어서 처리"));
                assert!(message.text.contains(next.unwrap()));
                assert_eq!(fake.state.requests().len(), 2);
            }
            "credits" => assert!(message.text.contains("유료 크레딧으로 계속 처리합니다.")),
            "exhausted" => {
                assert!(message.title.contains("모두 한도"));
                assert!(message.text.contains("가장 빠른 초기화"));
                match scenario {
                    "exhausted-disabled" => assert!(
                        message
                            .text
                            .contains("꺼진 계정 1개를 켜면 이어서 처리할 수 있습니다.")
                    ),
                    "exhausted-credit-offer" => {
                        assert!(message.text.contains(&format!(
                            "{name}의 유료 크레딧을 허용하면 바로 이어서 처리합니다."
                        )));
                        assert!(!message.text.contains("유료 크레딧으로 계속 처리합니다."));
                    }
                    _ => assert!(message.text.contains("그때까지 새 요청은 오류로 끝납니다.")),
                }
            }
            _ => unreachable!(),
        }
        let mut received = Vec::new();
        while let Ok(event) = events.try_recv() {
            received.push(event);
        }
        assert!(received.iter().any(|event| event["event"] == "core_warning"
            && event["data"]["kind"] == "quota_notice"
            && event["data"]["action"] == action
            && event["data"]["account"] == account["id"]));
        let text = serde_json::to_string(&received).unwrap() + &message.title + &message.text;
        for secret in [
            "synthetic-notice-access",
            "synthetic-notice-refresh",
            "synthetic-notice-id",
            "synthetic-notice-client",
            "synthetic-notice-admin",
            "synthetic-notice-raw-body",
        ] {
            assert!(
                !text.contains(secret),
                "A5 notification contains synthetic secret/body"
            );
        }
        servers.shutdown();
        tokio::time::timeout(Duration::from_secs(5), async {
            (&mut servers.data_task).await.unwrap().unwrap();
            (&mut servers.admin_task).await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }
}
