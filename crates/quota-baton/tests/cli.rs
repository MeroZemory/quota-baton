use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Core(Child);
static SERVE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_quota-baton"));
    cmd.env("QB_HOME", home);
    cmd
}

fn fixture() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/tests");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

#[test]
fn help_is_available() {
    let home = fixture();
    let output = command(home.path()).arg("--help").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("serve"));
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
        let output = command(home.path()).args(args).output().unwrap();
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
        .output()
        .unwrap();
    assert!(output.status.success());
    let account = qb_core::accounts::AccountStore::list_metadata(&home)
        .unwrap()
        .remove(0);
    let stderr_path = home.join("serve.stderr");
    let _core = Core(
        command(&home)
            .args(["serve", "--capture-bodies-dir"])
            .arg(&captures)
            .args(["--capture-bodies-limit", "1"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()))
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
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
    let output = tokio::task::spawn_blocking(move || replay.output())
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
    let mut core = Core(
        command(home.path())
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap(),
    );
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
        if let Some(status) = core.0.try_wait().unwrap() {
            panic!("serve exited: {status}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "core did not start");
    assert!(core.0.try_wait().unwrap().is_none());
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
        .output()
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
        .output()
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
    let _core = Core(
        command(home.path())
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::new();
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
    let output = command(home.path()).arg("status").output().unwrap();
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
        .output()
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
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("M8"));
    assert!(!home.join("accounts").exists());
    assert!(run_import(&home, &cpa, &[]).contains("비활성"));
    let core = Core(
        command(&home)
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
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
    let duplicate = command(&home).arg("serve").output().unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("계정 저장소"));
    let token = std::fs::read_to_string(home.join("admin.token")).unwrap();
    let adopted = client.post(format!("{base}/accounts/import")).bearer_auth(token.trim()).json(&serde_json::json!({"source":"cpa","mode":"adopted","dir":root.path().join("does-not-exist")})).send().await.unwrap();
    assert_eq!(adopted.status(), 400);
    assert!(adopted.text().await.unwrap().contains("M8"));
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
    let output = command(&home).args(["accounts", "list"]).output().unwrap();
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
        let output = tokio::task::spawn_blocking(move || cmd.output())
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
        let output = tokio::task::spawn_blocking(move || cmd.output())
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
            .output()
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
    let output = command(home.path()).arg("status").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "중지됨",
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
    let output = command(home.path()).arg("status").output().unwrap();
    assert!(output.status.success());
    let status = String::from_utf8(output.stdout).unwrap();
    let columns: Vec<_> = status.lines().nth(2).unwrap().split('\t').collect();
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
        let output = command(home.path()).arg("status").output().unwrap();
        assert!(output.status.success());
        let status = String::from_utf8(output.stdout).unwrap();
        let columns: Vec<_> = status.lines().nth(2).unwrap().split('\t').collect();
        assert_eq!(columns[4], expected);
    }
    let store = qb_core::accounts::AccountStore::acquire(home.path()).unwrap();
    let account_path = home.path().join("accounts").join(format!("{id}.json"));
    let before = std::fs::read(&account_path).unwrap();
    let output = command(home.path()).arg("status").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&account_path).unwrap(), before);
    let output = command(home.path())
        .args(["accounts", "disable", &id])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        store.list().unwrap()[0].user_state,
        qb_core::accounts::UserState::Active
    );
}

#[test]
fn responding_unhealthy_admin_never_falls_back_to_disk() {
    use std::io::{Read, Write};
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
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = [0; 4096];
                let amount = stream.read(&mut request).unwrap();
                assert!(amount > 0, "CLI closed before sending a request");
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            });
            let output = command(home.path()).args(args).output().unwrap();
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
    let output = command(home.path()).arg("status").output().unwrap();
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
    use std::io::{Read, Write};
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
        for body in [
            serde_json::json!({"ok":true,"version":"synthetic","pid":0,"started_at":"2030-01-01T00:00:00Z"}).to_string(),
            state.to_string(),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = [0;4096];
            let amount = stream.read(&mut request).unwrap();
            assert!(amount > 0, "CLI closed before sending a request");
            assert!(String::from_utf8_lossy(&request[..amount]).contains("Bearer synthetic-admin-secret"));
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let output = command(home.path()).arg("status").output().unwrap();
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
    assert_eq!(status.lines().count(), 3);
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
    use std::io::{Read, Write};
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
        // Both process launches query health, then state; no real core or upstream.
        for _ in 0..2 {
            for body in [
                serde_json::json!({"ok":true,"version":"synthetic","pid":0,"started_at":"2030-01-01T00:00:00Z"}).to_string(),
                state.to_string(),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).unwrap() > 0);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        }
    });
    let expected = concat!(
        "코어: 실행 중\n",
        "계정 ID\t제공자\t라벨\t사용 상태\t계정 상태\t5시간 사용률\t5시간 초기화\t7일 사용률\t7일 초기화\t상태 이유\t대기 종료\n",
        "cx-expired-five\tcodex\t합성 🧪\t활성\t인증 확인 대기\t리셋됨\t2000-01-01T09:00:00.123+09:00\t37.5%\t2099-01-07T00:00:00Z\t-\t-\n",
        "cx-expired-seven\tcodex\t합성 계정\t활성\t준비됨\t91.0%\t2099-01-01T05:00:00Z\t리셋됨\t2000-01-07T00:00:00Z\t-\t-\n",
        "cx-missing-reset\tcodex\t-\t활성\t준비됨\t12.0%\t-\t-\t-\t-\t-\n",
        "cx-invalid-reset\tcodex\t-\t활성\t대기 중\t-\t-\t25.0%\tinvalid\t인증 오류\t2099-01-01T00:00:00Z\n",
    );
    let piped = command(home.path()).arg("status").output().unwrap();
    let output_path = home.path().join("status.stdout");
    let redirected = command(home.path())
        .arg("status")
        .stdout(Stdio::from(std::fs::File::create(&output_path).unwrap()))
        .output()
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
