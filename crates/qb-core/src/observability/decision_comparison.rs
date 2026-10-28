//! Decision-tape verdicts. Evaluation belongs to the native router, not this comparator.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Dispatch,
    Hold,
    None,
    Blocked,
    MoveHeld,
    Refusal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub kind: OutcomeKind,
    pub account: Option<String>,
    pub until: Option<DateTime<Utc>>,
    pub refusal: Option<String>,
    pub moves: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub ordinal: u64,
    pub outcome: Outcome,
}

/// Provenance of one executed router. Unsupported decisions are never omitted silently.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub build: String,
    pub tape_sha256: String,
    pub initial_witness: String,
    pub losses: u64,
    pub unsupported: u64,
    pub complete: bool,
    pub frames: Vec<Frame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Insufficient,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub verdict: Verdict,
    pub compared: usize,
    pub differences: Option<usize>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    pub verdict: Verdict,
    pub decisions: usize,
    pub reference_moves: Option<u64>,
    pub candidate_moves: Option<u64>,
    pub margin: u64,
    pub reason: Option<String>,
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_frames(frames: &[Frame]) -> bool {
    !frames.is_empty()
        && frames[0].ordinal > 0
        && frames
            .windows(2)
            .all(|pair| pair[0].ordinal < pair[1].ordinal)
}

fn invalid(run: &Run) -> Option<&'static str> {
    if !digest(&run.build) || !digest(&run.tape_sha256) || !digest(&run.initial_witness) {
        Some("missing_provenance")
    } else if !run.complete || run.losses > 0 || run.unsupported > 0 {
        Some("incomplete_coverage")
    } else if !valid_frames(&run.frames) {
        Some("empty_or_unordered_decisions")
    } else {
        None
    }
}

/// Compare executed outcomes with the capture. This function never evaluates a router.
pub fn identity(recorded: &[Frame], run: &Run) -> Identity {
    let reason = invalid(run).or_else(|| {
        if !valid_frames(recorded) {
            Some("invalid_recorded_decisions")
        } else if recorded.len() != run.frames.len()
            || recorded
                .iter()
                .zip(&run.frames)
                .any(|(a, b)| a.ordinal != b.ordinal)
        {
            Some("decision_coverage_mismatch")
        } else {
            None
        }
    });
    if let Some(reason) = reason {
        return Identity {
            verdict: Verdict::Insufficient,
            compared: 0,
            differences: None,
            reason: Some(reason.into()),
        };
    }
    let differences = recorded
        .iter()
        .zip(&run.frames)
        .filter(|(a, b)| a.outcome != b.outcome)
        .count();
    Identity {
        verdict: if differences == 0 {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        compared: recorded.len(),
        differences: Some(differences),
        reason: None,
    }
}

/// A separately executed planted router must change exactly one decision and fail identity.
pub fn power(recorded: &[Frame], planted: &Run) -> Identity {
    let mut result = identity(recorded, planted);
    if result.verdict != Verdict::Insufficient {
        result.verdict = if result.differences == Some(1) {
            Verdict::Pass
        } else {
            Verdict::Fail
        };
        result.reason = Some("requires_exactly_one_identity_difference".into());
    }
    result
}

/// Margin zero concerns move totals; changed choices remain visible through identity reports.
pub fn paired(recorded: &[Frame], reference: &Run, candidate: &Run, planted: &Run) -> Paired {
    let reference_identity = identity(recorded, reference);
    let reason = if reference_identity.verdict != Verdict::Pass {
        Some("reference_identity_not_passed")
    } else if power(recorded, planted).verdict != Verdict::Pass
        || !reference
            .tape_sha256
            .eq_ignore_ascii_case(&planted.tape_sha256)
        || !reference
            .initial_witness
            .eq_ignore_ascii_case(&planted.initial_witness)
    {
        Some("power_not_passed_on_same_input")
    } else if let Some(reason) = invalid(candidate) {
        Some(reason)
    } else if !reference
        .tape_sha256
        .eq_ignore_ascii_case(&candidate.tape_sha256)
        || !reference
            .initial_witness
            .eq_ignore_ascii_case(&candidate.initial_witness)
    {
        Some("different_input_or_initial_witness")
    } else if reference.frames.len() != candidate.frames.len()
        || reference
            .frames
            .iter()
            .zip(&candidate.frames)
            .any(|(a, b)| a.ordinal != b.ordinal)
    {
        Some("decision_coverage_mismatch")
    } else {
        None
    };
    let sum = |run: &Run| {
        run.frames
            .iter()
            .try_fold(0u64, |n, frame| n.checked_add(frame.outcome.moves))
    };
    let totals = sum(reference).zip(sum(candidate));
    let reason = reason.or_else(|| totals.is_none().then_some("move_counter_overflow"));
    if let Some(reason) = reason {
        return Paired {
            verdict: Verdict::Insufficient,
            decisions: 0,
            reference_moves: None,
            candidate_moves: None,
            margin: 0,
            reason: Some(reason.into()),
        };
    }
    let (reference_moves, candidate_moves) = totals.unwrap();
    Paired {
        verdict: if candidate_moves <= reference_moves {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        decisions: reference.frames.len(),
        reference_moves: Some(reference_moves),
        candidate_moves: Some(candidate_moves),
        margin: 0,
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run() -> Run {
        Run {
            build: "a".repeat(64),
            tape_sha256: "b".repeat(64),
            initial_witness: "c".repeat(64),
            losses: 0,
            unsupported: 0,
            complete: true,
            frames: vec![Frame {
                ordinal: 1,
                outcome: Outcome {
                    kind: OutcomeKind::None,
                    account: None,
                    until: None,
                    refusal: None,
                    moves: 0,
                },
            }],
        }
    }

    fn planted(reference: &Run) -> Run {
        let mut changed = reference.clone();
        changed.frames[0].outcome.kind = OutcomeKind::Blocked;
        changed
    }

    #[test]
    fn planted_nonmove_difference_fails_identity_and_passes_power() {
        let reference = run();
        let mut planted = reference.clone();
        planted.frames[0].outcome.kind = OutcomeKind::Blocked;
        assert_eq!(identity(&reference.frames, &planted).verdict, Verdict::Fail);
        assert_eq!(power(&reference.frames, &planted).differences, Some(1));
        assert_eq!(power(&reference.frames, &planted).verdict, Verdict::Pass);
        assert_eq!(power(&reference.frames, &reference).verdict, Verdict::Fail);
    }

    #[test]
    fn missing_duplicate_empty_and_loss_are_insufficient_not_zero_moves() {
        let reference = run();
        for case in 0..5 {
            let mut candidate = reference.clone();
            match case {
                0 => candidate.frames.clear(),
                1 => candidate.frames.push(candidate.frames[0].clone()),
                2 => candidate.losses = 1,
                3 => candidate.unsupported = 1,
                _ => candidate.initial_witness.clear(),
            }
            let result = paired(
                &reference.frames,
                &reference,
                &candidate,
                &planted(&reference),
            );
            assert_eq!(result.verdict, Verdict::Insufficient);
            assert_eq!(result.candidate_moves, None);
        }
    }

    #[test]
    fn margin_zero_and_different_input_are_enforced() {
        let reference = run();
        let mut candidate = reference.clone();
        candidate.build = "d".repeat(64);
        assert_eq!(
            paired(&reference.frames, &reference, &candidate, &reference).verdict,
            Verdict::Insufficient
        );
        assert_eq!(
            paired(
                &reference.frames,
                &reference,
                &candidate,
                &planted(&reference)
            )
            .verdict,
            Verdict::Pass
        );
        candidate.frames[0].outcome.moves = 1;
        assert_eq!(
            paired(
                &reference.frames,
                &reference,
                &candidate,
                &planted(&reference)
            )
            .verdict,
            Verdict::Fail
        );
        candidate.tape_sha256 = "e".repeat(64);
        assert_eq!(
            paired(
                &reference.frames,
                &reference,
                &candidate,
                &planted(&reference)
            )
            .verdict,
            Verdict::Insufficient
        );
    }
}
