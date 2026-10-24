use std::{fs, path::PathBuf, process::Command, time::Duration};

use axum::{Router, routing::get};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::{
    config::Config,
    http,
    local_secrets::{self, LocalSecrets},
    logging,
};
use reqwest::{Client, StatusCode};
use tempfile::TempDir;

fn home() -> TempDir {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/tests");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("m0-")
        .tempdir_in(root)
        .unwrap()
}

fn client() -> Client {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn secrets() -> LocalSecrets {
    LocalSecrets {
        client_key: "fake-client-key".into(),
        admin_token: "fake-admin-token".into(),
    }
}

#[test]
fn config_defaults_partial_unknown_and_invalid() {
    let home = home();
    let defaults = Config::load(home.path()).unwrap();
    assert_eq!(defaults.server.data_port, 10200);
    assert_eq!(defaults.server.admin_port, 10201);
    assert_eq!(defaults.routing.gate_5h_pct, 90.0);
    assert_eq!(defaults.routing.gate_weekly_pct, 90.0);
    assert_eq!(defaults.routing.gate_weekly_near_reset_pct, 98.0);
    assert_eq!(defaults.routing.near_reset_secs, 18000);
    assert_eq!(defaults.routing.inflight_allowance_pct, 1.0);
    assert_eq!(defaults.routing.reset_hold_secs, 60);
    assert_eq!(defaults.routing.rebalance_idle_secs, 3600);
    assert_eq!(defaults.routing.session_binding_max_secs, 604800);
    assert_eq!(defaults.guard.retain_max_per_request_mib, 64);
    assert_eq!(defaults.guard.inflight_budget_mib, 384);
    assert_eq!(defaults.guard.memory_hard_limit_mib, 1024);
    assert_eq!(defaults.websocket.idle_close_secs, 1800);
    assert_eq!(defaults.websocket.max_message_size_mib, 192);
    assert!(!defaults.websocket.upstream_deflate);
    assert_eq!(defaults.quota.poll_min_interval_secs, 300);
    fs::write(home.path().join("config.toml"), "unknown = true\n[server]\ndata_port = 0\nunknown = 1\n[accounts.cx_fake]\ndrain = true\nunknown = false\n").unwrap();
    let config = Config::load(home.path()).unwrap();
    assert_eq!(config.server.data_port, 0);
    assert_eq!(config.server.admin_port, 10201);
    assert!(config.accounts["cx_fake"].drain);
    assert!(!config.accounts["cx_fake"].allow_credits);
    for invalid in ["[server]\ndata_port = 'fake-secret-value'", "broken = ["] {
        fs::write(home.path().join("config.toml"), invalid).unwrap();
        let error = Config::load(home.path()).unwrap_err().to_string();
        assert!(!error.contains("fake-secret-value"));
    }
    // D20 §4 replaces refusal for numeric bounds with warning + clamping.
    // Invalid types/syntax above still refuse without echoing source values.
    for (text, port, idle) in [
        ("[server]\ndata_port = 65536", 65535, 1800),
        ("[websocket]\nidle_close_secs = 3600", 10200, 3599),
    ] {
        fs::write(home.path().join("config.toml"), text).unwrap();
        let config = Config::load(home.path()).unwrap();
        assert_eq!(config.server.data_port, port);
        assert_eq!(config.websocket.idle_close_secs, idle);
        assert_eq!(
            fs::read_to_string(home.path().join("config.toml")).unwrap(),
            text
        );
    }
}

#[test]
fn local_secrets_round_trip_and_reject_corruption() {
    let home = home();
    let first = local_secrets::load_or_create(home.path()).unwrap();
    assert_ne!(first.client_key, first.admin_token);
    for value in [&first.client_key, &first.admin_token] {
        assert_eq!(value.len(), 43);
        assert_eq!(URL_SAFE_NO_PAD.decode(value).unwrap().len(), 32);
        assert!(!format!("{first:?}").contains(value));
    }
    let second = local_secrets::load_or_create(home.path()).unwrap();
    assert_eq!(first.client_key, second.client_key);
    assert_eq!(first.admin_token, second.admin_token);
    #[cfg(windows)]
    {
        // 합성 테스트 파일의 ACL만 검사하며 자격 증명 내용은 출력하지 않는다.
        let shell_home = home
            .path()
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned();
        let result = Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User; foreach($name in @('client.key','admin.token')) { $acl=Get-Acl -LiteralPath (Join-Path $env:QB_HOME $name); if(!$acl.AreAccessRulesProtected -or $acl.Access.Count -ne 1) { exit 1 }; $rule=$acl.Access[0]; if($rule.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]) -ne $sid -or $rule.AccessControlType -ne 'Allow') { exit 2 } }"])
            .env("QB_HOME", shell_home).env_remove("PSModulePath").output().unwrap();
        assert!(
            result.status.success(),
            "synthetic secret DACL check failed: {:?} {}",
            result.status.code(),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    fs::write(home.path().join("client.key"), "fake-invalid-secret").unwrap();
    let error = local_secrets::load_or_create(home.path())
        .unwrap_err()
        .to_string();
    assert!(!error.contains("fake-invalid-secret"));
}

#[tokio::test]
async fn listeners_enforce_c2_and_protect_extra_admin_routes() {
    let home = home();
    fs::write(
        home.path().join("config.toml"),
        "[server]\ndata_port = 0\nadmin_port = 0",
    )
    .unwrap();
    let config = Config::load(home.path()).unwrap();
    let servers = http::spawn_with_admin(
        &config,
        secrets(),
        Router::new().route("/admin/v1/extra", get(|| async { "extra" })),
    )
    .await
    .unwrap();
    assert!(servers.data_addr.ip().is_loopback());
    assert!(servers.admin_addr.ip().is_loopback());
    assert_ne!(servers.data_addr.port(), servers.admin_addr.port());
    let client = client();
    let data = format!("http://{}/v1/messages", servers.data_addr);
    let admin = format!("http://{}/admin/v1/health", servers.admin_addr);
    for (url, key) in [(&data, "fake-client-key"), (&admin, "fake-admin-token")] {
        assert_eq!(
            client
                .get(url)
                .header("host", "evil.example")
                .header("origin", "null")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::MISDIRECTED_REQUEST
        );
        assert_eq!(
            client
                .get(url)
                .header(
                    "host",
                    format!(
                        "127.0.0.1:{}",
                        if url == &data {
                            servers.admin_addr.port()
                        } else {
                            servers.data_addr.port()
                        }
                    )
                )
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::MISDIRECTED_REQUEST
        );
        assert_eq!(
            client
                .get(url)
                .header("origin", "")
                .bearer_auth(key)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client.get(url).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(url)
                .bearer_auth("fake-wrong-key")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    for request in [
        client.get(&data).bearer_auth("fake-client-key"),
        client.post(&data).header("x-api-key", "fake-client-key"),
        client
            .get(&data)
            .header("host", format!("localhost:{}", servers.data_addr.port()))
            .bearer_auth("fake-client-key"),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }
    for request in [
        client.get(&data).bearer_auth("fake-admin-token"),
        client.get(&data).header("x-api-key", "fake-admin-token"),
        client.get(&admin).bearer_auth("fake-client-key"),
        client.get(&admin).header("x-api-key", "fake-admin-token"),
        client
            .get(&data)
            .bearer_auth("fake-admin-token")
            .header("x-api-key", "fake-client-key"),
    ] {
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let response = client
        .get(&admin)
        .bearer_auth("fake-admin-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().await.unwrap();
    let health: qb_proto::HealthResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        serde_json::to_vec(&health).unwrap().as_slice(),
        body.as_ref()
    );
    assert!(health.ok);
    assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(health.pid, std::process::id());
    chrono::DateTime::parse_from_rfc3339(&health.started_at).unwrap();
    let extra = format!("http://{}/admin/v1/extra", servers.admin_addr);
    assert_eq!(
        client.get(&extra).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&extra)
            .bearer_auth("fake-admin-token")
            .header("origin", "null")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .get(&extra)
            .bearer_auth("fake-admin-token")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "extra"
    );
    drop(servers);
}

#[tokio::test]
async fn spawn_wrapper_and_drop_release_listeners() {
    let home = home();
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    let servers = http::spawn(&config, secrets()).await.unwrap();
    let data = servers.data_addr;
    let admin = servers.admin_addr;
    drop(servers);
    for addr in [data, admin] {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if tokio::net::TcpStream::connect(addr).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    assert!(home.path().is_dir());
}

#[test]
fn logging_child() {
    let Some(home) = std::env::var_os("QB_M0_LOG_CHILD") else {
        return;
    };
    let guard = logging::init(std::path::Path::new(&home)).unwrap();
    tracing::trace!("trace-level-marker");
    tracing::debug!("debug-level-marker");
    tracing::info!("info-level-marker");
    let span = tracing::info_span!(
        "synthetic",
        access_token = "fake-span-secret",
        client_key = tracing::field::Empty
    );
    span.record("client_key", "fake-recorded-secret");
    let _entered = span.enter();
    tracing::info!(authorization = "fake-auth-secret", "x-api-key" = "fake-api-secret", cookie = "fake-cookie-secret",
        access_token = "fake-access-secret", refresh_token = "fake-refresh-secret", id_token = "fake-id-secret",
        code = "fake-code-secret", client_key = "fake-client-secret", "Authorization" = "fake-case-secret",
        headers = ?[("authorization", "fake-header-secret")], safe_value = "visible-safe", "Synthetic redaction check");
    fs::write(
        std::path::Path::new(&home).join("config.toml"),
        "unknown = true",
    )
    .unwrap();
    Config::load(std::path::Path::new(&home)).unwrap();
    drop(guard);
}

#[test]
fn rolling_logs_and_stderr_redact_fields_headers_and_spans() {
    let home = home();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "logging_child", "--nocapture"])
        .env("QB_M0_LOG_CHILD", home.path())
        .env("QB_HOME", home.path())
        .env_remove("QB_LOG")
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let mut logs = String::new();
    for file in fs::read_dir(home.path().join("logs")).unwrap() {
        logs.push_str(&fs::read_to_string(file.unwrap().path()).unwrap());
    }
    for text in [&stderr, &logs] {
        assert!(text.contains("visible-safe"));
        assert!(text.contains("<redacted>"));
        assert!(text.contains("Unknown configuration key"));
        assert!(!text.contains("fake-"), "synthetic secret leaked into logs");
    }
}

#[test]
fn log_default_info_and_explicit_environment_levels() {
    for (qb, rust, debug, trace) in [
        (None, None, false, false),
        (None, Some("trace"), true, true),
        (Some("debug"), Some("trace"), true, false),
    ] {
        let home = home();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "logging_child", "--nocapture"])
            .env("QB_M0_LOG_CHILD", home.path())
            .env("QB_HOME", home.path())
            .env_remove("QB_LOG")
            .env_remove("RUST_LOG");
        if let Some(value) = qb {
            command.env("QB_LOG", value);
        }
        if let Some(value) = rust {
            command.env("RUST_LOG", value);
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        let file = fs::read_dir(home.path().join("logs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        for text in [
            String::from_utf8(output.stderr).unwrap(),
            fs::read_to_string(file).unwrap(),
        ] {
            assert!(text.contains("info-level-marker"));
            assert_eq!(text.contains("debug-level-marker"), debug);
            assert_eq!(text.contains("trace-level-marker"), trace);
        }
    }
}

#[tokio::test]
async fn r1_n5_jwt_hint_is_data_bearer_only_and_never_echoes_credentials() {
    let mut config = Config::default();
    config.server.data_port = 0;
    config.server.admin_port = 0;
    let servers = http::spawn(&config, secrets()).await.unwrap();
    let client = client();
    let jwt = "eyJsynthetic-header.synthetic-payload.synthetic-signature";
    let hint = "ChatGPT 토큰이 아니라 quota-baton 클라이언트 키를 써야 합니다";
    // admin, path, Authorization, x-api-key, duplicate Authorization, hint, status
    for (admin, path, bearer, api_key, duplicate, hinted, status) in [
        (false, "/v1/messages", Some(jwt), None, false, true, 401),
        (
            false,
            "/v1/messages/count_tokens",
            Some(jwt),
            None,
            false,
            true,
            401,
        ),
        (false, "/v1/responses", Some(jwt), None, false, true, 401),
        (true, "/admin/v1/health", Some(jwt), None, false, false, 401),
        (
            false,
            "/v1/responses",
            Some("fake-invalid-key"),
            None,
            false,
            false,
            401,
        ),
        (false, "/v1/responses", None, None, false, false, 401),
        (false, "/v1/responses", None, Some(jwt), false, false, 401),
        (
            false,
            "/v1/responses",
            Some("fake-admin-token"),
            None,
            false,
            false,
            401,
        ),
        (
            true,
            "/admin/v1/health",
            Some("fake-client-key"),
            None,
            false,
            false,
            401,
        ),
        (false, "/v1/responses", Some(jwt), None, true, false, 401),
        (
            false,
            "/v1/responses",
            Some(jwt),
            Some("fake-client-key"),
            false,
            false,
            404,
        ),
        (
            false,
            "/v1/responses",
            Some(jwt),
            Some("fake-admin-token"),
            false,
            true,
            401,
        ),
    ] {
        let addr = if admin {
            servers.admin_addr
        } else {
            servers.data_addr
        };
        let mut request = client.get(format!("http://{addr}{path}"));
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        if duplicate {
            request = request.header("authorization", "Bearer fake-invalid-key");
        }
        if let Some(api_key) = api_key {
            request = request.header("x-api-key", api_key);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status().as_u16(), status);
        let body = response.text().await.unwrap();
        for credential in [
            jwt,
            "fake-invalid-key",
            "fake-client-key",
            "fake-admin-token",
        ] {
            assert!(
                !body.contains(credential),
                "response echoed a synthetic credential"
            );
        }
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        if status == 401 {
            assert_eq!(body["error"]["type"], "authentication_error");
            assert_eq!(
                body["error"]["message"],
                if hinted {
                    hint
                } else {
                    "Invalid local credential"
                }
            );
            if path.starts_with("/v1/messages") {
                assert_eq!(body["type"], "error");
            }
        } else {
            assert_eq!(body["error"]["message"], "Route not found");
        }
    }
}

#[tokio::test]
async fn r15_http_error_real_protected_bytes_keep_path_specific_shape_and_order() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        let servers = http::spawn(&config, secrets()).await.unwrap();
        let client = client();
        for (admin, path, authenticated, status, kind, message, messages) in [
            (
                true,
                "/admin/v1/health",
                false,
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Invalid local credential",
                false,
            ),
            (
                false,
                "/v1/messages",
                false,
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Invalid local credential",
                true,
            ),
            (
                false,
                "/v1/messages/missing",
                true,
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "Route not found",
                true,
            ),
            (
                false,
                "/v1/responses",
                true,
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "Route not found",
                false,
            ),
            (
                false,
                "/v1/messages-other",
                true,
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "Route not found",
                false,
            ),
        ] {
            let addr = if admin {
                servers.admin_addr
            } else {
                servers.data_addr
            };
            let mut request = client.get(format!("http://{addr}{path}"));
            if authenticated {
                request = request.bearer_auth(if admin {
                    "fake-admin-token"
                } else {
                    "fake-client-key"
                });
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()["content-type"], "application/json");
            let body = response.bytes().await.unwrap();
            let decoded: qb_proto::admin_error::HttpErrorReply =
                serde_json::from_slice(&body).unwrap();
            assert_eq!(decoded.error.kind, kind);
            assert_eq!(decoded.error.message, message);
            assert_eq!(decoded.kind.as_deref(), messages.then_some("error"));
            let suffix = if messages { ",\"type\":\"error\"" } else { "" };
            let expected = format!(
                "{{\"error\":{{\"type\":{kind},\"message\":{message}}}{suffix}}}",
                kind = serde_json::to_string(kind).unwrap(),
                message = serde_json::to_string(message).unwrap(),
            );
            assert!(
                body.as_ref() == expected.as_bytes(),
                "HTTP error shape/order changed"
            );
            assert!(
                serde_json::to_vec(&decoded).unwrap().as_slice() == body.as_ref(),
                "same protected HTTP response did not roundtrip"
            );
        }
        servers.shutdown();
        servers.wait_for_listener_close().await;
    })
    .await
    .expect("synthetic HTTP error contract deadline");
}
