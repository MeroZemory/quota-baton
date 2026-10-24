//! Linux uses independent OAuth login; native client source access is disabled.
use crate::accounts::Provider;
use crate::accounts::shared_profile::{PROFILE, SharedProfile};
use crate::accounts::takeover::{
    Dependencies, Owner, SourceAccess, SourceProof, SourceSnapshot, TakeoverError,
};
use chrono::{DateTime, Utc};
use std::{path::Path, sync::Arc};

pub(crate) fn production(client: reqwest::Client) -> Dependencies {
    Dependencies {
        source: Arc::new(DisabledTakeoverSource),
        profile: Arc::new(SharedProfile {
            client,
            endpoint: PROFILE.into(),
        }),
    }
}

pub(crate) fn retirement_evidence(
    _proof: &SourceProof,
    _destination: &Path,
) -> Result<bool, TakeoverError> {
    Err(TakeoverError::Unavailable)
}

#[cfg(all(feature = "testkit", debug_assertions))]
pub(crate) fn synthetic(
    _client: reqwest::Client,
    _home: &Path,
    _root: &Path,
    _endpoint: String,
) -> Result<Dependencies, TakeoverError> {
    Err(TakeoverError::Unavailable)
}

#[cfg(all(feature = "testkit", debug_assertions))]
pub(crate) fn validate_synthetic_inputs(
    _home: &Path,
    _root: &Path,
    _endpoint: &str,
) -> Result<std::path::PathBuf, TakeoverError> {
    Err(TakeoverError::Unavailable)
}

/// Linux cannot inspect or retire the Windows client's credential source.
struct DisabledTakeoverSource;
impl SourceAccess for DisabledTakeoverSource {
    fn snapshot(&self, _provider: Provider, _path: &Path) -> Result<SourceSnapshot, TakeoverError> {
        Err(TakeoverError::Unavailable)
    }
    fn owner(&self, _provider: Provider, _proof: &SourceProof) -> Owner {
        Owner::Unknown
    }
    fn retire(
        &self,
        _provider: Provider,
        _proof: &SourceProof,
        _destination: &Path,
        _keep: bool,
    ) -> Result<(), TakeoverError> {
        Err(TakeoverError::Unavailable)
    }
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
