use super::*;
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qb_testkit::{ScriptedResponse, ScriptedUpstream};
use serde_json::json;
use sha2::{Digest, Sha256};

pub(super) struct Fixture {
    pub(super) service: LoginService,
    pub(super) fake: ScriptedUpstream,
    _home: tempfile::TempDir,
}

impl Fixture {
    pub(super) async fn new() -> Self {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/tests/oauth-manual");
        std::fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        // Explicit synthetic file backend: no inherited systemd credential or
        // global environment mutation is needed on the Linux test runner.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        crate::platform::configure_secret_open(&mut options);
        let mut config = options.open(home.path().join("config.toml")).unwrap();
        std::io::Write::write_all(&mut config, b"[secrets]\nlinux_backend = \"file-0600\"\n")
            .unwrap();
        drop(config);
        let store = Arc::new(AccountStore::acquire(home.path()).unwrap());
        let obs = Arc::new(Observability::new(home.path(), None).unwrap());
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        Self {
            service: LoginService::new(store, client, obs),
            fake: ScriptedUpstream::start().await.unwrap(),
            _home: home,
        }
    }

    pub(super) fn params(&self, provider: Provider) -> Parameters {
        let mut params = Parameters::new(provider);
        params.authorize = format!("{}/authorize", self.fake.url);
        params.token = format!("{}/token", self.fake.url);
        params.profile = format!("{}/api/oauth/profile", self.fake.url);
        params
    }

    pub(super) fn script_success(&self, provider: Provider) {
        let claims = json!({"email":"manual@example.invalid", "https://api.openai.com/auth": {
            "chatgpt_account_id":"synthetic-manual-account", "chatgpt_user_id":"synthetic-manual-user"
        }});
        let id_token = format!(
            "e30.{}.synthetic",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        self.fake.state.push(ScriptedResponse::json(StatusCode::OK, json!({
            "access_token":"synthetic-access", "refresh_token":"synthetic-refresh", "expires_in":3600, "id_token":id_token
        }).to_string()));
        if provider == Provider::Claude {
            self.fake.state.push(ScriptedResponse::json(
                StatusCode::OK,
                json!({
                    "account":{"uuid":"synthetic-manual-user", "email":"manual@example.invalid"},
                    "organization":{"uuid":"synthetic-manual-org"}
                })
                .to_string(),
            ));
        }
    }
}

fn auth_field(start: &LoginStart, key: &str) -> String {
    reqwest::Url::parse(&start.auth_url)
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == key)
        .unwrap()
        .1
        .into_owned()
}

fn paste(start: &LoginStart, provider: Provider, state: &str) -> String {
    match provider {
        Provider::Claude => format!("synthetic-code#{state}"),
        Provider::Codex => format!(
            "{}?code=synthetic-code&state={state}",
            auth_field(start, "redirect_uri")
        ),
    }
}

pub(super) async fn terminal(service: &LoginService, id: &str) -> LoginStatus {
    let mut status = service.session(id).unwrap().status.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        while status.borrow().status == LoginState::Pending {
            status.changed().await.unwrap();
        }
        status.borrow().clone()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn both_manual_providers_save_once_and_reuse_exact_pkce_exchange() {
    for provider in [Provider::Claude, Provider::Codex] {
        let fixture = Fixture::new().await;
        fixture.script_success(provider);
        let start = fixture
            .service
            .start_parameters(fixture.params(provider), false, true)
            .await
            .unwrap();
        let input = paste(&start, provider, &auth_field(&start, "state"));
        assert_eq!(
            auth_field(&start, "redirect_uri"),
            match provider {
                Provider::Claude => "https://platform.claude.com/oauth/code/callback",
                Provider::Codex => "http://localhost:1455/auth/callback",
            }
        );
        let (first, second) = tokio::join!(
            fixture.service.submit_manual(&start.login_id, &input),
            fixture.service.submit_manual(&start.login_id, &input)
        );
        assert!(first.is_ok());
        assert!(matches!(second, Err(LoginError::AlreadySubmitted)));
        let status = terminal(&fixture.service, &start.login_id).await;
        assert_eq!(status.status, LoginState::Completed);
        let saved = fixture
            .service
            .inner
            .store
            .load(status.account_id.as_ref().unwrap())
            .unwrap();
        assert_eq!(saved.provider, provider);
        assert_eq!(saved.email, "manual@example.invalid");
        assert_eq!(fixture.service.inner.store.list().unwrap().len(), 1);
        assert!(matches!(
            fixture.service.submit_manual(&start.login_id, &input).await,
            Err(LoginError::AlreadySubmitted)
        ));
        let requests = fixture.fake.state.requests();
        assert_eq!(
            requests.len(),
            if provider == Provider::Claude { 2 } else { 1 }
        );
        let fields: HashMap<String, String> = match provider {
            Provider::Claude => serde_json::from_slice(&requests[0].body).unwrap(),
            Provider::Codex => {
                let body = std::str::from_utf8(&requests[0].body).unwrap();
                reqwest::Url::parse(&format!("http://localhost/?{body}"))
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect()
            }
        };
        assert_eq!(fields["code"], "synthetic-code");
        assert_eq!(fields["redirect_uri"], auth_field(&start, "redirect_uri"));
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(fields["code_verifier"].as_bytes())),
            auth_field(&start, "code_challenge")
        );
        if provider == Provider::Claude {
            assert_eq!(fields["state"], auth_field(&start, "state"));
        }
    }
}

#[tokio::test]
async fn both_manual_providers_reject_wrong_state_and_consume_the_attempt() {
    for provider in [Provider::Claude, Provider::Codex] {
        let fixture = Fixture::new().await;
        let start = fixture
            .service
            .start_parameters(fixture.params(provider), false, true)
            .await
            .unwrap();
        assert!(matches!(
            fixture
                .service
                .submit_manual(&start.login_id, &paste(&start, provider, "wrong-state"))
                .await,
            Err(LoginError::StateMismatch)
        ));
        assert_eq!(
            terminal(&fixture.service, &start.login_id).await.status,
            LoginState::Failed
        );
        assert!(matches!(
            fixture
                .service
                .submit_manual(
                    &start.login_id,
                    &paste(&start, provider, &auth_field(&start, "state"))
                )
                .await,
            Err(LoginError::AlreadySubmitted)
        ));
        assert!(fixture.fake.state.requests().is_empty());
        assert!(fixture.service.inner.store.list().unwrap().is_empty());
    }
}

#[tokio::test]
async fn codex_manual_callback_rejects_aliases_authority_paths_fragments_and_duplicates() {
    let fixture = Fixture::new().await;
    for template in [
        "https://localhost:1455/auth/callback?code=x&state={state}",
        "http://127.0.0.1:1455/auth/callback?code=x&state={state}",
        "http://[::1]:1455/auth/callback?code=x&state={state}",
        "http://localhost.evil.invalid:1455/auth/callback?code=x&state={state}",
        "http://user@localhost:1455/auth/callback?code=x&state={state}",
        "http://localhost:1456/auth/callback?code=x&state={state}",
        "http://localhost:1455/auth/callback/extra?code=x&state={state}",
        "http://localhost:1455/auth/../auth/callback?code=x&state={state}",
        "http://localhost:1455/auth/%63allback?code=x&state={state}",
        "http://localhost:1455/auth/callback?code=x&state={state}#fragment",
        "http://localhost:1455/auth/callback?code=x&state={state}&state={state}",
        "http://localhost:1455/auth/callback?code=x&state={state}&%73tate={state}",
        "http://localhost:1455/auth/callback?code=x&code=y&state={state}",
        "http://localhost:1455/auth/callback?code=&state={state}",
        "http://localhost:1455/auth/callback?code=%ZZ&state={state}",
    ] {
        let start = fixture
            .service
            .start_parameters(fixture.params(Provider::Codex), false, true)
            .await
            .unwrap();
        let input = template.replace("{state}", &auth_field(&start, "state"));
        assert!(matches!(
            fixture.service.submit_manual(&start.login_id, &input).await,
            Err(LoginError::InvalidCallback)
        ));
        assert_eq!(
            terminal(&fixture.service, &start.login_id).await.status,
            LoginState::Failed
        );
    }
    assert!(fixture.fake.state.requests().is_empty());
}

#[tokio::test]
async fn manual_start_never_binds_the_occupied_callback_port() {
    let fixture = Fixture::new().await;
    let occupied = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = occupied.local_addr().unwrap().port();
    for provider in [Provider::Claude, Provider::Codex] {
        let mut params = fixture.params(provider);
        params.port = port;
        let start = fixture
            .service
            .start_parameters(params, false, true)
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
    }
    let mut params = fixture.params(Provider::Codex);
    params.port = port;
    assert!(matches!(
        fixture.service.start_parameters(params, false, false).await,
        Err(LoginError::CallbackPortBusy { .. })
    ));
    let mut params = fixture.params(Provider::Codex);
    params.port = 0;
    let browser = fixture
        .service
        .start_parameters(params, false, false)
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .service
            .submit_manual(&browser.login_id, "synthetic-code")
            .await,
        Err(LoginError::NotManual)
    ));
    fixture.service.cancel(&browser.login_id).await.unwrap();
    assert!(fixture.fake.state.requests().is_empty());
}

#[tokio::test]
async fn cancellation_before_and_during_exchange_never_saves_manual_credentials() {
    for provider in [Provider::Claude, Provider::Codex] {
        let fixture = Fixture::new().await;
        let start = fixture
            .service
            .start_parameters(fixture.params(provider), false, true)
            .await
            .unwrap();
        fixture.service.cancel(&start.login_id).await.unwrap();
        let input = paste(&start, provider, &auth_field(&start, "state"));
        assert!(matches!(
            fixture.service.submit_manual(&start.login_id, &input).await,
            Err(LoginError::Cancelled)
        ));
        assert!(fixture.fake.state.requests().is_empty());

        fixture.script_success(provider);
        let (arrived, release) = fixture.fake.state.pause_next_response();
        let start = fixture
            .service
            .start_parameters(fixture.params(provider), false, true)
            .await
            .unwrap();
        let input = paste(&start, provider, &auth_field(&start, "state"));
        fixture
            .service
            .submit_manual(&start.login_id, &input)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        fixture.service.cancel(&start.login_id).await.unwrap();
        release.send(()).ok();
        assert_eq!(
            fixture
                .service
                .status(&start.login_id)
                .await
                .unwrap()
                .status,
            LoginState::Cancelled
        );
        assert!(fixture.service.inner.store.list().unwrap().is_empty());
        assert_eq!(fixture.fake.state.requests().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn manual_expiry_wins_over_a_ready_submission_before_the_task_runs() {
    let fixture = Fixture::new().await;
    for provider in [Provider::Claude, Provider::Codex] {
        let start = fixture
            .service
            .start_parameters(fixture.params(provider), false, true)
            .await
            .unwrap();
        let input = paste(&start, provider, &auth_field(&start, "state"));
        fixture
            .service
            .submit_manual(&start.login_id, &input)
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(600)).await;
        assert_eq!(
            terminal(&fixture.service, &start.login_id).await.status,
            LoginState::Expired
        );
        assert!(matches!(
            fixture.service.submit_manual(&start.login_id, &input).await,
            Err(LoginError::Expired)
        ));
    }
    assert!(fixture.fake.state.requests().is_empty());
    assert!(fixture.service.inner.store.list().unwrap().is_empty());
}

#[tokio::test]
async fn manual_expiry_during_exchange_never_saves_or_reexchanges() {
    for provider in [Provider::Claude, Provider::Codex] {
        let fixture = Fixture::new().await;
        fixture.script_success(provider);
        let (arrived, release) = fixture.fake.state.pause_next_response();
        let start = fixture
            .service
            .start_parameters(fixture.params(provider), false, true)
            .await
            .unwrap();
        // Consume599s awaiting input, before HTTP/exchange timeouts start.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(599)).await;
        tokio::time::resume();
        assert_eq!(
            fixture
                .service
                .status(&start.login_id)
                .await
                .unwrap()
                .status,
            LoginState::Pending
        );
        assert!(fixture.fake.state.requests().is_empty());
        let input = paste(&start, provider, &auth_field(&start, "state"));
        fixture
            .service
            .submit_manual(&start.login_id, &input)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        // Actual token request remains held. Cross login expiry without
        // crossing the request's5s / exchange's30s timeout.
        assert_eq!(fixture.fake.state.requests().len(), 1);
        assert_eq!(
            fixture
                .service
                .status(&start.login_id)
                .await
                .unwrap()
                .status,
            LoginState::Pending
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            terminal(&fixture.service, &start.login_id).await.status,
            LoginState::Expired
        );
        assert!(matches!(
            fixture.service.submit_manual(&start.login_id, &input).await,
            Err(LoginError::Expired)
        ));
        tokio::time::resume();
        release.send(()).ok();
        assert_eq!(fixture.fake.state.requests().len(), 1);
        assert!(fixture.service.inner.store.list().unwrap().is_empty());
    }
}

#[tokio::test]
async fn claude_manual_requires_code_and_one_state_suffix() {
    let fixture = Fixture::new().await;
    for input in [
        "synthetic-code",
        "#state",
        "code#",
        "code#state#extra",
        "code with spaces#state",
        "",
    ] {
        let start = fixture
            .service
            .start_parameters(fixture.params(Provider::Claude), false, true)
            .await
            .unwrap();
        assert!(matches!(
            fixture.service.submit_manual(&start.login_id, input).await,
            Err(LoginError::InvalidCallback)
        ));
        assert_eq!(
            terminal(&fixture.service, &start.login_id).await.status,
            LoginState::Failed
        );
    }
    assert!(fixture.fake.state.requests().is_empty());
}
