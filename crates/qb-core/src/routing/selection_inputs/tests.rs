use super::*;
use crate::routing::{Movement, SessionPolicy};
use chrono::Duration;
use std::{path::PathBuf, sync::Arc};

struct PrivateHome(PathBuf);

impl PrivateHome {
    fn new() -> Self {
        let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
        let parent = executable.parent().unwrap().join(".sandbox");
        std::fs::create_dir_all(&parent).unwrap();
        let home = parent.join(format!("n6r-policy-{:032x}", rand::random::<u128>()));
        crate::platform::create_private_directory_new(&home).unwrap();
        crate::platform::create_private_directory_new(&home.join("state")).unwrap();
        Self(home)
    }
}

impl Drop for PrivateHome {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn settings(mode: Mode) -> Settings {
    Settings {
        mode,
        reset_wait_secs: 17,
        on_5h_limit: On5hLimit::Wait,
    }
}

fn session(mode: Mode) -> SessionPolicy {
    SessionPolicy {
        project: None,
        baseline: settings(Mode::Recover),
        explicit: Some(settings(mode)),
        anchor: Some("pool-a".to_owned()),
        urgency: None,
        movements: Vec::new(),
    }
}

fn publish(store: &PolicyStore, state: State, witness: State, blocked: bool, lifecycle: bool) {
    let _publication = store.publication.write().unwrap();
    *store.state.lock().unwrap() = Arc::new(state);
    *store.recovery_intent.lock().unwrap() = witness;
    store.recovery_blocked.store(blocked, Ordering::Release);
    store.lifecycle_recovery.store(lifecycle, Ordering::Release);
}

fn assert_plan(actual: &Plan, expected: &Plan) {
    assert_eq!(actual.settings, expected.settings);
    assert_eq!(actual.anchor, expected.anchor);
    assert_eq!(actual.project, expected.project);
    assert_eq!(actual.source, expected.source);
    assert_eq!(actual.revision, expected.revision);
}

fn parity(store: &PolicyStore, key: Option<&str>, now: DateTime<Utc>) -> SelectionPolicyInput {
    let accounts = ["pool-a".to_owned(), "other-b".to_owned()];
    let input = store.selection_inputs(key, &accounts);
    assert_plan(&input.plan(), &store.plan(key));
    let (actual, exists) = input.placement_plan();
    let (native, native_exists) = store.placement_plan(key);
    assert_plan(&actual, &native);
    assert_eq!(exists, native_exists);
    for account in &accounts {
        assert_eq!(
            input.permits_dispatch(account),
            store.permits_dispatch(key, account)
        );
    }
    // Native discretionary reads require a key. None uses the empty native ID.
    for target in [None, Some("pool-a"), Some("other-b")] {
        assert_eq!(
            input.discretionary_allowed(target, now),
            Ok(store.discretionary_allowed(key.unwrap_or(""), target, now))
        );
    }
    input
}

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap()
}

#[test]
fn n6r_policy_modes_urgency_project_and_strict_anchor_match_native() {
    let home = PrivateHome::new();
    let store = PolicyStore::new(&home.0, Settings::default()).unwrap();
    let key = "codex:policy-modes";
    for mode in [Mode::Strict, Mode::Recover, Mode::Rebalance] {
        store
            .set_session(&session_id(key), Some(settings(mode)), None, Some("pool-a"))
            .unwrap();
        let input = parity(&store, Some(key), now());
        assert_eq!(input.plan().settings.mode, mode);
        assert!(input.permits_dispatch("pool-a"));
        assert_eq!(input.permits_dispatch("other-b"), mode != Mode::Strict);
        assert_eq!(
            input.discretionary_allowed(None, now()),
            Ok(mode == Mode::Rebalance)
        );
    }
    let mut row = session(Mode::Strict);
    row.explicit = None;
    row.project = Some("project-id".to_owned());
    row.baseline = settings(Mode::Recover);
    let mut state = State {
        revision: 42,
        ..State::default()
    };
    state.sessions.insert(session_id(key), row.clone());
    publish(&store, state.clone(), State::default(), false, false);
    let input = parity(&store, Some(key), now());
    assert_eq!(input.plan().source, "project");
    assert_eq!(input.plan().settings.mode, Mode::Recover);
    assert_eq!(input.plan().revision, 42);
    assert_eq!(input.plan().project.as_deref(), Some("project-id"));
    row.project = None;
    row.urgency = Some(On5hLimit::Move);
    state.sessions.insert(session_id(key), row);
    publish(&store, state, State::default(), false, false);
    let input = parity(&store, Some(key), now());
    assert_eq!(input.plan().source, "session");
    assert_eq!(input.plan().settings.mode, Mode::Rebalance);
    assert_eq!(input.plan().settings.on_5h_limit, On5hLimit::Move);
}

#[test]
fn n6r_policy_legacy_child_exact_and_witness_recovery_precedence() {
    let home = PrivateHome::new();
    let store = PolicyStore::new(&home.0, Settings::default()).unwrap();
    let key = "codex:policy-root:agent:child";
    let exact = session_id(key);
    let legacy = session_id("codex:policy-root");
    for state_exact in [false, true] {
        for witness_exact in [false, true] {
            for blocked in [false, true] {
                let mut state = State::default();
                state.legacy_codex_roots.insert(legacy.clone());
                state
                    .sessions
                    .insert(legacy.clone(), session(Mode::Recover));
                if state_exact {
                    state
                        .sessions
                        .insert(exact.clone(), session(Mode::Rebalance));
                }
                let mut witness = State::default();
                witness.legacy_codex_roots.insert(legacy.clone());
                witness
                    .sessions
                    .insert(legacy.clone(), session(Mode::Strict));
                if witness_exact {
                    let mut row = session(Mode::Recover);
                    row.anchor = Some("other-b".to_owned());
                    witness.sessions.insert(exact.clone(), row);
                }
                publish(&store, state, witness, blocked, false);
                let input = parity(&store, Some(key), now());
                assert_eq!(input.scope.exact, exact);
                assert_eq!(input.scope.legacy.as_ref(), Some(&legacy));
                let expected = if blocked {
                    if witness_exact {
                        Mode::Recover
                    } else {
                        Mode::Strict
                    }
                } else if state_exact {
                    Mode::Rebalance
                } else {
                    Mode::Recover
                };
                assert_eq!(input.plan().settings.mode, expected);
                assert_eq!(
                    input.plan().anchor.as_deref(),
                    Some(if blocked && witness_exact {
                        "other-b"
                    } else {
                        "pool-a"
                    })
                );
                assert!(input.placement_plan().1);
            }
        }
    }
    // A legacy row alone is not an alias: membership is required independently.
    let mut state = State::default();
    state.sessions.insert(legacy, session(Mode::Strict));
    publish(&store, state, State::default(), true, false);
    let input = parity(&store, Some(key), now());
    assert_eq!(input.plan().settings.mode, Mode::Rebalance);
    assert!(input.placement_plan().1); // recovering, even with no matching row
}

#[test]
fn n6r_policy_witness_presence_and_missing_witness_fallback_match_native() {
    let home = PrivateHome::new();
    let store = PolicyStore::new(&home.0, Settings::default()).unwrap();
    let key = "codex:policy-witness";
    let mut witness = State::default();
    let mut global_row = session(Mode::Strict);
    global_row.explicit = None;
    witness.sessions.insert(session_id(key), global_row);
    publish(&store, State::default(), witness, false, false);
    let input = parity(&store, Some(key), now());
    assert!(input.placement_plan().1);
    assert_eq!(input.plan().anchor, None);
    assert_eq!(input.plan().source, "global");
    let mut state = State::default();
    state
        .sessions
        .insert(session_id(key), session(Mode::Strict));
    publish(&store, state, State::default(), true, false);
    let input = parity(&store, Some(key), now());
    assert_eq!(input.plan().settings.mode, Mode::Strict);
    assert_eq!(input.plan().anchor.as_deref(), Some("pool-a"));
    assert!(!input.permits_dispatch("pool-a"));
}

#[test]
fn n6r_policy_flags_dispatch_veto_without_extra_discretionary_vetoes() {
    let home = PrivateHome::new();
    let store = PolicyStore::new(&home.0, Settings::default()).unwrap();
    let key = "codex:policy-flags";
    for mode in [Mode::Strict, Mode::Recover, Mode::Rebalance] {
        for frozen in [false, true] {
            for blocked in [false, true] {
                for lifecycle in [false, true] {
                    let mut state = State {
                        frozen,
                        ..State::default()
                    };
                    state.sessions.insert(session_id(key), session(mode));
                    publish(&store, state, State::default(), blocked, lifecycle);
                    let input = parity(&store, Some(key), now());
                    assert_eq!(
                        input.permits_dispatch("pool-a"),
                        !frozen && (mode != Mode::Strict || (!blocked && !lifecycle))
                    );
                    assert_eq!(
                        input.discretionary_allowed(None, now()),
                        Ok(mode == Mode::Rebalance)
                    );
                }
            }
        }
    }
}

#[test]
fn n6r_policy_none_scope_and_empty_placement_follow_defaults() {
    let home = PrivateHome::new();
    for mode in [Mode::Strict, Mode::Recover, Mode::Rebalance] {
        let store = PolicyStore::new(&home.0, settings(mode)).unwrap();
        let input = parity(&store, None, now());
        assert_eq!(input.scope.key, None);
        assert_eq!(input.scope.exact, "");
        assert_eq!(input.scope.legacy, None);
        assert_eq!(input.plan().settings.mode, mode);
        assert_eq!(input.plan().source, "global");
        assert!(input.placement_plan().1);
        assert_eq!(input.permits_dispatch("pool-a"), mode != Mode::Strict);
        assert!(
            !parity(&store, Some("codex:absent"), now())
                .placement_plan()
                .1
        );
    }
}

#[test]
fn n6r_policy_group_pool_dwell_revisit_and_overflow_boundaries() {
    let home = PrivateHome::new();
    let limits = ChurnLimits {
        dwell_secs: 10,
        revisit_secs: 20,
        window_secs: 30,
        max_group_moves: 2,
        max_pool_moves: 2,
        ..ChurnLimits::default()
    };
    let store = PolicyStore::new(&home.0, Settings::default())
        .unwrap()
        .with_limits(limits)
        .unwrap();
    let key = "codex:policy-boundaries";
    let time = now();
    for guard in ["group", "pool", "dwell", "revisit", "overflow"] {
        let boundary = match guard {
            "group" | "pool" => 30,
            "dwell" => 10,
            "revisit" => 20,
            "overflow" => 0,
            _ => unreachable!(),
        };
        for offset in [-1, 0, 1] {
            let mut state = State::default();
            let mut row = session(Mode::Rebalance);
            let at = time - Duration::seconds(boundary + offset);
            match guard {
                "pool" => {
                    state.pool_moves.insert("pool".to_owned(), vec![at, at]);
                }
                "overflow" => {
                    state.history_overflow_until = Some(at);
                }
                _ => {
                    for _ in 0..if guard == "group" { 2 } else { 1 } {
                        row.movements.push(Movement {
                            id: None,
                            from: if guard == "revisit" {
                                "pool-a"
                            } else {
                                "other-b"
                            }
                            .to_owned(),
                            to: "pool-a".to_owned(),
                            at,
                            discretionary: guard != "revisit",
                        });
                    }
                }
            }
            state.sessions.insert(session_id(key), row);
            publish(&store, state, State::default(), false, false);
            let input = parity(&store, Some(key), time);
            assert_eq!(
                input.discretionary_allowed(Some("pool-a"), time),
                Ok(offset >= 0),
                "{guard} offset {offset}"
            );
        }
    }
    // Future movement timestamps count; forced moves only impose revisit.
    for discretionary in [false, true] {
        let mut state = State::default();
        let mut row = session(Mode::Rebalance);
        row.movements.push(Movement {
            id: None,
            from: "other-b".to_owned(),
            to: "pool-a".to_owned(),
            at: time + Duration::seconds(100),
            discretionary,
        });
        state.sessions.insert(session_id(key), row);
        publish(&store, state, State::default(), false, false);
        let input = parity(&store, Some(key), time);
        assert_eq!(
            input.discretionary_allowed(Some("pool-a"), time),
            Ok(!discretionary)
        );
        assert_eq!(
            input.discretionary_allowed(Some("other-b"), time),
            Ok(false)
        );
    }
}

#[test]
fn n6r_policy_snapshot_is_scoped_raw_and_preserves_pool_order() {
    let home = PrivateHome::new();
    let store = PolicyStore::new(&home.0, Settings::default()).unwrap();
    let key = "codex:raw-root:agent:child";
    let exact = session_id(key);
    let legacy = session_id("codex:raw-root");
    let unrelated = session_id("codex:unrelated");
    let time = now();
    let times = vec![
        time,
        time + Duration::seconds(50),
        time - Duration::seconds(5),
        time,
    ];
    let mut state = State::default();
    for id in [&exact, &legacy, &unrelated] {
        state.sessions.insert(id.clone(), session(Mode::Strict));
        state.legacy_codex_roots.insert(id.clone());
    }
    state
        .sessions
        .get_mut(&exact)
        .unwrap()
        .movements
        .push(Movement {
            id: Some("must-not-be-read".to_owned()),
            from: "other-b".to_owned(),
            to: "pool-a".to_owned(),
            at: time,
            discretionary: false,
        });
    state.pool_moves.insert("pool".to_owned(), times.clone());
    state.pool_moves.insert("unrelated".to_owned(), vec![time]);
    publish(&store, state.clone(), state, false, false);
    let input = store.selection_inputs(
        Some(key),
        &[
            "pool-a".to_owned(),
            "pool-b".to_owned(),
            "absent-c".to_owned(),
        ],
    );
    for rows in [&input.state, &input.witness] {
        assert_eq!(
            rows.sessions.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([exact.clone(), legacy.clone()])
        );
        assert_eq!(
            rows.legacy_codex_roots,
            BTreeSet::from([exact.clone(), legacy.clone()])
        );
        assert_eq!(rows.sessions[&exact].movements[0].from, "other-b");
        assert_eq!(rows.sessions[&exact].movements[0].at, time);
    }
    assert_eq!(
        input.target_pools,
        BTreeMap::from([
            ("pool-a".to_owned(), "pool".to_owned()),
            ("pool-b".to_owned(), "pool".to_owned()),
            ("absent-c".to_owned(), "absent".to_owned())
        ])
    );
    assert_eq!(
        input.pool_moves,
        BTreeMap::from([("pool".to_owned(), times)])
    );
    let value = serde_json::to_value(&input).unwrap();
    assert!(!value.to_string().contains("must-not-be-read"));
    let decoded: SelectionPolicyInput = serde_json::from_value(value.clone()).unwrap();
    assert_plan(&decoded.plan(), &input.plan());
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    let mut unknown = value;
    unknown
        .as_object_mut()
        .unwrap()
        .insert("resolved_plan".to_owned(), serde_json::Value::Null);
    assert!(serde_json::from_value::<SelectionPolicyInput>(unknown).is_err());
    // Snapshot owns its rows; subsequent native publication does not mutate it.
    publish(&store, State::default(), State::default(), false, false);
    assert_eq!(input.plan().settings.mode, Mode::Strict);
    assert_eq!(store.plan(Some(key)).settings.mode, Mode::Rebalance);
}

#[test]
fn n6r_policy_future_pool_and_fractional_window_keep_native_time_rules() {
    let home = PrivateHome::new();
    let limits = ChurnLimits {
        window_secs: 30,
        max_pool_moves: 1,
        ..ChurnLimits::default()
    };
    let store = PolicyStore::new(&home.0, Settings::default())
        .unwrap()
        .with_limits(limits)
        .unwrap();
    let time = now();
    for (at, allowed) in [
        (time + Duration::seconds(100), false),
        (
            time - Duration::seconds(30) + Duration::milliseconds(1),
            false,
        ),
        (time - Duration::seconds(30), true),
        (
            time - Duration::seconds(30) - Duration::milliseconds(1),
            true,
        ),
    ] {
        let mut state = State::default();
        state.pool_moves.insert("pool".to_owned(), vec![at]);
        publish(&store, state, State::default(), false, false);
        let input = parity(&store, Some("codex:pool-time"), time);
        assert_eq!(
            input.discretionary_allowed(Some("pool-a"), time),
            Ok(allowed)
        );
        assert_eq!(input.discretionary_allowed(None, time), Ok(true));
    }
}

#[test]
fn n6r_policy_raw_pool_relation_survives_opaque_id_mapping() {
    let home = PrivateHome::new();
    let store = PolicyStore::new(&home.0, Settings::default()).unwrap();
    let mut input = store.selection_inputs(Some("codex:raw-pool"), &[]);
    // IDs deliberately have no native account-prefix relationship after mapping.
    input.target_pools = BTreeMap::from([("account-X".to_owned(), "pool-Y".to_owned())]);
    input.pool_moves = BTreeMap::from([("pool-Y".to_owned(), vec![now(); 4])]);
    assert_eq!(
        input.discretionary_allowed(Some("account-X"), now()),
        Ok(false)
    );
    input.pool_moves.clear();
    assert_eq!(
        input.discretionary_allowed(Some("account-X"), now()),
        Ok(true)
    );
    input.target_pools.clear();
    assert_eq!(
        input.discretionary_allowed(Some("account-X"), now()),
        Err(PolicyInputRefusal::MissingTargetPool)
    );
}

#[test]
fn n6r_policy_missing_target_pool_is_typed_refusal() {
    let home = PrivateHome::new();
    let store = PolicyStore::new(&home.0, Settings::default()).unwrap();
    let mut input = store.selection_inputs(Some("codex:missing-pool"), &["pool-a".to_owned()]);
    assert_eq!(input.discretionary_allowed(Some("pool-a"), now()), Ok(true));
    assert_eq!(
        input.discretionary_allowed(Some("unknown-b"), now()),
        Err(PolicyInputRefusal::MissingTargetPool)
    );
    input.history_overflow_until = Some(now() + Duration::seconds(1));
    assert_eq!(
        input.discretionary_allowed(Some("unknown-b"), now()),
        Err(PolicyInputRefusal::MissingTargetPool)
    );
    assert_eq!(input.discretionary_allowed(None, now()), Ok(false));
}
