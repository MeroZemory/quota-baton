use clap::{Args, Subcommand, ValueEnum};
use qb_core::{accounts::AccountStore, config::Config};
use qb_proto::{
    CpaImportRequest, ImportMode,
    admin_login::{LoginStart, LoginState, LoginStatus},
};
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

/// Only static, reviewed local messages may cross the CLI error boundary.
#[derive(Debug)]
pub struct AccountCliError(pub &'static str);
impl std::fmt::Display for AccountCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for AccountCliError {}
fn account_error(message: &'static str) -> anyhow::Error {
    AccountCliError(message).into()
}

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
    #[command(about = "다른 앱에서 계정의 토큰 관리 권한을 가져옵니다")]
    Takeover {
        #[arg(value_enum)]
        provider: LoginProvider,
        path: PathBuf,
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        keep_source: bool,
        #[arg(long, help = "경고와 계정 확인 후 가져오기를 승인합니다")]
        yes: bool,
    },
    #[command(about = "계정 목록을 확인합니다")]
    List {
        #[arg(long, help = "사용량 예측의 속도·근거·모든 구간을 자세히 표시합니다")]
        detail: bool,
    },
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
    #[command(about = "사용량 한도를 해제하고 다음 요청으로 다시 시도합니다")]
    ReleaseHold { id: String },
    #[command(about = "CPA 연결 계정을 직접 갱신하도록 전환합니다")]
    Adopt {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
        #[arg(
            long,
            default_value = "cpa-file",
            help = "store, cpa-file 또는 백업 파일/폴더 경로"
        )]
        from: String,
    },
    #[command(about = "계정 토큰을 지금 갱신합니다")]
    Refresh {
        id: String,
        #[arg(long, help = "이전 갱신 결과가 불확실해도 한 번 시도합니다")]
        force: bool,
    },
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
    #[arg(long, help = "이 옵션 대신 accounts adopt 명령을 사용해 주세요")]
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
        AccountsCommand::Takeover {
            provider,
            path,
            replace,
            keep_source,
            yes,
        } => {
            takeover(home, config, provider, path, replace, keep_source, yes).await?;
        }
        AccountsCommand::Adopt { ids, from } => adopt_accounts(home, config, ids, from).await?,
        AccountsCommand::Refresh { id, force } => refresh_tokens(home, config, id, force).await?,
        AccountsCommand::ReleaseHold { id } => release_hold(home, config, &id).await?,
        AccountsCommand::RefreshUsage { id } => refresh_usage(home, config, &id).await?,
        AccountsCommand::Pause { id } => action(home, config, &id, "pause").await?,
        AccountsCommand::Resume { id } => action(home, config, &id, "resume").await?,
        AccountsCommand::Disable { id } => action(home, config, &id, "disable").await?,
        AccountsCommand::Enable { id } => action(home, config, &id, "enable").await?,
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
        AccountsCommand::Import {
            source: ImportSource::Cpa(args),
        } => {
            if args.adopt {
                return Err(account_error(
                    "소유권 이전은 accounts adopt 명령을 사용해 주세요.",
                ));
            }
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
                    admin_route_response(health, &client, &base, token.as_deref().unwrap_or(""))
                        .await?
                        .error_for_status()?;
                    let response = client
                        .post(format!("{base}/accounts/import"))
                        .timeout(Duration::from_secs(60))
                        .bearer_auth(token.as_deref().unwrap_or("").trim())
                        .json(&request)
                        .send()
                        .await?;
                    let bytes = admin_route_response(
                        response,
                        &client,
                        &base,
                        token.as_deref().unwrap_or(""),
                    )
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?;
                    decode_account_subset(&bytes, AccountSubset::Import)?
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

fn safe_version(value: &serde_json::Value) -> String {
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

fn missing_admin_route(status: reqwest::StatusCode, path: &str, value: &serde_json::Value) -> bool {
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

async fn admin_route_response(
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

fn takeover_inspect_lines(value: &serde_json::Value, keep_source: bool) -> Vec<String> {
    let mut lines = vec![format!(
        "가져오면 quota-baton이 이 계정의 토큰을 관리합니다. 원래 앱({})은 로그아웃되며, 다시 쓰려면 그 앱에서 새로 로그인해야 합니다. 원래 앱이 켜져 있으면 먼저 종료하세요.",
        cell(&value["source_path"])
    )];
    if keep_source {
        lines.push("강한 경고: --keep-source는 원본 토큰 파일을 남깁니다. 원래 앱이 이 토큰을 다시 갱신하면 전체 토큰 계열이 폐기되어 양쪽 모두 다시 로그인해야 할 수 있습니다. 원래 앱을 실행하지 마세요.".into());
    }
    for (key, label) in [
        ("email", "이메일"),
        ("plan", "요금제"),
        ("provider_account_id", "제공자 계정 ID"),
        ("source_path", "원본 경로"),
        ("source_last_refresh", "원본 최근 갱신"),
        ("source_mtime", "원본 수정 시각"),
        ("owner_alive", "원래 앱 실행 중"),
        ("existing_id", "기존 계정 ID"),
    ] {
        let display = if key == "owner_alive" {
            match value[key].as_bool() {
                Some(true) => "감지됨 · 먼저 종료하세요".into(),
                Some(false) => "감지되지 않음 (최선의 확인)".into(),
                None => "알 수 없음".into(),
            }
        } else {
            cell(&value[key])
        };
        lines.push(format!("{label}: {display}"));
    }
    lines
}

fn takeover_result_line(value: &serde_json::Value) -> String {
    format!(
        "계정 {}: 가져오기 완료 · 원본 이름 변경 {} · 갱신 {} · 만료 {}",
        cell(&value["id"]),
        cell(&value["renamed_to"]),
        match value["refresh"]["ok"].as_bool() {
            Some(true) => "성공",
            Some(false) | None => "확인 필요",
        },
        cell(&value["refresh"]["token_expires_at"])
    )
}

// Original live bytes -> phase-specific shared DTO -> existing display reader.
async fn takeover_response<T: serde::de::DeserializeOwned + serde::Serialize>(
    response: reqwest::Response,
) -> anyhow::Result<serde_json::Value> {
    if !response.status().is_success() {
        return account_response(response).await;
    }
    let bytes = response.bytes().await?;
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(account_error(
            "계정 가져오기 응답 형식이 올바르지 않습니다.",
        ));
    }
    let wire: T = serde_json::from_slice(&bytes)
        .map_err(|_| account_error("계정 가져오기 응답 형식이 올바르지 않습니다."))?;
    Ok(serde_json::to_value(wire)?)
}

async fn takeover(
    home: &Path,
    config: &Config,
    provider: LoginProvider,
    path: PathBuf,
    replace: bool,
    keep_source: bool,
    yes: bool,
) -> anyhow::Result<()> {
    let (client, base, token) = running_admin(home, config)
        .await?
        .ok_or_else(|| account_error("계정을 가져오려면 인증된 코어를 먼저 실행해 주세요."))?;
    // Resolve relative paths for the core without reading any source file locally.
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut request = serde_json::json!({
        "provider": qb_core::accounts::Provider::from(provider), "path": path,
        "phase": "inspect", "replace": replace, "keep_source": keep_source,
    });
    let inspect = takeover_response::<qb_proto::admin_takeover::TakeoverInspect>(
        admin_route_response(
            client
                .post(format!("{base}/accounts/takeover"))
                .bearer_auth(token.trim())
                .json(&request)
                .send()
                .await?,
            &client,
            &base,
            &token,
        )
        .await?,
    )
    .await?;
    for line in takeover_inspect_lines(&inspect, keep_source) {
        println_utf8!("{line}");
    }
    if !yes {
        return Err(TakeoverInspectOnly.into());
    }
    let digest = inspect["inspect_digest"]
        .as_str()
        .filter(|digest| !digest.is_empty())
        .ok_or_else(|| account_error("확인 다이제스트가 없습니다. 코어 응답을 확인해 주세요."))?;
    request["phase"] = serde_json::json!("commit");
    request["inspect_digest"] = serde_json::json!(digest);
    let result = takeover_response::<qb_proto::admin_takeover::TakeoverCommit>(
        admin_route_response(
            client
                .post(format!("{base}/accounts/takeover"))
                .timeout(Duration::from_secs(40))
                .bearer_auth(token.trim())
                .json(&request)
                .send()
                .await?,
            &client,
            &base,
            &token,
        )
        .await?,
    )
    .await?;
    println_utf8!("{}", takeover_result_line(&result));
    if result["refresh"]["ok"].as_bool() != Some(true) {
        return Err(account_error(
            "계정은 가져왔지만 갱신을 확인하지 못했습니다. 계정 상태를 확인해 주세요.",
        ));
    }
    Ok(())
}

fn valid_account_id(id: &str) -> anyhow::Result<()> {
    if qb_core::observability::safe_identifier(id).is_none() {
        return Err(account_error("계정 ID를 확인해 주세요."));
    }
    Ok(())
}

// Select the response contract at the request call site, never by trying
// permissive DTO alternatives against a normalized Value.
#[derive(Clone, Copy)]
enum AccountSubset {
    Import,
    Action,
    Released,
}

fn decode_account_subset(
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

async fn release_hold_response(response: reqwest::Response) -> anyhow::Result<serde_json::Value> {
    if !response.status().is_success() {
        // Keep the existing allowlisted status/code classifier unchanged.
        return account_response(response).await;
    }
    let bytes = response.bytes().await?;
    decode_account_subset(&bytes, AccountSubset::Released)
}

fn account_error_message(bytes: &[u8]) -> &'static str {
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

async fn account_response(response: reqwest::Response) -> anyhow::Result<serde_json::Value> {
    let status = response.status();
    if !status.is_success() {
        let bytes = response.bytes().await.unwrap_or_default();
        return Err(account_error(account_error_message(&bytes)));
    }
    // Preserve the existing success reader; its typed migrations are separate.
    let value: serde_json::Value = response.json().await.unwrap_or_default();
    Ok(value)
}
fn adoption_error(error: qb_core::accounts::adopt::AdoptError) -> anyhow::Error {
    use qb_core::accounts::adopt::AdoptError;
    account_error(match error {
        AdoptError::NotFound => "계정을 찾을 수 없습니다.",
        AdoptError::NotLinked => "CPA 연결 계정만 소유권을 이전할 수 있습니다.",
        AdoptError::Generation { .. } => "계정이 변경됐습니다. 상태를 다시 읽고 시도해 주세요.",
        AdoptError::IdentityMismatch | AdoptError::AmbiguousIdentity => {
            "계정의 사용자를 확인할 수 없거나 사용자 파일이 겹칩니다."
        }
        AdoptError::CpaRunning => "CPA가 아직 이 계정을 갱신할 수 있습니다.",
        AdoptError::ProbeUnavailable => "CPA 실행 상태를 확인할 수 없습니다.",
        AdoptError::SourceIo | AdoptError::Store(_) => "계정 자료를 읽거나 저장할 수 없습니다.",
    })
}

// Deserialize original successful bytes before any Value normalization.
// Error statuses retain the existing allowlisted account error behavior.
async fn adoption_response(
    response: reqwest::Response,
) -> anyhow::Result<qb_core::accounts::adopt::AdoptResult> {
    if !response.status().is_success() {
        account_response(response).await?;
        return Err(account_error(
            "계정 작업을 마치지 못했습니다. 코어 상태를 확인해 주세요.",
        ));
    }
    let bytes = response.bytes().await?;
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(account_error("소유권 이전 응답 형식이 올바르지 않습니다."));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| account_error("소유권 이전 응답 형식이 올바르지 않습니다."))
}

async fn adopt_accounts(
    home: &Path,
    config: &Config,
    ids: Vec<String>,
    from: String,
) -> anyhow::Result<()> {
    use qb_core::accounts::adopt::{AdoptFrom, AdoptRequest, AdoptSource};
    for id in &ids {
        valid_account_id(id)?;
    }
    let from = match from.as_str() {
        "store" => AdoptFrom::Source(AdoptSource::Store),
        "cpa-file" | "cpa_file" => AdoptFrom::Source(AdoptSource::CpaFile),
        _ => {
            let path = PathBuf::from(from);
            AdoptFrom::Path {
                path: if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()?.join(path)
                },
            }
        }
    };
    let admin = running_admin(home, config).await?;
    // A responding core always owns the lease, including on auth/health failure.
    let local = if admin.is_none() {
        Some(std::sync::Arc::new(AccountStore::acquire(home)?))
    } else {
        None
    };
    println_utf8!("계정 ID\t결과\t토큰 출처\t만료 시각");
    for id in ids {
        let result: qb_core::accounts::adopt::AdoptResult = if let Some((client, base, token)) =
            &admin
        {
            let state = account_response(
                admin_route_response(
                    client
                        .get(format!("{base}/state"))
                        .bearer_auth(token.trim())
                        .send()
                        .await?,
                    client,
                    base,
                    token,
                )
                .await?,
            )
            .await?;
            let generation = state["accounts"]
                .as_array()
                .and_then(|accounts| accounts.iter().find(|account| account["id"] == id))
                .and_then(|account| account["generation"].as_u64())
                .ok_or_else(|| {
                    account_error("계정을 찾을 수 없거나 코어가 소유권 이전을 지원하지 않습니다.")
                })?;
            adoption_response(
                admin_route_response(
                    client
                        .post(format!("{base}/accounts/{id}/adopt"))
                        .timeout(Duration::from_secs(35))
                        .bearer_auth(token.trim())
                        .json(&AdoptRequest {
                            from: from.clone(),
                            expected_generation: generation,
                        })
                        .send()
                        .await?,
                    client,
                    base,
                    token,
                )
                .await?,
            )
            .await?
        } else {
            let store = local.as_ref().expect("local lease").clone();
            let from = from.clone();
            let account_id = id.clone();
            tokio::task::spawn_blocking(move || {
                let account = store
                    .load(&account_id)
                    .map_err(qb_core::accounts::adopt::AdoptError::from)?;
                qb_core::accounts::adopt::run(
                    &store,
                    &account_id,
                    AdoptRequest {
                        from,
                        expected_generation: account.generation,
                    },
                )
            })
            .await?
            .map_err(adoption_error)?
        };
        let origin = match result.from.as_str() {
            "cpa_file" => "CPA 파일",
            "backup" => "백업",
            _ => "저장된 토큰",
        };
        println_utf8!(
            "{}\t직접 갱신으로 전환\t{}\t{}",
            id,
            origin,
            cell(&serde_json::to_value(result.token_expires_at)?)
        );
        for warning in result.warnings {
            use qb_proto::admin_adopt::AdoptWarning;
            let message = match warning {
                AdoptWarning::CpaFileMissing => "CPA 파일이 없어 다른 자료를 사용했습니다.",
                AdoptWarning::CpaFileUnreadable => {
                    "읽을 수 없는 CPA 파일이 있어 다른 자료를 사용했습니다."
                }
                AdoptWarning::BackupMissing | AdoptWarning::BackupUnreadable => {
                    "백업을 읽을 수 없어 저장된 자료를 사용했습니다."
                }
                AdoptWarning::OlderOrUnorderedCandidateIgnored => {
                    "오래되었거나 시각을 비교할 수 없는 토큰은 사용하지 않았습니다."
                }
                AdoptWarning::EmailUpdated => "같은 계정의 이메일 표시를 갱신했습니다.",
                AdoptWarning::StoredTokensMayNeedRelogin => {
                    "저장된 토큰의 만료 또는 불확실 상태를 확인해 주세요."
                }
            };
            println_utf8!("계정 {}: {}", id, message);
        }
    }
    Ok(())
}

async fn manual_refresh_response(
    response: reqwest::Response,
) -> anyhow::Result<qb_proto::admin_refresh::ManualRefreshReply> {
    if !response.status().is_success() {
        account_response(response).await?;
        return Err(account_error(
            "계정 작업을 마치지 못했습니다. 코어 상태를 확인해 주세요.",
        ));
    }
    let bytes = response.bytes().await?;
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(account_error("갱신 응답 형식이 올바르지 않습니다."));
    }
    serde_json::from_slice(&bytes).map_err(|_| account_error("갱신 응답 형식이 올바르지 않습니다."))
}

async fn refresh_tokens(
    home: &Path,
    config: &Config,
    id: String,
    force: bool,
) -> anyhow::Result<()> {
    valid_account_id(&id)?;
    let summary = if let Some((client, base, token)) = running_admin(home, config).await? {
        manual_refresh_response(
            admin_route_response(
                client
                    .post(format!("{base}/accounts/{id}/refresh"))
                    .timeout(Duration::from_secs(40))
                    .bearer_auth(token.trim())
                    .json(&serde_json::json!({"force":force}))
                    .send()
                    .await?,
                &client,
                &base,
                &token,
            )
            .await?,
        )
        .await?
    } else {
        let store = std::sync::Arc::new(AccountStore::acquire(home)?);
        let obs = std::sync::Arc::new(qb_core::observability::Observability::new(home, None)?);
        let relay = qb_core::relay::RelayState::new(
            store,
            config,
            obs.clone(),
            std::sync::Arc::new(qb_core::debug_pin::DebugPins::default()),
        )?;
        #[cfg(debug_assertions)]
        let relay = {
            let mut relay = relay;
            for (provider, endpoint) in [
                (
                    qb_core::accounts::Provider::Claude,
                    &config.upstream.claude_token_endpoint,
                ),
                (
                    qb_core::accounts::Provider::Codex,
                    &config.upstream.codex_token_endpoint,
                ),
            ] {
                if let Some(endpoint) = endpoint {
                    relay = relay.with_token_endpoint(provider, endpoint.clone());
                }
            }
            relay
        };
        let result = relay.refresh_account_typed(id.clone(), force).await;
        let mut warned = false;
        // Never drop a lease while detached consumption or an unsaved rotation survives.
        while relay.shutdown_refreshes().await.is_err() {
            if !warned {
                println_utf8!(
                    "갱신 결과를 저장하는 동안 계정 잠금을 유지합니다. 저장 공간과 권한을 확인해 주세요."
                );
                warned = true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        obs.flush()?;
        result?
    };
    if summary.outcome == qb_proto::admin_refresh::RefreshOutcome::Success {
        println_utf8!(
            "계정 {}: 갱신 완료 · 만료 {} · 최근 갱신 {}",
            id,
            cell(&serde_json::to_value(summary.token_expires_at)?),
            cell(&serde_json::to_value(summary.last_refresh_at)?)
        );
        Ok(())
    } else {
        Err(account_error(
            "갱신 자격 증명이 무효화되거나 만료됐습니다. 다시 로그인해 주세요.",
        ))
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

async fn release_hold(home: &Path, config: &Config, id: &str) -> anyhow::Result<()> {
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

async fn refresh_usage(home: &Path, config: &Config, id: &str) -> anyhow::Result<()> {
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

fn status_heading(home: &Path, running: bool) -> anyhow::Result<Vec<String>> {
    let mut lines = vec![format!(
        "코어: {}",
        if running {
            "실행 중"
        } else {
            "중지됨 (저장된 상태)"
        }
    )];
    lines.extend(crate::cutover::cpa_dependency_note(home)?);
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

type CliAccountState<Account = qb_proto::AdminAccount> = qb_proto::AdminState<
    Account,
    qb_proto::admin_warning::AdminWarning,
    Option<qb_proto::admin_pool::ReadPools>,
>;

async fn live_account_views(
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
fn cli_account_rows<Account: serde::Serialize>(
    accounts: &[Account],
) -> serde_json::Result<Vec<serde_json::Value>> {
    accounts.iter().map(serde_json::to_value).collect()
}

fn provider_list_name(
    a: &qb_tray::v2::Account,
    rows: &[&qb_tray::v2::Account],
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

fn provider_list_lines<Account: serde::Serialize>(
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
        .map(serde_json::from_value::<qb_tray::v2::Account>)
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
            .map(|a| state.map(|_| qb_tray::v2::summary_forecast(a, now)))
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

fn provider_list_line(
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

async fn live_account_state(
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
fn decode_cli_account_state(body: &[u8]) -> serde_json::Result<CliAccountState> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let state = qb_proto::admin_state::deserialize_compat(&mut deserializer, true)?;
    deserializer.end()?;
    Ok(state)
}

fn list_forecast_lines(
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

fn forecast_lines(account: Option<&serde_json::Value>, running: bool) -> Vec<String> {
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

fn status_window_label(name: &str) -> String {
    match name {
        "5h" => "5시간".into(),
        "7d" => "7일".into(),
        "43200m" => "30일".into(),
        _ => cell(&serde_json::json!(name)),
    }
}

fn status_window_names(accounts: &[serde_json::Value]) -> Vec<String> {
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
        .map(|value| {
            let remaining = format!("{:.1}", (100.0 - value).clamp(0.0, 100.0));
            format!("{}%", remaining.trim_end_matches(".0"))
        })
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

// These CLI responses have always required named object fields. Delegate the
// original map directly, retaining the shared DTO's duplicate-key checks.
struct LoginObject<T>(T);
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

async fn login(
    home: &Path,
    config: &Config,
    provider: qb_core::accounts::Provider,
    replace_linked: bool,
    no_browser: bool,
) -> anyhow::Result<()> {
    let mut offline_obs = None;
    let (backend, id, auth_url) =
        if let Some((client, base, token)) = running_admin(home, config).await? {
            let response = admin_route_response(client
            .post(format!("{base}/accounts/login"))
            .bearer_auth(token.trim())
            .json(&serde_json::json!({ "provider": provider, "replace_linked": replace_linked }))
            .send()
            .await?, &client, &base, &token).await?;
            if let Err(error) = response.error_for_status_ref() {
                let payload: serde_json::Value = response.json().await.unwrap_or_default();
                println_utf8!("{}", login_failure(payload["error"]["message"].as_str()));
                return Err(error.into());
            }
            let start = response
                .json::<LoginObject<LoginStart>>()
                .await
                .map_err(|_| anyhow::anyhow!("로그인 응답을 확인해 주세요."))?
                .0;
            let id = Some(start.login_id.as_str())
                .filter(|id| {
                    !id.is_empty()
                        && id.len() <= 100
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                })
                .ok_or_else(|| anyhow::anyhow!("로그인 응답을 확인해 주세요."))?
                .to_owned();
            let url = start.auth_url;
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
                let obs =
                    std::sync::Arc::new(qb_core::observability::Observability::new(&home, None)?);
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
            let start = match service.start(provider, replace_linked).await {
                Ok(start) => start,
                Err(error) => {
                    println_utf8!("{}", login_failure(Some(&error.to_string())));
                    finish_offline_login(service, obs).await?;
                    anyhow::bail!("로그인을 시작하지 못했습니다.");
                }
            };
            offline_obs = Some(obs);
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
            match status.status {
                LoginState::Pending => {}
                LoginState::Completed => {
                    println_utf8!("로그인을 완료했습니다.");
                    return Ok(());
                }
                LoginState::Cancelled => {
                    println_utf8!("로그인을 취소했습니다.");
                    return Ok(());
                }
                LoginState::Expired => {
                    println_utf8!("로그인 시간이 만료되었습니다. 다시 시작해 주세요.");
                    anyhow::bail!("로그인 시간이 만료되었습니다.");
                }
                LoginState::Failed => {
                    // Never print the returned error/code or any account fields.
                    println_utf8!("{}", login_failure(status.error.as_deref()));
                    anyhow::bail!("로그인에 실패했습니다.");
                }
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
    if let (LoginBackend::Local(service), Some(obs)) = (backend, offline_obs) {
        finish_offline_login(service, obs).await?;
    }
    result
}

async fn finish_offline_login(
    service: qb_core::oauth::LoginService,
    obs: std::sync::Arc<qb_core::observability::Observability>,
) -> anyhow::Result<()> {
    service.cancel_all().await;
    let flushed = tokio::task::spawn_blocking(move || obs.flush()).await?;
    drop(service);
    flushed
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
fn import_owned_account_reason_has_korean_label() {
    assert_eq!(
        translated(&serde_json::json!("owned_account")),
        "이미 소유한 계정"
    );
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

#[cfg(test)]
mod r1_cpa_status_tests {
    use super::*;
    use crate::cutover::cpa_dependency_tests::{account, scratch};
    use serde_json::json;

    #[test]
    fn r162_accounts_list_detail_flag_defaults_off() {
        use clap::Parser;
        #[derive(Parser)]
        struct Args {
            #[command(subcommand)]
            command: AccountsCommand,
        }
        assert!(matches!(
            Args::try_parse_from(["synthetic", "list"]).unwrap().command,
            AccountsCommand::List { detail: false }
        ));
        assert!(matches!(
            Args::try_parse_from(["synthetic", "list", "--detail"])
                .unwrap()
                .command,
            AccountsCommand::List { detail: true }
        ));
    }

    #[test]
    fn r162_accounts_list_summary_shared_and_detail_legacy_preserved() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T00:00:00Z")
            .unwrap()
            .to_utc();
        let mut samples = qb_core::usage_forecast::Forecasts::default();
        samples.observe(
            "synthetic",
            "5h",
            50.,
            Some(now + chrono::Duration::hours(3)),
            now,
        );
        let forecast = samples.view("synthetic", now, None);
        let account = json!({"usage_forecast":forecast,"credits_in_use":true});
        for english in [false, true] {
            assert_eq!(
                list_forecast_lines(Some(&account), true, false, english),
                vec![qb_core::usage_forecast_display::summary(&forecast, english).text]
            );
        }
        let detail = list_forecast_lines(Some(&account), true, true, false);
        assert_eq!(detail, forecast_lines(Some(&account), true));
        assert_eq!(
            detail,
            qb_core::usage_forecast_display::lines(&forecast, true)
        );
        assert!(detail.iter().any(|line| line.contains("25.0%/시간")));
        assert!(detail.iter().any(|line| line.contains("크레딧")));
        for account in [None, Some(&json!({}))] {
            assert_eq!(
                list_forecast_lines(account, true, false, false),
                vec!["측정 중"]
            );
            assert_eq!(
                list_forecast_lines(account, false, false, true),
                vec!["Measuring"]
            );
        }
        assert!(list_forecast_lines(None, false, true, false)[0].contains("코어가 꺼져"));
    }

    #[test]
    fn r13_forecast_cli_handles_old_core_offline_and_shared_text() {
        assert!(forecast_lines(None, false)[0].contains("코어가 꺼져"));
        assert_eq!(
            forecast_lines(Some(&json!({})), true),
            vec!["소진 예측: 측정 중"]
        );
        let dto = qb_core::usage_forecast::Forecasts::default().view(
            "cx-123abc",
            std::time::SystemTime::now().into(),
            None,
        );
        let account = json!({"usage_forecast":dto,"credits_in_use":true});
        assert_eq!(
            forecast_lines(Some(&account), true),
            qb_core::usage_forecast_display::lines(&dto, true)
        );
    }

    #[test]
    fn r1_cpa_status_heading_shares_wire_note_online_and_offline() {
        let dir = scratch();
        for (label, mode, state) in [
            ("one", "linked", "active"),
            ("two", "linked", "active"),
            ("paused", "linked", "paused"),
            ("disabled", "linked", "disabled"),
            ("owned", "owned", "active"),
            ("adopted", "adopted", "active"),
        ] {
            account(dir.path(), label, mode, state, json!({"kind":"ready"}));
        }
        let _lease = AccountStore::acquire(dir.path()).unwrap();
        for running in [false, true] {
            let lines = status_heading(dir.path(), running).unwrap();
            assert_eq!(lines.len(), 2);
            assert_eq!(
                lines[0],
                if running {
                    "코어: 실행 중"
                } else {
                    "코어: 중지됨 (저장된 상태)"
                }
            );
            assert_eq!(
                lines[1],
                "CPA는 계속 실행되어야 합니다 (연결 계정 2개의 토큰 갱신)"
            );
        }
    }

    #[test]
    fn r1_cpa_status_heading_omits_note_without_active_linked_accounts() {
        let dir = scratch();
        for (label, mode, state) in [
            ("paused", "linked", "paused"),
            ("disabled", "linked", "disabled"),
            ("owned", "owned", "active"),
            ("adopted", "adopted", "active"),
        ] {
            account(dir.path(), label, mode, state, json!({"kind":"ready"}));
        }
        for running in [false, true] {
            assert_eq!(status_heading(dir.path(), running).unwrap().len(), 1);
        }
    }
}

#[cfg(test)]
#[path = "accounts_cli_r6_tests.rs"]
mod r6_tests;

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
mod r15_adoption_contract_tests {
    #[tokio::test]
    async fn cli_adoption_reader_preserves_the_entire_shared_reply() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let raw = br#"{"id":"sample-acct","generation":2,"from":"store","token_expires_at":null,"last_refresh_at":null,"warnings":["stored_tokens_may_need_relogin"]}"#;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let byte = socket.read_u8().await.unwrap();
                request.push(byte);
                assert!(request.len() < 8192);
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                raw.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(raw).await.unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}"))
            .send()
            .await
            .unwrap();
        let reply = super::adoption_response(response).await.unwrap();
        server.await.unwrap();
        assert_eq!(serde_json::to_vec(&reply).unwrap(), raw);
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
