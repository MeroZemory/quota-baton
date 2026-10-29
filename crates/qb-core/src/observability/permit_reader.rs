//! Permit slice of the verified decision archive. The captured owner result is
//! used only for comparison; execution calls the raw-fact kernel independently.
use super::{decision_comparison::Verdict, decision_reader::Epoch};
use crate::bindings::{kernel::PermitDecision, permit_tape};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub ordinal: u64,
    pub decision: PermitDecision,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refused {
    pub ordinal: u64,
    pub reason: Option<permit_tape::WireRefusal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub schema: u16,
    pub build: String,
    pub tape_sha256: String,
    pub initial_witness: String,
    pub losses: u64,
    pub complete: bool,
    pub frames: Vec<Frame>,
    pub refused: Vec<Refused>,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub verdict: Verdict,
    pub compared: usize,
    pub differing_ordinals: Vec<u64>,
    pub reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: u16,
    pub scope: &'static str,
    pub verdict: Verdict,
    pub reason: Option<&'static str>,
    pub provenance: super::replay_recorder::Provenance,
    pub capture_elapsed_ns: i64,
    pub input_span_seconds: Option<i64>,
    pub recorded: usize,
    pub comparable: usize,
    pub not_comparable: usize,
    pub maximum_not_comparable_percent: u8,
    pub losses: u64,
    pub observed_commit_markers: u64,
    pub tape_sha256: String,
    pub builds: [String; 4],
    pub reference_identity: Check,
    pub candidate_identity: Check,
    pub no_guard_power: Check,
    pub free_rule_power: Check,
    pub reference_moves: Option<usize>,
    pub candidate_moves: Option<usize>,
    pub margin: u8,
}

pub fn execute(epoch: &Epoch, build: String) -> Execution {
    let mut frames = Vec::new();
    let mut refused = Vec::new();
    for record in &epoch.permits {
        match permit_tape::replay(record) {
            Ok(PermitDecision::NotComparable) => refused.push(Refused {
                ordinal: record.ordinal,
                reason: None,
            }),
            Ok(decision) => frames.push(Frame {
                ordinal: record.ordinal,
                decision,
            }),
            Err(reason) => refused.push(Refused {
                ordinal: record.ordinal,
                reason: Some(reason),
            }),
        }
    }
    Execution {
        schema: 1,
        build,
        tape_sha256: epoch.tape_sha256.clone(),
        initial_witness: epoch.initial_witness.clone(),
        losses: epoch.losses,
        complete: epoch.permit_complete,
        frames,
        refused,
    }
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn bound(epoch: &Epoch, run: &Execution) -> bool {
    let mut ordinals = BTreeSet::new();
    run.schema == 1
        && digest(&run.build)
        && run.tape_sha256 == epoch.tape_sha256
        && digest(&run.tape_sha256)
        && run.initial_witness == epoch.initial_witness
        && digest(&run.initial_witness)
        && run.losses == epoch.losses
        && run.complete == epoch.permit_complete
        && run.frames.len().checked_add(run.refused.len()) == Some(epoch.permits.len())
        && run
            .frames
            .windows(2)
            .all(|pair| pair[0].ordinal < pair[1].ordinal)
        && run
            .refused
            .windows(2)
            .all(|pair| pair[0].ordinal < pair[1].ordinal)
        && run
            .frames
            .iter()
            .all(|frame| ordinals.insert(frame.ordinal))
        && run
            .refused
            .iter()
            .all(|frame| ordinals.insert(frame.ordinal))
        && epoch
            .permits
            .iter()
            .all(|record| ordinals.contains(&record.ordinal))
}

fn identity(epoch: &Epoch, run: &Execution) -> Check {
    let differences = epoch
        .permits
        .iter()
        .filter_map(|record| {
            run.frames
                .binary_search_by_key(&record.ordinal, |frame| frame.ordinal)
                .ok()
                .filter(|index| run.frames[*index].decision != record.actual)
                .map(|_| record.ordinal)
        })
        .collect::<Vec<_>>();
    Check {
        verdict: if differences.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        compared: run.frames.len(),
        differing_ordinals: differences,
        reason: None,
    }
}

fn power(epoch: &Epoch, run: &Execution, expected: u64) -> Check {
    let mut check = identity(epoch, run);
    check.verdict = if check.differing_ordinals == [expected] {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    check.reason = Some("separately_built_fault_requires_exact_expected_difference");
    check
}

/// Expected fault ordinals come from the independently asserted fixture or the
/// capture review. No fault is injected here or into the recorded inputs.
#[allow(clippy::too_many_arguments)]
pub fn compare(
    epoch: &Epoch,
    reference: &Execution,
    candidate: &Execution,
    no_guard: &Execution,
    free_rule: &Execution,
    expected_builds: [&str; 4],
    guard_ordinal: u64,
    free_ordinal: u64,
) -> io::Result<Report> {
    let runs = [reference, candidate, no_guard, free_rule];
    if runs
        .iter()
        .zip(expected_builds)
        .any(|(run, expected)| !bound(epoch, run) || run.build != expected)
        || epoch.producer_build.as_deref() != Some(expected_builds[0])
        || no_guard.build == reference.build
        || free_rule.build == reference.build
        || no_guard.build == free_rule.build
        || guard_ordinal == 0
        || free_ordinal == 0
        || guard_ordinal == free_ordinal
        || !epoch
            .permits
            .iter()
            .any(|record| record.ordinal == guard_ordinal)
        || !epoch
            .permits
            .iter()
            .any(|record| record.ordinal == free_ordinal)
    {
        return Err(io::Error::other(
            "permit_execution_provenance_or_coverage_mismatch",
        ));
    }
    let reference_identity = identity(epoch, reference);
    let candidate_identity = identity(epoch, candidate);
    let no_guard_power = power(epoch, no_guard, guard_ordinal);
    let free_rule_power = power(epoch, free_rule, free_ordinal);
    let refused: Vec<_> = reference.refused.iter().map(|item| item.ordinal).collect();
    let reason = if epoch.permits.is_empty() || !epoch.permit_complete || epoch.losses > 0 {
        Some("incomplete_capture")
    } else if runs
        .iter()
        .any(|run| run.refused.len().saturating_mul(100) > epoch.permits.len())
    {
        Some("not_comparable_exceeds_one_percent")
    } else if runs.iter().any(|run| {
        run.refused
            .iter()
            .map(|item| item.ordinal)
            .collect::<Vec<_>>()
            != refused
    }) {
        Some("comparability_sets_differ")
    } else if reference_identity.verdict != Verdict::Pass {
        Some("reference_identity_failed")
    } else if no_guard_power.verdict != Verdict::Pass || free_rule_power.verdict != Verdict::Pass {
        Some("independent_power_failed")
    } else {
        None
    };
    let count = |run: &Execution| {
        run.frames
            .iter()
            .filter(|frame| matches!(&frame.decision, PermitDecision::Allowed(_)))
            .count()
    };
    let reference_moves = reason.is_none().then(|| count(reference));
    let candidate_moves = reason.is_none().then(|| count(candidate));
    let times: Vec<_> = epoch
        .permits
        .iter()
        .filter_map(|r| r.inputs.as_ref().map(|i| i.now))
        .collect();
    Ok(Report {
        schema: 1,
        scope: "permit_move_proposals_only_selection_not_replayed",
        reason,
        verdict: if reason.is_some() {
            Verdict::Insufficient
        } else if candidate_moves <= reference_moves {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        provenance: epoch.provenance,
        capture_elapsed_ns: epoch
            .capture_ended_utc_ns
            .saturating_sub(epoch.capture_started_utc_ns),
        input_span_seconds: times
            .iter()
            .min()
            .zip(times.iter().max())
            .map(|(a, b)| (*b - *a).num_seconds()),
        recorded: epoch.permits.len(),
        comparable: reference.frames.len(),
        not_comparable: reference.refused.len(),
        maximum_not_comparable_percent: 1,
        losses: epoch.losses,
        observed_commit_markers: epoch.permit_commits,
        tape_sha256: epoch.tape_sha256.clone(),
        reference_identity,
        candidate_identity,
        builds: expected_builds.map(str::to_owned),
        no_guard_power,
        free_rule_power,
        reference_moves,
        candidate_moves,
        margin: 0,
    })
}

#[cfg(test)]
mod tests;
