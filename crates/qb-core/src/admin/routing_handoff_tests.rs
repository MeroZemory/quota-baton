use super::*;
use crate::{config::LinuxSecretBackend, debug_pin::DebugPins, observability::Observability};
use std::time::Duration;

#[tokio::test]
async fn routing_handoff_refuses_before_policy_mutation_with_typed_reason() {
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let base = executable
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/r13-cli-admin");
    std::fs::create_dir_all(&base).unwrap();
    let root = tempfile::tempdir_in(base).unwrap();
    let home = root.path().join("home");
    crate::platform::create_private_directory_new(&home).unwrap();
    let mut config = crate::config::Config::default();
    config.secrets.linux_backend = LinuxSecretBackend::File0600;
    config.server.data_port = 0;
    config.server.admin_port = 0;
    crate::platform::atomic_write_account(
        &home,
        &home.join("config.toml"),
        toml::to_string(&config).unwrap().as_bytes(),
    )
    .unwrap();
    let store = Arc::new(AccountStore::acquire(&home).unwrap());
    let obs = Arc::new(Observability::new(&home, None).unwrap());
    let relay = Arc::new(
        crate::relay::RelayState::new(
            store.clone(),
            &config,
            obs.clone(),
            Arc::new(DebugPins::default()),
        )
        .unwrap(),
    );
    let mut servers = crate::http::spawn_with_admin(
        &config,
        crate::local_secrets::LocalSecrets {
            client_key: "synthetic-r13-client".into(),
            admin_token: "synthetic-r13-admin".into(),
        },
        runtime_routes(relay.clone(), 0),
    )
    .await
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let url = format!("http://{}/admin/v1/routing", servers.admin_addr);
    let project = serde_json::json!({"kind":"project", "name":"synthetic-before-drain",
        "settings":{"mode":"strict", "reset_wait_secs":5}});
    let accepted = client
        .post(&url)
        .header(reqwest::header::CONNECTION, "close")
        .bearer_auth("synthetic-r13-admin")
        .json(&project)
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let _: serde_json::Value = accepted.json().await.unwrap();

    relay.begin_drain().await.unwrap();
    let before = relay.routing_view();
    for request in [
        serde_json::json!({"kind":"urgent", "id":"synthetic-root", "on_5h_limit":"wait"}),
        serde_json::json!({"kind":"project", "name":"synthetic-after-drain",
            "settings":{"mode":"rebalance", "reset_wait_secs":9}}),
    ] {
        let response = client
            .post(&url)
            .header(reqwest::header::CONNECTION, "close")
            .bearer_auth("synthetic-r13-admin")
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let reply: qb_proto::admin_error::AdminErrorReply = response.json().await.unwrap();
        assert_eq!(
            reply.error.message,
            "코어 인계 중에는 정책을 변경할 수 없습니다."
        );
        let after = relay.routing_view();
        assert_eq!(after["revision"], before["revision"]);
        assert_eq!(after["projects"], before["projects"]);
        assert_eq!(after["sessions"], before["sessions"]);
    }
    drop(client);
    servers.shutdown();
    tokio::time::timeout(Duration::from_secs(3), async {
        (&mut servers.data_task).await.unwrap().unwrap();
        (&mut servers.admin_task).await.unwrap().unwrap();
    })
    .await
    .unwrap();
    // Keep the actual native owners through the complete HTTP shutdown, then
    // release them before the fixture directory is removed.
    std::thread::spawn(move || {
        drop(servers);
        drop(relay);
        drop(store);
        drop(obs);
        drop(root);
    })
    .join()
    .unwrap();
}
