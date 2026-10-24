//! Synthetic JSON fixture setup only; accounts are independent OAuth records.
//! This deliberately provides no CPA linking or reload behavior.

use crate::accounts::{AccountStore, Identity, StoreError, TokenMeta, Tokens, UserState};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::{fs, path::Path};
use zeroize::Zeroizing;

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "type")]
    provider: String,
    email: String,
    account_id: Option<String>,
    account_uuid: Option<String>,
    organization_uuid: Option<String>,
    plan_type: Option<String>,
    expired: Option<DateTime<Utc>>,
    last_refresh: Option<DateTime<Utc>>,
    #[serde(flatten)]
    tokens: Tokens,
}

pub(crate) fn seed_account_fixtures(
    store: &AccountStore,
    dir: &Path,
    selectors: &[String],
    dry_run: bool,
) -> Result<(), StoreError> {
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
    for path in paths {
        let bytes = Zeroizing::new(fs::read(path).map_err(|_| StoreError::Io)?);
        let fixture: Fixture =
            serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidData)?;
        if fixture.email.is_empty()
            || fixture.tokens.access_token.is_empty()
            || fixture.tokens.refresh_token.is_empty()
        {
            return Err(StoreError::InvalidData);
        }
        let identity = match fixture.provider.as_str() {
            "codex" => {
                let account_id = fixture
                    .account_id
                    .filter(|id| !id.is_empty())
                    .ok_or(StoreError::InvalidData)?;
                Identity::Codex {
                    user_id: crate::accounts::login::codex_user_id(&fixture.tokens, &account_id),
                    account_id,
                    email: fixture.email.clone(),
                }
            }
            "claude" => Identity::Claude {
                account_uuid: fixture
                    .account_uuid
                    .filter(|id| !id.is_empty())
                    .ok_or(StoreError::InvalidData)?,
                organization_uuid: fixture.organization_uuid.filter(|id| !id.is_empty()),
                email: fixture.email.clone(),
            },
            _ => return Err(StoreError::InvalidData),
        };
        let mut account = crate::accounts::health::tests::fixture();
        account.id = identity.id();
        account.provider = identity.provider();
        account.identity = identity;
        account.email = fixture.email;
        account.plan = fixture.plan_type;
        account.token_meta = TokenMeta {
            expires_at: fixture.expired,
            last_refresh_at: fixture.last_refresh,
            refresh_uncertain: false,
        };
        let old = store.list()?.into_iter().find(|old| old.id == account.id);
        account.generation = old.as_ref().map_or(0, |old| old.generation);
        account.user_state = if selectors
            .iter()
            .any(|selector| selector == &format!("{}:{}", account.provider.as_str(), account.email))
        {
            UserState::Active
        } else {
            old.as_ref()
                .map_or(UserState::Disabled, |old| old.user_state)
        };
        if !dry_run {
            store.save(&account, &fixture.tokens, old.map(|old| old.generation))?;
        }
    }
    Ok(())
}
