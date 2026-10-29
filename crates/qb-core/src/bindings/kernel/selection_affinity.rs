//! Pure affinity reads over owner-supplied selection observations.
//!
//! Group keys retain their original native BTree order, including after wire
//! projection. Missing facts are never reconstructed from another source. The
//! wire validator owns completeness and consistency; malformed LRU rows fail
//! closed here rather than indexing a missing group or underflowing a count.
use crate::bindings::{CAPACITY, launch::IDLE_SECS};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AffinityInput {
    pub bindings: BTreeMap<String, BindingInput>,
    pub groups: BTreeMap<String, GroupInput>,
    pub binding_count: usize,
    pub lru: Vec<(DateTime<Utc>, String)>,
    pub active: BTreeSet<String>,
    pub pins: BTreeSet<String>,
    pub launch_retention: BTreeMap<String, DateTime<Utc>>,
    pub launch_anchors: BTreeMap<String, String>,
    pub session_binding_max_secs: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingInput {
    pub key: String,
    pub account: String,
    pub last_used: DateTime<Utc>,
    pub accepted: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupInput {
    /// Original native BTree order, never the order of projected/hashed keys.
    pub keys: Vec<String>,
    pub account: String,
    pub last_used: DateTime<Utc>,
}

pub fn retained_binding<'a>(
    inputs: &'a super::DecisionInputs,
    key: &str,
) -> Option<&'a BindingInput> {
    retained(&inputs.selection.as_deref()?.affinity, key, inputs.now)
}

pub fn protected_source(
    inputs: &super::DecisionInputs,
    key: Option<&str>,
    anchor: Option<&str>,
) -> Option<String> {
    source(
        &inputs.selection.as_deref()?.affinity,
        key,
        anchor,
        inputs.now,
    )
}

fn idle(now: DateTime<Utc>, last: DateTime<Utc>) -> u64 {
    (now - last).num_seconds().max(0) as u64
}

fn launch_retained(inputs: &AffinityInput, key: &str, now: DateTime<Utc>) -> bool {
    inputs
        .launch_retention
        .get(key)
        .is_some_and(|last| idle(now, *last) < IDLE_SECS)
}

fn retained<'a>(
    inputs: &'a AffinityInput,
    key: &str,
    now: DateTime<Utc>,
) -> Option<&'a BindingInput> {
    let g = super::group(key);
    let entry = inputs.groups.get(g)?;
    if !inputs.active.contains(g) && !inputs.pins.contains(g) && !launch_retained(inputs, g, now) {
        if idle(now, entry.last_used) >= inputs.session_binding_max_secs {
            return None;
        }
        if inputs.binding_count > CAPACITY {
            let mut size = inputs.binding_count;
            for (_, oldest) in &inputs.lru {
                if launch_retained(inputs, oldest, now) {
                    continue;
                }
                if size <= CAPACITY {
                    break;
                }
                if oldest == g {
                    return None;
                }
                size = size.checked_sub(inputs.groups.get(oldest)?.keys.len())?;
            }
        }
    }
    inputs
        .bindings
        .get(key)
        .or_else(|| entry.keys.first().and_then(|key| inputs.bindings.get(key)))
}

fn source(
    inputs: &AffinityInput,
    key: Option<&str>,
    anchor: Option<&str>,
    now: DateTime<Utc>,
) -> Option<String> {
    anchor.map(str::to_owned).or_else(|| {
        key.and_then(|key| {
            let g = super::group(key);
            let owner = inputs.launch_anchors.get(g).map_or(g, String::as_str);
            retained(inputs, owner, now)
        })
        .filter(|binding| binding.accepted != Some(false))
        .map(|binding| binding.account.clone())
    })
}

#[cfg(test)]
#[path = "selection_affinity/tests.rs"]
mod tests;
