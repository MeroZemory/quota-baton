use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

const SECRET: &str = "synthetic-provider-code-and-token-never-print";

fn device_start(expires_in: u64) -> Reply {
    Reply {
        method: "POST",
        path: "/admin/v1/accounts/login",
        status: "200 OK",
        body: serde_json::json!({"login_id":"synthetic-login",
            "auth_url":"https://auth.openai.com/codex/device",
            "device":{"verification_url":"https://auth.openai.com/codex/device",
                "user_code":"ABCD-EFGH","expires_in":expires_in}}),
        request: Some(
            serde_json::json!({"provider":"codex","replace_linked":false,"no_browser":true}),
        ),
    }
}

fn device_pending() -> Reply {
    Reply {
        method: "GET",
        path: "/admin/v1/accounts/login/synthetic-login",
        status: "200 OK",
        body: serde_json::json!({"status":"pending"}),
        request: None,
    }
}

fn paste_replies() -> Vec<Reply> {
    vec![
        Reply {
            method: "POST",
            path: "/admin/v1/accounts/login",
            status: "200 OK",
            body: serde_json::json!({"login_id":"synthetic-paste","auth_url":"https://auth.openai.com/oauth/authorize"}),
            request: Some(
                serde_json::json!({"provider":"codex","replace_linked":false,"no_browser":true,"paste_redirect":true}),
            ),
        },
        Reply {
            method: "POST",
            path: "/admin/v1/accounts/login/synthetic-paste/submit",
            status: "200 OK",
            body: serde_json::json!({"status":"pending"}),
            request: Some(
                serde_json::json!({"input":format!("http://localhost:1455/auth/callback?code={SECRET}&state=synthetic")}),
            ),
        },
        Reply {
            method: "GET",
            path: "/admin/v1/accounts/login/synthetic-paste",
            status: "200 OK",
            body: serde_json::json!({"status":"pending"}),
            request: None,
        },
        Reply {
            method: "GET",
            path: "/admin/v1/accounts/login/synthetic-paste",
            status: "200 OK",
            body: serde_json::json!({"status":"completed","account_id":SECRET}),
            request: None,
        },
    ]
}

#[test]
fn device_enter_cancels_checks_terminal_then_starts_one_paste_with_retained_input() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let home = fixture(listener.local_addr().unwrap().port());
    let mut replies = vec![
        health("200 OK", true),
        device_start(900),
        device_pending(),
        cancel(),
        Reply {
            method: "GET",
            path: "/admin/v1/accounts/login/synthetic-login",
            status: "200 OK",
            body: serde_json::json!({"status":"cancelled"}),
            request: None,
        },
    ];
    replies.extend(paste_replies());
    let server = serve_manual(listener, replies);
    let input = format!("\nhttp://localhost:1455/auth/callback?code={SECRET}&state=synthetic\n");
    let result = output_with_stdin(
        command(home.path()).args(["accounts", "login", "codex", "--no-browser"]),
        Some(input.as_bytes()),
        false,
    );
    server.join().unwrap();
    assert!(result.status.success());
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert_eq!(stdout.matches("ABCD-EFGH").count(), 1);
    assert!(stdout.contains("ChatGPT Security"));
    assert!(stdout.contains("workspace admin"));
    assert!(stdout.contains("로그인을 완료했습니다"));
    no_secrets(&result);
}

#[test]
fn completed_device_cancel_race_never_starts_paste() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let home = fixture(listener.local_addr().unwrap().port());
    let server = serve_manual(
        listener,
        vec![
            health("200 OK", true),
            device_start(900),
            device_pending(),
            cancel(),
            Reply {
                method: "GET",
                path: "/admin/v1/accounts/login/synthetic-login",
                status: "200 OK",
                body: serde_json::json!({"status":"completed","account_id":SECRET}),
                request: None,
            },
        ],
    );
    let result = output_with_stdin(
        command(home.path()).args(["accounts", "login", "codex", "--no-browser"]),
        Some(b"\n"),
        false,
    );
    server.join().unwrap();
    assert!(result.status.success());
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.contains("로그인을 완료했습니다"));
    assert!(!stdout.contains("/oauth/authorize"));
    no_secrets(&result);
}

#[test]
fn fixed_device_start_errors_offer_one_explicit_paste_without_code_retry() {
    use qb_core::oauth::LoginError;
    for error in [LoginError::DeviceUnavailable, LoginError::DeviceRateLimited] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let home = fixture(listener.local_addr().unwrap().port());
        let mut replies = vec![
            health("200 OK", true),
            Reply {
                method: "POST",
                path: "/admin/v1/accounts/login",
                status: "400 Bad Request",
                body: serde_json::json!({"error":{"message":error.to_string()}}),
                request: Some(
                    serde_json::json!({"provider":"codex","replace_linked":false,"no_browser":true}),
                ),
            },
        ];
        replies.extend(paste_replies());
        let server = serve_manual(listener, replies);
        let input =
            format!("\nhttp://localhost:1455/auth/callback?code={SECRET}&state=synthetic\n");
        let result = output_with_stdin(
            command(home.path()).args(["accounts", "login", "codex", "--no-browser"]),
            Some(input.as_bytes()),
            false,
        );
        server.join().unwrap();
        assert!(result.status.success());
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("Enter"));
        if matches!(error, LoginError::DeviceUnavailable) {
            assert!(stdout.contains("ChatGPT Security"));
            assert!(stdout.contains("workspace admin"));
        } else {
            assert!(stdout.contains("자동 요청하지 않습니다"));
        }
        no_secrets(&result);
    }
}

#[test]
fn device_deadline_expires_without_requesting_another_code() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let home = fixture(listener.local_addr().unwrap().port());
    let server = serve_manual(
        listener,
        vec![
            health("200 OK", true),
            device_start(1),
            device_pending(),
            cancel(),
        ],
    );
    let result = output_with_stdin(
        command(home.path()).args(["accounts", "login", "codex", "--no-browser"]),
        None,
        true,
    );
    server.join().unwrap();
    assert!(!result.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(combined.contains("만료"));
    assert_eq!(combined.matches("ABCD-EFGH").count(), 1);
    no_secrets(&result);
}

#[cfg(windows)]
#[path = "m1b_cli/windows.rs"]
mod platform;
#[cfg(unix)]
#[path = "m1b_cli/unix.rs"]
mod platform;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
    command.env("QB_HOME", home).stdin(Stdio::null());
    command
}

fn output(command: &mut Command) -> Output {
    output_with_stdin(command, None, false)
}

// Keep a pending pipe open until the child exits, without joining a writer or
// relying on terminal input. This also exercises cancellation with no input.
fn output_with_stdin(command: &mut Command, input: Option<&[u8]>, pending: bool) -> Output {
    if input.is_some() || pending {
        command.stdin(Stdio::piped());
    }
    let child = Arc::new(Mutex::new(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let mut stdin = child.lock().unwrap().stdin.take();
    if let Some(input) = input {
        stdin.as_mut().unwrap().write_all(input).unwrap();
        drop(stdin.take());
    }
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
            true
        } else {
            false
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
    assert!(!watchdog.join().unwrap(), "CLI exceeded test deadline");
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

fn fixture(port: u16) -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join(".sandbox/tests");
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
    serve_replies(listener, replies, false)
}

fn serve_manual(listener: TcpListener, replies: Vec<Reply>) -> std::thread::JoinHandle<()> {
    serve_replies(listener, replies, true)
}

fn serve_replies(
    listener: TcpListener,
    mut replies: Vec<Reply>,
    manual: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        'requests: while !replies.is_empty() {
            assert!(Instant::now() < deadline, "mock admin accept timed out");
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
                if manual && count == 0 && bytes.is_empty() {
                    // Selecting ready stdin can cancel an in-flight status
                    // request after connect, before any HTTP bytes are sent.
                    continue 'requests;
                }
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            };
            let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            let request_line = headers.lines().next().unwrap();
            let index = if manual {
                replies
                    .iter()
                    .position(|reply| {
                        request_line == format!("{} {} HTTP/1.1", reply.method, reply.path)
                    })
                    .expect("unexpected manual-login request")
            } else {
                0
            };
            // Input and status polling race in the real CLI. Until submission,
            // status must stay pending; completion cannot consume the input.
            let keep_pending = manual
                && replies[index].body["status"] == "pending"
                && replies[index].method == "GET"
                && replies.iter().any(|reply| {
                    reply.path.ends_with("/submit") || reply.path.ends_with("/cancel")
                });
            let reply = &replies[index];
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
            if let Some(request) = &reply.request {
                assert_eq!(
                    &serde_json::from_slice::<serde_json::Value>(&bytes[header_end..]).unwrap(),
                    request
                );
            }
            let body = reply.body.to_string();
            let written = write!(
                stream,
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.status,
                body.len(),
                body
            );
            if manual && reply.method == "GET" && written.is_err() {
                // Enter may also cancel a status request after its headers
                // arrived. Keep its response available for the next poll.
                continue 'requests;
            }
            written.unwrap();
            if !keep_pending {
                let cancelled = reply.path.ends_with("/cancel");
                let cancelled_path = reply.path.strip_suffix("/cancel");
                replies.remove(index);
                if manual && cancelled {
                    replies.retain(|reply| {
                        !(reply.method == "GET"
                            && reply.body["status"] == "pending"
                            && Some(reply.path) == cancelled_path)
                    });
                }
            }
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
    for word in ["claude", "codex", "--no-browser"] {
        assert!(help.contains(word));
    }
    assert_eq!(
        help.contains("--replace-linked"),
        platform::REPLACE_LINKED_SUPPORTED
    );
    let invalid =
        output(command(home.path()).args(["accounts", "login", "unknown", "--no-browser"]));
    assert!(!invalid.status.success());
}

#[test]
fn online_login_polls_both_providers_without_browser_and_forwards_override() {
    for (provider, replace) in [
        ("claude", false),
        ("codex", platform::REPLACE_LINKED_SUPPORTED),
    ] {
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
                "https://platform.claude.com/oauth/code/callback",
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
        let submitted = if provider == "claude" {
            format!("{SECRET}#synthetic-state")
        } else {
            format!("{redirect}?code={SECRET}&state=synthetic-state")
        };
        let server = serve_manual(
            listener,
            vec![
                health("200 OK", true),
                Reply {
                    method: "POST",
                    path: "/admin/v1/accounts/login",
                    status: "200 OK",
                    body: serde_json::json!({"login_id":"synthetic-login","auth_url":url}),
                    request: Some(
                        serde_json::json!({"provider":provider,"replace_linked":replace,"no_browser":true}),
                    ),
                },
                Reply {
                    method: "POST",
                    path: "/admin/v1/accounts/login/synthetic-login/submit",
                    status: "200 OK",
                    body: serde_json::json!({"status":"pending"}),
                    request: Some(serde_json::json!({"input":submitted})),
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
        let result =
            output_with_stdin(&mut cmd, Some(format!("{submitted}\r\n").as_bytes()), false);
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
                    request: Some(
                        serde_json::json!({"provider":"codex","replace_linked":false,"no_browser":true}),
                    ),
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
        let result = output_with_stdin(
            command(home.path()).args(["accounts", "login", "codex", "--no-browser"]),
            None,
            true,
        );
        server.join().unwrap();
        assert!(!result.status.success());
        no_secrets(&result);
    }
}

#[test]
fn manual_pending_stdin_does_not_block_cancelled_login_shutdown() {
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
                request: Some(
                    serde_json::json!({"provider":"codex","replace_linked":false,"no_browser":true}),
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
                body: serde_json::json!({"status":"cancelled","error":SECRET}),
                request: None,
            },
        ],
    );
    let result = output_with_stdin(
        command(home.path()).args(["accounts", "login", "codex", "--no-browser"]),
        None,
        true,
    );
    server.join().unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("로그인을 취소했습니다"));
    no_secrets(&result);
}

#[test]
fn manual_eof_and_invalid_input_cancel_without_printing_secrets() {
    let mut invalid = SECRET.as_bytes().to_vec();
    invalid.extend_from_slice(b"\xff\n");
    for input in [None, Some(invalid.as_slice())] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let home = fixture(listener.local_addr().unwrap().port());
        let server = serve_manual(
            listener,
            vec![
                health("200 OK", true),
                Reply {
                    method: "POST",
                    path: "/admin/v1/accounts/login",
                    status: "200 OK",
                    body: serde_json::json!({"login_id":"synthetic-login","auth_url":"https://auth.openai.com/oauth/authorize"}),
                    request: Some(
                        serde_json::json!({"provider":"codex","replace_linked":false,"no_browser":true}),
                    ),
                },
                Reply {
                    method: "GET",
                    path: "/admin/v1/accounts/login/synthetic-login",
                    status: "200 OK",
                    body: serde_json::json!({"status":"pending"}),
                    request: None,
                },
                cancel(),
            ]
            .into_iter()
            .chain(input.is_none().then_some(Reply {
                method: "GET",
                path: "/admin/v1/accounts/login/synthetic-login",
                status: "200 OK",
                body: serde_json::json!({"status":"cancelled"}),
                request: None,
            }))
            .collect(),
        );
        let result = output_with_stdin(
            command(home.path()).args(["accounts", "login", "codex", "--no-browser"]),
            input,
            false,
        );
        server.join().unwrap();
        assert!(!result.status.success());
        assert!(!String::from_utf8_lossy(&result.stdout).contains("로그인을 완료했습니다"));
        assert!(!home.path().join("accounts").exists());
        no_secrets(&result);
    }
}

#[test]
fn no_browser_eof_checks_terminal_before_claiming_no_account_changes() {
    for (provider, device) in [("claude", false), ("codex", true), ("codex", false)] {
        for terminal in ["cancelled", "completed", "failed"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let home = fixture(listener.local_addr().unwrap().port());
            let start = if device {
                device_start(900)
            } else {
                let host = if provider == "claude" {
                    "claude.ai"
                } else {
                    "auth.openai.com"
                };
                Reply {
                    method: "POST",
                    path: "/admin/v1/accounts/login",
                    status: "200 OK",
                    body: serde_json::json!({"login_id":"synthetic-login","auth_url":format!("https://{host}/oauth/authorize")}),
                    request: Some(
                        serde_json::json!({"provider":provider,"replace_linked":false,"no_browser":true}),
                    ),
                }
            };
            let mut outcome = serde_json::json!({"status":terminal});
            if terminal == "completed" {
                outcome["account_id"] = SECRET.into();
            } else if terminal == "failed" {
                outcome["error"] = SECRET.into();
            }
            let mut replies = vec![
                health("200 OK", true),
                start,
                device_pending(),
                cancel(),
                Reply {
                    method: "GET",
                    path: "/admin/v1/accounts/login/synthetic-login",
                    status: "200 OK",
                    body: outcome,
                    request: None,
                },
            ];
            if terminal == "failed" {
                // An unverified terminal outcome retains ordinary outer cleanup.
                replies.push(cancel());
            }
            let server = serve_manual(listener, replies);
            let result = output_with_stdin(
                command(home.path()).args(["accounts", "login", provider, "--no-browser"]),
                None,
                false,
            );
            server.join().unwrap();
            assert_eq!(result.status.success(), terminal == "completed");
            let output = format!(
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                output.contains("로그인을 완료했습니다"),
                terminal == "completed"
            );
            assert_eq!(
                output.contains("계정 정보는 변경하지 않았습니다"),
                terminal == "cancelled"
            );
            if terminal == "cancelled" {
                assert!(output.contains("로그인 코드를 입력하지 않았습니다"));
            }
            assert!(!home.path().join("accounts").exists());
            no_secrets(&result);
        }
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
