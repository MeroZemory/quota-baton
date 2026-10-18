use super::{
    Account, AccountStore, Health, Identity, ImportMode, Provider, Source, SourceMatch, StoreError,
    TokenMeta, Tokens, UserState,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportAction {
    Created,
    Updated,
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    UnknownType,
    InvalidData,
    OlderToken,
    OwnedAccount,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportRow {
    pub id: Option<String>,
    pub provider: Option<Provider>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub user_state: Option<UserState>,
    pub action: ImportAction,
    pub reason: Option<SkipReason>,
    pub stored_timestamp: Option<DateTime<Utc>>,
    pub file_timestamp: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct CpaFile {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    tokens: Tokens,
    email: String,
    account_uuid: Option<String>,
    organization_uuid: Option<String>,
    organization_name: Option<String>,
    account_id: Option<String>,
    plan_type: Option<String>,
    expired: Option<DateTime<Utc>>,
    last_refresh: Option<DateTime<Utc>>,
}

pub(crate) struct Candidate {
    pub account: Account,
    pub tokens: Tokens,
}

pub(crate) enum ReadError {
    Io,
    InvalidData,
    UnknownType,
}

// Identity claims only: this does not authenticate or verify the JWT signature.
pub(crate) fn codex_user_id(tokens: &Tokens, account_id: &str) -> Option<String> {
    let mut parts = tokens.id_token.as_deref()?.split('.');
    parts.next()?;
    let payload = parts.next()?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let payload = Zeroizing::new(URL_SAFE_NO_PAD.decode(payload).ok()?);
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    let auth = &claims["https://api.openai.com/auth"];
    if auth["chatgpt_account_id"].as_str()? != account_id {
        return None;
    }
    auth["chatgpt_user_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

pub(crate) fn paths(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).map_err(|_| StoreError::Io)? {
        let entry = entry.map_err(|_| StoreError::Io)?;
        if entry.file_type().map_err(|_| StoreError::Io)?.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

pub(crate) fn read(path: &Path, dir: &Path) -> Result<Candidate, ReadError> {
    let bytes = Zeroizing::new(fs::read(path).map_err(|_| ReadError::Io)?);
    // 지원 여부만 먼저 읽어 미지원 형식에 토큰 필드를 요구하지 않는다.
    #[derive(Deserialize)]
    struct Kind {
        #[serde(rename = "type")]
        kind: String,
    }
    let kind: Kind = serde_json::from_slice(&bytes).map_err(|_| ReadError::InvalidData)?;
    if kind.kind != "claude" && kind.kind != "codex" {
        return Err(ReadError::UnknownType);
    }
    let file: CpaFile = serde_json::from_slice(&bytes).map_err(|_| ReadError::InvalidData)?;
    if file.email.is_empty()
        || file.tokens.access_token.is_empty()
        || file.tokens.refresh_token.is_empty()
    {
        return Err(ReadError::InvalidData);
    }
    let (identity, label, plan) = match file.kind.as_str() {
        "claude" => {
            let uuid = file
                .account_uuid
                .filter(|s| !s.is_empty())
                .ok_or(ReadError::InvalidData)?;
            (
                Identity::Claude {
                    account_uuid: uuid,
                    organization_uuid: file.organization_uuid.filter(|s| !s.is_empty()),
                    email: file.email.clone(),
                },
                file.organization_name.unwrap_or_else(|| file.email.clone()),
                None,
            )
        }
        "codex" => {
            let id = file
                .account_id
                .filter(|s| !s.is_empty())
                .ok_or(ReadError::InvalidData)?;
            (
                Identity::Codex {
                    user_id: codex_user_id(&file.tokens, &id),
                    account_id: id,
                    email: file.email.clone(),
                },
                file.email.clone(),
                file.plan_type,
            )
        }
        _ => return Err(ReadError::UnknownType),
    };
    let account = Account {
        schema: 1,
        id: identity.id(),
        provider: identity.provider(),
        label,
        email: file.email,
        plan,
        source: Source::Cpa {
            mode: ImportMode::Linked,
            dir: dir.to_path_buf(),
            r#match: SourceMatch::from(&identity),
        },
        identity,
        user_state: UserState::Disabled,
        health: Health::Ready,
        auth_failures: 0,
        payment_failures: 0,
        payment_until: None,
        auth_probe_pending: false,
        quota_cooling: None,
        drain: false,
        allow_credits: false,
        generation: 0,
        token_meta: TokenMeta {
            expires_at: file.expired,
            last_refresh_at: file.last_refresh,
            refresh_uncertain: false,
        },
    };
    Ok(Candidate {
        account,
        tokens: file.tokens,
    })
}

pub fn run(
    store: &AccountStore,
    dir: &Path,
    activate: &[String],
    dry_run: bool,
) -> Result<Vec<ImportRow>, StoreError> {
    let _guard = store.transaction()?;
    execute(Some(store), store.list_unlocked()?, dir, activate, dry_run)
}

// 홈이 아직 없어도 디렉터리나 잠금 파일을 생성하지 않는다.
pub fn preview(home: &Path, dir: &Path, activate: &[String]) -> Result<Vec<ImportRow>, StoreError> {
    execute(
        None,
        AccountStore::list_metadata(home)?,
        dir,
        activate,
        true,
    )
}

fn execute(
    store: Option<&AccountStore>,
    existing: Vec<Account>,
    dir: &Path,
    activate: &[String],
    dry_run: bool,
) -> Result<Vec<ImportRow>, StoreError> {
    let dir = fs::canonicalize(dir).map_err(|_| StoreError::Io)?;
    let mut accounts: BTreeMap<String, Account> = existing
        .into_iter()
        .map(|account| (account.id.clone(), account))
        .collect();
    let mut rows = Vec::new();
    'files: for path in paths(&dir)? {
        let Candidate {
            mut account,
            tokens,
        } = match read(&path, &dir) {
            Ok(candidate) => candidate,
            Err(error) => {
                rows.push(ImportRow {
                    id: None,
                    provider: None,
                    email: None,
                    plan: None,
                    expires_at: None,
                    user_state: None,
                    action: ImportAction::Skipped,
                    reason: Some(match error {
                        ReadError::UnknownType => SkipReason::UnknownType,
                        _ => SkipReason::InvalidData,
                    }),
                    stored_timestamp: None,
                    file_timestamp: None,
                });
                continue;
            }
        };
        // Keep a resolved legacy record's key until login's journaled migration.
        // Re-import must not expose the same family under a second id.
        let mut legacy = Vec::new();
        for old in accounts.values() {
            if let (
                Identity::Codex {
                    account_id: a,
                    user_id: Some(_),
                    ..
                },
                Identity::Codex {
                    account_id: b,
                    user_id: None,
                    ..
                },
            ) = (&old.identity, &account.identity)
                && a == b
            {
                // Losing the claim must not recreate a known family by email.
                rows.push(row(
                    &account,
                    ImportAction::Skipped,
                    Some(SkipReason::InvalidData),
                ));
                continue 'files;
            }
            if let (
                Identity::Codex {
                    account_id: a,
                    user_id: None,
                    ..
                },
                Identity::Codex {
                    account_id: b,
                    user_id: Some(_),
                    ..
                },
            ) = (&old.identity, &account.identity)
                && a == b
            {
                // An owned legacy record cannot be resolved through CPA. Never
                // recreate its possible token family under a resolved key.
                if owned(old) {
                    legacy.push(old);
                    continue;
                }
                let identity = match super::linked::resolve_identity(old) {
                    Ok(identity) => identity,
                    Err(_) => {
                        rows.push(row(
                            &account,
                            ImportAction::Skipped,
                            Some(SkipReason::InvalidData),
                        ));
                        continue 'files;
                    }
                };
                if matches!(identity, Identity::Codex { user_id: None, .. }) {
                    rows.push(row(
                        &account,
                        ImportAction::Skipped,
                        Some(SkipReason::InvalidData),
                    ));
                    continue 'files;
                }
                if identity.canonical_key() == account.identity.canonical_key() {
                    legacy.push(old);
                }
            }
        }
        if legacy.len() > 1 || (!legacy.is_empty() && accounts.contains_key(&account.id)) {
            rows.push(row(
                &account,
                ImportAction::Skipped,
                Some(SkipReason::InvalidData),
            ));
            continue 'files;
        }
        if let Some(old) = legacy.first() {
            account.id = old.id.clone();
            account.identity = old.identity.clone();
            account.source = old.source.clone();
        }
        let old = accounts.get(&account.id);
        if old.is_some_and(|old| old.identity.canonical_key() != account.identity.canonical_key()) {
            rows.push(row(
                &account,
                ImportAction::Skipped,
                Some(SkipReason::InvalidData),
            ));
            continue 'files;
        }
        let activated = activate
            .iter()
            .any(|value| value == &format!("{}:{}", account.provider.as_str(), account.email));
        let mut action = if old.is_some() {
            ImportAction::Updated
        } else {
            ImportAction::Created
        };
        let mut reason = None;
        let mut preserved_tokens = None;
        let mut stored_timestamp = None;
        let mut file_timestamp = None;
        let mut write_activation = false;
        if let Some(old) = old.filter(|old| owned(old)) {
            account = old.clone();
            action = ImportAction::Skipped;
            reason = Some(SkipReason::OwnedAccount);
            if activated && old.user_state != UserState::Active {
                write_activation = true;
                if !dry_run {
                    preserved_tokens = Some(
                        store
                            .ok_or(StoreError::Synchronization)?
                            .tokens_unlocked(old)?,
                    );
                }
            }
        } else if let Some(old) = old {
            account.generation = old.generation;
            account.user_state = old.user_state;
            account.label = old.label.clone();
            account.health = old.health.clone();
            account.auth_failures = old.auth_failures;
            account.payment_failures = old.payment_failures;
            account.payment_until = old.payment_until;
            account.auth_probe_pending = old.auth_probe_pending;
            account.quota_cooling = old.quota_cooling.clone();
            account.drain = old.drain;
            account.allow_credits = old.allow_credits;
            if account.token_meta.newer_than(&old.token_meta) {
                account.apply_health_signal(super::HealthSignal::NewerLinkedToken, Utc::now());
            }
            if account
                .token_meta
                .compare(&old.token_meta)
                .is_some_and(|order| order.is_lt())
            {
                let refresh_differs = account
                    .token_meta
                    .last_refresh_at
                    .zip(old.token_meta.last_refresh_at)
                    .is_some_and(|(a, b)| a != b);
                stored_timestamp = if refresh_differs {
                    old.token_meta.last_refresh_at
                } else {
                    old.token_meta.expires_at
                };
                file_timestamp = if refresh_differs {
                    account.token_meta.last_refresh_at
                } else {
                    account.token_meta.expires_at
                };
                account = old.clone();
                action = ImportAction::Skipped;
                reason = Some(SkipReason::OlderToken);
                if activated && old.user_state != UserState::Active {
                    write_activation = true;
                    if !dry_run {
                        preserved_tokens = Some(
                            store
                                .ok_or(StoreError::Synchronization)?
                                .tokens_unlocked(old)?,
                        );
                    }
                }
            }
        }
        if activated {
            account.user_state = UserState::Active;
        }
        if !dry_run && (action != ImportAction::Skipped || write_activation) {
            account = store.ok_or(StoreError::Synchronization)?.save_unlocked(
                &account,
                preserved_tokens.as_ref().unwrap_or(&tokens),
                old.map(|old| old.generation),
            )?;
        }
        let mut row = row(&account, action, reason);
        row.stored_timestamp = stored_timestamp;
        row.file_timestamp = file_timestamp;
        rows.push(row);
        accounts.insert(account.id.clone(), account);
    }
    Ok(rows)
}

fn owned(account: &Account) -> bool {
    matches!(
        account.source,
        Source::Oauth
            | Source::Takeover { .. }
            | Source::Cpa {
                mode: ImportMode::Adopted,
                ..
            }
    )
}

fn row(account: &Account, action: ImportAction, reason: Option<SkipReason>) -> ImportRow {
    ImportRow {
        id: Some(account.id.clone()),
        provider: Some(account.provider),
        email: Some(account.email.clone()),
        plan: account.plan.clone(),
        expires_at: account.token_meta.expires_at,
        user_state: Some(account.user_state),
        action,
        reason,
        stored_timestamp: None,
        file_timestamp: None,
    }
}
