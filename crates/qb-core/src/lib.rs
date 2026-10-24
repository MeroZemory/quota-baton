pub mod account_runtime;
pub mod accounts;
pub mod admin;
pub mod bindings;
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
