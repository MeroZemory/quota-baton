use super::*;
use crate::bindings::tests::{candidate, fixture_dir};

fn assert_same(
    actual: (PreviewDecision, SelectionReason, Option<SoleEligibleTarget>),
    expected: (PreviewDecision, SelectionReason, Option<SoleEligibleTarget>),
) {
    match (&actual.0, &expected.0) {
        (PreviewDecision::Selected { account: a }, PreviewDecision::Selected { account: b }) => {
            assert_eq!(a, b);
        }
        (PreviewDecision::Hold { until: a }, PreviewDecision::Hold { until: b }) => {
            assert_eq!(a, b);
        }
        (PreviewDecision::None, PreviewDecision::None) => {}
        (a, b) => panic!("bridge differs from legacy: {a:?} versus {b:?}"),
    }
    assert_eq!(actual.1.code(), expected.1.code());
    let evidence = |reason: SelectionReason| {
        reason.evidence.map(|proof| {
            (
                proof.kind.code(),
                proof.key,
                proof.from,
                proof.to,
                proof.model,
                proof.at,
            )
        })
    };
    assert_eq!(evidence(actual.1), evidence(expected.1));
    assert_eq!(
        actual.2.map(|proof| proof.decision_input()),
        expected.2.map(|proof| proof.decision_input()),
    );
}

#[test]
fn native_bridge_matches_legacy_entry_points() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let rows = [
        candidate("bridge-a", 20.0, 30, 2),
        candidate("bridge-b", 10.0, 8, 1),
    ];
    match table.choose(
        Provider::Claude,
        Some("claude:bridge-parent"),
        None,
        None,
        &rows[..1],
        None,
        100,
    ) {
        Decision::Selected { lease, .. } => lease.dispatched(),
        other => panic!("fixture placement failed: {other:?}"),
    }
    let now = Utc::now();
    let _mutation = table.mutations.lock().unwrap();
    let data = table.data.lock().unwrap();
    for supplied in [&rows[..], &rows[..1], &[][..]] {
        for (key, parent) in [
            (None, None),
            (Some("claude:bridge-new"), None),
            (Some("claude:bridge-parent"), None),
            (Some("claude:bridge-child"), Some("claude:bridge-parent")),
        ] {
            for pinned in [None, Some(rows[1].account.id.as_str())] {
                let actual = table.select_ordered_and_root_hint(
                    &data,
                    Provider::Claude,
                    key,
                    parent,
                    None,
                    supplied,
                    pinned,
                    now,
                    true,
                    None,
                    None,
                );
                let expected = table.legacy_select_ordered_and_root_hint(
                    &data,
                    Provider::Claude,
                    key,
                    parent,
                    None,
                    supplied,
                    pinned,
                    now,
                    true,
                    None,
                    None,
                );
                assert_same((actual.0, actual.1, None), (expected.0, expected.1, None));
                assert_same(
                    table.select_with_return_guard_and_root_hint(
                        &data,
                        Provider::Claude,
                        key,
                        parent,
                        None,
                        supplied,
                        &rows,
                        None,
                        MoveObservations::untracked(),
                        pinned,
                        now,
                        true,
                        None,
                        None,
                    ),
                    table.legacy_select_with_return_guard_and_root_hint(
                        &data,
                        Provider::Claude,
                        key,
                        parent,
                        None,
                        supplied,
                        &rows,
                        None,
                        MoveObservations::untracked(),
                        pinned,
                        now,
                        true,
                        None,
                        None,
                    ),
                );
            }
        }
    }
}

#[test]
fn native_reason_mapping_preserves_every_label() {
    use super::super::kernel::selection::SelectionLabel;
    for (label, expected) in [
        (SelectionLabel::New, "new"),
        (SelectionLabel::Forced, "forced"),
        (SelectionLabel::Inherited, "inherited"),
        (SelectionLabel::MarkWait, "mark_wait"),
        (SelectionLabel::ColdRebalance, "cold_rebalance"),
        (SelectionLabel::Overload, "overload"),
    ] {
        assert_eq!(selection_code(label).code(), expected);
    }
}

#[test]
fn snapshot_and_evaluation_refusals_never_construct_native_authority() {
    use super::super::kernel_shell::SelectionSnapshotRefusal;
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let now = Utc::now();
    let _mutation = table.mutations.lock().unwrap();
    let data = table.data.lock().unwrap();
    for refusal in [
        SelectionSnapshotRefusal::RootHint,
        SelectionSnapshotRefusal::Clock,
    ] {
        assert_same(
            table.selection_from_snapshot(Err(refusal)),
            (
                PreviewDecision::None,
                SelectionReason {
                    code: SelectionCode::New,
                    evidence: None,
                },
                None,
            ),
        );
    }
    let rows = [candidate("bridge-a", 20.0, 30, 2)];
    let mut inputs = super::super::kernel_shell::selection_snapshot(
        &table,
        &data,
        super::super::kernel::selection::SelectionEntry::Ordered,
        Provider::Claude,
        None,
        None,
        None,
        &rows,
        &[],
        None,
        MoveObservations::untracked(),
        None,
        now,
        true,
        None,
        None,
    )
    .unwrap();
    // A missing identity-order relation is a kernel evaluation error even
    // though this row would otherwise produce an ordinary selection.
    inputs.selection.as_mut().unwrap().account_order.clear();
    assert_same(
        table.selection_from_snapshot(Ok(inputs)),
        (
            PreviewDecision::None,
            SelectionReason {
                code: SelectionCode::New,
                evidence: None,
            },
            None,
        ),
    );
}
