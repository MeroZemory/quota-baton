//! Proof-free move proposal inputs and decisions. Native permits remain private
//! to the breaker owner, which rechecks its revision before committing a move.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::super::move_guard::{Limits, Refusal};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementInput {
    pub at: DateTime<Utc>,
    pub committed_age_ns: u64,
    pub from: String,
    pub departure_evidence: u64,
    pub to: String,
    pub destination_evidence: u64,
    pub emergency_return: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncidentInput {
    pub at: DateTime<Utc>,
    pub shared: Option<(String, u64)>,
}

/// Recorded contents of the selection owner's scoped sole-target judgement.
/// This is descriptive data, not a replacement for that owner's live authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoleTargetInput {
    pub from: String,
    pub to: String,
    pub model: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitInputs {
    pub limits: Limits,
    /// Breaker time after its ordinary prune/clamp, distinct from account time.
    pub now: DateTime<Utc>,
    pub history: Vec<MovementInput>,
    pub global: Vec<IncidentInput>,
    pub session_count: usize,
    pub session_exists: bool,
    pub tripped_until: Option<DateTime<Utc>>,
    pub revision: u64,
    /// Ages use one capture anchor, so equal/before/after order is exact.
    pub source_observation_age_ns: Option<u64>,
    pub target_observation_age_ns: Option<u64>,
    pub sole_target: Option<SoleTargetInput>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedMove {
    pub revision: u64,
    pub shared_incident: Option<(String, u64)>,
    pub key: String,
    pub at: DateTime<Utc>,
    pub from: String,
    pub to: String,
    pub departure_evidence: u64,
    pub destination_evidence: u64,
    pub emergency_return: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermitDenied {
    pub reason: Refusal,
    pub until: DateTime<Utc>,
    pub newly_tripped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "facts", rename_all = "snake_case")]
pub enum PermitDecision {
    NotRequired,
    Allowed(PreparedMove),
    Denied(PermitDenied),
    NotComparable,
}
