#![cfg(all(feature = "testkit", debug_assertions))]

//! Synthetic OAuth completion at a retained adopted ID. No real provider/browser.
pub use qb_core::{account_runtime, accounts, observability};
#[path = "support/unscoped_tasks.rs"]
mod boxes;
#[path = "../src/relay/credentials.rs"]
#[allow(dead_code)]
mod credentials;
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::{
    account_runtime::AccountRuntime,
    accounts::{Account, AccountStore, Health, HealthReason, Identity, Provider, Source, Tokens},
    oauth::{LoginService, LoginState, LoginStatus, LoginTestConfig},
    observability::Observability,
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::json;
use std::{fs, path::Path, sync::Arc, time::Duration};

fn id_token() -> String {
    format!(
        "e30.{}.synthetic",
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "email":"new@example.invalid",
                "exp":chrono::Utc::now().timestamp() + 7200,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id":"r6-workspace", "chatgpt_user_id":"r6-user"
                }
            }))
            .unwrap()
        )
    )
}

async fn complete(service: &LoginService, client: &reqwest::Client) -> LoginStatus {
    let start = service.start(Provider::Codex, false).await.unwrap();
    let auth = reqwest::Url::parse(&start.auth_url).unwrap();
    let pairs = auth
        .query_pairs()
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut callback = reqwest::Url::parse(pairs["redirect_uri"].as_ref()).unwrap();
    callback.query_pairs_mut().extend_pairs([
        ("code", "synthetic-r6-code"),
        ("state", pairs["state"].as_ref()),
    ]);
    client.get(callback).send().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = service.status(&start.login_id).await.unwrap();
            if status.status != LoginState::Pending {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn adopted_legacy_oauth_completion_clears_original_runtime_id_only_after_success() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r6-relogin-fixtures");
    fs::create_dir_all(&base).unwrap();
    let home = tempfile::tempdir_in(base).unwrap();
    let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
    let identity = Identity::Codex {
        account_id: "r6-workspace".into(),
        user_id: None,
        email: "old@example.invalid".into(),
    };
    let account: Account = serde_json::from_value(json!({
        "schema":1,"id":identity.id(),"identity":identity,"provider":"codex",
        "email":"old@example.invalid","label":"keep label",
        "source":{"kind":"cpa","mode":"adopted","dir":home.path().join("missing-cpa"),
            "match":{"account_id":"r6-workspace","email":"old@example.invalid"}},
        "user_state":"paused","drain":true,"allow_credits":true,
        "health":{"kind":"needs_relogin","reason":"invalid_grant","since":"2026-01-01T00:00:00Z"},
        "auth_failures":3,"auth_probe_pending":true,"payment_failures":2,
        "payment_until":"2099-01-01T00:00:00Z","generation":0,
        "token_meta":{"last_refresh_at":"2026-01-01T00:00:00Z",
            "expires_at":"2026-01-02T00:00:00Z","refresh_uncertain":false}
    }))
    .unwrap();
    let old_tokens = Tokens {
        access_token: "synthetic-old-access".into(),
        refresh_token: "synthetic-old-refresh".into(),
        id_token: Some(id_token()),
    };
    let old = store.save(&account, &old_tokens, None).unwrap();
    let before = fs::read(
        home.path()
            .join("accounts")
            .join(format!("{}.json", old.id)),
    )
    .unwrap();
    let obs = Arc::new(Observability::new(home.path(), None).unwrap());
    obs.record_login(
        Provider::Codex,
        "synthetic-prior-login",
        "completed",
        Some(&old.id),
    );
    obs.flush().unwrap();
    let runtime = AccountRuntime::new(store.clone(), obs.clone(), 1.0);
    runtime.set_refresh_attention(&old.id, true);
    let mut volatile = old.clone();
    volatile.health = Health::Cooling {
        reason: HealthReason::StoreError,
        until: chrono::Utc::now() + chrono::Duration::minutes(10),
        scope: "account".into(),
    };
    runtime.emit_account(&volatile);
    assert!(runtime.blocked_in_memory(&old.id));
    let fake = ScriptedUpstream::start().await.unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let service = LoginService::with_test_config(
        store.clone(),
        client.clone(),
        obs.clone(),
        LoginTestConfig {
            authorize_url: format!("{}/authorize", fake.url),
            token_url: format!("{}/token", fake.url),
            profile_url: format!("{}/profile", fake.url),
            callback_port: 0,
        },
    )
    .unwrap()
    .with_runtime(runtime.clone());

    fake.state.push(ScriptedResponse::json(
        StatusCode::BAD_REQUEST,
        r#"{"error":"synthetic-denied"}"#,
    ));
    let failed = complete(&service, &client).await;
    assert_eq!(failed.status, LoginState::Failed);
    assert!(runtime.blocked_in_memory(&old.id));
    let views = runtime.views().await.unwrap();
    assert_eq!(
        views.iter().find(|a| a["id"] == old.id).unwrap()["refresh_attention"],
        true
    );
    assert_eq!(
        fs::read(
            home.path()
                .join("accounts")
                .join(format!("{}.json", old.id))
        )
        .unwrap(),
        before
    );

    fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        serde_json::to_vec(&json!({
            "access_token":"synthetic-new-access","refresh_token":"synthetic-new-refresh",
            "id_token":id_token(),"expires_in":3600
        }))
        .unwrap(),
    ));
    let completed = complete(&service, &client).await;
    assert_eq!(completed.status, LoginState::Completed);
    assert_eq!(completed.account_id.as_deref(), Some(old.id.as_str()));
    assert!(!runtime.blocked_in_memory(&old.id));
    let views = runtime.views().await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0]["id"], old.id);
    assert_eq!(views[0]["refresh_attention"], false);
    assert_eq!(views[0]["health"]["kind"], "cooling");
    assert_eq!(views[0]["health"]["reason"], "payment");
    assert_eq!(views[0]["pool_eligible"], false);
    let saved = store.load(&old.id).unwrap();
    assert_eq!(saved.identity, old.identity);
    assert_eq!(saved.email, "new@example.invalid");
    assert_eq!(saved.source, Source::Oauth);
    assert_eq!(saved.label, old.label);
    assert_eq!(saved.user_state, old.user_state);
    assert_eq!(saved.drain, old.drain);
    assert_eq!(saved.allow_credits, old.allow_credits);
    assert_eq!(saved.payment_until, old.payment_until);
    assert_eq!(saved.payment_failures, old.payment_failures);
    assert_eq!(saved.auth_failures, 0);
    assert!(!saved.auth_probe_pending);
    assert_eq!(saved.generation, old.generation + 1);
    assert_eq!(
        store.tokens(&saved).unwrap().access_token,
        "synthetic-new-access"
    );
    assert!(!home.path().join("accounts/.login-migration").exists());
    obs.flush().unwrap();
    // The prior and completion login ledger rows retain one account association.
    let mut rows = Vec::new();
    fn collect(path: &Path, rows: &mut Vec<serde_json::Value>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, rows);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                for line in fs::read_to_string(path).unwrap().lines() {
                    rows.push(serde_json::from_str(line).unwrap());
                }
            }
        }
    }
    collect(home.path(), &mut rows);
    for login_id in ["synthetic-prior-login", completed_login_id(&rows)] {
        assert!(
            rows.iter().any(|r| r["event"] == "login"
                && r["login_id"] == login_id
                && r["account"] == old.id)
        );
    }
}

fn completed_login_id(rows: &[serde_json::Value]) -> &str {
    rows.iter()
        .find(|r| {
            r["event"] == "login"
                && r["outcome"] == "completed"
                && r["login_id"] != "synthetic-prior-login"
        })
        .unwrap()["login_id"]
        .as_str()
        .unwrap()
}

struct RecoverySource {
    proof: accounts::takeover::SourceProof,
    bytes: Vec<u8>,
    now: chrono::DateTime<chrono::Utc>,
}
impl accounts::takeover::SourceAccess for RecoverySource {
    fn snapshot(
        &self,
        _: Provider,
        _: &Path,
    ) -> Result<accounts::takeover::SourceSnapshot, accounts::takeover::TakeoverError> {
        Ok(accounts::takeover::SourceSnapshot {
            proof: self.proof.clone(),
            bytes: zeroize::Zeroizing::new(self.bytes.clone()),
        })
    }
    fn owner(&self, _: Provider, _: &accounts::takeover::SourceProof) -> accounts::takeover::Owner {
        accounts::takeover::Owner::Stopped
    }
    fn retire(
        &self,
        _: Provider,
        proof: &accounts::takeover::SourceProof,
        _: &Path,
        _: bool,
    ) -> Result<(), accounts::takeover::TakeoverError> {
        if proof == &self.proof {
            Ok(())
        } else {
            Err(accounts::takeover::TakeoverError::Conflict)
        }
    }
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.now
    }
}
struct NoProfile;
impl accounts::takeover::ProfileResolver for NoProfile {
    fn endpoint(&self) -> &str {
        "http://127.0.0.1:23456/unused"
    }
    fn claude<'a>(
        &'a self,
        _: &'a str,
    ) -> accounts::takeover::AsyncResult<'a, accounts::takeover::ProfileIdentity> {
        Box::pin(async { Err(accounts::takeover::TakeoverError::Invalid) })
    }
}

#[tokio::test]
async fn r6_started_legacy_takeover_reopens_then_actual_oauth_supersedes_and_old_commit_sends_zero_posts()
 {
    use accounts::takeover::{
        Dependencies, Owner, Phase, ReceiptState, SourceProof, TakeoverError, TakeoverRequest,
    };
    use sha2::Digest;
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r6-relogin-fixtures");
    fs::create_dir_all(&base).unwrap();
    let home = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
    let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
    let now = chrono::Utc::now();
    let identity = Identity::Codex {
        account_id: "r6-workspace".into(),
        user_id: None,
        email: "old@example.invalid".into(),
    };
    let account: Account = serde_json::from_value(json!({
        "schema":1,"id":identity.id(),"identity":identity,"provider":"codex","email":"old@example.invalid","label":"keep policy",
        "source":{"kind":"oauth"},"user_state":"paused","drain":false,"allow_credits":true,
        "health":{"kind":"ready"},"payment_failures":2,"payment_until":"2099-01-01T00:00:00Z","generation":0,
        "token_meta":{"expires_at":"2026-01-01T00:00:00Z","refresh_uncertain":false}
    })).unwrap();
    let old_tokens = Tokens {
        access_token: "synthetic-access".into(),
        refresh_token: "synthetic-old-family".into(),
        id_token: Some(id_token()),
    };
    let old = store.save(&account, &old_tokens, None).unwrap();
    let obs = Arc::new(Observability::new(home.path(), None).unwrap());
    obs.record_login(
        Provider::Codex,
        "r6-before-takeover",
        "completed",
        Some(&old.id),
    );
    obs.flush().unwrap();
    // A persisted association must survive at the same id; login does not own bindings.
    let table = qb_core::bindings::BindingTable::new(
        home.path(),
        qb_core::config::RoutingConfig::default(),
    )
    .unwrap();
    table.import(json!({"schema":1,"bindings":[{"key":"synthetic-session","account":old.id,"last_used":now}]})).unwrap();
    table.flush().unwrap();
    let bindings = home.path().join("state/bindings.json");
    let binding_bytes = fs::read(&bindings).unwrap();
    let bytes = serde_json::to_vec(&json!({"auth_mode":"chatgpt","tokens":{"access_token":"synthetic-access",
        "refresh_token":"synthetic-source-family","id_token":id_token(),"account_id":"r6-workspace"},"last_refresh":"unknown-localized"})).unwrap();
    let path = home.path().join("native/auth.json");
    let source = Arc::new(RecoverySource {
        proof: SourceProof {
            path: path.clone(),
            volume: 1,
            index: 100,
            modified: now - chrono::Duration::minutes(5),
            sha256: sha2::Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        },
        bytes,
        now,
    });
    assert_eq!(
        accounts::takeover::SourceAccess::owner(source.as_ref(), Provider::Codex, &source.proof),
        Owner::Stopped
    );
    let deps = Dependencies {
        source,
        profile: Arc::new(NoProfile),
    };
    let fake = ScriptedUpstream::start().await.unwrap();
    fake.state.push(ScriptedResponse::json(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"synthetic-ambiguous"}"#,
    ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let runtime = AccountRuntime::new(store.clone(), obs.clone(), 1.0);
    let gates = credentials::TokenGates::new(client.clone(), runtime)
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let mut request = TakeoverRequest {
        provider: Provider::Codex,
        path,
        phase: Phase::Inspect,
        inspect_digest: None,
        replace: true,
        keep_source: false,
    };
    let inspect = gates
        .takeover(store.clone(), obs.clone(), request.clone(), deps.clone())
        .await
        .unwrap();
    request.phase = Phase::Commit;
    request.inspect_digest = Some(inspect["inspect_digest"].as_str().unwrap().into());
    assert!(matches!(
        gates
            .takeover(store.clone(), obs.clone(), request.clone(), deps.clone())
            .await,
        Err(TakeoverError::Uncertain)
    ));
    assert_eq!(fake.state.requests().len(), 1);
    assert_eq!(
        store.takeover_receipt(&old.id).unwrap().unwrap().state,
        ReceiptState::Started
    );
    store.update_metadata(&old.id, |a| a.drain = true).unwrap();
    gates.shutdown_refreshes().await.unwrap();
    store.release_lease();
    let reopened = Arc::new(AccountStore::acquire(home.path()).unwrap());
    assert!(reopened.tokens(&reopened.load(&old.id).unwrap()).is_err());
    let runtime = AccountRuntime::new(reopened.clone(), obs.clone(), 1.0);
    let oauth = ScriptedUpstream::start().await.unwrap();
    oauth.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({"access_token":"synthetic-access",
        "refresh_token":"synthetic-fresh-oauth-family","id_token":id_token(),"expires_in":1800})
        .to_string(),
    ));
    let service = LoginService::with_test_config(
        reopened.clone(),
        client.clone(),
        obs.clone(),
        LoginTestConfig {
            authorize_url: format!("{}/authorize", oauth.url),
            token_url: format!("{}/token", oauth.url),
            profile_url: format!("{}/profile", oauth.url),
            callback_port: 0,
        },
    )
    .unwrap()
    .with_runtime(runtime.clone());
    let result = complete(&service, &client).await;
    assert_eq!(result.status, LoginState::Completed);
    assert_eq!(result.account_id.as_deref(), Some(old.id.as_str()));
    let saved = reopened.load(&old.id).unwrap();
    assert_eq!(saved.identity, old.identity);
    assert_eq!(saved.source, Source::Oauth);
    assert_eq!(saved.label, old.label);
    assert_eq!(saved.user_state, old.user_state);
    assert!(saved.drain && saved.allow_credits);
    assert_eq!(saved.payment_failures, old.payment_failures);
    assert_eq!(saved.payment_until, old.payment_until);
    assert!(!saved.token_meta.refresh_uncertain);
    assert_eq!(
        reopened.tokens(&saved).unwrap().refresh_token,
        "synthetic-fresh-oauth-family"
    );
    assert_eq!(fs::read(bindings).unwrap(), binding_bytes);
    assert_eq!(
        table.current(Some("synthetic-session")).as_deref(),
        Some(old.id.as_str())
    );
    let fresh = credentials::TokenGates::new(client, runtime)
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let replay = fresh
        .takeover(reopened.clone(), obs.clone(), request.clone(), deps.clone())
        .await
        .unwrap();
    assert_eq!(replay["refresh"]["ok"], false);
    assert_eq!(fake.state.requests().len(), 1);
    assert_eq!(oauth.state.requests().len(), 1);
    fresh.shutdown_refreshes().await.unwrap();
    drop(service);
    reopened.release_lease();
    let again = Arc::new(AccountStore::acquire(home.path()).unwrap());
    let fresh = credentials::TokenGates::new(
        reqwest::Client::new(),
        AccountRuntime::new(again.clone(), obs.clone(), 1.0),
    );
    assert_eq!(
        fresh
            .takeover(again.clone(), obs, request, deps)
            .await
            .unwrap(),
        replay
    );
    assert_eq!(
        again
            .tokens(&again.load(&old.id).unwrap())
            .unwrap()
            .refresh_token,
        "synthetic-fresh-oauth-family"
    );
    assert_eq!(fake.state.requests().len(), 1);
    fresh.shutdown_refreshes().await.unwrap();
}
