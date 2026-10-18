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
struct Core {
    child: Arc<Mutex<Child>>,
    timed_out: Arc<AtomicBool>,
    stop: mpsc::Sender<()>,
    watchdog: Option<std::thread::JoinHandle<()>>,
}
static SERVE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
impl Core {
    fn spawn(command: &mut Command) -> io::Result<Self> {
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
    let home = tempfile::tempdir_in(root).unwrap();
    let data = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[server]\ndata_port={}\nadmin_port={}\n",
            data.local_addr().unwrap().port(),
            admin.local_addr().unwrap().port(),
        ),
    )
    .unwrap();
    home
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

#[cfg(feature = "phase0")]
#[tokio::test]
async fn phase0_cli_capture_and_replay_print_counts_and_summary_only() {
    let _serial = SERVE_SERIAL.lock().await;
    let root = fixture();
    let home = root.path().join("home");
    let cpa = root.path().join("cpa");
    let captures = root.path().join("captures");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cpa).unwrap();
    std::fs::create_dir_all(&captures).unwrap();
    std::fs::write(
        cpa.join("codex.json"),
        serde_json::to_vec(&cpa_fixture(1)).unwrap(),
    )
    .unwrap();
    let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
    let data = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let data_port = data.local_addr().unwrap().port();
    let admin_port = admin.local_addr().unwrap().port();
    std::fs::write(home.join("config.toml"), format!("[server]\ndata_port={data_port}\nadmin_port={admin_port}\n[upstream]\ncodex_base='{}'\n", fake.url)).unwrap();
    drop((data, admin));
    let output = command(&home)
        .args(["accounts", "import", "cpa", "--dir"])
        .arg(&cpa)
        .args(["--activate", "codex:synthetic@example.invalid"])
        .bounded_output()
        .unwrap();
    assert!(output.status.success());
    let account = qb_core::accounts::AccountStore::list_metadata(&home)
        .unwrap()
        .remove(0);
    let stderr_path = home.join("serve.stderr");
    let _core = Core::spawn(
        command(&home)
            .args(["serve", "--capture-bodies-dir"])
            .arg(&captures)
            .args(["--capture-bodies-limit", "1"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
            .stdin(Stdio::null()),
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut ready = false;
    for _ in 0..100 {
        if client
            .get(format!("http://127.0.0.1:{admin_port}/admin/v1/health"))
            .send()
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready);
    assert!(
        std::fs::read_to_string(stderr_path)
            .unwrap()
            .contains("대화 내용")
    );
    let key = std::fs::read_to_string(home.join("client.key")).unwrap();
    let body = r#"{"input":[{"type":"reasoning","encrypted_content":"synthetic-encrypted"},{"type":"message","content":[{"type":"input_text","text":"synthetic conversation"}]}]}"#;
    client
        .post(format!("http://127.0.0.1:{data_port}/v1/responses"))
        .bearer_auth(key.trim())
        .body(body)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let capture = std::fs::read_dir(&captures)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut replay = command(&home);
    replay
        .current_dir(&captures)
        .args(["phase0", "replay", "--capture"])
        .arg(capture.file_name().unwrap())
        .args(["--account", &account.id, "--strip", "reasoning"]);
    let output = tokio::task::spawn_blocking(move || replay.bounded_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["encrypted_content"]["reasoning"], 1);
    assert_eq!(lines[1]["strip_encrypted"], "reasoning");
    assert_eq!(lines[1]["status"], 200);
    assert_eq!(lines[1]["account"], account.id);
    for forbidden in [
        "synthetic conversation",
        "synthetic-encrypted",
        "fake-access",
        "fake-refresh",
        key.trim(),
    ] {
        assert!(!text.contains(forbidden));
    }
    let sent = fake.state.requests();
    assert_eq!(sent.len(), 2);
    let stripped: serde_json::Value = serde_json::from_slice(&sent[1].body).unwrap();
    assert_eq!(stripped["input"].as_array().unwrap().len(), 1);
    assert_eq!(std::fs::read_dir(captures).unwrap().count(), 1);
}

#[tokio::test]
async fn startup_recovers_corrupt_quota_and_listens() {
    let _serial = SERVE_SERIAL.lock().await;
    let home = fixture();
    std::fs::create_dir_all(home.path().join("state")).unwrap();
    std::fs::write(
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
        format!("[server]\ndata_port = {data_port}\nadmin_port = {admin_port}\n"),
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
    let mut ready = false;
    for _ in 0..100 {
        let token = std::fs::read_to_string(home.path().join("admin.token")).unwrap_or_default();
        if client
            .get(format!("http://127.0.0.1:{admin_port}/admin/v1/health"))
            .bearer_auth(token.trim())
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            ready = true;
            break;
        }
        if let Some(status) = core.try_wait().unwrap() {
            panic!("serve exited: {status}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "core did not start");
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
        format!("[server]\ndata_port = {data_port}\nadmin_port = {admin_port}\n"),
    )
    .unwrap();
    drop((data, admin));
    let _core = Core::spawn(
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
    let url = format!("http://127.0.0.1:{admin_port}/admin/v1/health");
    let mut ready = false;
    for _ in 0..100 {
        if client.get(&url).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "core did not start");
    let output = command(home.path()).arg("status").bounded_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = String::from_utf8(output.stdout).unwrap();
    assert!(status.contains("코어: 실행 중"));
    assert!(status.contains("5시간 사용률"));
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

fn cpa_fixture(version: u8) -> serde_json::Value {
    serde_json::json!({"type":"codex", "email":"synthetic@example.invalid", "account_id":"fake-account", "plan_type":"plus", "access_token":format!("fake-access-{version}"), "refresh_token":format!("fake-refresh-{version}"), "id_token":format!("fake-id-{version}"), "expired":format!("2030-01-0{version}T02:00:00Z"), "last_refresh":format!("2030-01-0{version}T01:00:00Z")})
}

fn run_import(home: &Path, dir: &Path, extra: &[&str]) -> String {
    let output = command(home)
        .args(["accounts", "import", "cpa", "--dir"])
        .arg(dir)
        .args(extra)
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        for secret in ["fake-access-", "fake-refresh-", "fake-id-"] {
            assert!(!text.contains(secret), "CLI leaked fixture token");
        }
    }
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
async fn offline_online_import_and_admin_refusal() {
    let _serial = SERVE_SERIAL.lock().await;
    let root = fixture();
    let home = root.path().join("home");
    let cpa = root.path().join("cpa");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cpa).unwrap();
    let data = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let data_port = data.local_addr().unwrap().port();
    let admin_port = admin.local_addr().unwrap().port();
    std::fs::write(
        home.join("config.toml"),
        format!("[server]\ndata_port = {data_port}\nadmin_port = {admin_port}\n"),
    )
    .unwrap();
    drop((data, admin));
    let file = cpa.join("original.json");
    std::fs::write(&file, serde_json::to_vec(&cpa_fixture(2)).unwrap()).unwrap();
    let preview = run_import(&home, &cpa, &["--dry-run"]);
    assert!(preview.contains("생성"));
    assert!(!home.join("accounts").exists());
    assert!(!home.join("logs").exists());
    let refused = command(&home)
        .args(["accounts", "import", "cpa", "--adopt", "--dir"])
        .arg(root.path().join("does-not-exist"))
        .bounded_output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("소유권 이전"));
    assert!(!home.join("accounts").exists());
    assert!(run_import(&home, &cpa, &[]).contains("비활성"));
    let core = Core::spawn(
        command(&home)
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
    let base = format!("http://127.0.0.1:{admin_port}/admin/v1");
    let mut ready = false;
    for _ in 0..100 {
        if client.get(format!("{base}/health")).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready);
    let duplicate = command(&home).arg("serve").bounded_output().unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("계정 저장소"));
    let token = std::fs::read_to_string(home.join("admin.token")).unwrap();
    let adopted = client.post(format!("{base}/accounts/import")).bearer_auth(token.trim()).json(&serde_json::json!({"source":"cpa","mode":"adopted","dir":root.path().join("does-not-exist")})).send().await.unwrap();
    assert_eq!(adopted.status(), 400);
    assert!(adopted.text().await.unwrap().contains("소유권 이전"));
    let invalid = client
        .post(format!("{base}/accounts/import"))
        .bearer_auth(token.trim())
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    let _: serde_json::Value = invalid.json().await.unwrap();
    std::fs::write(&file, serde_json::to_vec(&cpa_fixture(1)).unwrap()).unwrap();
    let older = run_import(
        &home,
        &cpa,
        &["--activate", "codex:synthetic@example.invalid"],
    );
    assert!(older.contains("활성"));
    assert!(older.contains("이전 토큰"));
    std::fs::remove_file(&file).unwrap();
    std::fs::write(
        cpa.join("renamed.json"),
        serde_json::to_vec(&cpa_fixture(3)).unwrap(),
    )
    .unwrap();
    let updated = run_import(&home, &cpa, &[]);
    assert!(updated.contains("갱신"));
    let output = command(&home)
        .args(["accounts", "list"])
        .bounded_output()
        .unwrap();
    assert!(output.status.success());
    let listing = String::from_utf8(output.stdout).unwrap();
    assert!(listing.contains("활성"));
    assert!(listing.contains("연결"));
    assert!(listing.contains("준비됨"));
    assert!(!listing.contains('{'));
    assert!(!listing.contains("fake-access-"));
    let id = qb_core::accounts::AccountStore::list_metadata(&home).unwrap()[0]
        .id
        .clone();
    let mut events = client
        .get(format!("{base}/events"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), 200);
    assert!(
        events.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    for (action, expected) in [
        ("pause", "paused"),
        ("resume", "active"),
        ("disable", "disabled"),
        ("enable", "active"),
    ] {
        let mut cmd = command(&home);
        cmd.args(["accounts", action, &id]);
        let output = tokio::task::spawn_blocking(move || cmd.bounded_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let state: serde_json::Value = client
            .get(format!("{base}/state"))
            .bearer_auth(token.trim())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["accounts"][0]["user_state"], expected);
        assert_eq!(state["bindings"]["count"], 0);
        assert_eq!(state["data_port"], data_port);
        assert!(state["inflight"]["requests"].is_number());
        assert!(state["memory"]["private_bytes"].is_number());
        for forbidden in [
            "secret_dpapi_b64",
            "fake-access-",
            "fake-refresh-",
            "fake-id-",
        ] {
            assert!(!state.to_string().contains(forbidden));
        }
    }
    let mut status_command = command(&home);
    status_command.arg("status");
    let output = tokio::task::spawn_blocking(move || status_command.bounded_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("코어: 실행 중"));
    assert!(text.contains("CPA는 계속 실행되어야 합니다 (연결 계정 1개의 토큰 갱신)"));
    assert!(!text.contains("fake-access-"));
    let chunk = tokio::time::timeout(Duration::from_secs(2), events.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&chunk).contains("event: account_changed"));
    let patch = client
        .patch(format!("{base}/accounts/{id}"))
        .bearer_auth(token.trim())
        .json(&serde_json::json!({"label":"합성 계정", "drain":true, "allow_credits":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(patch.status(), 200);
    let patch: serde_json::Value = patch.json().await.unwrap();
    assert_eq!(patch["label"], "합성 계정");
    assert_eq!(patch["drain"], true);
    assert_eq!(patch["allow_credits"], true);
    assert!(patch.get("source").is_none());
    let bad = client
        .patch(format!("{base}/accounts/{id}"))
        .bearer_auth(token.trim())
        .json(&serde_json::json!({"user_state":"disabled"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let unauthenticated = client.get(format!("{base}/state")).send().await.unwrap();
    assert_eq!(unauthenticated.status(), 401);
    let invalid_label = client
        .patch(format!("{base}/accounts/{id}"))
        .bearer_auth(token.trim())
        .json(&serde_json::json!({"label":"invalid\nlabel"}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_label.status(), 400);
    let missing = client
        .post(format!("{base}/accounts/cx-missing/pause"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    std::fs::write(home.join("admin.token"), "synthetic-wrong-admin-token").unwrap();
    for args in [vec!["status"], vec!["accounts", "disable", &id]] {
        let mut cmd = command(&home);
        cmd.args(args);
        let output = tokio::task::spawn_blocking(move || cmd.bounded_output())
            .await
            .unwrap()
            .unwrap();
        assert!(!output.status.success());
    }
    assert_eq!(
        qb_core::accounts::AccountStore::list_metadata(&home).unwrap()[0].user_state,
        qb_core::accounts::UserState::Active
    );
    std::fs::write(home.join("admin.token"), &token).unwrap();
    drop(events);
    let accounts = qb_core::accounts::AccountStore::list_metadata(&home).unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        accounts[0].token_meta.expires_at.unwrap().to_rfc3339(),
        "2030-01-03T02:00:00+00:00"
    );
    let bytes = std::fs::read(
        home.join("accounts")
            .join(format!("{}.json", accounts[0].id)),
    )
    .unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("fake-access-"));
    drop(core);
    let store = qb_core::accounts::AccountStore::acquire(&home).unwrap();
    assert_eq!(
        store.tokens(&accounts[0]).unwrap().access_token,
        "fake-access-3"
    );
}

#[test]
fn offline_status_and_actions_use_synthetic_metadata_and_quota() {
    let _serial = SERVE_SERIAL.blocking_lock();
    let home = fixture();
    let cpa = home.path().join("cpa");
    std::fs::create_dir(&cpa).unwrap();
    std::fs::write(
        cpa.join("synthetic.json"),
        serde_json::to_vec(&cpa_fixture(1)).unwrap(),
    )
    .unwrap();
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
    run_import(home.path(), &cpa, &[]);
    let id = qb_core::accounts::AccountStore::list_metadata(home.path()).unwrap()[0]
        .id
        .clone();
    let health: qb_core::accounts::Health = serde_json::from_value(serde_json::json!({
        "kind":"cooling", "reason":"quota", "until":"2030-01-01T05:00:00Z", "scope":"account"
    }))
    .unwrap();
    {
        let store = qb_core::accounts::AccountStore::acquire(home.path()).unwrap();
        store
            .update_metadata(&id, |account| {
                account.health = health.clone();
                account.label = "합성 계정".into();
            })
            .unwrap();
    }
    std::fs::create_dir_all(home.path().join("state")).unwrap();
    std::fs::write(
        home.path().join("state/quota.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema":1, "accounts":{id.clone():{"windows":[
                {"name":"5h", "used_pct":37.5, "reset_at":"2030-01-01T05:00:00Z"},
                {"name":"7d", "used_pct":91.0, "reset_at":"2030-01-07T00:00:00Z"}
            ]}}
        }))
        .unwrap(),
    )
    .unwrap();
    for (action, expected) in [
        ("pause", qb_core::accounts::UserState::Paused),
        ("resume", qb_core::accounts::UserState::Active),
        ("disable", qb_core::accounts::UserState::Disabled),
        ("enable", qb_core::accounts::UserState::Active),
    ] {
        let output = command(home.path())
            .args(["accounts", action, &id])
            .bounded_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let account = qb_core::accounts::AccountStore::list_metadata(home.path())
            .unwrap()
            .remove(0);
        assert_eq!(account.user_state, expected);
        assert_eq!(account.health, health);
    }
    let output = command(home.path()).arg("status").bounded_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "중지됨",
        "CPA는 계속 실행되어야 합니다 (연결 계정 1개의 토큰 갱신)",
        "대기 중",
        "할당량 소진",
        "합성 계정",
        "37.5%",
        "91.0%",
        "2030-01-01T05:00:00Z",
        "2030-01-07T00:00:00Z",
    ] {
        assert!(status.contains(expected), "missing {expected}: {status}");
    }
    for forbidden in [
        "fake-access-",
        "fake-refresh-",
        "fake-id-",
        "secret_dpapi_b64",
    ] {
        assert!(!status.contains(forbidden));
    }
    // Offline snapshots use the same expiry display as the live admin response.
    std::fs::write(
        home.path().join("state/quota.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema":1, "accounts":{id.clone():{"windows":[
                {"name":"5h", "used_pct":100.0, "reset_at":"2000-01-01T05:00:00Z"},
                {"name":"7d", "used_pct":99.0, "reset_at":"2000-01-07T00:00:00Z"}
            ]}}
        }))
        .unwrap(),
    )
    .unwrap();
    let output = command(home.path()).arg("status").bounded_output().unwrap();
    assert!(output.status.success());
    let status = String::from_utf8(output.stdout).unwrap();
    let columns: Vec<_> = status
        .lines()
        .find(|line| line.starts_with(&format!("{id}\t")))
        .unwrap()
        .split('\t')
        .collect();
    assert_eq!(columns[5], "리셋됨");
    assert_eq!(columns[7], "리셋됨");
    assert!(!status.contains("100.0%"));
    assert!(!status.contains("99.0%"));
    for (health, pending, failures, expected) in [
        (
            serde_json::json!({"kind":"cooling", "reason":"auth", "until":"2000-01-01T00:00:00Z", "scope":"account"}),
            false,
            1,
            "인증 확인 대기",
        ),
        (
            serde_json::json!({"kind":"ready"}),
            false,
            1,
            "인증 확인 대기",
        ),
        (
            serde_json::json!({"kind":"ready"}),
            true,
            0,
            "인증 확인 대기",
        ),
        (serde_json::json!({"kind":"ready"}), false, 0, "준비됨"),
    ] {
        let health: qb_core::accounts::Health = serde_json::from_value(health).unwrap();
        {
            let store = qb_core::accounts::AccountStore::acquire(home.path()).unwrap();
            store
                .update_metadata(&id, |account| {
                    account.health = health.clone();
                    account.auth_probe_pending = pending;
                    account.auth_failures = failures;
                })
                .unwrap();
        }
        let output = command(home.path()).arg("status").bounded_output().unwrap();
        assert!(output.status.success());
        let status = String::from_utf8(output.stdout).unwrap();
        let columns: Vec<_> = status
            .lines()
            .find(|line| line.starts_with(&format!("{id}\t")))
            .unwrap()
            .split('\t')
            .collect();
        assert_eq!(columns[4], expected);
    }
    let store = qb_core::accounts::AccountStore::acquire(home.path()).unwrap();
    let account_path = home.path().join("accounts").join(format!("{id}.json"));
    let before = std::fs::read(&account_path).unwrap();
    let output = command(home.path()).arg("status").bounded_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&account_path).unwrap(), before);
    let output = command(home.path())
        .args(["accounts", "disable", &id])
        .bounded_output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        store.list().unwrap()[0].user_state,
        qb_core::accounts::UserState::Active
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
    std::fs::write(home.path().join("admin.token"), "synthetic-admin-secret").unwrap();
    let state = serde_json::json!({"accounts":[{
        "id":"cx-synthetic", "provider":"codex", "label":"합성 🧪\t계정\n\u{1b}",
        "user_state":"active", "health":{"kind":"future_health", "reason":{"access_token":"synthetic-reason-secret"}},
        "windows":[], "access_token":"synthetic-access-secret", "secret_dpapi_b64":"synthetic-dpapi-secret"
    }]});
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
    assert_eq!(status.lines().count(), 5);
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
    std::fs::write(home.path().join("admin.token"), "synthetic-admin-token").unwrap();
    let state = serde_json::json!({"accounts":[
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
         "windows":[{"name":"7d", "used_pct":25.0, "reset_at":"invalid"}]}
    ]});
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
        "계정 ID\t제공자\t라벨\t사용 상태\t계정 상태\t5시간 사용률\t5시간 초기화\t7일 사용률\t7일 초기화\t상태 이유\t대기 종료\n",
        "cx-expired-five\tcodex\t합성 🧪\t활성\t인증 확인 대기\t리셋됨\t2000-01-01T09:00:00.123+09:00\t37.5%\t2099-01-07T00:00:00Z\t-\t-\n",
        "cx-expired-seven\tcodex\t합성 계정\t활성\t준비됨\t91.0%\t2099-01-01T05:00:00Z\t리셋됨\t2000-01-07T00:00:00Z\t-\t-\n",
        "cx-missing-reset\tcodex\t-\t활성\t준비됨\t12.0%\t-\t-\t-\t-\t-\n",
        "cx-invalid-reset\tcodex\t-\t활성\t대기 중\t-\t-\t25.0%\tinvalid\t인증 오류\t2099-01-01T00:00:00Z\n",
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
        assert_eq!(std::str::from_utf8(bytes).unwrap(), expected);
        assert_eq!(bytes.as_slice(), expected.as_bytes());
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
    std::fs::write(home.path().join("admin.token"), "synthetic-admin-secret").unwrap();
    let state = serde_json::json!({"accounts":[
        {"id":"a","provider":"claude","user_state":"active","health":{"kind":"ready"}},
        {"id":"b","provider":"claude","user_state":"active","health":{"kind":"cooling","reason":"payment","until":"2099-01-01T00:00:00Z"}},
        {"id":"c","provider":"codex","user_state":"disabled","health":{"kind":"ready"}}
    ]});
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

#[cfg(debug_assertions)]
fn r2a_fixture_account(home: &Path, owned: bool, upstream: &str) -> String {
    let cpa = home.join("synthetic-r2a-cpa");
    std::fs::create_dir_all(&cpa).unwrap();
    let mut value = cpa_fixture(1);
    value["expired"] = serde_json::json!("2099-01-01T00:00:00Z");
    value["last_refresh"] = serde_json::json!("2020-01-01T00:00:00Z");
    let file = cpa.join("fixture.json");
    std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
    let mut config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    config.push_str(&format!("[upstream]\ncodex_base={upstream:?}\nanthropic_base={upstream:?}\ncodex_usage_base={upstream:?}\nclaude_usage_base={upstream:?}\ncodex_token_endpoint={upstream:?}\nclaude_token_endpoint={upstream:?}\n"));
    std::fs::write(home.join("config.toml"), config).unwrap();
    let store = qb_core::accounts::AccountStore::acquire(home).unwrap();
    qb_core::accounts::import::run(&store, &cpa, &[], false).unwrap();
    let mut account = store.list().unwrap().remove(0);
    if owned {
        let tokens = store.tokens(&account).unwrap();
        account.source = qb_core::accounts::Source::Oauth;
        account = store
            .save(&account, &tokens, Some(account.generation))
            .unwrap();
    }
    std::fs::remove_file(file).unwrap();
    account.id
}

#[cfg(debug_assertions)]
async fn r2a_core(home: &Path) -> Core {
    r2a_core_in(home, &std::env::current_dir().unwrap()).await
}

#[cfg(debug_assertions)]
async fn r2a_core_in(home: &Path, cwd: &Path) -> Core {
    let core = Core::spawn(
        command(home)
            .current_dir(cwd)
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null()),
    )
    .unwrap();
    let config = qb_core::config::Config::load(home).unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(core.try_wait().unwrap().is_none(), "synthetic core exited");
        let token = std::fs::read_to_string(home.join("admin.token")).unwrap_or_default();
        if let Ok(response) = client
            .get(format!(
                "http://127.0.0.1:{}/admin/v1/health",
                config.server.admin_port
            ))
            .bearer_auth(token.trim())
            .send()
            .await
            && response.status().is_success()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "synthetic core startup timeout"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    core
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r2a_cli_adopts_offline_or_via_running_lease_owner_without_tokens() {
    let _serial = SERVE_SERIAL.lock().await;
    for online in [false, true] {
        let home = fixture();
        let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
        let id = r2a_fixture_account(home.path(), false, &fake.url);
        let before = qb_core::accounts::AccountStore::list_metadata(home.path())
            .unwrap()
            .remove(0);
        let core = if online {
            Some(r2a_core(home.path()).await)
        } else {
            None
        };
        let result = command(home.path())
            .args(["accounts", "adopt", &id, "--from", "store"])
            .bounded_output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.contains("직접 갱신으로 전환"));
        assert!(text.contains("저장된 토큰"));
        for secret in ["fake-access", "fake-refresh", "fake-id"] {
            assert!(!text.contains(secret));
        }
        let adopted = qb_core::accounts::AccountStore::list_metadata(home.path())
            .unwrap()
            .remove(0);
        assert_eq!(adopted.id, before.id);
        assert_eq!(adopted.identity, before.identity);
        assert_eq!(adopted.generation, before.generation + 1);
        assert_eq!(adopted.token_meta, before.token_meta);
        assert_eq!(adopted.user_state, qb_core::accounts::UserState::Disabled);
        assert!(matches!(
            adopted.source,
            qb_core::accounts::Source::Cpa {
                mode: qb_core::accounts::ImportMode::Adopted,
                ..
            }
        ));
        assert!(fake.state.requests().is_empty());
        drop(core);
        let store = qb_core::accounts::AccountStore::acquire(home.path()).unwrap();
        assert!(store.tokens(&adopted).unwrap().access_token == "fake-access-1");
    }
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r2a_cli_adopts_relative_backup_from_client_cwd_online_and_offline() {
    let _serial = SERVE_SERIAL.lock().await;
    for online in [false, true] {
        let home = fixture();
        let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
        let id = r2a_fixture_account(home.path(), false, &fake.url);
        let core_cwd = home.path().join("core-cwd");
        let client_cwd = home.path().join("client-cwd");
        std::fs::create_dir_all(&core_cwd).unwrap();
        std::fs::create_dir_all(client_cwd.join("backups")).unwrap();
        assert!(!home.path().join("synthetic-r2a-cpa/fixture.json").exists());
        let mut backup = cpa_fixture(2);
        backup["expired"] = serde_json::json!("2099-01-01T00:00:00Z");
        backup["last_refresh"] = serde_json::json!("2021-01-01T00:00:00Z");
        std::fs::write(
            client_cwd.join("backups/latest.json"),
            serde_json::to_vec(&backup).unwrap(),
        )
        .unwrap();
        let core = if online {
            Some(r2a_core_in(home.path(), &core_cwd).await)
        } else {
            None
        };
        let result = command(home.path())
            .current_dir(&client_cwd)
            .args(["accounts", "adopt", &id, "--from", "backups/latest.json"])
            .bounded_output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("백업"));
        for bytes in [&result.stdout, &result.stderr] {
            let text = String::from_utf8_lossy(bytes);
            for secret in ["fake-access-", "fake-refresh-", "fake-id-"] {
                assert!(!text.contains(secret));
            }
        }
        assert!(fake.state.requests().is_empty());
        drop(core);
        let store = qb_core::accounts::AccountStore::acquire(home.path()).unwrap();
        let account = store.load(&id).unwrap();
        let tokens = store.tokens(&account).unwrap();
        assert!(tokens.access_token == "fake-access-2");
        assert!(tokens.refresh_token == "fake-refresh-2");
    }
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r2a_cli_refreshes_disabled_owned_account_online_and_offline() {
    let _serial = SERVE_SERIAL.lock().await;
    for online in [false, true] {
        let home = fixture();
        let fake = qb_testkit::ScriptedUpstream::start().await.unwrap();
        let id = r2a_fixture_account(home.path(), true, &fake.url);
        fake.state.push(qb_testkit::ScriptedResponse::json(reqwest::StatusCode::OK,
            r#"{"access_token":"synthetic-cli-rotated-access","refresh_token":"synthetic-cli-rotated-refresh","expires_in":3600}"#));
        let core = if online {
            Some(r2a_core(home.path()).await)
        } else {
            None
        };
        let result = command(home.path())
            .args(["accounts", "refresh", &id])
            .bounded_output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.contains("갱신 완료"));
        for secret in ["synthetic-cli-rotated", "fake-access", "fake-refresh"] {
            assert!(!text.contains(secret));
        }
        assert_eq!(fake.state.requests().len(), 1);
        let account = qb_core::accounts::AccountStore::list_metadata(home.path())
            .unwrap()
            .remove(0);
        assert_eq!(account.user_state, qb_core::accounts::UserState::Disabled);
        assert_eq!(account.source, qb_core::accounts::Source::Oauth);
        assert!(!account.token_meta.refresh_uncertain);
        assert!(
            std::time::SystemTime::from(account.token_meta.expires_at.unwrap())
                > std::time::SystemTime::now()
        );
        drop(core);
        let store = qb_core::accounts::AccountStore::acquire(home.path()).unwrap();
        assert!(store.tokens(&account).unwrap().access_token == "synthetic-cli-rotated-access");
    }
}
