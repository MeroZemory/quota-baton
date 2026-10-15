use clap::{Parser, Subcommand};

macro_rules! println_utf8 {
    ($($arg:tt)*) => {
        crate::print_stdout(format_args!($($arg)*))?
    };
}

mod accounts_cli;
mod cutover;

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
        #[arg(long, help = "실행 중인 코어를 drain한 뒤 계정 lease를 넘겨받습니다")]
        takeover: bool,
    },
    #[command(about = "상태를 확인합니다")]
    Status,
    #[command(about = "계정을 관리합니다")]
    Accounts {
        #[command(subcommand)]
        command: accounts_cli::AccountsCommand,
    },
    #[command(about = "클라이언트 설정을 연결하거나 되돌립니다")]
    Wire {
        #[arg(long, help = "실제 파일을 변경합니다; 생략하면 미리보기입니다")]
        apply: bool,
        #[arg(long, help = "현재 사용자 트레이 자동 시작을 연결합니다")]
        autostart: bool,
        #[arg(long, help = "CPA 예약 작업을 삭제하지 않고 비활성화합니다")]
        disable_cpa_tasks: bool,
    },
    #[command(about = "클라이언트 설정 연결을 되돌립니다")]
    Unwire {
        #[arg(long, help = "실제 파일을 변경합니다; 생략하면 미리보기입니다")]
        apply: bool,
        #[arg(long, help = "사용자 변경 감지를 무시하고 되돌립니다")]
        force: bool,
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
        let message = friendly_error(&error);
        if message == "로그인 시간이 만료되었습니다. 다시 시작해 주세요." {
            eprintln!("{message}");
        } else {
            eprintln!("실패했습니다: {message}");
        }
        std::process::exit(1);
    }
}

fn friendly_error(error: &anyhow::Error) -> String {
    if error.to_string().contains("로그인 시간이 만료되었습니다") {
        return "로그인 시간이 만료되었습니다. 다시 시작해 주세요.".into();
    }
    if let Some(error) = error.downcast_ref::<accounts_cli::UsageCliError>() {
        return error.to_string();
    }
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
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse)
    }) {
        return "포트 인계 대기 시간(2초)이 지났습니다. 포트를 사용하는 프로세스를 확인해 주세요."
            .into();
    }
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

async fn request_takeover(
    home: &std::path::Path,
    config: &qb_core::config::Config,
) -> anyhow::Result<serde_json::Value> {
    let secrets = qb_core::local_secrets::load_or_create(home)?;
    let port = config.server.admin_port;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(35))
        .build()?;
    let response = client
        .post(format!("http://127.0.0.1:{port}/admin/v1/lifecycle/drain"))
        .header(reqwest::header::HOST, format!("127.0.0.1:{port}"))
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", secrets.admin_token),
        )
        .json(&serde_json::json!({"drain_timeout_secs": config.server.drain_timeout_secs}))
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("running core refused takeover ({})", response.status());
    }
    Ok(response.json().await?)
}

async fn check_wire_preconditions(home: &std::path::Path) -> anyhow::Result<()> {
    let config = qb_core::config::Config::load(home)?;
    let secrets = qb_core::local_secrets::load_or_create(home)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .tcp_nodelay(true)
        .timeout(std::time::Duration::from_secs(3))
        .build()?;
    let port = config.server.admin_port;
    let response = client
        .get(format!("http://127.0.0.1:{port}/admin/v1/health"))
        .header(reqwest::header::HOST, format!("127.0.0.1:{port}"))
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", secrets.admin_token),
        )
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("적용하려면 정상 상태의 코어가 실행 중이어야 합니다."))?;
    if !response.status().is_success() {
        anyhow::bail!("적용하려면 정상 상태의 코어가 실행 중이어야 합니다.");
    }
    let accounts = qb_core::accounts::AccountStore::list_metadata(home)?;
    for provider in [
        qb_core::accounts::Provider::Codex,
        qb_core::accounts::Provider::Claude,
    ] {
        if !accounts
            .iter()
            .any(|account| account.provider == provider && account.eligible(None))
        {
            anyhow::bail!("각 프로바이더에 적용 가능한 활성 계정이 하나 이상 필요합니다.");
        }
    }
    Ok(())
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
            takeover,
        } => {
            let _logs = qb_core::logging::init(&home)?;
            let config = qb_core::config::Config::load(&home)?;
            let mut imported_bindings = None;
            let started = std::time::Instant::now();
            let store = loop {
                let account_home = home.clone();
                match tokio::task::spawn_blocking(move || {
                    qb_core::accounts::AccountStore::acquire(&account_home)
                })
                .await?
                {
                    Ok(store) => break std::sync::Arc::new(store),
                    Err(qb_core::accounts::StoreError::LeaseBusy)
                        if takeover && started.elapsed() < std::time::Duration::from_secs(2) =>
                    {
                        if imported_bindings.is_none() {
                            imported_bindings = Some(request_takeover(&home, &config).await?);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                    Err(error) => return Err(error.into()),
                }
            };
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
            if takeover {
                observability.record_takeover("lease_acquired");
            }
            let pins = std::sync::Arc::new(qb_core::debug_pin::DebugPins::default());
            let relay = qb_core::relay::RelayState::new(
                store.clone(),
                &config,
                observability.clone(),
                pins.clone(),
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
            relay.set_drain_timeout(config.server.drain_timeout_secs);
            if let Some(bindings) = imported_bindings {
                let value = serde_json::json!({
                    "schema": 1,
                    "bindings": bindings
                        .get("bindings")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                });
                relay
                    .bindings
                    .import(value)
                    .map_err(|_| anyhow::anyhow!("인계된 세션 바인딩을 읽지 못했습니다."))?;
                observability.record_takeover("bindings_imported");
            }
            relay
                .accounts
                .initialize_settings(config.accounts.clone())
                .await
                .map_err(|_| anyhow::anyhow!("계정 설정을 저장하지 못했습니다."))?;
            relay
                .quota
                .startup_reload()
                .await
                .map_err(|_| anyhow::anyhow!("연결된 계정의 토큰을 다시 읽지 못했습니다."))?;
            let login = relay.login_service();
            let admin_routes = qb_core::admin::account_routes(store.clone())
                .merge(qb_core::admin::login_routes(login.clone()))
                .merge(qb_core::admin::runtime_routes(
                    relay.clone(),
                    config.server.data_port,
                ))
                .merge(qb_core::debug_pin::routes(store, pins));
            #[cfg(feature = "phase0")]
            let admin_routes = admin_routes.merge(qb_core::relay::phase0_routes(relay.clone()));
            let secrets = qb_core::local_secrets::load_or_create(&home)?;
            let mut servers = qb_core::http::spawn_with_routes(
                &config,
                secrets,
                qb_core::relay::router(relay.clone()),
                admin_routes,
            )
            .await?;
            if takeover {
                observability.record_takeover("listening");
            }
            println_utf8!("프록시를 시작했습니다.");
            let polling = relay.quota.start();
            let refreshing = relay.start_refresh_scheduler();
            let memory_guard = relay.start_memory_guard();
            tokio::select! {
                result = tokio::signal::ctrl_c() => { result?; }
                _ = relay.wait_for_drain() => {}
            }
            polling.abort();
            refreshing.abort();
            memory_guard.abort();
            let _ = polling.await;
            let _ = refreshing.await;
            let _ = memory_guard.await;
            // Stop accepting new connections before handing the credential
            // lease to the successor. Existing handlers remain alive under
            // axum's graceful-shutdown drain.
            servers.shutdown();
            servers.wait_for_listener_close().await;
            if relay.is_draining() {
                relay.release_lease_after_drain();
            }
            let drain_deadline = relay.drain_timeout();
            let _ = tokio::time::timeout(drain_deadline, async {
                let _ = tokio::join!(&mut servers.data_task, &mut servers.admin_task);
                while relay
                    .inflight_requests
                    .load(std::sync::atomic::Ordering::Acquire)
                    > 0
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
            drop(servers);
            login.cancel_all().await;
            // The lease must outlive detached token consumption and durable writes.
            while relay.shutdown_refreshes().await.is_err() {
                println_utf8!("종료 중 토큰 저장 작업이 끝나기를 기다립니다.");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            // 종료 시 디스크 flush와 작성자 정리도 런타임 스레드를 막지 않는다.
            tokio::task::spawn_blocking(move || {
                if relay.is_draining() {
                    observability.record_takeover("exited");
                }
                relay.bindings.flush()?;
                observability.flush()
            })
            .await??;
        }
        Command::Status => {
            let config = qb_core::config::Config::load(&home)?;
            accounts_cli::status(&home, &config).await?;
        }
        Command::Accounts { command } => {
            let config = qb_core::config::Config::load(&home)?;
            accounts_cli::run(command, &home, &config).await?;
        }
        Command::Wire {
            apply,
            autostart,
            disable_cpa_tasks,
        } => {
            if apply {
                check_wire_preconditions(&home).await?;
            }
            let paths = cutover::CutoverPaths::from_environment(home)?;
            let report = cutover::wire_with_options(
                &paths,
                apply,
                cutover::WireOptions {
                    autostart,
                    disable_cpa_tasks,
                },
            )?;
            println_utf8!(
                "{} {}",
                if apply {
                    "적용했습니다:"
                } else {
                    "미리보기:"
                },
                report.changed.join(", ")
            );
            for warning in report.warnings {
                println_utf8!("경고: {warning}");
            }
            for line in report.diff {
                println_utf8!("{line}");
            }
            for item in report.report_only {
                println_utf8!("보고: {item}");
            }
        }
        Command::Unwire { apply, force } => {
            let paths = cutover::CutoverPaths::from_environment(home)?;
            let report = cutover::unwire(&paths, apply, force)?;
            println_utf8!(
                "{} {}",
                if apply {
                    "되돌렸습니다:"
                } else {
                    "미리보기:"
                },
                report.changed.join(", ")
            );
            for warning in report.warnings {
                println_utf8!("경고: {warning}");
            }
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
