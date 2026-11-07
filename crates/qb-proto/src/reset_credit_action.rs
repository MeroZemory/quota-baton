//! Explicit, account-bound reset-credit confirmation. Unrelated to paid credits.
use crate::reset_credits::{ResetCreditGrant, ResetCreditInventory};
use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetCreditResult {
    pub account: String,
    pub outcome: ResetCreditOutcome,
    pub inventory: ResetCreditInventory,
    pub refresh_failed: bool,
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
