//! Raw selection read set. These values contain no mutation capabilities.
//! Archive persistence must use the audited projection, not these native types.
use super::selection_metrics::MetricInputs;
use super::{CandidateInput, permit::MovementInput, selection_affinity::AffinityInput};
use crate::accounts::Provider;
pub use crate::routing::selection_inputs::SelectionPolicyInput;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreferenceInput {
    pub account: Option<String>,
    pub coverage: Option<BTreeSet<String>>,
    pub model_move: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootHintInput {
    pub provider: Provider,
    pub key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchInput {
    pub policy_key: String,
    pub idle_placement: bool,
}

/// Selection's return check does not prune or advance the breaker clock.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionReturnInput {
    pub last_now: Option<DateTime<Utc>>,
    pub history: Vec<MovementInput>,
    pub target_observation_age_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionEntry {
    Ordered,
    ReturnGuard,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SelectionChoice {
    Selected { account: String },
    Hold { until: DateTime<Utc> },
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionLabel {
    New,
    Forced,
    Inherited,
    MarkWait,
    ColdRebalance,
    Overload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionEvidence {
    pub kind: SelectionLabel,
    pub key: String,
    pub from: String,
    pub to: String,
    pub model: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionDecision {
    pub choice: SelectionChoice,
    pub label: SelectionLabel,
    pub rebalance: Option<SelectionEvidence>,
    pub sole_target: Option<super::permit::SoleTargetInput>,
    /// Observed native source for a proposal; equal destination means retention.
    pub budget_source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldDiagnostic {
    pub account: String,
    pub reason: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionEvaluation {
    pub decision: SelectionDecision,
    /// The native shell emits diagnostics; evaluation itself performs no I/O.
    pub diagnostics: Vec<HoldDiagnostic>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionInputError {
    MissingSelection,
    AccountOrder,
    PolicyScope,
    MissingPool,
    MissingReturnHistory,
    MissingCapacityFacts,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionInputs {
    pub entry: SelectionEntry,
    pub provider: Provider,
    pub parent: Option<String>,
    pub pinned: Option<String>,
    pub hold_allowed: bool,
    pub preference: Option<PreferenceInput>,
    pub root_hint: Option<RootHintInput>,
    /// Actual supplied destination order, before any ranking or admission.
    pub candidates: Vec<CandidateInput>,
    /// Full health-owner evidence is distinct from filtered destinations.
    pub source_accounts: Vec<CandidateInput>,
    /// Native lexical order is an identity relation, not an eligibility score.
    /// Re-sorting opaque IDs on replay would change deterministic tie breaks.
    pub account_order: Vec<String>,
    pub affinity: AffinityInput,
    pub metrics: MetricInputs,
    pub policy: SelectionPolicyInput,
    pub launches: BTreeMap<String, LaunchInput>,
    pub launch_aliases: BTreeSet<String>,
    pub unknown_cost: BTreeSet<String>,
    pub last_request_bytes: BTreeMap<String, u64>,
    pub outside_unknown: bool,
    pub outside_used: BTreeSet<String>,
    pub entitled_targets: Option<BTreeSet<String>>,
    pub return_guard: Option<SelectionReturnInput>,
}

/// Entry identity is native caller metadata. All eligibility and ranking reads
/// below are from this supplied transaction, including the distinct source set.
pub fn select(inputs: &DecisionInputs) -> Result<SelectionEvaluation, SelectionInputError> {
    let s = inputs
        .selection
        .as_deref()
        .ok_or(SelectionInputError::MissingSelection)?;
    let unique: BTreeSet<_> = s.account_order.iter().collect();
    if unique.len() != s.account_order.len()
        || s.candidates
            .iter()
            .any(|row| !unique.contains(&row.account.id))
    {
        return Err(SelectionInputError::AccountOrder);
    }
    if s.policy.scope.key.as_deref() != policy_key(inputs) {
        return Err(SelectionInputError::PolicyScope);
    }
    if s.candidates
        .iter()
        .any(|row| !s.policy.target_pools.contains_key(&row.account.id))
    {
        return Err(SelectionInputError::MissingPool);
    }
    if s.entry == SelectionEntry::ReturnGuard && s.return_guard.is_none() {
        return Err(SelectionInputError::MissingReturnHistory);
    }
    let mut selected = select_ordered(inputs, &s.candidates);
    if s.entry == SelectionEntry::Ordered {
        return Ok(selected);
    }
    let SelectionChoice::Selected { account } = &selected.decision.choice else {
        return Ok(selected);
    };
    let Some(from) = rebind_budget_source(inputs, account) else {
        return Ok(selected);
    };
    let Some(source) = s.source_accounts.iter().find(|row| row.account.id == from) else {
        return Ok(selected);
    };
    let source = effective_for(inputs, source, inputs.model.as_deref());
    if crate::accounts::health::UnusableEvidence::observe(
        &source,
        inputs.model.as_deref(),
        super::accepted_in(inputs, &from),
        inputs.now,
    )
    .is_none()
    {
        return Ok(selected);
    }
    let outside_unknown = s.outside_unknown || inputs.outside_failure;
    let viable: BTreeSet<_> = s
        .source_accounts
        .iter()
        .filter(|row| {
            row.account.id != from
                && row.account.provider == s.provider
                && !row.account.drain
                && effective_for(inputs, row, inputs.model.as_deref())
                    .eligible_at(inputs.model.as_deref(), inputs.now)
                && !outside_unknown
                && !s.outside_used.contains(&row.account.id)
                && s.entitled_targets
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&row.account.id))
                && s.policy.permits_dispatch(&row.account.id)
        })
        .map(|row| row.account.id.as_str())
        .collect();
    if viable.len() == 1 && viable.contains(account.as_str()) {
        selected.decision.sole_target = Some(super::permit::SoleTargetInput {
            from,
            to: account.clone(),
            model: inputs.model.clone(),
            at: inputs.now,
        });
        return Ok(selected);
    }
    let alternatives: Vec<_> = s
        .candidates
        .iter()
        .filter(|row| viable.contains(row.account.id.as_str()))
        .filter(|row| !return_blocked(inputs, row))
        .cloned()
        .collect();
    let selected_blocked = s
        .candidates
        .iter()
        .find(|row| row.account.id == *account)
        .is_some_and(|row| return_blocked(inputs, row));
    if selected_blocked && !alternatives.is_empty() {
        let mut alternative = select_ordered(inputs, &alternatives);
        selected.diagnostics.append(&mut alternative.diagnostics);
        alternative.diagnostics = selected.diagnostics;
        return Ok(alternative);
    }
    Ok(selected)
}

fn return_blocked(inputs: &DecisionInputs, row: &CandidateInput) -> bool {
    let s = inputs.selection.as_deref().expect("selection context");
    let guard = s.return_guard.as_ref().expect("validated return history");
    let now = guard
        .last_now
        .map_or(inputs.now, |last| inputs.now.max(last));
    let facts = super::candidate_facts(inputs, row, inputs.model.as_deref());
    // semantic_evidence intentionally projects the same effective account. Its
    // helper has no clock or metadata owner access.
    let evidence = super::semantic_evidence(&facts, Some(&row.account.id));
    guard
        .history
        .iter()
        .rev()
        .find(|movement| {
            movement.from == row.account.id && movement.at + chrono::Duration::minutes(30) > now
        })
        .is_some_and(|left| {
            left.departure_evidence == 0
                || evidence == 0
                || left.departure_evidence == evidence
                || guard
                    .target_observation_age_ns
                    .is_none_or(|age| age >= left.committed_age_ns)
        })
}

pub(crate) fn rebind_budget_source(inputs: &DecisionInputs, to: &str) -> Option<String> {
    let s = inputs.selection.as_deref().expect("selection context");
    let key = inputs.key.as_deref();
    let physical = key
        .and_then(|key| s.affinity.groups.get(group(key)))
        .map(|entry| entry.account.clone())
        .or_else(|| key.and_then(|_| s.policy.plan().anchor))
        .or_else(|| {
            key.is_none()
                .then(|| inputs.sticky.get(s.provider.as_str()).cloned())
                .flatten()
        });
    if physical.as_deref() != Some(to) {
        return physical;
    }
    s.policy.plan().anchor.or(physical)
}

fn select_ordered(inputs: &DecisionInputs, candidates: &[CandidateInput]) -> SelectionEvaluation {
    let s = inputs.selection.as_deref().expect("selection context");
    let key = inputs.key.as_deref();
    let (plan, policy_exists) = if s.root_hint.is_some() {
        s.policy.placement_plan()
    } else {
        (s.policy.plan(), true)
    };
    let hinted_account = s.root_hint.as_ref().and_then(|hint| {
        let key = key?;
        let root = group(key);
        if s.parent.is_some()
            || s.pinned.is_some()
            || s.preference.as_ref().is_some_and(|p| p.model_move)
            || policy_exists
            || plan.anchor.is_some()
            || plan.project.is_some()
            || s.affinity.groups.contains_key(root)
            || s.affinity.bindings.contains_key(key)
            || s.launches.contains_key(root)
            || s.affinity.launch_anchors.contains_key(root)
            || s.affinity.launch_anchors.contains_key(key)
            || s.launch_aliases.contains(root)
            || s.launch_aliases.contains(key)
            || s.affinity.launch_retention.contains_key(root)
            || s.affinity.active.contains(root)
            || s.affinity.pins.contains(root)
            || hint.provider != s.provider
        {
            return None;
        }
        affinity::retained_binding(inputs, &hint.key).map(|binding| binding.account.as_str())
    });
    let mut diagnostics = Vec::new();
    let (choice, label) =
        select_ranked(inputs, candidates, &plan, hinted_account, &mut diagnostics);
    let rebalance = if matches!(
        label,
        SelectionLabel::ColdRebalance | SelectionLabel::Overload
    ) {
        match (&choice, key) {
            (SelectionChoice::Selected { account }, Some(key)) => plan
                .anchor
                .clone()
                .or_else(|| {
                    affinity::retained_binding(inputs, key).map(|binding| binding.account.clone())
                })
                .filter(|from| from != account)
                .map(|from| SelectionEvidence {
                    kind: label,
                    key: group(key).to_owned(),
                    from,
                    to: account.clone(),
                    model: inputs.model.clone(),
                    at: inputs.now,
                }),
            _ => None,
        }
    } else {
        None
    };
    let budget_source = match &choice {
        SelectionChoice::Selected { account } => rebind_budget_source(inputs, account),
        _ => None,
    };
    SelectionEvaluation {
        decision: SelectionDecision {
            choice,
            label,
            rebalance,
            sole_target: None,
            budget_source,
        },
        diagnostics,
    }
}

fn prefer<T>(pool: &mut Vec<(&CandidateInput, T)>, preference: Option<&PreferenceInput>) {
    let Some(preference) = preference else { return };
    if let Some(coverage) = &preference.coverage
        && pool
            .iter()
            .any(|(row, _)| coverage.contains(&row.account.id))
    {
        pool.retain(|(row, _)| coverage.contains(&row.account.id));
    }
    if let Some(account) = preference.account.as_deref()
        && pool.iter().any(|(row, _)| row.account.id == account)
    {
        pool.retain(|(row, _)| row.account.id == account);
    }
}

fn prefer_root_hint<T>(pool: &mut Vec<(&CandidateInput, T)>, account: Option<&str>) {
    if let Some(account) = account
        && pool.iter().any(|(row, _)| row.account.id == account)
    {
        pool.retain(|(row, _)| row.account.id == account);
    }
}

fn prefer_pacing_and_evidence(
    pool: &mut Vec<(&CandidateInput, &m::Metrics)>,
    inputs: &DecisionInputs,
) {
    let s = inputs.selection.as_deref().expect("selection context");
    if pool.iter().any(|(row, _)| {
        !s.metrics
            .pacing
            .get(&row.account.id)
            .copied()
            .unwrap_or(false)
    }) {
        pool.retain(|(row, _)| {
            !s.metrics
                .pacing
                .get(&row.account.id)
                .copied()
                .unwrap_or(false)
        });
    }
    if pool
        .iter()
        .any(|(row, _)| !m::placement_capped_for(inputs, row).expect("selection context"))
    {
        pool.retain(|(row, _)| !m::placement_capped_for(inputs, row).expect("selection context"));
    } else if let Some(minimum) = pool
        .iter()
        .map(|(row, _)| {
            m::placement_evidence_for(inputs, row)
                .expect("selection context")
                .1
        })
        .min()
    {
        pool.retain(|(row, _)| {
            m::placement_evidence_for(inputs, row)
                .expect("selection context")
                .1
                == minimum
        });
    }
}

use super::{DecisionInputs, group, selection_affinity as affinity, selection_metrics as m};
use crate::accounts::{Account, Health, HealthReason, UserState};
use std::time::Duration;

fn effective_for(inputs: &DecisionInputs, row: &CandidateInput, model: Option<&str>) -> Account {
    let facts = super::candidate_facts(inputs, row, model);
    super::effective_account(&facts, &row.account.id).expect("provided raw candidate")
}
fn policy_key(inputs: &DecisionInputs) -> Option<&str> {
    let s = inputs.selection.as_deref()?;
    inputs.key.as_deref().map(|key| {
        s.launches
            .get(group(key))
            .map_or(key, |launch| launch.policy_key.as_str())
    })
}
fn account_cmp(inputs: &DecisionInputs, a: &str, b: &str) -> std::cmp::Ordering {
    let order = &inputs
        .selection
        .as_deref()
        .expect("selection context")
        .account_order;
    let at = |id: &str| {
        order
            .iter()
            .position(|entry| entry == id)
            .expect("validated native account order")
    };
    at(a).cmp(&at(b))
}
fn idle(now: DateTime<Utc>, last: DateTime<Utc>) -> u64 {
    (now - last).num_seconds().max(0) as u64
}
fn protected(inputs: &DecisionInputs, key: &str) -> bool {
    let affinity = &inputs
        .selection
        .as_deref()
        .expect("selection context")
        .affinity;
    affinity.active.contains(key) || affinity.pins.contains(key)
}
fn select_ranked(
    inputs: &DecisionInputs,
    candidates: &[CandidateInput],
    plan: &crate::routing::Plan,
    hinted_account: Option<&str>,
    diagnostics: &mut Vec<HoldDiagnostic>,
) -> (SelectionChoice, SelectionLabel) {
    let s = inputs.selection.as_deref().expect("selection context");
    let provider = s.provider;
    let key = inputs.key.as_deref();
    let parent = s.parent.as_deref();
    let model = inputs.model.as_deref();
    let pinned = s.pinned.as_deref();
    let now = inputs.now;
    let hold_allowed = s.hold_allowed;
    let preference = s.preference.as_ref();

    let model_move = preference.is_some_and(|p| p.model_move);
    let strict_anchor = plan.anchor.clone().or_else(|| {
        key.and_then(|k| affinity::retained_binding(inputs, k))
            .map(|b| b.account.clone())
    });
    let source = affinity::protected_source(inputs, key, plan.anchor.as_deref());
    let outside_unknown = s.outside_unknown || inputs.outside_failure;
    let mut entries: Vec<_> = candidates
        .iter()
        .filter(|c| c.account.provider == provider)
        .filter(|c| {
            model_move
                || plan.settings.mode != crate::routing::Mode::Strict
                || strict_anchor
                    .as_ref()
                    .is_none_or(|anchor| anchor == &c.account.id)
        })
        .map(|c| {
            let eligible = effective_for(inputs, c, model).eligible_at(model, now)
                && (!c.account.drain || super::accepted_in(inputs, &c.account.id));
            (c, eligible)
        })
        .collect();
    if let Some(source) = source.as_deref() {
        // Outside use never displaces an existing source. It only vetoes
        // destinations, including forced moves and explicit preferences.
        for (candidate, eligible) in &mut entries {
            *eligible &= candidate.account.id == source
                || (!outside_unknown && !s.outside_used.contains(&candidate.account.id));
        }
    } else if entries.iter().any(|(candidate, eligible)| {
        *eligible && !outside_unknown && !s.outside_used.contains(&candidate.account.id)
    }) {
        // Last resort is before C7 preference/gate partitions: a healthy
        // unmarked account cannot be bypassed by a reset or plan ranking.
        entries.retain(|(candidate, _)| !s.outside_used.contains(&candidate.account.id));
    }
    if let Some(id) = pinned
        && entries
            .iter()
            .any(|(c, eligible)| c.account.id == id && *eligible)
    {
        return (
            SelectionChoice::Selected { account: id.into() },
            SelectionLabel::Forced,
        );
    }
    struct SelectionBinding {
        key: String,
        account: String,
        last_used: DateTime<Utc>,
    }
    let mut bound = key
        .filter(|_| !model_move)
        .and_then(|k| affinity::retained_binding(inputs, k))
        .map(|b| SelectionBinding {
            key: b.key.clone(),
            account: b.account.clone(),
            last_used: b.last_used,
        });
    if let (Some(anchor), Some(key)) = (&plan.anchor, key.filter(|_| !model_move)) {
        let b = bound.get_or_insert_with(|| SelectionBinding {
            key: key.to_owned(),
            account: anchor.clone(),
            last_used: now,
        });
        b.account = anchor.clone();
    }
    let bound_account = bound.as_ref().map(|b| b.account.clone());
    let mut reason = SelectionLabel::New;
    if let Some(b) = bound {
        reason = SelectionLabel::Forced;
        if let Some((c, eligible)) = entries.iter().find(|(c, _)| c.account.id == b.account) {
            if hold_allowed
                && c.account.user_state == UserState::Active
                && let Health::Cooling {
                    reason: HealthReason::Quota,
                    until,
                    scope,
                } = &c.account.health
                && let Some(quota) = &c.account.quota_cooling
                && *until > now
                && (scope == "account"
                    || scope
                        .strip_prefix("model:")
                        .is_some_and(|m| model == Some(m)))
            {
                let reset = quota.reset_at;
                if reset > now
                    && (reset - now).to_std().is_ok_and(|wait| {
                        wait <= Duration::from_secs(plan.settings.reset_wait_secs)
                    })
                {
                    return (SelectionChoice::Hold { until: reset }, reason);
                }
                let skip_reason = if reset <= now {
                    "reset_elapsed"
                } else if reset.signed_duration_since(now).to_std().is_err() {
                    "reset_before_now"
                } else {
                    "reset_beyond_hold_bound"
                };
                diagnostics.push(HoldDiagnostic {
                    account: b.account.clone(),
                    reason: skip_reason,
                });
            }
            if *eligible && plan.settings.mode != crate::routing::Mode::Rebalance {
                return (
                    SelectionChoice::Selected {
                        account: b.account.clone(),
                    },
                    if m::has_overload_mark(inputs, &b.key).expect("selection context") {
                        SelectionLabel::MarkWait
                    } else {
                        SelectionLabel::Forced
                    },
                );
            }
            if *eligible && s.affinity.active.contains(group(&b.key)) {
                return (
                    SelectionChoice::Selected {
                        account: b.account.clone(),
                    },
                    if m::has_overload_mark(inputs, &b.key).expect("selection context") {
                        SelectionLabel::MarkWait
                    } else {
                        SelectionLabel::Forced
                    },
                );
            }
            let marked = m::has_overload_mark(inputs, &b.key).expect("selection context");
            if marked && *eligible {
                let known_cost = !s.unknown_cost.contains(group(&b.key))
                    && s.last_request_bytes
                        .get(group(&b.key))
                        .is_some_and(|n| *n > 0 && *n <= s.metrics.config.churn.max_move_bytes);
                let room_elsewhere = known_cost
                    && entries.iter().any(|(candidate, eligible)| {
                        candidate.account.id != b.account
                            && *eligible
                            && s.metrics
                                .overload_marks
                                .get(group(&b.key))
                                .is_some_and(|mark| mark.from == b.account)
                            && m::rebalance_target_for(
                                inputs,
                                &b.account,
                                candidate,
                                Some(group(&b.key)),
                            )
                            .expect("selection context")
                            && s.policy
                                .discretionary_allowed(Some(&candidate.account.id), now)
                                .expect("validated target pool")
                    });
                // Any discretionary veto waits on the existing mark. In
                // particular, fresh/uncapped targets must not erase a mark
                // while group, revisit, pool or cost guards are active.
                let guard_waiting = !known_cost
                    || !s
                        .policy
                        .discretionary_allowed(None, now)
                        .expect("validated target pool")
                    || entries.iter().any(|(candidate, eligible)| {
                        *eligible
                            && candidate.account.id != b.account
                            && m::rebalance_target_waiting_for(
                                inputs,
                                &b.account,
                                candidate,
                                Some(group(&b.key)),
                            )
                            .expect("selection context")
                            && (m::placement_capped_for(inputs, candidate)
                                .expect("selection context")
                                || !s
                                    .policy
                                    .discretionary_allowed(Some(&candidate.account.id), now)
                                    .expect("validated target pool"))
                    });
                if !room_elsewhere
                    && guard_waiting
                    && m::active_sessions(inputs, &b.account).expect("selection context")
                        > m::spread_threshold(inputs, &b.account).expect("selection context")
                {
                    return (
                        SelectionChoice::Selected {
                            account: b.account.clone(),
                        },
                        SelectionLabel::MarkWait,
                    );
                }
                if !room_elsewhere
                    || m::active_sessions(inputs, &b.account).expect("selection context")
                        <= m::spread_threshold(inputs, &b.account).expect("selection context")
                {
                    // choose_with_hold clears this mark when it reserves
                    // the unchanged binding. Preview remains read-only.
                    return (
                        SelectionChoice::Selected {
                            account: b.account.clone(),
                        },
                        SelectionLabel::Forced,
                    );
                }
                // Use the ordinary C7 ordering below, restricted to real
                // eligible targets with room rather than candidate id order.
                reason = SelectionLabel::Overload;
            }
            if *eligible && !marked {
                let g = group(&b.key);
                let last = s
                    .affinity
                    .groups
                    .get(g)
                    .map_or(b.last_used, |entry| entry.last_used);
                let at_capacity = m::active_sessions(inputs, &b.account)
                    .expect("selection context")
                    >= m::spread_threshold(inputs, &b.account).expect("selection context");
                let cold = !protected(inputs, g)
                    && idle(now, last) >= s.metrics.config.rebalance_idle_secs;
                if !cold
                    || !s
                        .policy
                        .discretionary_allowed(None, now)
                        .expect("validated target pool")
                    || s.unknown_cost.contains(group(&b.key))
                    || s.last_request_bytes.get(group(&b.key)).is_none_or(|bytes| {
                        *bytes == 0 || *bytes > s.metrics.config.churn.max_move_bytes
                    })
                    || (!at_capacity
                        && !c.account.drain
                        && m::metrics_for(inputs, c, true)
                            .expect("selection context")
                            .gate)
                {
                    return (
                        SelectionChoice::Selected {
                            account: b.account.clone(),
                        },
                        reason,
                    );
                }
                reason = if marked {
                    SelectionLabel::Overload
                } else {
                    SelectionLabel::ColdRebalance
                };
            }
        }
    } else if let Some(p) = parent.and_then(|p| affinity::retained_binding(inputs, p)) {
        if entries
            .iter()
            .any(|(c, eligible)| c.account.id == p.account && *eligible)
        {
            return (
                SelectionChoice::Selected {
                    account: p.account.clone(),
                },
                SelectionLabel::Inherited,
            );
        }
    } else if key.is_none()
        && let Some(id) = inputs.sticky.get(provider.as_str())
        && entries
            .iter()
            .any(|(c, eligible)| &c.account.id == id && *eligible)
    {
        return (
            SelectionChoice::Selected {
                account: id.clone(),
            },
            reason,
        );
    }
    let eligible: Vec<_> = entries
        .into_iter()
        .filter(|(c, eligible)| !c.account.drain && *eligible)
        .map(|(c, _)| {
            (
                c,
                m::metrics_for(inputs, c, false).expect("selection context"),
            )
        })
        .collect();
    let eligible: Vec<_> = eligible
        .into_iter()
        .filter(|(c, _)| {
            if !matches!(
                reason,
                SelectionLabel::Overload | SelectionLabel::ColdRebalance
            ) {
                return true;
            }
            bound_account.as_ref().is_some_and(|source| {
                source != &c.account.id
                    && m::rebalance_target_base_for(inputs, source, c, key.map(group))
                        .expect("selection context")
            }) && key.is_some_and(|_| {
                s.policy
                    .discretionary_allowed(Some(&c.account.id), now)
                    .expect("validated target pool")
            })
        })
        .collect();
    let has_gate = eligible.iter().any(|(_, m)| m.gate);
    let pool: Vec<_> = eligible
        .iter()
        .filter(|(_, m)| !has_gate || m.gate)
        .collect();
    let has_room = pool.iter().any(|(c, _)| {
        m::active_sessions(inputs, &c.account.id).expect("selection context")
            + m::reserved(inputs, &c.account.id, key.map(group)).expect("selection context")
            < m::spread_threshold(inputs, &c.account.id).expect("selection context")
    });
    let mut partition: Vec<_> = pool
        .into_iter()
        .filter(|(c, _)| {
            !has_room
                || m::active_sessions(inputs, &c.account.id).expect("selection context")
                    + m::reserved(inputs, &c.account.id, key.map(group)).expect("selection context")
                    < m::spread_threshold(inputs, &c.account.id).expect("selection context")
        })
        .collect();
    if reason == SelectionLabel::Overload {
        partition.retain(|(c, m)| {
            m.gate
                && key
                    .and_then(|key| s.metrics.overload_marks.get(group(key)))
                    .is_some_and(|mark| {
                        mark.from != c.account.id
                            && m::rebalance_target_base_for(inputs, &mark.from, c, key.map(group))
                                .expect("selection context")
                            && key.is_some_and(|_| {
                                s.policy
                                    .discretionary_allowed(Some(&c.account.id), now)
                                    .expect("validated target pool")
                            })
                    })
        });
    }
    if reason == SelectionLabel::ColdRebalance {
        partition.retain(|(c, _)| {
            bound_account.as_ref().is_some_and(|source| {
                source != &c.account.id
                    && m::rebalance_target_base_for(inputs, source, c, key.map(group))
                        .expect("selection context")
            }) && key.is_some_and(|_| {
                s.policy
                    .discretionary_allowed(Some(&c.account.id), now)
                    .expect("validated target pool")
            })
        });
    }
    if matches!(
        reason,
        SelectionLabel::Overload | SelectionLabel::ColdRebalance
    ) {
        // A discretionary move cannot bypass a capped, non-pacing target
        // by reviving a pacing alternative. Resolve room, pacing, then cap.
        if partition.iter().any(|(c, _)| {
            !s.metrics
                .pacing
                .get(&c.account.id)
                .copied()
                .unwrap_or(false)
        }) {
            partition.retain(|(c, _)| {
                !s.metrics
                    .pacing
                    .get(&c.account.id)
                    .copied()
                    .unwrap_or(false)
            });
        }
        partition.retain(|(c, _)| !m::placement_capped_for(inputs, c).expect("selection context"));
    }
    if partition.is_empty()
        && matches!(
            reason,
            SelectionLabel::ColdRebalance | SelectionLabel::Overload
        )
        && let Some(account) = bound_account
    {
        return (
            SelectionChoice::Selected { account },
            if reason == SelectionLabel::Overload {
                SelectionLabel::MarkWait
            } else {
                SelectionLabel::Forced
            },
        );
    }
    let ratio = |c: &CandidateInput| {
        (m::active_sessions(inputs, &c.account.id).expect("selection context")
            + m::reserved(inputs, &c.account.id, key.map(group)).expect("selection context"))
            as f64
            / m::spread_threshold(inputs, &c.account.id).expect("selection context") as f64
    };
    let min_ratio = partition
        .iter()
        .map(|(c, _)| ratio(c))
        .min_by(f64::total_cmp);
    if !has_room && let Some(minimum) = min_ratio {
        partition.retain(|(c, _)| ratio(c) == minimum);
    }
    // Step 4: form the gate-passing set before any pacing/cap ordering.
    // A lower-count or uncapped failing account must never displace it.
    let mut gated: Vec<_> = partition
        .iter()
        .filter(|entry| entry.1.gate)
        .map(|entry| (entry.0, &entry.1))
        .collect();
    let chosen = if !gated.is_empty() {
        prefer_pacing_and_evidence(&mut gated, inputs);
        prefer(&mut gated, preference);
        prefer_root_hint(&mut gated, hinted_account);
        // Approximate reset cohorts require one fixed anchor.
        let anchor = gated
            .iter()
            .filter(|(_, m)| !m.unknown)
            .filter_map(|(_, m)| m.weekly)
            .min();
        gated.sort_by(|(a, x), (b, y)| {
            m::compare_gated(inputs, &a.account.id, x, &b.account.id, y, anchor)
                .expect("validated native account order")
        });
        gated.first().map(|(candidate, _)| *candidate)
    } else {
        // Step 5: only when this set has no passing gate, apply its own
        // pacing/cap ordering and then choose the most headroom.
        let mut fallback: Vec<_> = partition.iter().map(|entry| (entry.0, &entry.1)).collect();
        prefer_pacing_and_evidence(&mut fallback, inputs);
        prefer(&mut fallback, preference);
        prefer_root_hint(&mut fallback, hinted_account);
        fallback
            .iter()
            .min_by(|(a, x), (b, y)| {
                x.usage
                    .total_cmp(&y.usage)
                    .then_with(|| account_cmp(inputs, &a.account.id, &b.account.id))
            })
            .map(|(candidate, _)| *candidate)
    };
    (
        chosen.map_or(SelectionChoice::None, |c| SelectionChoice::Selected {
            account: c.account.id.clone(),
        }),
        reason,
    )
}

#[cfg(test)]
#[path = "selection/tests.rs"]
mod tests;
