use clap::{Parser, Subcommand};

macro_rules! println_utf8 {
    ($($arg:tt)*) => {
        crate::print_stdout(format_args!($($arg)*))?
    };
}

#[allow(dead_code)] // Compatibility fields preserve the existing tray decoder.
mod account_display;
mod accounts_cli;

mod debug_snapshot;

mod platform;
mod routing_cli;
#[allow(unused_imports)] // Windows feature roots retain their existing crate:: paths.
use platform::*;

fn print_stdout(args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
    use std::io::IsTerminal;
    let stdout = std::io::stdout();
    if stdout.is_terminal() {
        // Keep the existing console path and its UTF-8 console setup.
        println!("{args}");
        return Ok(());
    }
    let line = format!("{args}\n");
    platform::write_stdout(&stdout, line.as_bytes())
}

#[derive(Parser)]
#[command(version, about = "로컬 계정 프록시")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Platform(platform::PlatformCommand),
    #[command(about = "Export diagnostic display data")]
    #[command(display_order = 0)]
    Debug {
        #[command(subcommand)]
        command: debug_snapshot::DebugCommand,
    },

    #[command(about = "기존 Codex 대화를 가져오거나 되돌립니다")]
    #[command(display_order = 2)]
    Sessions {
        #[command(subcommand)]
        command: platform::SessionsCommand,
    },
    #[command(about = "프로젝트·세션별 계정 전환을 설정합니다")]
    #[command(display_order = 3)]
    Routing {
        #[command(subcommand)]
        command: routing_cli::RoutingCommand,
    },

    #[command(about = "프록시를 시작합니다")]
    #[command(display_order = 5)]
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
    #[command(display_order = 6)]
    Status {
        #[command(flatten)]
        options: platform::StatusOptions,
    },
    #[command(about = "계정을 관리합니다")]
    #[command(display_order = 7)]
    Accounts {
        #[command(subcommand)]
        command: accounts_cli::AccountsCommand,
    },

    #[cfg(feature = "phase0")]
    #[command(about = "디버그 캡처를 재생합니다")]
    #[command(display_order = 10)]
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

fn main() {
    if qb_core::service::prepare_process().is_err() {
        eprintln!("실패했습니다: 서비스 신호 초기화를 완료하지 못했습니다.");
        std::process::exit(1);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed building the Runtime");
    let status = runtime.block_on(main_async());
    drop(runtime);
    let finished = qb_core::service::finish_process();
    if status != 0 {
        std::process::exit(status);
    }
    if finished.is_err() {
        eprintln!("실패했습니다: 서비스 교체를 완료하지 못했습니다.");
        std::process::exit(1);
    }
}

async fn main_async() -> i32 {
    qb_core::set_console_utf8();
    if let Err(error) = run().await {
        if error
            .downcast_ref::<accounts_cli::TakeoverInspectOnly>()
            .is_some()
        {
            eprintln!("{error}");
            qb_core::service::defer_error_exit(2);
            return 2;
        }
        let message = friendly_error(&error);
        if message == "로그인 시간이 만료되었습니다. 다시 시작해 주세요." {
            eprintln!("{message}");
        } else {
            eprintln!("실패했습니다: {message}");
        }
        platform::keep_launch_error_visible();
        qb_core::service::defer_error_exit(1);
        return 1;
    }
    0
}

fn friendly_error(error: &anyhow::Error) -> String {
    if let Some(message) = platform::friendly_error(error) {
        return message;
    }
    // Only exact fixed diagnostics may pass through; parser/transport chains
    // and appended file contents must stay behind the generic safe error.
    const WIRE_DIAGNOSTICS: &[&str] = &[
        "전용 파일이 변경되었습니다. 같은 파일의 내용 변경만 --force로 허용할 수 있습니다",
        "업그레이드 파일 identity/내용이 변경되었습니다. journal을 보존합니다",
        "부분 해제 journal입니다. unwire를 먼저 완료하세요",
        "PATH에서 안전한 Claude 실행 파일을 찾지 못했습니다. Claude 설치를 확인하세요",
        "quota-baton 실행 파일 경로를 확인해 주세요",
        "Claude 전용 설정 디렉터리 접근 권한을 제한하지 못했습니다",
        "npm 디렉터리가 없습니다. Claude 설치를 확인하세요",
        "전용 Claude 홈이 이미 존재합니다. 덮어쓰지 않습니다",
        "journal의 Claude 모드가 다릅니다. 먼저 unwire를 실행하세요",
        "Claude 실행 파일이 변경되었습니다. 먼저 unwire를 실행하세요",
        "Codex와 Claude를 모두 연결 안 함으로 선택할 수 없습니다",
        "적용하려면 client.key와 admin.token이 먼저 있어야 합니다.",
        "적용하려면 정상 상태의 코어가 실행 중이어야 합니다.",
        "설정된 데이터 포트에 연결할 수 없습니다.",
        "각 프로바이더에 적용 가능한 활성 계정이 하나 이상 필요합니다.",
        "npm 디렉터리 또는 codex.cmd가 없습니다. Codex 설치를 확인하세요",
        "전용 Codex 홈이 이미 존재합니다. 덮어쓰지 않습니다",
        "Codex config 형식이 올바르지 않습니다",
        "Codex auth.json 형식이 올바르지 않습니다",
        "선택된 비 CPA model_provider는 별도 홈 모드와 함께 사용할 수 없습니다",
        "선택된 사용자 정의 model_provider가 별도 홈 연결을 우회할 수 있어 중단했습니다",
        "journal의 Codex 모드가 다릅니다. 먼저 unwire를 실행하세요",
    ];
    let detail = error.to_string();
    if WIRE_DIAGNOSTICS.contains(&detail.as_str()) {
        return detail;
    }
    if error.to_string().contains("로그인 시간이 만료되었습니다") {
        return "로그인 시간이 만료되었습니다. 다시 시작해 주세요.".into();
    }
    if let Some(error) = error.downcast_ref::<accounts_cli::UsageCliError>() {
        return error.to_string();
    }
    if let Some(error) = error.downcast_ref::<accounts_cli::AdminVersionMismatch>() {
        return error.to_string();
    }
    if let Some(error) = error.downcast_ref::<accounts_cli::AccountCliError>() {
        return error.to_string();
    }
    if let Some(error) = error.downcast_ref::<qb_core::relay::ManualRefreshError>() {
        return error.to_string();
    }
    use qb_core::accounts::StoreError;
    if let Some(error) = error.downcast_ref::<StoreError>() {
        return match error {
            StoreError::LeaseBusy => "다른 코어가 계정 저장소를 사용 중입니다.".into(),
            StoreError::GenerationConflict { .. } => {
                "계정이 변경되었습니다. 다시 시도해 주세요.".into()
            }
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
        anyhow::bail!(
            "실행 중인 코어가 takeover를 거부했습니다 ({})",
            response.status()
        );
    }
    Ok(response.json().await?)
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Command::Platform(command) = cli.command {
        return platform::run(command).await;
    }
    let home = qb_core::home()?;
    match cli.command {
        Command::Debug { command } => debug_snapshot::run(&home, command).await?,
        Command::Platform(_) => unreachable!("platform commands handled before home resolution"),
        Command::Sessions { command } => platform::run_sessions(command, &home).await?,
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
            #[cfg(all(feature = "testkit", debug_assertions))]
            match (
                std::env::var_os("QB_TEST_TAKEOVER_HOME"),
                std::env::var("QB_TEST_TAKEOVER_PROFILE"),
            ) {
                (None, Err(std::env::VarError::NotPresent)) => {}
                (Some(root), Ok(endpoint)) => {
                    qb_core::relay::RelayState::validate_takeover_test_inputs(
                        &home,
                        &std::path::PathBuf::from(root),
                        &endpoint,
                    )?
                }
                _ => anyhow::bail!("합성 인계 검사의 홈과 프로필 주소를 함께 지정하세요."),
            }
            let config = qb_core::config::Config::load(&home)?;
            if qb_core::service::handle_takeover(&home, &config, takeover).await? {
                return Ok(());
            }
            qb_core::service::initialize_listeners(&config)?;
            let _logs = qb_core::logging::init(&home)?;
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
            qb_core::service::retain_store_lease(&store)?;
            let mut box_service = qb_core::service::BoxServiceOwner::prepare(&home, &config)?;
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
            qb_core::service::retain_observability(observability.clone())?;
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
            // Normal product wiring is explicit and lookup remains lazy. A
            // synthetic home must never cause enumeration of the real profile,
            // even when the spawned binary has its default features.
            let synthetic_home = home
                .components()
                .any(|part| matches!(part,std::path::Component::Normal(name) if name==".sandbox"));
            if config.dashboard.resolve_claude_project
                && !cfg!(feature = "testkit")
                && !synthetic_home
            {
                let projects = std::env::var_os("USERPROFILE")
                    .map(std::path::PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|path| path.join(".claude").join("projects"));
                relay
                    .dashboard()
                    .configure_claude_project_root(projects)
                    .map_err(anyhow::Error::msg)?;
            }
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
            #[cfg(all(feature = "testkit", debug_assertions))]
            let relay = match (
                std::env::var_os("QB_TEST_TAKEOVER_HOME"),
                std::env::var("QB_TEST_TAKEOVER_PROFILE"),
            ) {
                (None, Err(std::env::VarError::NotPresent)) => relay,
                (Some(root), Ok(endpoint)) => {
                    relay.with_takeover_test_source(&std::path::PathBuf::from(root), endpoint)?
                }
                _ => anyhow::bail!("합성 인계 검사의 홈과 프로필 주소를 함께 지정하세요."),
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
                let bindings = relay.bindings.clone();
                tokio::task::spawn_blocking(move || bindings.import(value))
                    .await
                    .map_err(|_| anyhow::anyhow!("인계된 세션 바인딩 복구 작업이 종료되었습니다."))?
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
            #[cfg(all(feature = "testkit", debug_assertions))]
            let login = if let Ok(base) = std::env::var("QB_TEST_LOGIN_BASE") {
                relay.test_login_service(qb_core::oauth::LoginTestConfig {
                    authorize_url: format!("{base}/authorize"),
                    token_url: format!("{base}/token"),
                    profile_url: format!("{base}/profile"),
                    callback_port: 0,
                })?
            } else {
                login
            };
            let admin_routes =
                qb_core::admin::account_routes_with_poller(store.clone(), relay.quota.clone())
                    .merge(qb_core::admin::login_routes(login.clone()))
                    .merge(qb_core::admin::runtime_routes(
                        relay.clone(),
                        config.server.data_port,
                    ))
                    .merge(qb_core::debug_pin::routes(store, pins))
                    .merge(qb_core::service::routes(relay.clone()))
                    .merge(box_service.admin_routes(relay.clone(), config.server.data_port));
            #[cfg(feature = "phase0")]
            let admin_routes = admin_routes.merge(qb_core::relay::phase0_routes(relay.clone()));
            let secrets = qb_core::local_secrets::load_or_create(&home)?;
            let mut servers = box_service
                .spawn_http(
                    &config,
                    secrets,
                    qb_core::relay::router(relay.clone()),
                    admin_routes,
                )
                .await?;
            if takeover {
                observability.record_takeover("listening");
            }
            let startup_failure = qb_core::service::defer_startup_error(
                box_service.start(relay.clone()).and_then(|_| {
                    qb_core::service::notify_ready().and_then(|_| {
                        print_stdout(format_args!("프록시를 시작했습니다.")).map_err(Into::into)
                    })
                }),
            )?;
            let polling = relay.quota.start();
            let refreshing = relay.start_refresh_scheduler();
            let catalogs = relay.start_catalog_scheduler();
            let memory_guard = relay.start_memory_guard();
            if startup_failure.is_none() {
                qb_core::service::wait_for_shutdown(relay.clone()).await?;
            }
            qb_core::service::complete_shutdown(async {
                box_service.begin_drain();
                polling.abort();
                refreshing.abort();
                catalogs.abort();
                memory_guard.abort();
                let _ = polling.await;
                let _ = refreshing.await;
                let _ = catalogs.await;
                let _ = memory_guard.await;
                // Stop accepting new connections before handing the credential
                // lease to the successor. Existing handlers remain alive under
                // axum's graceful-shutdown drain.
                servers.shutdown();
                servers.wait_for_listener_close().await;
                if relay.is_draining() && qb_core::service::should_release_lease_early() {
                    relay.release_lease_after_drain();
                }
                let drain_deadline = relay.drain_timeout();
                let clean = qb_core::service::drain_connections(
                    async {
                        let (data, admin) =
                            tokio::join!(&mut servers.data_task, &mut servers.admin_task);
                        while relay
                            .inflight_requests
                            .load(std::sync::atomic::Ordering::Acquire)
                            > 0
                        {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                        data.is_ok_and(|result| result.is_ok())
                            && admin.is_ok_and(|result| result.is_ok())
                    },
                    drain_deadline,
                )
                .await;
                qb_core::service::validate_listener_drain(clean)?;
                drop(servers);
                login.cancel_all().await;
                // The lease must outlive detached token consumption and durable writes.
                while relay.shutdown_refreshes().await.is_err() {
                    if qb_core::service::retry_durable_cleanup() {
                        qb_core::service::shutdown_retry_notice();
                    } else {
                        println_utf8!("종료 중 토큰 저장 작업이 끝나기를 기다립니다.");
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                // Pending rotations retain their box admission, so durable
                // credential retries precede the exact-grant quiescence wait.
                // Preserve lifecycle errors until mandatory core flush finishes.
                let box_cleanup = box_service
                    .join()
                    .await
                    .and_then(|_| box_service.freeze())
                    .and_then(|_| box_service.check_complete());
                // 종료 시 디스크 flush와 작성자 정리도 런타임 스레드를 막지 않는다.
                tokio::task::spawn_blocking(move || {
                    if relay.is_draining() {
                        observability.record_takeover("exited");
                    }
                    loop {
                        let binding_flush = relay.bindings.flush();
                        observability.shutdown_replay();
                        match binding_flush.and_then(|_| observability.flush()) {
                            Ok(()) => break Ok(()),
                            Err(_) if qb_core::service::retry_durable_cleanup() => {
                                qb_core::service::shutdown_retry_notice();
                                std::thread::sleep(std::time::Duration::from_secs(1));
                            }
                            Err(error) => break Err(error),
                        }
                    }
                })
                .await??;
                box_cleanup?;
                Ok(())
            })
            .await?;
            if let Some(error) = startup_failure {
                return Err(error);
            }
            qb_core::service::finish()?;
        }
        Command::Status { options } => {
            let config = qb_core::config::Config::load(&home)?;
            accounts_cli::status(&home, &config).await?;
            platform::status_extra(&home, options)?;
        }
        Command::Accounts { command } => {
            let config = qb_core::config::Config::load(&home)?;
            accounts_cli::run(command, &home, &config).await?;
        }
        Command::Routing { command } => {
            let config = qb_core::config::Config::load(&home)?;
            routing_cli::run(command, &home, &config).await?;
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
                .map_err(|_| anyhow::anyhow!("capture 경로가 존재해야 합니다"))?;
            let provider = if account.starts_with("cl-") {
                qb_core::accounts::Provider::Claude
            } else if account.starts_with("cx-") {
                qb_core::accounts::Provider::Codex
            } else {
                anyhow::bail!("phase0 계정 ID가 올바르지 않습니다");
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
            let summary_bytes = reqwest::Client::builder()
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
                .bytes()
                .await?;
            let summary: qb_proto::admin_phase0::ReplaySummary =
                serde_json::from_slice(&summary_bytes)?;
            println_utf8!("{}", serde_json::to_string(&summary)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod platform_structure_tests;
