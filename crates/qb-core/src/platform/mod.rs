//! Native operations, selected once at the module boundary.
#[cfg(windows)]
#[path = "windows/mod.rs"]
mod native;
#[cfg(unix)]
#[path = "unix/mod.rs"]
mod native;

pub(crate) use native::*;

pub(crate) mod box_sockets;
