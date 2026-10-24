use super::*;
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Mock {
    requests: Arc<Mutex<Vec<Value>>>,
    inspect: Value,
    failure: Option<(StatusCode, Value)>,
    health_status: StatusCode,
    version: String,
}

fn metadata() -> Value {
    serde_json::to_value(qb_proto::admin_takeover::TakeoverInspect {
        email: Some("synthetic@example.invalid".into()),
        plan: Some("free".into()),
        provider_account_id: Some("synthetic-id".into()),
        source_path: "synthetic/missing-home".into(),
        source_last_refresh: None,
        source_mtime: Some("2026-01-01T00:00:00Z".into()),
        owner_alive: false,
        existing_id: None,
        inspect_digest: "synthetic-digest".into(),
    })
    .unwrap()
}

async fn health(State(mock): State<Mock>, headers: HeaderMap) -> (StatusCode, axum::Json<Value>) {
    assert_eq!(headers["authorization"], "Bearer synthetic-admin");
    (
        mock.health_status,
        axum::Json(
            json!({"ok":true, "version":mock.version, "pid":1, "started_at":"2026-01-01T00:00:00Z"}),
        ),
    )
}

async fn handle(
    State(mock): State<Mock>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> (StatusCode, axum::Json<Value>) {
    assert_eq!(headers["authorization"], "Bearer synthetic-admin");
    mock.requests.lock().unwrap().push(body.clone());
    if let Some((status, value)) = mock.failure {
        return (status, axum::Json(value));
    }
    (
        StatusCode::OK,
        axum::Json(if body["phase"] == "inspect" {
            mock.inspect
        } else {
            serde_json::to_value(qb_proto::admin_takeover::TakeoverCommit {
                id: "cx-synthetic".into(),
                renamed_to: Some("synthetic/renamed".into()),
                refresh: qb_proto::admin_takeover::TakeoverRefresh {
                    ok: true,
                    token_expires_at: Some("2026-01-01T01:00:00Z".into()),
                },
            })
            .unwrap()
        }),
    )
}

struct Fixture {
    home: tempfile::TempDir,
    config: Config,
    mock: Mock,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(
    inspect: Value,
    failure: Option<(StatusCode, Value)>,
    health_status: StatusCode,
) -> Fixture {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r6-cli-tests");
    std::fs::create_dir_all(&base).unwrap();
    let home = tempfile::tempdir_in(base).unwrap();
    std::fs::write(home.path().join("admin.token"), "synthetic-admin").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::default();
    config.server.admin_port = listener.local_addr().unwrap().port();
    let mock = Mock {
        requests: Default::default(),
        inspect,
        failure,
        health_status,
        version: "99.1.0-synthetic".into(),
    };
    let router = Router::new()
        .route("/admin/v1/health", get(health))
        .route("/admin/v1/accounts/takeover", post(handle))
        .with_state(mock.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Fixture {
        home,
        config,
        mock,
        task,
    }
}

async fn invoke(f: &Fixture, yes: bool) -> anyhow::Result<()> {
    // This path deliberately does not exist. Only the synthetic core may interpret it.
    tokio::time::timeout(
        Duration::from_secs(3),
        takeover(
            f.home.path(),
            &f.config,
            LoginProvider::Codex,
            PathBuf::from("synthetic/missing-home"),
            true,
            true,
            yes,
        ),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn inspect_only_is_exit_two_marker_and_never_commits_or_reads_source() {
    let f = fixture(metadata(), None, StatusCode::OK).await;
    let error = invoke(&f, false).await.unwrap_err();
    assert!(error.downcast_ref::<TakeoverInspectOnly>().is_some());
    let requests = f.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["phase"], "inspect");
    assert!(requests[0].get("inspect_digest").is_none());
    assert!(Path::new(requests[0]["path"].as_str().unwrap()).is_absolute());
    assert!(!f.home.path().join("accounts").exists());
}

#[tokio::test]
async fn yes_always_inspects_then_commits_exact_digest_and_flags() {
    let f = fixture(metadata(), None, StatusCode::OK).await;
    invoke(&f, true).await.unwrap();
    let requests = f.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["phase"], "inspect");
    assert_eq!(requests[1]["phase"], "commit");
    assert_eq!(requests[1]["inspect_digest"], "synthetic-digest");
    for request in requests.iter() {
        assert_eq!(request["provider"], "codex");
        assert_eq!(request["replace"], true);
        assert_eq!(request["keep_source"], true);
        assert_eq!(request["path"], requests[0]["path"]);
    }
}

#[tokio::test]
async fn missing_digest_prevents_commit_even_with_yes() {
    let mut value = metadata();
    value.as_object_mut().unwrap().remove("inspect_digest");
    let f = fixture(value, None, StatusCode::OK).await;
    assert!(invoke(&f, true).await.is_err());
    assert_eq!(f.mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn claude_default_flags_are_forwarded_without_local_source_access() {
    let f = fixture(metadata(), None, StatusCode::OK).await;
    tokio::time::timeout(
        Duration::from_secs(3),
        takeover(
            f.home.path(),
            &f.config,
            LoginProvider::Claude,
            PathBuf::from("synthetic/missing-claude-home"),
            false,
            false,
            true,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let requests = f.mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request["provider"], "claude");
        assert_eq!(request["replace"], false);
        assert_eq!(request["keep_source"], false);
    }
}

#[tokio::test]
async fn stopped_core_does_not_fall_back_to_a_local_account_store() {
    let f = fixture(metadata(), None, StatusCode::OK).await;
    // Reserve then release a port so the probe targets a stopped synthetic core.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = f.config.clone();
    config.server.admin_port = listener.local_addr().unwrap().port();
    drop(listener);
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        takeover(
            f.home.path(),
            &config,
            LoginProvider::Codex,
            PathBuf::from("synthetic/missing-home"),
            false,
            false,
            true,
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(crate::friendly_error(&error).contains("코어를 먼저 실행"));
    assert!(!f.home.path().join("accounts").exists());
}

#[tokio::test]
async fn authentication_failure_never_posts_takeover() {
    let f = fixture(metadata(), None, StatusCode::UNAUTHORIZED).await;
    assert!(invoke(&f, true).await.is_err());
    assert!(f.mock.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn inspect_refusals_do_not_commit_or_echo_arbitrary_payload() {
    for status in [StatusCode::CONFLICT, StatusCode::UNPROCESSABLE_ENTITY] {
        let f = fixture(
            metadata(),
            Some((
                status,
                json!({"error":{"code":"unknown", "message":"synthetic-secret"}}),
            )),
            StatusCode::OK,
        )
        .await;
        let error = invoke(&f, true).await.unwrap_err();
        assert!(!crate::friendly_error(&error).contains("synthetic-secret"));
        assert_eq!(f.mock.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn missing_takeover_route_reports_safe_local_and_remote_versions() {
    for status in [StatusCode::NOT_FOUND, StatusCode::METHOD_NOT_ALLOWED] {
        let f = fixture(
            metadata(),
            Some((
                status,
                json!({"error":{"code":"not_found","message":"synthetic-secret"}}),
            )),
            StatusCode::OK,
        )
        .await;
        let error = invoke(&f, true).await.unwrap_err();
        let message = crate::friendly_error(&error);
        assert!(message.contains("버전"));
        assert!(message.contains(env!("CARGO_PKG_VERSION")));
        assert!(message.contains("99.1.0-synthetic"));
        assert!(!message.contains("synthetic-secret"));
    }
}

#[test]
fn warnings_and_results_only_render_allowlisted_scalar_fields() {
    // Keep hostile extra fields at the presentation boundary, independently of
    // the strict live transport fixtures above.
    let mut hostile = metadata();
    hostile["access_token"] = json!("synthetic-secret");
    hostile["unexpected"] = json!({"refresh_token":"synthetic-secret"});
    let normal = takeover_inspect_lines(&hostile, false).join("\n");
    assert!(normal.contains("원래 앱"));
    assert!(normal.contains("synthetic@example.invalid"));
    assert!(normal.contains("원본 수정 시각"));
    assert!(normal.contains("원래 앱 실행 중: 감지되지 않음 (최선의 확인)"));
    assert!(!normal.contains("synthetic-secret"));
    assert!(!normal.contains("synthetic-digest"));
    let stronger = takeover_inspect_lines(&hostile, true).join("\n");
    assert!(stronger.contains("전체 토큰 계열이 폐기"));
    let result = takeover_result_line(
        &json!({"id":"cx-synthetic", "refresh":{"ok":false}, "access_token":"synthetic-secret"}),
    );
    assert!(result.contains("확인 필요"));
    assert!(!result.contains("다시 로그인 필요"));
    assert!(!result.contains("synthetic-secret"));
    assert!(
        !cell(&json!("line\n\t\u{1b}escape"))
            .chars()
            .any(char::is_control)
    );
}

#[test]
fn account_absence_stays_distinct_from_missing_routes() {
    assert!(!missing_admin_route(
        StatusCode::NOT_FOUND,
        "/admin/v1/accounts/cx-synthetic/refresh-usage",
        &json!({"error":{"message":"Account not found"}})
    ));
    let missing = json!({"error":{"code":"not_found"}});
    assert!(!missing_admin_route(
        StatusCode::NOT_FOUND,
        "/admin/v1/accounts/cx-synthetic/refresh",
        &missing
    ));
    assert!(!missing_admin_route(
        StatusCode::NOT_FOUND,
        "/admin/v1/accounts/cx-synthetic",
        &missing
    ));
    assert!(missing_admin_route(
        StatusCode::NOT_FOUND,
        "/admin/v1/accounts/takeover",
        &missing
    ));
    assert!(missing_admin_route(
        StatusCode::NOT_FOUND,
        "/admin/v1/state",
        &missing
    ));
    assert!(missing_admin_route(
        StatusCode::NOT_FOUND,
        "/admin/v1/accounts/cx-synthetic/refresh",
        &json!({})
    ));
    assert!(missing_admin_route(
        StatusCode::METHOD_NOT_ALLOWED,
        "/admin/v1/accounts/cx-synthetic/refresh",
        &missing
    ));
    assert!(!missing_admin_route(
        StatusCode::UNAUTHORIZED,
        "/admin/v1/state",
        &json!({})
    ));
    assert_eq!(safe_version(&json!("1.2.3+build-1")), "1.2.3+build-1");
    assert_eq!(safe_version(&json!("secret\n\u{1b}payload")), "알 수 없음");
    assert_eq!(
        safe_version(&json!({"access_token":"secret"})),
        "알 수 없음"
    );
}

#[test]
fn status_discovers_free_monthly_and_mixed_plan_windows() {
    assert_eq!(status_window_label("5h"), "5시간");
    assert_eq!(status_window_label("7d"), "7일");
    assert_eq!(status_window_label("43200m"), "30일");
    let windows = status_window_names(&[
        json!({"windows":[{"name":"43200m", "used_pct":42.0}]}),
        json!({"windows":[{"name":"5h"},{"name":"7d"},{"name":"43200m"}]}),
        json!({"windows":null}),
    ]);
    assert_eq!(windows, ["43200m", "5h", "7d"]);
    assert!(status_window_names(&[]).is_empty());
    assert_eq!(
        percentage(
            &json!({"name":"43200m","used_pct":42.0}),
            std::time::SystemTime::now()
        ),
        "58%"
    );
}

#[test]
fn r8_status_remaining_values_preserve_used_snapshot_and_reset_marker() {
    let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000);
    for (used, expected) in [
        (0.0, "100%"),
        (2.0, "98%"),
        (37.5, "62.5%"),
        (100.0, "0%"),
        (105.0, "0%"),
        (-5.0, "100%"),
    ] {
        let snapshot = json!({"name":"5h", "used_pct":used});
        assert_eq!(percentage(&snapshot, now), expected);
        assert_eq!(snapshot["used_pct"], used, "JSON remains a used percentage");
    }
    assert_eq!(percentage(&json!({}), now), "-");
    assert_eq!(
        percentage(
            &json!({"used_pct":100.0, "reset_at":"2000-01-01T00:00:00Z"}),
            now
        ),
        "리셋됨"
    );
}

#[test]
fn parser_accepts_both_providers_and_optional_flags() {
    use clap::Parser;
    for provider in ["codex", "claude"] {
        let cli = crate::Cli::try_parse_from([
            "quota-baton",
            "accounts",
            "takeover",
            provider,
            "synthetic-home",
            "--replace",
            "--keep-source",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            crate::Command::Accounts {
                command: AccountsCommand::Platform(AccountsPlatformCommand::Takeover {
                    replace: true,
                    keep_source: true,
                    yes: true,
                    ..
                })
            }
        ));
    }
}
