//! Actual BindingTable fixtures; no substitute selector or hint-owned state.
use super::*;
use crate::bindings::tests::{candidate, fixture_dir};

fn fixture(cap: usize) -> (tempfile::TempDir, Arc<BindingTable>, [Candidate; 2]) {
    let home = fixture_dir();
    let config = RoutingConfig {
        weak_evidence_new_sessions: cap,
        ..RoutingConfig::default()
    };
    let table = BindingTable::new(home.path(), config).unwrap();
    let candidates = [candidate("a", 5.0, 8, 1), candidate("b", 10.0, 8, 1)];
    for c in &candidates {
        table.set_account_threshold(&c.account.id, Some(8));
    }
    place(&table, "claude:root", &candidates[1..]);
    (home, table, candidates)
}

fn place(table: &Arc<BindingTable>, key: &str, candidates: &[Candidate]) -> String {
    match table.choose(
        Provider::Claude,
        Some(key),
        None,
        None,
        candidates,
        None,
        100,
    ) {
        Decision::Selected { account, lease, .. } => {
            lease.dispatched();
            account
        }
        other => panic!("synthetic native placement failed: {other:?}"),
    }
}

fn hinted(table: &Arc<BindingTable>, key: &str, root: &str, candidates: &[Candidate]) -> String {
    let hint = crate::boxes::RootHint::parse(root).unwrap();
    match table.choose_with_hold_at_admitted_proof_and_root_hint(
        Provider::Claude,
        Some(key),
        None,
        None,
        candidates,
        candidates,
        MoveObservations::untracked(),
        None,
        100,
        false,
        false,
        Utc::now(),
        None,
        Some(&hint),
        |_| Some(DispatchAdmission::ordinary()),
    ) {
        Decision::Selected { account, lease, .. } => {
            lease.dispatched();
            account
        }
        other => panic!("synthetic hinted placement failed: {other:?}"),
    }
}

fn preview(
    table: &BindingTable,
    key: &str,
    candidates: &[Candidate],
    hint: Option<&crate::boxes::RootHint>,
    preference: Option<&launch::SelectionPreference<'_>>,
) -> (PreviewDecision, &'static str) {
    let _mutation = table.mutations.lock().unwrap();
    let data = table.data.lock().unwrap();
    let (choice, reason) = table.select_ordered_and_root_hint(
        &data,
        Provider::Claude,
        Some(key),
        None,
        None,
        candidates,
        None,
        Utc::now(),
        false,
        preference,
        hint,
    );
    (choice, reason.code())
}

fn selected(account: &str) -> PreviewDecision {
    PreviewDecision::Selected {
        account: account.into(),
    }
}

#[test]
fn new_placement_prefers_retained_root_with_room_without_touching_it() {
    let (_home, table, candidates) = fixture(100);
    let hint = crate::boxes::RootHint::parse("claude:root").unwrap();
    assert_eq!(
        preview(&table, "claude:new", &candidates, None, None).0,
        selected("a")
    );
    assert_eq!(
        preview(&table, "claude:new", &candidates, Some(&hint), None),
        (selected("b"), "new")
    );
    let before = table.data.lock().unwrap().bindings["claude:root"].last_used;
    assert_eq!(
        hinted(&table, "claude:new", "claude:root", &candidates),
        "b"
    );
    let data = table.data.lock().unwrap();
    assert_eq!(data.bindings["claude:root"].last_used, before);
    assert_eq!(data.groups["claude:new"].account, "b");
    assert!(data.bindings.contains_key("claude:new"));
    assert!(!data.bindings.contains_key("claude:new:agent:root"));
}

#[test]
fn root_without_room_loses_to_native_room_partition() {
    let (_home, table, candidates) = fixture(100);
    table.set_account_threshold("b", Some(1));
    assert_eq!(
        hinted(&table, "claude:new", "claude:root", &candidates),
        "a"
    );
}

#[test]
fn exhausted_root_keeps_native_choice() {
    let (_home, table, mut candidates) = fixture(100);
    candidates[1].account.health = Health::Cooling {
        reason: HealthReason::Quota,
        until: Utc::now() + chrono::Duration::hours(1),
        scope: "account".into(),
    };
    assert_eq!(
        hinted(&table, "claude:new", "claude:root", &candidates),
        "a"
    );
}

#[test]
fn preexisting_group_and_sibling_cannot_be_redirected() {
    let (_home, table, candidates) = fixture(100);
    place(&table, "claude:existing", &candidates[..1]);
    assert_eq!(
        hinted(&table, "claude:existing", "claude:root", &candidates),
        "a"
    );
    assert_eq!(
        hinted(
            &table,
            "claude:existing:agent:child",
            "claude:root",
            &candidates
        ),
        "a"
    );
}

#[test]
fn unknown_wrong_provider_and_expired_roots_keep_native_choice() {
    for root in ["claude:unknown", "codex:root", "claude:root"] {
        let (_home, table, candidates) = fixture(100);
        if root == "claude:root" {
            let _mutation = table.mutations.lock().unwrap();
            let mut data = table.data.lock().unwrap();
            data.bindings.get_mut(root).unwrap().last_used = Utc::now()
                - chrono::Duration::seconds(table.config.session_binding_max_secs as i64 + 1);
            rebuild_indexes(&mut data);
        }
        assert_eq!(hinted(&table, "claude:new", root, &candidates), "a");
    }
}

#[test]
fn existing_recovery_choice_and_native_reason_are_identical() {
    let (_home, table, candidates) = fixture(100);
    let mut source = candidate("source", 10.0, 8, 1);
    place(&table, "claude:existing", std::slice::from_ref(&source));
    source.account.health = Health::NeedsRelogin {
        reason: "synthetic".into(),
        since: Utc::now(),
    };
    let all = [source, candidates[0].clone(), candidates[1].clone()];
    let hint = crate::boxes::RootHint::parse("claude:root").unwrap();
    let native = preview(&table, "claude:existing", &all, None, None);
    assert_eq!(native.0, selected("a"));
    assert_eq!(
        preview(&table, "claude:existing", &all, Some(&hint), None),
        native
    );
}

#[test]
fn native_parent_and_pin_keep_their_existing_authority() {
    let (_home, table, candidates) = fixture(100);
    place(&table, "claude:native-parent", &candidates[..1]);
    table.set_account_threshold("a", Some(1));
    let hint = crate::boxes::RootHint::parse("claude:root").unwrap();
    let _mutation = table.mutations.lock().unwrap();
    let data = table.data.lock().unwrap();
    let (choice, reason) = table.select_ordered_and_root_hint(
        &data,
        Provider::Claude,
        Some("claude:new"),
        Some("claude:native-parent"),
        None,
        &candidates,
        None,
        Utc::now(),
        false,
        None,
        Some(&hint),
    );
    assert_eq!(choice, selected("a"));
    assert_eq!(reason.code(), "inherited");
    let (choice, reason) = table.select_ordered_and_root_hint(
        &data,
        Provider::Claude,
        Some("claude:new"),
        None,
        None,
        &candidates,
        Some("a"),
        Utc::now(),
        false,
        None,
        Some(&hint),
    );
    assert_eq!(choice, selected("a"));
    assert_eq!(reason.code(), "forced");
}

#[test]
fn inherited_global_policy_row_is_not_a_new_plan() {
    let (_home, table, candidates) = fixture(100);
    let key = "claude:existing-plan";
    table
        .policies
        .set_session(&crate::routing::session_id(key), None, None, None)
        .unwrap();
    let (plan, exists) = table.policies.placement_plan(Some(key));
    assert_eq!(plan.source, "global");
    assert!(plan.anchor.is_none());
    assert!(
        exists,
        "global settings do not mean absent native policy row"
    );
    assert_eq!(hinted(&table, key, "claude:root", &candidates), "a");
}

#[test]
fn coverage_explicit_preference_and_model_move_remain_stronger() {
    let (_home, table, candidates) = fixture(100);
    let hint = crate::boxes::RootHint::parse("claude:root").unwrap();
    let coverage = BTreeSet::from([String::from("a")]);
    for preference in [
        launch::SelectionPreference {
            account: Some("a"),
            coverage: None,
            model_move: false,
        },
        launch::SelectionPreference {
            account: None,
            coverage: Some(&coverage),
            model_move: false,
        },
        launch::SelectionPreference {
            account: None,
            coverage: None,
            model_move: true,
        },
    ] {
        assert_eq!(
            preview(
                &table,
                "claude:new",
                &candidates,
                Some(&hint),
                Some(&preference)
            )
            .0,
            selected("a")
        );
    }
}

#[test]
fn pacing_gate_outside_use_and_evidence_cap_precede_hint() {
    for mode in ["pacing", "gate", "outside", "cap"] {
        let (_home, table, mut candidates) = fixture(if mode == "cap" { 1 } else { 100 });
        match mode {
            "pacing" => table.set_pacing("b", true),
            "gate" => {
                for window in candidates[1].quota.as_mut().unwrap()["windows"]
                    .as_array_mut()
                    .unwrap()
                {
                    window["used_pct"] = serde_json::json!(95.0);
                }
            }
            "outside" => {
                table.data.lock().unwrap().outside_used.insert("b".into());
            }
            "cap" => {}
            _ => unreachable!(),
        }
        assert_eq!(
            hinted(&table, "claude:new", "claude:root", &candidates),
            "a"
        );
    }
}

#[test]
fn all_full_unequal_native_ratio_cannot_be_overridden() {
    let (_home, table, candidates) = fixture(100);
    place(&table, "claude:second-b", &candidates[1..]);
    place(&table, "claude:first-a", &candidates[..1]);
    for id in ["a", "b"] {
        table.set_account_threshold(id, Some(1));
    }
    assert_eq!(
        hinted(&table, "claude:new", "claude:root", &candidates),
        "a"
    );
}

#[test]
fn admission_refusal_never_publishes_a_hinted_binding() {
    let (_home, table, candidates) = fixture(100);
    let hint = crate::boxes::RootHint::parse("claude:root").unwrap();
    let result = table.choose_with_hold_at_admitted_proof_and_root_hint(
        Provider::Claude,
        Some("claude:new"),
        None,
        None,
        &candidates,
        &candidates,
        MoveObservations::untracked(),
        None,
        100,
        false,
        false,
        Utc::now(),
        None,
        Some(&hint),
        |account| {
            assert_eq!(account, "b");
            None
        },
    );
    assert!(matches!(result, Decision::None));
    let data = table.data.lock().unwrap();
    assert!(!data.bindings.contains_key("claude:new"));
    assert!(!data.groups.contains_key("claude:new"));
    assert_eq!(data.groups["claude:root"].account, "b");
}
