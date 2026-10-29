//! One offline consumer for identity, power and paired decision-tape checks.
//! Loading verifies the native archive; evaluating never returns its observed outcome.
use super::decision_comparison::{self as comparison, Frame, Identity, OutcomeKind, Paired, Run};
use crate::bindings::decision_tape::{DecisionRecord, replay_record};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

/// Constructed by the bounded archive reader from verified persisted bytes.
pub struct Epoch {
    pub producer_build: Option<String>,
    pub tape_sha256: String,
    pub initial_witness: String,
    pub losses: u64,
    pub complete: bool,
    pub decisions: Vec<DecisionRecord>,
    pub permits: Vec<crate::bindings::permit_tape::PermitRecord>,
    pub permit_complete: bool,
    pub(crate) selections: Vec<super::replay_recorder::tape::SelectionEnvelope>,
    pub(crate) selection_complete: bool,
    pub permit_commits: u64,
    pub provenance: super::replay_recorder::Provenance,
    pub capture_started_utc_ns: i64,
    pub capture_ended_utc_ns: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusedDecision {
    pub ordinal: u64,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub schema: u16,
    pub run: Run,
    pub requested_plant: Option<u64>,
    pub planted_ordinal: Option<u64>,
    pub refusals: Vec<RefusedDecision>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: u16,
    pub tape_sha256: String,
    pub initial_witness: String,
    pub reference_build: String,
    pub candidate_build: String,
    pub recorded_decisions: usize,
    pub recorded_rebind_moves: Option<u64>,
    pub losses: u64,
    pub reference_unsupported: u64,
    pub candidate_unsupported: u64,
    pub power_unsupported: u64,
    pub reference_identity: Identity,
    pub candidate_identity: Identity,
    pub power: Identity,
    pub paired: Paired,
}

pub fn load(path: &Path) -> io::Result<Epoch> {
    super::replay_recorder::read_epoch(path)
}

/// Run the router on every recorded input. A refusal is counted, never padded with a decision.
pub fn execute(epoch: &Epoch, build: String, plant: Option<u64>) -> io::Result<Execution> {
    let mut frames = Vec::with_capacity(epoch.decisions.len());
    let mut refusals = Vec::new();
    let mut planted = false;
    for record in &epoch.decisions {
        match replay_record(record) {
            Ok(mut outcome) => {
                if plant == Some(record.ordinal) {
                    if planted {
                        return Err(io::Error::other("duplicate_planted_decision"));
                    }
                    // Explicit power control AFTER native evaluation, affecting exactly one output.
                    outcome.kind = if outcome.kind == OutcomeKind::None {
                        OutcomeKind::Blocked
                    } else {
                        OutcomeKind::None
                    };
                    outcome.account = None;
                    outcome.until = None;
                    outcome.refusal = None;
                    outcome.moves = 0;
                    planted = true;
                }
                frames.push(Frame {
                    ordinal: record.ordinal,
                    outcome,
                });
            }
            Err(reason) => refusals.push(RefusedDecision {
                ordinal: record.ordinal,
                reason: reason.to_string(),
            }),
        }
    }
    if let Some(ordinal) = plant
        && !epoch
            .decisions
            .iter()
            .any(|record| record.ordinal == ordinal)
    {
        return Err(io::Error::other("planted_decision_not_present"));
    }
    Ok(Execution {
        schema: 1,
        run: Run {
            build,
            tape_sha256: epoch.tape_sha256.clone(),
            initial_witness: epoch.initial_witness.clone(),
            losses: epoch.losses,
            unsupported: refusals.len() as u64,
            complete: epoch.complete,
            frames,
        },
        requested_plant: plant,
        planted_ordinal: if planted { plant } else { None },
        refusals,
    })
}

fn bound(epoch: &Epoch, execution: &Execution) -> bool {
    execution.schema == 1
        && execution
            .run
            .tape_sha256
            .eq_ignore_ascii_case(&epoch.tape_sha256)
        && execution
            .run
            .initial_witness
            .eq_ignore_ascii_case(&epoch.initial_witness)
        && execution.run.losses == epoch.losses
        && execution.run.complete == epoch.complete
        && execution.run.unsupported == execution.refusals.len() as u64
        && execution
            .run
            .frames
            .len()
            .checked_add(execution.refusals.len())
            == Some(epoch.decisions.len())
}

/// Bind all runs to the actual epoch before invoking the margin-zero comparator.
pub fn compare(
    epoch: &Epoch,
    reference: &Execution,
    candidate: &Execution,
    planted: &Execution,
    expected_reference_build: &str,
    expected_candidate_build: &str,
) -> io::Result<Report> {
    if !bound(epoch, reference)
        || !bound(epoch, candidate)
        || !bound(epoch, planted)
        || reference.requested_plant.is_some()
        || reference.planted_ordinal.is_some()
        || candidate.requested_plant.is_some()
        || candidate.planted_ordinal.is_some()
        || planted.requested_plant.is_none()
        || planted
            .planted_ordinal
            .is_some_and(|ordinal| Some(ordinal) != planted.requested_plant)
        || !reference
            .run
            .build
            .eq_ignore_ascii_case(expected_reference_build)
        || !candidate
            .run
            .build
            .eq_ignore_ascii_case(expected_candidate_build)
        || !planted
            .run
            .build
            .eq_ignore_ascii_case(expected_candidate_build)
        || !epoch
            .producer_build
            .as_ref()
            .is_some_and(|build| build.eq_ignore_ascii_case(expected_reference_build))
    {
        return Err(io::Error::other("decision_run_provenance_mismatch"));
    }
    let recorded: Vec<_> = epoch
        .decisions
        .iter()
        .map(|record| Frame {
            ordinal: record.ordinal,
            outcome: record.outcome.clone(),
        })
        .collect();
    let differences: Vec<_> = recorded
        .iter()
        .zip(&planted.run.frames)
        .filter(|(a, b)| a.ordinal != b.ordinal || a.outcome != b.outcome)
        .map(|(a, _)| a.ordinal)
        .collect();
    if planted.run.unsupported == 0 && differences.as_slice() != planted.planted_ordinal.as_slice()
    {
        return Err(io::Error::other(
            "planted_ordinal_does_not_identify_single_difference",
        ));
    }
    if planted.planted_ordinal.is_none()
        && !planted
            .refusals
            .iter()
            .any(|refusal| Some(refusal.ordinal) == planted.requested_plant)
    {
        return Err(io::Error::other("missing_planted_execution_or_refusal"));
    }
    Ok(Report {
        schema: 1,
        tape_sha256: epoch.tape_sha256.clone(),
        initial_witness: epoch.initial_witness.clone(),
        reference_build: reference.run.build.clone(),
        candidate_build: candidate.run.build.clone(),
        recorded_decisions: recorded.len(),
        recorded_rebind_moves: recorded
            .iter()
            .try_fold(0u64, |n, frame| n.checked_add(frame.outcome.moves)),
        losses: epoch.losses,
        reference_unsupported: reference.run.unsupported,
        candidate_unsupported: candidate.run.unsupported,
        power_unsupported: planted.run.unsupported,
        reference_identity: comparison::identity(&recorded, &reference.run),
        candidate_identity: comparison::identity(&recorded, &candidate.run),
        power: comparison::power(&recorded, &planted.run),
        paired: comparison::paired(&recorded, &reference.run, &candidate.run, &planted.run),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::decision_tape::{Entry, Inputs, RebindMarker};
    use chrono::{DateTime, Utc};
    use comparison::{Outcome, Verdict};

    fn partial_epoch() -> Epoch {
        Epoch {
            producer_build: Some("c".repeat(64)),
            tape_sha256: "a".repeat(64),
            initial_witness: "b".repeat(64),
            losses: 0,
            complete: true,
            permits: Vec::new(),
            permit_complete: false,
            selections: Vec::new(),
            selection_complete: false,
            permit_commits: 0,
            provenance: super::super::replay_recorder::Provenance::Synthetic,
            capture_started_utc_ns: 0,
            capture_ended_utc_ns: 0,
            decisions: vec![DecisionRecord {
                schema: 1,
                ordinal: 1,
                capture_id: None,
                inputs: Inputs {
                    entry: Entry::Chooser,
                    at: DateTime::parse_from_rfc3339("2026-10-29T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                    provider: crate::accounts::Provider::Codex,
                    key: None,
                    parent: None,
                    model: None,
                    pinned: None,
                    request_bytes: 0,
                    hold_allowed: false,
                    hold_timed_out: false,
                    coverage: None,
                    root_hint_present: false,
                    launch_metadata_valid: None,
                    launch: None,
                },
                state: None,
                answers: Vec::new(),
                rebind: RebindMarker::default(),
                outcome: Outcome {
                    kind: OutcomeKind::None,
                    account: None,
                    until: None,
                    refusal: None,
                    moves: 0,
                },
                capture_aborted: false,
                read_state_complete: false,
                selection_ordinals: Vec::new(),
                selection_refusals: 0,
            }],
        }
    }

    #[test]
    fn partial_inputs_never_echo_observed_outcome_or_zero_pad_the_verdict() {
        let epoch = partial_epoch();
        let reference = execute(&epoch, "c".repeat(64), None).unwrap();
        let candidate = execute(&epoch, "d".repeat(64), None).unwrap();
        let power = execute(&epoch, "d".repeat(64), Some(1)).unwrap();
        assert!(reference.run.frames.is_empty());
        assert_eq!(reference.run.unsupported, 1);
        assert_eq!(power.requested_plant, Some(1));
        assert_eq!(power.planted_ordinal, None);
        let report = compare(
            &epoch,
            &reference,
            &candidate,
            &power,
            &"c".repeat(64),
            &"d".repeat(64),
        )
        .unwrap();
        assert_eq!(report.paired.verdict, Verdict::Insufficient);
        assert_eq!(report.reference_identity.differences, None);
        assert_eq!(report.paired.reference_moves, None);
        assert_eq!(report.paired.candidate_moves, None);
        assert_eq!(report.power_unsupported, 1);
        assert!(
            compare(
                &epoch,
                &reference,
                &reference,
                &power,
                &"c".repeat(64),
                &"d".repeat(64)
            )
            .is_err()
        );
    }

    #[test]
    fn wrong_declared_power_ordinal_is_not_a_valid_control() {
        let epoch = partial_epoch();
        let output = |build: char, changed: bool, declared: Option<u64>| Execution {
            schema: 1,
            run: Run {
                build: build.to_string().repeat(64),
                tape_sha256: epoch.tape_sha256.clone(),
                initial_witness: epoch.initial_witness.clone(),
                losses: 0,
                unsupported: 0,
                complete: true,
                frames: vec![Frame {
                    ordinal: 1,
                    outcome: Outcome {
                        kind: if changed {
                            OutcomeKind::Blocked
                        } else {
                            OutcomeKind::None
                        },
                        account: None,
                        until: None,
                        refusal: None,
                        moves: 0,
                    },
                }],
            },
            requested_plant: declared,
            planted_ordinal: declared,
            refusals: Vec::new(),
        };
        // Comparator-only output fixtures; these are not native execution receipts.
        let reference = output('c', false, None);
        let candidate = output('d', false, None);
        let wrong = output('d', true, Some(999));
        assert!(
            compare(
                &epoch,
                &reference,
                &candidate,
                &wrong,
                &"c".repeat(64),
                &"d".repeat(64)
            )
            .is_err()
        );
        let correct = output('d', true, Some(1));
        assert_eq!(
            compare(
                &epoch,
                &reference,
                &candidate,
                &correct,
                &"c".repeat(64),
                &"d".repeat(64)
            )
            .unwrap()
            .paired
            .verdict,
            Verdict::Pass
        );
    }
}
