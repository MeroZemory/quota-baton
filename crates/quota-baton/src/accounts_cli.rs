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
        AccountsCommand::List => {
            let accounts = AccountStore::list_metadata(home)?;
            let forecasts = live_account_views(home, config).await?;
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
                let live = forecasts
                    .as_ref()
                    .and_then(|rows| rows.iter().find(|row| row["id"] == value["id"]));
                for line in forecast_lines(live, forecasts.is_some()) {
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
                    admin_route_response(response, &client, &base, token.as_deref().unwrap_or(""))
                        .await?
                        .error_for_status()?
                        .json::<serde_json::Value>()
                        .await?
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
    let inspect = account_response(
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
    let result = account_response(
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

async fn account_response(response: reqwest::Response) -> anyhow::Result<serde_json::Value> {
    let status = response.status();
    let value: serde_json::Value = response.json().await.unwrap_or_default();
    // Only allowlisted local error codes are rendered; never echo arbitrary payloads.
    if !status.is_success() {
        let message = match value["error"]["code"].as_str() {
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
        };
        return Err(account_error(message));
    }
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
        let result = if let Some((client, base, token)) = &admin {
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
            account_response(
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
            serde_json::to_value(
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
                .map_err(adoption_error)?,
            )?
        };
        let origin = match result["from"].as_str() {
            Some("cpa_file") => "CPA 파일",
            Some("backup") => "백업",
            _ => "저장된 토큰",
        };
        println_utf8!(
            "{}\t직접 갱신으로 전환\t{}\t{}",
            id,
            origin,
            cell(&result["token_expires_at"])
        );
        for warning in result["warnings"].as_array().into_iter().flatten() {
            let message = match warning.as_str() {
                Some("cpa_file_missing") => "CPA 파일이 없어 다른 자료를 사용했습니다.",
                Some("cpa_file_unreadable") => {
                    "읽을 수 없는 CPA 파일이 있어 다른 자료를 사용했습니다."
                }
                Some("backup_missing" | "backup_unreadable") => {
                    "백업을 읽을 수 없어 저장된 자료를 사용했습니다."
                }
                Some("older_or_unordered_candidate_ignored") => {
                    "오래되었거나 시각을 비교할 수 없는 토큰은 사용하지 않았습니다."
                }
                Some("email_updated") => "같은 계정의 이메일 표시를 갱신했습니다.",
                Some("stored_tokens_may_need_relogin") => {
                    "저장된 토큰의 만료 또는 불확실 상태를 확인해 주세요."
                }
                _ => continue,
            };
            println_utf8!("계정 {}: {}", id, message);
        }
    }
    Ok(())
}

async fn refresh_tokens(
    home: &Path,
    config: &Config,
    id: String,
    force: bool,
) -> anyhow::Result<()> {
    valid_account_id(&id)?;
    let summary = if let Some((client, base, token)) = running_admin(home, config).await? {
        account_response(
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
        let result = relay.refresh_account(id.clone(), force).await;
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
    if summary["outcome"] == "success" {
        println_utf8!(
            "계정 {}: 갱신 완료 · 만료 {} · 최근 갱신 {}",
            id,
            cell(&summary["token_expires_at"]),
            cell(&summary["last_refresh_at"])
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
        admin_route_response(
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
    account_response(
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
    response.error_for_status()?;
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

async fn live_account_views(
    home: &Path,
    config: &Config,
) -> anyhow::Result<Option<Vec<serde_json::Value>>> {
    let Some((client, base, token)) = running_admin(home, config).await? else {
        return Ok(None);
    };
    let state: serde_json::Value = admin_route_response(
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
    .json()
    .await?;
    Ok(Some(
        state["accounts"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Invalid account state"))?
            .clone(),
    ))
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

impl LoginBackend {
    async fn status(&self, id: &str) -> anyhow::Result<serde_json::Value> {
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
