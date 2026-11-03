//! Native preparation of newly owned, empty filesystem fixtures only.
#[cfg(unix)]
#[path = "unix.rs"]
mod native;
#[cfg(windows)]
#[path = "windows.rs"]
mod native;

pub(crate) use native::prepare_new_fixture_directory;
