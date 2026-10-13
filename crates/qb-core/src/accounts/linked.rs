use super::{
    Account, AccountStore, ImportMode, Source, SourceMatch, StoreError, Tokens,
    import::{self, Candidate, ReadError},
};
use std::{thread, time::Duration};

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

fn candidate(account: &Account) -> Result<Candidate, LinkError> {
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
                        == account.identity.canonical_key() =>
                {
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
            if candidate.account.token_meta.newer_than(&account.token_meta) {
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
    Ok(store.save(&updated, &candidate.tokens, Some(account.generation))?)
}
