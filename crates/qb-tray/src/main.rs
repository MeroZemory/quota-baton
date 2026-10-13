mod client;
use qb_tray::{parse_state, render_png};

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
    #[arg(long, requires = "render_png")]
    fixture: Option<std::path::PathBuf>,
    /// Do not start or restart a missing core.
    #[arg(long)]
    no_supervise: bool,
}

#[cfg(windows)]
mod windows_tray;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(output) = args.render_png {
        let fixture = args
            .fixture
            .ok_or_else(|| anyhow::anyhow!("--fixture is required with --render-png"))?;
        let json = std::fs::read_to_string(fixture)?;
        let rows = parse_state(&json)?;
        let png =
            render_png(&rows, args.scale, args.theme == "dark").map_err(anyhow::Error::msg)?;
        std::fs::write(output, png)?;
        return Ok(());
    }
    windows_tray::run(!args.no_supervise)
}

#[cfg(not(windows))]
fn main() {
    eprintln!("quota-baton-tray requires Windows");
}
