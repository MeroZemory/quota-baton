//! Login writes share the account transaction and preserve user-managed state.
use super::{
    Account, AccountStore, Health, HealthReason, Identity, ImportMode, Source, StoreError, Tokens,
};

// Shared with the OAuth/CLI error mapping; fixed text contains no source data.
pub const IDENTITY_CONFLICT_REASON: &str = "계정의 사용자를 확인할 수 없거나 같은 사용자 계정이 중복되어 있습니다. CPA id_token의 사용자 정보와 연결 설정을 확인해 주세요.";

#[derive(Debug, thiserror::Error)]
pub enum LoginStoreError {
    #[error(
        "CPA 연결 계정이 이미 있습니다. 서로 다른 토큰 계열을 만들지 않으려면 연결 교체를 명시해 주세요."
    )]
    LinkedConflict,
    #[error("{}", IDENTITY_CONFLICT_REASON)]
    IdentityConflict,
    #[error("로그인 계정을 저장하지 못했습니다.")]
    Store,
}
impl From<StoreError> for LoginStoreError {
    fn from(_: StoreError) -> Self {
        Self::Store
    }
}

pub struct SavedLogin {
    pub account: Account,
    pub replaced_linked: bool,
}

fn ambiguous_legacy(existing: &Identity, incoming: &Identity) -> bool {
    let same_id = match (existing, incoming) {
        (
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
        ) => a == b,
        (
            Identity::Claude {
                account_uuid: a,
                organization_uuid: None,
                ..
            },
            Identity::Claude {
                account_uuid: b,
                organization_uuid: Some(_),
                ..
            },
        ) => a == b,
        _ => false,
    };
    same_id && (matches!(existing, Identity::Codex { .. }) || !matches(existing, incoming))
}

pub(crate) fn matches(existing: &Identity, incoming: &Identity) -> bool {
    if existing.canonical_key() == incoming.canonical_key() {
        return true;
    }
    match (existing, incoming) {
        // Preserve same-email compatibility for existing migration journals.
        // Changed-email legacy CPA recovery must resolve the current CPA user;
        // save refuses unresolved Codex records before this fallback is used.
        (
            Identity::Codex {
                account_id: a,
                email: e,
                user_id: None,
            },
            Identity::Codex {
                account_id: b,
                email: f,
                user_id: Some(_),
            },
        ) => a == b && e == f,
        (
            Identity::Claude {
                account_uuid: a,
                organization_uuid: None,
                email: e,
            },
            Identity::Claude {
                account_uuid: b,
                organization_uuid: Some(_),
                email: f,
            },
        ) => a == b && e == f,
        _ => false,
    }
}

pub fn save(
    store: &AccountStore,
    account: &Account,
    tokens: &Tokens,
    replace_linked: bool,
) -> Result<SavedLogin, LoginStoreError> {
    let _guard = store.transaction()?;
    store.check_login_migration_clear()?;
    let mut existing = store.list_unlocked()?;
    let persisted_identities: std::collections::BTreeMap<_, _> = existing
        .iter()
        .map(|old| (old.id.clone(), old.identity.clone()))
        .collect();
    for old in &mut existing {
        if let (
            Identity::Codex {
                account_id: a,
                user_id: None,
                ..
            },
            Identity::Codex { account_id: b, .. },
        ) = (&old.identity, &account.identity)
            && a == b
        {
            if matches!(
                &old.source,
                Source::Cpa {
                    mode: ImportMode::Linked,
                    ..
                }
            ) {
                // Linked recovery still requires current CPA evidence, never stored tokens.
                if let Ok(
                    identity @ Identity::Codex {
                        user_id: Some(_), ..
                    },
                ) = super::linked::resolve_identity(old)
                {
                    old.identity = identity;
                }
            } else {
                // Owned/adopted recovery uses only the current encrypted stored token.
                // codex_user_id checks that its claim belongs to this workspace.
                let user = store
                    .login_codex_user_unlocked(old, a)?
                    .ok_or(LoginStoreError::IdentityConflict)?;
                let mut resolved = old.identity.clone();
                if let Identity::Codex { user_id, .. } = &mut resolved {
                    *user_id = Some(user);
                }
                // Virtual matching only; owned re-login retains the persisted identity/key.
                old.identity = resolved;
            }
        }
    }
    // Unresolved Codex users are ambiguous even with the same email. Never
    // create or replace a family based on workspace/email alone.
    if existing
        .iter()
        .any(|old| ambiguous_legacy(&old.identity, &account.identity))
    {
        return Err(LoginStoreError::IdentityConflict);
    }
    let matches: Vec<_> = existing
        .into_iter()
        .filter(|old| matches(&old.identity, &account.identity))
        .collect();
    if matches.len() > 1 {
        return Err(LoginStoreError::IdentityConflict);
    }
    let old = matches.first();
    let replaced_linked = old.is_some_and(|old| {
        matches!(
            old.source,
            Source::Cpa {
                mode: ImportMode::Linked,
                ..
            }
        )
    });
    if replaced_linked && !replace_linked {
        return Err(LoginStoreError::LinkedConflict);
    }
    let mut next = account.clone();
    next.source = Source::Oauth;
    if let Some(old) = old {
        if !replaced_linked {
            // Match on proven user identity above, but keep the persisted hash key
            // so existing bindings and history continue to name this account.
            next.id = old.id.clone();
            next.identity = persisted_identities
                .get(&old.id)
                .ok_or(LoginStoreError::Store)?
                .clone();
        }
        next.label = old.label.clone();
        next.user_state = old.user_state;
        next.drain = old.drain;
        next.allow_credits = old.allow_credits;
        next.health = old.health.clone();
        next.quota_cooling = old.quota_cooling.clone();
        next.quota_credit_releasable = old.quota_credit_releasable;
        if matches!(
            next.health,
            Health::NeedsRelogin { .. }
                | Health::Cooling {
                    reason: HealthReason::Auth | HealthReason::StoreError | HealthReason::LinkStale,
                    ..
                }
        ) {
            next.health = Health::Ready;
            next.quota_cooling = None;
            next.quota_credit_releasable = false;
        }
        next.auth_failures = 0;
        next.payment_failures = old.payment_failures;
        next.payment_until = old.payment_until;
        next.expire_health(chrono::Utc::now());
        next.auth_probe_pending = false;
    }
    let same_file = old.filter(|old| old.id == next.id);
    next.generation = same_file.map_or(0, |old| old.generation);
    let migration = old.filter(|old| old.id != next.id);
    if let Some(old) = migration {
        store.begin_login_migration(&old.id, &next.id, &next.identity)?;
    }
    let updated =
        match store.save_login_unlocked(&next, tokens, same_file.map(|old| old.generation)) {
            Ok(updated) => updated,
            Err(error) => {
                if migration.is_some() {
                    store.finish_login_migration()?;
                }
                return Err(error.into());
            }
        };
    if let Some(old) = old.filter(|old| old.id != next.id) {
        // Migration never exposes two token families to another store reader:
        // both file mutations run under the same transaction and core lease.
        // If removing the legacy file fails, roll back the new file.
        if store.remove_login_record_unlocked(&old.id).is_err() {
            store.remove_login_record_unlocked(&updated.id)?;
            store.finish_login_migration()?;
            return Err(LoginStoreError::Store);
        }
        store.finish_login_migration()?;
    }
    Ok(SavedLogin {
        account: updated,
        replaced_linked,
    })
}

// Identity claims only: this does not authenticate or verify the JWT signature.
pub(crate) fn codex_user_id(tokens: &Tokens, account_id: &str) -> Option<String> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use zeroize::Zeroizing;

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
