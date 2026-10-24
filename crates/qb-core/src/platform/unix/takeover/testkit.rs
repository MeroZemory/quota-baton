use crate::accounts::takeover::{Dependencies, TakeoverError};
use std::path::Path;

pub(crate) fn synthetic(
    _client: reqwest::Client,
    _home: &Path,
    _root: &Path,
    _endpoint: String,
) -> Result<Dependencies, TakeoverError> {
    Err(TakeoverError::Unavailable)
}

pub(crate) fn validate_synthetic_inputs(
    _home: &Path,
    _root: &Path,
    _endpoint: &str,
) -> Result<std::path::PathBuf, TakeoverError> {
    Err(TakeoverError::Unavailable)
}
