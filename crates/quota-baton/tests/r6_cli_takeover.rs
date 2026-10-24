//! Spawn the actual Cargo-selected CLI against a synthetic authenticated admin.
//! No real core, upstream, credential source, or copied executable is used.
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

const ADMIN_TOKEN: &str = "r6-synthetic-admin-secret";
const DIGEST: &str = "r6-synthetic-inspect-digest";
const DEADLINE: Duration = Duration::from_secs(15);

#[derive(Debug)]
struct Observed {
    route: &'static str,
    authenticated: bool,
    body: Value,
}

#[derive(Clone)]
struct Mock {
    observed: Arc<Mutex<Vec<Observed>>>,
    refresh_ok: bool,
    invalid_reply: Option<InvalidReply>,
}

#[derive(Clone, Copy)]
enum InvalidReply {
    Health,
    Inspect,
    Commit,
    RefreshNull,
}

impl Mock {
    fn record(&self, route: &'static str, headers: &HeaderMap, body: Value) -> bool {
        let authenticated = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(format!("Bearer {ADMIN_TOKEN}").as_str());
        self.observed.lock().unwrap().push(Observed {
            route,
            authenticated,
            body,
        });
        authenticated
    }
}

async fn health(State(mock): State<Mock>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    if !mock.record("health", &headers, Value::Null) {
        return (StatusCode::UNAUTHORIZED, Json(json!({})));
    }
    let mut value = serde_json::to_value(qb_proto::HealthResponse {
        ok: true,
        version: "99.1.0-synthetic".into(),
        pid: 1,
        started_at: "2026-01-01T00:00:00Z".into(),
    })
    .unwrap();
    if matches!(mock.invalid_reply, Some(InvalidReply::Health)) {
        value["access_token"] = json!("r6-access-secret");
        value["refresh_token"] = json!("r6-refresh-secret");
    }
    (StatusCode::OK, Json(value))
}

async fn takeover(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !mock.record("takeover", &headers, body.clone()) {
        return (StatusCode::UNAUTHORIZED, Json(json!({})));
    }
    let phase = body["phase"].as_str();
    let mut value = match phase {
        Some("inspect") => serde_json::to_value(qb_proto::admin_takeover::TakeoverInspect {
            email: Some("synthetic@example.invalid".into()),
            plan: Some("free".into()),
            provider_account_id: Some("synthetic-account".into()),
            source_path: body["path"].as_str().unwrap().into(),
            source_last_refresh: None,
            source_mtime: Some("2026-01-01T00:00:00Z".into()),
            owner_alive: false,
            existing_id: None,
            inspect_digest: DIGEST.into(),
        })
        .unwrap(),
        Some("commit") if body["inspect_digest"] == DIGEST => {
            serde_json::to_value(qb_proto::admin_takeover::TakeoverCommit {
                id: "cx-synthetic".into(),
                renamed_to: None,
                refresh: qb_proto::admin_takeover::TakeoverRefresh {
                    ok: mock.refresh_ok,
                    token_expires_at: Some("2026-01-01T01:00:00Z".into()),
                },
            })
            .unwrap()
        }
        _ => return (StatusCode::BAD_REQUEST, Json(json!({}))),
    };
    if matches!(
        (phase, mock.invalid_reply),
        (Some("inspect"), Some(InvalidReply::Inspect))
            | (Some("commit"), Some(InvalidReply::Commit))
    ) {
        value["access_token"] = json!("r6-access-secret");
        value["refresh_token"] = json!("r6-refresh-secret");
        value["id_token"] = json!("eyJr6.synthetic.jwt");
        value["unexpected"] = json!({"client_secret":"r6-client-secret"});
        if phase == Some("commit") {
            value["refresh"]["access_token"] = json!("r6-access-secret");
            value["refresh"]["refresh_token"] = json!("r6-refresh-secret");
        }
    }
    if phase == Some("commit") && matches!(mock.invalid_reply, Some(InvalidReply::RefreshNull)) {
        value["refresh"]["ok"] = Value::Null;
    }
    (StatusCode::OK, Json(value))
}

struct Fixture {
    home: tempfile::TempDir,
    mock: Mock,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(refresh_ok: bool) -> Self {
        Self::with_invalid_reply(refresh_ok, None).await
    }

    async fn with_invalid_reply(refresh_ok: bool, invalid_reply: Option<InvalidReply>) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r6-cli-spawned");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "[server]\nadmin_port={}\n",
                listener.local_addr().unwrap().port()
            ),
        )
        .unwrap();
        std::fs::write(home.path().join("admin.token"), ADMIN_TOKEN).unwrap();
        let mock = Mock {
            observed: Default::default(),
            refresh_ok,
            invalid_reply,
        };
        let router = Router::new()
            .route("/admin/v1/health", get(health))
            .route("/admin/v1/accounts/takeover", post(takeover))
            .with_state(mock.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { home, mock, server }
    }

    fn source_path(&self) -> PathBuf {
        self.home
            .path()
            .canonicalize()
            .unwrap()
            .join("missing-source-home")
    }

    async fn run(&self, provider: &str, yes: bool, flags: bool) -> Output {
        // Use the default target binary selected by Cargo, without copying it.
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_quota-baton"));
        let root = self.home.path().canonicalize().unwrap();
        let profile = root.join("synthetic-profile");
        command
            .current_dir(&root)
            .env("QB_HOME", &root)
            .env("HOME", &profile)
            .env("USERPROFILE", &profile)
            .env("APPDATA", profile.join("AppData/Roaming"))
            .env("LOCALAPPDATA", profile.join("AppData/Local"))
            .env("CODEX_HOME", profile.join(".codex"))
            .env("CLAUDE_CONFIG_DIR", profile.join(".claude"))
            .args(["accounts", "takeover", provider, "missing-source-home"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if yes {
            command.arg("--yes");
        }
        if flags {
            command.args(["--replace", "--keep-source"]);
        }
        let child = command.spawn().unwrap();
        // Dropping wait_with_output on timeout drops its kill-on-drop child.
        tokio::time::timeout(DEADLINE, child.wait_with_output())
            .await
            .expect("spawned CLI exceeded its 15-second deadline")
            .unwrap()
    }

    fn assert_requests(&self, provider: &str, commit: bool, flags: bool) {
        let observed = self.mock.observed.lock().unwrap();
        let routes: Vec<_> = observed.iter().map(|request| request.route).collect();
        assert_eq!(
            routes,
            if commit {
                vec!["health", "takeover", "takeover"]
            } else {
                vec!["health", "takeover"]
            }
        );
        assert!(
            observed.iter().all(|request| request.authenticated),
            "every request, including health, must authenticate"
        );
        let posts: Vec<_> = observed
            .iter()
            .filter(|request| request.route == "takeover")
            .collect();
        assert_eq!(
            posts
                .iter()
                .filter(|request| request.body["phase"] == "inspect")
                .count(),
            1
        );
        assert_eq!(
            posts
                .iter()
                .filter(|request| request.body["phase"] == "commit")
                .count(),
            usize::from(commit)
        );
        for request in &posts {
            let path = request.body["path"].as_str().unwrap();
            assert!(Path::new(path).is_absolute());
            assert_eq!(
                Path::new(path).parent().unwrap().canonicalize().unwrap(),
                self.home.path().canonicalize().unwrap()
            );
            assert_eq!(Path::new(path).file_name().unwrap(), "missing-source-home");
            assert_eq!(request.body["provider"], provider);
            assert_eq!(request.body["replace"], flags);
            assert_eq!(request.body["keep_source"], flags);
            assert_eq!(
                request.body.as_object().unwrap().len(),
                if request.body["phase"] == "commit" {
                    6
                } else {
                    5
                }
            );
        }
        assert_eq!(posts[0].body["phase"], "inspect");
        assert!(posts[0].body.get("inspect_digest").is_none());
        if commit {
            assert_eq!(posts[1].body["phase"], "commit");
            assert_eq!(posts[1].body["inspect_digest"], DIGEST);
        }
        assert!(
            !self.source_path().exists(),
            "CLI must not create a credential source"
        );
        assert!(
            !self.home.path().join("accounts").exists(),
            "CLI must not acquire a local account store"
        );
    }
}

fn safe_output(output: &Output) -> (String, String) {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout must be UTF-8");
    let stderr = String::from_utf8(output.stderr.clone()).expect("stderr must be UTF-8");
    for text in [&stdout, &stderr] {
        for forbidden in [
            ADMIN_TOKEN,
            DIGEST,
            "r6-access-secret",
            "r6-refresh-secret",
            "r6-client-secret",
            "eyJ",
            "access_token",
            "refresh_token",
            "id_token",
            "client_secret",
            "Bearer ",
        ] {
            assert!(
                !text.contains(forbidden),
                "CLI leaked a token pattern or synthetic secret"
            );
        }
    }
    (stdout, stderr)
}

#[tokio::test]
async fn no_yes_exits_two_with_one_authenticated_inspect_and_no_commit() {
    for provider in ["codex", "claude"] {
        let fixture = Fixture::new(true).await;
        let output = fixture.run(provider, false, false).await;
        assert_eq!(output.status.code(), Some(2));
        fixture.assert_requests(provider, false, false);
        let (stdout, stderr) = safe_output(&output);
        for field in [
            "synthetic@example.invalid",
            "synthetic-account",
            "원본 경로",
            "원본 수정 시각",
            "원래 앱 실행 중: 감지되지 않음 (최선의 확인)",
            "원래 앱",
        ] {
            assert!(stdout.contains(field), "missing inspect field {field}");
        }
        assert!(stderr.contains("--yes"));
        assert!(!stdout.contains("가져오기 완료"));
    }
}

#[tokio::test]
async fn yes_exits_zero_with_exactly_inspect_then_digest_commit_and_no_secrets() {
    for (provider, flags) in [("codex", false), ("claude", true)] {
        let fixture = Fixture::new(true).await;
        let output = fixture.run(provider, true, flags).await;
        assert_eq!(output.status.code(), Some(0));
        fixture.assert_requests(provider, true, flags);
        let (stdout, stderr) = safe_output(&output);
        assert!(stderr.is_empty());
        assert!(stdout.contains("synthetic@example.invalid"));
        assert!(stdout.contains("cx-synthetic: 가져오기 완료"));
        assert!(stdout.contains("갱신 성공"));
        assert_eq!(stdout.contains("전체 토큰 계열이 폐기"), flags);
    }
}

#[tokio::test]
async fn false_refresh_requires_checking_without_claiming_relogin() {
    let fixture = Fixture::new(false).await;
    let output = fixture.run("codex", true, false).await;
    assert_eq!(output.status.code(), Some(1));
    fixture.assert_requests("codex", true, false);
    let (stdout, stderr) = safe_output(&output);
    assert!(stdout.contains("가져오기 완료"));
    assert!(stdout.contains("갱신 확인 필요"));
    assert!(stderr.contains("갱신을 확인하지 못했습니다"));
    assert!(!stdout.contains("다시 로그인 필요"));
    assert!(!stderr.contains("다시 로그인 필요"));
    assert!(!stdout.contains("갱신 성공"));
}

#[tokio::test]
async fn malformed_success_replies_reject_without_echo_or_claiming_completion() {
    for invalid in [
        InvalidReply::Health,
        InvalidReply::Inspect,
        InvalidReply::Commit,
        InvalidReply::RefreshNull,
    ] {
        let fixture = Fixture::with_invalid_reply(true, Some(invalid)).await;
        let output = fixture.run("codex", true, false).await;
        assert_eq!(output.status.code(), Some(1));
        if matches!(invalid, InvalidReply::Health) {
            let observed = fixture.mock.observed.lock().unwrap();
            assert_eq!(
                observed.iter().map(|r| r.route).collect::<Vec<_>>(),
                ["health"]
            );
            assert!(observed.iter().all(|r| r.authenticated));
            assert!(!fixture.source_path().exists());
            assert!(!fixture.home.path().join("accounts").exists());
        } else {
            fixture.assert_requests("codex", !matches!(invalid, InvalidReply::Inspect), false);
        }
        let (stdout, stderr) = safe_output(&output);
        assert!(!stdout.contains("가져오기 완료"));
        assert!(!stdout.contains("갱신 성공"));
        assert!(!stdout.contains("다시 로그인 필요"));
        assert!(!stderr.contains("다시 로그인 필요"));
        assert!(!stderr.is_empty());
    }
}
