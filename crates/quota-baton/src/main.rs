use clap::{Parser, Subcommand};

macro_rules! println_utf8 {
    ($($arg:tt)*) => {
        crate::print_stdout(format_args!($($arg)*))?
    };
}

mod accounts_cli;

fn print_stdout(args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
    use std::io::{IsTerminal, Write};
    let stdout = std::io::stdout();
    if stdout.is_terminal() {
        // Keep the existing console path and its UTF-8 console setup.
        println!("{args}");
        return Ok(());
    }
    let line = format!("{args}\n");
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        // A pipe/file receives the UTF-8 bytes directly, without console encoding.
        let mut output = std::fs::File::from(stdout.as_handle().try_clone_to_owned()?);
        output.write_all(line.as_bytes())
    }
    #[cfg(not(windows))]
    stdout.lock().write_all(line.as_bytes())
}

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
        #[cfg(feature = "phase0")]
        #[arg(long, help = "기존 .sandbox 폴더에 합성 대화 본문을 기록합니다")]
        capture_bodies_dir: Option<std::path::PathBuf>,
        #[cfg(feature = "phase0")]
        #[arg(long, default_value_t = 20, requires = "capture_bodies_dir")]
        capture_bodies_limit: usize,
        #[cfg(feature = "phase0")]
        #[arg(long, help = "응답 완료 후 소켓을 닫아 합성 이식성 검사를 진행합니다")]
        phase0_close_after_response: bool,
    },
    #[command(about = "상태를 확인합니다")]
    Status,
    #[command(about = "계정을 관리합니다")]
    Accounts {
        #[command(subcommand)]
        command: accounts_cli::AccountsCommand,
    },
    #[cfg(feature = "phase0")]
    #[command(about = "디버그 캡처를 재생합니다")]
    Phase0 {
        #[command(subcommand)]
        command: Phase0Command,
    },
}

#[cfg(feature = "phase0")]
#[derive(Subcommand)]
enum Phase0Command {
    Replay {
        #[arg(long)]
        capture: std::path::PathBuf,
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "none", value_parser = ["none", "reasoning", "all"])]
        strip: String,
        #[arg(long)]
        tamper_encrypted: bool,
    },
}

#[tokio::main]
async fn main() {
    qb_core::set_console_utf8();
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
    if detail.contains("websocket.upstream_deflate") {
        return "WebSocket upstream_deflate=true는 지원되지 않습니다. deflate 지원 라이브러리를 채택하기 전에는 false로 설정해 주세요.".into();
    }
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
            #[cfg(feature = "phase0")]
            capture_bodies_dir,
            #[cfg(feature = "phase0")]
            capture_bodies_limit,
            #[cfg(feature = "phase0")]
            phase0_close_after_response,
        } => {
            let _logs = qb_core::logging::init(&home)?;
            let config = qb_core::config::Config::load(&home)?;
            let account_home = home.clone();
            let store = std::sync::Arc::new(
                tokio::task::spawn_blocking(move || {
                    qb_core::accounts::AccountStore::acquire(&account_home)
                })
                .await??,
            );
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
            let relay = qb_core::relay::RelayState::new(
                store.clone(),
                &config,
                observability.clone(),
                pins.clone(),
            )?;
            #[cfg(feature = "phase0")]
            let relay = if let Some(dir) = capture_bodies_dir {
                let capture = qb_core::phase0::BodyCapture::new(dir, capture_bodies_limit)?;
                eprintln!(
                    "경고: 본문 캡처에는 대화 내용이 포함됩니다. 합성 대화만 사용하고 검증 후 파일을 삭제하세요."
                );
                relay.with_body_capture(std::sync::Arc::new(capture))
            } else {
                relay
            };
            #[cfg(feature = "phase0")]
            let relay = relay.with_close_after_response(phase0_close_after_response);
            let relay = std::sync::Arc::new(relay);
            relay
                .accounts
                .initialize_settings(config.accounts.clone())
                .await
                .map_err(|_| anyhow::anyhow!("계정 설정을 저장하지 못했습니다."))?;
            let admin_routes = qb_core::admin::account_routes(store.clone())
                .merge(qb_core::admin::runtime_routes(
                    relay.clone(),
                    config.server.data_port,
                ))
                .merge(qb_core::debug_pin::routes(store, pins));
            #[cfg(feature = "phase0")]
            let admin_routes = admin_routes.merge(qb_core::relay::phase0_routes(relay.clone()));
            let secrets = qb_core::local_secrets::load_or_create(&home)?;
            let servers = qb_core::http::spawn_with_routes(
                &config,
                secrets,
                qb_core::relay::router(relay),
                admin_routes,
            )
            .await?;
            println_utf8!("프록시를 시작했습니다.");
            tokio::signal::ctrl_c().await?;
            drop(servers);
            // 종료 시 디스크 flush와 작성자 정리도 런타임 스레드를 막지 않는다.
            tokio::task::spawn_blocking(move || observability.flush()).await??;
        }
        Command::Status => {
            let config = qb_core::config::Config::load(&home)?;
            accounts_cli::status(&home, &config).await?;
        }
        Command::Accounts { command } => {
            let config = qb_core::config::Config::load(&home)?;
            accounts_cli::run(command, &home, &config).await?;
        }
        #[cfg(feature = "phase0")]
        Command::Phase0 {
            command:
                Phase0Command::Replay {
                    capture,
                    account,
                    strip,
                    tamper_encrypted,
                },
        } => {
            let config = qb_core::config::Config::load(&home)?;
            let capture = capture
                .canonicalize()
                .map_err(|_| anyhow::anyhow!("Capture path must exist"))?;
            let provider = if account.starts_with("cl-") {
                qb_core::accounts::Provider::Claude
            } else if account.starts_with("cx-") {
                qb_core::accounts::Provider::Codex
            } else {
                anyhow::bail!("Invalid phase0 account");
            };
            let capture_path = capture.clone();
            let strip_encrypted: qb_core::phase0::StripMode =
                serde_json::from_value(serde_json::json!(strip))?;
            let prepared = tokio::task::spawn_blocking(move || {
                qb_core::phase0::prepare(&capture_path, provider, strip_encrypted, tamper_encrypted)
            })
            .await??;
            // 네트워크 전송 전에 출력하는 것은 개수뿐이다.
            println_utf8!("{}", serde_json::to_string(&prepared.presence)?);
            drop(prepared);
            let token = std::fs::read_to_string(home.join("admin.token"))?;
            let summary: serde_json::Value = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(150))
                .build()?
                .post(format!(
                    "http://127.0.0.1:{}/admin/v1/phase0/replay",
                    config.server.admin_port
                ))
                .bearer_auth(token.trim())
                .json(&serde_json::json!({"capture_path":capture,"account_id":account,"strip_encrypted":strip_encrypted,"tamper_encrypted":tamper_encrypted}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println_utf8!("{}", serde_json::to_string(&summary)?);
        }
    }
    Ok(())
}
