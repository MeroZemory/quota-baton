use super::*;
use crate::bindings::{
    BindingTable, Candidate, Data, Decision, MoveObservations, PreviewDecision, kernel_shell,
};
use crate::config::RoutingConfig;
use crate::routing::{Mode, On5hLimit, Settings};
use std::sync::Arc;

fn table() -> (tempfile::TempDir, Arc<BindingTable>) {
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let base = executable
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/selection-kernel-tests");
    std::fs::create_dir_all(&base).unwrap();
    let root = tempfile::tempdir_in(base).unwrap();
    let home = root.path().join("home");
    crate::platform::create_private_directory_new(&home).unwrap();
    crate::platform::create_private_directory_new(&home.join("state")).unwrap();
    let table = BindingTable::new(
        &home,
        RoutingConfig {
            mode: Mode::Recover,
            ..RoutingConfig::default()
        },
    )
    .unwrap();
    (root, table)
}

fn rows() -> Vec<Candidate> {
    let a = crate::bindings::tests::candidate("selection-a", 20.0, 168, 5);
    let mut b = crate::bindings::tests::candidate("selection-b", 10.0, 168, 5);
    b.quota = a.quota.clone();
    b.quota.as_mut().unwrap()["windows"][0]["used_pct"] = serde_json::json!(10.0);
    vec![a, b]
}

fn place(table: &Arc<BindingTable>, key: &str, rows: &[Candidate]) -> String {
    match table.choose(Provider::Claude, Some(key), None, None, rows, None, 32) {
        Decision::Selected { account, lease, .. } => {
            lease.dispatched();
            account
        }
        other => panic!("native fixture placement refused: {other:?}"),
    }
}

struct Case<'a> {
    key: Option<&'a str>,
    parent: Option<&'a str>,
    rows: &'a [Candidate],
    sources: &'a [Candidate],
    pinned: Option<&'a str>,
    hint: Option<&'a crate::boxes::RootHint>,
    now: DateTime<Utc>,
}

fn compare(table: &BindingTable, data: &Data, case: &Case<'_>) -> SelectionEvaluation {
    let native = table.legacy_select_with_return_guard_and_root_hint(
        data,
        Provider::Claude,
        case.key,
        case.parent,
        None,
        case.rows,
        case.sources,
        None,
        MoveObservations::untracked(),
        case.pinned,
        case.now,
        true,
        None,
        case.hint,
    );
    let inputs = kernel_shell::selection_snapshot(
        table,
        data,
        SelectionEntry::ReturnGuard,
        Provider::Claude,
        case.key,
        case.parent,
        None,
        case.rows,
        case.sources,
        None,
        MoveObservations::untracked(),
        case.pinned,
        case.now,
        true,
        None,
        case.hint,
    )
    .unwrap();
    let actual = select(&inputs).unwrap();
    match (&native.0, &actual.decision.choice) {
        (PreviewDecision::Selected { account: a }, SelectionChoice::Selected { account: b }) => {
            assert_eq!(a, b);
        }
        (PreviewDecision::Hold { until: a }, SelectionChoice::Hold { until: b }) => {
            assert_eq!(a, b)
        }
        (PreviewDecision::None, SelectionChoice::None) => {}
        (a, b) => panic!("native selection differs: {a:?} versus {b:?}"),
    }
    let expected_source = match &native.0 {
        PreviewDecision::Selected { account } => {
            table.rebind_budget_source(data, Provider::Claude, case.key, account)
        }
        _ => None,
    };
    assert_eq!(expected_source, actual.decision.budget_source);
    let label = match actual.decision.label {
        SelectionLabel::New => "new",
        SelectionLabel::Forced => "forced",
        SelectionLabel::Inherited => "inherited",
        SelectionLabel::MarkWait => "mark_wait",
        SelectionLabel::ColdRebalance => "cold_rebalance",
        SelectionLabel::Overload => "overload",
    };
    assert_eq!(native.1.code(), label);
    assert_eq!(
        native.1.evidence().is_some(),
        actual.decision.rebalance.is_some()
    );
    if let (Some(expected), Some(evidence)) = (native.1.evidence(), &actual.decision.rebalance) {
        assert!(expected.applies_to(
            case.key,
            &evidence.from,
            &evidence.to,
            evidence.model.as_deref(),
            evidence.at,
        ));
    }
    assert_eq!(
        native.2.as_ref().map(|proof| proof.decision_input()),
        actual.decision.sole_target
    );
    actual
}

#[test]
fn selection_kernel_matches_native_empty_new_bound_parent_pin_and_policy_modes() {
    let (root, table) = table();
    let rows = rows();
    let key = "claude:selection-parity";
    place(&table, key, &rows[..1]);
    for mode in [Mode::Strict, Mode::Recover, Mode::Rebalance] {
        table
            .policies
            .set_session(
                &crate::routing::session_id(key),
                Some(Settings {
                    mode,
                    reset_wait_secs: 60,
                    on_5h_limit: On5hLimit::Wait,
                }),
                None,
                Some(&rows[0].account.id),
            )
            .unwrap();
        let data = table.data.lock().unwrap();
        for supplied in [&rows[..], &rows[..1], &[][..]] {
            for (key, parent) in [
                (None, None),
                (Some("claude:new-selection"), None),
                (Some(key), None),
                (Some("claude:selection-child"), Some(key)),
            ] {
                for pinned in [None, Some(rows[1].account.id.as_str())] {
                    compare(
                        &table,
                        &data,
                        &Case {
                            key,
                            parent,
                            rows: supplied,
                            sources: &rows,
                            pinned,
                            hint: None,
                            now: Utc::now(),
                        },
                    );
                }
            }
        }
    }
    drop(table);
    drop(root);
}

#[test]
fn selection_kernel_keeps_native_tie_order_and_distinct_duplicate_rows() {
    let (_root, table) = table();
    let mut rows = rows();
    rows[0].account.id = "z-native".into();
    rows[1].account.id = "a-native".into();
    rows[1].quota = rows[0].quota.clone();
    let data = table.data.lock().unwrap();
    let result = compare(
        &table,
        &data,
        &Case {
            key: None,
            parent: None,
            rows: &rows,
            sources: &rows,
            pinned: None,
            hint: None,
            now: Utc::now(),
        },
    );
    assert_eq!(
        result.decision.choice,
        SelectionChoice::Selected {
            account: "a-native".into()
        }
    );
    rows[1].account.id = rows[0].account.id.clone();
    rows[0].account.user_state = crate::accounts::UserState::Paused;
    compare(
        &table,
        &data,
        &Case {
            key: Some("claude:duplicate-raw"),
            parent: None,
            rows: &rows,
            sources: &rows[..1],
            pinned: None,
            hint: None,
            now: Utc::now(),
        },
    );
}

#[test]
fn selection_kernel_matches_native_hint_and_outside_source_protection() {
    let (_root, table) = table();
    let rows = rows();
    let old = "claude:hint-old";
    place(&table, old, &rows[..1]);
    let hint = crate::boxes::RootHint::parse(old).unwrap();
    table.set_outside_use(&rows[0].account.id, true);
    for outside_unknown in [false, true] {
        table.set_outside_use_unknown(outside_unknown);
        let data = table.data.lock().unwrap();
        for key in [old, "claude:hint-new"] {
            compare(
                &table,
                &data,
                &Case {
                    key: Some(key),
                    parent: None,
                    rows: &rows,
                    sources: &rows,
                    pinned: None,
                    hint: Some(&hint),
                    now: Utc::now(),
                },
            );
        }
    }
}

#[test]
fn selection_kernel_matches_native_return_alternative_after_real_fixture_move() {
    let (_root, table) = table();
    let mut rows = rows();
    let mut third = rows[0].clone();
    third.account.id = "selection-c".into();
    third.quota.as_mut().unwrap()["windows"][0]["used_pct"] = serde_json::json!(80.0);
    rows.push(third);
    let key = "claude:guard-parity";
    place(&table, key, &rows[..1]);
    rows[0].account.user_state = crate::accounts::UserState::Paused;
    place(&table, key, &rows[..2]);
    rows[0].account.user_state = crate::accounts::UserState::Active;
    rows[0].quota.as_mut().unwrap()["windows"][0]["used_pct"] = serde_json::json!(0.0);
    rows[1].account.user_state = crate::accounts::UserState::Paused;
    let data = table.data.lock().unwrap();
    let result = compare(
        &table,
        &data,
        &Case {
            key: Some(key),
            parent: None,
            rows: &rows,
            sources: &rows,
            pinned: None,
            hint: None,
            now: Utc::now(),
        },
    );
    assert_eq!(
        result.decision.choice,
        SelectionChoice::Selected {
            account: "selection-c".into()
        }
    );
}
