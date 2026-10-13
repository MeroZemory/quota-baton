#![cfg(all(feature = "testkit", debug_assertions))]

// Compile the bounded internal credential layer against the production modules.
pub use qb_core::{account_runtime, accounts, observability};
#[path = "../src/relay/credentials.rs"]
#[allow(dead_code)] // This isolated harness exercises a subset of the shared production gate.
mod credentials;

use accounts::{Account, AccountStore, Health, HealthReason, Provider, Source, UserState, import};
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
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m1b-refresh-fixtures");
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

fn set_lifetime(f: &Fixture, remaining: i64, uncertain: bool) -> Account {
    // This is a synthetic clock change, not a credential import. Mutate only
    // fixture metadata so moving time cannot violate production anti-rollback.
    let now = Utc::now();
    let path = f
        ._root
        .path()
        .join("accounts")
        .join(format!("{}.json", f.account.id));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["token_meta"]["expires_at"] = json!(now + ChronoDuration::seconds(remaining));
    record["token_meta"]["last_refresh_at"] =
        json!(now + ChronoDuration::seconds(remaining - 1000));
    record["token_meta"]["refresh_uncertain"] = json!(uncertain);
    fs::write(path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    f.store.load(&f.account.id).unwrap()
}

fn success() -> ScriptedResponse {
    ScriptedResponse::json(
        StatusCode::OK,
        r#"{"access_token":"synthetic-access-new","refresh_token":"synthetic-refresh-new","expires_in":3600}"#,
    )
}

fn ledger(f: &Fixture) -> Vec<serde_json::Value> {
    f.obs.flush().unwrap();
    fs::read_dir(f._root.path().join("ledger"))
        .unwrap()
        .flat_map(|entry| {
            fs::read_to_string(entry.unwrap().path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[tokio::test]
async fn refresh_ahead_boundary_before_request_and_adopted_singleflight_401() {
    let _serial = SERIAL.lock().await;
    for provider in [Provider::Claude, Provider::Codex] {
        let f = Fixture::new(true, 60, provider);
        let mut adopted = f.store.load(&f.account.id).unwrap();
        let tokens = f.store.tokens(&adopted).unwrap();
        if let Source::Cpa { mode, .. } = &mut adopted.source {
            *mode = accounts::ImportMode::Adopted;
        }
        f.store
            .save(&adopted, &tokens, Some(adopted.generation))
            .unwrap();
        let fake = ScriptedUpstream::start().await.unwrap();
        let gates = f.gates().with_token_endpoint(provider, fake.url.clone());
        set_lifetime(&f, 250, false); // 25% remaining is before every jittered boundary.
        assert!(
            gates
                .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert!(fake.state.requests().is_empty());
        let account = set_lifetime(&f, 100, false); // 10% remaining is after every boundary.
        let old = f.store.tokens(&account).unwrap();
        fake.state.push(success());
        let (first, second) = tokio::join!(
            gates.before_request(f.store.clone(), f.obs.clone(), f.account.id.clone()),
            gates.after_401(f.store.clone(), f.obs.clone(), account, &old)
        );
        assert_eq!(first.unwrap().1.access_token, "synthetic-access-new");
        assert_eq!(
            second.unwrap().unwrap().1.access_token,
            "synthetic-access-new"
        );
        assert_eq!(fake.state.requests().len(), 1);
        let current = f.store.load(&f.account.id).unwrap();
        assert!(!current.token_meta.refresh_uncertain);
        let events = ledger(&f);
        let refresh: Vec<_> = events
            .iter()
            .filter(|e| e["event"] == "token_refresh")
            .collect();
        assert_eq!(refresh.len(), 1);
        assert_eq!(refresh[0]["outcome"], "success");
        assert!(refresh[0]["expires_at"].is_string());
        let log = serde_json::to_string(&events).unwrap();
        assert!(!log.contains("synthetic-access"));
        assert!(!log.contains("synthetic-refresh"));
    }
}

#[tokio::test]
async fn crash_uncertain_keeps_access_and_defers_even_current_token_401_until_ahead() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let fake = ScriptedUpstream::start().await.unwrap();
    let account = set_lifetime(&f, 250, true);
    let old = f.store.tokens(&account).unwrap();
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    assert_eq!(
        gates
            .before_request(f.store.clone(), f.obs.clone(), account.id.clone())
            .await
            .unwrap()
            .1
            .access_token,
        old.access_token
    );
    assert!(
        gates
            .after_401(f.store.clone(), f.obs.clone(), account, &old)
            .await
            .unwrap()
            .is_none()
    );
    assert!(fake.state.requests().is_empty());
    // Simulate restart at the next refresh point; the on-disk flag survives.
    set_lifetime(&f, 100, true);
    let restarted = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    fake.state.push(success());
    assert!(
        restarted
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(fake.state.requests().len(), 1);
    assert!(
        !f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
}

#[tokio::test]
async fn transient_refresh_backoff_is_shared_with_401_and_recovers() {
    let _serial = SERIAL.lock().await;
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        let f = Fixture::new(false, 60, Provider::Codex);
        let account = set_lifetime(&f, 100, false);
        let old = f.store.tokens(&account).unwrap();
        let fake = ScriptedUpstream::start().await.unwrap();
        let gates = f
            .gates()
            .with_token_endpoint(Provider::Codex, fake.url.clone());
        fake.state.push(ScriptedResponse::json(status, "not-json"));
        assert!(
            gates
                .refresh_ahead(f.store.clone(), f.obs.clone(), account.id.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            gates
                .after_401(f.store.clone(), f.obs.clone(), account.clone(), &old)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(fake.state.requests().len(), 1);
        assert_eq!(f.store.load(&account.id).unwrap().health, Health::Ready);
        fake.state.push(success());
        tokio::time::sleep(Duration::from_millis(2100)).await;
        assert!(
            gates
                .refresh_ahead(f.store.clone(), f.obs.clone(), account.id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(fake.state.requests().len(), 2);
        let outcomes: Vec<_> = ledger(&f)
            .into_iter()
            .filter(|e| e["event"] == "token_refresh")
            .map(|e| e["outcome"].clone())
            .collect();
        assert_eq!(
            outcomes,
            vec![json!(format!("http_{}", status.as_u16())), json!("success")]
        );
    }
}

#[tokio::test]
async fn unknown_4xx_attention_after_three_and_clear_on_success() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 100, false);
    let fake = ScriptedUpstream::start().await.unwrap();
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    for (index, wait_ms) in [(0, 0), (1, 2100), (2, 4100)] {
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        fake.state.push(ScriptedResponse::json(
            StatusCode::BAD_REQUEST,
            "malformed-error-json",
        ));
        assert!(
            gates
                .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(fake.state.requests().len(), index + 1);
        assert_eq!(
            f.runtime.views().await.unwrap()[0]["refresh_attention"],
            index == 2
        );
        assert_eq!(f.store.load(&f.account.id).unwrap().health, Health::Ready);
    }
    let events = ledger(&f);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["event"] == "core_warning" && e["code"] == "refresh_unknown_4xx")
            .count(),
        1
    );
    fake.state.push(success());
    tokio::time::sleep(Duration::from_millis(8100)).await;
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        f.runtime.views().await.unwrap()[0]["refresh_attention"],
        false
    );
}

#[tokio::test]
async fn linked_never_refreshes_and_non401_reload_unchanged_is_truthful() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, 60, Provider::Codex);
    let fake = ScriptedUpstream::start().await.unwrap();
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    for trigger in ["startup", "before_poll"] {
        assert_eq!(
            gates
                .reload_linked(
                    f.store.clone(),
                    f.obs.clone(),
                    f.account.id.clone(),
                    trigger
                )
                .await
                .unwrap()
                .health,
            Health::Ready
        );
    }
    assert!(fake.state.requests().is_empty());
    let events = ledger(&f);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["event"] == "linked_reload" && e["outcome"] == "unchanged")
            .count(),
        2
    );
    assert!(!events.iter().any(|e| e["outcome"] == "link_stale"));
    f.newer_link();
    assert_eq!(
        gates
            .reload_linked(
                f.store.clone(),
                f.obs.clone(),
                f.account.id.clone(),
                "before_poll"
            )
            .await
            .unwrap()
            .health,
        Health::Ready
    );
    fs::remove_file(f.cpa.join("fixture.json")).unwrap();
    gates
        .reload_linked(
            f.store.clone(),
            f.obs.clone(),
            f.account.id.clone(),
            "before_poll",
        )
        .await
        .unwrap();
    let events = ledger(&f);
    assert!(events.iter().any(|e| e["outcome"] == "link_missing"));
    assert!(!events.iter().any(|e| e["outcome"] == "link_stale"));
}

#[tokio::test]
async fn scheduler_starts_with_due_owned_account_and_aborts_cleanly() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 100, false);
    let fake = ScriptedUpstream::start().await.unwrap();
    fake.state.push(success());
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let task = gates.start_refresh_scheduler(f.store.clone(), f.obs.clone());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let a = f.store.load(&f.account.id).unwrap();
            if f.store.tokens(&a).unwrap().access_token == "synthetic-access-new" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(fake.state.requests().len(), 1);
}

#[tokio::test]
async fn test_build_refuses_default_production_endpoint_without_network() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 100, false);
    let before = std::time::Instant::now();
    assert!(
        f.gates()
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(before.elapsed() < Duration::from_secs(3));
    assert_eq!(f.store.load(&f.account.id).unwrap().health, Health::Ready);
    assert!(
        ledger(&f)
            .iter()
            .any(|e| e["event"] == "token_refresh" && e["outcome"] == "http_503")
    );
}

#[tokio::test]
async fn terminal_refresh_ahead_and_uncertain_reply_have_distinct_outcomes() {
    let _serial = SERIAL.lock().await;
    for code in [
        "invalid_grant",
        "refresh_token_invalidated",
        "refresh_token_reused",
        "refresh_token_expired",
        "refresh_token_revoked",
        "token_endpoint_401",
    ] {
        let f = Fixture::new(false, 60, Provider::Codex);
        set_lifetime(&f, 100, false);
        let fake = ScriptedUpstream::start().await.unwrap();
        let gates = f
            .gates()
            .with_token_endpoint(Provider::Codex, fake.url.clone());
        let status = if code == "token_endpoint_401" {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::BAD_REQUEST
        };
        fake.state.push(ScriptedResponse::json(
            status,
            serde_json::to_vec(&json!({"error":{"code":code}})).unwrap(),
        ));
        assert!(
            gates
                .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            matches!(f.store.load(&f.account.id).unwrap().health, Health::NeedsRelogin { reason, .. } if reason == code)
        );
        assert!(
            gates
                .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(fake.state.requests().len(), 1);
        assert!(
            ledger(&f)
                .iter()
                .any(|e| e["event"] == "token_refresh" && e["outcome"] == code)
        );
    }
    let f = Fixture::new(false, 60, Provider::Claude);
    let account = set_lifetime(&f, 100, false);
    let old = f.store.tokens(&account).unwrap();
    let fake = ScriptedUpstream::start().await.unwrap();
    fake.state
        .push(ScriptedResponse::json(StatusCode::OK, "truncated-json"));
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Claude, fake.url.clone());
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), account.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        gates
            .after_401(f.store.clone(), f.obs.clone(), account, &old)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fake.state.requests().len(), 1);
    let current = f.store.load(&f.account.id).unwrap();
    assert_eq!(current.health, Health::Ready);
    assert!(current.token_meta.refresh_uncertain);
    assert_eq!(
        f.store.tokens(&current).unwrap().access_token,
        old.access_token
    );
    assert!(
        ledger(&f)
            .iter()
            .any(|e| e["outcome"] == "transport_uncertain")
    );
}

#[tokio::test]
async fn writeahead_precedes_request_and_cancel_does_not_abort_refresh_ahead() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 100, false);
    let fake = ScriptedUpstream::start().await.unwrap();
    let mut response = ScriptedResponse::json(StatusCode::OK, Bytes::new());
    response.chunks = vec![(Duration::from_millis(200), Bytes::from_static(br#"{"access_token":"synthetic-access-new","refresh_token":"synthetic-refresh-new","expires_in":3600}"#))];
    fake.state.push(response);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let caller_gates = gates.clone();
    let store = f.store.clone();
    let obs = f.obs.clone();
    let id = f.account.id.clone();
    let caller = tokio::spawn(async move { caller_gates.refresh_ahead(store, obs, id).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
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
            if f.store.tokens(&a).unwrap().access_token == "synthetic-access-new" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
    assert_eq!(fake.state.requests().len(), 1);
    assert_eq!(
        ledger(&f)
            .iter()
            .filter(|e| e["event"] == "token_refresh")
            .count(),
        1
    );
}

#[tokio::test]
async fn linked_expiry_still_records_stale_and_repairs_legacy_health() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, -1, Provider::Codex);
    let gates = f.gates();
    let current = gates
        .reload_linked(
            f.store.clone(),
            f.obs.clone(),
            f.account.id.clone(),
            "before_poll",
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
    assert!(
        ledger(&f)
            .iter()
            .any(|e| e["event"] == "linked_reload" && e["outcome"] == "link_stale")
    );
}

#[tokio::test]
async fn writeahead_persist_failure_never_consumes_refresh_token() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 100, false);
    let fake = ScriptedUpstream::start().await.unwrap();
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let path = f
        ._root
        .path()
        .join("accounts")
        .join(format!("{}.json", f.account.id));
    let permissions = fs::metadata(&path).unwrap().permissions();
    let mut readonly = permissions.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&path, readonly).unwrap();
    let result = gates
        .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await;
    fs::set_permissions(&path, permissions).unwrap();
    assert!(result.is_err());
    assert!(fake.state.requests().is_empty());
    assert!(
        !f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
}

#[tokio::test]
async fn shutdown_waits_for_cancelled_callers_detached_rotation_and_blocks_new_attempts() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let account = set_lifetime(&f, 100, false);
    let old = f.store.tokens(&account).unwrap();
    let fake = ScriptedUpstream::start().await.unwrap();
    let mut response = success();
    response.chunks[0].0 = Duration::from_millis(200);
    fake.state.push(response);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let caller_gates = gates.clone();
    let store = f.store.clone();
    let obs = f.obs.clone();
    let id = account.id.clone();
    let caller = tokio::spawn(async move { caller_gates.refresh_ahead(store, obs, id).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    gates.shutdown_refreshes().await.unwrap();
    let persisted = f.store.load(&f.account.id).unwrap();
    assert_eq!(
        f.store.tokens(&persisted).unwrap().access_token,
        "synthetic-access-new"
    );
    assert!(!persisted.token_meta.refresh_uncertain);
    assert!(
        gates
            .after_401(f.store.clone(), f.obs.clone(), persisted, &old)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), account.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fake.state.requests().len(), 1);
    // Shutdown is idempotent and cannot admit another token-consuming task.
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn shutdown_retains_lease_on_store_failure_and_persists_on_retry() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 100, false);
    let fake = ScriptedUpstream::start().await.unwrap();
    let mut response = success();
    response.chunks[0].0 = Duration::from_millis(200);
    fake.state.push(response);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let caller_gates = gates.clone();
    let store = f.store.clone();
    let obs = f.obs.clone();
    let id = f.account.id.clone();
    let caller = tokio::spawn(async move { caller_gates.refresh_ahead(store, obs, id).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let path = f
        ._root
        .path()
        .join("accounts")
        .join(format!("{}.json", f.account.id));
    let permissions = fs::metadata(&path).unwrap().permissions();
    let mut readonly = permissions.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&path, readonly).unwrap();
    let refresh_result = caller.await;
    let shutdown_result =
        tokio::time::timeout(Duration::from_secs(3), gates.shutdown_refreshes()).await;
    // Restore before assertions so even failures leave the fixture disposable.
    fs::set_permissions(&path, permissions).unwrap();
    assert!(refresh_result.unwrap().unwrap().is_none());
    assert!(shutdown_result.unwrap().is_err());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let durable = f.store.load(&f.account.id).unwrap();
    assert_eq!(
        f.store.tokens(&durable).unwrap().access_token,
        "synthetic-access-old"
    );
    assert!(durable.token_meta.refresh_uncertain);
    // The caller must keep the lease on disk failure. A later shutdown attempt
    // persists the retained rotation after recovery, rather than losing it.
    gates.shutdown_refreshes().await.unwrap();
    let durable = f.store.load(&f.account.id).unwrap();
    assert_eq!(
        f.store.tokens(&durable).unwrap().access_token,
        "synthetic-access-new"
    );
    assert!(!durable.token_meta.refresh_uncertain);
    assert_eq!(fake.state.requests().len(), 1);
    assert!(
        ledger(&f)
            .iter()
            .any(|e| e["event"] == "core_warning" && e["code"] == "refresh_shutdown_store_error")
    );
}

#[tokio::test]
async fn paused_and_account_drained_skip_ahead_but_current_poll_401_still_refreshes() {
    let _serial = SERIAL.lock().await;
    for paused in [true, false] {
        let f = Fixture::new(false, 60, Provider::Codex);
        set_lifetime(&f, 100, false);
        f.store
            .update_settings(&f.account.id, Some(!paused), None, |a| {
                a.drain = !paused;
                a.user_state = if paused {
                    UserState::Paused
                } else {
                    UserState::Active
                };
            })
            .unwrap();
        let current = f.store.load(&f.account.id).unwrap();
        let old = f.store.tokens(&current).unwrap();
        let fake = ScriptedUpstream::start().await.unwrap();
        let gates = f
            .gates()
            .with_token_endpoint(Provider::Codex, fake.url.clone());
        assert!(
            gates
                .refresh_ahead(f.store.clone(), f.obs.clone(), current.id.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert!(fake.state.requests().is_empty());
        fake.state.push(success());
        assert!(
            gates
                .after_401(f.store.clone(), f.obs.clone(), current, &old)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(fake.state.requests().len(), 1);
        gates.shutdown_refreshes().await.unwrap();
    }
}

#[tokio::test]
async fn known_transient_error_breaks_unknown_4xx_streak() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 100, false);
    let fake = ScriptedUpstream::start().await.unwrap();
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    for (status, delay) in [
        (StatusCode::BAD_REQUEST, 0),
        (StatusCode::BAD_REQUEST, 2100),
        (StatusCode::INTERNAL_SERVER_ERROR, 4100),
        (StatusCode::BAD_REQUEST, 8100),
    ] {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        fake.state.push(ScriptedResponse::json(
            status,
            r#"{"error":"synthetic_unknown"}"#,
        ));
        assert!(
            gates
                .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            f.runtime.views().await.unwrap()[0]["refresh_attention"],
            false
        );
    }
    assert_eq!(fake.state.requests().len(), 4);
    assert!(
        !ledger(&f)
            .iter()
            .any(|e| e["event"] == "core_warning" && e["code"] == "refresh_unknown_4xx")
    );
    gates.shutdown_refreshes().await.unwrap();
}
