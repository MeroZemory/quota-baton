//! Actual admin route bytes consumed by the real tray transport.
//! Requires the frozen manual-refresh / refresh-usage / action-consumer candidates.
#![cfg(debug_assertions)]

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::OriginalUri,
    http::StatusCode,
    middleware,
    response::Response,
};
use chrono::{Duration as ChronoDuration, Utc};
use futures_util::FutureExt;
use qb_core::{
    accounts::{AccountStore, Health, HealthReason, Provider, Source, UserState, import},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::RelayState,
};
use qb_proto::admin_refresh::{ManualRefreshReply, RefreshOutcome};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use qb_tray::client::{
    AccountAction, AccountActionResponse, AccountActionWire, AccountPatch, AdminClient,
    ClientError, ReleaseHoldWire,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};

const ADMIN: &str = "r15-actions-synthetic-admin";
const LIMIT: Duration = Duration::from_secs(10);
const BODY_CAP: usize = 1024 * 1024;
type Captures = Arc<Mutex<Vec<(String, StatusCode, Vec<u8>)>>>;

struct Harness {
    servers: RunningServers,
    relay: Arc<RelayState>,
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    usage: ScriptedUpstream,
    token: ScriptedUpstream,
    data: ScriptedUpstream,
    root: tempfile::TempDir,
    captured: Captures,
    id: String,
}

impl Harness {
    async fn new() -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.sandbox/r15-account-actions-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
        let cpa = root.path().join("synthetic-cpa");
        fs::create_dir(&cpa).unwrap();
        fs::write(cpa.join("fixture.json"), serde_json::to_vec(&json!({
            "type":"codex", "email":"r15-actions@example.invalid", "account_id":"r15-actions",
            "access_token":"synthetic-actions-access", "refresh_token":"synthetic-actions-refresh",
            "expired":"2099-01-01T00:00:00Z", "last_refresh":"2026-01-01T00:00:00Z"
        })).unwrap()).unwrap();
        let store = Arc::new(AccountStore::acquire(&root.path().join("home")).unwrap());
        import::run(
            &store,
            &cpa,
            &["codex:r15-actions@example.invalid".into()],
            false,
        )
        .unwrap();
        let mut accounts = store.list().unwrap();
        assert_eq!(accounts.len(), 1);
        let mut account = accounts.remove(0);
        // Follow the existing m5 owned fixture: save tokens with exact generation.
        let tokens = store.tokens(&account).unwrap();
        account.source = Source::Oauth;
        let account = store
            .save(&account, &tokens, Some(account.generation))
            .unwrap();
        let id = account.id;
        let usage = ScriptedUpstream::start().await.unwrap();
        let token = ScriptedUpstream::start().await.unwrap();
        let data = ScriptedUpstream::start().await.unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.codex_base = data.url.clone();
        config.upstream.anthropic_base = data.url.clone();
        config.upstream.codex_usage_base = usage.url.clone();
        config.upstream.claude_usage_base = usage.url.clone();
        config.upstream.codex_token_endpoint = Some(token.url.clone());
        config.upstream.claude_token_endpoint = Some(token.url.clone());
        config.quota.poll_min_interval_secs = 300;
        config.dashboard.resolve_claude_project = false;
        let obs = Arc::new(Observability::new(&root.path().join("home"), None).unwrap());
        let relay = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap()
            .with_token_endpoint(Provider::Codex, token.url.clone()),
        );
        // No background scheduler, profile lookup or native process is started.
        let captured: Captures = Arc::new(Mutex::new(Vec::new()));
        let capture = captured.clone();
        let routes = qb_core::admin::runtime_routes(relay.clone(), 0).route_layer(
            middleware::map_response(move |OriginalUri(uri): OriginalUri, response: Response| {
                let capture = capture.clone();
                async move {
                    let (parts, body) = response.into_parts();
                    let bytes = to_bytes(body, BODY_CAP)
                        .await
                        .expect("bounded admin action body");
                    let mut rows = capture.lock().unwrap();
                    assert!(rows.len() < 32, "bounded response capture");
                    rows.push((uri.path().to_owned(), parts.status, bytes.to_vec()));
                    drop(rows);
                    Response::from_parts(parts, Body::from(bytes))
                }
            }),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "r15-actions-synthetic-client".into(),
                admin_token: ADMIN.into(),
            },
            Router::new(),
            routes,
        )
        .await
        .unwrap();
        for port in [servers.admin_addr.port(), servers.data_addr.port()] {
            assert_ne!(port, 0);
            assert!(!matches!(port, 10100 | 10101));
        }
        Self {
            servers,
            relay,
            store,
            obs,
            usage,
            token,
            data,
            root,
            captured,
            id,
        }
    }

    fn client(&self) -> AdminClient {
        AdminClient::new(self.servers.admin_addr.port(), ADMIN).unwrap()
    }

    fn body(&self, suffix: &str, status: StatusCode) -> Vec<u8> {
        let rows = std::mem::take(&mut *self.captured.lock().unwrap());
        assert_eq!(rows.len(), 1, "capture exactly the consumed response");
        let (path, actual, body) = rows.into_iter().next().unwrap();
        assert_eq!(path, format!("/admin/v1/accounts/{}{suffix}", self.id));
        assert_eq!(actual, status);
        // Fail without printing any body/token contents.
        let text = std::str::from_utf8(&body).unwrap();
        for secret in [
            "synthetic-actions-access",
            "synthetic-actions-refresh",
            "synthetic-secret",
        ] {
            assert!(
                !text.contains(secret),
                "credential/provider detail escaped into admin body"
            );
        }
        body
    }

    fn same_body<T: Serialize>(&self, suffix: &str, value: &T) {
        let body = self.body(suffix, StatusCode::OK);
        assert!(
            serde_json::to_vec(value).unwrap() == body,
            "real consumed reply changed bytes"
        );
    }

    fn presentation_body<T: DeserializeOwned + Serialize>(&self, suffix: &str) -> T {
        let body = self.body(suffix, StatusCode::OK);
        let typed: T = serde_json::from_slice(&body).unwrap();
        assert!(
            serde_json::to_vec(&typed).unwrap() == body,
            "presentation route changed wire bytes"
        );
        typed
    }

    async fn close(self) -> Result<(), String> {
        let Self {
            mut servers,
            relay,
            store,
            obs,
            usage,
            token,
            data,
            root,
            captured,
            id,
        } = self;
        let mut errors = Vec::new();
        servers.shutdown();
        for task in [&mut servers.data_task, &mut servers.admin_task] {
            match tokio::time::timeout(LIMIT, &mut *task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(_) => errors.push("server task failed"),
                Err(_) => {
                    errors.push("server shutdown timed out");
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        match tokio::time::timeout(LIMIT, relay.shutdown_refreshes()).await {
            Ok(Ok(())) => {}
            _ => errors.push("refresh owner drain failed"),
        }
        drop(servers);
        let root = root.keep();
        std::thread::spawn(move || {
            drop(relay);
            drop((usage, token, data, captured, id));
            if Arc::strong_count(&store) != 1 {
                errors.push("store owners remain");
            }
            // Store's AccountRuntime observer owns an obs clone.
            drop(store);
            if Arc::strong_count(&obs) != 1 {
                errors.push("observability owners remain after store drop");
            }
            if obs.flush().is_err() {
                errors.push("observability flush failed");
            }
            drop(obs); // Outside Tokio: synchronously stop/join writer.
            if !errors.is_empty() {
                return Err(format!(
                    "{}; retained {}",
                    errors.join("; "),
                    root.display()
                ));
            }
            fs::remove_dir_all(&root).map_err(|_| "synthetic home removal failed".to_owned())?;
            if root.exists() {
                return Err("synthetic home remains".into());
            }
            Ok(())
        })
        .join()
        .unwrap_or_else(|_| Err("cleanup thread panicked; home retained".into()))
    }
}

fn patch(label: &str) -> AccountPatch {
    AccountPatch {
        label: Some(label.into()),
        ..AccountPatch::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_account_actions_same_body_reaches_real_client() {
    let h = Harness::new().await;
    let body = std::panic::AssertUnwindSafe(async {
        let client = h.client();
        let denied = AdminClient::new(h.servers.admin_addr.port(), "synthetic-wrong-admin").unwrap();
        assert!(matches!(denied.patch_account_raw(&h.id, patch("denied")).await,
            Err(ClientError::Http(StatusCode::UNAUTHORIZED))));
        // Auth rejection happens outside route-layer middleware.
        assert!(h.captured.lock().unwrap().is_empty());
        drop(denied);
        let raw = client.patch_account_raw(&h.id, patch("raw label")).await.unwrap();
        h.same_body("", &raw);
        assert_eq!(raw.label, "raw label");
        assert_eq!(h.store.load(&h.id).unwrap().label, "raw label");
        let display = client.patch_account(&h.id, patch("display label")).await.unwrap();
        let raw: AccountActionWire = h.presentation_body("");
        assert_eq!(display.label.as_deref(), Some(raw.label.as_str()));
        assert_eq!(display.id, raw.id);
        for (action, expected) in [
            (AccountAction::Pause, "paused"), (AccountAction::Resume, "active"),
            (AccountAction::Disable, "disabled"), (AccountAction::Enable, "active"),
        ] {
            let result = client.account_action_raw(&h.id, action).await.unwrap();
            let AccountActionResponse::Changed(raw) = result else { panic!("wrong action graph"); };
            h.same_body(&format!("/{}", action.as_str()), &raw);
            assert_eq!(serde_json::to_value(raw.user_state).unwrap(), expected);
            let display = client.account_action(&h.id, action).await.unwrap();
            let raw: AccountActionWire = h.presentation_body(&format!("/{}", action.as_str()));
            assert_eq!(display.id, raw.id);
            assert_eq!(display.user_state, expected);
            assert_eq!(serde_json::to_value(h.store.load(&h.id).unwrap().user_state).unwrap(), expected);
        }
        for presentation in [false, true] {
            h.store.update_metadata(&h.id, |a| {
                a.health = Health::Cooling { reason: HealthReason::Quota,
                    until: Utc::now() + ChronoDuration::hours(1), scope: "account".into() };
            }).unwrap();
            if presentation {
                let display = client.account_action(&h.id, AccountAction::ReleaseHold).await.unwrap();
                let raw: ReleaseHoldWire = h.presentation_body("/release-hold");
                assert!(raw.released);
                assert_eq!(display.health["kind"], "ready");
            } else {
                let result = client.account_action_raw(&h.id, AccountAction::ReleaseHold).await.unwrap();
                let AccountActionResponse::Released(raw) = result else { panic!("wrong release graph"); };
                h.same_body("/release-hold", &raw);
                assert!(raw.released);
            }
            assert!(matches!(h.store.load(&h.id).unwrap().health, Health::Ready));
        }
        h.usage.state.push(ScriptedResponse::json(StatusCode::OK, json!({
            "rate_limit":{"limit_reached":false,"primary_window":{"used_percent":25.0,
                "limit_window_seconds":18000,"reset_at":Utc::now().timestamp()+3600}},
            "plan_type":"plus", "credits":{"has_credits":true,"unlimited":false,"balance":10}
        }).to_string()));
        let refreshed = client.account_action_raw(&h.id, AccountAction::RefreshUsage).await.unwrap();
        let AccountActionResponse::Usage(raw) = refreshed else { panic!("wrong usage graph"); };
        h.same_body("/refresh-usage", &raw);
        assert_eq!(raw.id, h.id);
        assert_eq!(raw.plan.as_deref(), Some("plus"));
        assert!(raw.windows.iter().any(|w| w.used_pct == Some(25.0)));
        assert_eq!(h.usage.state.requests().len(), 1);
        // Immediate second poll is a real core 429, not a fabricated error body.
        assert!(matches!(client.account_action_raw(&h.id, AccountAction::RefreshUsage).await,
            Err(ClientError::Http(StatusCode::TOO_MANY_REQUESTS))));
        let body = h.body("/refresh-usage", StatusCode::TOO_MANY_REQUESTS);
        let refused: qb_proto::admin_account_error::AccountErrorReply = serde_json::from_slice(&body).unwrap();
        assert!(serde_json::to_vec(&refused).unwrap() == body);
        assert_eq!(refused.error.code.as_deref(), Some("poll_too_soon"));
        assert!(refused.error.retry_after.is_some());
        assert_eq!(h.usage.state.requests().len(), 1);
        // Manual refresh is allowed for disabled owned accounts; exact existing m5 setup.
        h.store.update_metadata(&h.id, |a| a.user_state = UserState::Disabled).unwrap();
        h.token.state.push(ScriptedResponse::json(StatusCode::OK,
            r#"{"access_token":"synthetic-actions-access-new","refresh_token":"synthetic-actions-refresh-new","expires_in":3600}"#));
        let refreshed = client.refresh_tokens_raw(&h.id).await.unwrap();
        h.same_body("/refresh", &refreshed);
        assert_eq!(refreshed.outcome, RefreshOutcome::Success);
        assert!(refreshed.token_expires_at.is_some());
        assert!(refreshed.last_refresh_at.is_some());
        let account = h.store.load(&h.id).unwrap();
        assert_eq!(refreshed.token_expires_at, account.token_meta.expires_at);
        assert_eq!(refreshed.last_refresh_at, account.token_meta.last_refresh_at);
        assert_eq!(account.user_state, UserState::Disabled);
        h.token.state.push(ScriptedResponse::json(StatusCode::BAD_REQUEST,
            r#"{"error":"refresh_token_reused","error_description":"synthetic-secret"}"#));
        assert!(matches!(client.refresh_tokens(&h.id).await, Err(ClientError::AccountAction("계정 메뉴에서 다시 로그인하세요"))));
        let terminal: ManualRefreshReply = h.presentation_body("/refresh");
        assert_eq!(terminal.outcome, RefreshOutcome::RefreshTokenReused);
        assert!(matches!(h.store.load(&h.id).unwrap().health, Health::NeedsRelogin { .. }));
        assert_eq!(h.token.state.requests().len(), 2);
        assert!(matches!(client.refresh_tokens_raw(&h.id).await, Err(ClientError::AccountAction("계정 메뉴에서 다시 로그인하세요"))));
        let body = h.body("/refresh", StatusCode::CONFLICT);
        let refused: qb_proto::admin_account_error::AccountErrorReply = serde_json::from_slice(&body).unwrap();
        assert!(serde_json::to_vec(&refused).unwrap() == body);
        assert_eq!(refused.error.code.as_deref(), Some("needs_relogin"));
        assert_eq!(h.token.state.requests().len(), 2);
        assert!(h.data.state.requests().is_empty());
        assert!(h.captured.lock().unwrap().is_empty());
        drop(client);
    }).catch_unwind().await;
    let cleanup = h.close().await;
    if let Err(panic) = body {
        if let Err(error) = cleanup {
            eprintln!("{error}");
        }
        std::panic::resume_unwind(panic);
    }
    cleanup.unwrap_or_else(|error| panic!("{error}"));
}
