//! Synthetic UNIT controls only: these hashes and completion bits do not attest
//! a producer capture or independent builds. Main owns native source-fault gates.
use super::*;
use crate::{
    accounts::Provider,
    bindings::{
        decision_tape::Entry,
        kernel::{
            DecisionInputs,
            selection::{SelectionEntry, SelectionInputs, SelectionLabel, SelectionPolicyInput},
        },
        permit_tape::IdKind,
    },
    config::RoutingConfig,
    observability::replay_recorder::{Provenance, tape::SelectionEnvelope},
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const FAULT: u64 = 12;

fn now() -> DateTime<Utc> {
    "2026-10-29T00:00:00Z".parse().unwrap()
}

fn mapper(kind: IdKind, raw: &str) -> String {
    format!("{:x}", Sha256::digest(format!("{kind:?}:{raw}")))
}

fn epoch() -> Epoch {
    // Explicit empty read set: no candidates gives None/New, independently of
    // the kernel's result. Capture never evaluates that expectation.
    let policy: SelectionPolicyInput = serde_json::from_value(serde_json::json!({
        "scope": {"key": null, "exact": "", "legacy": null},
        "defaults": {"mode": "rebalance", "reset_wait_secs": 60, "on_5h_limit": "wait"},
        "limits": {}, "revision": 0,
        "state": {"sessions": {}, "legacy_codex_roots": []},
        "witness": {"sessions": {}, "legacy_codex_roots": []},
        "frozen": false, "history_overflow_until": null,
        "recovery_blocked": false, "lifecycle_recovery": false,
        "pool_moves": {}, "target_pools": {}
    }))
    .unwrap();
    let inputs = DecisionInputs {
        now: now(),
        model: None,
        key: None,
        from: None,
        to: String::new(),
        accounts: BTreeMap::new(),
        traffic: BTreeMap::new(),
        pending: BTreeMap::new(),
        groups: BTreeMap::new(),
        bindings: BTreeMap::new(),
        accepted_unbound: BTreeSet::new(),
        sticky: BTreeMap::new(),
        outside_failure: false,
        permit: None,
        selection: Some(Box::new(SelectionInputs {
            entry: SelectionEntry::Ordered,
            provider: Provider::Claude,
            parent: None,
            pinned: None,
            hold_allowed: false,
            preference: None,
            root_hint: None,
            candidates: vec![],
            source_accounts: vec![],
            account_order: vec![],
            affinity: serde_json::from_value(serde_json::json!({
                "bindings": {}, "groups": {}, "binding_count": 0, "lru": [],
                "active": [], "pins": [], "launch_retention": {}, "launch_anchors": {},
                "session_binding_max_secs": 604_800
            }))
            .unwrap(),
            metrics: serde_json::from_value(serde_json::json!({
                "config": RoutingConfig::default(), "recent": {}, "idle_placements": [],
                "thresholds": {}, "overload_marks": {}, "pacing": {}, "placements": {}
            }))
            .unwrap(),
            policy,
            launches: BTreeMap::new(),
            launch_aliases: BTreeSet::new(),
            unknown_cost: BTreeSet::new(),
            last_request_bytes: BTreeMap::new(),
            outside_unknown: false,
            outside_used: BTreeSet::new(),
            entitled_targets: None,
            return_guard: None,
        })),
    };
    let actual = SelectionDecision {
        choice: SelectionChoice::None,
        label: SelectionLabel::New,
        rebalance: None,
        sole_target: None,
        budget_source: None,
    };
    let record = selection::capture(&inputs, &actual, mapper).unwrap();
    Epoch {
        producer_build: Some("a".repeat(64)),
        tape_sha256: "b".repeat(64),
        initial_witness: "c".repeat(64),
        losses: 0,
        complete: false,
        decisions: vec![],
        permits: vec![],
        permit_complete: false,
        selections: (1..=100)
            .map(|ordinal| {
                let mut record = record.clone();
                record.ordinal = ordinal;
                if ordinal == FAULT {
                    record
                        .inputs
                        .as_mut()
                        .unwrap()
                        .sticky
                        .insert("claude".into(), "a:unit-source".into());
                }
                SelectionEnvelope {
                    entry: Entry::Chooser,
                    capture_id: 1,
                    invocation: ordinal,
                    record,
                }
            })
            .collect(),
        // UNIT comparator coverage, not a fabricated producer receipt.
        selection_complete: true,
        permit_commits: 7,
        provenance: Provenance::Synthetic,
        capture_started_utc_ns: 1_000,
        capture_ended_utc_ns: 2_000,
    }
}

fn controls(epoch: &Epoch) -> [Execution; 3] {
    let mut runs = ['a', 'd', 'e'].map(|b| execute(epoch, b.to_string().repeat(64)));
    // Deliberately edited UNIT output, never native power acceptance evidence.
    let fault = frame(&mut runs[2], FAULT);
    fault.label = SelectionLabel::Forced;
    fault.choice = SelectionChoice::Selected {
        account: "a:unit-target".into(),
    };
    fault.budget_source = Some("a:unit-source".into());
    runs
}

fn frame(run: &mut Execution, ordinal: u64) -> &mut SelectionDecision {
    &mut run
        .frames
        .iter_mut()
        .find(|f| f.ordinal == ordinal)
        .unwrap()
        .decision
}

fn comparison(epoch: &Epoch, runs: &[Execution; 3]) -> io::Result<Report> {
    let builds = ['a', 'd', 'e'].map(|b| b.to_string().repeat(64));
    compare(
        epoch,
        &runs[0],
        &runs[1],
        &runs[2],
        builds.each_ref().map(String::as_str),
        FAULT,
    )
}

fn insufficient(report: &Report, reason: &str) {
    assert_eq!(report.verdict, Verdict::Insufficient);
    assert_eq!(report.reason, Some(reason));
    assert_eq!(report.reference_moves, None);
    assert_eq!(report.candidate_moves, None);
}

#[test]
fn unit_controls_execute_codec_and_serialize_exact_observed_coverage() {
    let epoch = epoch();
    let runs = controls(&epoch);
    let report = comparison(&epoch, &runs).unwrap();
    assert_eq!(report.verdict, Verdict::Pass);
    assert_eq!(report.reference_identity.verdict, Verdict::Pass);
    assert_eq!(report.candidate_identity.verdict, Verdict::Pass);
    assert_eq!(report.power.verdict, Verdict::Pass);
    assert_eq!(report.power.differing_ordinals, [FAULT]);
    assert_eq!(report.recorded, report.comparable + report.not_comparable);
    assert_eq!(report.reference_moves, Some(0));
    assert_eq!(report.margin, 0);
    assert_eq!(report.observed_commit_markers, 7);
    assert_eq!(report.observed_completed_rebind_markers, Some(0));
    assert!(report.scope.contains("proposals_only"));
    assert!(matches!(report.provenance, Provenance::Synthetic));
    let encoded = serde_json::to_vec(&runs[0]).unwrap();
    let decoded: Execution = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded.frames, runs[0].frames);
    assert_eq!(decoded.refused, runs[0].refused);
}

#[test]
fn captured_actual_is_not_an_execution_oracle() {
    let mut epoch = epoch();
    epoch.selections[4].record.actual.label = SelectionLabel::Forced;
    let runs = controls(&epoch);
    assert_eq!(runs[0].frames[4].decision.label, SelectionLabel::New);
    let report = comparison(&epoch, &runs).unwrap();
    assert_eq!(report.reference_identity.differing_ordinals, [5]);
    insufficient(&report, "reference_identity_failed");
}

#[test]
fn every_run_role_rejects_duplicate_partial_unordered_or_unbound_outputs() {
    let epoch = epoch();
    for role in 0..3 {
        for case in 0..12 {
            let mut runs = controls(&epoch);
            let run = &mut runs[role];
            match case {
                0 => {
                    run.frames.pop();
                }
                1 => run.frames.push(run.frames[0].clone()),
                2 => run.frames.swap(0, 1),
                3 => run.frames[0].ordinal = 0,
                4 => run.frames[0].ordinal = 101,
                5 => run.build = "x".repeat(64),
                6 => run.tape_sha256 = "f".repeat(64),
                7 => run.initial_witness.clear(),
                8 => run.complete = false,
                9 => run.losses = 1,
                10 => run.schema = 2,
                _ => run.refused.push(Refused {
                    ordinal: 1,
                    reason: WireRefusal::Shape,
                }),
            }
            assert!(
                comparison(&epoch, &runs).is_err(),
                "role {role} case {case}"
            );
        }
    }
}

#[test]
fn producer_hashes_and_source_fault_build_are_bound_to_roles() {
    for case in 0..6 {
        let mut epoch = epoch();
        match case {
            0 => epoch.producer_build = None,
            1 => epoch.producer_build = Some("d".repeat(64)),
            2 => epoch.tape_sha256 = "z".repeat(64),
            3 => epoch.initial_witness = "short".into(),
            4 => epoch.selections[0].record.ordinal = 0,
            _ => epoch.selections.swap(0, 1),
        }
        let runs = controls(&epoch);
        assert!(comparison(&epoch, &runs).is_err(), "case {case}");
    }
    let epoch = epoch();
    let runs = controls(&epoch);
    let mut power = runs[2].clone();
    power.build = runs[0].build.clone();
    assert!(
        compare(
            &epoch,
            &runs[0],
            &runs[1],
            &power,
            [&runs[0].build, &runs[1].build, &power.build],
            FAULT
        )
        .is_err()
    );
    power.build = runs[1].build.clone();
    assert!(
        compare(
            &epoch,
            &runs[0],
            &runs[1],
            &power,
            [&runs[0].build, &runs[1].build, &power.build],
            FAULT
        )
        .is_err()
    );
}

fn refuse(epoch: &mut Epoch, ordinal: u64, reason: WireRefusal) {
    let record = &mut epoch
        .selections
        .iter_mut()
        .find(|e| e.record.ordinal == ordinal)
        .unwrap()
        .record;
    record.inputs = None;
    record.refusal = Some(reason);
}

#[test]
fn actual_refusals_retain_reasons_and_accept_exactly_one_percent() {
    let mut epoch = epoch();
    refuse(&mut epoch, 99, WireRefusal::Model);
    let runs = controls(&epoch);
    assert_eq!(
        runs[0].refused,
        [Refused {
            ordinal: 99,
            reason: WireRefusal::Model
        }]
    );
    let report = comparison(&epoch, &runs).unwrap();
    assert_eq!(report.verdict, Verdict::Pass);
    assert_eq!(
        (report.recorded, report.comparable, report.not_comparable),
        (100, 99, 1)
    );
    refuse(&mut epoch, 100, WireRefusal::Shape);
    insufficient(
        &comparison(&epoch, &controls(&epoch)).unwrap(),
        "not_comparable_exceeds_one_percent",
    );
    epoch.selections.pop();
    insufficient(
        &comparison(&epoch, &controls(&epoch)).unwrap(),
        "not_comparable_exceeds_one_percent",
    );
}

#[test]
fn comparability_sets_and_reasons_must_match_without_zero_padding() {
    let mut epoch = epoch();
    refuse(&mut epoch, 100, WireRefusal::Shape);
    for role in [1, 2] {
        let mut runs = controls(&epoch);
        runs[role].refused[0].reason = WireRefusal::Bounds;
        insufficient(
            &comparison(&epoch, &runs).unwrap(),
            "consumer_refusals_do_not_match_producer",
        );
        let mut runs = controls(&epoch);
        runs[role].refused[0].ordinal = 99;
        runs[role].frames.last_mut().unwrap().ordinal = 100;
        insufficient(
            &comparison(&epoch, &runs).unwrap(),
            "consumer_refusals_do_not_match_producer",
        );
    }
}

#[test]
fn a_dropped_comparable_record_cannot_be_claimed_as_an_under_one_percent_refusal() {
    let epoch = epoch();
    for role in 0..4 {
        let mut runs = controls(&epoch);
        for (index, run) in runs.iter_mut().enumerate() {
            if index == role || role == 3 {
                run.frames.pop();
                run.refused.push(Refused {
                    ordinal: 100,
                    reason: WireRefusal::Shape,
                });
            }
        }
        insufficient(
            &comparison(&epoch, &runs).unwrap(),
            "consumer_refusals_do_not_match_producer",
        );
    }
    // A genuine codec refusal of malformed raw facts is visible but cannot be
    // reinterpreted as a producer-declared refusal or a comparable zero move.
    let mut epoch = epoch;
    epoch.selections[99].record.schema = 0;
    let runs = controls(&epoch);
    assert_eq!(
        runs[0].refused,
        [Refused {
            ordinal: 100,
            reason: WireRefusal::Schema
        }]
    );
    insufficient(
        &comparison(&epoch, &runs).unwrap(),
        "consumer_refusals_do_not_match_producer",
    );
}

#[test]
fn incomplete_losses_and_zero_comparable_cannot_manufacture_zero_moves() {
    for case in 0..3 {
        let mut epoch = epoch();
        match case {
            0 => epoch.selection_complete = false,
            1 => epoch.losses = 1,
            _ => {
                for ordinal in 1..=100 {
                    refuse(&mut epoch, ordinal, WireRefusal::Shape);
                }
            }
        }
        let runs = if case == 2 {
            ['a', 'd', 'e'].map(|b| execute(&epoch, b.to_string().repeat(64)))
        } else {
            controls(&epoch)
        };
        insufficient(
            &comparison(&epoch, &runs).unwrap(),
            if case == 2 {
                "zero_comparable_selections"
            } else {
                "incomplete_capture"
            },
        );
        assert_eq!(runs[0].frames.len() + runs[0].refused.len(), 100);
    }
}

#[test]
fn power_requires_one_difference_at_the_registered_ordinal() {
    let epoch = epoch();
    for case in 0..3 {
        let mut runs = controls(&epoch);
        match case {
            0 => {
                let original = frame(&mut runs[0], FAULT).clone();
                *frame(&mut runs[2], FAULT) = original;
            }
            1 => frame(&mut runs[2], 13).label = SelectionLabel::Forced,
            _ => {
                let original = frame(&mut runs[0], FAULT).clone();
                *frame(&mut runs[2], FAULT) = original;
                frame(&mut runs[2], 13).label = SelectionLabel::Forced;
            }
        }
        insufficient(
            &comparison(&epoch, &runs).unwrap(),
            "independent_power_failed",
        );
    }
    let runs = controls(&epoch);
    let builds = [&*runs[0].build, &*runs[1].build, &*runs[2].build];
    insufficient(
        &compare(&epoch, &runs[0], &runs[1], &runs[2], builds, 13).unwrap(),
        "independent_power_failed",
    );
    for ordinal in [0, 101] {
        assert!(compare(&epoch, &runs[0], &runs[1], &runs[2], builds, ordinal).is_err());
    }
}

#[test]
fn identity_compares_all_outcomes_reasons_and_evidence() {
    use crate::bindings::kernel::selection::SelectionEvidence;
    let epoch = epoch();
    for case in 0..6 {
        let mut runs = controls(&epoch);
        let decision = frame(&mut runs[1], 1);
        match case {
            0 => {
                decision.choice = SelectionChoice::Selected {
                    account: "target".into(),
                }
            }
            1 => decision.choice = SelectionChoice::Hold { until: now() },
            2 => decision.label = SelectionLabel::MarkWait,
            3 => {
                decision.rebalance = Some(SelectionEvidence {
                    kind: SelectionLabel::Overload,
                    key: "key".into(),
                    from: "source".into(),
                    to: "target".into(),
                    model: None,
                    at: now(),
                })
            }
            4 => {
                decision.sole_target = Some(
                    serde_json::from_value(serde_json::json!({
                        "from": "source", "to": "target", "model": null, "at": now()
                    }))
                    .unwrap(),
                )
            }
            _ => decision.budget_source = Some("source".into()),
        }
        let report = comparison(&epoch, &runs).unwrap();
        assert_eq!(
            report.candidate_identity.verdict,
            Verdict::Fail,
            "case {case}"
        );
        assert_eq!(report.candidate_identity.differing_ordinals, [1]);
    }
}

#[test]
fn paired_zero_margin_counts_only_proposed_moves_with_a_known_different_source() {
    for case in 0..5 {
        let mut epoch = epoch();
        if case >= 2 {
            epoch.selections[0]
                .record
                .inputs
                .as_mut()
                .unwrap()
                .sticky
                .insert("claude".into(), "a:unit-source".into());
        }
        let mut runs = controls(&epoch);
        let decision = frame(&mut runs[1], 1);
        decision.choice = match case {
            3 => SelectionChoice::Hold { until: now() },
            4 => SelectionChoice::None,
            _ => SelectionChoice::Selected {
                account: "target".into(),
            },
        };
        decision.budget_source = match case {
            0 => None,                  // New placement, not a move.
            1 => Some("target".into()), // Retention.
            _ => Some("a:unit-source".into()),
        };
        let report = comparison(&epoch, &runs).unwrap();
        assert_eq!(report.reference_moves, Some(0));
        assert_eq!(report.candidate_moves, Some(usize::from(case == 2)));
        assert_eq!(
            report.verdict,
            if case == 2 {
                Verdict::Fail
            } else {
                Verdict::Pass
            }
        );
    }
}

#[test]
fn candidate_cannot_pass_zero_moves_by_silently_dropping_a_dispatch() {
    // UNIT outcome fixture, not native identity/power evidence. The actual
    // synthetic source fault independently exercises this same loss below.
    let mut epoch = epoch();
    let retained = SelectionChoice::Selected {
        account: "a:retained".into(),
    };
    epoch.selections[0].record.actual.choice = retained.clone();
    let mut runs = controls(&epoch);
    frame(&mut runs[0], 1).choice = retained.clone();
    frame(&mut runs[1], 1).choice = retained.clone();
    frame(&mut runs[2], 1).choice = retained;
    frame(&mut runs[1], 1).choice = SelectionChoice::None;
    let report = comparison(&epoch, &runs).unwrap();
    assert_eq!(report.reference_moves, Some(0));
    assert_eq!(report.candidate_moves, Some(0));
    assert_eq!(report.verdict, Verdict::Fail);
    assert_eq!(report.lost_selected_ordinals, [1]);
}

#[test]
fn proposal_counter_uses_raw_source_even_when_candidate_reports_retention() {
    let mut epoch = epoch();
    epoch.selections[0]
        .record
        .inputs
        .as_mut()
        .unwrap()
        .sticky
        .insert("claude".into(), "a:unit-source".into());
    let mut runs = controls(&epoch);
    let changed = frame(&mut runs[1], 1);
    changed.choice = SelectionChoice::Selected {
        account: "a:unit-target".into(),
    };
    changed.budget_source = Some("a:unit-target".into());
    let report = comparison(&epoch, &runs).unwrap();
    assert_eq!(report.reference_moves, Some(0));
    assert_eq!(report.candidate_moves, Some(1));
    assert_eq!(report.verdict, Verdict::Fail);
}

#[test]
fn power_must_fail_the_paired_rule_not_only_change_a_label() {
    let epoch = epoch();
    let mut runs = controls(&epoch);
    let original = frame(&mut runs[0], FAULT).clone();
    *frame(&mut runs[2], FAULT) = original;
    frame(&mut runs[2], FAULT).label = SelectionLabel::Forced;
    insufficient(
        &comparison(&epoch, &runs).unwrap(),
        "independent_power_failed",
    );
}
