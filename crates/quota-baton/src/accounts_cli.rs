use clap::{Args, Subcommand, ValueEnum};
use qb_core::{accounts::AccountStore, config::Config};
use qb_proto::{CpaImportRequest, ImportMode};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug)]
pub enum UsageCliError {
    CoreRequired,
    TooSoon,
}
impl std::fmt::Display for UsageCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CoreRequired => "사용량을 갱신하려면 코어를 먼저 실행해 주세요.",
            Self::TooSoon => "사용량 조회 간격이 지나지 않았습니다. 잠시 후 다시 시도해 주세요.",
        })
    }
}
impl std::error::Error for UsageCliError {}

#[derive(Subcommand)]
pub enum AccountsCommand {
    #[command(about = "브라우저로 계정에 로그인합니다")]
    Login {
        #[arg(value_enum)]
        provider: LoginProvider,
        #[arg(long, help = "CPA 연결을 해제하고 직접 로그인 계정으로 전환합니다")]
        replace_linked: bool,
        #[arg(long, help = "브라우저를 열지 않고 로그인 주소만 표시합니다")]
        no_browser: bool,
    },
    #[command(about = "계정 목록을 확인합니다")]
    List,
    #[command(about = "계정 사용을 일시 중지합니다")]
    Pause { id: String },
    #[command(about = "계정 사용을 재개합니다")]
    Resume { id: String },
    #[command(about = "계정을 비활성화합니다")]
    Disable { id: String },
    #[command(about = "계정을 활성화합니다")]
    Enable { id: String },
    #[command(about = "계정 사용량을 갱신합니다")]
    RefreshUsage { id: String },
    #[command(about = "CPA 계정을 가져옵니다")]
    Import {
        #[command(subcommand)]
        source: ImportSource,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum LoginProvider {
    Claude,
    Codex,
}

impl From<LoginProvider> for qb_core::accounts::Provider {
    fn from(provider: LoginProvider) -> Self {
        match provider {
            LoginProvider::Claude => Self::Claude,
            LoginProvider::Codex => Self::Codex,
        }
    }
}

#[derive(Subcommand)]
pub enum ImportSource {
    Cpa(CpaArgs),
}

#[derive(Args)]
pub struct CpaArgs {
    #[arg(long, help = "CPA 인증 폴더를 지정합니다")]
    dir: Option<PathBuf>,
    #[arg(long, num_args = 1.., help = "provider:email 형식으로 활성화할 계정을 지정합니다")]
    activate: Vec<String>,
    #[arg(long, help = "저장하지 않고 결과를 확인합니다")]
    dry_run: bool,
    #[arg(long, help = "M8 전환 단계에서만 사용할 수 있습니다")]
    adopt: bool,
}

pub async fn run(command: AccountsCommand, home: &Path, config: &Config) -> anyhow::Result<()> {
    match command {
        AccountsCommand::Login {
            provider,
            replace_linked,
            no_browser,
        } => {
            login(home, config, provider.into(), replace_linked, no_browser).await?;
        }
        AccountsCommand::RefreshUsage { id } => refresh_usage(home, config, &id).await?,
        AccountsCommand::Pause { id } => action(home, config, &id, "pause").await?,
        AccountsCommand::Resume { id } => action(home, config, &id, "resume").await?,
        AccountsCommand::Disable { id } => action(home, config, &id, "disable").await?,
        AccountsCommand::Enable { id } => action(home, config, &id, "enable").await?,
        AccountsCommand::List => {
            let accounts = AccountStore::list_metadata(home)?;
            println_utf8!(
                "계정 ID\t제공자\t이메일\t요금제\t출처\t연결 방식\t사용 상태\t계정 상태\t만료 시각"
            );
            for account in accounts {
                let value = serde_json::to_value(account)?;
                println_utf8!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    cell(&value["id"]),
                    cell(&value["provider"]),
                    cell(&value["email"]),
                    cell(&value["plan"]),
                    translated(&value["source"]["kind"]),
                    translated(&value["source"]["mode"]),
                    translated(&value["user_state"]),
                    health_label(&value["health"]),
                    cell(&value["token_meta"]["expires_at"])
                );
            }
        }
        AccountsCommand::Import {
            source: ImportSource::Cpa(args),
        } => {
            anyhow::ensure!(!args.adopt, "소유권 이전은 M8 전환 단계에서만 가능합니다.");
            let dir = match args.dir {
                Some(dir) => dir,
                None => PathBuf::from(
                    std::env::var_os("USERPROFILE")
                        .ok_or_else(|| anyhow::anyhow!("USERPROFILE is missing"))?,
                )
                .join(".cli-proxy-api/auths"),
            };
            let request = CpaImportRequest {
                source: "cpa".into(),
                mode: ImportMode::Linked,
                dir: Some(dir.clone()),
                activate: args.activate.clone(),
                dry_run: args.dry_run,
            };
            let client = reqwest::Client::builder()
                .no_proxy()
                .user_agent(concat!("quota-baton/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(5))
                .build()?;
            let base = format!("http://127.0.0.1:{}/admin/v1", config.server.admin_port);
            let token = std::fs::read_to_string(home.join("admin.token")).ok();
            let mut probe = client.get(format!("{base}/health"));
            if let Some(token) = &token {
                probe = probe.bearer_auth(token.trim());
            }
            let rows = match probe.send().await {
                Ok(health) => {
                    health.error_for_status()?;
                    let response = client
                        .post(format!("{base}/accounts/import"))
                        .timeout(Duration::from_secs(60))
                        .bearer_auth(token.as_deref().unwrap_or("").trim())
                        .json(&request)
                        .send()
                        .await?
                        .error_for_status()?;
                    response.json::<serde_json::Value>().await?
                }
                Err(error) if error.is_connect() => {
                    if args.dry_run {
                        serde_json::to_value(qb_core::accounts::import::preview(
                            home,
                            &dir,
                            &args.activate,
                        )?)?
                    } else {
                        let store = AccountStore::acquire(home)?;
                        serde_json::to_value(qb_core::accounts::import::run(
                            &store,
                            &dir,
                            &args.activate,
                            false,
                        )?)?
                    }
                }
                Err(error) => return Err(error.into()),
            };
            println_utf8!("계정 ID\t제공자\t이메일\t요금제\t만료 시각\t사용 상태\t결과\t이유");
            for row in rows
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Invalid import response"))?
            {
                println_utf8!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    cell(&row["id"]),
                    cell(&row["provider"]),
                    cell(&row["email"]),
                    cell(&row["plan"]),
                    cell(&row["expires_at"]),
                    translated(&row["user_state"]),
                    translated(&row["action"]),
                    if row["reason"] == "older_token" {
                        format!(
                            "이전 토큰 (저장 시각 {} > 파일 시각 {})",
                            cell(&row["stored_timestamp"]),
                            cell(&row["file_timestamp"])
                        )
                    } else {
                        translated(&row["reason"])
                    }
                );
            }
        }
    }
    Ok(())
}

fn translated(value: &serde_json::Value) -> String {
    match value.as_str() {
        Some("active") => "활성".into(),
        Some("paused") => "일시 중지".into(),
        Some("disabled") => "비활성".into(),
        Some("quota") => "할당량 소진".into(),
        Some("rate") => "요청 속도 제한".into(),
        Some("auth") => "인증 오류".into(),
        Some("permission") => "권한 부족".into(),
        Some("payment") => "결제 필요".into(),
        Some("store_error") => "인증 정보 저장 실패".into(),
        Some("link_stale") => "연결된 토큰 갱신 필요".into(),
        Some("linked") => "연결".into(),
        Some("adopted") => "소유권 이전".into(),
        Some("oauth") => "직접 로그인".into(),
        Some("created") => "생성".into(),
        Some("updated") => "갱신".into(),
        Some("skipped") => "건너뜀".into(),
        Some("unknown_type") => "지원하지 않는 계정 유형".into(),
        Some("invalid_data") => "잘못된 계정 데이터".into(),
        _ => cell(value),
    }
}

fn health_label(value: &serde_json::Value) -> String {
    match value["kind"].as_str() {
        Some("ready") => "준비됨".into(),
        Some("cooling") => "대기 중".into(),
        Some("needs_relogin") => "재로그인 필요".into(),
        _ => "알 수 없음".into(),
    }
}

fn cell(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::Null => "-".into(),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => value.to_string(),
        _ => "-".into(),
    };
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

// A responding admin server owns the store even if authentication or health fails.
async fn running_admin(
    home: &Path,
    config: &Config,
) -> anyhow::Result<Option<(reqwest::Client, String, String)>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("quota-baton/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(5))
        .build()?;
    let base = format!("http://127.0.0.1:{}/admin/v1", config.server.admin_port);
    let token = std::fs::read_to_string(home.join("admin.token")).unwrap_or_default();
    match client
        .get(format!("{base}/health"))
        .bearer_auth(token.trim())
        .send()
        .await
    {
        Ok(response) => {
            let health: qb_proto::HealthResponse = response.error_for_status()?.json().await?;
            anyhow::ensure!(health.ok, "코어 상태를 확인해 주세요.");
            Ok(Some((client, base, token)))
        }
        Err(error) if error.is_connect() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn action(home: &Path, config: &Config, id: &str, action: &str) -> anyhow::Result<()> {
    let state = match action {
        "pause" => qb_core::accounts::UserState::Paused,
        "disable" => qb_core::accounts::UserState::Disabled,
        _ => qb_core::accounts::UserState::Active,
    };
    // IDs are path segments, never arbitrary URLs or relative paths.
    anyhow::ensure!(
        qb_core::observability::safe_identifier(id).is_some(),
        "계정 ID를 확인해 주세요."
    );
    if let Some((client, base, token)) = running_admin(home, config).await? {
        client
            .post(format!("{base}/accounts/{id}/{action}"))
            .bearer_auth(token.trim())
            .send()
            .await?
            .error_for_status()?;
    } else {
        let home = home.to_owned();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let store = AccountStore::acquire(&home)?;
            store.update_metadata(&id, |account: &mut qb_core::accounts::Account| {
                account.user_state = state;
            })
        })
        .await??;
    }
    println_utf8!(
        "계정 {}: {}",
        cell(&serde_json::json!(id)),
        translated(&serde_json::to_value(state)?)
    );
    Ok(())
}

async fn refresh_usage(home: &Path, config: &Config, id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        qb_core::observability::safe_identifier(id).is_some(),
        "계정 ID를 확인해 주세요."
    );
    let (client, base, token) = running_admin(home, config)
        .await?
        .ok_or(UsageCliError::CoreRequired)?;
    let response = client
        .post(format!("{base}/accounts/{id}/refresh-usage"))
        .timeout(Duration::from_secs(35))
        .bearer_auth(token.trim())
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(UsageCliError::TooSoon.into());
    }
    response.error_for_status()?;
    println_utf8!("계정 {}의 사용량을 갱신했습니다.", id);
    Ok(())
}

pub async fn status(home: &Path, config: &Config) -> anyhow::Result<()> {
    let (running, accounts) = if let Some((client, base, token)) =
        running_admin(home, config).await?
    {
        let state: serde_json::Value = client
            .get(format!("{base}/state"))
            .bearer_auth(token.trim())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let accounts = state["accounts"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Invalid account state"))?
            .clone();
        (true, accounts)
    } else {
        let home = home.to_owned();
        let accounts =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<serde_json::Value>> {
                let quota: serde_json::Value = match std::fs::read(home.join("state/quota.json")) {
                    Ok(bytes) => serde_json::from_slice(&bytes)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        serde_json::json!({})
                    }
                    Err(error) => return Err(error.into()),
                };
                Ok(AccountStore::list_metadata(&home)?.into_iter().map(|account| {
                let snapshot = &quota["accounts"][&account.id];
                let probe_pending = account.auth_probe_pending
                    || (matches!(account.health, qb_core::accounts::Health::Ready)
                        && account.auth_failures > 0);
                serde_json::json!({
                    "id": account.id, "provider": account.provider, "label": account.label,
                    "email": account.email, "plan": account.plan, "user_state": account.user_state,
                    "health": account.health, "windows": snapshot["windows"],
                    "probe_pending": probe_pending
                })
            }).collect())
            })
            .await??;
        (false, accounts)
    };
    println_utf8!(
        "코어: {}",
        if running {
            "실행 중"
        } else {
            "중지됨 (저장된 상태)"
        }
    );
    println_utf8!(
        "계정 ID\t제공자\t라벨\t사용 상태\t계정 상태\t5시간 사용률\t5시간 초기화\t7일 사용률\t7일 초기화\t상태 이유\t대기 종료"
    );
    let now = std::time::SystemTime::now();
    for account in accounts {
        let window = |name: &str| {
            account["windows"]
                .as_array()
                .and_then(|windows| windows.iter().find(|window| window["name"] == name))
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        };
        let five = window("5h");
        let seven = window("7d");
        println_utf8!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            cell(&account["id"]),
            cell(&account["provider"]),
            cell(&account["label"]),
            translated(&account["user_state"]),
            if account["probe_pending"].as_bool() == Some(true) {
                "인증 확인 대기".into()
            } else {
                health_label(&account["health"])
            },
            percentage(&five, now),
            cell(&five["reset_at"]),
            percentage(&seven, now),
            cell(&seven["reset_at"]),
            translated(&account["health"]["reason"]),
            cell(&account["health"]["until"])
        );
    }
    Ok(())
}

fn percentage(window: &serde_json::Value, now: std::time::SystemTime) -> String {
    // Reuse the existing chrono-backed timestamp type without a new CLI dependency.
    let timestamp = qb_core::accounts::TokenMeta {
        expires_at: serde_json::from_value(window["reset_at"].clone()).ok(),
        ..Default::default()
    };
    if timestamp
        .expires_at
        .is_some_and(|reset| std::time::SystemTime::from(reset) <= now)
    {
        return "리셋됨".into();
    }
    window["used_pct"]
        .as_f64()
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "-".into())
}

enum LoginBackend {
    Admin {
        client: reqwest::Client,
        base: String,
        token: String,
    },
    Local(qb_core::oauth::LoginService),
}

impl LoginBackend {
    async fn status(&self, id: &str) -> anyhow::Result<serde_json::Value> {
        match self {
            Self::Admin {
                client,
                base,
                token,
            } => Ok(client
                .get(format!("{base}/accounts/login/{id}"))
                .bearer_auth(token.trim())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?),
            Self::Local(service) => Ok(serde_json::to_value(service.status(id).await?)?),
        }
    }

    async fn cancel(&self, id: &str) -> anyhow::Result<()> {
        match self {
            Self::Admin {
                client,
                base,
                token,
            } => {
                client
                    .post(format!("{base}/accounts/login/{id}/cancel"))
                    .bearer_auth(token.trim())
                    .send()
                    .await?
                    .error_for_status()?;
            }
            Self::Local(service) => {
                service.cancel(id).await?;
            }
        }
        Ok(())
    }
}

async fn login(
    home: &Path,
    config: &Config,
    provider: qb_core::accounts::Provider,
    replace_linked: bool,
    no_browser: bool,
) -> anyhow::Result<()> {
    let (backend, id, auth_url) = if let Some((client, base, token)) =
        running_admin(home, config).await?
    {
        let response = client
            .post(format!("{base}/accounts/login"))
            .bearer_auth(token.trim())
            .json(&serde_json::json!({ "provider": provider, "replace_linked": replace_linked }))
            .send()
            .await?;
        if let Err(error) = response.error_for_status_ref() {
            let payload: serde_json::Value = response.json().await.unwrap_or_default();
            println_utf8!("{}", login_failure(payload["error"]["message"].as_str()));
            return Err(error.into());
        }
        let start: serde_json::Value = response.json().await?;
        let id = start["login_id"]
            .as_str()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 100
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            })
            .ok_or_else(|| anyhow::anyhow!("로그인 응답을 확인해 주세요."))?
            .to_owned();
        let url = start["auth_url"].as_str().unwrap_or_default().to_owned();
        (
            LoginBackend::Admin {
                client,
                base,
                token,
            },
            id,
            url,
        )
    } else {
        let home = home.to_owned();
        let (store, obs) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let store = std::sync::Arc::new(AccountStore::acquire(&home)?);
            let obs = std::sync::Arc::new(qb_core::observability::Observability::new(&home, None)?);
            Ok((store, obs))
        })
        .await??;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("quota-baton/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let service = qb_core::oauth::LoginService::new(store, client, obs);
        let start = match service.start(provider, replace_linked).await {
            Ok(start) => start,
            Err(error) => {
                println_utf8!("{}", login_failure(Some(&error.to_string())));
                anyhow::bail!("로그인을 시작하지 못했습니다.");
            }
        };
        (LoginBackend::Local(service), start.login_id, start.auth_url)
    };

    // Only provider authorization URLs may be printed or opened. Never expose a
    // callback URL, arbitrary admin text, userinfo, or provider error payload.
    let result = async {
        let url = reqwest::Url::parse(&auth_url)?;
        let host = match provider {
            qb_core::accounts::Provider::Claude => "claude.ai",
            qb_core::accounts::Provider::Codex => "auth.openai.com",
        };
        anyhow::ensure!(
            url.scheme() == "https"
                && url.host_str() == Some(host)
                && url.path() == "/oauth/authorize"
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && !url.query_pairs().any(|(key, value)| (key == "code"
                    && !(provider == qb_core::accounts::Provider::Claude && value == "true"))
                    || matches!(
                        key.as_ref(),
                        "code_verifier"
                            | "client_secret"
                            | "access_token"
                            | "refresh_token"
                            | "id_token"
                            | "error"
                            | "error_description"
                    )),
            "로그인 주소를 확인해 주세요."
        );
        println_utf8!("로그인 주소: {}", url.as_str());
        if !no_browser && open_browser(url.as_str()).is_err() {
            println_utf8!("브라우저를 열지 못했습니다. 위 주소를 직접 열어 주세요.");
        }
        println_utf8!("로그인 대기 중입니다. 취소하려면 Ctrl+C를 누르세요.");
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        let deadline = tokio::time::sleep(Duration::from_secs(605));
        tokio::pin!(deadline);
        loop {
            let status = tokio::select! {
                signal = &mut interrupt => {
                    signal?;
                    backend.cancel(&id).await?;
                    println_utf8!("로그인을 취소했습니다.");
                    return Ok(());
                }
                _ = &mut deadline => {
                    println_utf8!("로그인 시간이 만료되었습니다. 다시 시작해 주세요.");
                    anyhow::bail!("로그인 시간이 만료되었습니다.");
                }
                status = backend.status(&id) => status?,
            };
            match status["status"].as_str() {
                Some("pending") => {}
                Some("completed") => {
                    println_utf8!("로그인을 완료했습니다.");
                    return Ok(());
                }
                Some("cancelled") => {
                    println_utf8!("로그인을 취소했습니다.");
                    return Ok(());
                }
                Some("expired") => {
                    println_utf8!("로그인 시간이 만료되었습니다. 다시 시작해 주세요.");
                    anyhow::bail!("로그인 시간이 만료되었습니다.");
                }
                Some("failed") => {
                    // Never print the returned error/code or any account fields.
                    println_utf8!("{}", login_failure(status["error"].as_str()));
                    anyhow::bail!("로그인에 실패했습니다.");
                }
                _ => anyhow::bail!("로그인 상태를 확인해 주세요."),
            }
            tokio::select! {
                signal = &mut interrupt => {
                    signal?;
                    backend.cancel(&id).await?;
                    println_utf8!("로그인을 취소했습니다.");
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }
    .await;
    if result.is_err() {
        let _ = backend.cancel(&id).await;
    }
    result
}

fn login_failure(error: Option<&str>) -> &'static str {
    use qb_core::oauth::LoginError;
    match error {
        Some(error)
            if error == "linked_conflict" || error == LoginError::LinkedConflict.to_string() =>
        {
            "CPA에 연결된 계정입니다. 독립된 토큰 충돌을 막기 위해 로그인을 거부했습니다. 연결을 해제하고 전환하려면 --replace-linked로 다시 시도해 주세요."
        }
        Some(error) if error == (LoginError::CallbackPortBusy { port: 1455 }).to_string() => {
            "Codex 로그인 콜백 포트 1455가 사용 중입니다. 다른 로그인 프로그램을 종료한 뒤 다시 시도해 주세요."
        }
        Some(error) if error == (LoginError::CallbackPortBusy { port: 54545 }).to_string() => {
            "Claude 로그인 콜백 포트 54545가 사용 중입니다. 다른 로그인 프로그램을 종료한 뒤 다시 시도해 주세요."
        }
        _ => "로그인에 실패했습니다. 로그인 요청과 계정 연결 상태를 확인한 뒤 다시 시도해 주세요.",
    }
}

#[cfg(windows)]
fn open_browser(url: &str) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            window: *mut std::ffi::c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> *mut std::ffi::c_void;
    }
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(Some(0))
        .collect();
    // ShellExecute receives the validated URL as data, without a command shell.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if result as isize <= 32 {
        return Err(std::io::Error::other("브라우저를 열지 못했습니다."));
    }
    Ok(())
}

#[cfg(not(windows))]
fn open_browser(url: &str) -> std::io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(program).arg(url).status()?;
    if !status.success() {
        return Err(std::io::Error::other("브라우저를 열지 못했습니다."));
    }
    Ok(())
}
