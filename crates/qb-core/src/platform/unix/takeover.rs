//! Linux never reads or retires another client's credential source.
use crate::accounts::takeover::SourceProof;
use std::path::Path;

#[cfg(not(feature = "testkit"))]
#[path = "takeover/refresh_source.rs"]
mod refresh_source;
#[cfg(not(feature = "testkit"))]
pub use refresh_source::RefreshSourceContext;

pub(crate) fn retirement_evidence(_proof: &SourceProof, _destination: &Path) -> Result<bool, ()> {
    Err(())
}

#[cfg(all(feature = "testkit", debug_assertions))]
#[path = "takeover/testkit.rs"]
mod testkit;
#[cfg(all(feature = "testkit", debug_assertions))]
pub(crate) use testkit::*;
