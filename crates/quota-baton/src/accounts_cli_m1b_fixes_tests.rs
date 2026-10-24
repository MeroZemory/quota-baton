use super::*;

#[test]
fn device_fallback_errors_are_exactly_allowlisted_and_never_echo_provider_text() {
    use qb_core::oauth::LoginError;
    for error in [LoginError::DeviceUnavailable, LoginError::DeviceRateLimited] {
        assert!(login_start_failure(Some(&error.to_string())).offer_paste);
    }
    let message = login_failure(Some(&LoginError::DeviceUnavailable.to_string()));
    assert!(message.contains("ChatGPT Security"));
    assert!(message.contains("workspace admin"));
    let unknown = "synthetic-secret-provider-payload";
    let failure = login_start_failure(Some(unknown));
    assert!(!failure.offer_paste);
    assert!(!failure.message.contains(unknown));
    assert!(
        !login_start_failure(Some(&format!(
            "{} {unknown}",
            LoginError::DeviceUnavailable
        )))
        .offer_paste
    );
}

#[test]
fn empty_line_switch_is_separate_from_paste_and_eof() {
    assert!(read_login_line(&b"\r\n"[..], true).unwrap().is_empty());
    assert!(read_manual_login_input(&b"\r\n"[..]).is_err());
    assert!(read_login_line(&b""[..], true).is_err());
    assert!(read_login_line(&vec![b'x'; 8193][..], true).is_err());
    assert!(read_login_line(&b"\xff\n"[..], true).is_err());
    assert_eq!(
        &**read_login_line(&b"synthetic-callback\n"[..], true).unwrap(),
        "synthetic-callback"
    );
}

#[test]
fn device_prompt_rejects_untrusted_urls_codes_and_expiry_before_display() {
    use qb_proto::admin_login::DevicePrompt;
    let valid = LoginStart {
        login_id: "synthetic-login".into(),
        auth_url: "https://auth.openai.com/codex/device".into(),
        device: Some(DevicePrompt {
            verification_url: "https://auth.openai.com/codex/device".into(),
            user_code: "ABCD-EFGH".into(),
            expires_in: 900,
        }),
    };
    assert_eq!(
        display_login_start(&valid, qb_core::accounts::Provider::Codex, true).unwrap(),
        900
    );
    for url in [
        "http://auth.openai.com/codex/device",
        "https://example.invalid/codex/device",
        "https://auth.openai.com/codex/device?access_token=synthetic",
        "https://user@auth.openai.com/codex/device",
        "https://auth.openai.com:444/codex/device",
    ] {
        let mut start = valid.clone();
        start.auth_url = url.into();
        start.device.as_mut().unwrap().verification_url = url.into();
        assert!(display_login_start(&start, qb_core::accounts::Provider::Codex, true).is_err());
    }
    for code in ["", "synthetic\nsecret", "\u{1b}[31m", "synthetic secret"] {
        let mut start = valid.clone();
        start.device.as_mut().unwrap().user_code = code.into();
        assert!(display_login_start(&start, qb_core::accounts::Provider::Codex, true).is_err());
    }
    for expiry in [0, 901, u64::MAX] {
        let mut start = valid.clone();
        start.device.as_mut().unwrap().expires_in = expiry;
        assert!(display_login_start(&start, qb_core::accounts::Provider::Codex, true).is_err());
    }
}

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
