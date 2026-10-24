use std::path::PathBuf;

pub fn set_console_utf8() {}
pub fn set_process_priority() -> anyhow::Result<()> {
    Ok(())
}
pub fn home() -> anyhow::Result<PathBuf> {
    for name in ["QB_HOME", "STATE_DIRECTORY"] {
        if let Some(value) = std::env::var_os(name) {
            return Ok(value.into());
        }
    }
    let state = if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(value)
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is missing"))?)
            .join(".local/state")
    };
    Ok(state.join("quota-baton"))
}

#[cfg(test)]
#[path = "process/liveness.rs"]
mod liveness;
