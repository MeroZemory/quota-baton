use clap::Subcommand;
use std::{path::Path, time::Duration};

#[derive(Subcommand)]
pub enum RoutingCommand {
    #[command(about = "프로젝트와 루트 세션의 계정 전환 정책을 확인합니다")]
    List,
    #[command(about = "프로젝트 기본 정책을 등록하거나 수정합니다")]
    Project {
        name: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long,value_parser=["strict","recover","rebalance"])]
        mode: String,
        #[arg(long, default_value_t = 60)]
        reset_wait_secs: u64,
    },
    #[command(about = "루트 세션의 정책을 지정합니다. 자식은 루트를 따릅니다")]
    Session {
        id: String,
        #[arg(long,value_parser=["strict","recover","rebalance"],conflicts_with="inherit")]
        mode: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, requires = "mode")]
        reset_wait_secs: Option<u64>,
        #[arg(long, conflicts_with = "mode")]
        inherit: bool,
    },
}
pub async fn run(
    command: RoutingCommand,
    home: &Path,
    config: &qb_core::config::Config,
) -> anyhow::Result<()> {
    let secrets = qb_core::local_secrets::load_existing(home)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let url = format!(
        "http://127.0.0.1:{}/admin/v1/routing",
        config.server.admin_port
    );
    let body = match command {
        RoutingCommand::List => None,
        RoutingCommand::Project {
            name,
            id,
            mode,
            reset_wait_secs,
        } => Some(
            serde_json::json!({"kind":"project","id":id,"name":name,"settings":{"mode":mode,"reset_wait_secs":reset_wait_secs}}),
        ),
        RoutingCommand::Session {
            id,
            mode,
            project,
            reset_wait_secs,
            inherit,
        } => {
            if mode.is_none() && project.is_none() && !inherit {
                return Err(crate::accounts_cli::AccountCliError(
                    "--mode, --project 또는 --inherit를 지정해 주세요.",
                )
                .into());
            }
            let settings=mode.map(|mode|serde_json::json!({"mode":mode,"reset_wait_secs":reset_wait_secs.unwrap_or(config.routing.reset_hold_secs)}));
            Some(
                serde_json::json!({"kind":"session","id":id,"settings":settings,"project":project,"inherit":inherit}),
            )
        }
    };
    let response = if let Some(body) = body {
        client
            .post(url)
            .bearer_auth(&secrets.admin_token)
            .json(&body)
            .send()
            .await?
    } else {
        client
            .get(url)
            .bearer_auth(&secrets.admin_token)
            .send()
            .await?
    };
    if !response.status().is_success() {
        return Err(crate::accounts_cli::AccountCliError(
            "계정 전환 정책 요청이 거절되었습니다. 코어 버전과 식별자·대기 시간을 확인해 주세요.",
        )
        .into());
    }
    let value: serde_json::Value = response.json().await?;
    crate::print_stdout(format_args!("{}", serde_json::to_string_pretty(&value)?))?;
    Ok(())
}
