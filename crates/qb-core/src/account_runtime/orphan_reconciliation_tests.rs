use super::*;
use crate::{
    accounts::Identity,
    config::{Config, LinuxSecretBackend},
};
use std::{fs, path::Path};

const ORPHAN: &str = "cx-413c3b";

fn home() -> tempfile::TempDir {
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let home = tempfile::Builder::new()
        .prefix("qb-orphan-observations-")
        .tempdir_in(executable.parent().unwrap())
        .unwrap();
    let mut config = Config::default();
    config.secrets.linux_backend = LinuxSecretBackend::File0600;
    crate::platform::atomic_write_account(
        home.path(),
        &home.path().join("config.toml"),
        toml::to_string(&config).unwrap().as_bytes(),
    )
    .unwrap();
    home
}

fn keeper(store: &AccountStore) -> Account {
    let mut account = crate::accounts::health::tests::fixture();
    account.identity = Identity::Codex {
        account_id: "synthetic-orphan-keeper".into(),
        user_id: None,
        email: "keeper@example.invalid".into(),
    };
    account.id = account.identity.id();
    account.email = "keeper@example.invalid".into();
    account.user_state = UserState::Active;
    account.health = Health::Ready;
    account.token_meta.expires_at = Some("2099-01-01T00:00:00Z".parse().unwrap());
    assert_ne!(account.id, ORPHAN);
    store
        .save(
            &account,
            &crate::accounts::Tokens {
                access_token: "synthetic-orphan-access".into(),
                refresh_token: "synthetic-orphan-refresh".into(),
                id_token: None,
            },
            None,
        )
        .unwrap()
}

fn snapshot(orphan: bool) -> Value {
    let mut windows = vec![json!({"name":"7d", "used_pct":23.,
        "observed_at":Utc::now().to_rfc3339(), "reset_at":"2099-01-01T00:00:00Z", "status":"allowed"})];
    if orphan {
        windows.push(
            json!({"name":"0m", "used_pct":7., "observed_at":Utc::now().to_rfc3339(),
            "reset_at":null, "status":"allowed"}),
        );
    }
    json!({"windows":windows,"plan":"plus","credits":null,"source":"poll",
        "observed_at":Utc::now().to_rfc3339()})
}

fn disk(home: &Path, name: &str) -> Value {
    serde_json::from_slice(&fs::read(home.join("state").join(name)).unwrap()).unwrap()
}

fn seed(home: &Path, keeper: Option<&str>) {
    let obs = Observability::new(home, None).unwrap();
    assert!(obs.observe_poll(ORPHAN, snapshot(true), true).is_some());
    if let Some(id) = keeper {
        assert!(obs.observe_poll(id, snapshot(false), true).is_some());
    }
    obs.freeze_quota().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_load_prunes_cx_413c3b_quota_usage_and_forecasts_but_preserves_keeper_and_reopen() {
    let home = home();
    let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
    let keeper = keeper(&store);
    seed(home.path(), Some(&keeper.id));
    let quota_before = disk(home.path(), "quota.json");
    let usage_before = disk(home.path(), "usage-history.json");
    assert_eq!(
        quota_before["accounts"][ORPHAN]["windows"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(usage_before["accounts"].get(ORPHAN).is_some());
    assert!(
        !home
            .path()
            .join("accounts")
            .join(format!("{ORPHAN}.json"))
            .exists()
    );

    let obs = Arc::new(Observability::new(home.path(), None).unwrap());
    obs.configure_forecast(true);
    // Also exercise warm display forecasts and an already queued quota update,
    // not only the restart-cold estimator and persisted history.
    let stale = obs.quota_snapshot(ORPHAN).unwrap();
    assert!(obs.observe_poll(ORPHAN, stale, true).is_some());
    assert!(
        !obs.forecast_view(ORPHAN, 0, 0)["windows"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !obs.dashboard()
            .usage_forecast(ORPHAN, Utc::now(), None)
            .windows
            .is_empty()
    );
    let keeper_quota = obs.quota_snapshot(&keeper.id).unwrap();
    let runtime = AccountRuntime::new(store.clone(), obs.clone(), 1.0);
    let views = runtime.typed_views().await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].id, keeper.id);
    assert!(obs.quota_snapshot(ORPHAN).is_none());
    assert!(
        obs.forecast_view(ORPHAN, 0, 0)["windows"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(obs.quota_snapshot(&keeper.id).unwrap(), keeper_quota);
    assert!(
        obs.dashboard()
            .usage_forecast(ORPHAN, Utc::now(), None)
            .windows
            .is_empty()
    );
    let chart = obs.dashboard().usage_page(48).unwrap();
    assert_eq!(chart.accounts.len(), 1);
    assert_eq!(chart.accounts[0].id.as_str(), keeper.id);
    let quota_after = disk(home.path(), "quota.json");
    let usage_after = disk(home.path(), "usage-history.json");
    assert!(quota_after["accounts"].get(ORPHAN).is_none());
    assert!(usage_after["accounts"].get(ORPHAN).is_none());
    assert_eq!(
        quota_after["accounts"][&keeper.id],
        quota_before["accounts"][&keeper.id]
    );
    assert_eq!(
        usage_after["accounts"][&keeper.id],
        usage_before["accounts"][&keeper.id]
    );
    obs.freeze_quota().unwrap();
    let reopened = Observability::new(home.path(), None).unwrap();
    assert!(reopened.quota_snapshot(ORPHAN).is_none());
    assert_eq!(reopened.quota_snapshot(&keeper.id).unwrap(), keeper_quota);
    assert_eq!(
        reopened.dashboard().usage_page(48).unwrap().accounts.len(),
        1
    );
    reopened.freeze_quota().unwrap();
}

#[test]
fn failed_authoritative_list_does_not_prune_from_a_guessed_empty_set() {
    let home = home();
    let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
    let keeper = keeper(&store);
    seed(home.path(), Some(&keeper.id));
    // A malformed synthetic account forces Store.list_unlocked to fail.
    fs::write(
        home.path().join(format!("accounts/{ORPHAN}.json")),
        b"not an account",
    )
    .unwrap();
    assert!(store.list().is_err());
    let obs = Arc::new(Observability::new(home.path(), None).unwrap());
    let before = obs.quota_snapshot(ORPHAN).unwrap();
    let quota_before = fs::read(home.path().join("state/quota.json")).unwrap();
    let usage_before = fs::read(home.path().join("state/usage-history.json")).unwrap();
    let _runtime = AccountRuntime::new(store, obs.clone(), 1.0);
    assert_eq!(obs.quota_snapshot(ORPHAN).unwrap(), before);
    assert_eq!(obs.dashboard().usage_page(48).unwrap().accounts.len(), 2);
    obs.freeze_quota().unwrap();
    assert_eq!(
        fs::read(home.path().join("state/quota.json")).unwrap(),
        quota_before
    );
    assert_eq!(
        fs::read(home.path().join("state/usage-history.json")).unwrap(),
        usage_before
    );
}

#[test]
fn direct_observability_without_store_preserves_its_library_cache() {
    let home = home();
    seed(home.path(), None);
    assert!(!home.path().join("accounts").exists());
    let obs = Observability::new(home.path(), None).unwrap();
    assert!(obs.quota_snapshot(ORPHAN).is_some());
    assert_eq!(obs.dashboard().usage_page(48).unwrap().accounts.len(), 1);
    obs.freeze_quota().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistence_failure_leaves_memory_pruned_and_native_core_usable() {
    let home = home();
    let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
    let keeper = keeper(&store);
    seed(home.path(), Some(&keeper.id));
    let obs = Arc::new(Observability::new(home.path(), None).unwrap());
    // Block only the synthetic quota cache destination, not native accounts.
    let path = home.path().join("state/quota.json");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let runtime = AccountRuntime::new(store, obs.clone(), 1.0);
    assert!(obs.quota_snapshot(ORPHAN).is_none());
    assert_eq!(obs.dashboard().usage_page(48).unwrap().accounts.len(), 1);
    let views = runtime.typed_views().await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].id, keeper.id);
    assert!(
        obs.reconcile_accounts(&[keeper]).is_err(),
        "no false durability claim"
    );
}
