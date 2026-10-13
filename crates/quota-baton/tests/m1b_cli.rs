use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

const SECRET: &str = "synthetic-provider-code-and-token-never-print";

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
    command.env("QB_HOME", home).stdin(Stdio::null());
    command
}

fn output(command: &mut Command) -> Output {
    let child = Arc::new(Mutex::new(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let (stdout, stderr) = {
        let mut child = child.lock().unwrap();
        (child.stdout.take().unwrap(), child.stderr.take().unwrap())
    };
    fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        })
    }
    let stdout = drain(stdout);
    let stderr = drain(stderr);
    let watched = Arc::clone(&child);
    let (stop, receiver) = mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if receiver.recv_timeout(Duration::from_secs(15)).is_err() {
            let mut child = watched.lock().unwrap();
            let _ = child.kill();
        }
    });
    let deadline = Instant::now() + Duration::from_secs(16);
    let status = loop {
        if let Some(status) = child.lock().unwrap().try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "CLI exceeded test deadline");
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = stop.send(());
    watchdog.join().unwrap();
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

fn fixture(port: u16) -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/tests");
    std::fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(root).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!("[server]\nadmin_port={port}\n"),
    )
    .unwrap();
    std::fs::write(home.path().join("admin.token"), "synthetic-admin-token").unwrap();
    home
}

struct Reply {
    method: &'static str,
    path: &'static str,
    status: &'static str,
    body: serde_json::Value,
    request: Option<serde_json::Value>,
}

fn serve(listener: TcpListener, replies: Vec<Reply>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        for reply in replies {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "mock admin accept timed out");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("mock admin accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break index + 4;
                }
                assert!(bytes.len() < 16 * 1024);
                let mut chunk = [0; 1024];
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            };
            let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            assert_eq!(
                headers.lines().next().unwrap(),
                format!("{} {} HTTP/1.1", reply.method, reply.path)
            );
            let header = |name: &str| {
                headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.trim())
            };
            assert_eq!(
                header("authorization"),
                Some("Bearer synthetic-admin-token")
            );
            assert_eq!(
                header("user-agent"),
                Some(concat!("quota-baton/", env!("CARGO_PKG_VERSION")))
            );
            let length = header("content-length")
                .unwrap_or("0")
                .parse::<usize>()
                .unwrap();
            while bytes.len() < header_end + length {
                let mut chunk = [0; 1024];
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            if let Some(request) = reply.request {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&bytes[header_end..]).unwrap(),
                    request
                );
            }
            let body = reply.body.to_string();
            write!(stream, "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.status, body.len(), body).unwrap();
        }
    })
}

fn health(status: &'static str, ok: bool) -> Reply {
    Reply {
        method: "GET",
        path: "/admin/v1/health",
        status,
        body: serde_json::json!({"ok":ok,"version":"test","pid":1,"started_at":"test"}),
        request: None,
    }
}

fn cancel() -> Reply {
    Reply {
        method: "POST",
        path: "/admin/v1/accounts/login/synthetic-login/cancel",
        status: "200 OK",
        body: serde_json::json!({}),
        request: None,
    }
}

fn no_secrets(result: &Output) {
    for bytes in [&result.stdout, &result.stderr] {
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains(SECRET));
        assert!(!text.contains("synthetic-admin-token"));
    }
}

#[test]
fn login_help_and_provider_validation() {
    let home = fixture(10201);
    let help = output(command(home.path()).args(["accounts", "login", "--help"]));
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for word in ["claude", "codex", "--replace-linked", "--no-browser"] {
        assert!(help.contains(word));
    }
    let invalid =
        output(command(home.path()).args(["accounts", "login", "unknown", "--no-browser"]));
    assert!(!invalid.status.success());
}

#[test]
fn online_login_polls_both_providers_without_browser_and_forwards_override() {
    for (provider, replace) in [("claude", false), ("codex", true)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let home = fixture(listener.local_addr().unwrap().port());
        let host = if provider == "claude" {
            "claude.ai"
        } else {
            "auth.openai.com"
        };
        let mut url = reqwest::Url::parse(&format!("https://{host}/oauth/authorize")).unwrap();
        let (client_id, redirect, scope) = if provider == "claude" {
            (
                "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                "http://localhost:54545/callback",
                "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
            )
        } else {
            (
                "app_EMoamEEZ73f0CkXaXp7hrann",
                "http://localhost:1455/auth/callback",
                "openid email profile offline_access",
            )
        };
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("scope", scope),
            ("state", "synthetic-state"),
            ("code_challenge", "synthetic-pkce"),
            ("code_challenge_method", "S256"),
        ]);
        if provider == "claude" {
            url.query_pairs_mut().append_pair("code", "true");
        } else {
            url.query_pairs_mut().extend_pairs([
                ("prompt", "login"),
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
            ]);
        }
        let url = url.to_string();
        let server = serve(
            listener,
            vec![
                health("200 OK", true),
                Reply {
                    method: "POST",
                    path: "/admin/v1/accounts/login",
                    status: "200 OK",
                    body: serde_json::json!({"login_id":"synthetic-login","auth_url":url}),
                    request: Some(
                        serde_json::json!({"provider":provider,"replace_linked":replace}),
                    ),
                },
                Reply {
                    method: "GET",
                    path: "/admin/v1/accounts/login/synthetic-login",
                    status: "200 OK",
                    body: serde_json::json!({"status":"pending"}),
                    request: None,
                },
                Reply {
                    method: "GET",
                    path: "/admin/v1/accounts/login/synthetic-login",
                    status: "200 OK",
                    body: serde_json::json!({"status":"completed","account_id":SECRET}),
                    request: None,
                },
            ],
        );
        let mut cmd = command(home.path());
        cmd.args(["accounts", "login", provider, "--no-browser"]);
        if replace {
            cmd.arg("--replace-linked");
        }
        let result = output(&mut cmd);
        server.join().unwrap();
        assert!(result.status.success());
        let stdout = String::from_utf8(result.stdout.clone()).unwrap();
        assert!(stdout.contains(&format!("로그인 주소: {url}")));
        assert!(stdout.contains("로그인 대기 중"));
        assert!(stdout.contains("로그인을 완료했습니다"));
        no_secrets(&result);
        assert!(
            !home.path().join("accounts").exists(),
            "online CLI acquired a local lease"
        );
    }
}

#[test]
fn any_responding_unhealthy_admin_prevents_local_login() {
    for (status, ok) in [
        ("401 Unauthorized", false),
        ("503 Service Unavailable", false),
        ("200 OK", false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let home = fixture(listener.local_addr().unwrap().port());
        let server = serve(listener, vec![health(status, ok)]);
        let result =
            output(command(home.path()).args(["accounts", "login", "codex", "--no-browser"]));
        server.join().unwrap();
        assert!(!result.status.success());
        assert!(!home.path().join("accounts").exists());
        assert!(!home.path().join("ledger").exists());
        assert!(!String::from_utf8_lossy(&result.stdout).contains("로그인 주소"));
        no_secrets(&result);
    }
}

#[test]
fn terminal_failures_hide_raw_provider_payload_and_close_login() {
    for state in ["failed", "expired", "invalid"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let home = fixture(listener.local_addr().unwrap().port());
        let server = serve(
            listener,
            vec![
                health("200 OK", true),
                Reply {
                    method: "POST",
                    path: "/admin/v1/accounts/login",
                    status: "200 OK",
                    body: serde_json::json!({"login_id":"synthetic-login","auth_url":"https://auth.openai.com/oauth/authorize"}),
                    request: Some(serde_json::json!({"provider":"codex","replace_linked":false})),
                },
                Reply {
                    method: "GET",
                    path: "/admin/v1/accounts/login/synthetic-login",
                    status: "200 OK",
                    body: serde_json::json!({"status":state,"error":SECRET,"account_id":SECRET}),
                    request: None,
                },
                cancel(),
            ],
        );
        let result =
            output(command(home.path()).args(["accounts", "login", "codex", "--no-browser"]));
        server.join().unwrap();
        assert!(!result.status.success());
        no_secrets(&result);
    }
}

#[test]
fn callback_url_is_never_printed_or_opened() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let home = fixture(listener.local_addr().unwrap().port());
    let server = serve(
        listener,
        vec![
            health("200 OK", true),
            Reply {
                method: "POST",
                path: "/admin/v1/accounts/login",
                status: "200 OK",
                body: serde_json::json!({"login_id":"synthetic-login","auth_url":format!("http://localhost:1455/auth/callback?code={SECRET}")}),
                request: None,
            },
            cancel(),
        ],
    );
    let result = output(command(home.path()).args(["accounts", "login", "codex", "--no-browser"]));
    server.join().unwrap();
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stdout).contains("로그인 주소"));
    no_secrets(&result);
}

#[test]
fn online_start_failure_uses_safe_korean_message_without_local_fallback() {
    for (error, expected) in [
        (
            qb_core::oauth::LoginError::CallbackPortBusy { port: 1455 }.to_string(),
            "포트 1455가 사용 중",
        ),
        (
            qb_core::oauth::LoginError::LinkedConflict.to_string(),
            "--replace-linked",
        ),
        (SECRET.to_owned(), "로그인에 실패했습니다"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let home = fixture(listener.local_addr().unwrap().port());
        let server = serve(
            listener,
            vec![
                health("200 OK", true),
                Reply {
                    method: "POST",
                    path: "/admin/v1/accounts/login",
                    status: "409 Conflict",
                    body: serde_json::json!({"error":{"message":error,"code":SECRET,"detail":SECRET}}),
                    request: None,
                },
            ],
        );
        let result =
            output(command(home.path()).args(["accounts", "login", "codex", "--no-browser"]));
        server.join().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains(expected));
        assert!(!home.path().join("accounts").exists());
        no_secrets(&result);
    }
}

#[test]
fn authorize_code_values_other_than_claude_true_are_rejected() {
    for (provider, host, query) in [
        ("claude", "claude.ai", format!("code={SECRET}")),
        ("claude", "claude.ai", "code=false".to_owned()),
        ("claude", "claude.ai", format!("code=true&code={SECRET}")),
        ("codex", "auth.openai.com", "code=true".to_owned()),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let home = fixture(listener.local_addr().unwrap().port());
        let server = serve(
            listener,
            vec![
                health("200 OK", true),
                Reply {
                    method: "POST",
                    path: "/admin/v1/accounts/login",
                    status: "200 OK",
                    body: serde_json::json!({"login_id":"synthetic-login","auth_url":format!("https://{host}/oauth/authorize?{query}")}),
                    request: None,
                },
                cancel(),
            ],
        );
        let result =
            output(command(home.path()).args(["accounts", "login", provider, "--no-browser"]));
        server.join().unwrap();
        assert!(!result.status.success());
        assert!(!String::from_utf8_lossy(&result.stdout).contains("로그인 주소"));
        no_secrets(&result);
    }
}
