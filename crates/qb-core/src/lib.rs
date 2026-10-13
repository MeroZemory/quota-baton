pub mod accounts;
pub mod admin;
#[cfg(feature = "shape-capture")]
pub mod capture;
pub mod config;
pub mod debug_pin;
pub mod guard;
pub mod observability;
#[cfg(all(feature = "phase0", debug_assertions))]
pub mod phase0;
pub mod relay;
pub mod session;

#[cfg(all(feature = "shape-capture", not(debug_assertions)))]
compile_error!("shape-capture is forbidden in release builds");

#[cfg(all(feature = "phase0", not(debug_assertions)))]
compile_error!("phase0 is forbidden in release builds");

pub fn set_process_priority() -> anyhow::Result<()> {
    // 릴레이가 CPU 부하로 밀리지 않도록 코어 우선순위를 높인다.
    unsafe {
        use windows::Win32::System::Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
        };
        SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS)?;
    }
    Ok(())
}
pub mod http;
pub mod local_secrets;
pub mod logging;

pub fn home() -> anyhow::Result<std::path::PathBuf> {
    if let Some(value) = std::env::var_os("QB_HOME") {
        return Ok(value.into());
    }
    Ok(std::path::PathBuf::from(
        std::env::var_os("LOCALAPPDATA")
            .ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA is missing"))?,
    )
    .join("quota-baton"))
}
