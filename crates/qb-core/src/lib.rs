pub mod accounts;
pub mod admin;
pub mod config;
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
