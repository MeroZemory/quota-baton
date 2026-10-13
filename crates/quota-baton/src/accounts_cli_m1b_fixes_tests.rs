use super::*;

#[test]
fn login_failure_explains_identity_state_store_and_bind_errors_without_echoing_input() {
    use qb_core::oauth::LoginError;
    assert_eq!(
        login_failure(Some(&LoginError::IdentityConflict.to_string())),
        qb_core::accounts::login::IDENTITY_CONFLICT_REASON
    );
    assert!(
        login_failure(Some(&LoginError::StateMismatch.to_string()))
            .contains("로그인 상태가 일치하지 않습니다")
    );
    assert!(
        login_failure(Some(&LoginError::StoreFailed.to_string())).contains("저장 공간과 접근 권한")
    );
    assert!(
        login_failure(Some(
            &(LoginError::CallbackBindFailed { port: 1455 }).to_string()
        ))
        .contains("루프백 네트워크")
    );
    assert!(
        login_failure(Some(
            &(LoginError::CallbackPortBusy { port: 1455 }).to_string()
        ))
        .contains("사용 중")
    );
    assert!(!login_failure(Some("synthetic-secret-error-provider-payload")).contains("synthetic"));
}

#[tokio::test]
async fn offline_cleanup_flushes_completed_and_failed_login_ledger_before_returning() {
    use std::sync::Arc;
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/m1b-cli-fixes-tests");
    std::fs::create_dir_all(&base).unwrap();
    let home = tempfile::tempdir_in(base).unwrap();
    let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
    let obs = Arc::new(qb_core::observability::Observability::new(home.path(), None).unwrap());
    let service = qb_core::oauth::LoginService::new(store, reqwest::Client::new(), obs.clone());
    for outcome in ["completed", "failed"] {
        obs.record_login(
            qb_core::accounts::Provider::Codex,
            "synthetic-login",
            outcome,
            None,
        );
    }
    finish_offline_login(service, obs).await.unwrap();
    // Read immediately: no sleeps or polling can hide a detached flush.
    let mut lines = String::new();
    for entry in std::fs::read_dir(home.path().join("ledger")).unwrap() {
        lines.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    assert!(lines.contains("\"outcome\":\"completed\""));
    assert!(lines.contains("\"outcome\":\"failed\""));
}
