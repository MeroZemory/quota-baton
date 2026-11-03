#[path = "../src/fixture_support.rs"]
mod fixture_support;
#[cfg(windows)]
#[path = "cli/windows_cases.rs"]
mod windows_cases;

#[cfg(windows)]
#[path = "cli/platform/windows.rs"]
mod status_platform;
#[cfg(unix)]
#[path = "cli/platform/unix.rs"]
mod status_platform;

use status_platform::status_without_platform_trailer;

use std::{
    io::{self, Read},
    path::Path,
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{
        Arc, Mutex, Once,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

// Current producer fields surround each presentation edge case. Deliberately
// malformed dates and future health payloads remain unchanged inside the row.
fn current_status_fixture(mut state: serde_json::Value) -> serde_json::Value {
    for row in state["accounts"].as_array_mut().unwrap() {
        let mut complete = serde_json::json!({
            "outside_used":false,"pool_eligible":false,"takeover_pending":false,
            "quota_cooling":null,"quota_credit_releasable":false,"observed_at":null,
            "quota_notice":null,"credits_in_use":false,
            "usage_forecast":{"as_of":"2030-01-01T00:00:00Z","observed_at":null,
                "windows":[],"earliest_window":null,"credit":null,"truncated":false},
            "has_alternative":false,"id":"sample-acct","generation":1,
            "last_refresh_at":null,"provider":"codex","label":"-","email":"",
            "plan":null,"user_state":"active","source":{"kind":"oauth"},
            "health":{"kind":"ready"},"probe_pending":false,
            "pacing":{"backoff_until":null,"step":0,"release_interval_ms":250,"waiting":0},
            "windows":[],"credits":null,"last_used_at":null,"bound_sessions":0,
            "token_expires_at":null,"drain":false,"allow_credits":false,
            "refresh_attention":false,"active_sessions":0,"spread_threshold":0
        });
        complete
            .as_object_mut()
            .unwrap()
            .extend(row.as_object().unwrap().clone());
        if complete["health"]["kind"] == "cooling" {
            complete["health"]
                .as_object_mut()
                .unwrap()
                .entry("scope")
                .or_insert_with(|| "account".into());
        }
        for window in complete["windows"].as_array_mut().unwrap() {
            for key in ["reset_at", "observed_at", "status"] {
                window
                    .as_object_mut()
                    .unwrap()
                    .entry(key)
                    .or_insert(serde_json::Value::Null);
            }
        }
        // Exercise the shared contract before sending the synthetic response.
        let _: qb_proto::AdminAccount = serde_json::from_value(complete.clone()).unwrap();
        *row = complete;
    }
    state
}

struct Core {
    child: Arc<Mutex<Child>>,
    timed_out: Arc<AtomicBool>,
    stop: mpsc::Sender<()>,
    watchdog: Option<std::thread::JoinHandle<()>>,
}
static SERVE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
impl Core {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        status_platform::configure_test_registry(command)?;
        let child = Arc::new(Mutex::new(command.spawn()?));
        let timed_out = Arc::new(AtomicBool::new(false));
        let (stop, stopped) = mpsc::channel();
        let watched = Arc::clone(&child);
        let expired = Arc::clone(&timed_out);
        let watchdog = std::thread::spawn(move || {
            if matches!(
                stopped.recv_timeout(CHILD_TIMEOUT),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                let mut child = watched.lock().unwrap();
                if !matches!(child.try_wait(), Ok(Some(_))) {
                    expired.store(true, Ordering::SeqCst);
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        });
        Ok(Self {
            child,
            timed_out,
            stop,
            watchdog: Some(watchdog),
        })
    }

    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        if self.timed_out.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "CLI child exceeded 30s",
            ));
        }
        self.child.lock().unwrap().try_wait()
    }

    fn wait(&self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                // The watchdog may have killed the child while try_wait took its lock.
                if self.timed_out.load(Ordering::SeqCst) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "CLI child exceeded 30s",
                    ));
                }
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        {
            let mut child = self.child.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

trait BoundedOutput {
    fn bounded_output(&mut self) -> io::Result<Output>;
    fn bounded_output_redirected(&mut self) -> io::Result<Output>;
}

impl BoundedOutput for Command {
    fn bounded_output(&mut self) -> io::Result<Output> {
        capture_output(self, false)
    }

    fn bounded_output_redirected(&mut self) -> io::Result<Output> {
        capture_output(self, true)
    }
}

fn capture_output(command: &mut Command, redirected: bool) -> io::Result<Output> {
    let before = status_platform::real_run_snapshot()?;
    let result = capture_output_inner(command, redirected);
    status_platform::verify_real_run_unchanged(&before)?;
    result
}

fn capture_output_inner(command: &mut Command, redirected: bool) -> io::Result<Output> {
    if !redirected {
        command.stdout(Stdio::piped());
    }
    command.stdin(Stdio::null()).stderr(Stdio::piped());
    let core = Core::spawn(command)?;
    let (stdout, stderr) = {
        let mut child = core.child.lock().unwrap();
        (
            read_output(child.stdout.take()),
            read_output(child.stderr.take()),
        )
    };
    let status = core.wait()?;
    Ok(Output {
        status,
        stdout: stdout
            .recv_timeout(Duration::from_secs(1))
            .map_err(io::Error::other)??,
        stderr: stderr
            .recv_timeout(Duration::from_secs(1))
            .map_err(io::Error::other)??,
    })
}

fn read_output(reader: Option<impl Read + Send + 'static>) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (send, receive) = mpsc::channel();
    // Drain both pipes concurrently, preserving the CLI's piped-stdout coverage.
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = match reader {
            Some(mut reader) => reader.read_to_end(&mut bytes).map(|_| bytes),
            None => Ok(bytes),
        };
        let _ = send.send(result);
    });
    receive
}

// One deadline covers all accepts and stream I/O in each mock server thread,
// so joining it remains bounded even when the CLI skips an expected request.
const MOCK_SERVER_TIMEOUT: Duration = Duration::from_secs(60);

fn mock_io_timeout(deadline: std::time::Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "mock admin deadline exceeded",
        ));
    }
    Ok(remaining.min(Duration::from_secs(5)))
}

fn mock_response(
    listener: &std::net::TcpListener,
    deadline: std::time::Instant,
    route: &str,
    status: &str,
    body: &str,
    bearer: Option<&str>,
) -> io::Result<()> {
    use std::io::Write;
    listener.set_nonblocking(true)?;
    let mut stream = loop {
        mock_io_timeout(deadline)?;
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(mock_io_timeout(deadline)?.min(Duration::from_millis(10)));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    };
    stream.set_nonblocking(false)?;
    let mut request = Vec::new();
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        if request.len() >= 16 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mock request headers too large",
            ));
        }
        stream.set_read_timeout(Some(mock_io_timeout(deadline)?))?;
        let mut chunk = [0; 1024];
        let amount = stream.read(&mut chunk)?;
        if amount == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "CLI closed before sending request headers",
            ));
        }
        request.extend_from_slice(&chunk[..amount]);
    }
    let request = String::from_utf8_lossy(&request);
    assert_eq!(
        request.lines().next(),
        Some(format!("GET {route} HTTP/1.1").as_str()),
        "unexpected mock admin route"
    );
    if let Some(bearer) = bearer {
        assert!(
            request.lines().any(|line| line
                .split_once(':')
                .is_some_and(|(name, value)| name.eq_ignore_ascii_case("authorization")
                    && value.trim() == format!("Bearer {bearer}"))),
            "missing mock admin authorization"
        );
    }
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut rest = response.as_bytes();
    while !rest.is_empty() {
        stream.set_write_timeout(Some(mock_io_timeout(deadline)?))?;
        let amount = stream.write(rest)?;
        if amount == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "mock response write stalled",
            ));
        }
        rest = &rest[amount..];
    }
    Ok(())
}

fn command(home: &Path) -> Command {
    static FEATURES_CHECKED: Once = Once::new();
    FEATURES_CHECKED.call_once(|| {
        let mut help = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
        help.env("QB_HOME", home).args(["serve", "--help"]);
        let output = help.bounded_output().unwrap();
        assert!(output.status.success(), "binary feature probe failed");
        let text = String::from_utf8(output.stdout).unwrap();
        for (switch, enabled) in [
            ("--capture-dir", cfg!(feature = "shape-capture")),
            ("--capture-bodies-dir", cfg!(feature = "phase0")),
            ("--phase0-close-after-response", cfg!(feature = "phase0")),
        ] {
            assert_eq!(
                text.contains(switch),
                enabled,
                "CLI binary feature mismatch: {switch}"
            );
        }
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
    cmd.env("QB_HOME", home);
    cmd
}

fn fixture() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/tests");
    std::fs::create_dir_all(&root).unwrap();
    let home = qb_testkit::fixture::tempdir_in(root.canonicalize().unwrap()).unwrap();
    let data = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    fixture_support::write_private(
        home.path().join("config.toml"),
        format!(
            "[server]\ndata_port={}\nadmin_port={}\n[secrets]\nlinux_backend='file-0600'\n",
            data.local_addr().unwrap().port(),
            admin.local_addr().unwrap().port(),
        ),
    )
    .unwrap();
    home
}

async fn wait_for_core(client: &reqwest::Client, home: &Path, port: u16, core: &Core) {
    let pid = core.child.lock().unwrap().id();
    let health = format!("http://127.0.0.1:{port}/admin/v1/health");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = core.try_wait().unwrap() {
                panic!("synthetic core {pid} exited before authenticated readiness: {status}");
            }
            let token = std::fs::read_to_string(home.join("admin.token")).unwrap_or_default();
            if !token.trim().is_empty()
                && let Ok(response) = client
                    .get(&health)
                    .bearer_auth(token.trim())
                    .timeout(Duration::from_millis(250))
                    .send()
                    .await
                && response.status().is_success()
                && let Ok(state) = response.json::<serde_json::Value>().await
                && state["ok"] == true
                && state["pid"].as_u64() == Some(u64::from(pid))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("synthetic core {pid} did not become authenticated/healthy within 15s")
    });
}

#[test]
fn help_is_available() {
    let home = fixture();
    let output = command(home.path()).arg("--help").bounded_output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("serve"));
}

#[test]
fn refresh_usage_is_exposed_and_requires_a_running_core() {
    let home = fixture();
    let help = command(home.path())
        .args(["accounts", "--help"])
        .bounded_output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("refresh-usage"));
    let result = command(home.path())
        .args(["accounts", "refresh-usage", "cx-synthetic"])
        .bounded_output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("코어를 먼저 실행"));
    assert!(!home.path().join("accounts").exists());
}

#[cfg(not(feature = "phase0"))]
#[test]
fn default_cli_has_no_phase0_or_body_capture() {
    let home = fixture();
    for args in [
        vec!["phase0", "replay"],
        vec!["serve", "--capture-bodies-dir", ".sandbox"],
        vec!["serve", "--phase0-close-after-response"],
    ] {
        let output = command(home.path()).args(args).bounded_output().unwrap();
        assert!(!output.status.success());
    }
    assert!(!home.path().join("accounts").exists());
}

#[tokio::test]
async fn startup_recovers_corrupt_quota_and_listens() {
    let _serial = SERVE_SERIAL.lock().await;
    let home = fixture();
    // Supply a real pre-existing corrupt record, not an unsafe-mode directory
    // that would be refused before the recovery parser runs.
    fixture_support::private_directory(home.path().join("state")).unwrap();
    fixture_support::write_private(
        home.path().join("state/quota.json"),
        "invalid synthetic state",
    )
    .unwrap();
    let data = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let data_port = data.local_addr().unwrap().port();
    let admin_port = admin.local_addr().unwrap().port();
    std::fs::write(
        home.path().join("config.toml"),
        format!("[server]\ndata_port = {data_port}\nadmin_port = {admin_port}\n[secrets]\nlinux_backend='file-0600'\n"),
    )
    .unwrap();
    drop((data, admin));
    // 경고 출력도 프로젝트 안의 가짜 홈에서만 보관한다.
    let stderr_path = home.path().join("serve.stderr");
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let core = Core::spawn(
        command(home.path())
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .stdin(Stdio::null()),
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    wait_for_core(&client, home.path(), admin_port, &core).await;
    assert!(core.try_wait().unwrap().is_none());
    let backups: Vec<_> = std::fs::read_dir(home.path().join("state"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("quota.json.corrupt-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&backups[0]).unwrap(),
        "invalid synthetic state"
    );
    assert!(!home.path().join("state/quota.json").exists());
    let stderr = std::fs::read_to_string(stderr_path).unwrap();
    assert!(stderr.contains("Corrupt quota state renamed; starting empty"));
    assert!(!stderr.contains("invalid synthetic state"));
}

#[cfg(not(feature = "shape-capture"))]
#[test]
fn default_cli_has_no_capture_switch() {
    let home = fixture();
    let output = command(home.path())
        .args(["serve", "--capture-dir"])
        .arg(home.path())
        .bounded_output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!home.path().join("accounts").exists());
}

#[cfg(feature = "shape-capture")]
#[test]
fn capture_cli_requires_existing_sandbox_directory() {
    let _serial = SERVE_SERIAL.blocking_lock();
    let home = fixture();
    let output = command(home.path())
        .args(["serve", "--capture-dir"])
        .arg(home.path().join("missing"))
        .bounded_output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("capture"));
}

#[tokio::test]
async fn serve_and_status_use_isolated_home() {
    let _serial = SERVE_SERIAL.lock().await;
    let home = fixture();
    let data = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let data_port = data.local_addr().unwrap().port();
    let admin_port = admin.local_addr().unwrap().port();
    std::fs::write(
        home.path().join("config.toml"),
        format!("[server]\ndata_port = {data_port}\nadmin_port = {admin_port}\n[secrets]\nlinux_backend='file-0600'\n"),
    )
    .unwrap();
    drop((data, admin));
    let core = Core::spawn(
        command(home.path())
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null()),
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    wait_for_core(&client, home.path(), admin_port, &core).await;
    let output = command(home.path()).arg("status").bounded_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = String::from_utf8(output.stdout).unwrap();
    assert!(status.contains("코어: 실행 중"));
    assert!(status.contains("계정 ID"));
    assert!(
        !status.contains("5시간 남음"),
        "empty state has no invented quota windows"
    );
    let key = std::fs::read_to_string(home.path().join("client.key")).unwrap();
    assert_eq!(
        client
            .post(format!("http://127.0.0.1:{data_port}/v1/messages"))
            .bearer_auth(key.trim())
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[test]
fn responding_unhealthy_admin_never_falls_back_to_disk() {
    for (status, body) in [
        ("401 Unauthorized", "{}"),
        ("503 Service Unavailable", "{}"),
        (
            "200 OK",
            r#"{"ok":false,"version":"synthetic","pid":0,"started_at":"2030-01-01T00:00:00Z"}"#,
        ),
    ] {
        for args in [vec!["status"], vec!["accounts", "pause", "cx-synthetic"]] {
            let home = fixture();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            std::fs::write(
                home.path().join("config.toml"),
                format!(
                    "[server]\nadmin_port={}\n",
                    listener.local_addr().unwrap().port()
                ),
            )
            .unwrap();
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + MOCK_SERVER_TIMEOUT;
                mock_response(&listener, deadline, "/admin/v1/health", status, body, None).unwrap();
            });
            let output = command(home.path()).args(args).bounded_output().unwrap();
            server.join().unwrap();
            assert!(!output.status.success());
            assert!(
                !home.path().join("accounts").exists(),
                "responding admin caused disk fallback"
            );
        }
    }
}

#[test]
fn offline_status_does_not_create_a_store() {
    let home = fixture();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[server]\nadmin_port={}\n",
            admin.local_addr().unwrap().port()
        ),
    )
    .unwrap();
    drop(admin);
    let output = command(home.path()).arg("status").bounded_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout).unwrap().contains("중지됨"));
    assert!(!home.path().join("accounts").exists());
    assert!(!home.path().join("state").exists());
}

#[test]
fn online_status_unknown_health_is_utf8_safe_and_omits_secret_fields() {
    let home = fixture();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[server]\nadmin_port={}\n",
            listener.local_addr().unwrap().port()
        ),
    )
    .unwrap();
    fixture_support::write_private(home.path().join("admin.token"), "synthetic-admin-secret")
        .unwrap();
    let state = current_status_fixture(serde_json::json!({"accounts":[{
        "id":"cx-synthetic", "provider":"codex", "label":"합성 🧪\t계정\n\u{1b}",
        "user_state":"active", "health":{"kind":"future_health", "reason":{"access_token":"synthetic-reason-secret","nested_token":"synthetic-access-secret","secret_dpapi_b64":"synthetic-dpapi-secret"}},
        "windows":[]
    }]}));
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + MOCK_SERVER_TIMEOUT;
        for (route, body) in [
            ("/admin/v1/health", serde_json::json!({"ok":true,"version":"synthetic","pid":0,"started_at":"2030-01-01T00:00:00Z"}).to_string()),
            ("/admin/v1/state", state.to_string()),
        ] {
            mock_response(&listener, deadline, route, "200 OK", &body, Some("synthetic-admin-secret")).unwrap();
        }
    });
    let output = command(home.path()).arg("status").bounded_output().unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = String::from_utf8(output.stdout).unwrap();
    assert!(status.contains("알 수 없음"));
    assert!(status.contains("합성 🧪 계정"));
    assert!(!status.contains('\u{1b}'));
    let table_status = status_without_platform_trailer(&status);
    assert_eq!(table_status.lines().count(), 6);
    assert_eq!(table_status.lines().last(), Some("  소진 예측: 측정 중"));
    for secret in [
        "synthetic-admin-secret",
        "synthetic-access-secret",
        "synthetic-dpapi-secret",
        "synthetic-reason-secret",
    ] {
        assert!(!status.contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    assert!(!home.path().join("accounts").exists());
}

#[test]
fn online_status_piped_and_file_stdout_are_utf8_with_expired_windows_and_probe_pending() {
    let home = fixture();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[server]\nadmin_port={}\n",
            listener.local_addr().unwrap().port()
        ),
    )
    .unwrap();
    fixture_support::write_private(home.path().join("admin.token"), "synthetic-admin-token")
        .unwrap();
    let state = current_status_fixture(serde_json::json!({"accounts":[
        {"id":"cx-expired-five", "provider":"codex", "label":"합성 🧪", "user_state":"active",
         "health":{"kind":"ready"}, "probe_pending":true, "windows":[
            {"name":"5h", "used_pct":100.0, "reset_at":"2000-01-01T09:00:00.123+09:00"},
            {"name":"7d", "used_pct":37.5, "reset_at":"2099-01-07T00:00:00Z"}
         ]},
        {"id":"cx-expired-seven", "provider":"codex", "label":"합성 계정", "user_state":"active",
         "health":{"kind":"ready"}, "probe_pending":false, "windows":[
            {"name":"5h", "used_pct":91.0, "reset_at":"2099-01-01T05:00:00Z"},
            {"name":"7d", "used_pct":99.0, "reset_at":"2000-01-07T00:00:00Z"}
         ]},
        {"id":"cx-missing-reset", "provider":"codex", "user_state":"active",
         "health":{"kind":"ready"}, "windows":[{"name":"5h", "used_pct":12.0}]},
        {"id":"cx-invalid-reset", "provider":"codex", "user_state":"active",
         "health":{"kind":"cooling", "reason":"auth", "until":"2099-01-01T00:00:00Z"},
         "windows":[{"name":"7d", "used_pct":25.0, "reset_at":"invalid"},
                    {"name":"43200m", "used_pct":2.0, "reset_at":"2099-02-01T00:00:00Z"}]}
    ]}));
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + MOCK_SERVER_TIMEOUT;
        // Both process launches query health, then state; no real core or upstream.
        for _ in 0..2 {
            for (route, body) in [
                ("/admin/v1/health", serde_json::json!({"ok":true,"version":"synthetic","pid":0,"started_at":"2030-01-01T00:00:00Z"}).to_string()),
                ("/admin/v1/state", state.to_string()),
            ] {
                mock_response(&listener, deadline, route, "200 OK", &body, Some("synthetic-admin-token")).unwrap();
            }
        }
    });
    let expected = concat!(
        "코어: 실행 중\n",
        "claude: 사용 가능 0 · 대기 [] · 사용 불가 0\n",
        "codex: 사용 가능 2 · 대기 [인증 오류 1] · 사용 불가 1\n",
        "계정 ID\t제공자\t라벨\t출처\t갱신 주체\t사용 상태\t계정 상태\t5시간 남음\t5시간 초기화\t7일 남음\t7일 초기화\t30일 남음\t30일 초기화\t상태 이유\t대기 종료\n",
        "cx-expired-five\tcodex\t합성 🧪\t직접 로그인\tqb\t활성\t인증 확인 대기\t리셋됨\t2000-01-01T09:00:00.123+09:00\t62.5%\t2099-01-07T00:00:00Z\t-\t-\t-\t-\n",
        "  소진 예측: 측정 중\n",
        "cx-expired-seven\tcodex\t합성 계정\t직접 로그인\tqb\t활성\t준비됨\t9%\t2099-01-01T05:00:00Z\t리셋됨\t2000-01-07T00:00:00Z\t-\t-\t-\t-\n",
        "  소진 예측: 측정 중\n",
        "cx-missing-reset\tcodex\t-\t직접 로그인\tqb\t활성\t준비됨\t88%\t-\t-\t-\t-\t-\t-\t-\n",
        "  소진 예측: 측정 중\n",
        "cx-invalid-reset\tcodex\t-\t직접 로그인\tqb\t활성\t대기 중\t-\t-\t75%\tinvalid\t98%\t2099-02-01T00:00:00Z\t인증 오류\t2099-01-01T00:00:00Z\n",
        "  소진 예측: 측정 중\n",
    );
    let piped = command(home.path()).arg("status").bounded_output().unwrap();
    let output_path = home.path().join("status.stdout");
    let redirected = command(home.path())
        .arg("status")
        .stdout(Stdio::from(std::fs::File::create(&output_path).unwrap()))
        .bounded_output_redirected()
        .unwrap();
    server.join().unwrap();
    assert!(piped.status.success(), "{:?}", piped.stderr);
    assert!(redirected.status.success(), "{:?}", redirected.stderr);
    assert!(redirected.stdout.is_empty());
    let file_bytes = std::fs::read(output_path).unwrap();
    for bytes in [&piped.stdout, &file_bytes] {
        let table_status = status_without_platform_trailer(std::str::from_utf8(bytes).unwrap());
        let header: Vec<_> = table_status
            .lines()
            .find(|line| line.starts_with("계정 ID\t"))
            .unwrap()
            .split('\t')
            .collect();
        let origin = header.iter().position(|name| *name == "출처").unwrap();
        let refresher = header.iter().position(|name| *name == "갱신 주체").unwrap();
        let rows: Vec<_> = table_status
            .lines()
            .filter(|line| line.starts_with("cx-"))
            .collect();
        assert_eq!(rows.len(), 4);
        for row in rows {
            let cells: Vec<_> = row.split('\t').collect();
            assert_eq!(cells.len(), header.len());
            assert_eq!(cells[origin], "직접 로그인");
            assert_eq!(cells[refresher], "qb");
        }
        assert_eq!(table_status, expected);
        assert_eq!(table_status.as_bytes(), expected.as_bytes());
        assert!(!bytes.starts_with(&[0xef, 0xbb, 0xbf]));
    }
    assert!(!home.path().join("accounts").exists());
}

#[test]
fn c6_4_status_renders_per_provider_pool_counts() {
    let home = fixture();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[server]\nadmin_port={}\n",
            listener.local_addr().unwrap().port()
        ),
    )
    .unwrap();
    fixture_support::write_private(home.path().join("admin.token"), "synthetic-admin-secret")
        .unwrap();
    let state = current_status_fixture(serde_json::json!({"accounts":[
        {"id":"a","provider":"claude","user_state":"active","health":{"kind":"ready"}},
        {"id":"b","provider":"claude","user_state":"active","health":{"kind":"cooling","reason":"payment","until":"2099-01-01T00:00:00Z"}},
        {"id":"c","provider":"codex","user_state":"disabled","health":{"kind":"ready"}}
    ]}));
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + MOCK_SERVER_TIMEOUT;
        for (route, body) in [
            ("/admin/v1/health", serde_json::json!({"ok":true,"version":"synthetic","pid":0,"started_at":"2030-01-01T00:00:00Z"}).to_string()),
            ("/admin/v1/state", state.to_string()),
        ] {
            mock_response(&listener, deadline, route, "200 OK", &body, Some("synthetic-admin-secret")).unwrap();
        }
    });
    let output = command(home.path()).arg("status").bounded_output().unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("claude: 사용 가능 1 · 대기 [결제 필요 1] · 사용 불가 0"));
    assert!(stdout.contains("codex: 사용 가능 0 · 대기 [] · 사용 불가 1"));
}
