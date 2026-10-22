use clap::{Parser, Subcommand};

macro_rules! println_utf8 {
    ($($arg:tt)*) => {
        crate::print_stdout(format_args!($($arg)*))?
    };
}

mod accounts_cli;
mod catalog_snapshot;
mod cutover;
mod image_canary;
mod image_launch;
mod launch_client;
mod launcher_label;
mod launcher_script;
mod resume_selection;
mod routing_cli;
mod sessions;
mod sessions_admission;
mod sessions_confirm;
mod sessions_db;
mod sessions_discovery;
mod sessions_guard;
mod sessions_map;
mod sessions_plain_header;
mod wire_readiness;

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
    #[command(about = "대화 이동 기록과 홈의 상태를 검사합니다")]
    Doctor,
    #[command(about = "기존 Codex 대화를 가져오거나 되돌립니다")]
    Sessions {
        #[command(subcommand)]
        command: sessions::SessionsCommand,
    },
    #[command(about = "프로젝트·세션별 계정 전환을 설정합니다")]
    Routing {
        #[command(subcommand)]
        command: routing_cli::RoutingCommand,
    },
    /// Internal managed child launch; ordinary Codex uses authenticated issuance.
    #[command(hide = true)]
    LaunchClient {
        #[arg(long, value_enum, default_value = "claude")]
        client: launcher_label::Client,
        #[arg(long, required_unless_present = "check", conflicts_with = "check")]
        command: Option<std::path::PathBuf>,
        #[arg(long, help = "Codex 연결 기록만 읽어 검사하고 종료합니다")]
        check: bool,
        #[arg(long)]
        settings: Option<std::path::PathBuf>,
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
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
    Status {
        #[arg(long, help = "M8 전환용 보고 전용 항목을 함께 표시합니다")]
        cutover_report: bool,
    },
    #[command(about = "계정을 관리합니다")]
    Accounts {
        #[command(subcommand)]
        command: accounts_cli::AccountsCommand,
    },
    #[command(about = "클라이언트 설정을 연결하거나 되돌립니다")]
    Wire {
        #[arg(
            long,
            value_parser = ["default", "dedicated", "none"],
            help = "Codex 연결 모드를 선택합니다; 생략하면 자동 감지합니다"
        )]
        codex_mode: Option<String>,
        #[arg(long, value_parser = ["default", "dedicated", "none"], help = "Claude 연결 모드를 선택합니다; 생략하면 별도 설정을 사용합니다")]
        claude_mode: Option<String>,
        #[arg(long, help = "실제 파일을 변경합니다; 생략하면 미리보기입니다")]
        apply: bool,
        #[arg(
            long,
            help = "같은 전용 파일의 내용 변경을 허용하여 제목 설정을 업그레이드합니다"
        )]
        force: bool,
        #[arg(long, help = "현재 사용자 트레이 자동 시작을 연결합니다")]
        autostart: bool,
        #[arg(long, help = "CPA 예약 작업을 삭제하지 않고 비활성화합니다")]
        disable_cpa_tasks: bool,
        #[arg(
            long,
            help = "ChatGPT 인증 세션 홈을 session-homes.json에서 분리합니다"
        )]
        detach_chatgpt_session_homes: bool,
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
        if error
            .downcast_ref::<accounts_cli::TakeoverInspectOnly>()
            .is_some()
        {
            eprintln!("{error}");
            std::process::exit(2);
        }
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
    if let Some(stage) = error.downcast_ref::<cutover::WireStage>() {
        return stage.to_string();
    }
    if let Some(message) = launch_client::friendly_error(error) {
        return message;
    }
    if let Some(message) = catalog_snapshot::friendly_error(error) {
        return message;
    }
    if let Some(message) = image_canary::friendly_error(error) {
        return message;
    }
    if let Some(message) = image_launch::friendly_error(error) {
        return message;
    }
    if let Some(message) = sessions_admission::friendly_error(error) {
        return message;
    }
    if let Some(message) = sessions::friendly_error(error) {
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
    if error.to_string() == cutover::CPA_DEPENDENCY_REFUSAL {
        return cutover::CPA_DEPENDENCY_REFUSAL.into();
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

async fn check_wire_preconditions(
    home: &std::path::Path,
    options: &cutover::WireOptions,
    codex_mode: cutover::CodexMode,
    claude_mode: cutover::ClaudeMode,
) -> anyhow::Result<()> {
    cutover::check_optional_preconditions(home, options)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let config = qb_core::config::Config::load(home)?;
    let secrets = qb_core::local_secrets::load_existing(home).map_err(|_| {
        anyhow::anyhow!("적용하려면 client.key와 admin.token이 먼저 있어야 합니다.")
    })?;
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
    tokio::net::TcpStream::connect(("127.0.0.1", config.server.data_port))
        .await
        .map_err(|_| anyhow::anyhow!("설정된 데이터 포트에 연결할 수 없습니다."))?;
    let accounts = qb_core::accounts::AccountStore::list_metadata(home)?;
    for provider in [
        qb_core::accounts::Provider::Codex,
        qb_core::accounts::Provider::Claude,
    ] {
        if provider == qb_core::accounts::Provider::Codex
            && matches!(codex_mode, cutover::CodexMode::None)
            || provider == qb_core::accounts::Provider::Claude
                && matches!(claude_mode, cutover::ClaudeMode::None)
        {
            continue;
        }
        let dedicated = match provider {
            qb_core::accounts::Provider::Codex => codex_mode == cutover::CodexMode::Dedicated,
            qb_core::accounts::Provider::Claude => claude_mode == cutover::ClaudeMode::Dedicated,
        };
        if let Some(warning) =
            wire_readiness::check(&accounts, provider, dedicated, chrono::Utc::now())?
        {
            println_utf8!("경고: {warning}");
        }
    }
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Command::LaunchClient {
        client,
        command,
        check,
        settings,
        args,
    } = &cli.command
    {
        if *check {
            if settings.is_some() || !args.is_empty() {
                anyhow::bail!("--check에는 실행 인자나 설정 경로를 함께 지정할 수 없습니다");
            }
            sessions_admission::check_wiring(&qb_core::home()?)?;
            println_utf8!("Codex 실행 연결 검사가 통과했습니다. 변경하지 않았습니다.");
            return Ok(());
        }
        let command = command
            .as_deref()
            .expect("clap requires --command without --check");
        let (images, native_args) = image_launch::split(args);
        if images {
            anyhow::ensure!(
                matches!(client, launcher_label::Client::Codex),
                "--images는 codex-qb에서만 사용할 수 있습니다"
            );
            let home = qb_core::home()?;
            image_launch::validate_args(native_args)?;
            let image = image_launch::ImageLaunch::load(&home)?;
            let id = sessions::resume_uuid(native_args)?;
            sessions::admission_check(&home, id.as_deref())?;
            sessions_admission::preflight_images(native_args, command, &image)?;
            image_canary::check(command, &home).await?;
            let (permit, child_args) =
                sessions_admission::prepare_images(native_args, command, &image)?;
            sessions::admission_check(&home, id.as_deref())?;
            let status = launcher_label::run_with_environment(
                *client,
                command,
                settings.as_deref(),
                &child_args,
                &image.environment(),
            );
            drop(permit);
            std::process::exit(status?);
        }
        if matches!(client, launcher_label::Client::Codex) {
            let status = launch_client::run_codex(command, settings.as_deref(), args).await;
            // run_codex explicitly releases snapshot/admission on success and
            // errors, after the existing contained runner's tree cleanup.
            std::process::exit(status?);
        }
        let status = launcher_label::run(*client, command, settings.as_deref(), args);
        std::process::exit(status?);
    }
    let home = qb_core::home()?;
    match cli.command {
        Command::Doctor => sessions::doctor(&home)?,
        Command::Sessions { command } => sessions::run(&home, command)?,
        Command::LaunchClient { .. } => {
            unreachable!("launcher handled before core home resolution")
        }
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
            let catalogs = relay.start_catalog_scheduler();
            let memory_guard = relay.start_memory_guard();
            tokio::select! {
                result = tokio::signal::ctrl_c() => { result?; }
                _ = relay.wait_for_drain() => {}
            }
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
        Command::Status { cutover_report } => {
            let config = qb_core::config::Config::load(&home)?;
            accounts_cli::status(&home, &config).await?;
            if cutover_report {
                let report = cutover::report_only(&cutover::CutoverPaths::from_environment(home)?);
                for line in report.summary {
                    println_utf8!("{line}");
                }
                for warning in report.warnings {
                    println_utf8!("경고: {warning}");
                }
                for item in report.report_only {
                    println_utf8!("보고: {item}");
                }
            }
        }
        Command::Accounts { command } => {
            let config = qb_core::config::Config::load(&home)?;
            accounts_cli::run(command, &home, &config).await?;
        }
        Command::Routing { command } => {
            let config = qb_core::config::Config::load(&home)?;
            routing_cli::run(command, &home, &config).await?;
        }
        Command::Wire {
            codex_mode,
            claude_mode,
            apply,
            force,
            autostart,
            disable_cpa_tasks,
            detach_chatgpt_session_homes,
        } => {
            let mut options = cutover::WireOptions {
                codex_mode: codex_mode.map(|mode| mode.parse()).transpose()?,
                claude_mode: claude_mode.map(|mode| mode.parse()).transpose()?,
                data_port: qb_core::config::Config::load(&home)?.server.data_port,
                force,
                autostart,
                disable_cpa_tasks,
                detach_chatgpt_session_homes,
            };
            let paths = cutover::CutoverPaths::from_environment(home.clone())?;
            let codex_mode = cutover::resolve_codex_mode(&paths, &options)?;
            let claude_mode = cutover::resolve_claude_mode(&paths, &options)?;
            if codex_mode == cutover::CodexMode::None && claude_mode == cutover::ClaudeMode::None {
                anyhow::bail!("Codex와 Claude를 모두 연결 안 함으로 선택할 수 없습니다");
            }
            options.codex_mode = Some(codex_mode);
            options.claude_mode = Some(claude_mode);
            if apply {
                check_wire_preconditions(&home, &options, codex_mode, claude_mode).await?;
            }
            let report = cutover::wire_with_options(&paths, apply, options)?;
            for line in report.summary {
                println_utf8!("{line}");
            }
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
            for line in report.summary {
                println_utf8!("{line}");
            }
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

#[cfg(test)]
mod r1_cpa_error_tests {
    #[test]
    fn r111_wire_stage_hides_paths_values_and_error_chain() {
        let error = anyhow::anyhow!("synthetic-private-token C:/private/config")
            .context(crate::cutover::WireStage("upgrade-shared-agents"));
        let text = super::friendly_error(&error);
        assert!(text.contains("wire:upgrade-shared-agents"));
        assert!(!text.contains("synthetic-private"));
        assert!(!text.contains("C:/private"));
        // A textual lookalike cannot bypass the typed diagnostic boundary.
        let forged = anyhow::anyhow!("wire:upgrade-shared-agents synthetic-private-token");
        assert!(!super::friendly_error(&forged).contains("synthetic-private"));
    }

    #[test]
    fn r3_wire_prerequisites_are_actionable_but_never_echo_appended_details() {
        for message in [
            "PATH에서 안전한 Claude 실행 파일을 찾지 못했습니다. Claude 설치를 확인하세요",
            "Claude 전용 설정 디렉터리 접근 권한을 제한하지 못했습니다",
            "Codex와 Claude를 모두 연결 안 함으로 선택할 수 없습니다",
            "적용하려면 정상 상태의 코어가 실행 중이어야 합니다.",
            "각 프로바이더에 적용 가능한 활성 계정이 하나 이상 필요합니다.",
            "npm 디렉터리 또는 codex.cmd가 없습니다. Codex 설치를 확인하세요",
        ] {
            assert_eq!(super::friendly_error(&anyhow::anyhow!(message)), message);
            let untrusted = anyhow::anyhow!("{message} synthetic-private-detail");
            assert!(!super::friendly_error(&untrusted).contains("synthetic-private-detail"));
        }
    }
    #[test]
    fn r1_cpa_refusal_survives_cli_error_sanitization() {
        let error = anyhow::anyhow!(crate::cutover::CPA_DEPENDENCY_REFUSAL);
        assert_eq!(
            super::friendly_error(&error),
            crate::cutover::CPA_DEPENDENCY_REFUSAL
        );
        let untrusted = anyhow::anyhow!(
            "{} synthetic-private-detail",
            crate::cutover::CPA_DEPENDENCY_REFUSAL
        );
        assert!(!super::friendly_error(&untrusted).contains("synthetic-private-detail"));
    }
}
