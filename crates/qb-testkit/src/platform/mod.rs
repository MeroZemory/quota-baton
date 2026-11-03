//! Native preparation of newly owned, empty filesystem fixtures only.
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::prepare_new_fixture_directory;
#[cfg(windows)]
pub(crate) use windows::prepare_new_fixture_directory;
