#[path = "../../build-support/windows_icon.rs"]
mod windows_icon;

fn main() {
    windows_icon::embed("quota-baton-tray");
}
