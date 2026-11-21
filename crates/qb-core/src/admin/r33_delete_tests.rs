//! R33 `DELETE /admin/v1/accounts/{id}` through the real admin router, account
//! store and runtime. Synthetic identities and tokens only; nothing here reaches a
//! provider, and no reset credit is touched.
use super::*;
use crate::{
    accounts::{Account, DeleteError, Identity, Provider, StoreError, Tokens},
    debug_pin::DebugPins,
    observability::Observability,
};
use futures_util::StreamExt;
use std::time::Duration;

const ADMIN: &str = "synthetic-r33-admin";

fn account(uuid: &str, label: &str, expires_in_secs: i64) -> Account {
    let identity = Identity::Claude {
        account_uuid: uuid.into(),
        organization_uuid: None,
        email: format!("{uuid}@example.invalid"),
    };
    let now = chrono::Utc::now();
    serde_json::from_value(serde_json::json!({
        "schema":1,"id":identity.id(),"provider":"claude","label":label,
        "email":format!("{uuid}@example.invalid"),"identity":identity,"plan":"max_5x",
        "source":{"kind":"oauth"},"user_state":"active","health":{"kind":"ready"},
        "generation":0,
        "token_meta":{
            "expires_at": now + chrono::Duration::seconds(expires_in_secs),
            "last_refresh_at": now + chrono::Duration::seconds(expires_in_secs - 1000),
            "refresh_uncertain": false
        }
    }))
    .unwrap()
}

fn tokens() -> Tokens {
    Tokens {
        access_token: "synthetic-r33-access".into(),
        refresh_token: "synthetic-r33-refresh".into(),
        id_token: None,
    }
}

struct Harness {
    store: Arc<AccountStore>,
    obs: Arc<Observability>,
    relay: Arc<crate::relay::RelayState>,
    servers: crate::http::RunningServers,
    client: reqwest::Client,
    base: String,
    doomed: Account,
    kept: Account,
    home: tempfile::TempDir,
}

impl Harness {
    async fn new(expires_in_secs: i64) -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r33/delete-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let home = qb_testkit::fixture::home_in(base).unwrap();
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let doomed = store
            .save(
                &account("synthetic-r33-doomed", "Doomed", expires_in_secs),
                &tokens(),
                None,
            )
            .unwrap();
        let kept = store
            .save(
                &account("synthetic-r33-kept", "Kept", 3600),
                &tokens(),
                None,
            )
            .unwrap();
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let mut config = crate::config::Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        let relay = Arc::new(
            crate::relay::RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap(),
        );
        let servers = crate::http::spawn_with_admin(
            &config,
            crate::local_secrets::LocalSecrets {
                client_key: "synthetic-r33-client".into(),
                admin_token: ADMIN.into(),
            },
            runtime_routes(relay.clone(), 0),
        )
        .await
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let base = format!("http://{}/admin/v1", servers.admin_addr);
        Self {
            store,
            obs,
            relay,
            servers,
            client,
            base,
            doomed,
            kept,
            home,
        }
    }

    fn delete(&self, id: &str) -> reqwest::RequestBuilder {
        self.client
            .delete(format!("{}/accounts/{id}", self.base))
            .header(reqwest::header::CONNECTION, "close")
            .bearer_auth(ADMIN)
    }

    async fn state_ids(&self) -> Vec<String> {
        let state: serde_json::Value = self
            .client
            .get(format!("{}/state", self.base))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut ids: Vec<_> = state["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    }

    fn file(&self, id: &str) -> std::path::PathBuf {
        self.home.path().join("accounts").join(format!("{id}.json"))
    }

    async fn finish(mut self) {
        self.servers.shutdown();
        tokio::time::timeout(Duration::from_secs(3), async {
            (&mut self.servers.data_task).await.unwrap().unwrap();
            (&mut self.servers.admin_task).await.unwrap().unwrap();
        })
        .await
        .unwrap();
        // Release the native owners on an ordinary thread, as the other admin fixtures do.
        std::thread::spawn(move || drop(self)).join().unwrap();
    }
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

/// Reads SSE frames until one satisfies `want`, ignoring keep-alives and other events.
async fn frame_matching(
    stream: &mut (impl futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
    want: impl Fn(&str) -> bool,
) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut buffer = Vec::new();
        loop {
            let chunk = stream.next().await.expect("SSE closed").unwrap();
            buffer.extend_from_slice(&chunk);
            while let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
                let frame = String::from_utf8(buffer.drain(..end + 2).collect()).unwrap();
                if want(&frame) {
                    return frame;
                }
            }
        }
    })
    .await
    .expect("expected SSE frame")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r33_delete_removes_the_file_drops_caches_and_emits_the_row_removal() {
    let h = Harness::new(3600).await;
    let (doomed, kept) = (h.doomed.id.clone(), h.kept.id.clone());
    // Per-account runtime caches: quota snapshot, reset-credit cache and catalog entry.
    let started = chrono::Utc::now();
    let body = serde_json::to_vec(&serde_json::json!({
        "five_hour": {"utilization": 7.0, "resets_at": started + chrono::Duration::hours(4)},
        "seven_day": {"utilization": 45.0, "resets_at": started + chrono::Duration::days(3)}
    }))
    .unwrap();
    let snapshot = crate::quota::parse_snapshot_for_test(Provider::Claude, &body, started).unwrap();
    assert!(
        h.obs
            .observe_claude_poll(&doomed, snapshot, true, started)
            .is_some()
    );
    let _ = h.relay.quota.reset_credit_cached(&doomed, Provider::Claude);
    drop(
        h.relay
            .accounts
            .catalogs
            .begin_refresh(&doomed, crate::catalog::RefreshMode::Force)
            .unwrap()
            .unwrap(),
    );
    assert!(h.obs.quota_snapshot(&doomed).is_some());
    assert!(h.relay.quota.holds_account_state(&doomed));
    assert!(h.relay.accounts.catalogs.holds(&doomed));
    assert_eq!(
        h.state_ids().await,
        sorted(vec![doomed.clone(), kept.clone()]),
        "the tray lists both accounts before the delete"
    );

    let events = h
        .client
        .get(format!("{}/events", h.base))
        .bearer_auth(ADMIN)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), StatusCode::OK);
    let mut stream = events.bytes_stream();

    let response = h.delete(&doomed).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.bytes().await.unwrap().is_empty());

    let frame = frame_matching(&mut stream, |frame| {
        frame.starts_with("event: account_changed\n") && frame.contains("\"removed\":true")
    })
    .await;
    assert!(frame.contains(&format!("\"id\":\"{doomed}\"")), "{frame}");
    assert_eq!(
        h.state_ids().await,
        vec![kept.clone()],
        "the tray row is gone"
    );
    assert!(!h.file(&doomed).exists());
    assert!(h.file(&kept).exists(), "other accounts are untouched");
    assert!(matches!(h.store.load(&doomed), Err(StoreError::NotFound)));
    assert!(h.obs.quota_snapshot(&doomed).is_none());
    assert!(!h.relay.quota.holds_account_state(&doomed));
    assert!(!h.relay.accounts.catalogs.holds(&doomed));

    // Deleting again is an ordinary 404, and the survivor keeps working.
    assert_eq!(
        h.delete(&doomed).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.store.load(&kept).unwrap().label, "Kept");
    drop(stream);
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r33_delete_unknown_malformed_and_unauthenticated_requests_change_nothing() {
    let h = Harness::new(3600).await;
    for id in [
        "cl-ffffff",
        "cx-ffffff",
        "not-an-id",
        "cl-ABCDEF",
        "cl-1234567890",
    ] {
        let response = h.delete(id).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{id}");
        let reply: qb_proto::admin_error::AdminErrorReply = response.json().await.unwrap();
        assert_eq!(reply.error.message, "Account not found");
    }
    let anonymous = h
        .client
        .delete(format!("{}/accounts/{}", h.base, h.doomed.id))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(h.file(&h.doomed.id).exists() && h.file(&h.kept.id).exists());
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r33_delete_is_409_during_drain_and_leaves_the_account() {
    let h = Harness::new(3600).await;
    h.relay.begin_drain().await.unwrap();
    let response = h.delete(&h.doomed.id).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let reply: qb_proto::admin_error::AdminErrorReply = response.json().await.unwrap();
    assert_eq!(reply.error.message, "Core is draining");
    assert!(h.file(&h.doomed.id).exists());
    assert_eq!(h.store.load(&h.doomed.id).unwrap().label, "Doomed");
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r33_delete_is_409_while_login_migration_or_takeover_owns_the_id() {
    let h = Harness::new(3600).await;
    let (doomed, kept) = (h.doomed.id.clone(), h.kept.id.clone());
    // A login migration moving doomed -> kept names both ids.
    h.store
        .begin_login_migration(&doomed, &kept, &h.kept.identity)
        .unwrap();
    for id in [&doomed, &kept] {
        assert_eq!(
            h.delete(id).send().await.unwrap().status(),
            StatusCode::CONFLICT,
            "{id}"
        );
        assert!(h.file(id).exists());
    }
    h.store.finish_login_migration().unwrap();

    // A fenced takeover receipt keeps the account until its owner finishes it.
    let receipt = crate::accounts::takeover::Receipt {
        operation: "a".repeat(64),
        state: crate::accounts::takeover::ReceiptState::Installed,
    };
    let current = h.store.load(&kept).unwrap();
    h.store
        .save_takeover_receipt(
            &current,
            &tokens(),
            Some(current.generation),
            None,
            &receipt,
        )
        .unwrap();
    assert_eq!(
        h.delete(&kept).send().await.unwrap().status(),
        StatusCode::CONFLICT
    );
    assert!(h.file(&kept).exists());
    // The other account is not owned by either operation.
    assert_eq!(
        h.delete(&doomed).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    h.finish().await;
}

#[test]
fn r33_store_delete_refuses_stopped_writes_and_unknown_ids_without_changes() {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/r33/delete-store-fixtures");
    std::fs::create_dir_all(&base).unwrap();
    let home = qb_testkit::fixture::home_in(base).unwrap();
    let store = AccountStore::acquire(home.path()).unwrap();
    let saved = store
        .save(
            &account("synthetic-r33-store", "Store", 3600),
            &tokens(),
            None,
        )
        .unwrap();
    assert!(matches!(
        store.delete_account("cl-ffffff"),
        Err(DeleteError::NotFound)
    ));
    assert!(matches!(
        store.delete_account("../escape"),
        Err(DeleteError::NotFound)
    ));
    store.stop_writes();
    assert!(matches!(
        store.delete_account(&saved.id),
        Err(DeleteError::Busy)
    ));
    assert_eq!(store.load(&saved.id).unwrap().label, "Store");
    drop(store);
    home.close().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r33_writes_arriving_after_the_delete_cannot_resurrect_the_account() {
    let h = Harness::new(3600).await;
    let stale = h.store.load(&h.doomed.id).unwrap();
    let stale_tokens = h.store.tokens(&stale).unwrap();
    assert_eq!(
        h.delete(&stale.id).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    let id = stale.id.clone();

    // Credential save of a refresh that finished late (generation fenced).
    assert!(
        h.store
            .save_credentials(&stale, &stale_tokens, stale.generation)
            .is_err()
    );
    assert!(
        h.store
            .save(&stale, &stale_tokens, Some(stale.generation))
            .is_err()
    );
    // Metadata, settings and health writers.
    assert!(matches!(
        h.store.update_metadata(&id, |a| a.label = "late".into()),
        Err(StoreError::NotFound)
    ));
    assert!(
        h.relay
            .accounts
            .change(&id, None, Some("late".into()), None, None)
            .await
            .is_err()
    );
    // A usage poll finishing late reaches the quota-state writer.
    let started = chrono::Utc::now();
    let body = serde_json::to_vec(&serde_json::json!({
        "five_hour": {"utilization": 99.0, "resets_at": started + chrono::Duration::hours(1)},
        "seven_day": {"utilization": 10.0, "resets_at": started + chrono::Duration::days(3)}
    }))
    .unwrap();
    let snapshot = crate::quota::parse_snapshot_for_test(Provider::Claude, &body, started).unwrap();
    assert!(
        h.relay
            .accounts
            .apply_polled_quota_snapshot(&id, snapshot.clone(), &snapshot, None, None, None)
            .await
            .is_err()
    );
    assert!(!h.file(&id).exists(), "no write path may recreate the file");
    assert!(matches!(h.store.load(&id), Err(StoreError::NotFound)));
    assert_eq!(h.state_ids().await, vec![h.kept.id.clone()]);
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r33_a_refresh_finishing_after_the_delete_does_not_resurrect_the_account() {
    // Due for refresh-ahead: 100 s of access-token life left.
    let h = Harness::new(100).await;
    let id = h.doomed.id.clone();
    let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
    let mut response = qb_testkit::ScriptedResponse::json(StatusCode::OK, bytes::Bytes::new());
    response.chunks = vec![(
        Duration::from_millis(600),
        bytes::Bytes::from_static(
            br#"{"access_token":"synthetic-r33-new","refresh_token":"synthetic-r33-new-refresh","expires_in":3600}"#,
        ),
    )];
    fake.state.push(response);
    let gates = crate::relay::credentials::TokenGates::new(
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        h.relay.accounts.clone(),
    )
    .with_token_endpoint(Provider::Claude, fake.url.clone());
    let (store, obs, caller_id) = (h.store.clone(), h.obs.clone(), id.clone());
    let refresh = tokio::spawn(async move { gates.refresh_ahead(store, obs, caller_id).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while fake.state.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the refresh request was sent");
    assert!(
        h.store.load(&id).unwrap().token_meta.refresh_uncertain,
        "the write-ahead preceded the request"
    );

    // The delete lands while the provider response is still in flight.
    assert_eq!(
        h.delete(&id).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    let outcome = tokio::time::timeout(Duration::from_secs(5), refresh)
        .await
        .expect("the refresh finishes")
        .unwrap();
    assert!(
        !matches!(outcome, Ok(Some(_))),
        "a deleted account is not reported as freshly refreshed"
    );
    assert_eq!(fake.state.requests().len(), 1);
    assert!(
        !h.file(&id).exists(),
        "the late rotation must not recreate the file"
    );
    assert!(matches!(h.store.load(&id), Err(StoreError::NotFound)));
    assert_eq!(h.state_ids().await, vec![h.kept.id.clone()]);
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r33_login_of_the_same_identity_after_a_delete_creates_a_fresh_account() {
    let h = Harness::new(3600).await;
    let id = h.doomed.id.clone();
    // The old account carried a label, a drain flag and a quota hold.
    h.store
        .update_metadata(&id, |a| {
            a.label = "Before delete".into();
            a.drain = true;
            a.user_state = crate::accounts::UserState::Paused;
            a.health = crate::accounts::Health::Cooling {
                reason: crate::accounts::HealthReason::Quota,
                until: chrono::Utc::now() + chrono::Duration::days(2),
                scope: "account".into(),
            };
        })
        .unwrap();
    let old = h.store.load(&id).unwrap();
    assert_eq!(
        h.delete(&id).send().await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let login = account("synthetic-r33-doomed", "", 3600);
    let saved = crate::accounts::login::save(&h.store, &login, &tokens(), false)
        .unwrap()
        .account;
    // The same identity hashes to the same id, but nothing of the old record survives.
    assert_eq!(saved.id, id);
    assert_eq!(saved.label, "");
    assert!(!saved.drain);
    assert_eq!(saved.user_state, crate::accounts::UserState::Active);
    assert_eq!(saved.health, crate::accounts::Health::Ready);
    assert!(saved.quota_cooling.is_none());
    assert_ne!(saved.health, old.health);
    assert!(h.file(&id).exists());
    assert_eq!(
        h.state_ids().await,
        sorted(vec![id.clone(), h.kept.id.clone()])
    );
    h.finish().await;
}
