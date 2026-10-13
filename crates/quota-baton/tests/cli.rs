use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Core(Child);
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

#[tokio::test]
async fn serve_and_status_use_isolated_home() {
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
        404
    );
}
