//! Pure move-decision read facts shared with native selection.
//!
//! These helpers read only supplied account/quota and affinity observations.
//! `outside_failure` is retained as a raw atomic observation for selection;
//! these five helpers do not consume it.
//!
//! `Account` is deliberately retained here to execute native health semantics.
//! This in-memory input is NOT a privacy-approved archive DTO: the producer must
//! audit its wire projection before serializing account metadata.
use crate::accounts::{Account, Health, HealthSignal};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) mod permit;
pub use permit::{PermitDecision, PermitDenied, PermitInputs, PreparedMove, decide_move};
pub(crate) mod selection;
pub(super) mod selection_affinity;
pub(super) mod selection_metrics;
pub use selection::{SelectionInputs, SelectionReturnInput};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateInput {
    pub account: Account,
    pub quota: Option<Value>,
    pub inflight: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceGroup {
    pub account: String,
    pub keys: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBinding {
    pub accepted: Option<bool>,
}

/// Raw observations after shell mutations, before evaluating move permission.
/// Missing accounts mean genuinely missing supplied evidence, never permission
/// to fill the gap from a store or a different candidate collection.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionInputs {
    pub now: DateTime<Utc>,
    pub model: Option<String>,
    pub key: Option<String>,
    pub from: Option<String>,
    pub to: String,
    pub accounts: BTreeMap<String, CandidateInput>,
    pub traffic: BTreeMap<String, DateTime<Utc>>,
    pub pending: BTreeMap<String, usize>,
    pub groups: BTreeMap<String, AcceptanceGroup>,
    pub bindings: BTreeMap<String, AcceptanceBinding>,
    pub accepted_unbound: BTreeSet<String>,
    pub sticky: BTreeMap<String, String>,
    /// Raw external-capacity failure observation, not computed eligibility.
    #[serde(default)]
    pub outside_failure: bool,
    pub permit: Option<PermitInputs>,
    /// Selection has its own raw read set; permit-only archives omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Box<SelectionInputs>>,
}

fn helper_facts(
    inputs: &DecisionInputs,
    accounts: BTreeMap<String, CandidateInput>,
    model: Option<&str>,
) -> DecisionInputs {
    DecisionInputs {
        now: inputs.now,
        model: model.map(str::to_owned),
        key: inputs.key.clone(),
        from: inputs.from.clone(),
        to: inputs.to.clone(),
        accounts,
        traffic: inputs.traffic.clone(),
        pending: inputs.pending.clone(),
        groups: inputs.groups.clone(),
        bindings: inputs.bindings.clone(),
        accepted_unbound: inputs.accepted_unbound.clone(),
        sticky: inputs.sticky.clone(),
        outside_failure: inputs.outside_failure,
        permit: None,
        selection: None,
    }
}

/// Ordered native collections can contain the same ID with different supplied
/// rows. Interpret this exact row rather than substituting a same-ID peer.
pub(super) fn candidate_facts(
    inputs: &DecisionInputs,
    candidate: &CandidateInput,
    model: Option<&str>,
) -> DecisionInputs {
    helper_facts(
        inputs,
        BTreeMap::from([(candidate.account.id.clone(), candidate.clone())]),
        model,
    )
}

pub fn stale_window(inputs: &DecisionInputs, id: &str, window: &Value) -> Option<bool> {
    let c = inputs.accounts.get(id)?;
    Some(date(&window["observed_at"]).is_some_and(|observed| {
        (inputs.now - observed).num_seconds() > 900
            && (inputs
                .traffic
                .get(&c.account.id)
                .is_some_and(|t| *t > observed)
                || inputs
                    .pending
                    .get(&c.account.id)
                    .copied()
                    .unwrap_or(0)
                    .max(c.inflight)
                    > 0)
    }))
}

pub fn effective_account(inputs: &DecisionInputs, id: &str) -> Option<Account> {
    let c = inputs.accounts.get(id)?;
    let model = inputs.model.as_deref();
    let now = inputs.now;
    let mut account = c.account.clone();
    if account.eligible_at(model, now)
        && let Some(snapshot) = &c.quota
    {
        account.health = Health::Ready;
        account.quota_cooling = None;
        let mut snapshot = snapshot.clone();
        if let Some(windows) = snapshot["windows"].as_array_mut() {
            // The account was resolved above; this query cannot be absent.
            windows.retain(|w| stale_window(inputs, id, w) == Some(false));
        }
        snapshot["signals"] = Value::Null;
        account.apply_health_signal(
            HealthSignal::UsageSnapshot {
                snapshot,
                model: model.map(str::to_owned),
                projected_allowance: 0.0,
            },
            now,
        );
    }
    Some(account)
}

pub fn accepted_in(inputs: &DecisionInputs, id: &str) -> bool {
    let key = inputs.key.as_deref();
    if key.is_none() {
        return inputs.accepted_unbound.contains(id)
            && inputs.sticky.values().any(|sticky| sticky == id);
    }
    key.and_then(|key| inputs.groups.get(group(key)))
        .is_some_and(|g| {
            g.account == id
                && g.keys.iter().any(|k| {
                    inputs
                        .bindings
                        .get(k)
                        .is_some_and(|b| b.accepted != Some(false))
                })
        })
}

pub fn semantic_evidence(inputs: &DecisionInputs, id: Option<&str>) -> u64 {
    use sha2::{Digest, Sha256};
    let candidate = id.and_then(|id| effective_account(inputs, id));
    let Some(c) = candidate.as_ref() else {
        return 0;
    };
    // Keep the native semantic projection: poll times, token generations and
    // quota started_at are deliberately excluded.
    let reset = c.quota_cooling.as_ref().map(|q| (&q.window, q.reset_at));
    let value = serde_json::to_vec(&(
        &c.user_state,
        &c.health,
        c.payment_until,
        c.drain,
        c.allow_credits,
        reset,
    ))
    .expect("account health serializes");
    u64::from_be_bytes(Sha256::digest(value)[..8].try_into().unwrap())
}

pub fn move_source_usable(inputs: &DecisionInputs, id: &str) -> bool {
    let Some(candidate) = inputs.accounts.get(id) else {
        return false;
    };
    effective_account(inputs, id)
        .is_some_and(|account| account.eligible_at(inputs.model.as_deref(), inputs.now))
        && (!candidate.account.drain || accepted_in(inputs, &candidate.account.id))
}

fn group(key: &str) -> &str {
    if key.starts_with("claude:") {
        key.split_once(":agent:").map_or(key, |(root, _)| root)
    } else {
        key
    }
}

fn date(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
#[path = "kernel/raw_tests.rs"]
mod raw_tests;
