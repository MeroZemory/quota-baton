use clap::{Args, Subcommand};
use qb_core::{accounts::AccountStore, config::Config};
use qb_proto::{CpaImportRequest, ImportMode};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Subcommand)]
pub enum AccountsCommand {
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
    #[command(about = "CPA 계정을 가져옵니다")]
    Import {
        #[command(subcommand)]
        source: ImportSource,
    },
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
        AccountsCommand::Pause { id } => action(home, config, &id, "pause").await?,
        AccountsCommand::Resume { id } => action(home, config, &id, "resume").await?,
        AccountsCommand::Disable { id } => action(home, config, &id, "disable").await?,
        AccountsCommand::Enable { id } => action(home, config, &id, "enable").await?,
        AccountsCommand::List => {
            let accounts = AccountStore::list_metadata(home)?;
            println!(
                "계정 ID\t제공자\t이메일\t요금제\t출처\t연결 방식\t사용 상태\t계정 상태\t만료 시각"
            );
            for account in accounts {
                let value = serde_json::to_value(account)?;
                println!(
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
            println!("계정 ID\t제공자\t이메일\t요금제\t만료 시각\t사용 상태\t결과\t이유");
            for row in rows
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Invalid import response"))?
            {
                println!(
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
    println!(
        "계정 {}: {}",
        cell(&serde_json::json!(id)),
        translated(&serde_json::to_value(state)?)
    );
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
                serde_json::json!({
                    "id": account.id, "provider": account.provider, "label": account.label,
                    "email": account.email, "plan": account.plan, "user_state": account.user_state,
                    "health": account.health, "windows": snapshot["windows"]
                })
            }).collect())
            })
            .await??;
        (false, accounts)
    };
    println!(
        "코어: {}",
        if running {
            "실행 중"
        } else {
            "중지됨 (저장된 상태)"
        }
    );
    println!(
        "계정 ID\t제공자\t라벨\t사용 상태\t계정 상태\t5시간 사용률\t5시간 초기화\t7일 사용률\t7일 초기화\t상태 이유\t대기 종료"
    );
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
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            cell(&account["id"]),
            cell(&account["provider"]),
            cell(&account["label"]),
            translated(&account["user_state"]),
            health_label(&account["health"]),
            percentage(&five["used_pct"]),
            cell(&five["reset_at"]),
            percentage(&seven["used_pct"]),
            cell(&seven["reset_at"]),
            translated(&account["health"]["reason"]),
            cell(&account["health"]["until"])
        );
    }
    Ok(())
}

fn percentage(value: &serde_json::Value) -> String {
    value
        .as_f64()
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "-".into())
}
