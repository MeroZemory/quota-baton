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

#[test]
fn startup_refuses_invalid_state_before_listening() {
    let _serial = SERVE_SERIAL.blocking_lock();
    let home = fixture();
    std::fs::create_dir_all(home.path().join("state")).unwrap();
    std::fs::write(
        home.path().join("state/quota.json"),
        "invalid synthetic state",
    )
    .unwrap();
    let output = command(home.path()).arg("serve").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("quota"));
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
    let health: qb_proto::HealthResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert!(health.ok);
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
