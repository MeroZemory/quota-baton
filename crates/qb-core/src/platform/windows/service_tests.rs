use super::*;

struct Fixture {
    home: std::path::PathBuf,
    config: Config,
    admin: std::net::TcpListener,
    _data: std::net::TcpListener,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let base = executable
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r20-service-bootstrap");
        std::fs::create_dir_all(&base).unwrap();
        let directory = tempfile::tempdir_in(base).unwrap();
        let home = directory.path().join("home");
        crate::platform::create_private_directory_new(&home).unwrap();
        let admin = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        admin.set_nonblocking(true).unwrap();
        let data = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut config = Config::default();
        config.server.data_port = data.local_addr().unwrap().port();
        config.server.admin_port = admin.local_addr().unwrap().port();
        Self {
            home,
            config,
            admin,
            _data: data,
            _directory: directory,
        }
    }
}

#[tokio::test]
async fn r20_takeover_free_lease_does_not_contact_an_unowned_admin_listener() {
    let fixture = Fixture::new();
    assert!(
        !handle_takeover(&fixture.home, &fixture.config, true)
            .await
            .unwrap()
    );
    assert!(matches!(
        fixture.admin.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert!(!fixture.home.join("admin.token").exists());
    let accounts = fixture.home.join("accounts");
    let _lease = crate::platform::Lease::acquire(&accounts).unwrap();
}

#[tokio::test]
async fn r20_takeover_busy_lease_still_requires_authenticated_listener_export() {
    let fixture = Fixture::new();
    let accounts = crate::platform::prepare_account_directory(&fixture.home).unwrap();
    let _owner = crate::platform::Lease::acquire(&accounts).unwrap();
    let error = handle_takeover(&fixture.home, &fixture.config, true)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "listener export authentication unavailable"
    );
    assert!(matches!(
        crate::platform::Lease::acquire(&accounts),
        Err(crate::accounts::StoreError::LeaseBusy)
    ));
}

fn drain_relay(fixture: &Fixture) -> (Arc<RelayState>, Arc<crate::observability::Observability>) {
    let store = Arc::new(crate::accounts::AccountStore::acquire(&fixture.home).unwrap());
    let obs = Arc::new(crate::observability::Observability::new(&fixture.home, None).unwrap());
    let relay = Arc::new(
        RelayState::new(
            store,
            &fixture.config,
            obs.clone(),
            Arc::new(crate::debug_pin::DebugPins::default()),
        )
        .unwrap(),
    );
    (relay, obs)
}

#[tokio::test]
async fn drain_watcher_retries_failed_preparation_without_releasing_lease() {
    let fixture = Fixture::new();
    let (relay, obs) = drain_relay(&fixture);
    let quota = fixture.home.join("state/quota.json");
    std::fs::create_dir(&quota).unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-codex-limit-reached", "true".parse().unwrap());
    obs.observe_headers(
        "cx-synthetic-drain",
        crate::accounts::Provider::Codex,
        &headers,
    );
    assert!(relay.begin_drain().await.is_err());
    assert!(relay.is_draining());
    assert!(matches!(
        crate::accounts::AccountStore::acquire(&fixture.home),
        Err(crate::accounts::StoreError::LeaseBusy)
    ));
    // Start the watcher after the original notification and failure. It must
    // observe the level-triggered drain start, then retry until storage repairs.
    let watching = tokio::spawn(wait_for_shutdown(relay.clone()));
    assert!(
        tokio::time::timeout(Duration::from_millis(150), relay.wait_for_drain())
            .await
            .is_err()
    );
    assert!(!watching.is_finished());
    assert!(matches!(
        crate::accounts::AccountStore::acquire(&fixture.home),
        Err(crate::accounts::StoreError::LeaseBusy)
    ));
    std::fs::remove_dir(&quota).unwrap();
    tokio::time::timeout(Duration::from_secs(5), watching)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(quota.is_file());
    assert!(matches!(
        crate::accounts::AccountStore::acquire(&fixture.home),
        Err(crate::accounts::StoreError::LeaseBusy)
    ));
    relay.release_lease_after_drain();
    let _successor = crate::accounts::AccountStore::acquire(&fixture.home).unwrap();
}

#[tokio::test]
async fn drain_watcher_accepts_completed_preparation_and_keeps_lease() {
    let fixture = Fixture::new();
    let (relay, _obs) = drain_relay(&fixture);
    relay.begin_drain().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), wait_for_shutdown(relay.clone()))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        crate::accounts::AccountStore::acquire(&fixture.home),
        Err(crate::accounts::StoreError::LeaseBusy)
    ));
}

#[tokio::test]
async fn drain_watcher_finishes_after_cancellation_or_concurrent_completion() {
    let fixture = Fixture::new();
    let (relay, _obs) = drain_relay(&fixture);
    let preparing_relay = relay.clone();
    let preparing = tokio::spawn(async move { preparing_relay.begin_drain().await });
    relay.wait_for_drain_start().await;
    preparing.abort();
    match preparing.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(prepared) => {
            // Native export can finish before this task receives drain_start.
            // That completed preparation must also remain safe to observe.
            prepared.unwrap();
        }
    }
    tokio::time::timeout(Duration::from_secs(5), wait_for_shutdown(relay.clone()))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        crate::accounts::AccountStore::acquire(&fixture.home),
        Err(crate::accounts::StoreError::LeaseBusy)
    ));
}

#[tokio::test]
async fn drain_watcher_preserves_original_successful_preparation() {
    let fixture = Fixture::new();
    let (relay, _obs) = drain_relay(&fixture);
    let (prepared, watched) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(relay.begin_drain(), wait_for_shutdown(relay.clone()))
    })
    .await
    .unwrap();
    prepared.unwrap();
    watched.unwrap();
    assert!(matches!(
        crate::accounts::AccountStore::acquire(&fixture.home),
        Err(crate::accounts::StoreError::LeaseBusy)
    ));
}
