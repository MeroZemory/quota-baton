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
    Serve {
        #[cfg(feature = "shape-capture")]
        #[arg(long, help = "기존 .sandbox 폴더에 요청 구조를 기록합니다")]
        capture_dir: Option<std::path::PathBuf>,
        #[cfg(feature = "shape-capture")]
        #[arg(
            long,
            default_value_t = 50,
            requires = "capture_dir",
            help = "기록할 요청 수를 지정합니다"
        )]
        capture_limit: usize,
    },
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
        eprintln!("실패했습니다: {}", friendly_error(&error));
        std::process::exit(1);
    }
}

fn friendly_error(error: &anyhow::Error) -> String {
    use qb_core::accounts::StoreError;
    if let Some(error) = error.downcast_ref::<StoreError>() {
        return match error {
            StoreError::LeaseBusy => "다른 코어가 계정 저장소를 사용 중입니다.".into(),
            StoreError::GenerationConflict { .. } => {
                "계정이 변경되었습니다. 다시 시도해 주세요.".into()
            }
            StoreError::AdoptUnavailable => "소유권 이전은 M8 전환 단계에서만 가능합니다.".into(),
            _ => "계정 저장소를 확인해 주세요.".into(),
        };
    }
    if error.downcast_ref::<reqwest::Error>().is_some() {
        return "코어와 통신하지 못했습니다. 실행 상태와 관리 토큰을 확인해 주세요.".into();
    }
    let detail = error.to_string();
    if detail.contains("capture") {
        return "요청 구조를 기록할 폴더를 확인해 주세요. (capture-dir)".into();
    }
    if detail.contains("quota") {
        return "할당량 상태 파일을 확인해 주세요. (state/quota.json)".into();
    }
    if detail.contains("config") || detail.contains("upstream base") {
        return "설정 파일을 확인해 주세요. (config.toml)".into();
    }
    if detail.contains("M8") {
        return "소유권 이전은 M8 전환 단계에서만 가능합니다.".into();
    }
    "요청을 처리하지 못했습니다. 데이터 폴더와 설정을 확인해 주세요.".into()
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let home = qb_core::home()?;
    match cli.command {
        Command::Serve {
            #[cfg(feature = "shape-capture")]
            capture_dir,
            #[cfg(feature = "shape-capture")]
            capture_limit,
        } => {
            let _logs = qb_core::logging::init(&home)?;
            let config = qb_core::config::Config::load(&home)?;
            let store = std::sync::Arc::new(qb_core::accounts::AccountStore::acquire(&home)?);
            store.list()?;
            qb_core::set_process_priority()?;
            #[cfg(feature = "shape-capture")]
            let capture = capture_dir.map(|dir| qb_core::observability::CaptureOptions {
                dir,
                limit: capture_limit,
            });
            #[cfg(not(feature = "shape-capture"))]
            let capture = None;
            let observability =
                std::sync::Arc::new(qb_core::observability::Observability::new(&home, capture)?);
            let pins = std::sync::Arc::new(qb_core::debug_pin::DebugPins::default());
            let relay = std::sync::Arc::new(qb_core::relay::RelayState::new(
                store.clone(),
                &config,
                observability.clone(),
                pins.clone(),
            )?);
            let secrets = qb_core::local_secrets::load_or_create(&home)?;
            let servers = qb_core::http::spawn_with_routes(
                &config,
                secrets,
                qb_core::relay::router(relay),
                qb_core::admin::account_routes(store.clone())
                    .merge(qb_core::debug_pin::routes(store, pins)),
            )
            .await?;
            println!("프록시를 시작했습니다.");
            tokio::signal::ctrl_c().await?;
            drop(servers);
            observability.flush()?;
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
