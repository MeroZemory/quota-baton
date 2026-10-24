use clap::{Args, Subcommand};

#[derive(Subcommand)]
pub enum PlatformCommand {}
#[derive(Args)]
pub struct StatusOptions {}
#[derive(Subcommand)]
pub(crate) enum SessionsCommand {
    #[command(about = "실행 중인 코어에서 루트 세션의 5시간 제한 긴급 전환을 설정합니다")]
    Urgent {
        id: String,
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
}
pub(crate) async fn run(command: PlatformCommand) -> anyhow::Result<()> {
    match command {}
}
pub(crate) async fn run_sessions(
    command: SessionsCommand,
    home: &std::path::Path,
) -> anyhow::Result<()> {
    let SessionsCommand::Urgent { id, state } = command;
    let config = qb_core::config::Config::load(home)?;
    crate::routing_cli::urgent(id, state == "on", home, &config).await
}
pub(crate) fn status_extra(_: &std::path::Path, _: StatusOptions) -> anyhow::Result<()> {
    Ok(())
}
pub(crate) fn friendly_error(_: &anyhow::Error) -> Option<String> {
    None
}
pub(crate) fn keep_launch_error_visible() {}
pub(crate) fn write_stdout(stdout: &std::io::Stdout, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    stdout.lock().write_all(bytes)
}

#[derive(Subcommand)]
pub enum AccountsPlatformCommand {}
#[derive(Args)]
pub struct LoginOptions {}
impl LoginOptions {
    pub(crate) fn replace_linked(&self) -> bool {
        false
    }
}
pub(crate) async fn run_accounts(
    command: AccountsPlatformCommand,
    _: &std::path::Path,
    _: &qb_core::config::Config,
) -> anyhow::Result<()> {
    match command {}
}
pub(crate) fn cpa_dependency_note(_: &std::path::Path) -> anyhow::Result<Option<String>> {
    Ok(None)
}
pub(crate) fn open_browser(url: &str) -> std::io::Result<()> {
    let program = "xdg-open";
    let status = std::process::Command::new(program).arg(url).status()?;
    if !status.success() {
        return Err(std::io::Error::other("브라우저를 열지 못했습니다."));
    }
    Ok(())
}

mod snapshot;
pub(crate) use snapshot::capture_debug_snapshot;

#[cfg(test)]
mod cli_surface_tests {
    use clap::{CommandFactory, Parser};

    #[test]
    fn headless_help_has_only_supported_commands() {
        let command = crate::Cli::command();
        let names: Vec<_> = command
            .get_subcommands()
            .map(|command| command.get_name())
            .collect();
        for expected in [
            "serve", "status", "accounts", "routing", "sessions", "debug",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        for excluded in ["wire", "unwire", "launch-client", "doctor"] {
            assert!(!names.contains(&excluded), "unexpected {excluded}");
        }
        let accounts = command.find_subcommand("accounts").unwrap();
        let names: Vec<_> = accounts
            .get_subcommands()
            .map(|command| command.get_name())
            .collect();
        for expected in [
            "login",
            "list",
            "pause",
            "resume",
            "refresh-usage",
            "release-hold",
        ] {
            assert!(names.contains(&expected), "missing accounts {expected}");
        }
        for excluded in [
            "takeover", "import", "adopt", "refresh", "enable", "disable",
        ] {
            assert!(!names.contains(&excluded), "unexpected accounts {excluded}");
        }
        let sessions = command.find_subcommand("sessions").unwrap();
        assert!(sessions.find_subcommand("urgent").is_some());
        for excluded in ["import", "unimport", "return", "recover"] {
            assert!(sessions.find_subcommand(excluded).is_none());
        }
    }

    #[test]
    fn headless_parser_accepts_real_surface_and_refuses_windows_extensions() {
        for args in [
            vec!["qb", "serve", "--takeover"],
            vec!["qb", "status"],
            vec!["qb", "accounts", "login", "claude", "--no-browser"],
            vec!["qb", "accounts", "login", "codex", "--no-browser"],
            vec!["qb", "accounts", "list", "--detail"],
            vec!["qb", "accounts", "pause", "synthetic"],
            vec!["qb", "accounts", "resume", "synthetic"],
            vec!["qb", "accounts", "refresh-usage", "synthetic"],
            vec!["qb", "accounts", "release-hold", "synthetic"],
            vec!["qb", "routing", "list"],
            vec!["qb", "sessions", "urgent", "synthetic", "on"],
            vec!["qb", "sessions", "urgent", "synthetic", "off"],
            vec!["qb", "debug", "snapshot", "--out", "synthetic.json"],
        ] {
            assert!(
                crate::Cli::try_parse_from(&args).is_ok(),
                "rejected {args:?}"
            );
        }
        for args in [
            vec!["qb", "wire"],
            vec!["qb", "unwire"],
            vec!["qb", "launch-client", "--check"],
            vec!["qb", "doctor"],
            vec!["qb", "status", "--cutover-report"],
            vec!["qb", "accounts", "login", "codex", "--replace-linked"],
            vec!["qb", "accounts", "disable", "synthetic"],
            vec!["qb", "accounts", "takeover", "codex", "synthetic"],
            vec!["qb", "accounts", "import", "cpa"],
            vec!["qb", "accounts", "adopt", "synthetic"],
            vec!["qb", "sessions", "return", "synthetic"],
            vec!["qb", "sessions", "urgent", "synthetic", "invalid"],
        ] {
            assert!(
                crate::Cli::try_parse_from(&args).is_err(),
                "accepted {args:?}"
            );
        }
    }
}
