use clap::Subcommand;
use std::{path::Path, time::Duration};

#[derive(Subcommand)]
pub enum RoutingCommand {
    #[command(about = "프로젝트와 루트 세션의 계정 전환 정책을 확인합니다")]
    List,
    #[command(
        about = "프로젝트 기본 정책을 등록하거나 수정합니다",
        long_about = "새로 연결할 세션의 프로젝트 기본 정책을 등록하거나 수정합니다. 이미 연결된 세션은 저장된 기존 기본값을 유지합니다."
    )]
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
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let url = format!(
        "http://127.0.0.1:{}/admin/v1/routing",
        config.server.admin_port
    );
    let inherited_wait = if let RoutingCommand::Session {
        id,
        mode: Some(_),
        reset_wait_secs: None,
        ..
    } = &command
    {
        let response = client
            .get(&url)
            .bearer_auth(&secrets.admin_token)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(crate::accounts_cli::AccountCliError(
                "기존 세션 정책을 조회하지 못했습니다.",
            )
            .into());
        }
        let value: serde_json::Value = response.json().await?;
        Some(
            current_wait(&value, id).ok_or(crate::accounts_cli::AccountCliError(
                "현재 세션 대기 시간을 확인하지 못했습니다.",
            ))?,
        )
    } else {
        None
    };
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
            let settings=mode.map(|mode|serde_json::json!({"mode":mode,"reset_wait_secs":reset_wait_secs.or(inherited_wait).unwrap_or(config.routing.reset_hold_secs)}));
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

fn current_wait(value: &serde_json::Value, id: &str) -> Option<u64> {
    value["sessions"][id]["settings"]["reset_wait_secs"]
        .as_u64()
        .or_else(|| {
            value["observed_sessions"]
                .as_array()?
                .iter()
                .find(|s| s["id"] == id)?["policy"]["settings"]["reset_wait_secs"]
                .as_u64()
        })
        .or_else(|| value["defaults"]["reset_wait_secs"].as_u64())
        .filter(|n| *n <= 120)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mode_edit_uses_running_root_wait_then_observed_policy_then_running_default() {
        let mut view = serde_json::json!({"defaults":{"reset_wait_secs":13},"sessions":{"synthetic":{"settings":{"reset_wait_secs":7}}},"observed_sessions":[{"id":"observed","policy":{"settings":{"reset_wait_secs":0}}}]});
        assert_eq!(current_wait(&view, "synthetic"), Some(7));
        assert_eq!(current_wait(&view, "observed"), Some(0));
        assert_eq!(current_wait(&view, "new"), Some(13));
        view["defaults"]["reset_wait_secs"] = 900.into();
        assert_eq!(current_wait(&view, "new"), None);
    }
}
