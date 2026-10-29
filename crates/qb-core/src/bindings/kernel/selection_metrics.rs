//! Pure metric, placement and activity interpretation of raw selection facts.
//! No observation is acknowledged or written back. Native placement counters
//! are reconstructed locally; timer-only metadata is deliberately absent.
use super::{CandidateInput, DecisionInputs, date};
use crate::bindings::{
    PlacementEvidence, PlacementState, PlacementWindow, UsageObservationAsOf, account_window,
};
use crate::config::RoutingConfig;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricInputs {
    pub config: RoutingConfig,
    pub recent: BTreeMap<String, Vec<(DateTime<Utc>, String)>>,
    pub idle_placements: BTreeSet<String>,
    pub thresholds: BTreeMap<String, usize>,
    pub overload_marks: BTreeMap<String, OverloadInput>,
    pub pacing: BTreeMap<String, bool>,
    pub placements: BTreeMap<String, PlacementInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverloadInput {
    pub expires: DateTime<Utc>,
    pub from: String,
    pub target: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementInput {
    pub latest: Option<u64>,
    pub total: u64,
    pub windows: BTreeMap<String, PlacementWindowInput>,
    pub window_set_sequence: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementWindowInput {
    pub value: Value,
    pub asof: Option<PlacementAsOfInput>,
    pub reported_sequence: u64,
    pub unresolved_equal_charge: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementAsOfInput {
    pub sequence: u64,
    pub placed_count: u64,
}

/// Ephemeral results: never persisted as part of the raw input protocol.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub gate: bool,
    pub unknown: bool,
    pub weekly: Option<DateTime<Utc>>,
    pub five: Option<DateTime<Utc>>,
    pub usage: f64,
}

#[cfg(test)]
pub fn metrics(inputs: &DecisionInputs, id: &str) -> Option<Metrics> {
    metrics_for(inputs, inputs.accounts.get(id)?, false)
}

/// Interpret this exact supplied row, including same-ID quota/inflight peers.
pub(super) fn metrics_for(
    inputs: &DecisionInputs,
    c: &CandidateInput,
    without_charge: bool,
) -> Option<Metrics> {
    let raw = &inputs.selection.as_deref()?.metrics;
    let facts = super::candidate_facts(inputs, c, inputs.model.as_deref());
    let mut config = raw.config.clone();
    if without_charge {
        config.placement_allowance_pct = 0.0;
    }
    Some(calculate(
        c,
        raw,
        &config,
        inputs.pending.get(&c.account.id).copied().unwrap_or(0),
        inputs.now,
        |w| super::stale_window(&facts, &c.account.id, w).expect("provided candidate row"),
    ))
}

fn placement_state(c: &CandidateInput, raw: &MetricInputs) -> PlacementState {
    let mut state = raw
        .placements
        .get(&c.account.id)
        .map_or_else(PlacementState::default, |p| PlacementState {
            latest: p.latest,
            total: p.total,
            windows: p
                .windows
                .iter()
                .map(|(name, w)| {
                    (
                        name.clone(),
                        PlacementWindow {
                            value: w.value.clone(),
                            asof: w.asof.as_ref().map(|a| UsageObservationAsOf {
                                sequence: a.sequence,
                                placed_count: a.placed_count,
                            }),
                            reported_sequence: w.reported_sequence,
                            unresolved_equal_charge: w.unresolved_equal_charge,
                        },
                    )
                })
                .collect(),
            window_set_sequence: p.window_set_sequence,
            ..PlacementState::default()
        });
    if let Some(snapshot) = &c.quota {
        state.observe(snapshot);
    }
    state
}

fn evidence(
    c: &CandidateInput,
    raw: &MetricInputs,
    now: DateTime<Utc>,
) -> (PlacementEvidence, usize) {
    let state = placement_state(c, raw);
    (
        if state.weak(now) {
            PlacementEvidence::Weak
        } else {
            PlacementEvidence::Fresh
        },
        state.count(),
    )
}

pub(super) fn placement_evidence_for(
    inputs: &DecisionInputs,
    c: &CandidateInput,
) -> Option<(PlacementEvidence, usize)> {
    Some(evidence(
        c,
        &inputs.selection.as_deref()?.metrics,
        inputs.now,
    ))
}

pub(super) fn placement_capped_for(inputs: &DecisionInputs, c: &CandidateInput) -> Option<bool> {
    let raw = &inputs.selection.as_deref()?.metrics;
    let (evidence, count) = placement_evidence_for(inputs, c)?;
    Some(evidence == PlacementEvidence::Weak && count >= raw.config.weak_evidence_new_sessions)
}

fn activity(raw: &MetricInputs, active: &BTreeSet<String>, id: &str, now: DateTime<Utc>) -> usize {
    let cutoff = now - chrono::Duration::seconds(raw.config.active_window_secs as i64);
    raw.recent.get(id).map_or(0, |recent| {
        recent
            .iter()
            .filter(|(last, group)| {
                (*last >= cutoff && !raw.idle_placements.contains(group)) || active.contains(group)
            })
            .count()
    })
}

pub fn active_sessions(inputs: &DecisionInputs, id: &str) -> Option<usize> {
    let selection = inputs.selection.as_deref()?;
    Some(activity(
        &selection.metrics,
        &selection.affinity.active,
        id,
        inputs.now,
    ))
}

fn reservations(raw: &MetricInputs, id: &str, except: Option<&str>, now: DateTime<Utc>) -> usize {
    raw.overload_marks
        .iter()
        .filter(|(g, mark)| Some(g.as_str()) != except && mark.target == id && mark.expires > now)
        .count()
}

pub fn reserved(inputs: &DecisionInputs, id: &str, except: Option<&str>) -> Option<usize> {
    Some(reservations(
        &inputs.selection.as_deref()?.metrics,
        id,
        except,
        inputs.now,
    ))
}

fn threshold(raw: &MetricInputs, id: &str) -> usize {
    raw.thresholds
        .get(id)
        .copied()
        .unwrap_or(raw.config.spread_threshold)
}

pub fn spread_threshold(inputs: &DecisionInputs, id: &str) -> Option<usize> {
    Some(threshold(&inputs.selection.as_deref()?.metrics, id))
}

pub fn has_overload_mark(inputs: &DecisionInputs, key: &str) -> Option<bool> {
    Some(
        inputs
            .selection
            .as_deref()?
            .metrics
            .overload_marks
            .get(super::group(key))
            .is_some_and(|mark| mark.expires > inputs.now),
    )
}

pub(super) fn rebalance_target_waiting_for(
    inputs: &DecisionInputs,
    source: &str,
    c: &CandidateInput,
    except: Option<&str>,
) -> Option<bool> {
    let raw = &inputs.selection.as_deref()?.metrics;
    let id = &c.account.id;
    let m = metrics_for(inputs, c, false)?;
    let target = active_sessions(inputs, id)? + reserved(inputs, id, except)?;
    let source_count = active_sessions(inputs, source)?;
    let facts = super::candidate_facts(inputs, c, None);
    let eligible = super::effective_account(&facts, id)?.eligible_at(None, inputs.now);
    Some(waiting(
        raw,
        c,
        &m,
        eligible,
        target,
        source_count,
        except,
        inputs.now,
    ))
}

// Keep the calculation separate from the parent's context construction so raw
// fixture parity can exercise all balance branches before parent integration.
#[allow(clippy::too_many_arguments)]
fn waiting(
    raw: &MetricInputs,
    c: &CandidateInput,
    m: &Metrics,
    eligible: bool,
    target: usize,
    source_count: usize,
    except: Option<&str>,
    now: DateTime<Utc>,
) -> bool {
    // `except` is already a group, unlike the key accepted by has_overload_mark.
    let require_balance =
        except.is_none_or(|g| raw.overload_marks.get(g).is_some_and(|m| m.expires > now));
    !c.account.drain
        && eligible
        && m.gate
        && !m.unknown
        && (m.weekly.is_some() || m.five.is_some())
        && target < threshold(raw, &c.account.id)
        && (!require_balance || target.saturating_add(2) <= source_count)
}

pub(super) fn rebalance_target_base_for(
    inputs: &DecisionInputs,
    source: &str,
    c: &CandidateInput,
    except: Option<&str>,
) -> Option<bool> {
    rebalance_target_waiting_for(inputs, source, c, except)
}

pub(super) fn rebalance_target_for(
    inputs: &DecisionInputs,
    source: &str,
    c: &CandidateInput,
    except: Option<&str>,
) -> Option<bool> {
    Some(rebalance_target_base_for(inputs, source, c, except)? && !placement_capped_for(inputs, c)?)
}

pub fn compare_gated(
    inputs: &DecisionInputs,
    a: &str,
    x: &Metrics,
    b: &str,
    y: &Metrics,
    anchor: Option<DateTime<Utc>>,
) -> Option<Ordering> {
    inputs.accounts.get(a)?;
    inputs.accounts.get(b)?;
    compare_in_order(
        &inputs.selection.as_deref()?.account_order,
        a,
        x,
        b,
        y,
        anchor,
    )
}

fn compare_in_order(
    order: &[String],
    a: &str,
    x: &Metrics,
    b: &str,
    y: &Metrics,
    anchor: Option<DateTime<Utc>>,
) -> Option<Ordering> {
    // Validate both positions even when an earlier metric resolves the order.
    let a = order.iter().position(|id| id == a)?;
    let b = order.iter().position(|id| id == b)?;
    let cohort = |t: Option<DateTime<Utc>>| match (anchor, t) {
        (Some(start), Some(t)) if (t - start).num_seconds() <= 10800 => (0, None),
        (_, Some(t)) => (1, Some(t)),
        (_, None) => (2, None),
    };
    Some(
        x.unknown
            .cmp(&y.unknown)
            .then_with(|| cohort(x.weekly).cmp(&cohort(y.weekly)))
            .then_with(|| x.five.is_none().cmp(&y.five.is_none()))
            .then_with(|| x.five.cmp(&y.five))
            .then_with(|| x.usage.total_cmp(&y.usage))
            .then_with(|| a.cmp(&b)),
    )
}

fn calculate(
    c: &CandidateInput,
    raw: &MetricInputs,
    cfg: &RoutingConfig,
    pending: usize,
    now: DateTime<Utc>,
    stale: impl Fn(&Value) -> bool,
) -> Metrics {
    let mut m = Metrics {
        gate: true,
        unknown: false,
        weekly: None,
        five: None,
        usage: 0.0,
    };
    let Some(windows) = c.quota.as_ref().and_then(|q| q["windows"].as_array()) else {
        m.unknown = true;
        m.usage = f64::INFINITY;
        return m;
    };
    let pending = pending.max(c.inflight);
    let state = placement_state(c, raw);
    let mut seen = false;
    let mut slot_usage = [None::<f64>; 2];
    for w in windows {
        let Some(weekly) = account_window(c.account.provider, w) else {
            continue;
        };
        seen = true;
        let reset = date(&w["reset_at"]);
        let expired = reset.is_some_and(|r| r <= now);
        let reset = reset.filter(|r| *r > now);
        let charge = cfg.placement_allowance_pct
            * state.charge_count(w["name"].as_str().expect("gating window")) as f64;
        let usage = if expired {
            Some(cfg.inflight_allowance_pct * pending as f64 + charge)
        } else if stale(w) {
            None
        } else {
            w["used_pct"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n + cfg.inflight_allowance_pct * pending as f64 + charge)
        };
        // Gate every window; the highest projected usage (including D20
        // charges) supplies its slot's reset key. Unknown still sorts last.
        let projected = usage.unwrap_or(f64::INFINITY);
        let slot = if weekly { &mut m.weekly } else { &mut m.five };
        let previous = &mut slot_usage[usize::from(weekly)];
        if previous.is_none_or(|old| projected > old) {
            *previous = Some(projected);
            *slot = reset;
        } else if *previous == Some(projected) {
            *slot = match (*slot, reset) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        let Some(usage) = usage else {
            m.unknown = true;
            m.usage = f64::INFINITY;
            continue;
        };
        m.usage = m.usage.max(usage);
        let near = reset.is_some_and(|r| (r - now).num_seconds() as u64 <= cfg.near_reset_secs);
        let gate = if weekly {
            if near {
                cfg.gate_weekly_near_reset_pct
            } else {
                cfg.gate_weekly_pct
            }
        } else {
            cfg.gate_5h_pct
        };
        if usage >= gate {
            m.gate = false;
        }
    }
    if !seen {
        m.unknown = true;
        m.usage = f64::INFINITY;
    }
    m
}

#[cfg(test)]
mod tests;
