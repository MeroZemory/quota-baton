//! Persisted receipt and journal schema shared with ordinary account fencing.
use super::{Account, Provider, StoreError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Receipt {
    pub operation: String,
    pub state: ReceiptState,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReceiptState {
    Installed,
    Ready,
    Started,
    Finished { result: Verification },
    Superseded,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Verification {
    pub ok: bool,
    pub token_expires_at: Option<DateTime<Utc>>,
}
impl Receipt {
    pub fn fenced(&self) -> bool {
        !matches!(
            self.state,
            ReceiptState::Finished { .. } | ReceiptState::Superseded
        )
    }
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.operation.len() != 64 || !self.operation.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::InvalidData);
        }
        Ok(())
    }
    pub fn permits(&self, next: &Self) -> bool {
        if !self.fenced() && next.state == ReceiptState::Installed {
            return self.operation != next.operation;
        }
        self.operation == next.operation
            && matches!(
                (&self.state, &next.state),
                (ReceiptState::Installed, ReceiptState::Ready)
                    | (ReceiptState::Ready, ReceiptState::Started)
                    | (ReceiptState::Started, ReceiptState::Finished { .. })
                    | (
                        ReceiptState::Installed | ReceiptState::Ready | ReceiptState::Started,
                        ReceiptState::Superseded
                    )
            )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SourceProof {
    pub path: PathBuf,
    pub volume: u32,
    pub index: u64,
    pub modified: DateTime<Utc>,
    pub sha256: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Journal {
    pub schema: u8,
    pub operation: String,
    pub inspect_digest: String,
    pub provider: Provider,
    pub requested_path: PathBuf,
    pub source: SourceProof,
    pub replace: bool,
    pub keep_source: bool,
    pub destination: PathBuf,
    pub account: Account,
    pub expected: Option<u64>,
    /// Existing account DPAPI Tokens format; dropped on durable completion.
    pub protected_tokens: Option<String>,
    pub retired: bool,
    #[serde(default)]
    pub retirement_started: bool,
    #[serde(default)]
    pub superseded: bool,
    pub result: Option<Verification>,
}
impl Journal {
    pub fn validate(&self) -> Result<(), StoreError> {
        Receipt {
            operation: self.operation.clone(),
            state: ReceiptState::Installed,
        }
        .validate()?;
        let filename = if self.provider == Provider::Codex {
            "auth.json"
        } else {
            ".credentials.json"
        };
        let prefix = format!("{filename}.taken-over-by-quota-baton-");
        let suffix = self
            .destination
            .file_name()
            .and_then(|v| v.to_str())
            .and_then(|v| v.strip_prefix(&prefix));
        if self.schema != 1
            || self.inspect_digest.len() != 64
            || !self.inspect_digest.bytes().all(|b| b.is_ascii_hexdigit())
            || self.source.sha256.len() != 64
            || !self.source.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || self.operation != digest(self.inspect_digest.as_bytes())
            || !self.source.path.is_absolute()
            || self.source.path.file_name().is_none_or(|n| n != filename)
            || self.destination.parent() != self.source.path.parent()
            || suffix.is_none_or(|s| s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()))
            || self.account.provider != self.provider
            || self.account.id != self.account.identity.id()
            || self.account.identity.provider() != self.provider
            || !matches!(&self.account.source, super::Source::Takeover {from,..} if from == &self.source.path)
            || (self.result.is_some()
                && ((!self.retired && !self.superseded) || self.protected_tokens.is_some()))
            || (self.superseded && self.result.as_ref().is_none_or(|r| r.ok))
            || (self.result.is_none() && self.protected_tokens.is_none())
        {
            return Err(StoreError::InvalidData);
        }
        Ok(())
    }
}

pub(super) fn digest(value: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(value)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
