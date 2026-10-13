#![cfg(all(feature = "testkit", debug_assertions))]

// Compile the bounded internal credential layer against the production modules.
pub use qb_core::{account_runtime, accounts, observability};
#[path = "../src/relay/credentials.rs"]
mod credentials;

use accounts::{
    Account, AccountStore, Health, HealthReason, HealthSignal, Provider, Source, UserState, import,
};
use axum::http::StatusCode;
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, Utc};
use credentials::TokenGates;
use observability::Observability;
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::json;
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    cpa: PathBuf,
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    runtime: Arc<account_runtime::AccountRuntime>,
    account: Account,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new(linked: bool, expiry_minutes: i64, provider: Provider) -> Self {
        let sandbox =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/credential-fixtures");
        fs::create_dir_all(&sandbox).unwrap();
        let root = tempfile::tempdir_in(sandbox).unwrap();
        let cpa = root.path().join("synthetic-cpa");
        fs::create_dir(&cpa).unwrap();
        let file = json!({"type":provider.as_str(),"account_id":"synthetic-credential","account_uuid":"synthetic-credential","organization_uuid":"synthetic-org","email":"credential@example.invalid","access_token":"synthetic-access-old","refresh_token":"synthetic-refresh-old","last_refresh":Utc::now()-ChronoDuration::hours(1),"expired":Utc::now()+ChronoDuration::minutes(expiry_minutes)});
        fs::write(cpa.join("fixture.json"), serde_json::to_vec(&file).unwrap()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let store = loop {
            match AccountStore::acquire(root.path()) {
                Ok(store) => break Arc::new(store),
                Err(accounts::StoreError::LeaseBusy) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("synthetic credential lease: {error}"),
            }
        };
        import::run(&store, &cpa, &[], false).unwrap();
        let mut account = store.list().unwrap().remove(0);
        if !linked {
            let tokens = store.tokens(&account).unwrap();
            account.source = Source::Oauth;
            account.user_state = UserState::Active;
            account = store
                .save(&account, &tokens, Some(account.generation))
                .unwrap();
        } else {
            account = store
                .update_metadata(&account.id, |a| a.user_state = UserState::Active)
                .unwrap();
        }
        let obs = Arc::new(Observability::new(root.path(), None).unwrap());
        let runtime = account_runtime::AccountRuntime::new(store.clone(), obs.clone(), 1.0);
        Self {
            _root: root,
            cpa,
            store,
            obs,
            runtime,
            account,
        }
    }

    fn gates(&self) -> TokenGates {
        TokenGates::new(
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            self.runtime.clone(),
        )
    }

    fn newer_link(&self) {
        let path = self.cpa.join("fixture.json");
        let mut file: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        file["last_refresh"] = json!(Utc::now());
        file["expired"] = json!(Utc::now() + ChronoDuration::hours(1));
        file["access_token"] = json!("synthetic-access-new");
        file["refresh_token"] = json!("synthetic-refresh-new");
        fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
    }
}

#[tokio::test]
async fn c5_linked_stale_401_newer_token_recovery_and_stale_race() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, 5, Provider::Codex);
    let gates = f.gates();
    let (account, old) = gates
        .before_request(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    // A valid linked token with no newer file stays ready before expiry.
    assert_eq!(account.health, Health::Ready);
    assert!(
        gates
            .after_401(f.store.clone(), f.obs.clone(), account.clone(), &old)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        f.store.load(&account.id).unwrap().health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
    f.newer_link();
    let (new, tokens) = gates
        .after_401(f.store.clone(), f.obs.clone(), account.clone(), &old)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.health, Health::Ready);
    assert_eq!(tokens.access_token, "synthetic-access-new");
    gates
        .auth_failed(f.store.clone(), f.obs.clone(), account, &old)
        .await
        .unwrap();
    assert_eq!(f.store.load(&new.id).unwrap().health, Health::Ready);
    assert_eq!(f.store.load(&new.id).unwrap().auth_failures, 0);
}

#[tokio::test]
async fn c5_linked_expired_no_newer_token_cools() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, -1, Provider::Codex);
    let (account, _) = f
        .gates()
        .before_request(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    assert!(matches!(
        account.health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
}

#[tokio::test]
async fn c5_owned_refresh_singleflight_stale_401_and_auth_backoff() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let upstream = ScriptedUpstream::start().await.unwrap();
    upstream.state.push(ScriptedResponse::json(StatusCode::OK,r#"{"access_token":"synthetic-access-new","refresh_token":"synthetic-refresh-new","expires_in":3600}"#));
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, upstream.url.clone());
    let old = f.store.tokens(&f.account).unwrap();
    let (first, second) = tokio::join!(
        gates.after_401(f.store.clone(), f.obs.clone(), f.account.clone(), &old),
        gates.after_401(f.store.clone(), f.obs.clone(), f.account.clone(), &old)
    );
    let (account, new) = first.unwrap().unwrap();
    assert_eq!(second.unwrap().unwrap().1.access_token, new.access_token);
    assert_eq!(upstream.state.requests().len(), 1);
    let request = &upstream.state.requests()[0];
    assert!(
        request.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/x-www-form-urlencoded")
    );
    let body = std::str::from_utf8(&request.body).unwrap();
    assert!(body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
    assert!(body.contains("grant_type=refresh_token"));
    assert!(!account.token_meta.refresh_uncertain);
    gates
        .auth_failed(f.store.clone(), f.obs.clone(), f.account.clone(), &old)
        .await
        .unwrap();
    assert_eq!(f.store.load(&account.id).unwrap().health, Health::Ready);
    for minutes in [15, 30] {
        let before = Utc::now();
        gates
            .auth_failed(f.store.clone(), f.obs.clone(), account.clone(), &new)
            .await
            .unwrap();
        let updated = f.store.load(&account.id).unwrap();
        assert!(
            matches!(updated.health,Health::Cooling {reason:HealthReason::Auth,until,..} if until>=before+ChronoDuration::minutes(minutes) && until<=Utc::now()+ChronoDuration::minutes(minutes))
        );
    }
    assert_eq!(f.store.load(&account.id).unwrap().auth_failures, 2);
}

#[tokio::test]
async fn c5_owned_terminal_refresh_codes_and_token_endpoint_401() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let upstream = ScriptedUpstream::start().await.unwrap();
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, upstream.url.clone());
    for code in [
        "refresh_token_invalidated",
        "refresh_token_reused",
        "refresh_token_expired",
        "refresh_token_revoked",
        "invalid_grant",
    ] {
        f.store
            .update_metadata(&f.account.id, |a| {
                a.apply_health_signal(HealthSignal::ReloginCompleted, Utc::now());
            })
            .unwrap();
        let account = f.store.load(&f.account.id).unwrap();
        let tokens = f.store.tokens(&account).unwrap();
        upstream.state.push(ScriptedResponse::json(
            StatusCode::BAD_REQUEST,
            serde_json::to_vec(&json!({"error":code})).unwrap(),
        ));
        assert!(
            gates
                .after_401(f.store.clone(), f.obs.clone(), account.clone(), &tokens)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            matches!(f.store.load(&account.id).unwrap().health,Health::NeedsRelogin {reason,..} if reason==code)
        );
    }
    f.store
        .update_metadata(&f.account.id, |a| {
            a.apply_health_signal(HealthSignal::ReloginCompleted, Utc::now());
        })
        .unwrap();
    let account = f.store.load(&f.account.id).unwrap();
    let tokens = f.store.tokens(&account).unwrap();
    upstream
        .state
        .push(ScriptedResponse::json(StatusCode::UNAUTHORIZED, "not-json"));
    gates
        .after_401(f.store.clone(), f.obs.clone(), account.clone(), &tokens)
        .await
        .unwrap();
    assert!(
        matches!(f.store.load(&account.id).unwrap().health,Health::NeedsRelogin {reason,..} if reason=="token_endpoint_401")
    );
}

#[tokio::test]
async fn c5_owned_transient_refresh_errors_keep_health_and_writeahead() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Claude);
    let upstream = ScriptedUpstream::start().await.unwrap();
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::BAD_REQUEST,
    ] {
        let gates = f
            .gates()
            .with_token_endpoint(Provider::Claude, upstream.url.clone());
        let account = f.store.load(&f.account.id).unwrap();
        let tokens = f.store.tokens(&account).unwrap();
        upstream.state.push(ScriptedResponse::json(
            status,
            r#"{"error":"unknown_error"}"#,
        ));
        assert!(
            gates
                .after_401(f.store.clone(), f.obs.clone(), account.clone(), &tokens)
                .await
                .unwrap()
                .is_none()
        );
        let current = f.store.load(&account.id).unwrap();
        assert_eq!(current.health, Health::Ready);
        assert!(current.token_meta.refresh_uncertain);
        assert_eq!(
            f.store.tokens(&current).unwrap().access_token,
            tokens.access_token
        );
    }
    let requests = upstream.state.requests();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(body["grant_type"], "refresh_token");
}

#[tokio::test]
async fn c5_owned_refresh_detached_from_cancelled_request() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let upstream = ScriptedUpstream::start().await.unwrap();
    let mut response = ScriptedResponse::json(StatusCode::OK, Bytes::new());
    response.chunks=vec![(Duration::from_millis(200),Bytes::from_static(br#"{"access_token":"synthetic-detached","refresh_token":"synthetic-rotated","expires_in":3600}"#))];
    upstream.state.push(response);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, upstream.url.clone());
    let store = f.store.clone();
    let obs = f.obs.clone();
    let account = f.account.clone();
    let tokens = f.store.tokens(&account).unwrap();
    let caller = tokio::spawn(async move { gates.after_401(store, obs, account, &tokens).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while upstream.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
    caller.abort();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let a = f.store.load(&f.account.id).unwrap();
            if f.store.tokens(&a).unwrap().access_token == "synthetic-detached" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(upstream.state.requests().len(), 1);
}

#[tokio::test]
async fn c5_rotated_persist_failure_retains_memory_and_retries_without_token_rollback() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let upstream = ScriptedUpstream::start().await.unwrap();
    let mut response = ScriptedResponse::json(StatusCode::OK, Bytes::new());
    response.chunks = vec![(Duration::from_millis(200), Bytes::from_static(br#"{"access_token":"synthetic-retained","refresh_token":"synthetic-retained-refresh","expires_in":3600}"#))];
    upstream.state.push(response);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, upstream.url.clone());
    let refresh_gates = gates.clone();
    let store = f.store.clone();
    let obs = f.obs.clone();
    let account = f.account.clone();
    let old = f.store.tokens(&account).unwrap();
    let caller =
        tokio::spawn(async move { refresh_gates.after_401(store, obs, account, &old).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while upstream.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let path = f
        ._root
        .path()
        .join("accounts")
        .join(format!("{}.json", f.account.id));
    let original_permissions = fs::metadata(&path).unwrap().permissions();
    let mut permissions = original_permissions.clone();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();
    let result = caller.await.unwrap();
    // Always restore the synthetic file before assertions or fixture cleanup.
    fs::set_permissions(&path, original_permissions).unwrap();
    assert!(result.unwrap().is_none());
    assert_eq!(
        f.runtime.views().await.unwrap()[0]["health"]["reason"],
        "store_error"
    );
    assert!(!f.runtime.eligible(&f.account.id, None).await);
    assert!(
        gates
            .before_request(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .is_err()
    );
    let current = f.store.load(&f.account.id).unwrap();
    assert_eq!(
        f.store.tokens(&current).unwrap().access_token,
        "synthetic-access-old"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let a = f.store.load(&f.account.id).unwrap();
            if f.store.tokens(&a).unwrap().access_token == "synthetic-retained" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let current = f.store.load(&f.account.id).unwrap();
    assert!(!current.token_meta.refresh_uncertain);
    assert_eq!(current.health, Health::Ready);
    assert_eq!(
        f.runtime.views().await.unwrap()[0]["health"]["kind"],
        "ready"
    );
    assert_eq!(upstream.state.requests().len(), 1);
}
