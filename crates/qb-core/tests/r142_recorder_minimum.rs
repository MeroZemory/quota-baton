//! Minimum capture acceptance: actual listener and relay, synthetic credentials only.
#![cfg(feature = "testkit")]

use qb_core::{
    accounts::{AccountStore, import},
    config::Config,
    debug_pin::DebugPins,
    http,
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc, time::Duration};

const DEADLINE: Duration = Duration::from_secs(10);

fn files(path: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for item in fs::read_dir(path).unwrap() {
        let path = item.unwrap().path();
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn n6_replay_actual_http_off_on_nonfatal_storage_and_drained_archive() {
    for mode in ["off", "on", "foreign-storage"] {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r142-minimum-fixtures");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let cpa = root.path().join("synthetic-cpa");
        fs::create_dir_all(&cpa).unwrap();
        fs::write(
            cpa.join("sample.json"),
            serde_json::to_vec(&json!({
                "type":"claude", "email":"sample-acct@example.test",
                "account_uuid":"sample-uuid", "organization_uuid":"sample-org",
                "access_token":"private-access-canary", "refresh_token":"private-refresh-canary",
                "expired":"2099-01-01T00:00:00Z", "last_refresh":"2026-01-01T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
        let home = root.path().join("home");
        let store = Arc::new(AccountStore::acquire(&home).unwrap());
        import::run(
            &store,
            &cpa,
            &["claude:sample-acct@example.test".into()],
            false,
        )
        .unwrap();
        let account_id = store.list().unwrap()[0].id.clone();
        let capture = home.join("state/routing-replay");
        if mode == "foreign-storage" {
            fs::create_dir_all(&capture).unwrap();
            fs::write(capture.join("operator-file"), b"preserve-this-file").unwrap();
        }
        let fake = ScriptedUpstream::start().await.unwrap();
        fake.state.push(ScriptedResponse::json(
            axum::http::StatusCode::OK,
            "{\"ok\":true}",
        ));
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.anthropic_base = fake.url.clone();
        config.upstream.codex_base = fake.url.clone();
        config.routing_replay.enabled = mode != "off";
        config.routing_replay.disk_limit_mib = 16;
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap(),
        );
        let mut servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "sample-client-secret".into(),
                admin_token: "sample-admin-secret".into(),
            },
            relay::router(state.clone()),
            axum::Router::new(),
        )
        .await
        .unwrap();
        let response = reqwest::Client::builder().timeout(DEADLINE).build().unwrap()
            .post(format!("http://{}/v1/messages", servers.data_addr))
            .bearer_auth("sample-client-secret")
            .header("anthropic-version", "2023-06-01")
            .header("x-session-id", "private-session-canary")
            .json(&json!({"model":"sample-model","max_tokens":8,"messages":[{"role":"user","content":"private-body-canary"}]}))
            .send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{mode}");
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"{\"ok\":true}");
        tokio::time::timeout(DEADLINE, state.begin_drain())
            .await
            .unwrap()
            .unwrap();
        servers.shutdown();
        tokio::time::timeout(DEADLINE, &mut servers.data_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(DEADLINE, &mut servers.admin_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match mode {
            "off" => assert!(!capture.exists(), "off must not create a key/root/thread"),
            "foreign-storage" => assert_eq!(
                fs::read(capture.join("operator-file")).unwrap(),
                b"preserve-this-file"
            ),
            _ => {
                let mut events = Vec::new();
                let mut manifests = Vec::new();
                for path in files(&capture) {
                    let content = fs::read(&path).unwrap();
                    let text = String::from_utf8_lossy(&content);
                    for secret in [
                        "private-access-canary",
                        "private-refresh-canary",
                        "private-body-canary",
                        "private-session-canary",
                        "sample-acct@example.test",
                        &account_id,
                    ] {
                        assert!(
                            !text.contains(secret),
                            "private input escaped into metadata"
                        );
                    }
                    if path.extension().is_some_and(|e| e == "jsonl") {
                        events.extend(
                            text.lines()
                                .map(|line| serde_json::from_str::<Value>(line).unwrap()),
                        );
                    }
                    if path.file_name().is_some_and(|name| name == "manifest.json") {
                        manifests.push(serde_json::from_slice::<Value>(&content).unwrap());
                    }
                }
                assert!(events.iter().any(|e| e["event"]["kind"] == "AccountState"));
                assert!(
                    events
                        .iter()
                        .any(|e| e["event"]["kind"] == "CollectionMember")
                );
                assert!(
                    events
                        .iter()
                        .any(|e| e["event"]["kind"] == "CollectionBoundary"
                            && e["event"]["facts"]["complete"] == true)
                );
                assert_eq!(manifests.len(), 1);
                let manifest = &manifests[0];
                assert_eq!(manifest["sealed"], true);
                assert_eq!(manifest["replay_ready"], false);
                assert_eq!(manifest["initial_state_reconciled"], false);
                assert_eq!(manifest["owner_hooks_complete"], false);
                assert_eq!(manifest["retention_ns"], 7_u64 * 24 * 3600 * 1_000_000_000);
                assert_eq!(manifest["queued_records"], 0);
                assert!(manifest["persisted_through"].as_u64().unwrap() >= 3);
            }
        }
        drop(servers);
        drop(state);
        drop(store);
        tokio::task::spawn_blocking(move || drop(obs))
            .await
            .unwrap();
        root.close().unwrap();
    }
}
