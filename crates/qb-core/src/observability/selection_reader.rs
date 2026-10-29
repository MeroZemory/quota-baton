//! Selection proposals from one verified archive. Replay calls the shared kernel
//! through the codec; captured decisions are comparison expectations only.
use super::{decision_comparison::Verdict, decision_reader::Epoch};
use crate::bindings::{
    kernel::selection::{SelectionChoice, SelectionDecision},
    permit_tape::{WireRefusal, selection},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub ordinal: u64,
    pub decision: SelectionDecision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refused {
    pub ordinal: u64,
    pub reason: WireRefusal,
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
    pub observed_completed_rebind_markers: Option<u64>,
    pub observed_commit_markers: u64,
    pub tape_sha256: String,
    pub initial_witness: String,
    pub builds: [String; 3],
    pub expected_fault_ordinal: u64,
    pub reference_identity: Check,
    pub candidate_identity: Check,
    pub power: Check,
    pub reference_moves: Option<usize>,
    pub candidate_moves: Option<usize>,
    pub margin: u8,
}

pub fn execute(epoch: &Epoch, build: String) -> Execution {
    let mut frames = Vec::new();
    let mut refused = Vec::new();
    for envelope in &epoch.selections {
        let ordinal = envelope.record.ordinal;
        match selection::replay(&envelope.record) {
            Ok(decision) => frames.push(Frame { ordinal, decision }),
            Err(reason) => refused.push(Refused { ordinal, reason }),
        }
    }
    Execution {
        schema: 1,
        build,
        tape_sha256: epoch.tape_sha256.clone(),
        initial_witness: epoch.initial_witness.clone(),
        losses: epoch.losses,
        complete: epoch.selection_complete,
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
        && digest(&run.tape_sha256)
        && run.tape_sha256 == epoch.tape_sha256
        && digest(&run.initial_witness)
        && run.initial_witness == epoch.initial_witness
        && run.losses == epoch.losses
        && run.complete == epoch.selection_complete
        && run.frames.len().checked_add(run.refused.len()) == Some(epoch.selections.len())
        && run.frames.windows(2).all(|w| w[0].ordinal < w[1].ordinal)
        && run.refused.windows(2).all(|w| w[0].ordinal < w[1].ordinal)
        && run.frames.iter().all(|f| ordinals.insert(f.ordinal))
        && run.refused.iter().all(|r| ordinals.insert(r.ordinal))
        && epoch
            .selections
            .iter()
            .all(|e| ordinals.contains(&e.record.ordinal))
}

fn identity(epoch: &Epoch, run: &Execution) -> Check {
    let differences = epoch
        .selections
        .iter()
        .filter_map(|envelope| {
            let record = &envelope.record;
            run.frames
                .binary_search_by_key(&record.ordinal, |frame| frame.ordinal)
                .ok()
                .filter(|index| run.frames[*index].decision != record.actual)
                .map(|_| record.ordinal)
        })
        .collect::<Vec<_>>();
    Check {
        verdict: if run.frames.is_empty() {
            Verdict::Insufficient
        } else if differences.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        compared: run.frames.len(),
        differing_ordinals: differences,
        reason: run
            .frames
            .is_empty()
            .then_some("zero_comparable_selections"),
    }
}

fn moves(run: &Execution) -> usize {
    run.frames
        .iter()
        .filter(|frame| {
            matches!((&frame.decision.choice, &frame.decision.budget_source),
            (SelectionChoice::Selected { account }, Some(source)) if account != source)
        })
        .count()
}

/// `power` is output from a separately built source fault. The ordinal must be
/// registered by the caller before comparison, never inferred from differences.
/// No input or output fault is injected by this consumer.
pub fn compare(
    epoch: &Epoch,
    reference: &Execution,
    candidate: &Execution,
    power: &Execution,
    builds: [&str; 3],
    expected_fault_ordinal: u64,
) -> io::Result<Report> {
    let runs = [reference, candidate, power];
    if runs
        .iter()
        .zip(builds)
        .any(|(run, build)| !bound(epoch, run) || run.build != build)
        || epoch.producer_build.as_deref() != Some(builds[0])
        || power.build == reference.build
        || power.build == candidate.build
        || expected_fault_ordinal == 0
        || !epoch
            .selections
            .iter()
            .any(|e| e.record.ordinal == expected_fault_ordinal)
        || epoch.selections.iter().any(|e| e.record.ordinal == 0)
        || !epoch
            .selections
            .windows(2)
            .all(|w| w[0].record.ordinal < w[1].record.ordinal)
    {
        return Err(io::Error::other(
            "selection_execution_provenance_or_coverage_mismatch",
        ));
    }
    let reference_identity = identity(epoch, reference);
    let candidate_identity = identity(epoch, candidate);
    let mut power = identity(epoch, power);
    power.verdict = if power.differing_ordinals == [expected_fault_ordinal] {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    power.reason = Some("separately_built_source_fault_requires_exact_registered_difference");
    let observed_completed_rebind_markers = epoch
        .decisions
        .iter()
        .try_fold(0u64, |n, record| n.checked_add(record.rebind.completed));
    let producer_refused = epoch
        .selections
        .iter()
        .filter_map(|envelope| {
            envelope.record.refusal.map(|reason| Refused {
                ordinal: envelope.record.ordinal,
                reason,
            })
        })
        .collect::<Vec<_>>();
    let reason = if epoch.selections.is_empty() || !epoch.selection_complete || epoch.losses > 0 {
        Some("incomplete_capture")
    } else if runs.iter().any(|run| run.frames.is_empty()) {
        Some("zero_comparable_selections")
    } else if runs
        .iter()
        .any(|run| (run.refused.len() as u128) * 100 > epoch.selections.len() as u128)
    {
        Some("not_comparable_exceeds_one_percent")
    } else if runs.iter().any(|run| run.refused != producer_refused) {
        Some("consumer_refusals_do_not_match_producer")
    } else if reference_identity.verdict != Verdict::Pass {
        Some("reference_identity_failed")
    } else if power.verdict != Verdict::Pass {
        Some("independent_power_failed")
    } else if observed_completed_rebind_markers.is_none() {
        Some("completed_rebind_counter_overflow")
    } else {
        None
    };
    let reference_moves = reason.is_none().then(|| moves(reference));
    let candidate_moves = reason.is_none().then(|| moves(candidate));
    let times = epoch
        .selections
        .iter()
        .filter_map(|e| e.record.inputs.as_ref().map(|i| i.now))
        .collect::<Vec<_>>();
    Ok(Report {
        schema: 1,
        scope: "selection_move_proposals_only_completed_rebinds_not_replayed",
        verdict: if reason.is_some() {
            Verdict::Insufficient
        } else if candidate_moves <= reference_moves {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        reason,
        provenance: epoch.provenance,
        capture_elapsed_ns: epoch
            .capture_ended_utc_ns
            .saturating_sub(epoch.capture_started_utc_ns),
        input_span_seconds: times
            .iter()
            .min()
            .zip(times.iter().max())
            .map(|(a, b)| (*b - *a).num_seconds()),
        recorded: epoch.selections.len(),
        comparable: reference.frames.len(),
        not_comparable: reference.refused.len(),
        maximum_not_comparable_percent: 1,
        losses: epoch.losses,
        observed_completed_rebind_markers,
        observed_commit_markers: epoch.permit_commits,
        tape_sha256: epoch.tape_sha256.clone(),
        initial_witness: epoch.initial_witness.clone(),
        builds: builds.map(str::to_owned),
        expected_fault_ordinal,
        reference_identity,
        candidate_identity,
        power,
        reference_moves,
        candidate_moves,
        margin: 0,
    })
}

#[cfg(test)]
mod tests;
