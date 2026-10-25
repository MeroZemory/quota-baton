#![cfg(all(feature = "testkit", debug_assertions))]

// Compile the bounded internal credential layer against the production modules.
pub use qb_core::{account_runtime, accounts, observability};
#[path = "support/unscoped_tasks.rs"]
mod boxes;
#[path = "../src/relay/credentials.rs"]
#[allow(dead_code)] // This isolated harness exercises a subset of the shared production gate.
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
async fn m51_linked_first_request_reloads_without_near_expiry() {
    let _serial = SERIAL.lock().await;
    for provider in [Provider::Claude, Provider::Codex] {
        let f = Fixture::new(true, 60, provider);
        f.newer_link();
        let (account, tokens) = f
            .gates()
            .before_request(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap();
        assert_eq!(tokens.access_token, "synthetic-access-new");
        assert_eq!(account.health, Health::Ready);
    }
}

#[tokio::test]
async fn m51_startup_and_prepoll_reload_are_unthrottled_and_recover_legacy_health() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, 60, Provider::Codex);
    let gates = f.gates();
    // Occupy request/selection admission before the startup and poll reloads.
    gates
        .before_request(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    for health in [
        Health::NeedsRelogin {
            reason: "legacy-revoked".into(),
            since: Utc::now(),
        },
        Health::Cooling {
            reason: HealthReason::LinkStale,
            until: Utc::now() + ChronoDuration::hours(1),
            scope: "account".into(),
        },
    ] {
        f.store
            .update_metadata(&f.account.id, |account| {
                account.health = health;
                account.auth_failures = 3;
                account.auth_probe_pending = true;
            })
            .unwrap();
        f.newer_link();
        let updated = gates
            .reload_linked(
                f.store.clone(),
                f.obs.clone(),
                f.account.id.clone(),
                "prepoll",
            )
            .await
            .unwrap();
        assert_eq!(updated.health, Health::Ready);
        assert_eq!(updated.auth_failures, 0);
        assert!(!updated.auth_probe_pending);
        let persisted = f.store.load(&updated.id).unwrap();
        assert_eq!(persisted.health, Health::Ready);
        assert_eq!(
            persisted.token_meta.last_refresh_at,
            updated.token_meta.last_refresh_at
        );
    }
}

#[tokio::test]
async fn m51_linked_401_reloads_before_legacy_needs_relogin_guard() {
    let _serial = SERIAL.lock().await;
    for newer in [false, true] {
        let f = Fixture::new(true, 60, Provider::Claude);
        let legacy = f
            .store
            .update_metadata(&f.account.id, |account| {
                account.health = Health::NeedsRelogin {
                    reason: "legacy-revoked".into(),
                    since: Utc::now(),
                };
            })
            .unwrap();
        let failed = f.store.tokens(&legacy).unwrap();
        if newer {
            f.newer_link();
        }
        let retry = f
            .gates()
            .after_401(f.store.clone(), f.obs.clone(), legacy, &failed)
            .await
            .unwrap();
        assert_eq!(retry.is_some(), newer);
        let current = f.store.load(&f.account.id).unwrap();
        if newer {
            assert_eq!(current.health, Health::Ready);
            assert_eq!(retry.unwrap().1.access_token, "synthetic-access-new");
        } else {
            assert!(matches!(
                current.health,
                Health::Cooling {
                    reason: HealthReason::LinkStale,
                    ..
                }
            ));
        }
    }
}

#[tokio::test]
async fn m51_exhausted_linked_retry_rereads_and_persists_without_auth_backoff() {
    let _serial = SERIAL.lock().await;
    for newer in [false, true] {
        let f = Fixture::new(true, 60, Provider::Codex);
        let gates = f.gates();
        let failed = f.store.tokens(&f.account).unwrap();
        if newer {
            f.newer_link();
        }
        // auth_failed has no retry result: it can only persist the reload.
        gates
            .auth_failed(f.store.clone(), f.obs.clone(), f.account.clone(), &failed)
            .await
            .unwrap();
        let current = f.store.load(&f.account.id).unwrap();
        assert_eq!(current.auth_failures, 0);
        assert!(!current.auth_probe_pending);
        if newer {
            assert_eq!(current.health, Health::Ready);
            assert_eq!(
                f.store.tokens(&current).unwrap().access_token,
                "synthetic-access-new"
            );
        } else {
            assert!(matches!(
                current.health,
                Health::Cooling {
                    reason: HealthReason::LinkStale,
                    ..
                }
            ));
            gates
                .cool_unavailable_auth(f.store.clone(), &current, &failed)
                .await
                .unwrap();
            assert_eq!(f.store.load(&current.id).unwrap().health, current.health);
            // Expired link-stale must not become auth cooling either.
            f.store
                .update_metadata(&current.id, |account| {
                    account.health = Health::Cooling {
                        reason: HealthReason::LinkStale,
                        until: Utc::now() - ChronoDuration::seconds(1),
                        scope: "account".into(),
                    };
                })
                .unwrap();
            gates
                .cool_unavailable_auth(f.store.clone(), &current, &failed)
                .await
                .unwrap();
            assert!(!matches!(
                f.store.load(&current.id).unwrap().health,
                Health::NeedsRelogin { .. }
                    | Health::Cooling {
                        reason: HealthReason::Auth,
                        ..
                    }
            ));
        }
    }
}

#[tokio::test]
async fn m51_linked_stale_failed_token_still_rereads_cpa() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, 60, Provider::Codex);
    let failed = f.store.tokens(&f.account).unwrap();
    let mut rotated = f.store.tokens(&f.account).unwrap();
    rotated.access_token = "synthetic-concurrent-token".into();
    f.store
        .save(&f.account, &rotated, Some(f.account.generation))
        .unwrap();
    f.newer_link();
    let (current, tokens) = f
        .gates()
        .after_401(f.store.clone(), f.obs.clone(), f.account.clone(), &failed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tokens.access_token, "synthetic-access-new");
    assert_eq!(current.health, Health::Ready);
}

#[tokio::test]
async fn m51_startup_stale_link_converts_legacy_needs_relogin() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, 60, Provider::Codex);
    f.store
        .update_metadata(&f.account.id, |account| {
            account.health = Health::NeedsRelogin {
                reason: "legacy-revoked".into(),
                since: Utc::now(),
            };
        })
        .unwrap();
    let current = f
        .gates()
        .reload_linked(
            f.store.clone(),
            f.obs.clone(),
            f.account.id.clone(),
            "startup",
        )
        .await
        .unwrap();
    assert!(matches!(
        current.health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
    assert_eq!(
        f.store.tokens(&current).unwrap().access_token,
        "synthetic-access-old"
    );
    assert_eq!(f.store.load(&current.id).unwrap().health, current.health);
}

#[tokio::test]
async fn m51_second_linked_401_cools_current_retry_token() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, 60, Provider::Claude);
    let gates = f.gates();
    let old = f.store.tokens(&f.account).unwrap();
    f.newer_link();
    let (current, retried) = gates
        .after_401(f.store.clone(), f.obs.clone(), f.account.clone(), &old)
        .await
        .unwrap()
        .unwrap();
    gates
        .auth_failed(f.store.clone(), f.obs.clone(), current, &retried)
        .await
        .unwrap();
    let cooled = f.store.load(&f.account.id).unwrap();
    assert!(matches!(
        cooled.health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
    assert_eq!(cooled.auth_failures, 0);
}

#[tokio::test]
async fn m51_stale_linked_401_never_cools_concurrently_replaced_token() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, 60, Provider::Codex);
    let gates = f.gates();
    let old = f.store.tokens(&f.account).unwrap();
    f.newer_link();
    let (current, new) = gates
        .after_401(f.store.clone(), f.obs.clone(), f.account.clone(), &old)
        .await
        .unwrap()
        .unwrap();
    // Even an expired replacement must not be cooled by a stale-token failure.
    fs::remove_file(f.cpa.join("fixture.json")).unwrap();
    let mut expired = current.clone();
    expired.token_meta.last_refresh_at = Some(Utc::now());
    expired.token_meta.expires_at = Some(Utc::now() - ChronoDuration::minutes(1));
    let current = f
        .store
        .save(&expired, &new, Some(current.generation))
        .unwrap();
    let (after, tokens) = gates
        .after_401(f.store.clone(), f.obs.clone(), current.clone(), &old)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.health, Health::Ready);
    assert_eq!(tokens.access_token, new.access_token);
    gates
        .auth_failed(f.store.clone(), f.obs.clone(), current.clone(), &old)
        .await
        .unwrap();
    assert_eq!(f.store.load(&current.id).unwrap().health, Health::Ready);
    let after = gates
        .revoke_if_current(f.store.clone(), &current, &old)
        .await
        .unwrap();
    assert_eq!(after.health, Health::Ready);
    assert_eq!(f.store.load(&current.id).unwrap().auth_failures, 0);
}

#[tokio::test]
async fn m51_revoked_linked_fallback_rereads_and_never_revokes() {
    let _serial = SERIAL.lock().await;
    for newer in [false, true] {
        let f = Fixture::new(true, 60, Provider::Claude);
        let failed = f.store.tokens(&f.account).unwrap();
        if newer {
            f.newer_link();
        }
        let current = f
            .gates()
            .revoke_if_current(f.store.clone(), &f.account, &failed)
            .await
            .unwrap();
        if newer {
            assert_eq!(current.health, Health::Ready);
            assert_eq!(
                f.store.tokens(&current).unwrap().access_token,
                "synthetic-access-new"
            );
        } else {
            assert!(matches!(
                current.health,
                Health::Cooling {
                    reason: HealthReason::LinkStale,
                    ..
                }
            ));
        }
    }
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
        // Each status is an independent attempt, not a simulated crash with
        // the previous attempt's uncertain write-ahead marker.
        let mut account = f.store.load(&f.account.id).unwrap();
        let tokens = f.store.tokens(&account).unwrap();
        account.token_meta.refresh_uncertain = false;
        let account = f
            .store
            .save(&account, &tokens, Some(account.generation))
            .unwrap();
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
        assert!(!current.token_meta.refresh_uncertain);
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

#[tokio::test]
async fn c5_terminal_refresh_survives_metadata_generation_race() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let fake = ScriptedUpstream::start().await.unwrap();
    let mut reply = ScriptedResponse::json(StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#);
    reply.chunks[0].0 = Duration::from_millis(150);
    fake.state.push(reply);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let old = f.store.tokens(&f.account).unwrap();
    let (result, _) = tokio::join!(
        gates.after_401(f.store.clone(), f.obs.clone(), f.account.clone(), &old),
        async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while fake.state.requests().is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            f.store
                .update_metadata(&f.account.id, |a| a.label = "concurrent metadata".into())
                .unwrap();
        }
    );
    assert!(result.unwrap().is_none());
    let current = f.store.load(&f.account.id).unwrap();
    assert!(matches!(current.health, Health::NeedsRelogin { .. }));
    assert_eq!(current.label, "concurrent metadata");
    assert!(
        gates
            .after_401(f.store.clone(), f.obs.clone(), current, &old)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fake.state.requests().len(), 1);
}

#[tokio::test]
async fn c5_stale_revocation_does_not_revoke_rotated_credentials() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let old = f.store.tokens(&f.account).unwrap();
    let mut rotated = f.store.tokens(&f.account).unwrap();
    rotated.access_token = "synthetic-rotated".into();
    f.store
        .save(&f.account, &rotated, Some(f.account.generation))
        .unwrap();
    let gates = f.gates();
    let current = gates
        .revoke_if_current(f.store.clone(), &f.account, &old)
        .await
        .unwrap();
    assert_eq!(current.health, Health::Ready);
    assert!(
        !gates
            .is_current(f.store.clone(), &f.account, &old)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn c5_link_stale_selection_reload_uses_shared_sixty_second_throttle() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, -1, Provider::Codex);
    let gates = f.gates();
    let current = gates
        .selection_reload(f.store.clone(), f.obs.clone(), f.account.clone())
        .await
        .unwrap();
    assert!(matches!(
        current.health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ));
    f.newer_link();
    let still = gates
        .selection_reload(f.store.clone(), f.obs.clone(), current.clone())
        .await
        .unwrap();
    let old = f.store.tokens(&still).unwrap();
    assert_eq!(old.access_token, "synthetic-access-old");
    assert!(
        gates
            .after_401(f.store.clone(), f.obs.clone(), still, &old)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn c5_transient_auth_cooling_is_token_guarded_and_does_not_increment_backoff() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let gates = f.gates();
    let old = f.store.tokens(&f.account).unwrap();
    gates
        .cool_unavailable_auth(f.store.clone(), &f.account, &old)
        .await
        .unwrap();
    let current = f.store.load(&f.account.id).unwrap();
    assert_eq!(current.auth_failures, 0);
    assert!(matches!(
        current.health,
        Health::Cooling {
            reason: HealthReason::Auth,
            ..
        }
    ));
    let mut new = f.store.tokens(&current).unwrap();
    new.access_token = "synthetic-new-token".into();
    let mut ready = current.clone();
    ready.health = Health::Ready;
    f.store.save(&ready, &new, Some(ready.generation)).unwrap();
    gates
        .cool_unavailable_auth(f.store.clone(), &ready, &old)
        .await
        .unwrap();
    assert_eq!(f.store.load(&ready.id).unwrap().health, Health::Ready);
}
