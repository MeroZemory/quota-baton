//! CPA source access is a Windows-only feature.
use super::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("account is not a linked CPA account")]
    NotLinked,
    #[error("linked account source is missing")]
    Missing,
    #[error("linked account source is invalid after retries")]
    InvalidData,
    #[error("linked account source has no newer token")]
    Stale,
    #[error("linked account source I/O failed")]
    Io,
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub(crate) use cpa::resolve_identity;
pub use cpa::{reload, reload_and_store};

#[cfg(windows)]
#[path = "linked/cpa_windows.rs"]
mod cpa;
#[cfg(unix)]
#[path = "linked/cpa_unix.rs"]
mod cpa;
