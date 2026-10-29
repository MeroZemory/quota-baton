use super::*;
use crate::accounts::{ImportMode, SourceMatch};
use crate::bindings::kernel::selection::SelectionLabel;
use crate::config::RoutingConfig;
use crate::routing::ChurnLimits;
use serde_json::json;

const A: &str = "secret-account-a";
const B: &str = "secret-account-b";
const ROOT: &str = "claude:secret-root";
const KEY: &str = "claude:secret-root:agent:secret-child";

fn now() -> DateTime<Utc> {
    "2026-10-30T12:34:56.123456789Z".parse().unwrap()
}

fn row(id: &str, quota: Option<Value>, inflight: usize) -> CandidateInput {
    let mut account = AccountFacts {
        id: id.into(),
        generation: 19,
        provider: Provider::Claude,
        plan: Some(Plan::Pro),
        user_state: UserState::Active,
        health: HealthFacts::Ready,
        auth_failures: 0,
        payment_failures: 0,
        payment_until: None,
        auth_probe_pending: false,
        quota_cooling: None,
        quota_credit_releasable: false,
        drain: false,
        allow_credits: false,
    }
    .account()
    .unwrap();
    account.email = "secret-email@example.test".into();
    account.label = "secret-label".into();
    account.identity = Identity::Claude {
        account_uuid: "secret-uuid".into(),
        organization_uuid: Some("secret-org".into()),
        email: account.email.clone(),
    };
    account.source = Source::Cpa {
        mode: ImportMode::Linked,
        dir: "C:/secret-source-path".into(),
        r#match: SourceMatch::Claude {
            account_uuid: "secret-match".into(),
            organization_uuid: None,
            email: account.email.clone(),
        },
    };
    CandidateInput {
        account,
        quota,
        inflight,
    }
}

fn inputs() -> DecisionInputs {
    // Construct only account-free metadata through serde; all native accounts
    // are supplied as typed rows afterward.
    let mut selection: SelectionInputs = serde_json::from_value(json!({
        "entry": "ordered", "provider": "claude", "parent": ROOT, "pinned": A,
        "hold_allowed": true,
        "preference": {"account": B, "coverage": [A, B], "model_move": true},
        "root_hint": {"provider": "claude", "key": ROOT},
        "candidates": [], "source_accounts": [], "account_order": [A, B],
        "affinity": {
            "bindings": {KEY: {"key": KEY, "account": A, "last_used": now(), "accepted": null}},
            "groups": {ROOT: {"keys": [KEY, ROOT], "account": A, "last_used": now()}},
            "binding_count": 3, "lru": [[now(), ROOT], [now(), KEY]],
            "active": [ROOT], "pins": [ROOT], "launch_retention": {ROOT: now()},
            "launch_anchors": {KEY: ROOT}, "session_binding_max_secs": 77
        },
        "metrics": {
            "config": RoutingConfig::default(), "recent": {A: [[now(), ROOT], [now(), KEY]]},
            "idle_placements": [ROOT], "thresholds": {A: 3},
            "overload_marks": {ROOT: {"expires": now(), "from": A, "target": B}},
            "pacing": {B: true},
            "placements": {A: {"latest": 7, "total": 9, "window_set_sequence": 11,
                "windows": {"5h": {"value": {"name": "5h", "used_pct": 12.5,
                    "status": null, "secret-extra": "secret-placement-payload"},
                    "asof": {"sequence": 7, "placed_count": 8}, "reported_sequence": 6,
                    "unresolved_equal_charge": true}}}}
        },
        "policy": {
            "scope": {"key": KEY, "exact": "secret-policy-id", "legacy": "secret-legacy-id"},
            "defaults": {"mode": "recover", "reset_wait_secs": 17, "on_5h_limit": "move"},
            "limits": ChurnLimits::default(), "revision": 55,
            "state": {"sessions": {"secret-policy-id": {
                "project": "secret-project-id", "baseline": {"mode": "strict", "reset_wait_secs": 1},
                "explicit": null, "anchor": A, "urgency": "wait",
                "movements": [{"from": A, "to": B, "at": now(), "discretionary": true}]
            }}, "legacy_codex_roots": ["secret-legacy-id"]},
            "witness": {"sessions": {"secret-legacy-id": {
                "project": "secret-project-id", "baseline": {"mode": "recover", "reset_wait_secs": 2},
                "explicit": {"mode": "rebalance", "reset_wait_secs": 3}, "anchor": B,
                "urgency": null, "movements": [{"from": B, "to": A, "at": now(), "discretionary": false}]
            }}, "legacy_codex_roots": ["secret-legacy-id"]},
            "frozen": false, "history_overflow_until": null, "recovery_blocked": true,
            "lifecycle_recovery": false, "pool_moves": {"secret-pool": [now(), now()]},
            "target_pools": {A: "secret-pool", B: "secret-pool"}
        },
        "launches": {KEY: {"policy_key": KEY, "idle_placement": true}},
        "launch_aliases": [KEY], "unknown_cost": [ROOT], "last_request_bytes": {ROOT: 12345},
        "outside_unknown": true, "outside_used": [A], "entitled_targets": [B],
        "return_guard": {"last_now": now(), "history": [{
            "at": now(), "committed_age_ns": 123, "from": A, "departure_evidence": 456,
            "to": B, "destination_evidence": 789, "emergency_return": true
        }], "target_observation_age_ns": 122}
    })).unwrap();
    selection.candidates = vec![
        row(A, None, 1),
        row(A, Some(Value::Null), 2),
        row(
            B,
            Some(
                json!({"windows": [{"name": "5h", "used_pct": 12.125, "status": null}],
            "credits": {"balance": 123456789}, "secret-extra": "secret-quota-payload"}),
            ),
            3,
        ),
    ];
    selection.source_accounts = vec![row(B, Some(json!({"windows": null})), 9), row(A, None, 10)];
    DecisionInputs {
        now: now(),
        model: Some("claude-opus-4-6".into()),
        key: Some(KEY.into()),
        from: None,
        to: String::new(),
        permit: None,
        accounts: [
            (A.into(), row(A, None, 71)),
            (B.into(), row(B, Some(Value::Null), 72)),
        ]
        .into(),
        traffic: [(A.into(), now())].into(),
        pending: [(B.into(), 23)].into(),
        groups: [(
            ROOT.into(),
            AcceptanceGroup {
                account: A.into(),
                keys: [KEY.into()].into(),
            },
        )]
        .into(),
        bindings: [(
            KEY.into(),
            AcceptanceBinding {
                accepted: Some(false),
            },
        )]
        .into(),
        accepted_unbound: [B.into()].into(),
        sticky: [("claude".into(), A.into())].into(),
        outside_failure: true,
        selection: Some(Box::new(selection)),
    }
}

fn actual() -> SelectionDecision {
    SelectionDecision {
        choice: SelectionChoice::Selected { account: B.into() },
        label: SelectionLabel::Forced,
        budget_source: Some(A.into()),
        rebalance: Some(kernel::selection::SelectionEvidence {
            kind: SelectionLabel::ColdRebalance,
            key: KEY.into(),
            from: A.into(),
            to: B.into(),
            model: Some("opus".into()),
            at: now(),
        }),
        sole_target: Some(kernel::permit::SoleTargetInput {
            from: A.into(),
            to: B.into(),
            model: Some("opus".into()),
            at: now(),
        }),
    }
}

fn mapper() -> impl FnMut(IdKind, &str) -> String {
    let mut ids = BTreeMap::new();
    move |kind, raw| {
        let next = ids.len();
        ids.entry((kind, raw.to_owned()))
            .or_insert_with(|| format!("t{next}"))
            .clone()
    }
}

fn capture_input(input: &DecisionInputs, output: &SelectionDecision) -> SelectionRecord {
    capture(input, output, mapper()).unwrap()
}

#[test]
fn privacy_projection_preserves_distinct_rows_order_and_closed_quota_fields() {
    let record = capture_input(&inputs(), &actual());
    let text = serde_json::to_string(&record).unwrap();
    assert!(
        !text.contains("secret"),
        "native identity or arbitrary payload escaped"
    );
    let parsed: SelectionRecord = serde_json::from_str(&text).unwrap();
    validate(&parsed).unwrap();
    let restored = parsed.inputs.as_ref().unwrap().to_inputs().unwrap();
    assert_eq!(restored.now, now());
    assert_eq!(restored.model.as_deref(), Some("claude-opus-4-6"));
    assert!(restored.outside_failure);
    let s = restored.selection.as_ref().unwrap();
    assert_eq!(
        s.candidates.iter().map(|r| r.inflight).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        s.source_accounts
            .iter()
            .map(|r| r.inflight)
            .collect::<Vec<_>>(),
        [9, 10]
    );
    assert_eq!(s.candidates[0].account.id, s.candidates[1].account.id);
    assert_eq!(s.source_accounts[0].account.id, s.candidates[2].account.id);
    assert_eq!(
        s.account_order,
        [
            s.candidates[0].account.id.clone(),
            s.candidates[2].account.id.clone()
        ]
    );
    assert!(s.candidates[0].quota.is_none());
    assert_eq!(s.candidates[1].quota, Some(Value::Null));
    assert_eq!(
        s.candidates[2].quota.as_ref().unwrap(),
        &json!({
            "windows": [{"name": "5h", "used_pct": 12.125, "status": null}],
            "credits": {"balance": 123456789}
        })
    );
    assert_eq!(s.source_accounts[0].quota, Some(json!({"windows": null})));
    assert!(s.candidates[0].account.email.is_empty());
    assert!(matches!(s.candidates[0].account.source, Source::Oauth));
    assert_eq!(s.candidates[0].account.generation, 19);
    assert_eq!(restored.accounts[&s.candidates[0].account.id].inflight, 71);
}

#[test]
fn every_nested_identity_uses_one_mapper_and_keeps_native_relations() {
    let r = capture_input(&inputs(), &actual());
    let w = r.inputs.as_ref().unwrap();
    let s = &w.selection;
    let a = &s.candidates[0].account.id;
    let b = &s.candidates[2].account.id;
    let key = w.key.as_ref().unwrap();
    let root = s.parent.as_ref().unwrap();
    assert!(key.starts_with(&format!("{root}:agent:")));
    assert_eq!(s.pinned.as_ref(), Some(a));
    assert_eq!(s.preference.as_ref().unwrap().account.as_ref(), Some(b));
    assert_eq!(
        s.preference.as_ref().unwrap().coverage.as_ref().unwrap(),
        &BTreeSet::from([a.clone(), b.clone()])
    );
    assert_eq!(&s.root_hint.as_ref().unwrap().key, root);
    assert_eq!(&s.affinity.bindings[key].account, a);
    assert_eq!(&s.affinity.bindings[key].key, key);
    assert_eq!(s.affinity.groups[root].keys, [key.clone(), root.clone()]);
    assert_eq!(&s.affinity.groups[root].account, a);
    assert_eq!(
        s.affinity.lru,
        [(now(), root.clone()), (now(), key.clone())]
    );
    assert!(s.affinity.active.contains(root) && s.affinity.pins.contains(root));
    assert!(s.affinity.launch_retention.contains_key(root));
    assert_eq!(&s.affinity.launch_anchors[key], root);
    assert_eq!(
        s.metrics.recent[a],
        [(now(), root.clone()), (now(), key.clone())]
    );
    assert!(s.metrics.idle_placements.contains(root));
    assert_eq!(s.metrics.thresholds[a], 3);
    assert_eq!(&s.metrics.overload_marks[root].from, a);
    assert_eq!(&s.metrics.overload_marks[root].target, b);
    assert!(s.metrics.pacing[b]);
    let placement = &s.metrics.placements[a];
    assert_eq!(placement.total, 9);
    assert_eq!(
        placement.windows["5h"].value,
        json!({"name": "5h", "used_pct": 12.5, "status": null})
    );
    assert!(placement.windows["5h"].unresolved_equal_charge);
    let p = &s.policy;
    assert_eq!(p.scope.key.as_ref(), Some(key));
    let state = &p.state.sessions[&p.scope.exact];
    let witness = &p.witness.sessions[p.scope.legacy.as_ref().unwrap()];
    assert_eq!(state.project, witness.project);
    assert_eq!(state.anchor.as_ref(), Some(a));
    assert_eq!(witness.anchor.as_ref(), Some(b));
    assert_eq!(&state.movements[0].from, a);
    assert_eq!(&state.movements[0].to, b);
    assert!(
        p.state
            .legacy_codex_roots
            .contains(p.scope.legacy.as_ref().unwrap())
    );
    assert_eq!(p.target_pools[a], p.target_pools[b]);
    assert_eq!(p.pool_moves[&p.target_pools[a]], [now(), now()]);
    assert_eq!(&s.launches[key].policy_key, key);
    assert!(s.launch_aliases.contains(key) && s.unknown_cost.contains(root));
    assert_eq!(s.last_request_bytes[root], 12345);
    assert!(s.outside_used.contains(a));
    assert!(s.entitled_targets.as_ref().unwrap().contains(b));
    let guard = s.return_guard.as_ref().unwrap();
    assert_eq!(&guard.history[0].from, a);
    assert_eq!(&guard.history[0].to, b);
    assert_eq!(guard.history[0].committed_age_ns, 123);
    assert_eq!(guard.target_observation_age_ns, Some(122));
    assert_eq!(&w.groups[root].account, a);
    assert!(w.groups[root].keys.contains(key));
    assert!(w.bindings.contains_key(key) && w.accepted_unbound.contains(b));
    assert_eq!(&w.sticky["claude"], a);
    assert!(w.traffic.contains_key(a) && w.pending.contains_key(b));
    assert_eq!(r.actual.budget_source.as_ref(), Some(a));
    assert_eq!(
        r.actual.choice,
        SelectionChoice::Selected { account: b.clone() }
    );
    let e = r.actual.rebalance.as_ref().unwrap();
    assert_eq!((&e.key, &e.from, &e.to), (key, a, b));
    assert_eq!(&r.actual.sole_target.as_ref().unwrap().from, a);
}

#[test]
fn explicit_order_survives_mapper_reversal_and_duplicate_rows() {
    let mut other = mapper();
    let r = capture(&inputs(), &actual(), |kind, raw| {
        // Reverse lexical order of accounts without changing explicit order.
        if kind == IdKind::Account && raw == A {
            "z".into()
        } else if kind == IdKind::Account && raw == B {
            "a".into()
        } else {
            other(kind, raw)
        }
    })
    .unwrap();
    let w = r.inputs.as_ref().unwrap();
    assert_eq!(w.selection.account_order, ["a:z", "a:a"]);
    assert_eq!(
        w.selection
            .candidates
            .iter()
            .map(|r| r.account.id.as_str())
            .collect::<Vec<_>>(),
        ["a:z", "a:z", "a:a"]
    );
    assert_eq!(
        w.to_inputs().unwrap().selection.unwrap().account_order,
        ["a:z", "a:a"]
    );
}

#[test]
fn replay_uses_only_input_and_validate_does_not_compare_expected_output() {
    let mut i = inputs();
    i.selection.as_mut().unwrap().candidates.clear();
    let mut r = capture_input(&i, &actual());
    // A recorded selected output deliberately disagrees with the empty read set.
    validate(&r).unwrap();
    assert_eq!(replay(&r).unwrap().choice, SelectionChoice::None);
    r.actual.choice = SelectionChoice::Hold { until: now() };
    validate(&r).unwrap();
    assert_eq!(replay(&r).unwrap().choice, SelectionChoice::None);
}

#[test]
fn native_input_evaluation_matches_replay_after_mapping() {
    let i = inputs();
    let expected = kernel::selection::select(&i).unwrap().decision;
    let r = capture_input(&i, &expected);
    assert_eq!(replay(&r).unwrap(), r.actual);
    // An empty destination vector is a separate raw observation, not a pool
    // to be repopulated from the full health-owner evidence or account index.
    let mut empty = inputs();
    empty.selection.as_mut().unwrap().candidates.clear();
    let expected = kernel::selection::select(&empty).unwrap().decision;
    let r = capture_input(&empty, &expected);
    assert_eq!(replay(&r).unwrap(), r.actual);
}

#[test]
fn budget_source_preserves_same_account_retention_and_move_distinction() {
    let mut output = actual();
    output.choice = SelectionChoice::Selected { account: A.into() };
    let retained = capture_input(&inputs(), &output);
    assert_eq!(
        retained.actual.choice,
        SelectionChoice::Selected {
            account: retained.actual.budget_source.clone().unwrap()
        }
    );
    let moved = capture_input(&inputs(), &actual());
    assert_ne!(
        moved.actual.choice,
        SelectionChoice::Selected {
            account: moved.actual.budget_source.clone().unwrap()
        }
    );
}

#[test]
fn refusals_are_finite_and_do_not_retain_inputs() {
    let mut ids = BTreeMap::new();
    let r = refused_record(&actual(), WireRefusal::Quota, |kind, raw| {
        let next = ids.len();
        ids.entry((kind, raw.to_owned()))
            .or_insert_with(|| format!("t{next}"))
            .clone()
    })
    .unwrap();
    assert!(r.inputs.is_none());
    validate(&r).unwrap();
    assert_eq!(replay(&r), Err(WireRefusal::Quota));
    assert!(!serde_json::to_string(&r).unwrap().contains("secret"));
}

#[test]
fn malformed_ids_collisions_models_quota_and_bounds_refuse_closed() {
    assert!(matches!(
        capture(&inputs(), &actual(), |_, _| "same".into()),
        Err(WireRefusal::Collision)
    ));
    assert!(matches!(
        capture(&inputs(), &actual(), |_, _| "secret@example.test".into()),
        Err(WireRefusal::Identity)
    ));
    let mut i = inputs();
    i.selection.as_mut().unwrap().candidates[0]
        .account
        .id
        .clear();
    assert!(matches!(
        capture(&i, &actual(), mapper()),
        Err(WireRefusal::Identity)
    ));
    let mut i = inputs();
    i.model = Some("secret-unsupported-model".into());
    assert!(matches!(
        capture(&i, &actual(), |_, _| "x".into()),
        Err(WireRefusal::Model)
    ));
    let mut output = actual();
    output.sole_target.as_mut().unwrap().model = Some("secret-unsupported-model".into());
    assert!(matches!(
        refused_record(&output, WireRefusal::Shape, mapper()),
        Err(WireRefusal::Model)
    ));
    let mut i = inputs();
    i.selection.as_mut().unwrap().candidates[0].quota = Some(json!({"windows": "secret"}));
    assert!(matches!(
        capture(&i, &actual(), |_, _| "x".into()),
        Err(WireRefusal::QuotaWindows)
    ));
    let mut i = inputs();
    i.selection.as_mut().unwrap().source_accounts = vec![row(A, None, 0); MAX_ROWS + 1];
    assert!(matches!(
        capture(&i, &actual(), |_, _| "x".into()),
        Err(WireRefusal::Bounds)
    ));
    let mut i = inputs();
    let row = i.selection.as_ref().unwrap().candidates[0].clone();
    i.selection.as_mut().unwrap().candidates = vec![row; MAX_ROWS];
    assert!(matches!(
        capture(&i, &actual(), mapper()),
        Err(WireRefusal::Bounds)
    ));
    let mut i = inputs();
    i.selection.as_mut().unwrap().metrics.config.gate_5h_pct = f64::NAN;
    assert!(matches!(
        capture(&i, &actual(), mapper()),
        Err(WireRefusal::Shape)
    ));
    let mut r = capture_input(&inputs(), &actual());
    r.schema += 1;
    assert_eq!(validate(&r), Err(WireRefusal::Schema));
    let mut r = capture_input(&inputs(), &actual());
    r.actual.budget_source = Some("secret-raw-id".into());
    assert_eq!(validate(&r), Err(WireRefusal::Identity));
    let mut r = capture_input(&inputs(), &actual());
    let row = r.inputs.as_ref().unwrap().selection.candidates[0].clone();
    r.inputs.as_mut().unwrap().selection.candidates = vec![row; MAX_ROWS];
    assert_eq!(validate(&r), Err(WireRefusal::Bounds));
    let mut r = capture_input(&inputs(), &actual());
    r.inputs
        .as_mut()
        .unwrap()
        .selection
        .metrics
        .placements
        .values_mut()
        .next()
        .unwrap()
        .windows
        .get_mut("5h")
        .unwrap()
        .value["extra"] = json!("secret");
    assert_eq!(validate(&r), Err(WireRefusal::Shape));
}

#[test]
fn no_key_sentinel_and_unknown_wire_fields_are_not_filled() {
    let mut i = inputs();
    i.key = None;
    i.selection.as_mut().unwrap().policy.scope.key = None;
    i.selection.as_mut().unwrap().policy.scope.exact.clear();
    let r = capture_input(&i, &actual());
    let w = r.inputs.as_ref().unwrap();
    assert!(w.key.is_none() && w.selection.policy.scope.key.is_none());
    assert!(w.selection.policy.scope.exact.is_empty());
    let mut value = serde_json::to_value(&r).unwrap();
    value["inputs"]["selection"]["candidates"][0]["account"]["email"] = json!("secret");
    assert!(serde_json::from_value::<SelectionRecord>(value).is_err());
}
