#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod client;
#[cfg(windows)]
mod dashboard_persistence;
#[cfg(windows)]
mod notices;
#[cfg(windows)]
mod theme;
#[cfg(windows)]
mod tray_icon;
#[cfg(windows)]
mod viewport;
use qb_tray::{parse_state_at, render_png};

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "quota-baton-tray")]
struct Args {
    /// Render a deterministic review image and exit.
    #[arg(long)]
    render_png: Option<std::path::PathBuf>,
    #[arg(long, default_value = "light", requires = "render_png", value_parser = ["light", "dark"])]
    theme: String,
    #[arg(long, default_value_t = 1.0, requires = "render_png")]
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
    let args = Args::parse();
    anyhow::ensure!(
        !args.open_flyout_live || cfg!(debug_assertions),
        "--open-flyout-live is debug-only"
    );
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
