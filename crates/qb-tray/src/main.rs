mod client;
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
    #[arg(long, requires_all = ["fixture", "no_supervise"])]
    open_flyout: bool,
    /// Do not start or restart a missing core.
    #[arg(long)]
    no_supervise: bool,
}

#[cfg(windows)]
mod windows_tray;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let args = Args::parse();
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
            args.expand_disabled,
            &output,
            &args.state,
        )
        .map_err(anyhow::Error::msg)?;
        return Ok(());
    }
    windows_tray::run(!args.no_supervise, args.open_flyout, args.fixture, now)
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
            "--fixture",
            "fixture.json",
            "--no-supervise",
            "--now",
            "2026-10-19T07:00:00Z",
        ])
        .expect("debug flyout arguments should parse");
        assert!(args.open_flyout);
        assert!(args.render_png.is_none());
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("quota-baton-tray requires Windows");
}
