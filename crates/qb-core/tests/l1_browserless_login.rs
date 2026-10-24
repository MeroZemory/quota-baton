#![cfg(all(feature = "testkit", debug_assertions))]

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_core::{
    accounts::{AccountStore, Health, Provider, Source, StoreError},
    admin,
    config::Config,
    http::{self, RunningServers},
    local_secrets::LocalSecrets,
    oauth::{DeviceTestConfig, LoginService, LoginStart, LoginState, LoginStatus, LoginTestConfig},
    observability::Observability,
};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, sync::Arc, time::Duration};
use tokio::net::TcpListener;

const ADMIN: &str = "synthetic-browserless-admin";
const CODE: &str = "synthetic-browserless-code";
const ACCESS: &str = "synthetic-browserless-access";
const REFRESH: &str = "synthetic-browserless-refresh";
const EMAIL: &str = "browserless@example.invalid";
const CLAUDE_MANUAL_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    servers: RunningServers,
    service: LoginService,
    store: Arc<AccountStore>,
    _obs: Arc<Observability>,
    fake: ScriptedUpstream,
    client: reqwest::Client,
    callback: TcpListener,
    _root: tempfile::TempDir,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Harness {
    async fn new() -> Self {
        let serial = SERIAL.lock().await;
        // Normalize the workspace ancestor before creating the private fixture.
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/l1-browserless-login-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        // Keep the private child under root, whose Drop removes the whole fixture.
        let home = tempfile::tempdir_in(root.path()).unwrap().keep();
        let mut config_file = tempfile::NamedTempFile::new_in(&home).unwrap();
        std::io::Write::write_all(
            &mut config_file,
            b"[secrets]\nlinux_backend = \"file-0600\"\n",
        )
        .unwrap();
        config_file.persist(home.join("config.toml")).unwrap();
        let store = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match AccountStore::acquire(&home) {
                    Ok(store) => break Arc::new(store),
                    Err(StoreError::LeaseBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("synthetic store acquisition failed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        let obs = Arc::new(Observability::new(&home, None).unwrap());
        let fake = ScriptedUpstream::start().await.unwrap();
        // An accidental browser-mode start must fail instead of opening a listener.
        let callback = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let service = LoginService::with_test_config(
            store.clone(),
            client.clone(),
            obs.clone(),
            LoginTestConfig {
                authorize_url: format!("{}/authorize", fake.url),
                token_url: format!("{}/token", fake.url),
                profile_url: format!("{}/profile", fake.url),
                callback_port: callback.local_addr().unwrap().port(),
            },
        )
        .unwrap()
        .with_device_test_config(DeviceTestConfig {
            usercode_url: format!("{}/device/usercode", fake.url),
            poll_url: format!("{}/device/token", fake.url),
            poll_interval: Duration::from_millis(25),
            lifetime: Duration::from_secs(10),
        })
        .unwrap();
        let mut config = Config::default();
        config.server.data_port = 0;
        config.server.admin_port = 0;
        // login_routes alone has no authentication layer. Use the production
        // wrapper so start, status and submit all cross the real admin boundary.
        let servers = http::spawn_with_admin(
            &config,
            LocalSecrets {
                client_key: "synthetic-browserless-client".into(),
                admin_token: ADMIN.into(),
            },
            admin::login_routes(service.clone()),
        )
        .await
        .unwrap();
        Self {
            servers,
            service,
            store,
            _obs: obs,
            fake,
            client,
            callback,
            _root: root,
            _serial: serial,
        }
    }

    fn url(&self, suffix: &str) -> String {
        format!(
            "http://{}/admin/v1/accounts/login{suffix}",
            self.servers.admin_addr
        )
    }

    async fn start(&self, provider: Provider) -> LoginStart {
        let response = self
            .client
            .post(self.url(""))
            .bearer_auth(ADMIN)
            .json(&json!({
                "provider":provider,"replace_linked":false,"no_browser":true,
                "paste_redirect":provider == Provider::Codex
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let start: LoginStart = response.json().await.unwrap();
        let auth = reqwest::Url::parse(&start.auth_url).unwrap();
        let redirect = parameter(&auth, "redirect_uri");
        match provider {
            Provider::Claude => assert_eq!(redirect, CLAUDE_MANUAL_REDIRECT),
            Provider::Codex => assert_eq!(
                redirect,
                format!(
                    "http://localhost:{}/auth/callback",
                    self.callback.local_addr().unwrap().port()
                )
            ),
        }
        assert_eq!(parameter(&auth, "code_challenge_method"), "S256");
        assert!(!parameter(&auth, "state").is_empty());
        start
    }

    async fn submit(&self, id: &str, input: &str) -> reqwest::Response {
        self.client
            .post(self.url(&format!("/{id}/submit")))
            .bearer_auth(ADMIN)
            .json(&json!({"input":input}))
            .send()
            .await
            .unwrap()
    }

    async fn terminal(&self, id: &str) -> LoginStatus {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response = self
                    .client
                    .get(self.url(&format!("/{id}")))
                    .bearer_auth(ADMIN)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let status: LoginStatus = response.json().await.unwrap();
                assert_redacted(&serde_json::to_string(&status).unwrap());
                if status.status != LoginState::Pending {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("manual login did not reach a terminal status")
    }

    fn replies(&self, provider: Provider) {
        let claims = json!({"email":EMAIL,"https://api.openai.com/auth":{
            "chatgpt_account_id":"synthetic-browserless-account",
            "chatgpt_user_id":"synthetic-browserless-user","chatgpt_plan_type":"plus"
        }});
        let id_token = format!(
            "e30.{}.synthetic-signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        self.fake.state.push(ScriptedResponse::json(
            StatusCode::OK,
            serde_json::to_vec(&json!({"access_token":ACCESS,"refresh_token":REFRESH,
                "id_token":id_token,"expires_in":3600}))
            .unwrap(),
        ));
        if provider == Provider::Claude {
            self.fake.state.push(ScriptedResponse::json(
                StatusCode::OK,
                serde_json::to_vec(&json!({"account":{
                    "uuid":"synthetic-browserless-uuid","email":EMAIL},
                    "organization":{"uuid":"synthetic-browserless-org"}}))
                .unwrap(),
            ));
        }
    }

    async fn close(mut self) {
        self.service.cancel_all().await;
        self.servers.shutdown();
        tokio::time::timeout(Duration::from_secs(5), async {
            (&mut self.servers.data_task).await.unwrap().unwrap();
            (&mut self.servers.admin_task).await.unwrap().unwrap();
        })
        .await
        .expect("admin fixture did not drain");
    }
}

fn parameter(url: &reqwest::Url, key: &str) -> String {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .unwrap()
        .1
        .into_owned()
}

fn submission(provider: Provider, start: &LoginStart, wrong_state: bool) -> String {
    let auth = reqwest::Url::parse(&start.auth_url).unwrap();
    let state = if wrong_state {
        "synthetic-wrong-state".into()
    } else {
        parameter(&auth, "state")
    };
    match provider {
        Provider::Claude => format!("{CODE}#{state}"),
        Provider::Codex => {
            let mut redirect = reqwest::Url::parse(&parameter(&auth, "redirect_uri")).unwrap();
            redirect
                .query_pairs_mut()
                .extend_pairs([("code", CODE), ("state", &state)]);
            redirect.into()
        }
    }
}

fn assert_redacted(body: &str) {
    for secret in [CODE, ACCESS, REFRESH, "synthetic-signature"] {
        assert!(
            !body.contains(secret),
            "login status leaked synthetic OAuth material"
        );
    }
}

#[tokio::test]
async fn codex_no_browser_defaults_to_device_through_authenticated_admin() {
    let h = Harness::new().await;
    let verifier = "synthetic-device-verifier-with-enough-bounded-characters";
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    for body in [
        json!({
            "device_auth_id":"synthetic-device-id","user_code":"ABCD-EFGH",
            "interval":"1","expires_in":60
        }),
        json!({
            "authorization_code":CODE,"code_challenge":challenge,"code_verifier":verifier
        }),
    ] {
        h.fake.state.push(ScriptedResponse::json(
            StatusCode::OK,
            serde_json::to_vec(&body).unwrap(),
        ));
    }
    h.replies(Provider::Codex);
    let response = h
        .client
        .post(h.url(""))
        .bearer_auth(ADMIN)
        .json(&json!({"provider":"codex","no_browser":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let start: LoginStart = response.json().await.unwrap();
    let prompt = start.device.as_ref().expect("device prompt is the default");
    assert_eq!(
        prompt.verification_url,
        "https://auth.openai.com/codex/device"
    );
    assert_eq!(start.auth_url, prompt.verification_url);
    assert_eq!(prompt.user_code, "ABCD-EFGH");
    assert!(prompt.expires_in <= 10);
    assert!(!format!("{start:?}").contains("ABCD-EFGH"));
    let status = h.terminal(&start.login_id).await;
    assert_eq!(status.status, LoginState::Completed);
    assert_redacted(&serde_json::to_string(&status).unwrap());
    let account = h.store.load(status.account_id.as_ref().unwrap()).unwrap();
    assert_eq!(h.store.tokens(&account).unwrap().access_token, ACCESS);
    let requests = h.fake.state.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].path_and_query, "/device/usercode");
    assert_eq!(requests[1].path_and_query, "/device/token");
    assert_eq!(requests[2].path_and_query, "/token");
    let encoded = reqwest::Url::parse(&format!(
        "http://127.0.0.1/?{}",
        std::str::from_utf8(&requests[2].body).unwrap()
    ))
    .unwrap();
    let form: std::collections::BTreeMap<_, _> = encoded.query_pairs().into_owned().collect();
    assert_eq!(
        form["redirect_uri"],
        "https://auth.openai.com/deviceauth/callback"
    );
    assert_eq!(form["code_verifier"], verifier);
    h.close().await;
}

#[tokio::test]
async fn manual_http_login_persists_both_providers_and_refuses_replay() {
    let h = Harness::new().await;
    let unauthorized = h
        .client
        .post(h.url(""))
        .json(&json!({
            "provider":"claude","no_browser":true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let before = h.fake.state.requests().len();
    for (provider, no_browser) in [(Provider::Claude, true), (Provider::Codex, false)] {
        let response = h
            .client
            .post(h.url(""))
            .bearer_auth(ADMIN)
            .json(&json!({
                "provider":provider,"no_browser":no_browser,"paste_redirect":true
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(h.fake.state.requests().len(), before);
    for provider in [Provider::Claude, Provider::Codex] {
        h.replies(provider);
        let start = h.start(provider).await;
        let input = submission(provider, &start, false);
        let unauthorized = h
            .client
            .post(h.url(&format!("/{}/submit", start.login_id)))
            .json(&json!({"input":input}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let before = h.fake.state.requests().len();
        let (exchange_arrived, release_exchange) = h.fake.state.pause_next_response();
        let response = h.submit(&start.login_id, &input).await;
        assert_eq!(response.status(), StatusCode::OK);
        let submitted: LoginStatus = response.json().await.unwrap();
        assert!(matches!(
            submitted.status,
            LoginState::Pending | LoginState::Completed
        ));
        assert_redacted(&serde_json::to_string(&submitted).unwrap());
        // Hold the real token endpoint open: replay must be refused even while
        // the first exchange is still pending, without creating another request.
        tokio::time::timeout(Duration::from_secs(5), exchange_arrived)
            .await
            .unwrap()
            .unwrap();
        let replay = h.submit(&start.login_id, &input).await;
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
        assert_eq!(h.fake.state.requests().len(), before + 1);
        release_exchange.send(()).unwrap();
        let status = h.terminal(&start.login_id).await;
        assert_eq!(status.status, LoginState::Completed);
        let account = h.store.load(status.account_id.as_ref().unwrap()).unwrap();
        assert_eq!(account.provider, provider);
        assert_eq!(account.source, Source::Oauth);
        assert_eq!(account.health, Health::Ready);
        assert_eq!(account.email, EMAIL);
        let tokens = h.store.tokens(&account).unwrap();
        assert_eq!(tokens.access_token, ACCESS);
        assert_eq!(tokens.refresh_token, REFRESH);

        let requests = h.fake.state.requests();
        let exchanges: Vec<_> = requests[before..]
            .iter()
            .filter(|request| request.path_and_query == "/token")
            .collect();
        assert_eq!(exchanges.len(), 1);
        let token = exchanges[0];
        let fields: Value = if provider == Provider::Claude {
            serde_json::from_slice(&token.body).unwrap()
        } else {
            let encoded = reqwest::Url::parse(&format!(
                "http://127.0.0.1/?{}",
                std::str::from_utf8(&token.body).unwrap()
            ))
            .unwrap();
            Value::Object(
                encoded
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                    .collect(),
            )
        };
        let auth = reqwest::Url::parse(&start.auth_url).unwrap();
        assert_eq!(fields["code"], CODE);
        assert_eq!(fields["redirect_uri"], parameter(&auth, "redirect_uri"));
        let verifier = fields["code_verifier"].as_str().unwrap();
        assert!(URL_SAFE_NO_PAD.decode(verifier).unwrap().len() >= 32);
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            parameter(&auth, "code_challenge")
        );
        let profiles = requests[before..]
            .iter()
            .filter(|request| request.path_and_query == "/profile")
            .count();
        assert_eq!(profiles, usize::from(provider == Provider::Claude));
        let count = requests.len();
        let replay = h.submit(&start.login_id, &input).await;
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
        assert_redacted(&replay.text().await.unwrap());
        assert_eq!(h.fake.state.requests().len(), count);
    }
    assert_eq!(h.store.list().unwrap().len(), 2);
    h.close().await;
}

#[tokio::test]
async fn wrong_state_http_submission_never_exchanges_or_stores_either_provider() {
    let h = Harness::new().await;
    for provider in [Provider::Claude, Provider::Codex] {
        let start = h.start(provider).await;
        let response = h
            .submit(&start.login_id, &submission(provider, &start, true))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_redacted(&response.text().await.unwrap());
        let status = h.terminal(&start.login_id).await;
        assert_eq!(status.status, LoginState::Failed);
        assert!(status.account_id.is_none());
        // Replacing a consumed invalid submission with a valid one is a replay.
        let replay = h
            .submit(&start.login_id, &submission(provider, &start, false))
            .await;
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
        assert!(h.fake.state.requests().is_empty());
        assert!(h.store.list().unwrap().is_empty());
    }
    h.close().await;
}

#[tokio::test]
async fn codex_http_submission_requires_the_exact_localhost_final_url() {
    let h = Harness::new().await;
    for variant in ["ip-alias", "wrong-port", "wrong-path", "fragment"] {
        let start = h.start(Provider::Codex).await;
        let mut input = reqwest::Url::parse(&submission(Provider::Codex, &start, false)).unwrap();
        match variant {
            "ip-alias" => input.set_host(Some("127.0.0.1")).unwrap(),
            "wrong-port" => input.set_port(Some(0)).unwrap(),
            "wrong-path" => input.set_path("/other/callback"),
            "fragment" => input.set_fragment(Some("synthetic-fragment")),
            _ => unreachable!(),
        }
        let response = h.submit(&start.login_id, input.as_str()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{variant}");
        assert_eq!(h.terminal(&start.login_id).await.status, LoginState::Failed);
        assert!(h.fake.state.requests().is_empty());
        assert!(h.store.list().unwrap().is_empty());
    }
    h.close().await;
}
