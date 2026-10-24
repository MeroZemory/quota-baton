//! The only OS selection boundary for the CLI.
#[cfg(windows)]
#[path = "windows/mod.rs"]
mod native;
#[cfg(unix)]
#[path = "unix/mod.rs"]
mod native;
pub(crate) use native::*;
