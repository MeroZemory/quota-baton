pub mod account_runtime;
pub mod accounts;
pub mod admin;
pub mod bindings;
pub mod boxes;
mod burst;
#[cfg(feature = "shape-capture")]
pub mod capture;
pub mod catalog;
pub mod config;
pub mod credits;
pub mod dashboard;
pub mod debug_pin;
pub mod guard;
pub(crate) mod issue_operation;
mod json_unique;
pub(crate) mod launch_path;
pub mod launch_tokens;
pub mod oauth;
pub mod observability;
#[cfg(all(feature = "phase0", debug_assertions))]
pub mod phase0;
pub(crate) mod placement_capacity;
pub(crate) mod platform;
pub mod provider_forecast;
pub mod quota;
pub mod relay;
pub mod routing;
pub mod service;
pub mod session;
pub mod usage_forecast;
pub mod usage_forecast_display;

pub fn set_console_utf8() {
    platform::set_console_utf8();
}

#[cfg(all(feature = "shape-capture", not(debug_assertions)))]
compile_error!("shape-capture is forbidden in release builds");

#[cfg(all(feature = "phase0", not(debug_assertions)))]
compile_error!("phase0 is forbidden in release builds");

pub fn set_process_priority() -> anyhow::Result<()> {
    platform::set_process_priority()
}
pub mod http;
pub mod local_secrets;
pub mod logging;

pub fn home() -> anyhow::Result<std::path::PathBuf> {
    platform::home()
}

/// Prepare application-owned data directories without repairing unsafe existing paths.
pub fn ensure_private_dir_all(path: &std::path::Path) -> std::io::Result<()> {
    platform::ensure_private_dir_all(path)
}

/// Return only a native, bounded permission diagnostic, never an arbitrary cause.
pub fn startup_permission_diagnostic(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    platform::startup_permission_diagnostic(error)
}

/// Check an existing data home and its startup directories without changing them.
pub fn check_private_data_home(path: &std::path::Path) -> std::io::Result<()> {
    platform::check_private_data_home(path)
}

/// Exclusively create one new private data directory; never reuse an existing leaf.
pub fn create_private_directory_new(path: &std::path::Path) -> std::io::Result<()> {
    platform::create_private_directory_new(path)
}
