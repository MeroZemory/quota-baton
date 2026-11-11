//! Reset-credit facts only; these types confer no authority to redeem a grant.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetCreditInventory {
    pub state: ResetCreditState,
    pub observed_at: Option<String>,
    /// Provider-reported available count. Selection additionally requires complete,
    /// eligible details; this count alone never authorizes consumption.
    pub available_count: Option<u64>,
    pub grants: Vec<ResetCreditGrant>,
    pub refusal: Option<ResetCreditRefusal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeResetInventory>,
}

impl Default for ResetCreditInventory {
    fn default() -> Self {
        Self {
            state: ResetCreditState::Unknown,
            observed_at: None,
            available_count: None,
            grants: Vec::new(),
            refusal: Some(ResetCreditRefusal::Unknown),
            claude: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetCreditState {
    Unknown,
    Unsupported,
    Fresh,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetCreditGrant {
    pub id: String,
    pub expires_at: ResetCreditExpiry,
    pub status: ResetCreditStatus,
    pub reset_type: ResetCreditType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResetCreditExpiry {
    At { at: String },
    Never,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetCreditStatus {
    Available,
    Redeeming,
    Redeemed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetCreditType {
    CodexRateLimits,
    ClaudeRateLimits,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetCreditRefusal {
    Unknown,
    Unsupported,
    Stale,
    NoCredits,
    Incomplete,
    UnknownEligibility,
    UnknownExpiry,
    Malformed,
    DuplicateIds,
    Ineligible,
    NotUsable,
}

impl std::fmt::Display for ResetCreditRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unknown => "reset-credit inventory is unknown",
            Self::Unsupported => "reset credits are unsupported",
            Self::Stale => "reset-credit inventory needs refresh",
            Self::NoCredits => "no usable reset credits",
            Self::Incomplete => "reset-credit details do not prove completeness",
            Self::UnknownEligibility => "reset-credit eligibility is unknown",
            Self::UnknownExpiry => "reset-credit expiry is unknown",
            Self::Malformed => "reset-credit facts are malformed",
            Self::DuplicateIds => "reset-credit identifiers are duplicated",
            Self::Ineligible => "account is ineligible for reset credits",
            Self::NotUsable => "reset credit is not usable now",
        })
    }
}

impl std::error::Error for ResetCreditRefusal {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeResetInventory {
    pub eligible: bool,
    pub ineligible_reason: Option<String>,
    pub at_limit: Option<bool>,
    pub exhausted: Vec<String>,
    pub next_grant_id: Option<String>,
    pub weekly_resets_at: Option<String>,
    pub cooldown_until: Option<String>,
    pub grants: Vec<ClaudeResetGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeResetGrant {
    pub id: String,
    pub label: String,
    pub grant_type: Option<String>,
    pub resets_total: Option<u64>,
    pub resets_left: u64,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub clears: Vec<String>,
    pub blocking: Vec<String>,
    pub paused: bool,
    pub usable_now: bool,
    pub use_requires_limit: bool,
    pub gives_back_weekly: bool,
    pub percent_used: std::collections::BTreeMap<String, serde_json::Number>,
    pub unavailable_reason: Option<String>,
}
