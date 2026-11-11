//! Explicit, account-bound reset-credit confirmation. Unrelated to paid credits.
use crate::reset_credits::{ResetCreditGrant, ResetCreditInventory};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetCreditPreviewRequest {
    pub grant_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetCreditPreview {
    pub account: String,
    pub label: String,
    pub email: String,
    pub preview_token: String,
    pub expires_at: String,
    pub grant: ResetCreditGrant,
    pub inventory: ResetCreditInventory,
    pub effect: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetCreditCommitRequest {
    pub preview_token: String,
    pub confirm: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetCreditOutcome {
    Reset,
    AlreadyRedeemed,
    NothingToReset,
    NoCredit,
    Unknown,
    Refused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetCreditResult {
    pub account: String,
    pub outcome: ResetCreditOutcome,
    pub inventory: ResetCreditInventory,
    pub refresh_failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeResetResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeResetResult {
    pub result: String,
    pub reason: Option<String>,
    pub resets_left: Option<u64>,
    pub cleared: Vec<String>,
    pub weekly_resets_at: Option<String>,
    pub cooldown_until: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetCreditErrorCode {
    #[serde(rename = "reset_credit_stale")]
    Stale,
    #[serde(rename = "reset_credit_unknown")]
    Unknown,
    #[serde(rename = "reset_credit_busy")]
    Busy,
    #[serde(rename = "reset_credit_unsupported")]
    Unsupported,
    #[serde(rename = "reset_credit_ineligible")]
    Ineligible,
    #[serde(rename = "reset_credit_unavailable")]
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetCreditErrorReply {
    pub error: ResetCreditErrorDetail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetCreditErrorDetail {
    pub code: ResetCreditErrorCode,
    pub message: String,
}
