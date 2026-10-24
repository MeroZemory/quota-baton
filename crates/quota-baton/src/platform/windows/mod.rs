use clap::{Args, Subcommand};
#[path = "../../catalog_snapshot.rs"]
pub(crate) mod catalog_snapshot;
#[path = "../../cutover.rs"]
pub(crate) mod cutover;
#[path = "../../image_canary.rs"]
pub(crate) mod image_canary;
#[path = "../../image_launch.rs"]
pub(crate) mod image_launch;
#[path = "../../launch_client.rs"]
pub(crate) mod launch_client;
#[path = "../../launcher_label.rs"]
pub(crate) mod launcher_label;
#[path = "../../launcher_script.rs"]
pub(crate) mod launcher_script;
#[path = "../../native_resume/mod.rs"]
pub(crate) mod native_resume;
#[path = "../../resume_selection.rs"]
pub(crate) mod resume_selection;
#[path = "../../sessions.rs"]
pub(crate) mod sessions;
#[path = "../../sessions_admission.rs"]
pub(crate) mod sessions_admission;
#[path = "../../sessions_confirm.rs"]
pub(crate) mod sessions_confirm;
#[path = "../../sessions_db.rs"]
pub(crate) mod sessions_db;
#[path = "../../sessions_discovery.rs"]
pub(crate) mod sessions_discovery;
#[path = "../../sessions_guard.rs"]
pub(crate) mod sessions_guard;
#[path = "../../sessions_map.rs"]
pub(crate) mod sessions_map;
#[path = "../../sessions_plain_header.rs"]
pub(crate) mod sessions_plain_header;
#[path = "../../sessions_presentation.rs"]
pub(crate) mod sessions_presentation;
#[path = "../../sessions_preview_title.rs"]
pub(crate) mod sessions_preview_title;
#[path = "../../sessions_progress.rs"]
pub(crate) mod sessions_progress;
#[path = "../../wire_readiness.rs"]
pub(crate) mod wire_readiness;

pub(crate) use sessions::SessionsCommand;

#[derive(Subcommand)]
pub enum PlatformCommand {
    #[command(about = "대화 이동 기록과 홈의 상태를 검사합니다")]
    #[command(display_order = 1)]
    Doctor,
    /// Internal managed child launch; ordinary Codex uses authenticated issuance.
    #[command(hide = true)]
    #[command(display_order = 4)]
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
    #[command(about = "클라이언트 설정을 연결하거나 되돌립니다")]
    #[command(display_order = 8)]
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
    #[command(display_order = 9)]
    Unwire {
        #[arg(long, help = "실제 파일을 변경합니다; 생략하면 미리보기입니다")]
        apply: bool,
        #[arg(long, help = "사용자 변경 감지를 무시하고 되돌립니다")]
        force: bool,
    },
}

#[derive(Args)]
pub struct StatusOptions {
    #[arg(long, help = "M8 전환용 보고 전용 항목을 함께 표시합니다")]
    cutover_report: bool,
}
pub(crate) fn status_extra(home: &std::path::Path, options: StatusOptions) -> anyhow::Result<()> {
    let cutover_report = options.cutover_report;
    if cutover_report {
        let report = cutover::report_only(&cutover::CutoverPaths::from_environment(
            home.to_path_buf(),
        )?);
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
    Ok(())
}

pub(crate) async fn run(command: PlatformCommand) -> anyhow::Result<()> {
    if let PlatformCommand::LaunchClient {
        client,
        command,
        check,
        settings,
        args,
    } = &command
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
        let status = launch_client::run_claude(command, settings.as_deref(), args).await;
        std::process::exit(status?);
    }
    let home = qb_core::home()?;
    match command {
        PlatformCommand::Doctor => sessions::doctor(&home)?,
        PlatformCommand::LaunchClient { .. } => {
            unreachable!("launcher handled before home resolution")
        }
        PlatformCommand::Wire {
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
        PlatformCommand::Unwire { apply, force } => {
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
    }
    Ok(())
}
pub(crate) async fn run_sessions(
    command: SessionsCommand,
    home: &std::path::Path,
) -> anyhow::Result<()> {
    if let SessionsCommand::Urgent { id, state } = command {
        let config = qb_core::config::Config::load(home)?;
        crate::routing_cli::urgent(id, state == "on", home, &config).await
    } else {
        sessions::run(home, command)
    }
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
pub(crate) fn keep_launch_error_visible() {
    use std::io::IsTerminal;
    // Automated exec/pipe callers must retain their noninteractive exit path.
    // A terminal launched for the interactive wrapper must not disappear before
    // the user can read an issuance/model/resume failure.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("launch-client"))
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
    {
        eprintln!("실행하지 못했습니다. 위 오류를 확인한 뒤 Enter 키를 누르면 닫습니다.");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
}
pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    if let Some(stage) = error.downcast_ref::<cutover::WireStage>() {
        return Some(stage.to_string());
    }
    if let Some(message) = launch_client::friendly_error(error) {
        return Some(message);
    }
    if let Some(message) = catalog_snapshot::friendly_error(error) {
        return Some(message);
    }
    if let Some(message) = image_canary::friendly_error(error) {
        return Some(message);
    }
    if let Some(message) = image_launch::friendly_error(error) {
        return Some(message);
    }
    if let Some(message) = sessions_admission::friendly_error(error) {
        return Some(message);
    }
    if let Some(message) = sessions::friendly_error(error) {
        return Some(message);
    }
    if error.to_string() == cutover::CPA_DEPENDENCY_REFUSAL {
        return Some(cutover::CPA_DEPENDENCY_REFUSAL.into());
    }
    None
}
pub(crate) fn write_stdout(stdout: &std::io::Stdout, bytes: &[u8]) -> std::io::Result<()> {
    use std::{io::Write, os::windows::io::AsHandle};
    let mut output = std::fs::File::from(stdout.as_handle().try_clone_to_owned()?);
    output.write_all(bytes)
}

#[cfg(test)]
mod r1_cpa_error_tests {
    #[test]
    fn r111_wire_stage_hides_paths_values_and_error_chain() {
        let error = anyhow::anyhow!("synthetic-private-token C:/private/config")
            .context(crate::cutover::WireStage("upgrade-shared-agents"));
        let text = crate::friendly_error(&error);
        assert!(text.contains("wire:upgrade-shared-agents"));
        assert!(!text.contains("synthetic-private"));
        assert!(!text.contains("C:/private"));
        // A textual lookalike cannot bypass the typed diagnostic boundary.
        let forged = anyhow::anyhow!("wire:upgrade-shared-agents synthetic-private-token");
        assert!(!crate::friendly_error(&forged).contains("synthetic-private"));
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
            assert_eq!(crate::friendly_error(&anyhow::anyhow!(message)), message);
            let untrusted = anyhow::anyhow!("{message} synthetic-private-detail");
            assert!(!crate::friendly_error(&untrusted).contains("synthetic-private-detail"));
        }
    }
    #[test]
    fn r1_cpa_refusal_survives_cli_error_sanitization() {
        let error = anyhow::anyhow!(crate::cutover::CPA_DEPENDENCY_REFUSAL);
        assert_eq!(
            crate::friendly_error(&error),
            crate::cutover::CPA_DEPENDENCY_REFUSAL
        );
        let untrusted = anyhow::anyhow!(
            "{} synthetic-private-detail",
            crate::cutover::CPA_DEPENDENCY_REFUSAL
        );
        assert!(!crate::friendly_error(&untrusted).contains("synthetic-private-detail"));
    }
}

mod accounts;
pub(crate) use accounts::*;

pub(crate) async fn capture_debug_snapshot(home: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    use qb_tray::client::{AdminClient, snapshot};
    let client = AdminClient::from_home_path(home)
        .map_err(|_| anyhow::anyhow!("Unable to read the local admin connection"))?;
    snapshot::encode(&snapshot::capture(&client).await?)
}

#[cfg(test)]
mod help_order_tests {
    use clap::CommandFactory;

    fn visible_commands(help: &str) -> Vec<&str> {
        help.lines()
            .skip_while(|line| *line != "Commands:")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| *name != "help")
            .collect()
    }

    #[test]
    fn platform_root_help_preserves_windows_command_order() {
        let help = crate::Cli::command().render_help().to_string();
        let mut expected = vec![
            "debug", "doctor", "sessions", "routing", "serve", "status", "accounts", "wire",
            "unwire",
        ];
        if cfg!(feature = "phase0") {
            expected.push("phase0");
        }
        assert_eq!(visible_commands(&help), expected);
        assert!(!visible_commands(&help).contains(&"launch-client"));
    }

    #[test]
    fn platform_accounts_help_preserves_windows_command_order() {
        let mut command = crate::Cli::command();
        let help = command
            .find_subcommand_mut("accounts")
            .unwrap()
            .render_help()
            .to_string();
        assert_eq!(
            visible_commands(&help),
            [
                "login",
                "takeover",
                "list",
                "pause",
                "resume",
                "disable",
                "enable",
                "refresh-usage",
                "release-hold",
                "adopt",
                "refresh",
                "import"
            ]
        );
    }
}
