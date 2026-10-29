//! Proof-free move proposal inputs and decisions. Native permits remain private
//! to the breaker owner, which rechecks its revision before committing a move.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::super::move_guard::{GLOBAL_RECORDS, Limits, RECORDS, Refusal, SESSIONS};

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// Evaluate a move proposal from captured owner facts. This value carries no
/// native mutation authority; the breaker must still apply it under its gate.
pub fn decide_move(inputs: &super::DecisionInputs) -> PermitDecision {
    use chrono::Duration;
    let Some(from) = inputs.from.as_deref().filter(|from| *from != inputs.to) else {
        return PermitDecision::NotRequired;
    };
    let (Some(key), Some(permit)) = (inputs.key.as_deref(), inputs.permit.as_ref()) else {
        return PermitDecision::NotComparable;
    };
    let to = &inputs.to;
    let now = permit.now;
    let limits = permit.limits.clone().bounded();
    let denied = |reason, until| {
        PermitDecision::Denied(PermitDenied {
            reason,
            until,
            newly_tripped: false,
        })
    };
    if key.is_empty()
        || key.len() > 256
        || from.is_empty()
        || from.len() > 128
        || to.is_empty()
        || to.len() > 128
        || from == to
    {
        return denied(Refusal::Capacity, now + Duration::minutes(10));
    }
    if let Some(until) = permit.tripped_until {
        return denied(Refusal::GlobalTrip, until);
    }
    let source_account = super::effective_account(inputs, from);
    let source_unusable = source_account.as_ref().is_some_and(|account| {
        crate::accounts::health::UnusableEvidence::observe(
            account,
            inputs.model.as_deref(),
            super::accepted_in(inputs, &account.id),
            inputs.now,
        )
        .is_some_and(|proof| proof.applies_to(from, inputs.now))
    });
    let departure_evidence = super::semantic_evidence(inputs, Some(from));
    let destination_evidence = super::semantic_evidence(inputs, Some(to));
    let shared_incident =
        (source_unusable && departure_evidence != 0 && permit.source_observation_age_ns.is_some())
            .then(|| (from.to_owned(), departure_evidence));
    let shares_incident = shared_incident.as_ref().is_some_and(|shared| {
        permit
            .global
            .iter()
            .any(|incident| incident.shared.as_ref() == Some(shared))
    });
    if !shares_incident && permit.global.len() >= limits.global_ten_minutes {
        return PermitDecision::Denied(PermitDenied {
            reason: Refusal::GlobalTrip,
            until: now + Duration::seconds(limits.trip_seconds as i64),
            newly_tripped: true,
        });
    }
    if (!shares_incident && permit.global.len() >= GLOBAL_RECORDS)
        || permit.revision == u64::MAX
        || (!permit.session_exists && permit.session_count >= SESSIONS)
    {
        return denied(Refusal::Capacity, now + Duration::minutes(10));
    }
    let mut emergency_return = false;
    let history = &permit.history;
    if permit.session_exists {
        if history.len() >= RECORDS {
            return denied(Refusal::Capacity, history[0].at + Duration::hours(1));
        }
        let after = |age: Option<u64>, committed: u64| age.is_some_and(|age| age < committed);
        let fresh_unusable = source_unusable
            && history.last().is_some_and(|last| {
                last.to == from
                    && after(permit.source_observation_age_ns, last.committed_age_ns)
                    && departure_evidence != 0
                    && last.destination_evidence != 0
                    && last.destination_evidence != departure_evidence
            });
        if let Some(left) = history
            .iter()
            .rev()
            .find(|m| m.from == *to && m.at + Duration::minutes(30) > now)
            && (left.departure_evidence == 0
                || destination_evidence == 0
                || left.departure_evidence == destination_evidence
                || !after(permit.target_observation_age_ns, left.committed_age_ns))
        {
            let sole_target = permit.sole_target.as_ref().is_some_and(|proof| {
                proof.from == from
                    && proof.to == *to
                    && proof.model == inputs.model
                    && proof.at == inputs.now
            });
            if !sole_target || !fresh_unusable {
                return denied(Refusal::PingPong, left.at + Duration::minutes(30));
            }
            emergency_return = true;
        }
        let rescued = history.iter().any(|m| m.emergency_return);
        let rescue_allowance = emergency_return && !rescued;
        if (!fresh_unusable || emergency_return || rescued) && !rescue_allowance {
            let recent: Vec<_> = history
                .iter()
                .filter(|m| m.at + Duration::minutes(10) > now)
                .collect();
            if recent.len() >= limits.session_ten_minutes {
                return denied(
                    Refusal::SessionTenMinutes,
                    recent[recent.len() - limits.session_ten_minutes].at + Duration::minutes(10),
                );
            }
            if history.len() >= limits.session_hour {
                return denied(
                    Refusal::SessionHour,
                    history[history.len() - limits.session_hour].at + Duration::hours(1),
                );
            }
        }
    }
    PermitDecision::Allowed(PreparedMove {
        revision: permit.revision,
        shared_incident,
        key: key.into(),
        at: now,
        from: from.into(),
        to: to.clone(),
        departure_evidence,
        destination_evidence,
        emergency_return,
    })
}
