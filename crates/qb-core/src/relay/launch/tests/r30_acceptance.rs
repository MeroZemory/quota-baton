//! R30's first acceptance case through authenticated admin and data HTTP.
use super::*;
use crate::accounts::{Identity, Tokens};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;

fn union_account<'a>(value: &'a serde_json::Value, provider: &str) -> &'a serde_json::Value {
    let models = &value["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == provider)
        .unwrap()["models"];
    let model = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "special")
        .unwrap();
    assert_eq!(model["accounts"].as_array().unwrap().len(), 1);
    &model["accounts"][0]
}

#[tokio::test]
async fn r30_two_provider_union_routes_limits_and_recovers_only_entitled_accounts() {
    let f = Fixture::new().await;
    let codex_special = second_issue_account(&f).await;
    let mut claude = Vec::new();
    for n in 0..2 {
        let email = format!("r30-claude-{n}@example.invalid");
        let identity = Identity::Claude {
            account_uuid: format!("r30-claude-{n}"),
            organization_uuid: None,
            email: email.clone(),
        };
        let account: Account = serde_json::from_value(json!({
            "schema":1,"id":identity.id(),"provider":"claude","label":email,
            "email":email,"identity":identity,"source":{"kind":"oauth"},
            "plan":"max","user_state":"active","health":{"kind":"ready"},"generation":0,
            "token_meta":{"expires_at":"2099-01-01T00:00:00Z","refresh_uncertain":false}
        }))
        .unwrap();
        f.relay
            .store
            .save(
                &account,
                &Tokens {
                    access_token: format!("synthetic-r30-claude-{n}"),
                    refresh_token: format!("synthetic-r30-refresh-{n}"),
                    id_token: None,
                },
                None,
            )
            .unwrap();
        claude.push(account.id);
    }
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    config.upstream.codex_base = f.upstream.url.clone();
    config.upstream.anthropic_base = f.upstream.url.clone();
    config.upstream.claude_usage_base = f.upstream.url.clone();
    config.upstream.codex_usage_base = f.upstream.url.clone();
    let relay = Arc::new(
        RelayState::new(
            f.relay.store.clone(),
            &config,
            f.relay.observability.clone(),
            Arc::new(DebugPins::default()),
        )
        .unwrap(),
    );
    for (id, special, provider) in [
        (&f.id, false, Provider::Codex),
        (&codex_special, true, Provider::Codex),
        (&claude[0], false, Provider::Claude),
        (&claude[1], true, Provider::Claude),
    ] {
        let model = if special { "special" } else { "common" };
        f.reply(
            StatusCode::OK,
            match provider {
                Provider::Codex => json!({"models":[{"slug":model}]}),
                Provider::Claude => json!({"data":[{"id":model,"display_name":model}]}),
            },
        );
        relay.launch_catalog(id, true, false).await.unwrap();
    }
    let mut servers = crate::http::spawn_with_routes(
        &config,
        crate::local_secrets::LocalSecrets {
            client_key: "synthetic-r30-client".into(),
            admin_token: "synthetic-r30-admin".into(),
        },
        crate::relay::router(relay.clone()),
        crate::admin::runtime_routes(relay.clone(), 10200),
    )
    .await
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let admin = format!("http://{}", servers.admin_addr);
    let data = format!("http://{}", servers.data_addr);
    let exercised = AssertUnwindSafe(async {
        // The authenticated admin union names exactly the second account of each provider.
        let union: serde_json::Value = client.get(format!("{admin}/admin/v1/models"))
            .bearer_auth("synthetic-r30-admin").send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        for (provider, id) in [("claude", &claude[1]), ("codex", &codex_special)] {
            assert_eq!(union_account(&union, provider)["id"], id.as_str());
            assert_eq!(union_account(&union, provider)["usable_now"], true);
        }
        let catalog_calls = f.upstream.state.requests();
        for (index, id) in [(2, "synthetic-r30-claude-0"), (3, "synthetic-r30-claude-1")] {
            assert_eq!(catalog_calls[index].path_and_query, "/v1/models");
            assert_eq!(catalog_calls[index].headers["authorization"], format!("Bearer {id}"));
            assert_eq!(catalog_calls[index].headers["anthropic-beta"], "oauth-2025-04-20");
            assert_eq!(catalog_calls[index].headers["anthropic-version"], "2023-06-01");
            assert!(!catalog_calls[index].headers.contains_key("chatgpt-account-id"));
        }
        for phase in 0..3 {
            let now = chrono::Utc::now();
            let reset = now + chrono::Duration::hours(4);
            if phase == 1 {
                // An earlier recovery on unentitled accounts must not affect the answer.
                for id in [&f.id, &claude[0]] {
                    relay.store.update_metadata(id, |a| quota_hold(a, None, now + chrono::Duration::minutes(1))).unwrap();
                }
                for id in [&codex_special, &claude[1]] {
                    relay.store.update_metadata(id, |a| quota_hold(a, Some("special"), reset)).unwrap();
                }
                // Native before_unavailable polls each Claude account and then
                // its permission profile, even after usage fails. These are reads.
                for _ in 0..4 {
                    f.reply(StatusCode::SERVICE_UNAVAILABLE, json!({"error":"synthetic usage unavailable"}));
                }
            } else if phase == 2 {
                for id in [&codex_special, &claude[1]] {
                    relay.store.update_metadata(id, |a| {
                        a.health = Health::Ready; a.quota_cooling = None;
                    }).unwrap();
                }
            }
            if phase != 1 {
                f.reply(StatusCode::OK, json!({"type":"message","id":"synthetic-r30-message",
                    "model":"special","role":"assistant","content":[],"stop_reason":"end_turn",
                    "usage":{"input_tokens":1,"output_tokens":1}}));
            }
            let before = f.upstream.state.requests().len();
            let response = client.post(format!("{data}/v1/messages"))
                .bearer_auth("synthetic-r30-client").header("anthropic-version", "2023-06-01")
                .json(&json!({"model":"special","max_tokens":1,"messages":[{"role":"user","content":"synthetic"}]}))
                .send().await.unwrap();
            if phase == 1 {
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
                let retry: u64 = response.headers()["retry-after"].to_str().unwrap().parse().unwrap();
                assert!((14380..=14400).contains(&retry));
                let _ = response.bytes().await.unwrap();
                let reads = f.upstream.state.requests();
                assert_eq!(reads[before..].len(), 4);
                assert_eq!(reads[before..].iter().filter(|r| r.path_and_query == "/api/oauth/usage").count(), 2);
                assert_eq!(reads[before..].iter().filter(|r| r.path_and_query == "/api/oauth/profile").count(), 2);
                for read in &reads[before..] {
                    assert_eq!(read.method, axum::http::Method::GET);
                    assert!(matches!(read.path_and_query.as_str(),
                        "/api/oauth/usage" | "/api/oauth/profile"));
                }
                relay.notice_quota(&claude[1], None, Some("special")).await;
                let state: serde_json::Value = client.get(format!("{admin}/admin/v1/state"))
                    .bearer_auth("synthetic-r30-admin").send().await.unwrap().json().await.unwrap();
                let account = state["accounts"].as_array().unwrap().iter().find(|a| a["id"] == claude[1]).unwrap();
                assert_eq!(account["quota_notice"]["earliest_account"], claude[1]);
                assert_eq!(account["quota_notice"]["reset_at"], json!(reset));
                assert_eq!(account["quota_notice"]["model_limited"], true);
            } else {
                assert_eq!(response.status(), StatusCode::OK);
                let _ = response.bytes().await.unwrap();
                let requests = f.upstream.state.requests();
                assert_eq!(requests[before].path_and_query, "/v1/messages");
                assert_eq!(requests[before].headers["authorization"], "Bearer synthetic-r30-claude-1");
            }
            let launch = client.post(format!("{admin}/admin/v1/launches"))
                .bearer_auth("synthetic-r30-admin").json(&json!({"model":"special"})).send().await.unwrap();
            if phase == 1 {
                assert_eq!(launch.status(), StatusCode::SERVICE_UNAVAILABLE);
                let failure: serde_json::Value = launch.json().await.unwrap();
                assert_eq!(failure["error"]["code"], "launch_unavailable");
                assert_eq!(failure["error"]["reason"], "model_limited");
                assert_eq!(failure["error"]["earliest_account"], codex_special);
                assert_eq!(failure["error"]["reset_at"], json!(reset));
                assert!((14380..=14400).contains(&failure["error"]["retry_after"].as_u64().unwrap()));
                let union: serde_json::Value = client.get(format!("{admin}/admin/v1/models"))
                    .bearer_auth("synthetic-r30-admin").send().await.unwrap().json().await.unwrap();
                for provider in ["claude", "codex"] {
                    assert_eq!(union_account(&union, provider)["usable_now"], false);
                    assert_eq!(union_account(&union, provider)["limited_until"], json!(reset));
                }
            } else {
                assert_eq!(launch.status(), StatusCode::OK);
                let launched: serde_json::Value = launch.json().await.unwrap();
                assert_eq!(launched["account"], codex_special);
                assert_eq!(launched["model"], "special");
            }
        }
    }).catch_unwind().await;
    servers.shutdown();
    (&mut servers.data_task).await.unwrap().unwrap();
    (&mut servers.admin_task).await.unwrap().unwrap();
    if let Err(panic) = exercised {
        std::panic::resume_unwind(panic);
    }
}

// Distinct-risk cases share the existing account/upstream fixture. No scheduler
// is started: every provider reply and catalog observation is test-controlled.
struct RiskHttp {
    f: Fixture,
    relay: Arc<RelayState>,
    servers: crate::http::RunningServers,
    client: reqwest::Client,
    ids: Vec<String>,
}
impl RiskHttp {
    async fn new(provider: Provider) -> Self {
        let f = Fixture::new().await;
        let ids = if provider == Provider::Codex {
            vec![f.id.clone(), second_issue_account(&f).await]
        } else {
            let mut ids = Vec::new();
            for n in 0..2 {
                let email = format!("r30-risk-{n}@example.invalid");
                let identity = Identity::Claude {
                    account_uuid: format!("r30-risk-{n}"),
                    organization_uuid: None,
                    email: email.clone(),
                };
                let account: Account = serde_json::from_value(json!({
                    "schema":1,"id":identity.id(),"provider":"claude","label":email,"email":email,
                    "identity":identity,"source":{"kind":"oauth"},"plan":"max",
                    "user_state":"active","health":{"kind":"ready"},"generation":0,
                    "token_meta":{"expires_at":"2099-01-01T00:00:00Z","refresh_uncertain":false}
                }))
                .unwrap();
                f.relay
                    .store
                    .save(
                        &account,
                        &Tokens {
                            access_token: format!("synthetic-risk-{n}"),
                            refresh_token: format!("synthetic-risk-refresh-{n}"),
                            id_token: None,
                        },
                        None,
                    )
                    .unwrap();
                ids.push(account.id);
            }
            ids
        };
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.routing.weak_evidence_new_sessions = 100;
        config.upstream.codex_base = f.upstream.url.clone();
        config.upstream.anthropic_base = f.upstream.url.clone();
        config.upstream.claude_usage_base = f.upstream.url.clone();
        config.upstream.codex_usage_base = f.upstream.url.clone();
        let relay = Arc::new(
            RelayState::new(
                f.relay.store.clone(),
                &config,
                f.relay.observability.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap(),
        );
        let servers = crate::http::spawn_with_routes(
            &config,
            crate::local_secrets::LocalSecrets {
                client_key: "synthetic-risk-client".into(),
                admin_token: "synthetic-risk-admin".into(),
            },
            crate::relay::router(relay.clone()),
            crate::admin::runtime_routes(relay.clone(), 10200),
        )
        .await
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        Self {
            f,
            relay,
            servers,
            client,
            ids,
        }
    }
    async fn catalog(&self, index: usize, provider: Provider, models: &[&str]) {
        self.f.reply(
            StatusCode::OK,
            match provider {
                Provider::Codex => {
                    json!({"models":models.iter().map(|m|json!({"slug":m})).collect::<Vec<_>>()})
                }
                Provider::Claude => {
                    json!({"data":models.iter().map(|m|json!({"id":m})).collect::<Vec<_>>()})
                }
            },
        );
        self.relay
            .launch_catalog(&self.ids[index], true, false)
            .await
            .unwrap();
    }
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("http://{}{path}", self.servers.data_addr))
            .bearer_auth("synthetic-risk-client")
    }
    async fn claude(&self, model: &str, session: &str) -> reqwest::Response {
        self.post("/v1/messages").header("anthropic-version","2023-06-01")
            .header("x-claude-code-session-id",session)
            .json(&json!({"model":model,"max_tokens":1,"messages":[{"role":"user","content":"synthetic"}]}))
            .send().await.unwrap()
    }
    fn success(&self, model: &str) {
        self.f.reply(StatusCode::OK,json!({"type":"message","id":"synthetic-risk-message","role":"assistant",
            "model":model,"content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}));
    }
    async fn finish(mut self, result: Result<(), Box<dyn std::any::Any + Send>>) {
        self.servers.shutdown();
        (&mut self.servers.data_task).await.unwrap().unwrap();
        (&mut self.servers.admin_task).await.unwrap().unwrap();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

#[tokio::test]
async fn r30_actual_claude_unknown_startup_and_nowhere_model_pass_through() {
    let h = RiskHttp::new(Provider::Claude).await;
    let result = AssertUnwindSafe(async {
        assert!(h.ids.iter().all(|id| h.relay.catalogs.view(id).unwrap().observation.last_known().is_none()));
        h.success("brand-new");
        let response = h.claude("brand-new","r30-startup").await;
        assert_eq!(response.status(),StatusCode::OK);
        let _ = response.bytes().await.unwrap();
        assert_eq!(h.f.upstream.state.requests().len(),1);
        for n in 0..2 { h.catalog(n,Provider::Claude,&["known"]).await; }
        // Complete catalogs contain no such model. Preserve the upstream answer,
        // rather than manufacturing model_not_supported locally.
        let upstream = json!({"error":{"type":"invalid_request_error","message":"synthetic unchanged answer"}});
        h.f.reply(StatusCode::BAD_REQUEST,upstream.clone());
        let before = h.f.upstream.state.requests().len();
        let response = h.claude("not-in-any-catalog","r30-nowhere").await;
        assert_eq!(response.status(),StatusCode::BAD_REQUEST);
        assert_eq!(response.json::<serde_json::Value>().await.unwrap(),upstream);
        let calls = h.f.upstream.state.requests();
        assert_eq!(calls.len(),before+1);
        assert_eq!(calls[before].path_and_query,"/v1/messages");
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&calls[before].body).unwrap()["model"],"not-in-any-catalog");
    }).catch_unwind().await;
    h.finish(result).await;
}

#[tokio::test]
async fn r30_actual_managed_model_change_moves_to_entitled_account_without_rewriting_body() {
    let h = RiskHttp::new(Provider::Codex).await;
    let result = AssertUnwindSafe(async {
        h.catalog(0,Provider::Codex,&["common"]).await;
        h.catalog(1,Provider::Codex,&["special"]).await;
        let issued = h.relay.issue_launch(issue_request(Some("common")),h.servers.data_addr.port()).await.unwrap();
        assert_eq!(issued.account,h.ids[0]);
        let path = format!("/l/{}/v1/responses",issued_handle(&issued));
        let first = br#"{"model":"common","input":"synthetic initial context"}"#;
        h.f.reply(StatusCode::OK,json!({"id":"synthetic-response-a","object":"response","output":[],"status":"completed"}));
        let response = h.post(&path).header("session-id","r30-model-change").header("content-type","application/json")
            .body(first.as_slice()).send().await.unwrap();
        assert_eq!(response.status(),StatusCode::OK);
        let _ = response.bytes().await.unwrap();
        assert_eq!(h.relay.bindings.current(Some("codex:r30-model-change")),Some(h.ids[0].clone()));
        assert!(h.relay.store.load(&h.ids[0]).unwrap().eligible(Some("common")));
        let body = br#"{ "model" : "special", "input" : "synthetic retained context" }"#;
        // Existing admission refreshes a missing bound-model pick once, then
        // selects the known destination under the unmodified D20 move guards.
        h.f.reply(StatusCode::OK,json!({"models":[{"slug":"common"}]}));
        h.f.reply(StatusCode::OK,json!({"id":"synthetic-response-b","object":"response","output":[],"status":"completed"}));
        let before = h.f.upstream.state.requests().len();
        let response = h.post(&path).header("session-id","r30-model-change").header("content-type","application/json")
            .body(body.as_slice()).send().await.unwrap();
        assert_eq!(response.status(),StatusCode::OK);
        let _ = response.bytes().await.unwrap();
        let calls = h.f.upstream.state.requests();
        let data: Vec<_> = calls[before..].iter().filter(|r|r.path_and_query=="/responses").collect();
        assert_eq!(data.len(),1);
        assert_eq!(data[0].headers["chatgpt-account-id"],"second-account");
        assert_eq!(data[0].body.as_ref(),body.as_slice());
        assert_eq!(h.relay.bindings.current(Some("codex:r30-model-change")),Some(h.ids[1].clone()));
    }).catch_unwind().await;
    h.finish(result).await;
}

#[tokio::test]
async fn r30_actual_claude_model_denial_replays_once_and_survives_failed_refresh() {
    let h = RiskHttp::new(Provider::Claude).await;
    let result = AssertUnwindSafe(async {
        for n in 0..2 {
            h.catalog(n, Provider::Claude, &["special"]).await;
        }
        h.f.reply(
            StatusCode::FORBIDDEN,
            json!({"error":{"type":"permission_error","code":"model_not_available",
            "message":"model is not available for this account"}}),
        );
        h.success("special");
        let before = h.f.upstream.state.requests().len();
        let response = h.claude("special", "r30-denial").await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.bytes().await.unwrap();
        let calls = h.f.upstream.state.requests();
        assert_eq!(
            calls.len(),
            before + 2,
            "one replay and no denial-clearing catalog fetch"
        );
        assert_eq!(calls[before].path_and_query, "/v1/messages");
        assert_eq!(calls[before + 1].path_and_query, "/v1/messages");
        assert_ne!(
            calls[before].headers["authorization"],
            calls[before + 1].headers["authorization"]
        );
        assert_eq!(calls[before].body, calls[before + 1].body);
        let rejected = if calls[before].headers["authorization"] == "Bearer synthetic-risk-0" {
            0
        } else {
            1
        };
        assert!(
            h.relay
                .catalogs
                .is_model_denied(&h.ids[rejected], "special")
                .unwrap()
        );
        h.success("special");
        let next = h.f.upstream.state.requests().len();
        let response = h.claude("special", "r30-after-denial").await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.bytes().await.unwrap();
        let calls = h.f.upstream.state.requests();
        assert_eq!(calls.len(), next + 1);
        assert_eq!(
            calls[next].headers["authorization"],
            calls[before + 1].headers["authorization"]
        );
        h.f.reply(
            StatusCode::BAD_GATEWAY,
            json!({"error":"synthetic catalog failure"}),
        );
        let _ = h.relay.launch_catalog(&h.ids[rejected], true, true).await;
        assert!(
            h.relay
                .catalogs
                .is_model_denied(&h.ids[rejected], "special")
                .unwrap()
        );
        h.catalog(rejected, Provider::Claude, &["special"]).await;
        assert!(
            !h.relay
                .catalogs
                .is_model_denied(&h.ids[rejected], "special")
                .unwrap()
        );
    })
    .catch_unwind()
    .await;
    h.finish(result).await;
}

#[tokio::test]
async fn r30_actual_unmanaged_codex_compact_uses_requested_model_entitlement() {
    let h = RiskHttp::new(Provider::Codex).await;
    let result = AssertUnwindSafe(async {
        h.catalog(0, Provider::Codex, &["common"]).await;
        h.catalog(1, Provider::Codex, &["special"]).await;
        let body = br#"{ "model":"special", "input":[] }"#;
        h.f.reply(
            StatusCode::OK,
            json!({"object":"response.compaction","output":[]}),
        );
        let before = h.f.upstream.state.requests().len();
        let response = h
            .post("/v1/responses/compact")
            .header("session-id", "r30-unmanaged-compact")
            .header("content-type", "application/json")
            .body(body.as_slice())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.bytes().await.unwrap();
        let calls = h.f.upstream.state.requests();
        assert_eq!(calls.len(), before + 1);
        assert_eq!(calls[before].path_and_query, "/responses/compact");
        assert_eq!(
            calls[before].headers["chatgpt-account-id"],
            "second-account"
        );
        assert_eq!(calls[before].body.as_ref(), body.as_slice());
        assert_eq!(
            h.relay
                .bindings
                .current(Some("codex:r30-unmanaged-compact")),
            Some(h.ids[1].clone())
        );
    })
    .catch_unwind()
    .await;
    h.finish(result).await;
}
