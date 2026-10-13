use clap::{Parser, Subcommand};
mod accounts_cli;

#[derive(Parser)]
#[command(version, about = "로컬 계정 프록시")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "프록시를 시작합니다")]
    Serve,
    #[command(about = "상태를 확인합니다")]
    Status,
    #[command(about = "계정을 관리합니다")]
    Accounts {
        #[command(subcommand)]
        command: accounts_cli::AccountsCommand,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("실패했습니다: {error}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let home = qb_core::home()?;
    match cli.command {
        Command::Serve => {
            let _logs = qb_core::logging::init(&home)?;
            let config = qb_core::config::Config::load(&home)?;
            let store = std::sync::Arc::new(qb_core::accounts::AccountStore::acquire(&home)?);
            let secrets = qb_core::local_secrets::load_or_create(&home)?;
            let _servers = qb_core::http::spawn_with_admin(
                &config,
                secrets,
                qb_core::admin::account_routes(store),
            )
            .await?;
            println!("프록시를 시작했습니다.");
            tokio::signal::ctrl_c().await?;
        }
        Command::Status => {
            let config = qb_core::config::Config::load(&home)?;
            let token = std::fs::read_to_string(home.join("admin.token"))?;
            let health: qb_proto::HealthResponse = reqwest::Client::builder()
                .no_proxy()
                .build()?
                .get(format!(
                    "http://127.0.0.1:{}/admin/v1/health",
                    config.server.admin_port
                ))
                .bearer_auth(token.trim())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&health)?);
        }
        Command::Accounts { command } => {
            let config = qb_core::config::Config::load(&home)?;
            accounts_cli::run(command, &home, &config).await?;
        }
    }
    Ok(())
}
