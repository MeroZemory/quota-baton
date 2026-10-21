#![cfg(all(feature = "testkit", debug_assertions))]
//! Proposal only: synthetic numeric-loopback upstream and isolated spawned core.
use futures_util::{SinkExt, StreamExt};
use qb_core::{
    accounts::{AccountStore, import},
    dashboard::SessionId,
};
use qb_testkit::websocket::WebSocketUpstream;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Stdio, time::Duration};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const CLIENT: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA";
const ADMIN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const SID: &str = "r101-synthetic-session";

async fn detail(client: &reqwest::Client, port: u16) -> Value {
    let id = SessionId::from_key(&format!("codex:{SID}"));
    let response = client
        .get(format!(
            "http://127.0.0.1:{port}/admin/v1/sessions/{}",
            id.as_str()
        ))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    response.json().await.unwrap()
}

#[tokio::test]
async fn r101_spawned_core_ws_create_model_visible_while_inflight_and_updates_next_turn() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r101-core-tests");
    fs::create_dir_all(&base).unwrap();
    let root = tempfile::tempdir_in(base).unwrap();
    let home = root.path().join("home");
    let profile = root.path().join("profile");
    let external = root.path().join("synthetic-cpa");
    for path in [&home, &profile, &external, &root.path().join("temp")] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(
        external.join("fixture.json"),
        serde_json::to_vec(&json!({
            "type":"codex", "email":"r101@example.invalid", "account_id":"r101-account",
            "access_token":"r101-synthetic-access", "refresh_token":"r101-synthetic-refresh",
            "expired":"2099-01-01T00:00:00Z", "last_refresh":"2026-10-21T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    {
        let store = AccountStore::acquire(&home).unwrap();
        import::run(
            &store,
            &external,
            &["codex:r101@example.invalid".into()],
            false,
        )
        .unwrap();
    } // Release the lease before starting the real CLI process.
    fs::write(home.join("client.key"), CLIENT).unwrap();
    fs::write(home.join("admin.token"), ADMIN).unwrap();
    let fake = WebSocketUpstream::start().await.unwrap();
    fake.state.set_echo(false);
    // Numeric loopback only, including polling and refresh endpoints.
    assert!(fake.url.starts_with("http://127.0.0.1:"));
    let data = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let data_port = data.local_addr().unwrap().port();
    let admin_port = admin.local_addr().unwrap().port();
    fs::write(home.join("config.toml"), format!(
        "[server]\ndata_port={data_port}\nadmin_port={admin_port}\n[dashboard]\nresolve_claude_project=false\n[upstream]\ncodex_base='{0}'\nanthropic_base='{0}'\ncodex_usage_base='{0}'\nclaude_usage_base='{0}'\ncodex_token_endpoint='{0}/oauth/token'\nclaude_token_endpoint='{0}/oauth/token'\n", fake.url
    )).unwrap();
    drop((data, admin));
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_quota-baton"));
    command
        .env_clear()
        .arg("serve")
        .env("QB_HOME", &home)
        .env("HOME", &profile)
        .env("USERPROFILE", &profile)
        .env("APPDATA", profile.join("AppData/Roaming"))
        .env("LOCALAPPDATA", profile.join("AppData/Local"))
        .env("CODEX_HOME", profile.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", profile.join(".claude"))
        .env("TEMP", root.path().join("temp"))
        .env("TMP", root.path().join("temp"))
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Preserve only OS bootstrap paths; no inherited API keys or proxy settings.
    for name in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut core = command.spawn().unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                core.try_wait().unwrap().is_none(),
                "core exited before readiness"
            );
            if client
                .get(format!("http://127.0.0.1:{admin_port}/admin/v1/state"))
                .bearer_auth(ADMIN)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let mut request = format!("ws://127.0.0.1:{data_port}/v1/responses")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {CLIENT}").parse().unwrap());
    request
        .headers_mut()
        .insert("session-id", SID.parse().unwrap());
    request
        .headers_mut()
        .insert("originator", "codex-tui".parse().unwrap());
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .unwrap()
    .unwrap();
    let idle = detail(&client, admin_port).await;
    assert!(idle["model"].is_null()); // No handshake model supplied by the fake.
    assert_eq!(idle["inflight"]["ws_response_in_flight"], false);
    for (turn, supplied, expected) in [
        (1, Some("r101-first"), "r101-first"),
        (2, None, "r101-first"), // Missing evidence must not erase a known model.
        (3, Some("r101-second"), "r101-second"),
    ] {
        let response_id = format!("r101-response-{turn}");
        fake.state.push_reply([Message::Text(
            json!({"type":"response.created",
            "response":{"id":response_id,"status":"in_progress"}})
            .to_string()
            .into(),
        )]);
        let mut create = json!({"type":"response.create","input":[]});
        if let Some(model) = supplied {
            create["model"] = json!(model);
        }
        socket
            .send(Message::Text(create.to_string().into()))
            .await
            .unwrap();
        // Receiving this marker proves the real pump forwarded response.create.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match socket
                    .next()
                    .await
                    .expect("socket ended")
                    .expect("WS error")
                {
                    Message::Text(text) => {
                        let event: Value = serde_json::from_str(&text).unwrap();
                        if event["type"] == "response.created" {
                            assert_eq!(event["response"]["id"], response_id);
                            break;
                        }
                    }
                    Message::Close(_) => panic!("socket closed before created"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let active = detail(&client, admin_port).await;
        assert_eq!(active["model"], expected);
        assert_eq!(active["client"]["kind"], "codex_cli");
        assert_eq!(active["inflight"]["ws_response_in_flight"], true);
        assert_eq!(active["inflight"]["requests"], 1);
        assert_eq!(active["requests_today"], turn);
        // Only now release the terminal event; the assertion above is in flight.
        fake.state
            .send(Message::Text(
                json!({"type":"response.completed",
            "response":{"id":response_id,"status":"completed"}})
                .to_string()
                .into(),
            ))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if detail(&client, admin_port).await["inflight"]["requests"] == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    assert_eq!(
        fake.state
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Text(t)
        if serde_json::from_str::<Value>(t).unwrap()["type"] == "response.create"))
            .count(),
        3
    );
    socket.close(None).await.unwrap();
    core.kill().await.unwrap();
    core.wait().await.unwrap();
}
