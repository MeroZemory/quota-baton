use qb_core::accounts::AccountStore;

#[tokio::test]
async fn takeover_listener_close_is_observed_while_handler_is_still_active() {
    use std::{sync::Arc, time::Duration};
    let started = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let app = axum::Router::new().route(
        "/hold",
        axum::routing::get({
            let started = started.clone();
            let finish = finish.clone();
            move || {
                let started = started.clone();
                let finish = finish.clone();
                async move {
                    started.notify_one();
                    finish.notified().await;
                    "synthetic-complete"
                }
            }
        }),
    );
    let mut config = qb_core::config::Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    let servers = qb_core::http::spawn_with_routes(
        &config,
        qb_core::local_secrets::LocalSecrets {
            client_key: "synthetic-local-key".into(),
            admin_token: "synthetic-admin-key".into(),
        },
        app,
        axum::Router::new(),
    )
    .await
    .unwrap();
    let address = servers.data_addr;
    let request = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}/hold"))
            .bearer_auth("synthetic-local-key")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    servers.shutdown();
    tokio::time::timeout(Duration::from_secs(2), servers.wait_for_listener_close())
        .await
        .unwrap();
    assert!(
        !request.is_finished(),
        "active handler was aborted during listener close"
    );
    let successor_listener = tokio::net::TcpListener::bind(address).await.unwrap();
    finish.notify_one();
    assert_eq!(request.await.unwrap(), "synthetic-complete");
    drop(successor_listener);
}

#[test]
fn takeover_can_release_the_home_lease_before_store_drop() {
    let parent = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent).unwrap();
    let home = root.path().join("home");
    let first = AccountStore::acquire(&home).unwrap();
    first.release_lease();

    // The runtime objects may still hold the first store while the successor
    // acquires the same home during takeover.
    let second = AccountStore::acquire(&home).unwrap();
    second.release_lease();
}

#[test]
fn takeover_freezes_the_old_store_before_successor_acquires() {
    let parent = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent).unwrap();
    let home = root.path().join("home");
    let first = AccountStore::acquire(&home).unwrap();
    first.stop_writes();
    assert!(first.writes_stopped());
    first.release_lease();
    let second = AccountStore::acquire(&home).unwrap();
    second.release_lease();
}
