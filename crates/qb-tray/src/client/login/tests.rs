//! Synthetic source tests only: no native chooser, shell, profile, or provider I/O.
use super::super::AdminClient;
use super::*;
use qb_proto::admin_login::{DevicePrompt, LoginState};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

const DEVICE_URL: &str = "https://auth.openai.com/codex/device";
const CODEX_WEB: &str = "https://auth.openai.com/oauth/authorize?state=synthetic-state";
const CLAUDE_WEB: &str = "https://claude.ai/oauth/authorize?state=synthetic-state";
const TOKEN: &str = "synthetic-r261-admin";
const IO_LIMIT: Duration = Duration::from_secs(3);

fn web(provider: &str) -> LoginStart {
    LoginStart {
        login_id: "synthetic_login-1".into(),
        auth_url: if provider == "claude" {
            CLAUDE_WEB
        } else {
            CODEX_WEB
        }
        .into(),
        device: None,
    }
}

fn device() -> LoginStart {
    LoginStart {
        login_id: "synthetic_device-1".into(),
        auth_url: DEVICE_URL.into(),
        device: Some(DevicePrompt {
            verification_url: DEVICE_URL.into(),
            user_code: "SYNTHETIC-x7Q9".into(),
            expires_in: 137,
        }),
    }
}

fn assert_unresolved(error: &ClientError, expected: Option<&str>) {
    assert!(matches!(error, ClientError::LoginStartUnresolved { .. }));
    assert_eq!(known_session(error), expected);
    assert_eq!(start_error(error), LoginFailure::Unknown);
}

#[test]
fn r261_decode_duplicate_or_missing_ids_never_recovers_an_arbitrary_session() {
    for bytes in [
        br#"{"login_id":"safe-first","login_id":"safe-second","auth_url":"unsafe","device":{}}"#
            .as_slice(),
        br#"{"login_id":"same-id","login_id":"same-id","auth_url":"unsafe"}"#.as_slice(),
        br#"{"auth_url":"unsafe","device":{}}"#.as_slice(),
        br#"{"login_id":"../unsafe","auth_url":"unsafe","device":{}}"#.as_slice(),
        br#"{"login_id":17,"auth_url":"unsafe","device":{}}"#.as_slice(),
    ] {
        let error = decode_start("codex", LoginMethod::Device, bytes).unwrap_err();
        assert_unresolved(&error, None);
    }
}

#[test]
fn r261_decode_malformed_device_preserves_only_safe_id_without_raw_payload() {
    let bytes = br#"{"login_id":"safe-known","auth_url":"https://evil.invalid/SYNTHETIC-URL-SECRET","device":{"user_code":"SYNTHETIC-CODE-SECRET","expires_in":"bad"},"access_token":"SYNTHETIC-TOKEN-SECRET"}"#;
    let error = decode_start("codex", LoginMethod::Device, bytes).unwrap_err();
    assert_unresolved(&error, Some("safe-known"));
    for rendered in [format!("{error}"), format!("{error:?}")] {
        for secret in [
            "SYNTHETIC-URL-SECRET",
            "SYNTHETIC-CODE-SECRET",
            "SYNTHETIC-TOKEN-SECRET",
            "evil.invalid",
            "auth_url",
            "access_token",
        ] {
            assert!(!rendered.contains(secret));
        }
    }
}

#[test]
fn r261_known_session_is_not_invented_for_other_client_errors() {
    assert_eq!(
        known_session(&ClientError::Http(reqwest::StatusCode::TOO_MANY_REQUESTS)),
        None
    );
    assert_eq!(
        known_session(&ClientError::AccountAction("synthetic")),
        None
    );
    assert_unresolved(&ClientError::LoginStartUnresolved { login_id: None }, None);
}

#[test]
fn r261_web_requests_contain_provider_only_for_both_providers() {
    for provider in ["codex", "claude"] {
        assert_eq!(
            request_body(provider, LoginMethod::Web),
            Some(json!({"provider":provider}))
        );
    }
}

#[test]
fn r261_device_request_has_no_browser_without_replacement_authority() {
    let body = request_body("codex", LoginMethod::Device).unwrap();
    assert_eq!(body, json!({"provider":"codex","no_browser":true}));
    assert!(body.get("replace_linked").is_none());
    assert!(request_body("claude", LoginMethod::Device).is_none());
}

#[test]
fn r261_unknown_or_misspelled_providers_never_form_a_request() {
    for provider in ["", "Codex", "codex ", "openai", "../codex", "claude\n"] {
        for method in [LoginMethod::Web, LoginMethod::Device] {
            assert!(request_body(provider, method).is_none());
        }
    }
}

#[test]
fn r261_trusted_urls_accept_only_provider_method_pairs() {
    assert!(trusted_url("codex", LoginMethod::Web, CODEX_WEB));
    assert!(trusted_url("claude", LoginMethod::Web, CLAUDE_WEB));
    assert!(trusted_url("codex", LoginMethod::Device, DEVICE_URL));
    for (provider, method, url) in [
        ("claude", LoginMethod::Device, DEVICE_URL),
        ("claude", LoginMethod::Web, CODEX_WEB),
        ("codex", LoginMethod::Web, CLAUDE_WEB),
        ("codex", LoginMethod::Web, DEVICE_URL),
        ("codex", LoginMethod::Device, CODEX_WEB),
        ("other", LoginMethod::Web, CODEX_WEB),
    ] {
        assert!(!trusted_url(provider, method, url));
    }
}

#[test]
fn r261_web_urls_reject_spoofs_credentials_wrong_paths_and_fragments() {
    for url in [
        "http://auth.openai.com/oauth/authorize",
        "https://auth.openai.com.evil.invalid/oauth/authorize",
        "https://evil-auth.openai.com/oauth/authorize",
        "https://auth.openai.com@evil.invalid/oauth/authorize",
        "https://user@auth.openai.com/oauth/authorize",
        "https://user:password@auth.openai.com/oauth/authorize",
        "https://auth.openai.com:444/oauth/authorize",
        "https://auth.openai.com/oauth/authorize/",
        "https://auth.openai.com/oauth/token",
        "https://auth.openai.com/oauth/authorize#code",
        "https://auth.openai.com/oauth/authorize?state=x#",
        "https://auth.openai.com\\evil.invalid/oauth/authorize",
        "https://auth.openai.com/oauth/authorize\n",
        "javascript:alert(1)",
        "/oauth/authorize",
    ] {
        assert!(
            !trusted_url("codex", LoginMethod::Web, url),
            "accepted {url:?}"
        );
    }
    assert!(!trusted_url(
        "claude",
        LoginMethod::Web,
        "https://claude.ai.evil.invalid/oauth/authorize"
    ));
    assert!(!trusted_url(
        "codex",
        LoginMethod::Web,
        &format!("{CODEX_WEB}{}", "x".repeat(8193))
    ));
}

#[test]
fn r261_device_url_must_be_exact_without_query_fragment_or_alias() {
    for url in [
        "https://auth.openai.com/codex/device?user_code=SYNTHETIC",
        "https://auth.openai.com/codex/device?",
        "https://auth.openai.com/codex/device#",
        "https://auth.openai.com/codex/device/",
        "https://auth.openai.com:443/codex/device",
        "https://AUTH.OPENAI.COM/codex/device",
        "https://auth.openai.com/codex/./device",
        "https://auth.openai.com/codex/%64evice",
        "https://auth.openai.com.evil.invalid/codex/device",
        "https://user:password@auth.openai.com/codex/device",
        "http://auth.openai.com/codex/device",
    ] {
        assert!(
            !trusted_url("codex", LoginMethod::Device, url),
            "accepted {url:?}"
        );
    }
}

#[test]
fn r261_login_id_refuses_path_query_controls_and_overlong_values() {
    assert!(valid_login_id("synthetic_A-19"));
    assert!(valid_login_id(&"a".repeat(256)));
    for id in [
        "",
        "../session",
        "a/b",
        "a?cancel",
        "a#fragment",
        "a%2fb",
        "a\nb",
        "코드",
    ] {
        assert!(!valid_login_id(id));
    }
    assert!(!valid_login_id(&"a".repeat(257)));
}

#[test]
fn r261_device_validation_preserves_exact_code_and_actual_expiry() {
    let mut start = device();
    for seconds in [1, 137, 899, 900] {
        start.device.as_mut().unwrap().expires_in = seconds;
        let original = start.clone();
        assert_eq!(validate_start("codex", LoginMethod::Device, &start), Ok(()));
        assert_eq!(start, original);
        let prompt = start.device.as_ref().unwrap();
        assert_eq!(prompt.user_code, "SYNTHETIC-x7Q9");
        assert_eq!(prompt.expires_in, seconds);
        assert_eq!(prompt.verification_url, DEVICE_URL);
    }
}

#[test]
fn r261_missing_or_mismatched_device_data_is_refused() {
    let mut missing = device();
    missing.device = None;
    assert!(validate_start("codex", LoginMethod::Device, &missing).is_err());
    let mut mismatch = device();
    mismatch.device.as_mut().unwrap().verification_url = CODEX_WEB.into();
    assert!(validate_start("codex", LoginMethod::Device, &mismatch).is_err());
    let mut mismatch = device();
    mismatch.auth_url = CODEX_WEB.into();
    assert!(validate_start("codex", LoginMethod::Device, &mismatch).is_err());
}

#[test]
fn r261_device_code_and_expiry_boundaries_refuse_malformed_facts() {
    for code in [
        "",
        "space code",
        "line\ncode",
        "tab\tcode",
        "코드",
        "\u{7f}",
    ] {
        let mut start = device();
        start.device.as_mut().unwrap().user_code = code.into();
        assert!(validate_start("codex", LoginMethod::Device, &start).is_err());
    }
    let mut start = device();
    start.device.as_mut().unwrap().user_code = "X".repeat(129);
    assert!(validate_start("codex", LoginMethod::Device, &start).is_err());
    for seconds in [0, 901, u64::MAX] {
        let mut start = device();
        start.device.as_mut().unwrap().expires_in = seconds;
        assert!(validate_start("codex", LoginMethod::Device, &start).is_err());
    }
}

#[test]
fn r261_browser_replies_require_no_device_payload_and_valid_session() {
    for provider in ["codex", "claude"] {
        let mut start = web(provider);
        assert_eq!(validate_start(provider, LoginMethod::Web, &start), Ok(()));
        start.device = device().device;
        assert!(validate_start(provider, LoginMethod::Web, &start).is_err());
        let mut start = web(provider);
        start.login_id = "../other".into();
        assert!(validate_start(provider, LoginMethod::Web, &start).is_err());
    }
}

#[test]
fn r261_http_failure_categories_do_not_invent_completion() {
    use reqwest::StatusCode;
    for (status, expected) in [
        (StatusCode::CONFLICT, LoginFailure::Busy),
        (StatusCode::TOO_MANY_REQUESTS, LoginFailure::RateLimited),
        (StatusCode::BAD_REQUEST, LoginFailure::Unavailable),
        (StatusCode::SERVICE_UNAVAILABLE, LoginFailure::Unavailable),
        (StatusCode::INTERNAL_SERVER_ERROR, LoginFailure::Unknown),
    ] {
        assert_eq!(start_error(&ClientError::Http(status)), expected);
    }
    assert_eq!(
        start_error(&ClientError::AccountAction("synthetic")),
        LoginFailure::Unknown
    );
}

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

enum Reply {
    Json(u16, Value),
    Raw(u16, Vec<u8>),
    DelayedJson(Duration, Value),
    Disconnect,
}

/// Every connection is bounded, all request bytes are capped, and Drop aborts
/// the sole owned task on assertion unwind. Normal finish signals and joins it.
struct Fake {
    port: u16,
    requests: Arc<Mutex<Vec<Recorded>>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Fake {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = loop {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            if !matches!(listener.local_addr().unwrap().port(), 10100 | 10101) {
                break listener;
            }
        };
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut replies = VecDeque::from(replies);
            loop {
                let (mut socket, _) = tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => accepted.unwrap(),
                };
                let request = timeout(IO_LIMIT, read_request(&mut socket)).await.unwrap();
                recorded.lock().unwrap().push(request);
                match replies
                    .pop_front()
                    .unwrap_or(Reply::Json(500, json!({"error":"unexpected request"})))
                {
                    Reply::Disconnect => drop(socket),
                    reply => {
                        let (status, body) = match reply {
                            Reply::Json(status, value) => {
                                (status, serde_json::to_vec(&value).unwrap())
                            }
                            Reply::Raw(status, body) => (status, body),
                            Reply::DelayedJson(delay, value) => {
                                timeout(Duration::from_secs(10), tokio::time::sleep(delay))
                                    .await
                                    .unwrap();
                                (200, serde_json::to_vec(&value).unwrap())
                            }
                            Reply::Disconnect => unreachable!(),
                        };
                        let head = format!(
                            "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        timeout(IO_LIMIT, async {
                            socket.write_all(head.as_bytes()).await.unwrap();
                            socket.write_all(&body).await.unwrap();
                            socket.shutdown().await.unwrap();
                        })
                        .await
                        .unwrap();
                    }
                }
            }
        });
        Self {
            port,
            requests,
            stop: Some(stop),
            task: Some(task),
        }
    }

    fn client(&self) -> AdminClient {
        AdminClient::new(self.port, TOKEN).unwrap()
    }

    async fn finish(mut self) -> Vec<Recorded> {
        // Keep listening briefly after the awaited operation to detect an
        // unexpected detached retry/cancel. Inline retries are already awaited.
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.stop.take().unwrap().send(()).ok();
        let task = self.task.as_mut().unwrap();
        timeout(Duration::from_secs(4), task)
            .await
            .unwrap()
            .unwrap();
        self.task.take();
        let requests = self.requests.lock().unwrap();
        requests.clone()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> Recorded {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            break index + 4;
        }
        assert!(
            bytes.len() < 16 * 1024,
            "synthetic request headers exceeded cap"
        );
        let mut chunk = [0; 1024];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "request ended before headers");
        bytes.extend_from_slice(&chunk[..count]);
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = headers.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let path = first.next().unwrap().to_owned();
    let mut length = 0;
    let mut authorization = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse::<usize>().unwrap();
        }
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.trim().to_owned());
        }
        assert!(
            !name.eq_ignore_ascii_case("transfer-encoding"),
            "fake expects bounded JSON content-length"
        );
    }
    assert!(length <= 16 * 1024);
    while bytes.len() < header_end + length {
        let mut chunk = [0; 1024];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "request ended before body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    Recorded {
        method,
        path,
        authorization,
        body: bytes[header_end..header_end + length].to_vec(),
    }
}

fn assert_start(request: &Recorded, expected: Value) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/admin/v1/accounts/login");
    assert_eq!(
        request.authorization.as_deref(),
        Some(concat!("Bearer ", "synthetic-r261-admin"))
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body, expected);
    assert!(body.get("replace_linked").is_none());
}

#[tokio::test]
async fn r261_real_client_web_sends_exactly_one_provider_only_post() {
    for provider in ["codex", "claude"] {
        let expected = web(provider);
        let fake = Fake::start(vec![Reply::Json(200, json!(expected))]).await;
        let result = timeout(
            Duration::from_secs(8),
            fake.client()
                .start_login_with_method(provider, LoginMethod::Web),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, expected);
        let requests = fake.finish().await;
        assert_eq!(requests.len(), 1);
        assert_start(&requests[0], json!({"provider":provider}));
    }
}

#[tokio::test]
async fn r261_real_client_device_preserves_payload_and_sends_one_post() {
    let expected = device();
    let fake = Fake::start(vec![Reply::Json(200, json!(expected))]).await;
    let result = timeout(
        Duration::from_secs(8),
        fake.client()
            .start_login_with_method("codex", LoginMethod::Device),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, expected);
    let requests = fake.finish().await;
    assert_eq!(requests.len(), 1);
    assert_start(&requests[0], json!({"provider":"codex","no_browser":true}));
}

#[tokio::test]
async fn r261_client_legacy_start_remains_web_without_device_flags() {
    let fake = Fake::start(vec![Reply::Json(200, json!(web("codex")))]).await;
    timeout(Duration::from_secs(8), fake.client().start_login("codex"))
        .await
        .unwrap()
        .unwrap();
    let requests = fake.finish().await;
    assert_eq!(requests.len(), 1);
    assert_start(&requests[0], json!({"provider":"codex"}));
}

#[tokio::test]
async fn r261_invalid_provider_or_method_sends_no_http() {
    let fake = Fake::start(vec![]).await;
    let client = fake.client();
    for (provider, method) in [
        ("unknown", LoginMethod::Web),
        ("Codex", LoginMethod::Device),
        ("claude", LoginMethod::Device),
    ] {
        let result = timeout(
            Duration::from_secs(8),
            client.start_login_with_method(provider, method),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(ClientError::AccountAction(_))));
    }
    assert!(fake.finish().await.is_empty());
}

#[tokio::test]
async fn r261_fact_validation_without_explicit_start_sends_zero_requests() {
    let fake = Fake::start(vec![]).await;
    let _client = fake.client();
    for method in [LoginMethod::Web, LoginMethod::Device] {
        assert!(request_body("codex", method).is_some());
    }
    assert!(validate_start("codex", LoginMethod::Device, &device()).is_ok());
    assert!(trusted_url("codex", LoginMethod::Device, DEVICE_URL));
    // Only pure client facts are exercised; this does not simulate main UI.
    assert!(fake.finish().await.is_empty());
}

#[tokio::test]
async fn r261_malformed_known_session_is_returned_without_cancel_or_another_start() {
    for malformed in [
        LoginStart {
            device: None,
            ..device()
        },
        LoginStart {
            auth_url: "https://evil.invalid/codex/device".into(),
            ..device()
        },
    ] {
        let id = malformed.login_id.clone();
        let fake = Fake::start(vec![Reply::Json(200, json!(malformed))]).await;
        let result = timeout(
            Duration::from_secs(12),
            fake.client()
                .start_login_with_method("codex", LoginMethod::Device),
        )
        .await
        .unwrap();
        let error = result.unwrap_err();
        assert_unresolved(&error, Some(&id));
        let requests = fake.finish().await;
        assert_eq!(requests.len(), 1);
        assert_start(&requests[0], json!({"provider":"codex","no_browser":true}));
    }
}

#[tokio::test]
async fn r261_invalid_session_id_is_not_interpolated_into_cancel_path() {
    let malformed = LoginStart {
        login_id: "../other?cancel".into(),
        ..device()
    };
    let fake = Fake::start(vec![Reply::Json(200, json!(malformed))]).await;
    let result = timeout(
        Duration::from_secs(8),
        fake.client()
            .start_login_with_method("codex", LoginMethod::Device),
    )
    .await
    .unwrap();
    assert_unresolved(&result.unwrap_err(), None);
    assert_eq!(fake.finish().await.len(), 1);
}

#[tokio::test]
async fn r261_browser_start_with_unexpected_device_preserves_session_without_cancel() {
    let malformed = LoginStart {
        device: device().device,
        ..web("codex")
    };
    let fake = Fake::start(vec![Reply::Json(200, json!(malformed))]).await;
    let result = timeout(
        Duration::from_secs(12),
        fake.client()
            .start_login_with_method("codex", LoginMethod::Web),
    )
    .await
    .unwrap();
    assert_unresolved(&result.unwrap_err(), Some("synthetic_login-1"));
    let requests = fake.finish().await;
    assert_eq!(requests.len(), 1);
    assert_start(&requests[0], json!({"provider":"codex"}));
}

#[tokio::test]
async fn r261_explicit_cancel_failure_stays_with_native_owner_without_client_retry() {
    let malformed = LoginStart {
        device: None,
        ..device()
    };
    let fake = Fake::start(vec![
        Reply::Json(200, json!(malformed)),
        Reply::Json(503, json!({"error":"synthetic"})),
    ])
    .await;
    let client = fake.client();
    let result = timeout(
        Duration::from_secs(12),
        client.start_login_with_method("codex", LoginMethod::Device),
    )
    .await
    .unwrap();
    let error = result.unwrap_err();
    assert_unresolved(&error, Some("synthetic_device-1"));
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
    // The native owner, not start_login_with_method, explicitly chooses cleanup.
    let result = timeout(
        Duration::from_secs(8),
        client.cancel_login(known_session(&error).unwrap()),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(ClientError::Http(reqwest::StatusCode::SERVICE_UNAVAILABLE))
    ));
    let requests = fake.finish().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].path,
        "/admin/v1/accounts/login/synthetic_device-1/cancel"
    );
}

#[tokio::test]
async fn r261_start_http_429_and_400_never_retry_or_fallback_to_web() {
    for status in [429, 400] {
        let fake = Fake::start(vec![Reply::Json(
            status,
            json!({"error":"synthetic start refusal"}),
        )])
        .await;
        let result = timeout(
            Duration::from_secs(8),
            fake.client()
                .start_login_with_method("codex", LoginMethod::Device),
        )
        .await
        .unwrap();
        assert!(matches!(result,Err(ClientError::Http(code)) if code.as_u16()==status));
        let requests = fake.finish().await;
        assert_eq!(requests.len(), 1);
        assert_start(&requests[0], json!({"provider":"codex","no_browser":true}));
    }
}

#[tokio::test]
async fn r261_disconnect_after_complete_start_request_never_replays() {
    let fake = Fake::start(vec![Reply::Disconnect]).await;
    let result = timeout(
        Duration::from_secs(8),
        fake.client()
            .start_login_with_method("codex", LoginMethod::Device),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(ClientError::Transport(_))));
    let requests = fake.finish().await;
    assert_eq!(requests.len(), 1);
    assert_start(&requests[0], json!({"provider":"codex","no_browser":true}));
}

#[tokio::test]
async fn r261_malformed_nested_device_retains_safe_id_without_cancel_or_restart() {
    for prompt in [
        json!({}),
        json!({"verification_url":DEVICE_URL,"user_code":"SYNTHETIC","expires_in":"137"}),
    ] {
        let fake = Fake::start(vec![Reply::Json(
            200,
            json!({"login_id":"synthetic-wire","auth_url":DEVICE_URL,"device":prompt}),
        )])
        .await;
        let result = timeout(
            Duration::from_secs(8),
            fake.client()
                .start_login_with_method("codex", LoginMethod::Device),
        )
        .await
        .unwrap();
        assert_unresolved(&result.unwrap_err(), Some("synthetic-wire"));
        assert_eq!(fake.finish().await.len(), 1);
    }
}

#[tokio::test]
async fn r261_duplicate_login_id_wire_never_selects_a_session_or_sends_cancel() {
    let raw = br#"{"login_id":"first-safe","login_id":"second-safe","auth_url":"https://auth.openai.com/codex/device","device":{}}"#.to_vec();
    let fake = Fake::start(vec![Reply::Raw(200, raw)]).await;
    let result = timeout(
        Duration::from_secs(8),
        fake.client()
            .start_login_with_method("codex", LoginMethod::Device),
    )
    .await
    .unwrap();
    assert_unresolved(&result.unwrap_err(), None);
    let requests = fake.finish().await;
    assert_eq!(requests.len(), 1);
    assert_start(&requests[0], json!({"provider":"codex","no_browser":true}));
}

#[tokio::test]
async fn r261_start_body_cap_accepts_exact_boundary_but_does_not_recover_oversize_id() {
    assert_eq!(MAX_START_BODY, 16 * 1024);
    let expected = device();
    let mut body = serde_json::to_vec(&expected).unwrap();
    body.resize(MAX_START_BODY, b' ');
    let fake = Fake::start(vec![Reply::Raw(200, body.clone())]).await;
    let result = timeout(
        Duration::from_secs(8),
        fake.client()
            .start_login_with_method("codex", LoginMethod::Device),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, expected);
    assert_eq!(fake.finish().await.len(), 1);

    // Even with a valid ID early in the bytes, an oversized reply is not a
    // trusted complete start response. Do not salvage the prefix or auto-cancel.
    body.push(b' ');
    let fake = Fake::start(vec![Reply::Raw(200, body)]).await;
    let result = timeout(
        Duration::from_secs(8),
        fake.client()
            .start_login_with_method("codex", LoginMethod::Device),
    )
    .await
    .unwrap();
    assert_unresolved(&result.unwrap_err(), None);
    assert_eq!(fake.finish().await.len(), 1);
}

#[tokio::test]
async fn r261_six_second_provider_delay_returns_original_session_without_retry() {
    let expected = device();
    let fake = Fake::start(vec![Reply::DelayedJson(
        Duration::from_secs(6),
        json!(expected),
    )])
    .await;
    // Genuinely wait past the incumbent 5s client default. The start-specific
    // 20s budget must return the first session; this is not a paused-clock test.
    let result = timeout(
        Duration::from_secs(12),
        fake.client()
            .start_login_with_method("codex", LoginMethod::Device),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, expected);
    let requests = fake.finish().await;
    assert_eq!(requests.len(), 1);
    assert_start(&requests[0], json!({"provider":"codex","no_browser":true}));
}

#[tokio::test]
async fn r261_existing_status_and_cancel_routes_preserve_typed_terminal_facts() {
    let fake = Fake::start(vec![
        Reply::Json(200, json!({"status":"pending"})),
        Reply::Json(
            200,
            json!({"status":"completed","account_id":"synthetic-account"}),
        ),
        Reply::Json(
            200,
            json!({"status":"completed","account_id":"synthetic-account"}),
        ),
    ])
    .await;
    let client = fake.client();
    let pending = timeout(
        Duration::from_secs(8),
        client.login_status("synthetic-status"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(pending.status, LoginState::Pending);
    assert!(pending.account_id.is_none());
    let completed = timeout(
        Duration::from_secs(8),
        client.login_status("synthetic-status"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(completed.status, LoginState::Completed);
    assert_eq!(completed.account_id.as_deref(), Some("synthetic-account"));
    // Existing cancel API acknowledges HTTP only; do not invent a cancelled
    // state from a successful cancel response containing Completed.
    timeout(
        Duration::from_secs(8),
        client.cancel_login("synthetic-status"),
    )
    .await
    .unwrap()
    .unwrap();
    let requests = fake.finish().await;
    assert_eq!(requests.len(), 3);
    for request in &requests[..2] {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/admin/v1/accounts/login/synthetic-status");
    }
    assert_eq!(requests[2].method, "POST");
    assert_eq!(
        requests[2].path,
        "/admin/v1/accounts/login/synthetic-status/cancel"
    );
}

#[test]
fn r261_unresolved_and_invalid_request_copy_is_localized_without_session_material() {
    for language in [crate::i18n::UiLanguage::Ko, crate::i18n::UiLanguage::En] {
        crate::i18n::with_language(language, || {
            let unknown = ClientError::LoginStartUnresolved {
                login_id: Some("synthetic-private-session".into()),
            };
            let invalid = ClientError::AccountAction("로그인 요청이 올바르지 않습니다");
            let (unknown_text, invalid_text) = if language == crate::i18n::UiLanguage::En {
                (
                    "Could not confirm the sign-in response",
                    "Invalid sign-in request",
                )
            } else {
                (
                    "로그인 응답을 확인할 수 없습니다",
                    "로그인 요청이 올바르지 않습니다",
                )
            };
            assert_eq!(super::super::short_error(&unknown), unknown_text);
            assert_eq!(super::super::short_error(&invalid), invalid_text);
            assert!(!super::super::short_error(&unknown).contains("synthetic-private-session"));
        });
    }
}
