//! Synthetic loopback only; no production management or image endpoints.
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use bytes::Bytes;
use qb_core::{
    accounts::{AccountStore, Health, HealthReason, Provider, UserState, import},
    config::Config,
    debug_pin::DebugPins,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    observability::Observability,
    relay::{self, RelayState},
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::{Value, json};
use std::{fs, sync::Arc, time::Duration};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const GENERATE: &str = "/v1/images/generations";
const EDIT: &str = "/v1/images/edits";
const INLINE: &str = r#"{"model":"gpt-image-2","prompt":"synthetic","images":[{"image_url":"data:image/png;base64,Zm9v"}]}"#;

struct Harness {
    _serial: tokio::sync::MutexGuard<'static, ()>,
    _root: tempfile::TempDir,
    store: Arc<AccountStore>,
    state: Arc<RelayState>,
    obs: Arc<Observability>,
    fake: ScriptedUpstream,
    servers: RunningServers,
}

impl Harness {
    async fn new(plans: &[Option<&str>], dead: bool) -> Self {
        Self::with_retain(plans, dead, 64).await
    }

    async fn with_retain(plans: &[Option<&str>], dead: bool, retain_mib: u64) -> Self {
        Self::with_options(plans, dead, retain_mib, &[]).await
    }

    async fn with_options(
        plans: &[Option<&str>],
        dead: bool,
        retain_mib: u64,
        last_resort: &[(usize, bool)],
    ) -> Self {
        let serial = SERIAL.lock().await;
        let base =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r12-images");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let home = root.path().join("home");
        let imports = root.path().join("imports");
        fs::create_dir_all(&imports).unwrap();
        let mut activate = Vec::new();
        for i in 0..plans.len() {
            let email = format!("synthetic-{i}@example.invalid");
            fs::write(imports.join(format!("{i}.json")), serde_json::to_vec(&json!({
                "type":"codex", "email":email, "account_id":format!("synthetic-{i}"),
                "access_token":format!("synthetic-access-{i}"), "refresh_token":format!("synthetic-refresh-{i}"),
                "id_token":"synthetic-id", "expired":"2099-01-01T00:00:00Z",
                "last_refresh":"2026-01-01T00:00:00Z"
            })).unwrap()).unwrap();
            activate.push(format!("codex:{email}"));
        }
        let store = Arc::new(AccountStore::acquire(&home).unwrap());
        import::run(&store, &imports, &activate, false).unwrap();
        for (a, plan) in store.list().unwrap().into_iter().zip(plans) {
            store
                .update_metadata(&a.id, |a| a.plan = plan.map(str::to_owned))
                .unwrap();
        }
        let fake = ScriptedUpstream::start().await.unwrap();
        let mut config = Config::default();
        config.guard.retain_max_per_request_mib = retain_mib;
        config.server.data_port = 0;
        config.server.admin_port = 0;
        config.upstream.codex_base = fake.url.clone();
        for (index, account) in store.list().unwrap().iter().enumerate() {
            if let Some((_, enabled)) = last_resort.iter().find(|(i, _)| *i == index) {
                config
                    .accounts
                    .entry(account.id.clone())
                    .or_default()
                    .images_last_resort = Some(*enabled);
            }
        }
        if dead {
            let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            config.upstream.codex_base = format!("http://{}", socket.local_addr().unwrap());
            drop(socket);
        }
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let state = Arc::new(
            RelayState::new(
                store.clone(),
                &config,
                obs.clone(),
                Arc::new(DebugPins::default()),
            )
            .unwrap()
            .with_token_endpoint(Provider::Codex, fake.url.clone()),
        );
        let servers = http::spawn_with_routes(
            &config,
            LocalSecrets {
                client_key: "synthetic-client".into(),
                admin_token: "synthetic-admin".into(),
            },
            relay::router(state.clone()),
            axum::Router::new(),
        )
        .await
        .unwrap();
        Self {
            _serial: serial,
            _root: root,
            store,
            state,
            obs,
            fake,
            servers,
        }
    }

    fn request(&self, path: &str, body: impl Into<reqwest::Body>) -> reqwest::RequestBuilder {
        self.request_with_content_type(path, body, "application/json")
    }

    fn request_with_content_type(
        &self,
        path: &str,
        body: impl Into<reqwest::Body>,
        content_type: &str,
    ) -> reqwest::RequestBuilder {
        let mut headers = HeaderMap::new();
        headers.append(
            "X-OpenAI-Actor-Authorization",
            HeaderValue::from_static("qb-local"),
        );
        headers.append(
            "x-openai-actor-authorization",
            HeaderValue::from_static("second"),
        );
        reqwest::Client::new()
            .post(format!("http://{}{path}", self.servers.data_addr))
            .headers(headers)
            .bearer_auth("synthetic-client")
            .header("content-type", content_type)
            .body(body)
    }

    async fn code(response: reqwest::Response, status: StatusCode, code: &str) {
        assert_eq!(response.status(), status);
        let v: Value = response.json().await.unwrap();
        assert_eq!(v["error"]["code"], code);
        assert!(v["error"]["message"].as_str().unwrap().contains("이미지"));
        assert!(v["error"]["message"].as_str().unwrap().contains(
            if code.starts_with("image_edit_") {
                "편집"
            } else {
                "생성"
            }
        ));
    }

    fn ids(&self) -> Vec<String> {
        self.store
            .list()
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect()
    }

    fn bind(&self, account: &str, key: &str) {
        let candidate = qb_core::bindings::Candidate {
            account: self.store.load(account).unwrap(),
            quota: None,
            inflight: 0,
        };
        let qb_core::bindings::Decision::Selected { lease, .. } = self.state.bindings.choose(
            Provider::Codex,
            Some(key),
            None,
            None,
            &[candidate],
            None,
            0,
        ) else {
            panic!("synthetic binding placement");
        };
        lease.dispatched();
        drop(lease);
    }

    fn binding_snapshot(&self, key: &str) -> Value {
        // Match the client's existing dashboard classification before comparing
        // the full binding record. Do not discard any fields after the request.
        self.obs.dashboard().evidence(key, &HeaderMap::new(), None);
        let snapshot = self.state.bindings.export().unwrap();
        let binding = snapshot["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["key"] == key)
            .unwrap();
        assert_eq!(binding["client_kind"], "unknown");
        assert_eq!(snapshot, self.state.bindings.export().unwrap());
        snapshot
    }

    fn expire_token(&self, id: &str) -> qb_core::accounts::Account {
        let mut account = self.store.load(id).unwrap();
        let tokens = self.store.tokens(&account).unwrap();
        let generation = account.generation;
        // Token metadata cannot be changed with update_metadata. Simulate a
        // newer stored credential observation through the credential CAS API.
        account.token_meta.last_refresh_at = Some(
            account
                .token_meta
                .last_refresh_at
                .expect("synthetic fixture refresh timestamp")
                + chrono::Duration::seconds(1),
        );
        account.token_meta.expires_at = Some(chrono::Utc::now() - chrono::Duration::hours(1));
        let saved = self
            .store
            .save(&account, &tokens, Some(generation))
            .unwrap();
        assert!(saved.generation > generation);
        assert!(saved.token_meta.expires_at.unwrap() < chrono::Utc::now());
        saved
    }

    fn ledger(&self) -> Vec<Value> {
        self.obs.flush().unwrap();
        fs::read_dir(self.store.home().join("ledger"))
            .unwrap()
            .flat_map(|entry| {
                fs::read_to_string(entry.unwrap().path())
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    async fn wait_requests(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.fake.state.requests().len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("synthetic upstream did not observe expected dispatches");
        assert_eq!(self.fake.state.requests().len(), count);
    }

    fn safety_hold(&self, id: &str, reason: Option<HealthReason>, payment: bool) {
        let until = chrono::Utc::now() + chrono::Duration::hours(6);
        self.store
            .update_metadata(id, |a| {
                a.health = reason.map_or(Health::Ready, |reason| Health::Cooling {
                    reason,
                    until,
                    scope: "account".into(),
                });
                a.payment_until = payment.then_some(until);
            })
            .unwrap();
    }

    fn replace_token(&self, id: &str) {
        let mut account = self.store.load(id).unwrap();
        let mut tokens = self.store.tokens(&account).unwrap();
        let generation = account.generation;
        tokens.access_token = "synthetic-current-generation-access".into();
        account.token_meta.last_refresh_at = Some(
            account
                .token_meta
                .last_refresh_at
                .expect("synthetic fixture refresh timestamp")
                + chrono::Duration::seconds(1),
        );
        let updated = self
            .store
            .save(&account, &tokens, Some(generation))
            .unwrap();
        assert!(updated.generation > generation);
        assert_eq!(
            self.store.tokens(&updated).unwrap().access_token,
            "synthetic-current-generation-access"
        );
    }
}

#[tokio::test]
async fn eligible_known_paid_only_and_real_credentials() {
    let h = Harness::new(&[Some("free"), None, Some("unknown"), Some("plus")], false).await;
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{\"data\":[]}"));
    let response = h
        .request(GENERATE, "{ \"prompt\": \"synthetic\" }")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path_and_query, "/images/generations");
    let paid = h
        .store
        .list()
        .unwrap()
        .into_iter()
        .find(|a| a.plan.as_deref() == Some("plus"))
        .unwrap();
    let tokens = h.store.tokens(&paid).unwrap();
    assert_eq!(
        requests[0].headers["authorization"],
        format!("Bearer {}", tokens.access_token)
    );
    assert!(
        !requests[0]
            .headers
            .contains_key("x-openai-actor-authorization")
    );
    assert!(!requests[0].headers.contains_key("x-api-key"));
    assert_eq!(requests[0].body, "{ \"prompt\": \"synthetic\" }");
    assert_eq!(h.state.bindings.count(), 0);
}

#[tokio::test]
async fn free_unknown_paused_drain_relogin_and_expired_are_excluded() {
    let h = Harness::new(
        &[
            Some("free"),
            None,
            Some("plus"),
            Some("pro"),
            Some("team"),
            Some("business"),
        ],
        false,
    )
    .await;
    let ids = h.ids();
    h.store
        .update_metadata(&ids[2], |a| a.user_state = UserState::Paused)
        .unwrap();
    h.store
        .update_metadata(&ids[3], |a| a.drain = true)
        .unwrap();
    h.store
        .update_metadata(&ids[4], |a| {
            a.health = Health::NeedsRelogin {
                reason: "synthetic".into(),
                since: chrono::Utc::now(),
            }
        })
        .unwrap();
    h.expire_token(&ids[5]);
    Harness::code(
        h.request(GENERATE, "{}").send().await.unwrap(),
        StatusCode::BAD_REQUEST,
        "image_generation_no_eligible_account",
    )
    .await;
    assert!(h.fake.state.requests().is_empty());
}

#[tokio::test]
async fn bound_preference_quota_retry_once_and_no_responses_binding_change() {
    let h = Harness::new(&[Some("plus"), Some("pro"), Some("team")], false).await;
    let ids = h.ids();
    let candidate = qb_core::bindings::Candidate {
        account: h.store.load(&ids[2]).unwrap(),
        quota: None,
        inflight: 0,
    };
    let qb_core::bindings::Decision::Selected { lease, .. } = h.state.bindings.choose(
        Provider::Codex,
        Some("codex:image-session"),
        None,
        None,
        &[candidate],
        None,
        0,
    ) else {
        panic!("synthetic binding placement");
    };
    lease.dispatched();
    lease.successful();
    drop(lease);
    assert_eq!(
        h.state.bindings.current(Some("codex:image-session")),
        Some(ids[2].clone())
    );
    let binding_before = h.binding_snapshot("codex:image-session");
    for _ in 0..3 {
        let mut response = ScriptedResponse::json(
            StatusCode::TOO_MANY_REQUESTS,
            "{\"error\":{\"code\":\"image_quota_exceeded\"}}",
        );
        response
            .headers
            .insert("retry-after", "3600".parse().unwrap());
        h.fake.state.push(response);
    }
    let response = h
        .request(GENERATE, "{\"prompt\":\"synthetic\"}")
        .header("session-id", "image-session")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.headers()["x-qb-image-error"],
        "image_generation_quota_exhausted"
    );
    response.bytes().await.unwrap();
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 3);
    let account = h.store.load(&ids[2]).unwrap();
    assert_eq!(
        requests[0].headers["authorization"],
        format!("Bearer {}", h.store.tokens(&account).unwrap().access_token)
    );
    let mut seen = std::collections::BTreeSet::new();
    for request in &requests {
        assert!(seen.insert(request.headers["authorization"].as_bytes().to_vec()));
        assert_eq!(request.body, requests[0].body);
        assert!(!request.headers.contains_key("x-openai-actor-authorization"));
    }
    assert_eq!(
        h.state.bindings.current(Some("codex:image-session")),
        Some(ids[2].clone())
    );
    Harness::code(
        h.request(GENERATE, "{}").send().await.unwrap(),
        StatusCode::TOO_MANY_REQUESTS,
        "image_generation_quota_exhausted",
    )
    .await;
    assert_eq!(h.fake.state.requests().len(), 3);
    assert_eq!(h.state.bindings.export().unwrap(), binding_before);
}

#[tokio::test]
async fn image_401_403_never_refresh_rotate_or_change_token_metadata() {
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        let h = Harness::new(&[Some("pro"), Some("plus")], false).await;
        let before = h.store.list().unwrap();
        let token_bytes: Vec<_> = before
            .iter()
            .map(|a| serde_json::to_vec(&h.store.tokens(a).unwrap()).unwrap())
            .collect();
        let body = "{\"error\":{\"code\":\"invalid_token\",\"message\":\"synthetic rejection\"}}";
        h.fake.state.push(ScriptedResponse::json(status, body));
        let response = h.request(EDIT, INLINE).send().await.unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(
            response.headers()["x-qb-image-error"],
            "image_edit_auth_rejected"
        );
        Harness::code(response, status, "image_edit_auth_rejected").await;
        assert_eq!(h.fake.state.requests().len(), 1);
        assert_eq!(h.store.list().unwrap(), before);
        for (a, bytes) in before.iter().zip(token_bytes) {
            assert_eq!(
                serde_json::to_vec(&h.store.tokens(a).unwrap()).unwrap(),
                bytes
            );
        }
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        let response = h.request(GENERATE, "{}").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        let requests = h.fake.state.requests();
        assert_ne!(
            requests[0].headers["authorization"],
            requests[1].headers["authorization"]
        );
        h.obs.flush().unwrap();
        for entry in fs::read_dir(h.store.home().join("ledger")).unwrap() {
            let text = fs::read_to_string(entry.unwrap().path()).unwrap();
            for secret in [
                "synthetic-access",
                "synthetic-refresh",
                "synthetic rejection",
                "synthetic-client",
            ] {
                assert!(!text.contains(secret));
            }
        }
    }
}

#[tokio::test]
async fn edit_inline_and_binary_multipart_are_exact_and_unknown_serverrefs_fail_closed() {
    let h = Harness::new(&[Some("plus")], false).await;
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{}"));
    let response = h.request(EDIT, INLINE).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let mut multipart = b"--synthetic\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-image-2\r\n--synthetic\r\nContent-Disposition: form-data; name=\"image\"; filename=\"local.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
    multipart.extend_from_slice(&[0, 255, 128, 13, 10]);
    multipart.extend_from_slice(b"\r\n--synthetic--\r\n");
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{}"));
    let response = h
        .request_with_content_type(
            EDIT,
            multipart.clone(),
            "multipart/form-data; boundary=synthetic",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    let requests = h.fake.state.requests();
    assert_eq!(requests[0].body, INLINE);
    assert_eq!(requests[1].body.as_ref(), multipart);
    assert_eq!(
        requests[1].headers.get_all("content-type").iter().count(),
        1
    );
    assert_eq!(
        requests[1].headers["content-type"],
        "multipart/form-data; boundary=synthetic"
    );
    for body in [
        r#"{"images":[{"file_id":"file-synthetic"}]}"#,
        r#"{"images":[{"image_url":"https://example.invalid/image"}]}"#,
    ] {
        Harness::code(
            h.request(EDIT, body).send().await.unwrap(),
            StatusCode::CONFLICT,
            "image_edit_reference_unknown",
        )
        .await;
    }
    assert_eq!(h.fake.state.requests().len(), 2);
}

#[tokio::test]
async fn sentinel_alone_never_authorizes_images() {
    let h = Harness::new(&[Some("plus")], false).await;
    for route in [GENERATE, EDIT] {
        let response = reqwest::Client::new()
            .post(format!("http://{}{route}", h.servers.data_addr))
            .header("x-openai-actor-authorization", "qb-local")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(h.fake.state.requests().is_empty());
}

#[tokio::test]
async fn moderation_and_unclassified_errors_preserve_bytes_and_never_retry() {
    let h = Harness::new(&[Some("plus"), Some("pro")], false).await;
    for (status, body) in [
        (
            StatusCode::TOO_MANY_REQUESTS,
            "{\"error\":{\"code\":\"moderation_blocked\"}}",
        ),
        (
            StatusCode::BAD_REQUEST,
            "{ \"error\": {\"code\":\"moderation_blocked\"} }",
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "{\"error\":{\"message\":\"unsupported\"}}",
        ),
    ] {
        h.fake.state.push(ScriptedResponse::json(status, body));
        let response = h.request(GENERATE, "{}").send().await.unwrap();
        assert_eq!(response.status(), status);
        assert!(!response.headers().contains_key("x-qb-image-error"));
        assert!(response.headers().contains_key("x-qb-image-account"));
        assert_eq!(response.text().await.unwrap(), body);
    }
    assert_eq!(h.fake.state.requests().len(), 3);
}

#[tokio::test]
async fn transport_failure_is_operation_specific_without_retry_or_token_changes() {
    let h = Harness::new(&[Some("plus"), Some("pro")], true).await;
    let before = h.store.list().unwrap();
    Harness::code(
        h.request(EDIT, INLINE).send().await.unwrap(),
        StatusCode::BAD_GATEWAY,
        "image_edit_transport_error",
    )
    .await;
    assert_eq!(h.store.list().unwrap(), before);
    assert!(h.fake.state.requests().is_empty());
}

#[tokio::test]
async fn explicit_quota_body_rotates_once_and_known_reset_is_honored() {
    for status in [StatusCode::BAD_REQUEST, StatusCode::OK] {
        let h = Harness::new(&[Some("plus"), Some("pro")], false).await;
        let mut quota =
            ScriptedResponse::json(status, "{\"error\":{\"code\":\"insufficient_quota\"}}");
        quota.headers.insert(
            "x-ratelimit-reset",
            (chrono::Utc::now() + chrono::Duration::hours(48))
                .timestamp()
                .to_string()
                .parse()
                .unwrap(),
        );
        h.fake.state.push(quota);
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        assert_eq!(
            h.request(GENERATE, "{}")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "{}"
        );
        let attempts = h.fake.state.requests();
        assert_eq!(attempts.len(), 2);
        assert_ne!(
            attempts[0].headers["authorization"],
            attempts[1].headers["authorization"]
        );
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        h.request(GENERATE, "{}")
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let requests = h.fake.state.requests();
        assert_eq!(
            requests[1].headers["authorization"],
            requests[2].headers["authorization"]
        );
    }
}

#[tokio::test]
async fn streaming_generation_and_response_preserve_all_chunks() {
    let h = Harness::new(&[Some("plus")], false).await;
    h.fake.state.push(ScriptedResponse {
        status: StatusCode::OK,
        headers: HeaderMap::new(),
        chunks: vec![
            (Duration::ZERO, Bytes::from_static(b"first\0")),
            (Duration::from_millis(20), Bytes::from_static(b"last\xff")),
        ],
    });
    let body = reqwest::Body::wrap_stream(futures_util::stream::iter([
        Ok::<_, std::io::Error>(Bytes::from_static(b"{\"prompt\":")),
        Ok(Bytes::from_static(b"\"synthetic\"}")),
    ]));
    let response = h.request(GENERATE, body).send().await.unwrap();
    assert_eq!(response.bytes().await.unwrap(), &b"first\0last\xff"[..]);
    assert_eq!(
        h.fake.state.requests()[0].body,
        "{\"prompt\":\"synthetic\"}"
    );
}

#[tokio::test]
async fn nonreplayable_upload_does_not_rotate_and_edits_fail_before_dispatch() {
    let h = Harness::with_retain(&[Some("plus"), Some("pro")], false, 0).await;
    let body = "{\"error\":{\"code\":\"image_quota_exceeded\"}}";
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, body));
    let response = h.request(GENERATE, "{}").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key("retry-after"));
    Harness::code(
        response,
        StatusCode::TOO_MANY_REQUESTS,
        "image_generation_quota_exhausted",
    )
    .await;
    assert_eq!(h.fake.state.requests().len(), 1);
    Harness::code(
        h.request(EDIT, INLINE).send().await.unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "image_edit_body_uninspectable",
    )
    .await;
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn ambiguous_edit_fields_and_compressed_edits_fail_closed() {
    let h = Harness::new(&[Some("plus")], false).await;
    for body in [
        r#"{"images":[{"file_id":"server"}],"images":[{"image_url":"data:image/png;base64,Zm9v"}]}"#,
        r#"{"images":[{"image_url":"data:image/png;base64,Zm9v","file_id":"server"}]}"#,
        r#"{"images":[{"image_url":"data:image/png;base64,Zm9v"}],"mask":{"file_id":"server"}}"#,
    ] {
        Harness::code(
            h.request(EDIT, body).send().await.unwrap(),
            StatusCode::CONFLICT,
            "image_edit_reference_unknown",
        )
        .await;
    }
    Harness::code(
        h.request(EDIT, INLINE)
            .header("content-encoding", "gzip")
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "image_edit_body_uninspectable",
    )
    .await;
    // The multipart success fixture must replace the content type, while an
    // ambiguous request with two content types remains rejected by the relay.
    Harness::code(
        h.request(EDIT, INLINE)
            .header("content-type", "multipart/form-data; boundary=synthetic")
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "image_edit_body_uninspectable",
    )
    .await;
    assert!(h.fake.state.requests().is_empty());
}

#[tokio::test]
async fn bound_unavailable_and_image_incapable_errors_are_distinct() {
    let h = Harness::new(&[Some("plus")], false).await;
    let id = h.ids().remove(0);
    let candidate = qb_core::bindings::Candidate {
        account: h.store.load(&id).unwrap(),
        quota: None,
        inflight: 0,
    };
    let qb_core::bindings::Decision::Selected { lease, .. } = h.state.bindings.choose(
        Provider::Codex,
        Some("codex:unavailable"),
        None,
        None,
        &[candidate],
        None,
        0,
    ) else {
        panic!("synthetic binding placement");
    };
    lease.dispatched();
    drop(lease);
    let binding_before = h.binding_snapshot("codex:unavailable");
    h.store.update_metadata(&id, |a| a.drain = true).unwrap();
    Harness::code(
        h.request(EDIT, INLINE)
            .header("session-id", "unavailable")
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "image_edit_bound_account_unavailable",
    )
    .await;
    assert!(h.fake.state.requests().is_empty());
    assert_eq!(h.state.bindings.export().unwrap(), binding_before);
    h.store.update_metadata(&id, |a| a.drain = false).unwrap();
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::FORBIDDEN, "{}"));
    h.request(GENERATE, "{}")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    Harness::code(
        h.request(GENERATE, "{}").send().await.unwrap(),
        StatusCode::BAD_REQUEST,
        "image_generation_auth_rejected",
    )
    .await;
    assert_eq!(h.fake.state.requests().len(), 1);
}

#[tokio::test]
async fn normal_accounts_precede_bound_last_resort_and_affinity_breaks_tier_ties() {
    // Unset Pro is protected even when bound. Explicit false restores normal
    // priority, and true can put Plus in the last tier. Ties prefer the binding.
    for (flagged, expected_first) in [
        (vec![(1, true)], 0),
        (vec![(0, true), (1, true)], 1),
        (vec![], 0),
        (vec![(1, false)], 1),
        (vec![(0, true), (1, false)], 1),
    ] {
        let h = Harness::with_options(&[Some("plus"), Some("pro")], false, 64, &flagged).await;
        let ids = h.ids();
        h.bind(&ids[1], "codex:priority");
        let before = h.binding_snapshot("codex:priority");
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, "{}"));
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        let response = h
            .request(GENERATE, "{}")
            .header("session-id", "priority")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        let account = h.store.load(&ids[expected_first]).unwrap();
        assert_eq!(
            requests[0].headers["authorization"],
            format!("Bearer {}", h.store.tokens(&account).unwrap().access_token)
        );
        assert_ne!(
            requests[0].headers["authorization"],
            requests[1].headers["authorization"]
        );
        assert_eq!(h.state.bindings.export().unwrap(), before);
    }
}

#[tokio::test]
async fn quota_failover_revalidates_drain_pause_expiry_and_current_token_generation() {
    for change in 0..4 {
        let h = Harness::new(&[Some("plus"), Some("pro")], false).await;
        let ids = h.ids();
        let body = Bytes::from_static(b"{\"error\":{\"code\":\"image_quota_exceeded\"}}");
        h.fake.state.push(ScriptedResponse {
            status: StatusCode::TOO_MANY_REQUESTS,
            headers: HeaderMap::new(),
            chunks: vec![(Duration::from_secs(1), body.clone())],
        });
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        let request = h.request(GENERATE, "{\"model\":\"gpt-image-2\"}");
        let pending = tokio::spawn(async move { request.send().await.unwrap() });
        // The first dispatch proves the candidate snapshot already exists.
        tokio::time::timeout(Duration::from_secs(5), async {
            while h.fake.state.requests().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        match change {
            0 => h
                .store
                .update_metadata(&ids[1], |a| a.drain = true)
                .unwrap(),
            1 => h
                .store
                .update_metadata(&ids[1], |a| a.user_state = UserState::Paused)
                .unwrap(),
            2 => h.expire_token(&ids[1]),
            _ => {
                let mut account = h.store.load(&ids[1]).unwrap();
                let mut tokens = h.store.tokens(&account).unwrap();
                let generation = account.generation;
                tokens.access_token = "synthetic-replaced-current-access".into();
                account.token_meta.last_refresh_at = Some(chrono::Utc::now());
                h.store.save(&account, &tokens, Some(generation)).unwrap()
            }
        };
        let response = pending.await.unwrap();
        if change == 3 {
            assert_eq!(response.status(), StatusCode::OK);
            response.bytes().await.unwrap();
            let requests = h.fake.state.requests();
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[1].headers["authorization"],
                "Bearer synthetic-replaced-current-access"
            );
        } else {
            // The last quota result is classified if every alternative
            // became unavailable between snapshot and dispatch.
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            Harness::code(
                response,
                StatusCode::TOO_MANY_REQUESTS,
                "image_generation_quota_exhausted",
            )
            .await;
            assert_eq!(h.fake.state.requests().len(), 1);
        }
    }
}

#[tokio::test]
async fn classified_failures_reach_tool_body_and_record_original_status_and_actual_model() {
    for (path, status, reason) in [
        (
            GENERATE,
            StatusCode::UNAUTHORIZED,
            "image_generation_auth_rejected",
        ),
        (EDIT, StatusCode::FORBIDDEN, "image_edit_auth_rejected"),
        (
            GENERATE,
            StatusCode::TOO_MANY_REQUESTS,
            "image_generation_quota_exhausted",
        ),
        (
            GENERATE,
            StatusCode::BAD_REQUEST,
            "image_generation_quota_exhausted",
        ),
        (GENERATE, StatusCode::OK, "image_generation_quota_exhausted"),
        (
            EDIT,
            StatusCode::NOT_FOUND,
            "image_edit_endpoint_unsupported",
        ),
    ] {
        let h = Harness::new(&[Some("plus")], false).await;
        let body = if matches!(status, StatusCode::BAD_REQUEST | StatusCode::OK) {
            "{\"error\":{\"code\":\"insufficient_quota\",\"message\":\"synthetic-upstream-private-body\"}}"
        } else {
            "{ \"error\": {\"message\":\"synthetic-upstream-private-body\"} }"
        };
        let mut upstream = ScriptedResponse::json(status, body);
        upstream
            .headers
            .insert("content-encoding", "identity".parse().unwrap());
        upstream
            .headers
            .insert("etag", "synthetic-old-body".parse().unwrap());
        upstream
            .headers
            .insert("content-length", body.len().to_string().parse().unwrap());
        h.fake.state.push(upstream);
        let input = if path == EDIT {
            INLINE
        } else {
            "{\"model\":\"gpt-image-2\",\"prompt\":\"synthetic-prompt-private\"}"
        };
        let response = h.request(path, input).send().await.unwrap();
        let downstream = if reason.ends_with("quota_exhausted") {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            status
        };
        assert_eq!(response.status(), downstream);
        assert!(!response.headers().contains_key("content-encoding"));
        assert!(!response.headers().contains_key("etag"));
        assert_eq!(response.headers()["content-type"], "application/json");
        assert!(response.headers().contains_key("x-qb-image-account"));
        if downstream == StatusCode::TOO_MANY_REQUESTS {
            assert!(response.headers().contains_key("retry-after"));
        }
        assert_eq!(response.headers()["x-qb-image-error"], reason);
        Harness::code(response, downstream, reason).await;
        let rows = h.ledger();
        let row = rows
            .iter()
            .find(|v| v["route"].as_str().is_some_and(|r| r.ends_with(path)))
            .unwrap();
        assert_eq!(row["status"], downstream.as_u16());
        assert_eq!(row["upstream_status"], status.as_u16());
        assert_eq!(row["model"], "gpt-image-2");
        assert_eq!(row["outcome"], reason);
        assert_eq!(row["upstream_error_code"], reason);
        let serialized = serde_json::to_string(&rows).unwrap();
        assert!(!serialized.contains("synthetic-upstream-private-body"));
        assert!(!serialized.contains("synthetic-prompt-private"));
    }
}

#[tokio::test]
async fn moderation_on_401_403_preserves_bytes_but_marks_image_incapable_without_retry() {
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        let h = Harness::new(&[Some("plus")], false).await;
        let before = h.store.list().unwrap();
        let body = "{ \"error\": {\"type\":\"moderation_blocked\",\"code\":\"invalid_token\",\"message\":\"synthetic moderation details\"} }";
        h.fake.state.push(ScriptedResponse::json(status, body));
        let response = h.request(EDIT, INLINE).send().await.unwrap();
        assert_eq!(response.status(), status);
        assert!(response.headers().contains_key("x-qb-image-account"));
        assert!(!response.headers().contains_key("x-qb-image-error"));
        assert_eq!(response.text().await.unwrap(), body);
        assert_eq!(h.store.list().unwrap(), before);
        assert_eq!(h.fake.state.requests().len(), 1);
        Harness::code(
            h.request(GENERATE, "{}").send().await.unwrap(),
            StatusCode::BAD_REQUEST,
            "image_generation_auth_rejected",
        )
        .await;
        assert_eq!(h.fake.state.requests().len(), 1);
        let rows = h.ledger();
        let row = rows
            .iter()
            .find(|r| r["route"] == "POST /v1/images/edits")
            .unwrap();
        assert_eq!(row["outcome"], "image_edit_upstream_error");
    }
}

#[tokio::test]
async fn unsupported_endpoints_return_operation_specific_json_without_retry() {
    let h = Harness::new(&[Some("plus"), Some("pro")], false).await;
    for (path, status, code) in [
        (
            GENERATE,
            StatusCode::NOT_FOUND,
            "image_generation_endpoint_unsupported",
        ),
        (
            EDIT,
            StatusCode::METHOD_NOT_ALLOWED,
            "image_edit_endpoint_unsupported",
        ),
        (
            GENERATE,
            StatusCode::NOT_IMPLEMENTED,
            "image_generation_endpoint_unsupported",
        ),
    ] {
        h.fake.state.push(ScriptedResponse::json(
            status,
            "{\"error\":{\"message\":\"upstream unavailable\"}}",
        ));
        let response = h
            .request(path, if path == EDIT { INLINE } else { "{}" })
            .send()
            .await
            .unwrap();
        assert!(response.headers().contains_key("x-qb-image-account"));
        Harness::code(response, status, code).await;
    }
    assert_eq!(h.fake.state.requests().len(), 3);
}

#[tokio::test]
async fn incomplete_oversized_and_delayed_429_moderation_preserves_bytes_without_retry() {
    for delayed in [false, true] {
        let h = Harness::new(&[Some("plus"), Some("plus")], false).await;
        let (body, chunks) = if delayed {
            // The relevant code arrives after the two-second bounded hold.
            let first = Bytes::from_static(b"{\"error\":{\"message\":\"synthetic moderation\",");
            let last = Bytes::from_static(b"\"code\":\"moderation_blocked\"}}");
            let mut body = first.to_vec();
            body.extend_from_slice(&last);
            (
                Bytes::from(body),
                vec![(Duration::ZERO, first), (Duration::from_secs(3), last)],
            )
        } else {
            let body = Bytes::from(format!(
                "{{\"error\":{{\"code\":\"moderation_blocked\",\"message\":\"{}\"}}}}",
                "x".repeat(65_537)
            ));
            assert!(body.len() > 65_536);
            (body.clone(), vec![(Duration::ZERO, body)])
        };
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["error"]["code"], "moderation_blocked");
        let mut upstream = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, body.clone());
        upstream.chunks = chunks;
        h.fake.state.push(upstream);
        // This response must remain queued until an explicit new client request.
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        let response = h
            .request(GENERATE, "{\"prompt\":\"synthetic\"}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key("x-qb-image-account"));
        assert!(!response.headers().contains_key("x-qb-image-error"));
        assert_eq!(response.bytes().await.unwrap(), body);
        assert_eq!(h.fake.state.requests().len(), 1);
        let response = h.request(GENERATE, "{}").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers["authorization"], requests[1].headers["authorization"],
            "inconclusive error inspection incorrectly exhausted the first account"
        );
    }
}

#[tokio::test]
async fn image_routes_exclude_account_safety_holds_and_independent_payment_deadline() {
    for (reason, payment) in [
        (Some(HealthReason::Payment), true),
        (Some(HealthReason::Auth), false),
        (Some(HealthReason::StoreError), false),
        (Some(HealthReason::LinkStale), false),
        (None, true),
        (Some(HealthReason::Quota), true),
    ] {
        let h = Harness::new(&[Some("plus"), Some("plus")], false).await;
        let ids = h.ids();
        h.safety_hold(&ids[0], reason, payment);
        let before = h.store.list().unwrap();
        let allowed = h.store.load(&ids[1]).unwrap();
        let expected = format!("Bearer {}", h.store.tokens(&allowed).unwrap().access_token);
        for path in [GENERATE, EDIT] {
            h.fake
                .state
                .push(ScriptedResponse::json(StatusCode::OK, "{}"));
            let response = h
                .request(path, if path == EDIT { INLINE } else { "{}" })
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.bytes().await.unwrap();
        }
        let requests = h.fake.state.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(request.headers["authorization"], expected);
        }
        assert_eq!(h.store.list().unwrap(), before);
        h.store
            .update_metadata(&ids[1], |a| a.drain = true)
            .unwrap();
        for (path, code) in [
            (GENERATE, "image_generation_no_eligible_account"),
            (EDIT, "image_edit_no_eligible_account"),
        ] {
            Harness::code(
                h.request(path, if path == EDIT { INLINE } else { "{}" })
                    .send()
                    .await
                    .unwrap(),
                StatusCode::BAD_REQUEST,
                code,
            )
            .await;
        }
        assert_eq!(h.fake.state.requests().len(), 2);
    }
}

#[tokio::test]
async fn responses_quota_only_cooling_keeps_both_image_routes_eligible() {
    let h = Harness::new(&[Some("plus")], false).await;
    let id = h.ids().remove(0);
    h.safety_hold(&id, Some(HealthReason::Quota), false);
    let before = h.store.list().unwrap();
    for path in [GENERATE, EDIT] {
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        let response = h
            .request(path, if path == EDIT { INLINE } else { "{}" })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
    }
    assert_eq!(h.fake.state.requests().len(), 2);
    assert_eq!(h.store.list().unwrap(), before);
}

#[tokio::test]
async fn quota_retry_rechecks_new_account_safety_holds_before_dispatch() {
    for (reason, payment) in [
        (Some(HealthReason::Payment), true),
        (Some(HealthReason::Auth), false),
        (Some(HealthReason::StoreError), false),
        (Some(HealthReason::LinkStale), false),
        (None, true),
    ] {
        let h = Harness::new(&[Some("plus"), Some("plus")], false).await;
        let ids = h.ids();
        let mut first = ScriptedResponse::json(
            StatusCode::TOO_MANY_REQUESTS,
            "{\"error\":{\"code\":\"image_quota_exceeded\"}}",
        );
        first.headers.insert("retry-after", "3600".parse().unwrap());
        first.chunks[0].0 = Duration::from_secs(1);
        h.fake.state.push(first);
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        let request = h.request(GENERATE, "{}");
        let pending = tokio::spawn(async move { request.send().await.unwrap() });
        h.wait_requests(1).await;
        h.safety_hold(&ids[1], reason, payment);
        let before = h.store.list().unwrap();
        Harness::code(
            pending.await.unwrap(),
            StatusCode::TOO_MANY_REQUESTS,
            "image_generation_quota_exhausted",
        )
        .await;
        assert_eq!(h.fake.state.requests().len(), 1);
        Harness::code(
            h.request(GENERATE, "{}").send().await.unwrap(),
            StatusCode::TOO_MANY_REQUESTS,
            "image_generation_quota_exhausted",
        )
        .await;
        assert_eq!(h.fake.state.requests().len(), 1);
        assert_eq!(h.store.list().unwrap(), before);
    }
}

#[tokio::test]
async fn concurrent_delayed_quota_cannot_erase_auth_rejection_for_current_credentials() {
    for replace_between_dispatches in [false, true] {
        let h = Harness::new(&[Some("plus")], false).await;
        let id = h.ids().remove(0);
        let mut quota = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, "{}");
        quota.headers.insert("retry-after", "1".parse().unwrap());
        quota.chunks[0].0 = Duration::from_secs(1);
        h.fake.state.push(quota);
        let request = h.request(GENERATE, "{}");
        let delayed = tokio::spawn(async move { request.send().await.unwrap() });
        h.wait_requests(1).await;
        if replace_between_dispatches {
            h.replace_token(&id);
        }
        let before = h.store.list().unwrap();
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::UNAUTHORIZED, "{}"));
        Harness::code(
            h.request(GENERATE, "{}").send().await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "image_generation_auth_rejected",
        )
        .await;
        h.wait_requests(2).await;
        Harness::code(
            delayed.await.unwrap(),
            StatusCode::TOO_MANY_REQUESTS,
            "image_generation_quota_exhausted",
        )
        .await;
        // Expire the independent short quota deadline, leaving auth as the
        // reason to reject reuse. A queued success must never be consumed.
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        h.fake
            .state
            .push(ScriptedResponse::json(StatusCode::OK, "{}"));
        Harness::code(
            h.request(GENERATE, "{}").send().await.unwrap(),
            StatusCode::BAD_REQUEST,
            "image_generation_auth_rejected",
        )
        .await;
        assert_eq!(h.fake.state.requests().len(), 2);
        let requests = h.fake.state.requests();
        assert_eq!(
            requests[0].headers["authorization"] == requests[1].headers["authorization"],
            !replace_between_dispatches
        );
        assert_eq!(h.store.list().unwrap(), before);
    }
}

#[tokio::test]
async fn concurrent_delayed_auth_cannot_erase_quota_after_legitimate_token_replacement() {
    let h = Harness::new(&[Some("plus")], false).await;
    let id = h.ids().remove(0);
    let mut auth = ScriptedResponse::json(StatusCode::UNAUTHORIZED, "{}");
    auth.chunks[0].0 = Duration::from_secs(1);
    h.fake.state.push(auth);
    let request = h.request(GENERATE, "{}");
    let delayed = tokio::spawn(async move { request.send().await.unwrap() });
    h.wait_requests(1).await;
    let mut quota = ScriptedResponse::json(StatusCode::TOO_MANY_REQUESTS, "{}");
    quota.headers.insert("retry-after", "3600".parse().unwrap());
    h.fake.state.push(quota);
    Harness::code(
        h.request(GENERATE, "{}").send().await.unwrap(),
        StatusCode::TOO_MANY_REQUESTS,
        "image_generation_quota_exhausted",
    )
    .await;
    h.wait_requests(2).await;
    Harness::code(
        delayed.await.unwrap(),
        StatusCode::UNAUTHORIZED,
        "image_generation_auth_rejected",
    )
    .await;
    h.replace_token(&id);
    let before = h.store.list().unwrap();
    h.fake
        .state
        .push(ScriptedResponse::json(StatusCode::OK, "{}"));
    let response = h.request(GENERATE, "{}").send().await.unwrap();
    assert!(
        response.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 3000
    );
    Harness::code(
        response,
        StatusCode::TOO_MANY_REQUESTS,
        "image_generation_quota_exhausted",
    )
    .await;
    assert_eq!(h.fake.state.requests().len(), 2);
    assert_eq!(h.store.list().unwrap(), before);
}
