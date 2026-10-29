//! Synthetic unit comparator controls only, NOT native power-gate acceptance.
//! Fake build/tape/witness hashes attest nothing. Independently built processes
//! and actual native fault controls are Main's separate acceptance checks.

use super::*;
use crate::bindings::{
    MoveGuardRefusal as Refusal,
    kernel::{DecisionInputs, PermitDenied, PreparedMove},
};
use crate::observability::replay_recorder::Provenance;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

fn now() -> DateTime<Utc> {
    "2026-10-29T00:00:00Z".parse().unwrap()
}

fn mapper(kind: permit_tape::IdKind, raw: &str) -> String {
    format!("{:x}", Sha256::digest(format!("{kind:?}:{raw}")))
}

fn epoch() -> Epoch {
    let inputs = DecisionInputs {
        selection: None,
        now: now(),
        model: None,
        key: None,
        from: None,
        to: "synthetic-target".into(),
        accounts: BTreeMap::new(),
        traffic: BTreeMap::new(),
        pending: BTreeMap::new(),
        groups: BTreeMap::new(),
        bindings: BTreeMap::new(),
        accepted_unbound: BTreeSet::new(),
        sticky: BTreeMap::new(),
        outside_failure: false,
        permit: None,
    };
    // Manual expectation: no source means no move needs permission. Do not
    // derive the captured expectation by calling the kernel under test.
    let captured = permit_tape::capture(&inputs, &PermitDecision::NotRequired, mapper).unwrap();
    permit_tape::validate_record(&captured).unwrap();
    Epoch {
        producer_build: Some("a".repeat(64)),
        tape_sha256: "b".repeat(64),
        initial_witness: "c".repeat(64),
        losses: 0,
        complete: false,
        decisions: vec![],
        permits: (1..=100)
            .map(|ordinal| {
                let mut record = captured.clone();
                record.ordinal = ordinal;
                record
            })
            .collect(),
        permit_complete: true,
        selections: Vec::new(),
        selection_complete: false,
        permit_commits: 0,
        provenance: Provenance::Synthetic,
        capture_started_utc_ns: 1_000,
        capture_ended_utc_ns: 2_000,
    }
}

fn denied(reason: Refusal) -> PermitDecision {
    PermitDecision::Denied(PermitDenied {
        reason,
        until: now(),
        newly_tripped: false,
    })
}

fn set_decision(run: &mut Execution, ordinal: u64, decision: PermitDecision) {
    run.frames
        .iter_mut()
        .find(|frame| frame.ordinal == ordinal)
        .unwrap()
        .decision = decision;
}

fn unit_controls(epoch: &Epoch) -> [Execution; 4] {
    // Each execution calls the real native kernel. These are same-process unit
    // runs, NOT independent baseline/candidate binaries. Only the control
    // frames below are edited; the captured inputs/actuals remain untouched.
    let mut runs = ['a', 'd', 'e', 'f'].map(|ch| execute(epoch, ch.to_string().repeat(64)));
    set_decision(&mut runs[2], 12, denied(Refusal::PingPong));
    set_decision(&mut runs[3], 45, denied(Refusal::SessionTenMinutes));
    runs
}

fn comparison(epoch: &Epoch, runs: &[Execution; 4]) -> io::Result<Report> {
    let builds = ['a', 'd', 'e', 'f'].map(|ch| ch.to_string().repeat(64));
    compare(
        epoch,
        &runs[0],
        &runs[1],
        &runs[2],
        &runs[3],
        builds.each_ref().map(String::as_str),
        12,
        45,
    )
}

fn assert_insufficient(report: &Report, reason: &str) {
    assert_eq!(report.verdict, Verdict::Insufficient);
    assert_eq!(report.reason, Some(reason));
    // Unusable evidence is not padded with zero proposal counts.
    assert_eq!(report.reference_moves, None);
    assert_eq!(report.candidate_moves, None);
}

fn refuse(epoch: &mut Epoch, ordinal: u64) {
    let mut record = permit_tape::refused_record(
        &PermitDecision::NotRequired,
        permit_tape::WireRefusal::Model,
        mapper,
    )
    .unwrap();
    record.ordinal = ordinal;
    assert!(record.inputs.is_none());
    permit_tape::validate_record(&record).unwrap();
    epoch.permits[ordinal as usize - 1] = record;
}

#[test]
fn exact_unit_comparator_controls_pass_with_zero_proposals() {
    let epoch = epoch();
    let runs = unit_controls(&epoch);
    for run in &runs[..2] {
        assert_eq!(run.frames.len(), 100);
        assert!(run.refused.is_empty());
        assert!(
            run.frames
                .iter()
                .all(|f| f.decision == PermitDecision::NotRequired)
        );
    }
    let report = comparison(&epoch, &runs).unwrap();
    assert_eq!(report.verdict, Verdict::Pass);
    assert_eq!(report.reason, None);
    assert_eq!(report.recorded, 100);
    assert_eq!(report.comparable, 100);
    assert_eq!(report.not_comparable, 0);
    assert_eq!(report.reference_moves, Some(0));
    assert_eq!(report.candidate_moves, Some(0));
    assert_eq!(report.margin, 0);
    assert_eq!(report.reference_identity.verdict, Verdict::Pass);
    assert_eq!(report.candidate_identity.verdict, Verdict::Pass);
    assert_eq!(report.reference_identity.compared, 100);
    assert!(report.reference_identity.differing_ordinals.is_empty());
    assert_eq!(report.no_guard_power.verdict, Verdict::Pass);
    assert_eq!(report.no_guard_power.differing_ordinals, [12]);
    assert_eq!(report.free_rule_power.verdict, Verdict::Pass);
    assert_eq!(report.free_rule_power.differing_ordinals, [45]);
    assert!(matches!(report.provenance, Provenance::Synthetic));
}

#[test]
fn coverage_and_provenance_mutations_refuse_every_run_role() {
    let epoch = epoch();
    let original = unit_controls(&epoch);
    for role in 0..4 {
        for mutation in 0..11 {
            let mut runs = original.clone();
            let run = &mut runs[role];
            match mutation {
                0 => {
                    run.frames.pop();
                }
                1 => run.frames.push(Frame {
                    ordinal: 101,
                    decision: PermitDecision::NotRequired,
                }),
                2 => run.frames[1].ordinal = 1,
                3 => run.frames.swap(0, 1),
                4 => run.frames[99].ordinal = 101,
                5 => run.tape_sha256 = "0".repeat(64),
                6 => run.initial_witness = "0".repeat(64),
                7 => run.build = "0".repeat(64),
                8 => run.schema = 2,
                9 => run.complete = false,
                10 => run.losses = 1,
                _ => unreachable!(),
            }
            let error = comparison(&epoch, &runs).unwrap_err();
            assert_eq!(
                error.to_string(),
                "permit_execution_provenance_or_coverage_mismatch",
                "role {role}, mutation {mutation}"
            );
        }
    }
    let mut wrong_producer = epoch;
    wrong_producer.producer_build = Some("0".repeat(64));
    assert!(comparison(&wrong_producer, &original).is_err());
}

#[test]
fn reference_identity_mismatch_is_insufficient_without_zero_padding() {
    let epoch = epoch();
    let mut runs = unit_controls(&epoch);
    set_decision(&mut runs[0], 7, denied(Refusal::Capacity));
    let report = comparison(&epoch, &runs).unwrap();
    assert_insufficient(&report, "reference_identity_failed");
    assert_eq!(report.reference_identity.verdict, Verdict::Fail);
    assert_eq!(report.reference_identity.differing_ordinals, [7]);
    assert_eq!(report.candidate_identity.verdict, Verdict::Pass);
}

#[test]
fn shared_actual_wire_refusals_accept_one_percent_but_not_two() {
    for (count, expected) in [(1, Verdict::Pass), (2, Verdict::Insufficient)] {
        let mut epoch = epoch();
        for ordinal in 1..=count {
            refuse(&mut epoch, ordinal);
        }
        let runs = unit_controls(&epoch);
        for run in &runs {
            assert_eq!(run.refused.len(), count as usize);
            assert!(
                run.refused
                    .iter()
                    .all(|r| r.reason == Some(permit_tape::WireRefusal::Model))
            );
        }
        let report = comparison(&epoch, &runs).unwrap();
        assert_eq!(report.verdict, expected);
        assert_eq!(report.comparable, 100 - count as usize);
        assert_eq!(report.not_comparable, count as usize);
        assert_eq!(report.maximum_not_comparable_percent, 1);
        if count == 1 {
            assert_eq!(report.reference_moves, Some(0));
            assert_eq!(report.candidate_moves, Some(0));
        } else {
            assert_insufficient(&report, "not_comparable_exceeds_one_percent");
        }
    }
}

#[test]
fn different_comparability_sets_are_insufficient() {
    let mut epoch = epoch();
    refuse(&mut epoch, 1);
    let original = unit_controls(&epoch);
    for role in 1..4 {
        let mut runs = original.clone();
        let run = &mut runs[role];
        // Preserve sorted, complete coverage and the 1% limit, but refuse a
        // different ordinal. This isolates set agreement from coverage checks.
        run.refused[0].ordinal = 2;
        run.frames[0] = Frame {
            ordinal: 1,
            decision: PermitDecision::NotRequired,
        };
        let report = comparison(&epoch, &runs).unwrap();
        assert_insufficient(&report, "comparability_sets_differ");
    }
}

#[test]
fn increased_candidate_allowed_proposals_fail_at_zero_margin() {
    let epoch = epoch();
    let mut runs = unit_controls(&epoch);
    // Pure descriptive proposal, with no native permit/mutation authority.
    let proposal = PreparedMove {
        revision: 0,
        shared_incident: None,
        key: "s:synthetic".into(),
        at: now(),
        from: "a:source".into(),
        to: "a:target".into(),
        departure_evidence: 0,
        destination_evidence: 0,
        emergency_return: false,
    };
    set_decision(&mut runs[1], 7, PermitDecision::Allowed(proposal));
    let report = comparison(&epoch, &runs).unwrap();
    assert_eq!(report.verdict, Verdict::Fail);
    assert_eq!(report.reason, None);
    assert_eq!(report.margin, 0);
    assert_eq!(report.reference_moves, Some(0));
    assert_eq!(report.candidate_moves, Some(1));
    assert_eq!(report.candidate_identity.verdict, Verdict::Fail);
    assert_eq!(report.candidate_identity.differing_ordinals, [7]);
    assert_eq!(report.reference_identity.verdict, Verdict::Pass);
}

#[test]
fn missing_inputs_unsealed_capture_and_capture_loss_are_insufficient() {
    for scenario in 0..3 {
        let mut epoch = epoch();
        match scenario {
            0 => {
                // No captured native inputs: retain real wire refusals rather
                // than inventing successful replay outcomes for missing data.
                for ordinal in 1..=100 {
                    refuse(&mut epoch, ordinal);
                }
            }
            1 => epoch.permit_complete = false,
            2 => epoch.losses = 1,
            _ => unreachable!(),
        }
        let mut runs = ['a', 'd', 'e', 'f'].map(|ch| execute(&epoch, ch.to_string().repeat(64)));
        if scenario != 0 {
            set_decision(&mut runs[2], 12, denied(Refusal::PingPong));
            set_decision(&mut runs[3], 45, denied(Refusal::SessionTenMinutes));
        }
        let report = comparison(&epoch, &runs).unwrap();
        assert_insufficient(
            &report,
            if scenario == 0 {
                "not_comparable_exceeds_one_percent"
            } else {
                "incomplete_capture"
            },
        );
        if scenario == 0 {
            assert_eq!(report.comparable, 0);
            assert_eq!(report.not_comparable, 100);
        }
    }
}

#[test]
fn wrong_ordinal_unchanged_and_repeated_unit_fault_controls_fail_power() {
    let epoch = epoch();
    let original = unit_controls(&epoch);
    for (role, expected, reason) in [
        (2, 12, Refusal::PingPong),
        (3, 45, Refusal::SessionTenMinutes),
    ] {
        for scenario in 0..3 {
            let mut runs = original.clone();
            let differences = match scenario {
                0 => {
                    set_decision(&mut runs[role], expected, PermitDecision::NotRequired);
                    set_decision(&mut runs[role], 8, denied(reason));
                    vec![8]
                }
                1 => {
                    set_decision(&mut runs[role], expected, PermitDecision::NotRequired);
                    vec![]
                }
                2 => {
                    set_decision(&mut runs[role], 90, denied(reason));
                    vec![expected, 90]
                }
                _ => unreachable!(),
            };
            let report = comparison(&epoch, &runs).unwrap();
            assert_insufficient(&report, "independent_power_failed");
            let (failed, intact) = if role == 2 {
                (&report.no_guard_power, &report.free_rule_power)
            } else {
                (&report.free_rule_power, &report.no_guard_power)
            };
            assert_eq!(failed.verdict, Verdict::Fail);
            assert_eq!(failed.differing_ordinals, differences);
            assert_eq!(intact.verdict, Verdict::Pass);
        }
    }
}
