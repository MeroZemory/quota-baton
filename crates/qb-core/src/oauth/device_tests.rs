use super::manual_tests::{Fixture, terminal};
use super::*;
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_testkit::ScriptedResponse;
use serde_json::json;
use sha2::{Digest, Sha256};

fn params(fixture: &Fixture) -> Parameters {
    let mut params = fixture.params(Provider::Codex);
    params.device_code = format!("{}/usercode", fixture.fake.url);
    params.device_token = format!("{}/device-token", fixture.fake.url);
    params.device_interval = Some(Duration::from_millis(10));
    params.device_lifetime = Duration::from_secs(60);
    params
}

fn code(fixture: &Fixture) {
    fixture.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({
            "device_auth_id":"synthetic-device-id", "user_code":"SYNTHETIC", "interval":"1"
        })
        .to_string(),
    ));
}

fn authorization(fixture: &Fixture) {
    let verifier = "v".repeat(43);
    fixture.fake.state.push(ScriptedResponse::json(
        StatusCode::OK,
        json!({
            "authorization_code":"synthetic-device-code",
            "code_verifier":verifier,
            "code_challenge":URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        })
        .to_string(),
    ));
}

#[tokio::test]
async fn device_pending_then_shared_pkce_exchange_saves_once_without_listener() {
    let fixture = Fixture::new().await;
    code(&fixture);
    for (status, body) in [
        (StatusCode::FORBIDDEN, "{}"),
        (StatusCode::NOT_FOUND, "{}"),
        (
            StatusCode::BAD_REQUEST,
            r#"{"error":"authorization_pending"}"#,
        ),
    ] {
        fixture
            .fake
            .state
            .push(ScriptedResponse::json(status, body));
    }
    authorization(&fixture);
    fixture.script_success(Provider::Codex);
    let start = fixture
        .service
        .start_device_parameters(params(&fixture), false)
        .await
        .unwrap();
    assert_eq!(start.auth_url, "https://auth.openai.com/codex/device");
    assert_eq!(start.device.as_ref().unwrap().user_code, "SYNTHETIC");
    assert!(!format!("{start:?}").contains("SYNTHETIC"));
    assert!(matches!(
        fixture
            .service
            .submit_manual(&start.login_id, "ignored")
            .await,
        Err(LoginError::NotManual)
    ));
    assert_eq!(
        terminal(&fixture.service, &start.login_id).await.status,
        LoginState::Completed
    );
    assert_eq!(fixture.service.inner.store.list().unwrap().len(), 1);
    let requests = fixture.fake.state.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path_and_query == "/usercode")
            .count(),
        1
    );
    assert_eq!(requests.len(), 6);
    let client: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        client["client_id"],
        Parameters::new(Provider::Codex).client_id
    );
    assert_eq!(requests[0].headers["user-agent"], exchange::user_agent());
    let body = std::str::from_utf8(&requests[5].body).unwrap();
    let fields: HashMap<String, String> = reqwest::Url::parse(&format!("http://localhost/?{body}"))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(fields["code"], "synthetic-device-code");
    assert_eq!(fields["code_verifier"], "v".repeat(43));
    assert_eq!(
        fields["redirect_uri"],
        "https://auth.openai.com/deviceauth/callback"
    );
}

#[tokio::test]
async fn initial_unavailable_and_rate_limit_do_not_request_new_codes() {
    for status in [StatusCode::NOT_FOUND, StatusCode::TOO_MANY_REQUESTS] {
        let fixture = Fixture::new().await;
        fixture
            .fake
            .state
            .push(ScriptedResponse::json(status, "{}"));
        let result = fixture
            .service
            .start_device_parameters(params(&fixture), false)
            .await;
        assert!(match status {
            StatusCode::NOT_FOUND => matches!(result, Err(LoginError::DeviceUnavailable)),
            _ => matches!(result, Err(LoginError::DeviceRateLimited)),
        });
        assert_eq!(fixture.fake.state.requests().len(), 1);
        assert!(fixture.service.inner.sessions.lock().unwrap().is_empty());
    }
    let message = LoginError::DeviceUnavailable.to_string();
    assert!(
        message.contains("Security")
            && message.contains("workspace admin")
            && message.contains("paste-redirect")
    );
}

#[tokio::test]
async fn slow_down_and_poll_rate_limit_wait_without_new_code_requests() {
    for (status, body) in [
        (StatusCode::BAD_REQUEST, r#"{"error":"slow_down"}"#),
        (StatusCode::TOO_MANY_REQUESTS, r#"{"error":"rate_limited"}"#),
    ] {
        let fixture = Fixture::new().await;
        code(&fixture);
        fixture
            .fake
            .state
            .push(ScriptedResponse::json(status, body));
        authorization(&fixture);
        fixture.script_success(Provider::Codex);
        let mut params = params(&fixture);
        params.device_lifetime = Duration::from_secs(1);
        let start = fixture
            .service
            .start_device_parameters(params, false)
            .await
            .unwrap();
        // A broken immediate retry consumes the queued success and saves;
        // correct >=5s backoff must expire first without another request.
        assert_eq!(
            terminal(&fixture.service, &start.login_id).await.status,
            LoginState::Expired
        );
        assert_eq!(fixture.fake.state.requests().len(), 2);
        assert!(fixture.service.inner.store.list().unwrap().is_empty());
    }
}

#[tokio::test]
async fn ambiguous_pending_expires_without_a_second_usercode() {
    let fixture = Fixture::new().await;
    code(&fixture);
    for _ in 0..128 {
        fixture
            .fake
            .state
            .push(ScriptedResponse::json(StatusCode::FORBIDDEN, "{}"));
    }
    let mut params = params(&fixture);
    params.device_interval = Some(Duration::from_millis(10));
    params.device_lifetime = Duration::from_secs(1);
    let start = fixture
        .service
        .start_device_parameters(params, false)
        .await
        .unwrap();
    assert_eq!(
        terminal(&fixture.service, &start.login_id).await.status,
        LoginState::Expired
    );
    let requests = fixture.fake.state.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path_and_query == "/usercode")
            .count(),
        1
    );
    assert!(fixture.service.inner.store.list().unwrap().is_empty());
}

#[tokio::test]
async fn cancelled_device_is_terminal_before_explicit_paste_fallback() {
    let fixture = Fixture::new().await;
    code(&fixture);
    let start = fixture
        .service
        .start_device_parameters(params(&fixture), false)
        .await
        .unwrap();
    fixture.service.cancel(&start.login_id).await.unwrap();
    assert_eq!(
        fixture
            .service
            .status(&start.login_id)
            .await
            .unwrap()
            .status,
        LoginState::Cancelled
    );
    assert_eq!(fixture.fake.state.requests().len(), 1);
    assert!(fixture.service.inner.store.list().unwrap().is_empty());
    let paste = fixture.service.start_paste_redirect(false).await.unwrap();
    assert!(paste.device.is_none());
    assert_ne!(paste.login_id, start.login_id);
    assert!(paste.auth_url.contains("code_challenge="));
    fixture.service.cancel(&paste.login_id).await.unwrap();
    assert_eq!(fixture.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn device_cancellation_during_exchange_does_not_save() {
    let fixture = Fixture::new().await;
    code(&fixture);
    authorization(&fixture);
    fixture.script_success(Provider::Codex);
    let mut params = params(&fixture);
    params.device_interval = Some(Duration::from_millis(100));
    let authorization = device::request(&fixture.service.inner.client, &params)
        .await
        .unwrap();
    let (arrived, release) = fixture.fake.state.pause_next_response();
    let generation = fixture
        .service
        .inner
        .cancel_generation
        .load(Ordering::Acquire);
    let start = fixture
        .service
        .start_prepared(params, false, true, Some((authorization, generation)))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), arrived)
        .await
        .unwrap()
        .unwrap();
    let (exchange_arrived, exchange_release) = fixture.fake.state.pause_next_response();
    let _ = release.send(());
    tokio::time::timeout(Duration::from_secs(5), exchange_arrived)
        .await
        .unwrap()
        .unwrap();
    fixture.service.cancel(&start.login_id).await.unwrap();
    let _ = exchange_release.send(());
    assert!(fixture.service.inner.store.list().unwrap().is_empty());
    assert_eq!(
        fixture
            .service
            .status(&start.login_id)
            .await
            .unwrap()
            .status,
        LoginState::Cancelled
    );
}

#[tokio::test]
async fn malformed_device_pkce_and_oversized_responses_fail_without_exchange() {
    for body in [
        json!({"authorization_code":"synthetic", "code_verifier":"short", "code_challenge":"wrong"}).to_string(),
        "x".repeat(16 * 1024 + 1),
    ] {
        let fixture = Fixture::new().await;
        code(&fixture);
        fixture.fake.state.push(ScriptedResponse::json(StatusCode::OK, body));
        let start = fixture.service.start_device_parameters(params(&fixture), false).await.unwrap();
        assert_eq!(terminal(&fixture.service, &start.login_id).await.status, LoginState::Failed);
        assert_eq!(fixture.fake.state.requests().len(), 2);
        assert!(fixture.service.inner.store.list().unwrap().is_empty());
    }
}

#[tokio::test]
async fn dropping_initial_request_leaves_no_live_session_or_poller() {
    let fixture = Fixture::new().await;
    code(&fixture);
    let (arrived, release) = fixture.fake.state.pause_next_response();
    let params = params(&fixture);
    let mut request = Box::pin(fixture.service.start_device_parameters(params, false));
    tokio::select! {
        _ = arrived => {},
        _ = &mut request => panic!("initial response should be held"),
    }
    drop(request);
    let _ = release.send(());
    assert!(fixture.service.inner.sessions.lock().unwrap().is_empty());
    assert_eq!(fixture.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn cancel_all_fences_an_initial_device_request_before_session_publication() {
    let fixture = Fixture::new().await;
    code(&fixture);
    let (arrived, release) = fixture.fake.state.pause_next_response();
    let params = params(&fixture);
    let mut request = Box::pin(fixture.service.start_device_parameters(params, false));
    tokio::select! {
        _ = arrived => {},
        _ = &mut request => panic!("initial response should be held"),
    }
    fixture.service.cancel_all().await;
    let _ = release.send(());
    assert!(matches!(request.await, Err(LoginError::Cancelled)));
    assert!(fixture.service.inner.sessions.lock().unwrap().is_empty());
    assert_eq!(fixture.fake.state.requests().len(), 1);
}
