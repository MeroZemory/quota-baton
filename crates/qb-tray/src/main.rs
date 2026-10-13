mod client;

#[cfg(windows)]
mod windows_tray;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows_tray::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("quota-baton-tray requires Windows");
}
