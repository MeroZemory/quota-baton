//! Transfer refresh ownership from CPA while preserving account identity.
use super::{
    Account, AccountStore, Health, HealthReason, Identity, ImportMode, Source, SourceMatch,
    StoreError, TokenMeta,
    import::{self, Candidate, ReadError},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdoptSource {
    #[default]
    CpaFile,
    Store,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AdoptFrom {
    Source(AdoptSource),
    Path { path: PathBuf },
}

impl Default for AdoptFrom {
    fn default() -> Self {
        Self::Source(AdoptSource::CpaFile)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AdoptRequest {
    #[serde(default)]
    pub from: AdoptFrom,
    pub expected_generation: u64,
}

/// Shared current response leaves; storage and adoption policy remain local.
pub use qb_proto::admin_adopt::AdoptWarning;
pub type AdoptResult = qb_proto::admin_adopt::AdoptReply<DateTime<Utc>>;

#[derive(Debug, thiserror::Error)]
pub enum AdoptError {
    #[error("account was not found")]
    NotFound,
    #[error("account is not a linked CPA account")]
    NotLinked,
    #[error("account generation mismatch (expected {expected}, actual {actual})")]
    Generation { expected: u64, actual: u64 },
    #[error("CPA identity does not match the stored account")]
    IdentityMismatch,
    #[error("multiple CPA canonical identities match the account")]
    AmbiguousIdentity,
    #[error("CPA can still refresh this account")]
    CpaRunning,
    #[error("CPA process ownership probe is unavailable")]
    ProbeUnavailable,
    #[error("CPA source directory could not be inspected")]
    SourceIo,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Unknown is fail-closed for matching or unreadable potentially matching CPA files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeState {
    Stopped,
    Running,
    Unknown,
}

pub fn run(
    store: &AccountStore,
    id: &str,
    request: AdoptRequest,
) -> Result<AdoptResult, AdoptError> {
    execute(store, id, request, production_probe)
}

/// External testkit users must inject a synthetic probe; never call `run` in tests.
#[cfg(any(test, feature = "testkit"))]
pub fn run_with_probe(
    store: &AccountStore,
    id: &str,
    request: AdoptRequest,
    probe: impl FnOnce() -> ProbeState,
) -> Result<AdoptResult, AdoptError> {
    execute(store, id, request, probe)
}

fn execute(
    store: &AccountStore,
    id: &str,
    request: AdoptRequest,
    probe: impl FnOnce() -> ProbeState,
) -> Result<AdoptResult, AdoptError> {
    let _guard = store.transaction()?;
    if store.writes_stopped() {
        return Err(StoreError::LeaseUnavailable.into());
    }
    // Read metadata without expiring unrelated health during adoption.
    let mut account = store
        .read_account_unlocked(id)
        .map_err(|error| match error {
            StoreError::NotFound => AdoptError::NotFound,
            other => AdoptError::Store(other),
        })?;
    if account.generation != request.expected_generation {
        return Err(AdoptError::Generation {
            expected: request.expected_generation,
            actual: account.generation,
        });
    }
    let dir = match &account.source {
        Source::Cpa {
            mode: ImportMode::Linked,
            dir,
            r#match,
        } if *r#match == SourceMatch::from(&account.identity) => dir.clone(),
        Source::Cpa {
            mode: ImportMode::Linked,
            ..
        } => return Err(AdoptError::IdentityMismatch),
        _ => return Err(AdoptError::NotLinked),
    };
    let stored = store.tokens_unlocked(&account)?;
    // A legacy identity may still have an independently usable user-id claim.
    let known_user = match &account.identity {
        Identity::Codex {
            account_id,
            user_id,
            ..
        } => user_id
            .clone()
            .or_else(|| import::codex_user_id(&stored, account_id)),
        _ => None,
    };
    let mut warnings = Vec::new();
    let cpa = scan(&dir, &account, known_user.as_deref(), false, &mut warnings)?;
    // Always inspect CPA before honoring explicit store or backup selection.
    if cpa.candidate.is_some() || cpa.invalid {
        match probe() {
            ProbeState::Running => return Err(AdoptError::CpaRunning),
            ProbeState::Unknown => return Err(AdoptError::ProbeUnavailable),
            ProbeState::Stopped => {}
        }
    }
    // Ownership is checked even for unusable CPA tokens, before source selection.
    let cpa = if matches!(&request.from, AdoptFrom::Source(AdoptSource::Store)) {
        None
    } else {
        usable_candidate(cpa.candidate, &account.token_meta, &mut warnings)
    };
    let (candidate, origin) = match request.from {
        AdoptFrom::Source(AdoptSource::Store) => (None, "store"),
        AdoptFrom::Source(AdoptSource::CpaFile) => (cpa, "cpa_file"),
        AdoptFrom::Path { path } => {
            if cpa.is_some() {
                (cpa, "cpa_file")
            } else {
                (
                    usable_candidate(
                        scan(&path, &account, known_user.as_deref(), true, &mut warnings)?
                            .candidate,
                        &account.token_meta,
                        &mut warnings,
                    ),
                    "backup",
                )
            }
        }
    };
    let mut tokens = stored;
    let mut from = "store";
    if let Some(candidate) = candidate {
        // Unknown ordering is not permission to overwrite an existing pair.
        if freshness(&candidate.account.token_meta, &account.token_meta).is_ge() {
            if candidate.account.email != account.email {
                account.email = candidate.account.email.clone();
                warnings.push(AdoptWarning::EmailUpdated);
            }
            let uncertain = account.token_meta.refresh_uncertain;
            account.token_meta = candidate.account.token_meta;
            // Adoption is not proof of a completed refresh.
            account.token_meta.refresh_uncertain |= uncertain;
            tokens = candidate.tokens;
            from = origin;
        } else {
            warnings.push(AdoptWarning::OlderOrUnorderedCandidateIgnored);
        }
    }
    if from == "store"
        && (account.token_meta.refresh_uncertain
            || account
                .token_meta
                .expires_at
                .is_some_and(|expiry| expiry <= Utc::now()))
    {
        warnings.push(AdoptWarning::StoredTokensMayNeedRelogin);
    }
    if let Source::Cpa { mode, .. } = &mut account.source {
        *mode = ImportMode::Adopted;
    }
    if matches!(
        account.health,
        Health::Cooling {
            reason: HealthReason::LinkStale,
            ..
        }
    ) {
        account.health = Health::Ready;
    }
    account.auth_failures = 0;
    account.auth_probe_pending = false;
    // The save path preserves unrelated health for this ownership transition.
    let saved = store.save_unlocked(&account, &tokens, Some(request.expected_generation))?;
    Ok(AdoptResult {
        id: saved.id,
        generation: saved.generation,
        from: from.into(),
        token_expires_at: saved.token_meta.expires_at,
        last_refresh_at: saved.token_meta.last_refresh_at,
        warnings,
    })
}

/// None sorts before Some; last_refresh is primary, expiry breaks exact ties.
/// More conservative than TokenMeta::compare when one refresh timestamp is absent.
fn freshness(new: &TokenMeta, old: &TokenMeta) -> Ordering {
    (new.last_refresh_at, new.expires_at).cmp(&(old.last_refresh_at, old.expires_at))
}

fn usable_candidate(
    candidate: Option<Candidate>,
    stored: &TokenMeta,
    warnings: &mut Vec<AdoptWarning>,
) -> Option<Candidate> {
    candidate.filter(|candidate| {
        let meta = &candidate.account.token_meta;
        // Respect both source ordering and the store's independent rollback guard.
        let usable = freshness(meta, stored).is_ge()
            && meta.compare(stored).is_some_and(|order| order.is_ge());
        if !usable {
            warnings.push(AdoptWarning::OlderOrUnorderedCandidateIgnored);
        }
        usable
    })
}

fn compatible(account: &Account, candidate: &Account, known_user: Option<&str>) -> bool {
    match (&account.identity, &candidate.identity) {
        (
            Identity::Codex {
                account_id: a,
                user_id: stored_user,
                ..
            },
            Identity::Codex {
                account_id: b,
                user_id: candidate_user,
                ..
            },
        ) => {
            a == b
                && match known_user.or(stored_user.as_deref()) {
                    Some(user) => candidate_user.as_deref() == Some(user),
                    // A workspace may contain multiple users. Without independent
                    // user proof, require the linked-mode workspace/email match.
                    None => {
                        SourceMatch::from(&account.identity)
                            == SourceMatch::from(&candidate.identity)
                    }
                }
        }
        (
            Identity::Claude {
                account_uuid: a,
                organization_uuid: org_a,
                ..
            },
            Identity::Claude {
                account_uuid: b,
                organization_uuid: org_b,
                ..
            },
        ) => a == b && org_a == org_b,
        _ => false,
    }
}

fn relevant(account: &Account, candidate: &Account) -> bool {
    match (&account.identity, &candidate.identity) {
        (
            Identity::Codex {
                account_id: a,
                email: ea,
                ..
            },
            Identity::Codex {
                account_id: b,
                email: eb,
                ..
            },
        ) => a == b || ea == eb,
        (
            Identity::Claude {
                account_uuid: a,
                email: ea,
                ..
            },
            Identity::Claude {
                account_uuid: b,
                email: eb,
                ..
            },
        ) => a == b || ea == eb,
        _ => false,
    }
}

struct ScanResult {
    candidate: Option<Candidate>,
    // An unreadable auth file may be this account during CPA's truncate/write.
    invalid: bool,
}

fn scan(
    path: &Path,
    account: &Account,
    known_user: Option<&str>,
    backup: bool,
    warnings: &mut Vec<AdoptWarning>,
) -> Result<ScanResult, AdoptError> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            warnings.push(if backup {
                AdoptWarning::BackupMissing
            } else {
                AdoptWarning::CpaFileMissing
            });
            return Ok(ScanResult {
                candidate: None,
                invalid: false,
            });
        }
        Err(_) => return Err(AdoptError::SourceIo),
    };
    let paths = if metadata.is_dir() {
        import::paths(path).map_err(|_| AdoptError::SourceIo)?
    } else if backup && metadata.is_file() {
        vec![path.to_path_buf()]
    } else {
        return Err(AdoptError::SourceIo);
    };
    let mut best: Option<Candidate> = None;
    let mut invalid = false;
    for file in paths {
        match import::read(&file, path) {
            Ok(candidate) => {
                // Unrelated auth-dir entries do not authorize adoption of their tokens.
                // A single explicitly supplied backup must itself match.
                if !relevant(account, &candidate.account) {
                    if backup && metadata.is_file() {
                        return Err(AdoptError::IdentityMismatch);
                    }
                    continue;
                }
                if !compatible(account, &candidate.account, known_user) {
                    return Err(AdoptError::IdentityMismatch);
                }
                if best.as_ref().is_some_and(|old| {
                    old.account.identity.canonical_key()
                        != candidate.account.identity.canonical_key()
                }) {
                    return Err(AdoptError::AmbiguousIdentity);
                }
                if best.as_ref().is_none_or(|old| {
                    freshness(&candidate.account.token_meta, &old.account.token_meta).is_gt()
                }) {
                    best = Some(candidate);
                }
            }
            Err(ReadError::UnknownType) => {}
            Err(ReadError::InvalidData | ReadError::Io) => invalid = true,
        }
    }
    if invalid {
        warnings.push(if backup {
            AdoptWarning::BackupUnreadable
        } else {
            AdoptWarning::CpaFileUnreadable
        });
    }
    if best.is_none() && !invalid {
        warnings.push(if backup {
            AdoptWarning::BackupMissing
        } else {
            AdoptWarning::CpaFileMissing
        });
    }
    Ok(ScanResult {
        candidate: best,
        invalid,
    })
}

/// Source only. Do not execute during preparation or synthetic verification.
fn production_probe() -> ProbeState {
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    struct Snapshot(HANDLE);
    impl Drop for Snapshot {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    // No command lines, environment, executable paths, ports, or task inspection.
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(handle) => Snapshot(handle),
        Err(_) => return ProbeState::Unknown,
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    loop {
        if let Err(error) = next {
            return if error.code() == ERROR_NO_MORE_FILES.to_hresult() {
                ProbeState::Stopped
            } else {
                ProbeState::Unknown
            };
        }
        let end = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
        if name.eq_ignore_ascii_case("cli-proxy-api.exe")
            || name.eq_ignore_ascii_case("cliproxyapi.exe")
        {
            return ProbeState::Running;
        }
        next = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
}
