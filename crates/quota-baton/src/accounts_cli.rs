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
        other => other.to_string(),
    };
    text.replace(['\t', '\r', '\n'], " ")
}
