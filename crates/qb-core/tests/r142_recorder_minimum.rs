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
use std::{fs, io::Read, path::Path, sync::Arc, time::Duration};

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
        let capture = home.join("state/decision-tape-v8");
        let legacy = home.join("state/routing-replay");
        if mode == "on" {
            fs::create_dir_all(&legacy).unwrap();
            fs::write(legacy.join("operator-file"), b"preserve-legacy-root").unwrap();
        }
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
        let response = reqwest::Client::builder().no_proxy().timeout(DEADLINE).build().unwrap()
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
                    let decoded = if path.extension().is_some_and(|e| e == "zst") {
                        let mut decoded = Vec::new();
                        zstd::stream::read::Decoder::new(content.as_slice())
                            .unwrap()
                            .take(16 * 1024 * 1024 + 1)
                            .read_to_end(&mut decoded)
                            .unwrap();
                        assert!(decoded.len() <= 16 * 1024 * 1024);
                        decoded
                    } else {
                        content.clone()
                    };
                    let text = String::from_utf8_lossy(&decoded);
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
                    if path.extension().is_some_and(|e| e == "zst") {
                        events.extend(
                            text.lines()
                                .map(|line| serde_json::from_str::<Value>(line).unwrap()),
                        );
                    }
                    if path.file_name().is_some_and(|name| name == "manifest.json") {
                        manifests.push(serde_json::from_slice::<Value>(&content).unwrap());
                    }
                }
                let metadata: Vec<_> = events
                    .iter()
                    .filter(|e| e["record"]["event"]["kind"] == "Metadata")
                    .map(|e| &e["record"]["event"]["facts"])
                    .collect();
                assert!(metadata.iter().any(|e| e["kind"] == "AccountState"));
                assert!(metadata.iter().any(|e| e["kind"] == "CollectionMember"));
                assert!(
                    metadata.iter().any(
                        |e| e["kind"] == "CollectionBoundary" && e["facts"]["complete"] == true
                    )
                );
                assert_eq!(manifests.len(), 1);
                let manifest = &manifests[0];
                assert_eq!(manifest["schema"], 8);
                assert_eq!(manifest["sealed"], true);
                assert_eq!(manifest["accepted"], manifest["persisted"]);
                assert!(manifest["persisted"].as_u64().unwrap() >= 3);
                assert_eq!(manifest["hourly_encoded_byte_limit"], 20_000_000);
                assert!(
                    manifest["losses"]
                        .as_object()
                        .unwrap()
                        .values()
                        .all(|v| v == 0 || v == false)
                );
                let epoch =
                    qb_core::observability::decision_reader::load(&capture.join("e00")).unwrap();
                assert_eq!(epoch.losses, 0);
                assert!(!epoch.initial_witness.is_empty());
                assert_eq!(
                    fs::read(legacy.join("operator-file")).unwrap(),
                    b"preserve-legacy-root"
                );
                if let Some(output) = std::env::var_os("QB_RECORDER_SAMPLE_OUT") {
                    let output = Path::new(&output);
                    fs::create_dir_all(output).unwrap();
                    fs::write(
                        output.join("synthetic-events.json"),
                        serde_json::to_vec_pretty(&events).unwrap(),
                    )
                    .unwrap();
                    fs::write(
                        output.join("synthetic-manifest.json"),
                        serde_json::to_vec_pretty(manifest).unwrap(),
                    )
                    .unwrap();
                }
            }
        }
        drop(servers);
        drop(state);
        drop(store);
        tokio::task::spawn_blocking(move || std::thread::spawn(move || drop(obs)).join().unwrap())
            .await
            .unwrap();
        root.close().unwrap();
    }
}
