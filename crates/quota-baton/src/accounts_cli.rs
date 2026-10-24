use clap::{Subcommand, ValueEnum};
use qb_core::{accounts::AccountStore, config::Config};
use qb_proto::admin_login::{LoginStart, LoginState, LoginStatus};
use std::{path::Path, time::Duration};

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

/// Only static, reviewed local messages may cross the CLI error boundary.
#[derive(Debug)]
pub struct AccountCliError(pub &'static str);
impl std::fmt::Display for AccountCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for AccountCliError {}
pub(crate) fn account_error(message: &'static str) -> anyhow::Error {
    AccountCliError(message).into()
}

#[derive(Subcommand)]
pub enum AccountsCommand {
    #[command(flatten)]
    Platform(crate::platform::AccountsPlatformCommand),
    #[command(about = "브라우저로 계정에 로그인합니다")]
    #[command(display_order = 0)]
    Login {
        #[arg(value_enum)]
        provider: LoginProvider,
        #[command(flatten)]
        options: crate::platform::LoginOptions,
        #[arg(long, help = "브라우저 없이 코드 또는 리디렉션 주소로 로그인합니다")]
        no_browser: bool,
    },
    #[command(about = "계정 목록을 확인합니다")]
    #[command(display_order = 2)]
    List {
        #[arg(long, help = "사용량 예측의 속도·근거·모든 구간을 자세히 표시합니다")]
        detail: bool,
    },
    #[command(about = "계정 사용을 일시 중지합니다")]
    #[command(display_order = 3)]
    Pause { id: String },
    #[command(about = "계정 사용을 재개합니다")]
    #[command(display_order = 4)]
    Resume { id: String },
    #[command(about = "계정 사용량을 갱신합니다")]
    #[command(display_order = 7)]
    RefreshUsage { id: String },
    #[command(about = "사용량 한도를 해제하고 다음 요청으로 다시 시도합니다")]
    #[command(display_order = 8)]
    ReleaseHold { id: String },
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

pub async fn run(command: AccountsCommand, home: &Path, config: &Config) -> anyhow::Result<()> {
    match command {
        AccountsCommand::Platform(command) => {
            crate::platform::run_accounts(command, home, config).await?
        }
        AccountsCommand::Login {
            provider,
            options,
            no_browser,
        } => {
            login(
                home,
                config,
                provider.into(),
                options.replace_linked(),
                no_browser,
            )
            .await?;
        }
        AccountsCommand::ReleaseHold { id } => release_hold(home, config, &id).await?,
        AccountsCommand::RefreshUsage { id } => refresh_usage(home, config, &id).await?,
        AccountsCommand::Pause { id } => action(home, config, &id, "pause").await?,
        AccountsCommand::Resume { id } => action(home, config, &id, "resume").await?,
        AccountsCommand::List { detail } => {
            let accounts = AccountStore::list_metadata(home)?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            let state = live_account_state(home, config).await?;
            let english = config.ui.language == qb_core::config::UiLanguage::En;
            let now = chrono::Utc::now();
            for line in provider_list_lines(state.as_ref(), &accounts, now, english)? {
                println_utf8!("{line}");
            }
            let forecasts = state
                .as_ref()
                .map(|state| cli_account_rows(&state.accounts))
                .transpose()?;
            println_utf8!(
                "계정 ID\t제공자\t이메일\t요금제\t출처\t연결 방식\t사용 상태\t계정 상태\t만료 시각"
            );
            for value in accounts {
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
                let live = forecasts
                    .as_ref()
                    .and_then(|rows| rows.iter().find(|row| row["id"] == value["id"]));
                for line in list_forecast_lines(
                    live,
                    forecasts.is_some(),
                    detail,
                    config.ui.language == qb_core::config::UiLanguage::En,
                ) {
                    println_utf8!("  {line}");
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn translated(value: &serde_json::Value) -> String {
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
        Some("takeover") => "다른 앱에서 가져옴".into(),
        Some("created") => "생성".into(),
        Some("updated") => "갱신".into(),
        Some("skipped") => "건너뜀".into(),
        Some("unknown_type") => "지원하지 않는 계정 유형".into(),
        Some("invalid_data") => "잘못된 계정 데이터".into(),
        Some("owned_account") => "이미 소유한 계정".into(),
        _ => cell(value),
    }
}

pub(crate) fn health_label(value: &serde_json::Value) -> String {
    match value["kind"].as_str() {
        Some("ready") => "준비됨".into(),
        Some("cooling") => "대기 중".into(),
        Some("needs_relogin") => "재로그인 필요".into(),
        _ => "알 수 없음".into(),
    }
}

pub(crate) fn cell(value: &serde_json::Value) -> String {
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
pub(crate) async fn running_admin(
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
            let health: qb_proto::HealthResponse =
                admin_route_response(response, &client, &base, &token)
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
            anyhow::ensure!(health.ok, "코어 상태를 확인해 주세요.");
            Ok(Some((client, base, token)))
        }
        Err(error) if error.is_connect() => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// An inspect-only invocation is intentional and has its own CLI exit status.
#[derive(Debug)]
pub struct TakeoverInspectOnly;
impl std::fmt::Display for TakeoverInspectOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("확인만 했습니다. 가져오려면 --yes로 다시 실행해 주세요.")
    }
}
impl std::error::Error for TakeoverInspectOnly {}

#[derive(Debug)]
pub struct AdminVersionMismatch(String);
impl std::fmt::Display for AdminVersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CLI와 코어의 관리 API 버전이 맞지 않습니다 (CLI {}, 코어 {}). 같은 버전으로 업데이트해 주세요.",
            env!("CARGO_PKG_VERSION"),
            self.0
        )
    }
}
impl std::error::Error for AdminVersionMismatch {}

pub(crate) fn safe_version(value: &serde_json::Value) -> String {
    value
        .as_str()
        .filter(|version| {
            !version.is_empty()
                && version.len() <= 64
                && version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
        })
        .unwrap_or("알 수 없음")
        .to_owned()
}

pub(crate) fn missing_admin_route(
    status: reqwest::StatusCode,
    path: &str,
    value: &serde_json::Value,
) -> bool {
    // Resource absence is not evidence of an absent API route.
    let account_resource = path
        .strip_prefix("/admin/v1/accounts/")
        .is_some_and(|tail| tail.contains('/') || !matches!(tail, "takeover" | "login" | "import"));
    let resource_missing = value["error"]["code"] == "not_found"
        || value["error"]["message"] == "Account not found"
        || value["error"]["message"].as_str()
            == Some(qb_core::oauth::LoginError::NotFound.to_string().as_str());
    status == reqwest::StatusCode::METHOD_NOT_ALLOWED
        || (status == reqwest::StatusCode::NOT_FOUND && !(account_resource && resource_missing))
}

pub(crate) async fn admin_route_response(
    response: reqwest::Response,
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> anyhow::Result<reqwest::Response> {
    let status = response.status();
    if !matches!(
        status,
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
    ) {
        return Ok(response);
    }
    let path = response.url().path().to_owned();
    let value = response
        .json::<serde_json::Value>()
        .await
        .unwrap_or_default();
    if !missing_admin_route(status, &path, &value) {
        return Err(account_error("계정을 찾을 수 없습니다."));
    }
    // Version lookup is best effort, authenticated, and never renders raw server text.
    let version = match client
        .get(format!("{base}/health"))
        .bearer_auth(token.trim())
        .send()
        .await
    {
        Ok(health) if health.status().is_success() => {
            let health = health.json::<serde_json::Value>().await.unwrap_or_default();
            safe_version(&health["version"])
        }
        _ => "알 수 없음".into(),
    };
    Err(AdminVersionMismatch(version).into())
}

// Original live bytes -> phase-specific shared DTO -> existing display reader.

pub(crate) fn valid_account_id(id: &str) -> anyhow::Result<()> {
    if qb_core::observability::safe_identifier(id).is_none() {
        return Err(account_error("계정 ID를 확인해 주세요."));
    }
    Ok(())
}

// Select the response contract at the request call site, never by trying
// permissive DTO alternatives against a normalized Value.
#[derive(Clone, Copy)]
pub(crate) enum AccountSubset {
    #[allow(dead_code)] // Used by the Windows command extension and contract tests.
    Import,
    Action,
    Released,
}

pub(crate) fn decode_account_subset(
    bytes: &[u8],
    expected: AccountSubset,
) -> anyhow::Result<serde_json::Value> {
    use qb_proto::admin_account::{AdminHealth, AdminProvider, AdminUserState};
    type Imported = qb_proto::admin_import::ImportRow<
        AdminProvider,
        AdminUserState,
        chrono::DateTime<chrono::Utc>,
    >;
    type Action =
        qb_proto::admin_action::AccountActionReply<AdminProvider, AdminUserState, AdminHealth>;
    type Released =
        qb_proto::admin_action::ReleaseHoldReply<AdminProvider, AdminUserState, AdminHealth>;
    let invalid = || account_error("계정 응답 형식이 올바르지 않습니다.");
    // Reuse the existing map-only raw adapter. Import has an array root, but
    // each row must still be an object. Its diagnostic text is never rendered.
    match expected {
        AccountSubset::Import => {
            let _: Vec<LoginObject<Imported>> =
                serde_json::from_slice(bytes).map_err(|_| invalid())?;
        }
        AccountSubset::Action => {
            let _: LoginObject<Action> = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        }
        AccountSubset::Released => {
            let _: LoginObject<Released> = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        }
    }
    // Present the original payload, preserving its key order and timestamp
    // spelling rather than reserializing the validation DTO.
    serde_json::from_slice(bytes).map_err(|_| invalid())
}

pub(crate) async fn release_hold_response(
    response: reqwest::Response,
) -> anyhow::Result<serde_json::Value> {
    if !response.status().is_success() {
        // Keep the existing allowlisted status/code classifier unchanged.
        return account_response(response).await;
    }
    let bytes = response.bytes().await?;
    decode_account_subset(&bytes, AccountSubset::Released)
}

pub(crate) fn account_error_message(bytes: &[u8]) -> &'static str {
    let signal =
        serde_json::from_slice::<qb_proto::admin_account_error::AccountErrorSignal>(bytes).ok();
    let code = signal.as_ref().and_then(|signal| signal.code.as_deref());
    match code {
        Some("not_found") => "계정을 찾을 수 없습니다.",
        Some("linked") => "CPA가 갱신하는 계정입니다.",
        Some("not_linked") => "CPA 연결 계정만 소유권을 이전할 수 있습니다.",
        Some("drain") => "드레인 중인 계정은 갱신할 수 없습니다.",
        Some("needs_relogin") => "다시 로그인해야 하는 계정입니다.",
        Some("uncertain") => {
            "이전 갱신 결과가 불확실합니다. 확인 후 --force로 한 번 시도해 주세요."
        }
        Some("generation_conflict") => "계정이 변경됐습니다. 상태를 다시 읽고 시도해 주세요.",
        Some("identity_conflict") => "계정의 사용자를 확인할 수 없거나 사용자 파일이 겹칩니다.",
        Some("cpa_running") => "CPA가 아직 이 계정을 갱신할 수 있습니다.",
        Some("cpa_unknown") => "CPA 실행 상태를 확인할 수 없습니다.",
        _ => "계정 작업을 마치지 못했습니다. 코어 상태를 확인해 주세요.",
    }
}

pub(crate) async fn account_response(
    response: reqwest::Response,
) -> anyhow::Result<serde_json::Value> {
    let status = response.status();
    if !status.is_success() {
        let bytes = response.bytes().await.unwrap_or_default();
        return Err(account_error(account_error_message(&bytes)));
    }
    // Preserve the existing success reader; its typed migrations are separate.
    let value: serde_json::Value = response.json().await.unwrap_or_default();
    Ok(value)
}

// Deserialize original successful bytes before any Value normalization.
// Error statuses retain the existing allowlisted account error behavior.

pub(crate) async fn action(
    home: &Path,
    config: &Config,
    id: &str,
    action: &str,
) -> anyhow::Result<()> {
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
        let bytes = admin_route_response(
            client
                .post(format!("{base}/accounts/{id}/{action}"))
                .bearer_auth(token.trim())
                .send()
                .await?,
            &client,
            &base,
            &token,
        )
        .await?
        .error_for_status()?
        .bytes()
        .await?;
        decode_account_subset(&bytes, AccountSubset::Action)?;
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

pub(crate) async fn release_hold(home: &Path, config: &Config, id: &str) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};
    valid_account_id(id)?;
    if !std::io::stdin().is_terminal() {
        return Err(account_error(
            "대화형 터미널에서 계정 ID를 입력해 확인해 주세요.",
        ));
    }
    println_utf8!(
        "계정 {}의 사용량 한도를 해제합니다. 다음 요청은 상위 서비스에 다시 시도합니다. 계정 ID를 입력하세요:",
        id
    );
    std::io::stdout().flush()?;
    let mut confirm = String::new();
    std::io::stdin().read_line(&mut confirm)?;
    if confirm.trim() != id {
        return Err(account_error("확인이 일치하지 않아 취소했습니다."));
    }
    let (client, base, token) = running_admin(home, config)
        .await?
        .ok_or_else(|| account_error("한도를 해제하려면 코어를 먼저 실행해 주세요."))?;
    release_hold_response(
        admin_route_response(
            client
                .post(format!("{base}/accounts/{id}/release-hold"))
                .bearer_auth(token.trim())
                .json(&serde_json::json!({"confirm":id}))
                .send()
                .await?,
            &client,
            &base,
            &token,
        )
        .await?,
    )
    .await?;
    println_utf8!(
        "계정 {}의 한도를 해제했습니다. 다음 요청으로 다시 시도합니다.",
        id
    );
    Ok(())
}

pub(crate) async fn refresh_usage(home: &Path, config: &Config, id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        qb_core::observability::safe_identifier(id).is_some(),
        "계정 ID를 확인해 주세요."
    );
    let (client, base, token) = running_admin(home, config)
        .await?
        .ok_or(UsageCliError::CoreRequired)?;
    let response = admin_route_response(
        client
            .post(format!("{base}/accounts/{id}/refresh-usage"))
            .timeout(Duration::from_secs(35))
            .bearer_auth(token.trim())
            .send()
            .await?,
        &client,
        &base,
        &token,
    )
    .await?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(UsageCliError::TooSoon.into());
    }
    let body = response.error_for_status()?.bytes().await?;
    if body.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(account_error("계정 응답 형식이 올바르지 않습니다."));
    }
    let _: qb_proto::AdminAccount = serde_json::from_slice(&body)
        .map_err(|_| account_error("계정 응답 형식이 올바르지 않습니다."))?;
    println_utf8!("계정 {}의 사용량을 갱신했습니다.", id);
    Ok(())
}

pub(crate) fn status_heading(home: &Path, running: bool) -> anyhow::Result<Vec<String>> {
    let mut lines = vec![format!(
        "코어: {}",
        if running {
            "실행 중"
        } else {
            "중지됨 (저장된 상태)"
        }
    )];
    lines.extend(crate::platform::cpa_dependency_note(home)?);
    Ok(lines)
}

pub async fn status(home: &Path, config: &Config) -> anyhow::Result<()> {
    let (running, accounts) = if let Some(accounts) = live_account_views(home, config).await? {
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
    for line in status_heading(home, running)? {
        println_utf8!("{line}");
    }
    let pools = qb_core::account_runtime::pool_counts(&accounts);
    for provider in ["claude", "codex"] {
        let pool = &pools[provider];
        let cooling = pool["cooling"]
            .as_object()
            .map(|reasons| {
                reasons
                    .iter()
                    .map(|(reason, count)| {
                        format!(
                            "{} {count}",
                            translated(&serde_json::Value::String(reason.clone()))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        println_utf8!(
            "{provider}: 사용 가능 {} · 대기 [{}] · 사용 불가 {}",
            pool["eligible"],
            cooling,
            pool["unavailable"]
        );
    }
    let names = status_window_names(&accounts);
    let columns = names
        .iter()
        .map(|name| {
            format!(
                "{} 남음\t{} 초기화",
                status_window_label(name),
                status_window_label(name)
            )
        })
        .collect::<Vec<_>>()
        .join("\t");
    println_utf8!(
        "계정 ID\t제공자\t라벨\t사용 상태\t계정 상태{}\t상태 이유\t대기 종료",
        if columns.is_empty() {
            String::new()
        } else {
            format!("\t{columns}")
        }
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
        let windows = names
            .iter()
            .map(|name| {
                let value = window(name);
                format!(
                    "\t{}\t{}",
                    percentage(&value, now),
                    cell(&value["reset_at"])
                )
            })
            .collect::<String>();
        println_utf8!(
            "{}\t{}\t{}\t{}\t{}{}\t{}\t{}",
            cell(&account["id"]),
            cell(&account["provider"]),
            cell(&account["label"]),
            translated(&account["user_state"]),
            if account["probe_pending"].as_bool() == Some(true) {
                "인증 확인 대기".into()
            } else {
                health_label(&account["health"])
            },
            windows,
            translated(&account["health"]["reason"]),
            cell(&account["health"]["until"])
        );
        for line in forecast_lines(Some(&account), running) {
            println_utf8!("  {line}");
        }
    }
    Ok(())
}

pub(crate) type CliAccountState<Account = qb_proto::AdminAccount> = qb_proto::AdminState<
    Account,
    qb_proto::admin_warning::AdminWarning,
    Option<qb_proto::admin_pool::ReadPools>,
>;

pub(crate) async fn live_account_views(
    home: &Path,
    config: &Config,
) -> anyhow::Result<Option<Vec<serde_json::Value>>> {
    live_account_state(home, config)
        .await?
        .map(|state| cli_account_rows(&state.accounts))
        .transpose()
        .map_err(Into::into)
}

/// Presentation boundary only: live decoding has already checked the complete
/// shared account. Preserve existing readers, object source and forecast facts.
pub(crate) fn cli_account_rows<Account: serde::Serialize>(
    accounts: &[Account],
) -> serde_json::Result<Vec<serde_json::Value>> {
    accounts.iter().map(serde_json::to_value).collect()
}

pub(crate) fn provider_list_name(
    a: &crate::account_display::Account,
    rows: &[&crate::account_display::Account],
    english: bool,
) -> String {
    let nonempty = |s: &str| !s.trim().is_empty();
    let name = if let Some(label) = a.label.as_deref().filter(|s| nonempty(s)).filter(|label| {
        !a.email
            .as_deref()
            .is_some_and(|email| *label == email || *label == format!("{email}'s Organization"))
    }) {
        label
    } else if let Some(email) = a.email.as_deref().filter(|s| nonempty(s)) {
        let local = email.split('@').next().unwrap_or(email);
        if rows
            .iter()
            .filter(|b| {
                b.email
                    .as_deref()
                    .filter(|s| nonempty(s))
                    .is_some_and(|s| s.split('@').next() == Some(local))
            })
            .count()
            > 1
        {
            email
        } else {
            local
        }
    } else if english {
        "Unnamed account"
    } else {
        "이름 없는 계정"
    };
    name.chars().filter(|c| !c.is_control()).collect()
}

pub(crate) fn provider_list_lines<Account: serde::Serialize>(
    state: Option<&CliAccountState<Account>>,
    metadata: &[serde_json::Value],
    now: chrono::DateTime<chrono::Utc>,
    english: bool,
) -> serde_json::Result<Vec<String>> {
    use qb_core::{
        accounts::{Provider, UserState},
        provider_forecast::{self, ProviderAccount},
    };
    let live = state
        .map(|state| cli_account_rows(&state.accounts))
        .transpose()?;
    let source = live.as_deref().unwrap_or(metadata);
    let accounts = source
        .iter()
        .cloned()
        .map(serde_json::from_value::<crate::account_display::Account>)
        .collect::<Result<Vec<_>, _>>()?;
    let mut lines = Vec::new();
    for provider in [Provider::Claude, Provider::Codex] {
        let rows: Vec<_> = accounts
            .iter()
            .filter(|a| a.provider == provider.as_str())
            .collect();
        let names: Vec<_> = rows
            .iter()
            .map(|a| provider_list_name(a, &rows, english))
            .collect();
        // The tray owns health/scope visibility; a hidden forecast stays cold.
        let forecasts: Vec<_> = rows
            .iter()
            .map(|a| state.map(|_| crate::account_display::summary_forecast(a, now)))
            .collect();
        let inputs: Vec<_> = rows
            .iter()
            .zip(&names)
            .zip(&forecasts)
            .map(|((a, name), forecast)| ProviderAccount {
                id: &a.id,
                name,
                user_state: match a.user_state.as_str() {
                    "active" => UserState::Active,
                    "paused" => UserState::Paused,
                    _ => UserState::Disabled,
                },
                drain: a.drain,
                usable: state.is_some()
                    && a.pool_eligible == Some(true)
                    && a.health.kind == "ready"
                    && !a.probe_pending
                    && !a.refresh_attention,
                cooling_until: state
                    .and_then(|_| {
                        if a.health.kind == "cooling"
                            && a.health.scope.as_deref() == Some("account")
                        {
                            a.health.until.as_deref()
                        } else {
                            None
                        }
                    })
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|at| at.to_utc()),
                allow_credits: a.allow_credits,
                credits_in_use: state.is_some()
                    && a.provider == "codex"
                    && a.credits_in_use == Some(true),
                credits_unlimited: a.credits.as_ref().and_then(|c| c.unlimited),
                forecast: forecast.as_ref(),
            })
            .collect();
        let eligible = state.and_then(|state| {
            state
                .pools
                .as_ref()
                .and_then(|pools| pools.get(provider.as_str()))
                .and_then(Option::as_ref)
                .and_then(|pool| pool.eligible)
        });
        let summary = provider_forecast::summarize(provider, &inputs, eligible, now);
        lines.push(provider_list_line(&summary, state.is_some(), english));
    }
    Ok(lines)
}

pub(crate) fn provider_list_line(
    summary: &qb_core::provider_forecast::ProviderSummary,
    online: bool,
    english: bool,
) -> String {
    use qb_core::provider_forecast::{CreditRunway, SubscriptionOutlook};
    use qb_core::usage_forecast_display::{forecast_clock, time_left};
    let provider = match summary.provider {
        qb_core::accounts::Provider::Claude => "Claude",
        qb_core::accounts::Provider::Codex => "Codex",
    };
    let count = match (english, summary.usable) {
        (true, Some(n)) => format!("{n} of {} usable", summary.configured),
        (false, Some(n)) => format!("{}개 중 {n}개 사용 가능", summary.configured),
        (true, None) => format!("Usable unknown / {} accounts", summary.configured),
        (false, None) => format!("전체 {}개 · 사용 가능 확인 전", summary.configured),
    };
    let mut result = format!("{provider} · {count}");
    let clock = |at| forecast_clock(summary.as_of, at, english);
    match &summary.subscription {
        SubscriptionOutlook::Next(next) => {
            let left = time_left(summary.as_of, next.at, english)
                .expect("core selected future exhaustion");
            result.push_str(&if english {
                format!(
                    " · next out: {} · {left} ({})",
                    next.account.name,
                    clock(next.at)
                )
            } else {
                format!(
                    " · 다음 소진 {} · {left} ({})",
                    next.account.name,
                    clock(next.at)
                )
            });
        }
        SubscriptionOutlook::NoExhaustionBeforeReset => result.push_str(if english {
            " · no exhaustion before reset"
        } else {
            " · 초기화 전 소진 없음"
        }),
        SubscriptionOutlook::Unknown => result.push_str(if english {
            " · next exhaustion: Measuring"
        } else {
            " · 다음 소진: 측정 중"
        }),
    }
    if let Some(at) = summary.earliest_recovery {
        result.push_str(&if english {
            format!(" · earliest recovery {}", clock(at))
        } else {
            format!(" · 가장 빠른 회복 {}", clock(at))
        });
    }
    for credit in &summary.credit_continuations {
        let runway = match &credit.runway {
            CreditRunway::Unknown => {
                if english {
                    "runway unknown".into()
                } else {
                    "잔여시간 확인 전".into()
                }
            }
            CreditRunway::Unlimited => {
                if english {
                    "unlimited".into()
                } else {
                    "무제한".into()
                }
            }
            CreditRunway::Forecast(forecast) => {
                if let Some(at) = forecast.exhaustion_at.filter(|at| *at > summary.as_of) {
                    let left =
                        time_left(summary.as_of, at, english).expect("positive credit interval");
                    format!("{left} ({})", clock(at))
                } else if forecast.outcome == qb_core::usage_forecast::ForecastOutcome::Exhausted {
                    if english {
                        "exhausted".into()
                    } else {
                        "소진".into()
                    }
                } else if english {
                    "no finite estimate".into()
                } else {
                    "유한 예측 없음".into()
                }
            }
        };
        result.push_str(&if english {
            format!(" · credits continue on {} ({runway})", credit.account.name)
        } else {
            format!(" · {} 크레딧으로 계속 사용 ({runway})", credit.account.name)
        });
    }
    let coverage = summary.coverage;
    result.push_str(&if english {
        format!(
            " · forecast measured {}/{} accounts",
            coverage.measured, coverage.total
        )
    } else {
        format!(" · 예측 측정 {}/{} 계정", coverage.measured, coverage.total)
    });
    if !online {
        result.push_str(if english {
            " · core offline"
        } else {
            " · 코어 중지됨"
        });
    }
    // Pooled runway remains unavailable; no pooled number is rendered here.
    result
}

pub(crate) async fn live_account_state(
    home: &Path,
    config: &Config,
) -> anyhow::Result<Option<CliAccountState>> {
    let Some((client, base, token)) = running_admin(home, config).await? else {
        return Ok(None);
    };
    let body = admin_route_response(
        client
            .get(format!("{base}/state"))
            .bearer_auth(token.trim())
            .send()
            .await?,
        &client,
        &base,
        &token,
    )
    .await?
    .error_for_status()?
    .bytes()
    .await?;
    Ok(Some(decode_cli_account_state(&body)?))
}

// Decode raw response bytes: Value-first parsing would discard duplicate keys.
pub(crate) fn decode_cli_account_state(body: &[u8]) -> serde_json::Result<CliAccountState> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let state = qb_proto::admin_state::deserialize_compat(&mut deserializer, true)?;
    deserializer.end()?;
    Ok(state)
}

pub(crate) fn list_forecast_lines(
    account: Option<&serde_json::Value>,
    running: bool,
    detail: bool,
    english: bool,
) -> Vec<String> {
    if detail {
        return forecast_lines(account, running);
    }
    let forecast = running.then_some(account).flatten().and_then(|account| {
        serde_json::from_value::<qb_core::usage_forecast::AccountUsageForecast>(
            account["usage_forecast"].clone(),
        )
        .ok()
    });
    vec![
        forecast
            .as_ref()
            .map(|forecast| qb_core::usage_forecast_display::summary(forecast, english).text)
            .unwrap_or_else(|| {
                if english {
                    "Measuring".into()
                } else {
                    "측정 중".into()
                }
            }),
    ]
}

pub(crate) fn forecast_lines(account: Option<&serde_json::Value>, running: bool) -> Vec<String> {
    if !running {
        return vec!["소진 예측: 코어가 꺼져 있어 측정할 수 없음".into()];
    }
    let Some(account) = account else {
        return vec!["소진 예측: 측정 중".into()];
    };
    match serde_json::from_value::<qb_core::usage_forecast::AccountUsageForecast>(
        account["usage_forecast"].clone(),
    ) {
        Ok(forecast) => qb_core::usage_forecast_display::lines(
            &forecast,
            account["credits_in_use"].as_bool() == Some(true),
        ),
        Err(_) => vec!["소진 예측: 측정 중".into()],
    }
}

pub(crate) fn status_window_label(name: &str) -> String {
    match name {
        "5h" => "5시간".into(),
        "7d" => "7일".into(),
        "43200m" => "30일".into(),
        _ => cell(&serde_json::json!(name)),
    }
}

pub(crate) fn status_window_names(accounts: &[serde_json::Value]) -> Vec<String> {
    let mut names = Vec::new();
    for account in accounts {
        for window in account["windows"].as_array().into_iter().flatten() {
            if let Some(name) = window["name"].as_str()
                && !names.iter().any(|existing| existing == name)
            {
                names.push(name.to_owned());
            }
        }
    }
    names
}

pub(crate) fn percentage(window: &serde_json::Value, now: std::time::SystemTime) -> String {
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
        .map(|value| {
            let remaining = format!("{:.1}", (100.0 - value).clamp(0.0, 100.0));
            format!("{}%", remaining.trim_end_matches(".0"))
        })
        .unwrap_or_else(|| "-".into())
}

pub(crate) enum LoginBackend {
    Admin {
        client: reqwest::Client,
        base: String,
        token: String,
    },
    Local(qb_core::oauth::LoginService),
}

// These CLI responses have always required named object fields. Delegate the
// original map directly, retaining the shared DTO's duplicate-key checks.
pub(crate) struct LoginObject<T>(T);
#[cfg(test)]
mod r15_login_object_tests {
    use super::*;
    #[test]
    fn cli_login_requires_named_objects_and_retains_duplicate_checks() {
        for raw in [
            r#"["synthetic-login","https://example.test/login"]"#,
            r#"{"login_id":"a","login_id":"b","auth_url":"https://example.test/login"}"#,
        ] {
            assert!(serde_json::from_str::<LoginObject<LoginStart>>(raw).is_err());
        }
        for raw in [
            r#"["completed",null,null]"#,
            r#"{"status":"pending","status":"completed"}"#,
        ] {
            assert!(serde_json::from_str::<LoginObject<LoginStatus>>(raw).is_err());
        }
        let start: LoginObject<LoginStart> = serde_json::from_str(
            r#"{"login_id":"synthetic-login","auth_url":"https://example.test/login"}"#,
        )
        .unwrap();
        assert_eq!(start.0.login_id, "synthetic-login");
        let status: LoginObject<LoginStatus> =
            serde_json::from_str(r#"{"status":"completed"}"#).unwrap();
        assert_eq!(status.0.status, LoginState::Completed);
    }
}
impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for LoginObject<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: serde::Deserialize<'de>> serde::de::Visitor<'de> for ObjectVisitor<T> {
            type Value = LoginObject<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a login response object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                map: M,
            ) -> Result<Self::Value, M::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(LoginObject)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData))
    }
}

impl LoginBackend {
    async fn start(
        &self,
        provider: qb_core::accounts::Provider,
        replace_linked: bool,
        no_browser: bool,
        paste_redirect: bool,
    ) -> Result<LoginStart, LoginStartFailure> {
        match self {
            Self::Admin {
                client,
                base,
                token,
            } => {
                let mut body = serde_json::json!({
                    "provider": provider, "replace_linked": replace_linked, "no_browser": no_browser,
                });
                if paste_redirect {
                    body["paste_redirect"] = true.into();
                }
                let response = client
                    .post(format!("{base}/accounts/login"))
                    .bearer_auth(token.trim())
                    .json(&body)
                    .send()
                    .await
                    .map_err(|_| login_start_failure(None))?;
                let response = admin_route_response(response, client, base, token)
                    .await
                    .map_err(|_| login_start_failure(None))?;
                if !response.status().is_success() {
                    let payload: serde_json::Value = response.json().await.unwrap_or_default();
                    return Err(login_start_failure(payload["error"]["message"].as_str()));
                }
                response
                    .json::<LoginObject<LoginStart>>()
                    .await
                    .map(|value| value.0)
                    .map_err(|_| login_start_failure(None))
            }
            Self::Local(service) => {
                let result = if paste_redirect {
                    service.start_paste_redirect(replace_linked).await
                } else if no_browser {
                    service.start_manual(provider, replace_linked).await
                } else {
                    service.start(provider, replace_linked).await
                };
                result.map_err(|error| login_start_failure(Some(&error.to_string())))
            }
        }
    }

    async fn submit_manual(&self, id: &str, input: &str) -> anyhow::Result<()> {
        match self {
            Self::Admin {
                client,
                base,
                token,
            } => {
                // This body contains an authorization code: never log it or the
                // returned response body. The server returns only fixed errors.
                admin_route_response(
                    client
                        .post(format!("{base}/accounts/login/{id}/submit"))
                        .bearer_auth(token.trim())
                        .json(&serde_json::json!({ "input": input }))
                        .send()
                        .await?,
                    client,
                    base,
                    token,
                )
                .await?
                .error_for_status()?;
            }
            Self::Local(service) => service.submit_manual(id, input).await?,
        }
        Ok(())
    }

    async fn status(&self, id: &str) -> anyhow::Result<LoginStatus> {
        match self {
            Self::Admin {
                client,
                base,
                token,
            } => Ok(admin_route_response(
                client
                    .get(format!("{base}/accounts/login/{id}"))
                    .bearer_auth(token.trim())
                    .send()
                    .await?,
                client,
                base,
                token,
            )
            .await?
            .error_for_status()?
            .json::<LoginObject<LoginStatus>>()
            .await
            .map_err(|_| anyhow::anyhow!("로그인 상태를 확인해 주세요."))?
            .0),
            Self::Local(service) => Ok(service.status(id).await?),
        }
    }

    async fn cancel(&self, id: &str) -> anyhow::Result<()> {
        match self {
            Self::Admin {
                client,
                base,
                token,
            } => {
                admin_route_response(
                    client
                        .post(format!("{base}/accounts/login/{id}/cancel"))
                        .bearer_auth(token.trim())
                        .send()
                        .await?,
                    client,
                    base,
                    token,
                )
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

struct LoginStartFailure {
    message: &'static str,
    offer_paste: bool,
}

fn login_start_failure(error: Option<&str>) -> LoginStartFailure {
    use qb_core::oauth::LoginError;
    LoginStartFailure {
        message: login_failure(error),
        offer_paste: error.is_some_and(|error| {
            error == LoginError::DeviceUnavailable.to_string()
                || error == LoginError::DeviceRateLimited.to_string()
        }),
    }
}

pub(crate) async fn login(
    home: &Path,
    config: &Config,
    provider: qb_core::accounts::Provider,
    replace_linked: bool,
    no_browser: bool,
) -> anyhow::Result<()> {
    let mut offline_obs = None;
    let backend = if let Some((client, base, token)) = running_admin(home, config).await? {
        LoginBackend::Admin {
            client,
            base,
            token,
        }
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
        let service = qb_core::oauth::LoginService::new(store, client, obs.clone());
        offline_obs = Some(obs);
        LoginBackend::Local(service)
    };
    let mut active_id = None;
    let result = login_interaction(
        &backend,
        provider,
        replace_linked,
        no_browser,
        &mut active_id,
    )
    .await;
    // Also covers interrupted starts of the explicit fallback and invalid UI
    // metadata. Never let the stdin owner retain this backend or its lease.
    let cancelled = if let Some(id) = active_id {
        backend
            .cancel(&id)
            .await
            .map_err(|_| anyhow::anyhow!("로그인 취소 상태를 확인해 주세요."))
    } else {
        Ok(())
    };
    if let (LoginBackend::Local(service), Some(obs)) = (backend, offline_obs) {
        finish_offline_login(service, obs).await?;
    }
    result.and(cancelled)
}

async fn login_interaction(
    backend: &LoginBackend,
    provider: qb_core::accounts::Provider,
    replace_linked: bool,
    no_browser: bool,
    active_id: &mut Option<String>,
) -> anyhow::Result<()> {
    use qb_core::accounts::Provider;
    let device_requested = no_browser && provider == Provider::Codex;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let deadline = tokio::time::sleep(Duration::from_secs(if device_requested {
        900
    } else {
        605
    }));
    tokio::pin!(deadline);
    let mut start_next = Some(false);
    let mut device = false;
    let mut offer_paste = false;
    let mut input = None;
    let mut poll_at = tokio::time::Instant::now();
    loop {
        if let Some(paste) = start_next.take() {
            let started = tokio::select! {
                biased;
                signal = &mut interrupt => {
                    signal?;
                    println_utf8!("로그인을 취소했습니다.");
                    return Ok(());
                }
                _ = &mut deadline => anyhow::bail!("로그인 시간이 만료되었습니다."),
                started = backend.start(provider, replace_linked, no_browser, paste) => started,
            };
            match started {
                Ok(start) => {
                    anyhow::ensure!(
                        !start.login_id.is_empty()
                            && start.login_id.len() <= 100
                            && start
                                .login_id
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
                        "로그인 응답을 확인해 주세요."
                    );
                    *active_id = Some(start.login_id.clone());
                    device = start.device.is_some();
                    anyhow::ensure!(
                        !device || device_requested && !paste,
                        "로그인 응답을 확인해 주세요."
                    );
                    let lifetime = display_login_start(&start, provider, no_browser)?;
                    if device {
                        let remaining = tokio::time::Instant::now() + Duration::from_secs(lifetime);
                        let bounded = remaining.min(deadline.deadline());
                        deadline.as_mut().reset(bounded);
                    } else {
                        let manual = tokio::time::Instant::now() + Duration::from_secs(605);
                        let bounded = manual.min(deadline.deadline());
                        deadline.as_mut().reset(bounded);
                    }
                    offer_paste = false;
                    poll_at = tokio::time::Instant::now();
                }
                Err(error) => {
                    println_utf8!("{}", error.message);
                    if !device_requested || paste || !error.offer_paste {
                        anyhow::bail!("로그인을 시작하지 못했습니다.");
                    }
                    offer_paste = true;
                    println_utf8!("붙여넣기 로그인으로 전환하려면 Enter를 누르세요. 취소: Ctrl+C");
                }
            }
            if no_browser && input.is_none() {
                input = Some(manual_login_input(device || offer_paste));
            }
        }

        let status = tokio::select! {
            biased;
            signal = &mut interrupt => {
                signal?;
                println_utf8!("로그인을 취소했습니다.");
                return Ok(());
            }
            _ = &mut deadline => anyhow::bail!("로그인 시간이 만료되었습니다. 다시 시작해 주세요."),
            line = async {
                match &mut input {
                    Some(input) => input.await,
                    None => std::future::pending().await,
                }
            } => {
                input = None;
                let line = line.map_err(|_| anyhow::anyhow!("로그인 입력을 읽지 못했습니다."))??;
                if device || offer_paste {
                    anyhow::ensure!(line.is_empty(), "붙여넣기 로그인으로 전환하려면 먼저 Enter를 누르세요.");
                    if let Some(id) = active_id.as_deref() {
                        let terminal = tokio::select! {
                            biased;
                            signal = &mut interrupt => {
                                signal?;
                                println_utf8!("로그인을 취소했습니다.");
                                return Ok(());
                            }
                            _ = &mut deadline => anyhow::bail!("로그인 시간이 만료되었습니다."),
                            terminal = async {
                                backend.cancel(id).await?;
                                backend.status(id).await
                            } => terminal.map_err(|_| anyhow::anyhow!("로그인 취소 상태를 확인해 주세요."))?,
                        };
                        match terminal.status {
                            LoginState::Completed => {
                                *active_id = None;
                                println_utf8!("로그인을 완료했습니다.");
                                return Ok(());
                            }
                            LoginState::Cancelled | LoginState::Expired => *active_id = None,
                            _ => anyhow::bail!("기존 로그인 종료를 확인하지 못했습니다. 다시 시작해 주세요."),
                        }
                    }
                    device = false;
                    offer_paste = false;
                    start_next = Some(true); // Exactly one user-requested paste start.
                    deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(605));
                    continue;
                }
                anyhow::ensure!(!line.is_empty(), "로그인 입력을 확인해 주세요.");
                let id = active_id.as_deref().ok_or_else(|| anyhow::anyhow!("로그인 요청을 확인해 주세요."))?;
                tokio::select! {
                    biased;
                    signal = &mut interrupt => {
                        signal?;
                        println_utf8!("로그인을 취소했습니다.");
                        return Ok(());
                    }
                    _ = &mut deadline => anyhow::bail!("로그인 시간이 만료되었습니다."),
                    submitted = backend.submit_manual(id, &line) => {
                        submitted.map_err(|_| anyhow::anyhow!("로그인 코드 또는 리디렉션 주소를 확인하고 다시 시작해 주세요."))?;
                    }
                }
                poll_at = tokio::time::Instant::now();
                continue;
            }
            status = async {
                tokio::time::sleep_until(poll_at).await;
                backend.status(active_id.as_deref().expect("active login")).await
            }, if !offer_paste => status.map_err(|_| anyhow::anyhow!("로그인 상태를 확인해 주세요."))?,
        };
        match status.status {
            LoginState::Pending => poll_at = tokio::time::Instant::now() + Duration::from_secs(1),
            LoginState::Completed => {
                *active_id = None;
                println_utf8!("로그인을 완료했습니다.");
                return Ok(());
            }
            LoginState::Cancelled => {
                *active_id = None;
                println_utf8!("로그인을 취소했습니다.");
                return Ok(());
            }
            LoginState::Expired => {
                anyhow::bail!("로그인 시간이 만료되었습니다. 다시 시작해 주세요.")
            }
            LoginState::Failed => {
                println_utf8!("{}", login_failure(status.error.as_deref()));
                anyhow::bail!("로그인에 실패했습니다.");
            }
        }
    }
}

fn display_login_start(
    start: &LoginStart,
    provider: qb_core::accounts::Provider,
    no_browser: bool,
) -> anyhow::Result<u64> {
    if let Some(device) = &start.device {
        anyhow::ensure!(
            start.auth_url == "https://auth.openai.com/codex/device"
                && device.verification_url == start.auth_url
                && (1..=900).contains(&device.expires_in)
                && (1..=64).contains(&device.user_code.len())
                && device
                    .user_code
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "로그인 응답을 확인해 주세요."
        );
        println_utf8!("로그인 주소: {}", device.verification_url);
        println_utf8!("기기 코드: {}", device.user_code);
        println_utf8!(
            "{}초 안에 인증해 주세요. 새 코드를 자동 요청하지 않습니다.",
            device.expires_in
        );
        println_utf8!(
            "개인 계정은 ChatGPT Security 설정에서 기기 코드 로그인을 켜거나, workspace admin에게 허용을 요청하세요."
        );
        println_utf8!("붙여넣기 로그인으로 전환하려면 Enter를 누르세요. 취소: Ctrl+C");
        return Ok(device.expires_in);
    }
    let url = reqwest::Url::parse(&start.auth_url)
        .map_err(|_| anyhow::anyhow!("로그인 주소를 확인해 주세요."))?;
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
    if !no_browser && crate::platform::open_browser(url.as_str()).is_err() {
        println_utf8!("브라우저를 열지 못했습니다. 위 주소를 직접 열어 주세요.");
    }
    println_utf8!("로그인 대기 중입니다. 취소하려면 Ctrl+C를 누르세요.");
    if no_browser {
        println_utf8!(
            "{}",
            match provider {
                qb_core::accounts::Provider::Claude =>
                    "인증 페이지의 코드를 붙여넣고 Enter를 누르세요.",
                qb_core::accounts::Provider::Codex =>
                    "브라우저의 최종 localhost 리디렉션 주소를 붙여넣고 Enter를 누르세요.",
            }
        );
    }
    Ok(605)
}

fn read_manual_login_input(
    reader: impl std::io::BufRead,
) -> anyhow::Result<zeroize::Zeroizing<String>> {
    read_login_line(reader, false)
}

fn read_login_line(
    reader: impl std::io::BufRead,
    allow_empty: bool,
) -> anyhow::Result<zeroize::Zeroizing<String>> {
    use std::io::BufRead;
    let mut input = zeroize::Zeroizing::new(Vec::with_capacity(8193));
    reader.take(8193).read_until(b'\n', &mut input)?;
    anyhow::ensure!(
        !input.is_empty() && input.len() <= 8192,
        "로그인 입력 길이를 확인해 주세요."
    );
    let value =
        std::str::from_utf8(&input).map_err(|_| anyhow::anyhow!("로그인 입력을 확인해 주세요."))?;
    let value = zeroize::Zeroizing::new(value.trim_end_matches(['\r', '\n']).to_owned());
    anyhow::ensure!(
        allow_empty || !value.is_empty(),
        "로그인 입력을 확인해 주세요."
    );
    Ok(value)
}

fn manual_login_input(
    allow_empty: bool,
) -> tokio::sync::oneshot::Receiver<anyhow::Result<zeroize::Zeroizing<String>>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // Tokio's blocking pool waits for blocked stdin at runtime shutdown. A
    // detached ordinary thread owns no service/store handles and permits Ctrl+C
    // and timeout to finish login cleanup even while stdin is still open.
    std::thread::spawn(move || {
        let result = if allow_empty {
            read_login_line(std::io::stdin().lock(), true)
        } else {
            read_manual_login_input(std::io::stdin().lock())
        };
        let _ = sender.send(result);
    });
    receiver
}

#[cfg(test)]
mod manual_login_input_tests {
    use super::read_manual_login_input;

    #[test]
    fn reads_one_bounded_line_without_waiting_for_eof() {
        assert_eq!(
            &**read_manual_login_input(&b"sample-code#sample-state\r\nignored\n"[..]).unwrap(),
            "sample-code#sample-state"
        );
        assert!(read_manual_login_input(&b"\n"[..]).is_err());
        assert!(read_manual_login_input(&b""[..]).is_err());
        assert!(read_manual_login_input(&vec![b'x'; 8193][..]).is_err());
        assert!(read_manual_login_input(&b"\xff\n"[..]).is_err());
    }
}

pub(crate) async fn finish_offline_login(
    service: qb_core::oauth::LoginService,
    obs: std::sync::Arc<qb_core::observability::Observability>,
) -> anyhow::Result<()> {
    service.cancel_all().await;
    let flushed = tokio::task::spawn_blocking(move || obs.flush()).await?;
    drop(service);
    flushed
}

pub(crate) fn login_failure(error: Option<&str>) -> &'static str {
    use qb_core::oauth::LoginError;
    match error {
        Some(error) if error == LoginError::DeviceUnavailable.to_string() => {
            "기기 코드 로그인을 사용할 수 없습니다. 개인 계정의 ChatGPT Security 설정에서 켜거나 workspace admin에게 허용을 요청하세요. Enter로 붙여넣기 로그인을 사용할 수 있습니다."
        }
        Some(error) if error == LoginError::DeviceRateLimited.to_string() => {
            "기기 코드 요청이 제한되었습니다. 새 코드를 자동 요청하지 않습니다. Enter로 붙여넣기 로그인을 사용할 수 있습니다."
        }
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
        Some(error) if error == qb_core::accounts::login::IDENTITY_CONFLICT_REASON => {
            qb_core::accounts::login::IDENTITY_CONFLICT_REASON
        }
        Some(error) if error == LoginError::StateMismatch.to_string() => {
            "로그인 상태가 일치하지 않습니다. 로그인을 다시 시작해 주세요."
        }
        Some(error) if error == LoginError::StoreFailed.to_string() => {
            "계정 정보를 저장하지 못했습니다. 저장 공간과 접근 권한을 확인해 주세요."
        }
        Some(error) if error == LoginError::NotFound.to_string() => {
            "로그인 요청을 찾을 수 없습니다. 로그인을 다시 시작해 주세요."
        }
        Some(error) if error == LoginError::InvalidCallback.to_string() => {
            "로그인 콜백이 올바르지 않습니다. 로그인을 다시 시작해 주세요."
        }
        Some(error) if error == LoginError::ExchangeFailed.to_string() => {
            "인증 서버와 통신하지 못했습니다. 로그인을 다시 시도해 주세요."
        }
        Some(error) if error == LoginError::InvalidIdentity.to_string() => {
            "인증 서버의 계정 정보를 확인하지 못했습니다."
        }
        Some(error) if error == LoginError::InvalidTestEndpoint.to_string() => {
            "테스트 주소는 관리 포트를 제외한 숫자 루프백 주소여야 합니다."
        }
        Some(error)
            if [1455, 54545]
                .iter()
                .any(|&port| error == (LoginError::CallbackBindFailed { port }).to_string()) =>
        {
            "로그인 콜백에 연결할 수 없습니다. 루프백 네트워크와 접근 권한을 확인해 주세요."
        }
        _ => "로그인에 실패했습니다. 로그인 요청과 계정 연결 상태를 확인한 뒤 다시 시도해 주세요.",
    }
}

#[cfg(test)]
#[path = "accounts_cli_m1b_fixes_tests.rs"]
mod m1b_fixes_tests;

#[cfg(test)]
#[test]
pub(crate) fn import_owned_account_reason_has_korean_label() {
    assert_eq!(
        translated(&serde_json::json!("owned_account")),
        "이미 소유한 계정"
    );
}

#[cfg(test)]
mod r13f_provider_list_tests {
    use super::*;
    // These sparse cases exercise the existing provider-line presentation
    // adapter. They are not current /state transport fixtures.
    type CliAccountState = super::CliAccountState<serde_json::Value>;
    use qb_core::usage_forecast::{AccountUsageForecast, ForecastOutcome, Forecasts};
    use serde_json::{Value, json};
    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }
    fn row(id: &str) -> Value {
        json!({"id":id,"provider":"codex","label":id,"email":format!("{id}@example.test"),"user_state":"active","health":{"kind":"ready"},"source":{"kind":"oauth"},"pool_eligible":true,"windows":[{"name":"5h","scope":"account","used_pct":50.,"reset_at":now()+chrono::Duration::hours(3)}]})
    }
    fn snapshot(accounts: Vec<Value>, eligible: Value) -> CliAccountState {
        qb_proto::admin_state::deserialize_compat(
            json!({"accounts":accounts,"pools":{"codex":{"eligible":eligible}}}),
            true,
        )
        .unwrap()
    }
    fn forecast() -> AccountUsageForecast {
        let mut f = Forecasts::default();
        for (minute, pct) in [(-10, 40.), (-5, 45.), (0, 50.)] {
            f.observe(
                "sample-acct",
                "5h",
                pct,
                Some(now() + chrono::Duration::hours(3)),
                now() + chrono::Duration::minutes(minute),
            );
        }
        f.view("sample-acct", now(), None)
    }
    fn codex(state: Option<&CliAccountState>, metadata: &[Value], english: bool) -> String {
        provider_list_lines(state, metadata, now(), english)
            .unwrap()
            .remove(1)
    }
    #[test]
    fn snapshot_usable_count_needs_no_forecast_or_rate_and_never_uses_metadata_online() {
        let mut paused = row("sample-acct-paused");
        paused["user_state"] = json!("paused");
        paused["pool_eligible"] = json!(false);
        let state = snapshot(
            vec![row("sample-acct-a"), row("sample-acct-b"), paused],
            json!(2),
        );
        for english in [true, false] {
            let lines =
                provider_list_lines(Some(&state), &[row("sample-acct-other")], now(), english)
                    .unwrap();
            assert_eq!(lines.len(), 2);
            assert!(lines[0].starts_with(if english {
                "Claude · 0 of 0 usable"
            } else {
                "Claude · 0개 중 0개 사용 가능"
            }));
            let line = &lines[1];
            assert!(line.starts_with(if english {
                "Codex · 2 of 3 usable"
            } else {
                "Codex · 3개 중 2개 사용 가능"
            }));
            assert!(line.contains(if english {
                "forecast measured 0/2 accounts"
            } else {
                "예측 측정 0/2 계정"
            }));
            assert!(!line.contains(if english {
                "next out:"
            } else {
                "다음 소진 sample"
            }));
        }
        assert_eq!(
            state
                .pools
                .as_ref()
                .unwrap()
                .codex
                .as_ref()
                .unwrap()
                .eligible,
            Some(2)
        );
        let old: CliAccountState = qb_proto::admin_state::deserialize_compat(
            json!({"accounts":[row("sample-acct")]}),
            true,
        )
        .unwrap();
        assert!(codex(Some(&old), &[], true).contains("Usable unknown / 1 accounts"));
        assert!(codex(None, &[row("sample-acct")], true).contains("Usable unknown / 1 accounts"));
        assert!(codex(None, &[row("sample-acct")], true).contains("core offline"));
        assert!(codex(Some(&snapshot(vec![], Value::Null)), &[], true).contains("0 of 0 usable"));
    }
    #[test]
    fn partial_coverage_includes_exhausted_facts_and_unknowns_not_paused_disabled_or_drained() {
        let mut spent = row("sample-acct-spent");
        let mut f = Forecasts::default();
        f.observe(
            "sample-acct-spent",
            "5h",
            100.,
            Some(now() + chrono::Duration::hours(3)),
            now() - chrono::Duration::hours(2),
        );
        spent["usage_forecast"] = json!(f.view("sample-acct-spent", now(), None));
        spent["health"] = json!({"kind":"cooling","scope":"account","reason":"quota","until":now()+chrono::Duration::minutes(10)});
        spent["pool_eligible"] = json!(false);
        let mut paused = row("sample-acct-paused");
        paused["user_state"] = json!("paused");
        let mut drained = row("sample-acct-drained");
        drained["drain"] = json!(true);
        let mut disabled = row("sample-acct-disabled");
        disabled["user_state"] = json!("disabled");
        let mut relogin = row("sample-acct-relogin");
        relogin["health"] = json!({"kind":"needs_relogin"});
        let state = snapshot(
            vec![
                spent,
                row("sample-acct-cold"),
                paused,
                drained,
                disabled,
                relogin,
            ],
            json!(1),
        );
        let line = codex(Some(&state), &[], true);
        assert!(line.contains("1 of 6 usable"));
        assert!(line.contains("forecast measured 1/3 accounts"));
        assert!(line.contains(&format!(
            "earliest recovery {}",
            qb_core::usage_forecast_display::forecast_clock(
                now(),
                now() + chrono::Duration::hours(3),
                true
            )
        )));
    }
    #[test]
    fn shared_forecast_visibility_never_names_hidden_estimates() {
        let mut a = row("sample-acct");
        a["usage_forecast"] = json!(forecast());
        let line = codex(Some(&snapshot(vec![a.clone()], json!(1))), &[], true);
        assert!(line.contains("next out: sample-acct"));
        assert!(line.contains("forecast measured 1/1 accounts"));
        for variant in 0..5 {
            let mut bad = a.clone();
            match variant {
                0 => bad["windows"][0]["scope"] = json!("model:sample"),
                1 => bad["health"] = json!({"kind":"cooling","scope":"account","reason":"auth"}),
                2 => bad["usage_forecast"]["as_of"] = json!(now() + chrono::Duration::seconds(1)),
                3 => {
                    bad["usage_forecast"]["observed_at"] =
                        json!(now() - chrono::Duration::minutes(6))
                }
                _ => {
                    bad["usage_forecast"]["windows"][0]["outcome"] =
                        json!(ForecastOutcome::ResetPending)
                }
            }
            let line = codex(Some(&snapshot(vec![bad], json!(1))), &[], true);
            assert!(
                !line.contains("sample-acct"),
                "hidden account named in estimate: {line}"
            );
            assert!(!line.contains("next out:"));
            assert!(line.contains("1 of 1 usable")); // Authoritative count never forecast-derived.
        }
    }
    #[test]
    fn actual_credits_without_forecast_differ_from_permission_and_unlimited() {
        let mut a = row("sample-acct");
        a["allow_credits"] = json!(true);
        a["credits"] = json!({"has_credits":true});
        assert!(
            !codex(Some(&snapshot(vec![a.clone()], json!(1))), &[], true)
                .contains("credits continue")
        );
        a["credits_in_use"] = json!(true);
        let line = codex(Some(&snapshot(vec![a.clone()], json!(1))), &[], true);
        assert!(line.contains("credits continue on sample-acct (runway unknown)"));
        assert!(line.contains("forecast measured 0/1 accounts"));
        a["credits"]["unlimited"] = json!(true);
        assert!(
            codex(Some(&snapshot(vec![a.clone()], json!(1))), &[], true).contains("(unlimited)")
        );
        a["allow_credits"] = json!(false);
        assert!(!codex(Some(&snapshot(vec![a], json!(1))), &[], true).contains("credits continue"));
    }
    #[test]
    fn finite_and_no_finite_credit_forecasts_stay_monetary_not_unlimited() {
        use qb_core::usage_forecast::CreditUsageForecast;
        let mut a = row("sample-acct");
        a["allow_credits"] = json!(true);
        a["credits_in_use"] = json!(true);
        let mut f = forecast();
        f.credit = Some(CreditUsageForecast {
            observed_at: now(),
            balance: 10.,
            recent_credits_per_hour: Some(0.),
            measurement_start: Some(now() - chrono::Duration::minutes(10)),
            exhaustion_at: None,
            seconds_to_exhaustion: None,
            outcome: ForecastOutcome::NoFiniteForecast,
            changing: false,
            truncated: false,
        });
        a["usage_forecast"] = json!(f);
        let line = codex(Some(&snapshot(vec![a.clone()], json!(1))), &[], true);
        assert!(line.contains("credits continue on sample-acct (no finite estimate)"));
        assert!(!line.contains("unlimited"));
        assert!(!line.contains("next out:"));
        f.credit.as_mut().unwrap().outcome = ForecastOutcome::Exhaustion;
        f.credit.as_mut().unwrap().exhaustion_at = Some(now() + chrono::Duration::minutes(20));
        a["usage_forecast"] = json!(f);
        assert!(
            codex(Some(&snapshot(vec![a], json!(1))), &[], true)
                .contains("credits continue on sample-acct (20m left")
        );
    }
    #[test]
    fn ordering_and_outside_use_do_not_create_pooled_capacity_or_rate_estimates() {
        let mut a = row("sample-acct-a");
        a["usage_forecast"] = json!(forecast());
        a["outside_used"] = json!(true);
        let mut b = row("sample-acct-b");
        b["usage_forecast"] = json!(forecast());
        let first = codex(
            Some(&snapshot(vec![b.clone(), a.clone()], json!(2))),
            &[],
            true,
        );
        assert_eq!(
            first,
            codex(
                Some(&snapshot(vec![a.clone(), b.clone()], json!(2))),
                &[],
                true
            )
        );
        assert!(first.contains("next out: sample-acct-a"));
        a["outside_used"] = json!(false);
        assert_eq!(
            first,
            codex(Some(&snapshot(vec![a, b], json!(2))), &[], true)
        );
        assert!(!first.contains("pooled"));
        assert!(!first.contains("%/"));
    }

    #[test]
    fn complete_shared_cli_transport_keeps_account_facts_and_usable_count_without_rate() {
        let account = json!({
            "outside_used":false,"pool_eligible":true,"takeover_pending":false,
            "quota_cooling":null,"quota_credit_releasable":false,"observed_at":null,
            "quota_notice":null,"credits_in_use":false,
            "usage_forecast":{"as_of":now(),"observed_at":null,"windows":[],
                "earliest_window":null,"credit":null,"truncated":false},
            "has_alternative":false,"id":"sample-acct-cli","generation":1,
            "last_refresh_at":null,"provider":"codex","label":"sample-acct-cli",
            "email":"sample-acct-cli@example.test","plan":null,"user_state":"active",
            "source":{"kind":"takeover","from":"sample-acct-origin","at":now()},
            "health":{"kind":"ready"},"probe_pending":false,
            "pacing":{"backoff_until":null,"step":0,"release_interval_ms":0,"waiting":0},
            "windows":[],"credits":null,"last_used_at":null,"bound_sessions":0,
            "token_expires_at":null,"drain":false,"allow_credits":false,
            "refresh_attention":false,"active_sessions":0,"spread_threshold":0
        });
        let wire = json!({
            "accounts":[account.clone()],
            "pools":{"claude":{"eligible":0,"cooling":{},"unavailable":0},
                "codex":{"eligible":1,"cooling":{},"unavailable":0}}
        });
        let bytes = serde_json::to_vec(&wire).unwrap();
        let state = decode_cli_account_state(&bytes).unwrap();
        assert_eq!(state.accounts[0].generation, 1);
        assert!(!state.accounts[0].takeover_pending);
        let rows = cli_account_rows(&state.accounts).unwrap();
        assert_eq!(
            serde_json::to_vec(&rows[0]).unwrap(),
            serde_json::to_vec(&account).unwrap()
        );
        assert_eq!(rows[0]["source"], account["source"]);
        assert!(rows[0].get("last_relevant_state_at").is_none());
        for (english, expected) in [
            (true, "Codex · 1 of 1 usable"),
            (false, "Codex · 1개 중 1개 사용 가능"),
        ] {
            let lines = provider_list_lines(Some(&state), &[], now(), english).unwrap();
            assert_eq!(lines.len(), 2);
            assert!(lines[1].starts_with(expected));
            assert!(!lines[1].contains("next out:"));
            assert!(!lines[1].contains("pooled"));
        }
        let complete = String::from_utf8(bytes).unwrap();
        for first in ["null", "\"bad\"", "2"] {
            let duplicate = complete.replacen(
                "\"generation\":1",
                &format!("\"generation\":{first},\"generation\":1"),
                1,
            );
            assert_ne!(complete, duplicate);
            assert!(decode_cli_account_state(duplicate.as_bytes()).is_err());
        }
        let mut missing = wire;
        missing["accounts"][0]
            .as_object_mut()
            .unwrap()
            .remove("generation");
        assert!(decode_cli_account_state(&serde_json::to_vec(&missing).unwrap()).is_err());
    }

    #[test]
    fn typed_cli_rejects_sparse_live_accounts_instead_of_inventing_facts() {
        for wire in [
            br#"{"accounts":[{"id":"sample-acct"}]}"#.as_slice(),
            br#"{"accounts":[{"id":"sample-acct","id":"sample-acct-other"}]}"#.as_slice(),
        ] {
            assert!(decode_cli_account_state(wire).is_err());
        }
    }

    #[test]
    fn raw_cli_state_decoder_rejects_duplicates_before_value_collapse() {
        for wire in [
            r#"{"accounts":[],"accounts":[]}"#,
            r#"{"accounts":null,"accounts":[]}"#,
            r#"{"accounts":[],"inflight":{"requests":1,"requests":2}}"#,
            r#"{"accounts":[],"inflight":{"requests":null,"requests":2}}"#,
            r#"{"accounts":[],"inflight":{"bytes":1,"bytes":2}}"#,
            r#"{"accounts":[],"inflight":{"bytes":null,"bytes":2}}"#,
            r#"{"accounts":[],"memory":{"private_bytes":1,"private_bytes":2}}"#,
            r#"{"accounts":[],"memory":{"private_bytes":null,"private_bytes":2}}"#,
            r#"{"accounts":[],"bindings":{"count":1,"count":2}}"#,
            r#"{"accounts":[],"bindings":{"count":null,"count":2}}"#,
            r#"{"accounts":[],"capabilities":{"account_adopt":true,"account_adopt":false,"manual_refresh":true,"account_takeover":true}}"#,
        ] {
            assert!(decode_cli_account_state(wire.as_bytes()).is_err(), "{wire}");
        }
        let legacy = decode_cli_account_state(
            br#"{"accounts":[],"inflight":{"requests":4},"memory":{},"bindings":{}}"#,
        )
        .unwrap();
        assert_eq!(legacy.inflight.requests, 4);
        assert_eq!(legacy.inflight.bytes, 0);
        assert_eq!(legacy.memory.private_bytes, 0);
        assert_eq!(legacy.bindings.count, 0);
        assert!(legacy.pools.is_none());
        assert_eq!(legacy.draining, None);
        assert_eq!(legacy.capabilities, None);
        assert!(decode_cli_account_state(b"{}").is_err());
        assert!(decode_cli_account_state(br#"{"accounts":null}"#).is_err());
        assert!(decode_cli_account_state(br#"{"accounts":[]} {}"#).is_err());
    }
}

#[cfg(test)]
mod r15_pool_warning_transport_tests {
    use super::*;

    #[test]
    fn raw_pool_and_state_warning_duplicates_never_collapse() {
        for raw in [
            r#"{"accounts":[],"pools":{"codex":{"eligible":1,"eligible":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"eligible":"bad","eligible":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"eligible":null,"eligible":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":null,"codex":{"eligible":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"unavailable":null,"unavailable":2}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"cooling":{"auth":1,"auth":2}}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"cooling":{"auth":null,"auth":2}}}}"#,
            r#"{"accounts":[],"pools":{"codex":{"cooling":null,"cooling":{}}}}"#,
            r#"{"accounts":[],"warnings":[{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":0,"unknown_count":1}]}"#,
            r#"{"accounts":[],"warnings":[{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":null,"unknown_count":1}]}"#,
            r#"{"accounts":[],"warnings":[{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":"bad","unknown_count":1}]}"#,
            r#"{"accounts":[],"warnings":[{"kind":"move_breaker","observed_at":"2030-01-01T00:00:00Z","move_list":[{"from":null,"from":"a","to":"b"}],"until":"2030-01-01T00:04:00Z","session":"0123456789ab","from":"a","to":"b","reason":"global_trip","forced":false}]}"#,
        ] {
            assert!(
                decode_cli_account_state(raw.as_bytes()).is_err(),
                "accepted {raw}"
            );
        }
    }

    #[test]
    fn legacy_sparse_pools_remain_unknown_and_warning_payload_is_preserved() {
        let raw = r#"{"accounts":[],"pools":{"codex":{"eligible":2}},"warnings":[{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":1}]}"#;
        let state = decode_cli_account_state(raw.as_bytes()).unwrap();
        let pools = state.pools.as_ref().unwrap();
        assert!(pools.claude.is_none());
        let codex = pools.codex.as_ref().unwrap();
        assert_eq!(codex.eligible, Some(2));
        assert!(codex.cooling.is_none());
        assert!(codex.unavailable.is_none());
        let wire = serde_json::to_value(&state.warnings[0]).unwrap();
        assert_eq!(wire["known_count"], 2);
        assert_eq!(wire["unknown_count"], 1);
        for raw in [r#"{"accounts":[]}"#, r#"{"accounts":[],"pools":null}"#] {
            assert!(
                decode_cli_account_state(raw.as_bytes())
                    .unwrap()
                    .pools
                    .is_none()
            );
        }
        // /state does not adopt the separate SSE quota_notice schema.
        let raw = r#"{"accounts":[],"warnings":[{"kind":"quota_notice","account":"a"}]}"#;
        assert!(decode_cli_account_state(raw.as_bytes()).is_err());
    }
}

#[cfg(test)]
mod r15_account_subset_raw_tests {
    use super::*;
    use serde_json::{Value, json};

    fn action(released: bool) -> Value {
        let mut value = json!({"id":"synthetic-account"});
        if released {
            value["released"] = json!(true);
        }
        for (key, field) in json!({"provider":"codex","label":"synthetic-label",
            "email":"synthetic@example.invalid","plan":null,"user_state":"active",
            "health":{"kind":"ready"},"drain":false,"allow_credits":false})
        .as_object()
        .unwrap()
        {
            value[key] = field.clone();
        }
        value
    }
    fn row() -> Value {
        json!({"id":"synthetic-account","provider":"codex",
            "email":"synthetic@example.invalid","plan":null,
            "expires_at":"2030-01-01T00:00:00+00:00","user_state":"active",
            "action":"updated","reason":null,
            "stored_timestamp":null,"file_timestamp":null})
    }
    fn fixture(kind: AccountSubset) -> Value {
        match kind {
            AccountSubset::Import => json!([row()]),
            AccountSubset::Action => action(false),
            AccountSubset::Released => action(true),
        }
    }
    fn rejected(bytes: &[u8], kind: AccountSubset) {
        let error = decode_account_subset(bytes, kind).unwrap_err();
        assert_eq!(error.to_string(), "계정 응답 형식이 올바르지 않습니다.");
    }
    #[test]
    fn original_presentation_bytes_and_timestamp_spelling_survive_validation() {
        for kind in [
            AccountSubset::Import,
            AccountSubset::Action,
            AccountSubset::Released,
        ] {
            let raw = fixture(kind).to_string();
            let value = decode_account_subset(raw.as_bytes(), kind).unwrap();
            assert_eq!(serde_json::to_vec(&value).unwrap(), raw.as_bytes());
            let padded = format!(" \n{raw}\t");
            assert_eq!(
                decode_account_subset(padded.as_bytes(), kind).unwrap(),
                value
            );
        }
        assert_eq!(
            decode_account_subset(b"[]", AccountSubset::Import).unwrap(),
            json!([])
        );
        let skipped = br#"[{"id":null,"provider":null,"email":null,"plan":null,"expires_at":null,"user_state":null,"action":"skipped","reason":"invalid_data","stored_timestamp":null,"file_timestamp":null}]"#;
        let value = decode_account_subset(skipped, AccountSubset::Import).unwrap();
        assert_eq!(
            serde_json::to_vec(&value).unwrap().as_slice(),
            skipped.as_slice()
        );
    }
    #[test]
    fn every_current_subset_field_is_required_and_raw_duplicates_are_visible() {
        for kind in [
            AccountSubset::Import,
            AccountSubset::Action,
            AccountSubset::Released,
        ] {
            let value = fixture(kind);
            let object = if matches!(kind, AccountSubset::Import) {
                &value[0]
            } else {
                &value
            };
            for (key, field) in object.as_object().unwrap() {
                let mut missing = object.clone();
                missing.as_object_mut().unwrap().remove(key);
                let missing = if matches!(kind, AccountSubset::Import) {
                    json!([missing])
                } else {
                    missing
                };
                rejected(missing.to_string().as_bytes(), kind);
                let raw = object.to_string();
                let duplicate = format!(
                    "{{{}:{},{}",
                    serde_json::to_string(key).unwrap(),
                    field,
                    &raw[1..]
                );
                let duplicate = if matches!(kind, AccountSubset::Import) {
                    format!("[{duplicate}]")
                } else {
                    duplicate
                };
                rejected(duplicate.as_bytes(), kind);
            }
        }
    }
    #[test]
    fn caller_selects_array_or_object_contract_without_permissive_branch_guessing() {
        for kind in [
            AccountSubset::Import,
            AccountSubset::Action,
            AccountSubset::Released,
        ] {
            for bad in [
                "null",
                "true",
                "17",
                "\"synthetic\"",
                "{}",
                "[true]",
                "not-json",
            ] {
                rejected(bad.as_bytes(), kind);
            }
            let raw = fixture(kind).to_string();
            rejected(format!("{raw} {{}}").as_bytes(), kind);
            let object = if matches!(kind, AccountSubset::Import) {
                row()
            } else {
                fixture(kind)
            };
            let positional = Value::Array(object.as_object().unwrap().values().cloned().collect());
            let bad = if matches!(kind, AccountSubset::Import) {
                json!([positional])
            } else {
                positional
            };
            rejected(bad.to_string().as_bytes(), kind);
        }
        rejected(row().to_string().as_bytes(), AccountSubset::Import);
        rejected(
            fixture(AccountSubset::Import).to_string().as_bytes(),
            AccountSubset::Action,
        );
        rejected(
            action(false).to_string().as_bytes(),
            AccountSubset::Released,
        );
        for kind in [AccountSubset::Action, AccountSubset::Released] {
            rejected(b"[]", kind);
        }
    }
    #[test]
    fn malformed_domain_leaves_never_reach_value_presentation() {
        for kind in [
            AccountSubset::Import,
            AccountSubset::Action,
            AccountSubset::Released,
        ] {
            let object = if matches!(kind, AccountSubset::Import) {
                row()
            } else {
                fixture(kind)
            };
            let mut bad = object.clone();
            bad["provider"] = json!("synthetic-secret-provider");
            let bad = if matches!(kind, AccountSubset::Import) {
                json!([bad])
            } else {
                bad
            };
            rejected(bad.to_string().as_bytes(), kind);
            let mut bad = object;
            bad["user_state"] = json!(17);
            let bad = if matches!(kind, AccountSubset::Import) {
                json!([bad])
            } else {
                bad
            };
            rejected(bad.to_string().as_bytes(), kind);
        }
        let mut bad = row();
        bad["expires_at"] = json!("not-a-timestamp");
        rejected(json!([bad]).to_string().as_bytes(), AccountSubset::Import);
        let raw = action(false).to_string().replace(
            "\"kind\":\"ready\"",
            "\"kind\":\"ready\",\"kind\":\"ready\"",
        );
        rejected(raw.as_bytes(), AccountSubset::Action);
        let mut bad = action(true);
        bad["released"] = json!("true");
        rejected(bad.to_string().as_bytes(), AccountSubset::Released);
    }
}

#[cfg(test)]
mod r15_account_error_reader_tests {
    #[test]
    fn account_error_cli_allowlist_and_no_payload_echo() {
        for (code, expected) in [
            ("not_found", "계정을 찾을 수 없습니다."),
            ("linked", "CPA가 갱신하는 계정입니다."),
            ("not_linked", "CPA 연결 계정만 소유권을 이전할 수 있습니다."),
            ("drain", "드레인 중인 계정은 갱신할 수 없습니다."),
            ("needs_relogin", "다시 로그인해야 하는 계정입니다."),
            (
                "uncertain",
                "이전 갱신 결과가 불확실합니다. 확인 후 --force로 한 번 시도해 주세요.",
            ),
            (
                "generation_conflict",
                "계정이 변경됐습니다. 상태를 다시 읽고 시도해 주세요.",
            ),
            (
                "identity_conflict",
                "계정의 사용자를 확인할 수 없거나 사용자 파일이 겹칩니다.",
            ),
            ("cpa_running", "CPA가 아직 이 계정을 갱신할 수 있습니다."),
            ("cpa_unknown", "CPA 실행 상태를 확인할 수 없습니다."),
        ] {
            let raw = format!(
                r#"{{"error":{{"code":"{code}","message":"synthetic-secret","extra":true}}}}"#
            );
            assert_eq!(super::account_error_message(raw.as_bytes()), expected);
        }
        for raw in [
            r#"{"error":{"message":"synthetic-secret"}}"#,
            r#"{"error":{"code":"terminal","outcome":"invalid_grant"}}"#,
            r#"{"error":{"code":"poll_too_soon","retry_after":3}}"#,
            r#"{"error":{"code":"future","message":"synthetic-secret"}}"#,
            r#"{"error":{"code":"linked","code":"drain"}}"#,
            r#"[{"code":"linked"}]"#,
            "not json",
        ] {
            assert_eq!(
                super::account_error_message(raw.as_bytes()),
                "계정 작업을 마치지 못했습니다. 코어 상태를 확인해 주세요."
            );
        }
    }
}
