#![cfg(all(feature = "testkit", debug_assertions))]

// Compile the bounded internal credential layer against the production modules.
pub use qb_core::{account_runtime, accounts, observability};
#[path = "support/unscoped_tasks.rs"]
mod boxes;
#[path = "../src/relay/credentials.rs"]
#[allow(dead_code)] // This isolated harness exercises a subset of the shared production gate.
mod credentials;

use accounts::{Account, AccountStore, Health, HealthReason, Provider, Source, UserState, import};
use axum::http::StatusCode;
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, Utc};
use credentials::{ManualRefreshError, ManualRefreshOutcome, ManualRefreshTerminal, TokenGates};
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
        assert!(
            ["synthetic-access-old", "synthetic-access-new"]
                .contains(&first.unwrap().1.access_token.as_str())
        );
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
        gates.shutdown_refreshes().await.unwrap();
    }
}

#[tokio::test]
async fn crash_uncertain_after_ahead_waits_for_expiry_margin_or_current401() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let fake = ScriptedUpstream::start().await.unwrap();
    // 12 minutes left of a 2-hour token: after ahead, before expiry-minus10.
    let account = set_long_lifetime(&f, 720, true);
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
            .refresh_ahead(f.store.clone(), f.obs.clone(), account.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(fake.state.requests().is_empty());
    // A new gate models a restart after the old ahead boundary. No replay.
    let restarted = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    assert!(
        restarted
            .refresh_ahead(f.store.clone(), f.obs.clone(), account.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(fake.state.requests().is_empty());
    fake.state.push(success());
    assert!(
        restarted
            .after_401(f.store.clone(), f.obs.clone(), account, &old)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(fake.state.requests().len(), 1);
    gates.shutdown_refreshes().await.unwrap();
    restarted.shutdown_refreshes().await.unwrap();
    drop(gates);
    drop(restarted);
    drop(f);
    // A separate account/gate proves the expiry margin trigger without a 401.
    let f = Fixture::new(false, 60, Provider::Codex);
    set_long_lifetime(&f, 599, true);
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
    assert_eq!(fake.state.requests().len(), 2);
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
        assert!(
            !f.store
                .load(&account.id)
                .unwrap()
                .token_meta
                .refresh_uncertain
        );
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
            .any(|e| e["event"] == "token_refresh" && e["outcome"] == "transport_connect")
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
    // The refresh task records its ledger event after persisting the tokens,
    // so wait for it instead of reading the ledger at once.
    let mut refresh_events = 0;
    for _ in 0..300 {
        refresh_events = ledger(&f)
            .iter()
            .filter(|e| e["event"] == "token_refresh")
            .count();
        if refresh_events > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(refresh_events, 1);
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
    let request = tokio::time::timeout(
        Duration::from_millis(300),
        gates.before_request(f.store.clone(), f.obs.clone(), f.account.id.clone()),
    )
    .await;
    gates.shutdown_refreshes().await.unwrap();
    fs::set_permissions(&path, permissions).unwrap();
    assert_eq!(
        request.unwrap().unwrap().1.access_token,
        "synthetic-access-old"
    );
    assert!(result.unwrap().is_none());
    assert_eq!(
        f.runtime.views().await.unwrap()[0]["refresh_attention"],
        true
    );
    assert_eq!(f.store.load(&f.account.id).unwrap().health, Health::Ready);
    assert_eq!(
        gates
            .before_request(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .1
            .access_token,
        "synthetic-access-old"
    );
    gates.shutdown_refreshes().await.unwrap();
    assert!(
        ledger(&f)
            .iter()
            .any(|e| e["outcome"] == "store_error_writeahead")
    );
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
async fn account_drain_skips_ahead_but_current_poll_401_still_refreshes() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 100, false);
    let current = f
        .store
        .update_metadata(&f.account.id, |a| a.drain = true)
        .unwrap();
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

fn set_long_lifetime(f: &Fixture, remaining: i64, uncertain: bool) -> Account {
    set_lifetime(f, remaining, uncertain);
    let path = f
        ._root
        .path()
        .join("accounts")
        .join(format!("{}.json", f.account.id));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["token_meta"]["last_refresh_at"] =
        json!(Utc::now() + ChronoDuration::seconds(remaining - 7200));
    fs::write(path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    f.store.load(&f.account.id).unwrap()
}

#[tokio::test]
async fn runtime_uncertain_after_ahead_waits_for_expiry_margin() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_long_lifetime(&f, 720, false);
    let fake = ScriptedUpstream::start().await.unwrap();
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    fake.state
        .push(ScriptedResponse::json(StatusCode::OK, "truncated-json"));
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fake.state.requests().len(), 1);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(120)).await;
    tokio::time::resume();
    set_long_lifetime(&f, 599, true);
    fake.state.push(success());
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(fake.state.requests().len(), 2);
}

#[tokio::test]
async fn request_serves_current_access_during_slow_ahead_refresh() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 100, false);
    let fake = ScriptedUpstream::start().await.unwrap();
    let mut response = success();
    response.chunks[0].0 = Duration::from_millis(500);
    fake.state.push(response);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let first = tokio::time::timeout(
        Duration::from_millis(300),
        gates.before_request(f.store.clone(), f.obs.clone(), f.account.id.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.1.access_token, "synthetic-access-old");
    tokio::time::timeout(Duration::from_secs(3), async {
        while fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let second = tokio::time::timeout(
        Duration::from_millis(100),
        gates.before_request(f.store.clone(), f.obs.clone(), f.account.id.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.1.access_token, "synthetic-access-old");
    gates.shutdown_refreshes().await.unwrap();
    assert_eq!(fake.state.requests().len(), 1);
}

#[tokio::test]
async fn old401_during_pending_rotation_never_changes_auth_health() {
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
    let caller = {
        let gates = gates.clone();
        let store = f.store.clone();
        let obs = f.obs.clone();
        let id = account.id.clone();
        tokio::spawn(async move { gates.refresh_ahead(store, obs, id).await })
    };
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
        .join(format!("{}.json", account.id));
    let permissions = fs::metadata(&path).unwrap().permissions();
    let mut readonly = permissions.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&path, readonly).unwrap();
    caller.await.unwrap().unwrap();
    // Restore so an erroneous health write would succeed; the retry has a 1s delay.
    fs::set_permissions(&path, permissions).unwrap();
    assert!(
        !gates
            .is_current(f.store.clone(), &account, &old)
            .await
            .unwrap()
    );
    assert!(
        gates
            .after_401(f.store.clone(), f.obs.clone(), account.clone(), &old)
            .await
            .unwrap()
            .is_none()
    );
    gates
        .revoke_if_current(f.store.clone(), &account, &old)
        .await
        .unwrap();
    gates
        .auth_failed(f.store.clone(), f.obs.clone(), account.clone(), &old)
        .await
        .unwrap();
    gates
        .cool_unavailable_auth(f.store.clone(), &account, &old)
        .await
        .unwrap();
    assert!(!matches!(
        f.store.load(&account.id).unwrap().health,
        Health::NeedsRelogin { .. }
    ));
    gates.shutdown_refreshes().await.unwrap();
    let current = f.store.load(&account.id).unwrap();
    assert_eq!(current.health, Health::Ready);
    assert_eq!(
        f.store.tokens(&current).unwrap().access_token,
        "synthetic-access-new"
    );
}

#[tokio::test]
async fn deleted_pending_rotation_is_superseded_and_shutdown_finishes() {
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
    let caller = {
        let gates = gates.clone();
        let store = f.store.clone();
        let obs = f.obs.clone();
        let id = f.account.id.clone();
        tokio::spawn(async move { gates.refresh_ahead(store, obs, id).await })
    };
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
    caller.await.unwrap().unwrap();
    fs::set_permissions(&path, permissions).unwrap();
    fs::remove_file(&path).unwrap();
    tokio::time::timeout(Duration::from_secs(3), gates.shutdown_refreshes())
        .await
        .unwrap()
        .unwrap();
    assert!(ledger(&f).iter().any(|e| e["outcome"] == "superseded"));
    assert!(!path.exists());
}

#[tokio::test]
async fn error_type_terminal_clears_uncertainty() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Claude);
    set_lifetime(&f, 100, false);
    let fake = ScriptedUpstream::start().await.unwrap();
    fake.state.push(ScriptedResponse::json(
        StatusCode::BAD_REQUEST,
        r#"{"error":{"type":"invalid_grant"}}"#,
    ));
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Claude, fake.url.clone());
    gates
        .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    let current = f.store.load(&f.account.id).unwrap();
    assert!(!current.token_meta.refresh_uncertain);
    assert!(matches!(current.health,Health::NeedsRelogin {reason,..} if reason=="invalid_grant"));
}

#[tokio::test]
async fn notdue_scheduler_preflight_does_not_decrypt_tokens() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, 250, false);
    let path = f
        ._root
        .path()
        .join("accounts")
        .join(format!("{}.json", f.account.id));
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["secret_dpapi_b64"] = json!("synthetic-invalid-ciphertext");
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(
        f.store
            .tokens(&f.store.load(&f.account.id).unwrap())
            .is_err()
    );
    let gates = f.gates();
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn writeahead_failure_cools_only_when_access_cannot_serve() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_lifetime(&f, -1, false);
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
    assert!(result.unwrap().is_none());
    assert!(fake.state.requests().is_empty());
    assert_eq!(
        f.runtime.views().await.unwrap()[0]["health"]["reason"],
        "store_error"
    );
    fake.state.push(success());
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_some()
    );
    assert!(!f.runtime.blocked_in_memory(&f.account.id));
    assert_eq!(
        f.runtime.views().await.unwrap()[0]["health"]["kind"],
        "ready"
    );
    assert_eq!(
        f.runtime.views().await.unwrap()[0]["refresh_attention"],
        false
    );
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn connect_failure_clears_uncertainty_and_uses_regular_backoff() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_long_lifetime(&f, 720, false);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, format!("http://{address}/token"));
    gates
        .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    assert!(
        !f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
    assert!(
        ledger(&f)
            .iter()
            .any(|e| e["outcome"] == "transport_connect")
    );
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn not_sent_retry_does_not_clear_uncertainty_from_earlier_attempt() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let account = set_long_lifetime(&f, 720, true);
    let old = f.store.tokens(&account).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, format!("http://{address}/token"));
    gates
        .after_401(f.store.clone(), f.obs.clone(), account, &old)
        .await
        .unwrap();
    assert!(
        f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        ledger(&f)
            .iter()
            .filter(|e| e["event"] == "token_refresh")
            .count(),
        1
    );
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn uncertain_near_expiry_spaces_proactive_attempts_but_current401_overrides() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_long_lifetime(&f, 599, true);
    let fake = ScriptedUpstream::start().await.unwrap();
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    for _ in 0..5 {
        fake.state
            .push(ScriptedResponse::json(StatusCode::OK, "truncated-json"));
    }
    gates
        .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    assert_eq!(fake.state.requests().len(), 1);
    for _ in 0..3 {
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
            .await
            .unwrap();
    }
    assert_eq!(
        fake.state.requests().len(),
        1,
        "proactive replay within two minutes"
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(119)).await;
    tokio::time::resume();
    gates
        .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    assert_eq!(fake.state.requests().len(), 1);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    gates
        .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    assert_eq!(fake.state.requests().len(), 2);
    let (a, t) = gates
        .before_request(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    gates
        .after_401(f.store.clone(), f.obs.clone(), a, &t)
        .await
        .unwrap();
    assert_eq!(
        fake.state.requests().len(),
        3,
        "new current-token 401 bypasses uncertain spacing"
    );
    gates
        .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
        .await
        .unwrap();
    assert_eq!(fake.state.requests().len(), 3);
    gates.shutdown_refreshes().await.unwrap();
}

// D17 review 7-11. All accounts, CPA files and endpoint replies are synthetic.
fn synthetic_adopt_from_current_store(f: &Fixture) -> Account {
    // Source is not mutable through update_metadata. Save the SAME current
    // credential snapshot when establishing adopted fixture ownership.
    let mut current = f.store.load(&f.account.id).unwrap();
    let tokens = f.store.tokens(&current).unwrap();
    if let Source::Cpa { mode, .. } = &mut current.source {
        *mode = accounts::ImportMode::Adopted;
    }
    f.store
        .save(&current, &tokens, Some(current.generation))
        .unwrap()
}

async fn wait_for_refresh_request(fake: &ScriptedUpstream) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn scheduler_refreshes_disabled_and_paused_owned_accounts_but_not_drain_or_needs_relogin() {
    let _serial = SERIAL.lock().await;
    for adopted in [false, true] {
        for state in [UserState::Paused, UserState::Disabled] {
            for blocked in [0, 1, 2] {
                let f = Fixture::new(adopted, 60, Provider::Codex);
                set_lifetime(&f, 100, false);
                if adopted {
                    synthetic_adopt_from_current_store(&f);
                }
                f.store
                    .update_metadata(&f.account.id, |a| {
                        a.user_state = state;
                        a.drain = blocked == 1;
                        if blocked == 2 {
                            a.apply_health_signal(
                                accounts::HealthSignal::TerminalRefresh {
                                    reason: "invalid_grant".into(),
                                },
                                Utc::now(),
                            );
                        }
                    })
                    .unwrap();
                let fake = ScriptedUpstream::start().await.unwrap();
                fake.state.push(success());
                let gates = f
                    .gates()
                    .with_token_endpoint(Provider::Codex, fake.url.clone());
                let task = gates.start_refresh_scheduler(f.store.clone(), f.obs.clone());
                if blocked == 0 {
                    wait_for_refresh_request(&fake).await;
                } else {
                    assert!(
                        gates
                            .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
                            .await
                            .unwrap()
                            .is_none()
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                gates.shutdown_refreshes().await.unwrap();
                task.await.unwrap();
                assert_eq!(fake.state.requests().len(), usize::from(blocked == 0));
                let current = f.store.load(&f.account.id).unwrap();
                assert_eq!(current.user_state, state);
                assert_eq!(
                    f.store.tokens(&current).unwrap().access_token,
                    if blocked == 0 {
                        "synthetic-access-new"
                    } else {
                        "synthetic-access-old"
                    }
                );
            }
        }
    }
}

#[tokio::test]
async fn manual_refresh_shares_singleflight_with_ahead() {
    let _serial = SERIAL.lock().await;
    for (manual_first, same_access) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let f = Fixture::new(false, 60, Provider::Codex);
        set_lifetime(&f, 100, false);
        let fake = ScriptedUpstream::start().await.unwrap();
        let mut response = if same_access {
            ScriptedResponse::json(
                StatusCode::OK,
                r#"{"access_token":"synthetic-access-old","expires_in":3600}"#,
            )
        } else {
            success()
        };
        response.chunks[0].0 = Duration::from_millis(300);
        fake.state.push(response);
        let gates = f
            .gates()
            .with_token_endpoint(Provider::Codex, fake.url.clone());
        if manual_first {
            let caller = {
                let (g, s, o, id) = (
                    gates.clone(),
                    f.store.clone(),
                    f.obs.clone(),
                    f.account.id.clone(),
                );
                tokio::spawn(async move { g.manual_refresh(s, o, id, false).await })
            };
            wait_for_refresh_request(&fake).await;
            gates
                .refresh_ahead(f.store.clone(), f.obs.clone(), f.account.id.clone())
                .await
                .unwrap();
            assert_eq!(
                caller.await.unwrap().unwrap().outcome,
                ManualRefreshOutcome::Success
            );
        } else {
            let caller = {
                let (g, s, o, id) = (
                    gates.clone(),
                    f.store.clone(),
                    f.obs.clone(),
                    f.account.id.clone(),
                );
                tokio::spawn(async move { g.refresh_ahead(s, o, id).await })
            };
            wait_for_refresh_request(&fake).await;
            let summary = gates
                .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), false)
                .await
                .unwrap();
            let (current, _) = caller.await.unwrap().unwrap().unwrap();
            assert_eq!(summary.outcome, ManualRefreshOutcome::Success);
            assert_eq!(summary.last_refresh_at, current.token_meta.last_refresh_at);
            assert_eq!(summary.token_expires_at, current.token_meta.expires_at);
            let json = serde_json::to_value(&summary).unwrap();
            assert_eq!(json.as_object().unwrap().len(), 3);
            assert_eq!(json["outcome"], "success");
            assert!(!json.to_string().contains("synthetic-"));
        }
        assert_eq!(fake.state.requests().len(), 1);
        if same_access {
            // A later explicit request still refreshes: only the overlapping
            // caller shares a completion newer than its captured baseline.
            fake.state.push(success());
            gates
                .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), false)
                .await
                .unwrap();
            assert_eq!(fake.state.requests().len(), 2);
        }
        gates.shutdown_refreshes().await.unwrap();
    }
}

#[tokio::test]
async fn manual_refresh_refused_for_linked_drain_needs_relogin_and_uncertain_without_force() {
    let _serial = SERIAL.lock().await;
    for (case, expected) in [
        (0, ManualRefreshError::Linked),
        (1, ManualRefreshError::Drain),
        (2, ManualRefreshError::NeedsRelogin),
        (3, ManualRefreshError::Uncertain),
        (4, ManualRefreshError::NotFound),
        (5, ManualRefreshError::Unavailable),
    ] {
        let f = Fixture::new(case == 0, 60, Provider::Codex);
        set_long_lifetime(&f, 3600, case == 3);
        f.store
            .update_metadata(&f.account.id, |a| {
                a.drain = case == 1;
                if case == 2 {
                    a.apply_health_signal(
                        accounts::HealthSignal::TerminalRefresh {
                            reason: "invalid_grant".into(),
                        },
                        Utc::now(),
                    );
                }
            })
            .unwrap();
        f.newer_link(); // Manual linked refusal must never reload this newer file.
        let before = f.store.load(&f.account.id).unwrap();
        let fake = ScriptedUpstream::start().await.unwrap();
        let gates = f
            .gates()
            .with_token_endpoint(Provider::Codex, fake.url.clone());
        if case == 5 {
            gates.shutdown_refreshes().await.unwrap();
        }
        let id = if case == 4 {
            "cx-000000".into()
        } else {
            f.account.id.clone()
        };
        assert_eq!(
            gates
                .manual_refresh(f.store.clone(), f.obs.clone(), id, false)
                .await
                .unwrap_err(),
            expected
        );
        if case < 3 {
            assert_eq!(
                gates
                    .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), true)
                    .await
                    .unwrap_err(),
                expected
            );
        }
        let current = f.store.load(&f.account.id).unwrap();
        assert_eq!(current.generation, before.generation);
        assert_eq!(
            f.store.tokens(&current).unwrap().access_token,
            "synthetic-access-old"
        );
        assert!(fake.state.requests().is_empty());
        gates.shutdown_refreshes().await.unwrap();
    }
}

#[tokio::test]
async fn manual_refresh_force_presents_uncertain_token_once() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_long_lifetime(&f, 3600, true);
    let fake = ScriptedUpstream::start().await.unwrap();
    let mut response = ScriptedResponse::json(StatusCode::OK, "synthetic-truncated-json");
    response.chunks[0].0 = Duration::from_millis(300);
    fake.state.push(response);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let caller = {
        let (g, s, o, id) = (
            gates.clone(),
            f.store.clone(),
            f.obs.clone(),
            f.account.id.clone(),
        );
        tokio::spawn(async move { g.manual_refresh(s, o, id, true).await })
    };
    wait_for_refresh_request(&fake).await;
    assert!(
        f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
    assert_eq!(
        gates
            .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), true)
            .await
            .unwrap_err(),
        ManualRefreshError::Unavailable
    );
    assert_eq!(
        caller.await.unwrap().unwrap_err(),
        ManualRefreshError::Unavailable
    );
    assert_eq!(
        gates
            .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), true)
            .await
            .unwrap_err(),
        ManualRefreshError::Unavailable
    );
    assert_eq!(fake.state.requests().len(), 1);
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn manual_force_preserves_known_transient_backoff() {
    let _serial = SERIAL.lock().await;
    for status in [
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let f = Fixture::new(false, 60, Provider::Codex);
        set_long_lifetime(&f, 3600, true);
        let fake = ScriptedUpstream::start().await.unwrap();
        fake.state
            .push(ScriptedResponse::json(status, "synthetic-provider-error"));
        let gates = f
            .gates()
            .with_token_endpoint(Provider::Codex, fake.url.clone());
        for _ in 0..2 {
            assert_eq!(
                gates
                    .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), true)
                    .await
                    .unwrap_err(),
                ManualRefreshError::Unavailable
            );
        }
        assert_eq!(fake.state.requests().len(), 1);
        gates.shutdown_refreshes().await.unwrap();
    }
}

#[tokio::test]
async fn manual_uncertain_inside_expiry_margin_needs_no_force() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_long_lifetime(&f, 599, true);
    let fake = ScriptedUpstream::start().await.unwrap();
    fake.state.push(success());
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    assert_eq!(
        gates
            .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), false)
            .await
            .unwrap()
            .outcome,
        ManualRefreshOutcome::Success
    );
    assert_eq!(fake.state.requests().len(), 1);
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn manual_refresh_terminal_reply_sets_needs_relogin_and_reports_outcome() {
    let _serial = SERIAL.lock().await;
    for (status, code, outcome) in [
        (
            StatusCode::BAD_REQUEST,
            "refresh_token_invalidated",
            ManualRefreshTerminal::RefreshTokenInvalidated,
        ),
        (
            StatusCode::BAD_REQUEST,
            "refresh_token_reused",
            ManualRefreshTerminal::RefreshTokenReused,
        ),
        (
            StatusCode::BAD_REQUEST,
            "refresh_token_expired",
            ManualRefreshTerminal::RefreshTokenExpired,
        ),
        (
            StatusCode::BAD_REQUEST,
            "refresh_token_revoked",
            ManualRefreshTerminal::RefreshTokenRevoked,
        ),
        (
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            ManualRefreshTerminal::InvalidGrant,
        ),
        (
            StatusCode::UNAUTHORIZED,
            "synthetic-provider-secret",
            ManualRefreshTerminal::TokenEndpoint401,
        ),
    ] {
        let f = Fixture::new(false, 60, Provider::Claude);
        let fake = ScriptedUpstream::start().await.unwrap();
        fake.state.push(ScriptedResponse::json(
            status,
            serde_json::to_string(&json!({"error":code,"message":"synthetic-provider-secret"}))
                .unwrap(),
        ));
        let gates = f
            .gates()
            .with_token_endpoint(Provider::Claude, fake.url.clone());
        let error = gates
            .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), false)
            .await
            .unwrap_err();
        assert_eq!(error, ManualRefreshError::Terminal { outcome });
        assert!(!error.to_string().contains("synthetic-"));
        let report = serde_json::to_string(&error).unwrap();
        assert!(report.contains("terminal"));
        assert!(!report.contains("synthetic-"));
        let current = f.store.load(&f.account.id).unwrap();
        assert!(matches!(current.health, Health::NeedsRelogin { .. }));
        assert!(!current.token_meta.refresh_uncertain);
        assert_eq!(
            gates
                .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), true)
                .await
                .unwrap_err(),
            ManualRefreshError::NeedsRelogin
        );
        assert_eq!(fake.state.requests().len(), 1);
        gates.shutdown_refreshes().await.unwrap();
    }
}

#[tokio::test]
async fn adopted_claude_401_after_refresh_follows_owned_path_and_without_rotation_keeps_old_rt() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(true, 60, Provider::Claude);
    let current = synthetic_adopt_from_current_store(&f);
    let old = f.store.tokens(&current).unwrap();
    f.newer_link(); // Adopted refresh must ignore this CPA copy.
    let fake = ScriptedUpstream::start().await.unwrap();
    fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        r#"{"access_token":"synthetic-access-owned","expires_in":3600}"#,
    ));
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Claude, fake.url.clone());
    let (updated, tokens) = gates
        .after_401(f.store.clone(), f.obs.clone(), current, &old)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tokens.access_token, "synthetic-access-owned");
    assert_eq!(tokens.refresh_token, "synthetic-refresh-old");
    let durable = f
        .store
        .tokens(&f.store.load(&f.account.id).unwrap())
        .unwrap();
    assert_eq!(durable.refresh_token, old.refresh_token);
    gates
        .auth_failed(f.store.clone(), f.obs.clone(), updated, &tokens)
        .await
        .unwrap();
    let current = f.store.load(&f.account.id).unwrap();
    assert!(matches!(
        current.health,
        Health::Cooling {
            reason: HealthReason::Auth,
            ..
        }
    ));
    assert_eq!(fake.state.requests().len(), 1);
    assert!(!ledger(&f).iter().any(|e| e["outcome"] == "link_stale"));
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn manual_refresh_runs_before_due_for_paused_and_disabled_accounts() {
    let _serial = SERIAL.lock().await;
    for state in [UserState::Paused, UserState::Disabled] {
        let f = Fixture::new(false, 60, Provider::Codex);
        set_lifetime(&f, 900, false); // Before the refresh-ahead boundary.
        f.store
            .update_metadata(&f.account.id, |a| a.user_state = state)
            .unwrap();
        let fake = ScriptedUpstream::start().await.unwrap();
        fake.state.push(success());
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
        assert!(fake.state.requests().is_empty());
        gates
            .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), false)
            .await
            .unwrap();
        assert_eq!(f.store.load(&f.account.id).unwrap().user_state, state);
        assert_eq!(fake.state.requests().len(), 1);
        gates.shutdown_refreshes().await.unwrap();
    }
}

#[tokio::test]
async fn manual_force_never_bypasses_durable_writeahead() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    set_long_lifetime(&f, 3600, true);
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
        .manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), true)
        .await;
    fs::set_permissions(&path, permissions).unwrap();
    assert_eq!(result.unwrap_err(), ManualRefreshError::Unavailable);
    assert!(fake.state.requests().is_empty());
    assert!(
        f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
    gates.shutdown_refreshes().await.unwrap();
}

#[tokio::test]
async fn cancelled_manual_keeps_detached_rotation_and_core_stop_excludes_all_refresh_triggers() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let before = set_lifetime(&f, 900, false);
    let old = f.store.tokens(&before).unwrap();
    let fake = ScriptedUpstream::start().await.unwrap();
    let mut response = success();
    response.chunks[0].0 = Duration::from_millis(300);
    fake.state.push(response);
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let caller = {
        let (g, s, o, id) = (
            gates.clone(),
            f.store.clone(),
            f.obs.clone(),
            f.account.id.clone(),
        );
        tokio::spawn(async move { g.manual_refresh(s, o, id, false).await })
    };
    wait_for_refresh_request(&fake).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    gates.shutdown_refreshes().await.unwrap();
    let current = f.store.load(&f.account.id).unwrap();
    assert_eq!(
        f.store.tokens(&current).unwrap().access_token,
        "synthetic-access-new"
    );
    assert!(!current.token_meta.refresh_uncertain);
    // Core lifecycle is stricter than the separate per-account drain rule.
    assert!(
        gates
            .after_401(f.store.clone(), f.obs.clone(), before, &old)
            .await
            .unwrap()
            .is_none()
    );
    let current_tokens = f.store.tokens(&current).unwrap();
    assert!(
        gates
            .after_401(
                f.store.clone(),
                f.obs.clone(),
                current.clone(),
                &current_tokens
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        gates
            .refresh_ahead(f.store.clone(), f.obs.clone(), current.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        gates
            .manual_refresh(f.store.clone(), f.obs.clone(), current.id, true)
            .await
            .unwrap_err(),
        ManualRefreshError::Unavailable
    );
    assert_eq!(fake.state.requests().len(), 1);
}

#[tokio::test]
async fn cancelled_manual_preflight_keeps_shutdown_waiting_for_blocking_credential_read() {
    use std::{future::Future, task::Poll};

    let _serial = SERIAL.lock().await;
    let f = Fixture::new(false, 60, Provider::Codex);
    let fake = ScriptedUpstream::start().await.unwrap();
    let gates = f
        .gates()
        .with_token_endpoint(Provider::Codex, fake.url.clone());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered_tx = std::sync::Mutex::new(Some(entered_tx));
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    f.store.set_observer(Arc::new(move |_, _| {
        if let Some(entered_tx) = entered_tx.lock().unwrap().take() {
            let _ = entered_tx.send(());
            // Observer runs under AccountStore's transaction gate. Bound the
            // fixture barrier so a failed assertion cannot strand a worker.
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
    }));
    let writer = {
        let store = f.store.clone();
        let id = f.account.id.clone();
        tokio::task::spawn_blocking(move || {
            store.update_metadata(&id, |a| a.user_state = UserState::Paused)
        })
    };
    tokio::time::timeout(Duration::from_secs(3), entered_rx)
        .await
        .unwrap()
        .unwrap();

    // Poll exactly once: admit lifecycle work and submit spawn_blocking while
    // the store gate is held. No sleeps or worker scheduling guesses are needed.
    let mut manual =
        Box::pin(gates.manual_refresh(f.store.clone(), f.obs.clone(), f.account.id.clone(), false));
    let preflight_pending =
        std::future::poll_fn(|cx| Poll::Ready(manual.as_mut().poll(cx).is_pending())).await;
    drop(manual); // Cancel before the blocking preflight can read credentials.

    let mut shutdown = Box::pin(gates.shutdown_refreshes());
    let shutdown_pending =
        std::future::poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx).is_pending())).await;
    // Always release the fixture barrier before asserting the observed states.
    release_tx.send(()).unwrap();
    writer.await.unwrap().unwrap();
    assert!(preflight_pending);
    assert!(
        shutdown_pending,
        "shutdown forgot the cancelled blocking preflight"
    );
    tokio::time::timeout(Duration::from_secs(3), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(fake.state.requests().is_empty());
    assert!(
        !f.store
            .load(&f.account.id)
            .unwrap()
            .token_meta
            .refresh_uncertain
    );
}
