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
mod cpa {
    use super::super::{
        Account, AccountStore, Identity, ImportMode, Source, SourceMatch, Tokens,
        import::{self, Candidate, ReadError},
    };
    use super::{LinkError, StoreError};
    use std::{thread, time::Duration};

    pub(crate) fn resolve_identity(account: &Account) -> Result<Identity, LinkError> {
        Ok(find_candidate(account, false)?.account.identity)
    }

    fn candidate(account: &Account) -> Result<Candidate, LinkError> {
        find_candidate(account, true)
    }

    fn find_candidate(account: &Account, require_newer: bool) -> Result<Candidate, LinkError> {
        let dir = match &account.source {
            Source::Cpa {
                mode: ImportMode::Linked,
                dir,
                r#match,
            } if *r#match == SourceMatch::from(&account.identity) => dir,
            _ => return Err(LinkError::NotLinked),
        };
        // 최초 시도 이후 최대 세 번, 매번 전체 디렉터리를 다시 스캔한다.
        for attempt in 0..=3 {
            let mut invalid = false;
            let mut best: Option<Candidate> = None;
            let paths = match import::paths(dir) {
                Ok(paths) => paths,
                Err(_) if !dir.exists() => return Err(LinkError::Missing),
                Err(_) => return Err(LinkError::Io),
            };
            for path in paths {
                match import::read(&path, dir) {
                    Ok(candidate)
                        if candidate.account.identity.canonical_key()
                            == account.identity.canonical_key()
                            || (matches!(
                                account.identity,
                                Identity::Codex { user_id: None, .. }
                            ) && SourceMatch::from(&candidate.account.identity)
                                == SourceMatch::from(&account.identity)) =>
                    {
                        if best.as_ref().is_some_and(|old| {
                            old.account.identity.canonical_key()
                                != candidate.account.identity.canonical_key()
                        }) {
                            return Err(LinkError::InvalidData);
                        }
                        if best.as_ref().is_none_or(|old| {
                            candidate
                                .account
                                .token_meta
                                .newer_than(&old.account.token_meta)
                        }) {
                            best = Some(candidate);
                        }
                    }
                    Err(ReadError::InvalidData | ReadError::Io) => invalid = true,
                    _ => {}
                }
            }
            if let Some(candidate) = best {
                if !require_newer || candidate.account.token_meta.newer_than(&account.token_meta) {
                    return Ok(candidate);
                }
                if !invalid || attempt == 3 {
                    return Err(LinkError::Stale);
                }
            }
            if !invalid {
                return Err(LinkError::Missing);
            }
            if attempt == 3 {
                return Err(LinkError::InvalidData);
            }
            thread::sleep(Duration::from_millis(200));
        }
        Err(LinkError::InvalidData)
    }

    pub fn reload(account: &Account) -> Result<Tokens, LinkError> {
        Ok(candidate(account)?.tokens)
    }

    pub fn reload_and_store(store: &AccountStore, account: &Account) -> Result<Account, LinkError> {
        let candidate = candidate(account)?;
        let mut updated = account.clone();
        updated.token_meta = candidate.account.token_meta;
        match store.save_credentials(&updated, &candidate.tokens, account.generation) {
            // Linked-token recovery can legitimately clear LinkStale/NeedsRelogin or
            // expire health. Preserve that native behavior as an ordinary FullSave.
            Err(StoreError::CredentialHealthChange) => {
                Ok(store.save(&updated, &candidate.tokens, Some(account.generation))?)
            }
            result => Ok(result?),
        }
    }
}

#[cfg(unix)]
mod cpa {
    pub(crate) use crate::platform::linked::resolve_identity;
    pub use crate::platform::linked::{reload, reload_and_store};
}
