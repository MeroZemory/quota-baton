//! Authenticated local routing CLI must ignore inherited HTTP proxy settings.
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[tokio::test]
async fn routing_cli_never_sends_admin_credentials_to_an_environment_proxy() {
    let base =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/routing-proxy");
    std::fs::create_dir_all(&base).unwrap();
    let home = tempfile::tempdir_in(base).unwrap();
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[server]\nadmin_port={}\n",
            admin.local_addr().unwrap().port()
        ),
    )
    .unwrap();
    std::fs::write(
        home.path().join("admin.token"),
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    )
    .unwrap();
    std::fs::write(
        home.path().join("client.key"),
        "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA",
    )
    .unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let server = tokio::spawn(async move {
        let router = axum::Router::new().route(
            "/admin/v1/routing",
            axum::routing::get(move |headers: axum::http::HeaderMap| async move {
                assert_eq!(
                    headers["authorization"],
                    "Bearer AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                );
                count.fetch_add(1, Ordering::SeqCst);
                let reply: qb_proto::admin_routing::RoutingGet =
                    serde_json::from_value(serde_json::json!({
                        "revision": 0,
                        "defaults": qb_core::routing::Settings::default(),
                        "projects": {},
                        "sessions": {},
                        "observed_sessions": [],
                        "forecast_shadow": [],
                    }))
                    .unwrap();
                axum::Json(reply)
            }),
        );
        axum::serve(admin, router).await.unwrap();
    });
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_quota-baton"));
    let profile = home.path().join("synthetic-profile");
    command
        .args(["routing", "list"])
        .env("QB_HOME", home.path())
        .env("HOME", &profile)
        .env("USERPROFILE", &profile)
        .env("APPDATA", profile.join("AppData/Roaming"))
        .env("LOCALAPPDATA", profile.join("AppData/Local"))
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .unwrap()
        .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), proxy.accept())
            .await
            .is_err(),
        "admin request reached environment proxy"
    );
}
