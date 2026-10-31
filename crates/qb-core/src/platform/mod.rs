//! Native operations, selected once at the module boundary.
#[cfg(windows)]
#[path = "windows/mod.rs"]
mod native;
#[cfg(unix)]
#[path = "unix/mod.rs"]
mod native;

pub(crate) use native::*;

/// Exclusively create one private leaf; an existing path is never reused.
pub(crate) fn create_private_directory_new(path: &std::path::Path) -> std::io::Result<()> {
    native::create_private_directory_new(path)
}

pub(crate) mod box_sockets;
