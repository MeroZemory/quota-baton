pub mod adopt;
mod crypto;
pub mod health;
pub mod import;
mod lease;
pub mod linked;
pub mod login;
mod store;
#[cfg(not(feature = "testkit"))]
pub(crate) mod takeover;
#[cfg(feature = "testkit")]
#[doc(hidden)]
pub mod takeover;
pub(crate) mod takeover_native;

use chrono::{DateTime, Utc};
pub use health::HealthSignal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
pub use store::{AccountObserver, AccountStore, StoreError};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum Identity {
    Claude {
        account_uuid: String,
        organization_uuid: Option<String>,
        email: String,
    },
    Codex {
        account_id: String,
        email: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_id: Option<String>,
    },
}

impl Identity {
    // 구분자를 포함한 JSON 배열로 해시 입력의 모호성을 제거한다.
    pub fn canonical_key(&self) -> String {
        match self {
            Self::Claude {
                account_uuid,
                organization_uuid: Some(org),
                ..
            } => {
                serde_json::to_string(&("claude", org, account_uuid)).expect("string serialization")
            }
            Self::Claude {
                account_uuid,
                email,
                ..
            } => serde_json::to_string(&("claude", account_uuid, email))
                .expect("string serialization"),
            Self::Codex {
                account_id,
                user_id: Some(user_id),
                ..
            } => serde_json::to_string(&("codex", account_id, "user", user_id))
                .expect("string serialization"),
            Self::Codex {
                account_id, email, ..
            } => {
                serde_json::to_string(&("codex", account_id, email)).expect("string serialization")
            }
        }
    }
    pub fn id(&self) -> String {
        let digest = Sha256::digest(self.canonical_key().as_bytes());
        let prefix = match self {
            Self::Claude { .. } => "cl",
            Self::Codex { .. } => "cx",
        };
        format!(
            "{prefix}-{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2]
        )
    }
    pub fn provider(&self) -> Provider {
        match self {
            Self::Claude { .. } => Provider::Claude,
            Self::Codex { .. } => Provider::Codex,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourceMatch {
    Claude {
        account_uuid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        organization_uuid: Option<String>,
        email: String,
    },
    Codex {
        account_id: String,
        email: String,
    },
}

impl From<&Identity> for SourceMatch {
    fn from(identity: &Identity) -> Self {
        match identity {
            Identity::Claude {
                account_uuid,
                organization_uuid,
                email,
            } => Self::Claude {
                account_uuid: account_uuid.clone(),
                organization_uuid: organization_uuid.clone(),
                email: email.clone(),
            },
            Identity::Codex {
                account_id, email, ..
            } => Self::Codex {
                account_id: account_id.clone(),
                email: email.clone(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportMode {
    Linked,
    Adopted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    Oauth,
    Takeover {
        from: PathBuf,
        at: DateTime<Utc>,
    },
    Cpa {
        mode: ImportMode,
        dir: PathBuf,
        r#match: SourceMatch,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserState {
    Active,
    Paused,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthReason {
    Quota,
    Rate,
    Auth,
    Permission,
    Payment,
    StoreError,
    LinkStale,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Health {
    Ready,
    Cooling {
        reason: HealthReason,
        until: DateTime<Utc>,
        scope: String,
    },
    NeedsRelogin {
        reason: String,
        since: DateTime<Utc>,
    },
}

/// Persisted evidence required to release quota cooling before its deadline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaCooling {
    pub window: String,
    pub reset_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenMeta {
    pub expires_at: Option<DateTime<Utc>>,
    pub last_refresh_at: Option<DateTime<Utc>>,
    pub refresh_uncertain: bool,
}

impl TokenMeta {
    pub(crate) fn newer_than(&self, old: &Self) -> bool {
        self.compare(old).is_some_and(|order| order.is_gt())
    }
    pub(crate) fn compare(&self, old: &Self) -> Option<std::cmp::Ordering> {
        match (self.last_refresh_at, old.last_refresh_at) {
            (Some(new), Some(previous)) if new != previous => Some(new.cmp(&previous)),
            _ => match (self.expires_at, old.expires_at) {
                (Some(new), Some(previous)) => Some(new.cmp(&previous)),
                (Some(_), None) => Some(std::cmp::Ordering::Greater),
                _ => None,
            },
        }
    }
}

// 공개 메타데이터에는 암호화된 비밀도 넣지 않는다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub schema: u32,
    pub id: String,
    pub provider: Provider,
    pub label: String,
    pub email: String,
    pub plan: Option<String>,
    pub identity: Identity,
    pub source: Source,
    pub user_state: UserState,
    pub health: Health,
    #[serde(default)]
    pub auth_failures: u32,
    /// Repeated billing rejections back off for 6/12/24 hours across restarts.
    #[serde(default)]
    pub payment_failures: u32,
    /// Funding remains blocked even if another health reason temporarily wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment_until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub auth_probe_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_cooling: Option<QuotaCooling>,
    /// A subscription observation, never an upstream rejection, caused this hold.
    #[serde(default)]
    pub quota_credit_releasable: bool,
    #[serde(default)]
    pub drain: bool,
    #[serde(default)]
    pub allow_credits: bool,
    pub generation: u64,
    pub token_meta: TokenMeta,
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokens(<redacted>)")
    }
}
