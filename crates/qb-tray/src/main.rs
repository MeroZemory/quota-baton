#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

use qb_tray::client;
#[cfg(windows)]
mod dashboard_persistence;
#[cfg(windows)]
mod notices;
#[cfg(windows)]
mod theme;
#[cfg(windows)]
use qb_tray::tray_icon;
#[cfg(windows)]
mod viewport;
use qb_tray::{parse_state_at, render_png};

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "quota-baton-tray")]
#[command(group(clap::ArgGroup::new("render_mode").args(["render_png", "render_snapshot"])))]
struct Args {
    /// Render a deterministic review image and exit.
    #[arg(long)]
    render_png: Option<std::path::PathBuf>,
    /// Render a read-only admin snapshot without a core or a user profile.
    #[arg(long, requires = "out")]
    render_snapshot: Option<std::path::PathBuf>,
    #[arg(long, requires = "render_snapshot")]
    out: Option<std::path::PathBuf>,
    #[arg(long, default_value = "ko", requires = "render_snapshot", value_parser = ["ko", "en"])]
    lang: String,
    #[arg(long, default_value = "overview", requires = "render_snapshot", value_parser = ["overview", "usage", "events", "flyout", "menu"])]
    view: String,
    #[arg(long, default_value = "1280x800", requires = "render_snapshot")]
    size: String,
    #[arg(long, default_value = "light", requires = "render_mode", value_parser = ["light", "dark"])]
    theme: String,
    #[arg(long, default_value_t = 1.0, requires = "render_mode")]
    scale: f32,
    #[arg(long)]
    fixture: Option<std::path::PathBuf>,
    #[arg(long)]
    now: Option<String>,
    #[arg(long, requires = "render_png")]
    expand_disabled: bool,
    /// Review state to paint: list, menu:<id>, confirm-delete:<id>, login:claude,
    /// login:codex, or quit.
    #[arg(long, default_value = "list", requires = "render_png")]
    state: String,
    #[arg(long, requires = "no_supervise", conflicts_with = "open_flyout_live")]
    open_flyout: bool,
    /// Debug-only live flyout mode. It reads the spawned core through the admin API.
    #[arg(long, requires = "no_supervise", conflicts_with = "open_flyout")]
    open_flyout_live: bool,
    /// Do not start or restart a missing core.
    #[arg(long)]
    no_supervise: bool,
}

#[cfg(windows)]
mod windows_tray;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => {
            // Offline render errors must not resolve a live home or initialize logs.
            if std::env::args_os().skip(1).any(|arg| {
                let arg = arg.to_string_lossy();
                arg == "--render-snapshot"
                    || arg.starts_with("--render-snapshot=")
                    || arg == "--render-png"
                    || arg.starts_with("--render-png=")
            }) {
                return std::process::ExitCode::FAILURE;
            }
            // A GUI process has no stderr console. Persist a stable diagnostic,
            // never a parser error, request payload, or credential-bearing path.
            if let Ok(home) = qb_core::home()
                && let Ok(_logs) = qb_core::logging::init(&home)
            {
                tracing::error!(event = "tray_startup_failed", "트레이 실행에 실패했습니다");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn run() -> anyhow::Result<()> {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            error.print()?;
            return Ok(());
        }
        // Parser diagnostics can echo secret arguments. Discard them and let
        // main persist only the fixed startup-failure diagnostic.
        Err(_) => anyhow::bail!("invalid tray arguments"),
    };
    anyhow::ensure!(
        !args.open_flyout_live || cfg!(debug_assertions),
        "--open-flyout-live is debug-only"
    );
    if let Some(input) = &args.render_snapshot {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(input)?
            .take(qb_tray::client::snapshot::MAX_BUNDLE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        let snapshot = qb_tray::client::snapshot::decode(&bytes)?;
        let now = match &args.now {
            Some(now) => chrono::DateTime::parse_from_rfc3339(now)?,
            None => snapshot.capture_completed_at.with_timezone(
                &chrono::FixedOffset::east_opt(snapshot.utc_offset_seconds)
                    .ok_or_else(|| anyhow::anyhow!("invalid snapshot UTC offset"))?,
            ),
        };
        anyhow::ensure!([1., 2.].contains(&args.scale), "scale must be 1 or 2");
        let language = if args.lang == "en" {
            qb_core::config::UiLanguage::En
        } else {
            qb_core::config::UiLanguage::Ko
        };
        return qb_tray::i18n::with_language(language, || {
            let data = snapshot.project(std::time::SystemTime::from(now.to_utc()))?;
            qb_tray::snapshot_render::save(
                &data,
                &qb_tray::snapshot_render::Options {
                    view: args.view.parse().map_err(anyhow::Error::msg)?,
                    size: qb_tray::snapshot_render::size(&args.size).map_err(anyhow::Error::msg)?,
                    scale: args.scale as u32,
                    dark: args.theme == "dark",
                    now,
                },
                args.out.as_deref().expect("clap requires --out"),
            )
        });
    }
    let now = args
        .now
        .as_deref()
        .and_then(|value| {
            let meta = qb_core::accounts::TokenMeta {
                expires_at: serde_json::from_value(serde_json::Value::String(value.into())).ok(),
                ..Default::default()
            };
            meta.expires_at.map(std::time::SystemTime::from)
        })
        .unwrap_or_else(std::time::SystemTime::now);
    if let Some(output) = args.render_png {
        let fixture = args
            .fixture
            .ok_or_else(|| anyhow::anyhow!("--fixture is required with --render-png"))?;
        let json = std::fs::read_to_string(fixture)?;
        let state: qb_tray::State = serde_json::from_str(&json)?;
        let rows = parse_state_at(&json, now)?;
        render_png(
            &rows,
            args.scale,
            args.theme == "dark",
            &state.version,
            now,
            args.expand_disabled || args.state == "expanded",
            &output,
            &args.state,
        )
        .map_err(anyhow::Error::msg)?;
        return Ok(());
    }
    let language = qb_core::home()
        .ok()
        .and_then(|home| qb_core::config::Config::load(&home).ok())
        .map(|config| config.ui.language)
        .unwrap_or_default();
    qb_tray::i18n::set_language(language);
    windows_tray::run(
        !args.no_supervise,
        args.open_flyout || args.open_flyout_live,
        args.fixture,
        now,
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn open_flyout_is_independent_of_png_rendering() {
        let args = Args::try_parse_from([
            "quota-baton-tray",
            "--open-flyout",
            "--no-supervise",
            "--now",
            "2026-10-19T07:00:00Z",
        ])
        .expect("debug flyout arguments should parse");
        assert!(args.open_flyout);
        assert!(args.render_png.is_none());
        let live =
            Args::try_parse_from(["quota-baton-tray", "--open-flyout-live", "--no-supervise"])
                .expect("live flyout arguments should parse");
        assert!(live.open_flyout_live);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("quota-baton-tray requires Windows");
}
