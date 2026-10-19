use super::tests::{candidate, fixture_dir};
use super::*;

#[test]
fn failed_root_selection_cannot_accumulate_unbounded_cost_metadata() {
    let (_root, t, _cs) = fixture();
    let before = t.data.lock().unwrap().last_request_bytes.len();
    for n in 0..=CAPACITY {
        let key = format!("claude:failed-{n}");
        assert!(matches!(
            t.choose(Provider::Claude, Some(&key), None, None, &[], None, 100),
            Decision::None
        ));
        t.unknown_request_cost(Some(&key));
    }
    let d = t.data.lock().unwrap();
    assert_eq!(d.last_request_bytes.len(), before);
    assert!(d.unknown_cost.len() <= d.groups.len());
}

#[test]
fn restored_root_stays_unknown_after_small_child_until_full_root_proof() {
    let (_root, t, cs) = fixture();
    t.import(t.export().unwrap()).unwrap();
    t.note_success(&cs[1].account.id, Utc::now());
    t.request_bytes(Some("claude:a0:agent:small"), 10);
    assert_eq!(
        place(&t, "claude:a0:agent:small", &cs[..2], 10),
        cs[0].account.id
    );
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs[..2]), 0);
    assert!(t.data.lock().unwrap().unknown_cost.contains("claude:a0"));
    t.request_bytes(Some("claude:a0"), 1000);
    assert!(trigger(&t, &cs[0].account.id, "a", &cs[..2]) > 0);
}

#[test]
fn drained_unaccepted_placement_never_becomes_warm_by_merely_retrying() {
    let (_root, t, mut cs) = fixture();
    let decision = t.choose(
        Provider::Claude,
        Some("claude:new"),
        None,
        None,
        &cs[..1],
        None,
        100,
    );
    let Decision::Selected { lease: old, .. } = decision else {
        panic!("fixture selection");
    };
    assert!(!t.existing_placement(Some("claude:new"), &cs[0].account.id));
    cs[0].account.drain = true;
    let Decision::Selected {
        account,
        lease: new,
        ..
    } = t.choose(
        Provider::Claude,
        Some("claude:new"),
        None,
        None,
        &cs[..2],
        None,
        100,
    )
    else {
        panic!("drained unaccepted target must be replaced");
    };
    assert_eq!(account, cs[1].account.id);
    old.dispatched(); // a stale receipt cannot admit the successor.
    assert!(!t.existing_placement(Some("claude:new"), &account));
    new.dispatched();
    assert!(t.existing_placement(Some("claude:new"), &account));
    drop(old);
    drop(new);
    cs[1].account.drain = true;
    assert_eq!(place(&t, "claude:new", &cs[..2], 100), cs[1].account.id);
}

fn fixture() -> (tempfile::TempDir, Arc<BindingTable>, Vec<Candidate>) {
    let root = fixture_dir();
    let mut cfg = RoutingConfig {
        spread_threshold: 2,
        ..RoutingConfig::default()
    };
    cfg.churn.target_quiet_secs = 0;
    let t = BindingTable::new(root.path(), cfg).unwrap();
    let cs = vec![
        candidate("cl-aaaaaa", 10., 8, 1),
        candidate("cl-bbbbbb", 10., 30, 2),
        candidate("cl-cccccc", 10., 40, 3),
    ];
    for n in 0..6 {
        place(&t, &format!("claude:a{n}"), &cs[..1], 100);
    }
    t.note_success(&cs[1].account.id, Utc::now());
    (root, t, cs)
}
fn place(t: &Arc<BindingTable>, key: &str, cs: &[Candidate], bytes: u64) -> String {
    match t.choose(Provider::Claude, Some(key), None, None, cs, None, bytes) {
        Decision::Selected { account, lease, .. } => {
            lease.dispatched();
            drop(lease);
            account
        }
        other => panic!("unexpected selection {other:?}"),
    }
}
fn trigger(t: &BindingTable, id: &str, prefix: &str, cs: &[Candidate]) -> usize {
    let mut marked = 0;
    for n in 0..3 {
        marked += t
            .note_client_rate(
                Provider::Claude,
                id,
                Some(&format!("claude:{prefix}{n}")),
                cs,
                Utc::now(),
            )
            .3;
    }
    marked
}
#[test]
fn routing_target_needs_success_fresh_usage_and_non_pacing_proof() {
    let (_root, t, mut cs) = fixture();
    t.data.lock().unwrap().successes.clear();
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs[..2]), 0);
    t.note_success(&cs[1].account.id, Utc::now());
    cs[1].quota.as_mut().unwrap()["observed_at"] =
        serde_json::json!(Utc::now() - chrono::Duration::seconds(301));
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs[..2]), 0);
    cs[1].quota.as_mut().unwrap()["observed_at"] = serde_json::json!(Utc::now());
    t.set_pacing(&cs[1].account.id, true);
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs[..2]), 0);
    t.set_pacing(&cs[1].account.id, false);
    assert!(trigger(&t, &cs[0].account.id, "a", &cs[..2]) > 0);
}
#[test]
fn routing_mark_is_cancelled_if_target_is_paused_or_load_advantage_disappears() {
    for pause in [false, true] {
        let (_root, t, mut cs) = fixture();
        assert!(trigger(&t, &cs[0].account.id, "a", &cs[..2]) > 0);
        let key = t
            .data
            .lock()
            .unwrap()
            .overload_marks
            .keys()
            .next()
            .unwrap()
            .clone();
        if pause {
            cs[1].account.user_state = UserState::Paused;
        } else {
            t.set_account_threshold(&cs[1].account.id, Some(10));
            for n in 0..6 {
                place(&t, &format!("claude:b{n}"), &cs[1..2], 200);
            }
        }
        assert_eq!(place(&t, &key, &cs[..2], 100), cs[0].account.id);
        assert!(!t.overload_marked(Some(&key), Utc::now()));
    }
}
#[test]
fn routing_active_sibling_cancels_move_and_moved_group_cannot_ping_pong() {
    let (_root, t, cs) = fixture();
    assert!(trigger(&t, &cs[0].account.id, "a", &cs[..2]) > 0);
    let key = t
        .data
        .lock()
        .unwrap()
        .overload_marks
        .keys()
        .next()
        .unwrap()
        .clone();
    let sibling = t.begin_request(Some(&format!("{key}:agent:peer")), &cs[0].account.id);
    assert!(!t.overload_marked(Some(&key), Utc::now()));
    assert_eq!(place(&t, &key, &cs[..2], 100), cs[0].account.id);
    drop(sibling);
    // The active peer cancelled that move. A different, already reserved group
    // can move without bypassing or resetting either account's mark budget.
    let key = t
        .data
        .lock()
        .unwrap()
        .overload_marks
        .keys()
        .next()
        .unwrap()
        .clone();
    assert_eq!(place(&t, &key, &cs[..2], 100), cs[1].account.id);
    for n in 0..6 {
        place(&t, &format!("claude:b{n}"), &cs[1..2], 200);
    }
    {
        let mut d = t.data.lock().unwrap();
        for binding in d
            .bindings
            .values_mut()
            .filter(|b| b.account == cs[0].account.id)
        {
            binding.last_used = Utc::now() - chrono::Duration::seconds(301);
        }
        rebuild_indexes(&mut d);
    }
    t.note_success(&cs[0].account.id, Utc::now());
    assert!(trigger(&t, &cs[1].account.id, "b", &cs[..2]) > 0);
    assert!(
        !t.overload_marked(Some(&key), Utc::now()),
        "the cheap recently moved group must not be a reverse victim"
    );
    assert_eq!(place(&t, &key, &cs[..2], 100), cs[1].account.id);
}
#[test]
fn routing_different_sources_reserve_one_destination_slot_atomically() {
    let (_root, t, cs) = fixture();
    for n in 0..6 {
        place(&t, &format!("claude:c{n}"), &cs[2..], 100);
    }
    t.set_account_threshold(&cs[1].account.id, Some(1));
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs), 1);
    assert_eq!(trigger(&t, &cs[2].account.id, "c", &cs), 0);
    let d = t.data.lock().unwrap();
    assert_eq!(
        d.overload_marks
            .values()
            .filter(|m| m.target == cs[1].account.id)
            .count(),
        1
    );
}
#[test]
fn routing_unknown_restored_cost_and_small_ws_delta_are_never_cheapest_victims() {
    let (_root, t, cs) = fixture();
    let snapshot = t.export().unwrap();
    t.import(snapshot).unwrap();
    t.note_success(&cs[1].account.id, Utc::now());
    assert_eq!(
        trigger(&t, &cs[0].account.id, "a", &cs[..2]),
        0,
        "restored bodies have unknown cost"
    );
    for n in 0..6 {
        t.request_bytes(Some(&format!("claude:a{n}")), 100);
    }
    t.unknown_request_cost(Some("claude:a0"));
    // A handshake or child call cannot turn unknown full context into zero.
    place(&t, "claude:a0", &cs[..1], 0);
    assert!(trigger(&t, &cs[0].account.id, "a", &cs[..2]) > 0);
    assert!(!t.overload_marked(Some("claude:a0"), Utc::now()));
}

#[test]
fn routing_exhausted_discretionary_budget_keeps_healthy_binding() {
    let (_root, t, cs) = fixture();
    let now = Utc::now();
    t.policies
        .dispatch(
            Some("claude:a0"),
            &cs[0].account.id,
            Some(&cs[1].account.id),
            "forced",
            now,
        )
        .unwrap();
    assert!(
        !t.policies
            .discretionary_allowed("claude:a0", Some(&cs[1].account.id), now)
    );
    // Mark production overload for other eligible roots, but the recently
    // moved root must continue on its healthy account rather than return 409.
    trigger(&t, &cs[0].account.id, "a", &cs[..2]);
    assert_eq!(place(&t, "claude:a0", &cs[..2], 100), cs[0].account.id);
}

#[test]
fn routing_old_window_cannot_borrow_fresh_snapshot_timestamp() {
    let (_root, t, mut cs) = fixture();
    cs[1].quota.as_mut().unwrap()["windows"][0]["observed_at"] =
        serde_json::json!(Utc::now() - chrono::Duration::seconds(301));
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs[..2]), 0);
}

#[test]
fn routing_full_group_budget_keeps_healthy_source_but_blocks_mandatory_fourth_move() {
    let (_root, t, mut cs) = fixture();
    let now = Utc::now();
    for (from, to) in [
        ("cl-z", "cl-x"),
        ("cl-x", "cl-y"),
        ("cl-y", cs[0].account.id.as_str()),
    ] {
        t.policies
            .dispatch(Some("claude:a0"), to, Some(from), "forced", now)
            .unwrap();
    }
    assert!(
        !t.policies
            .discretionary_allowed("claude:a0", Some(&cs[1].account.id), now)
    );
    trigger(&t, &cs[0].account.id, "a", &cs[..2]);
    assert_eq!(place(&t, "claude:a0", &cs[..2], 100), cs[0].account.id);
    cs[0].account.user_state = UserState::Paused;
    assert!(matches!(
        t.choose(
            Provider::Claude,
            Some("claude:a0"),
            None,
            None,
            &cs[..2],
            None,
            100
        ),
        Decision::Blocked
    ));
}

#[test]
fn routing_mark_rechecks_unknown_and_current_oversized_input_before_moving() {
    for unknown in [false, true] {
        let (_root, t, cs) = fixture();
        assert!(trigger(&t, &cs[0].account.id, "a", &cs[..2]) > 0);
        let key = t
            .data
            .lock()
            .unwrap()
            .overload_marks
            .keys()
            .next()
            .unwrap()
            .clone();
        let bytes = if unknown {
            t.unknown_request_cost(Some(&key));
            100
        } else {
            t.config.churn.max_move_bytes + 1
        };
        assert_eq!(place(&t, &key, &cs[..2], bytes), cs[0].account.id);
        assert!(!t.overload_marked(Some(&key), Utc::now()));
    }
}

#[test]
fn routing_unproven_cooling_expiry_is_never_used_as_reset_evidence() {
    let (_root, t, mut cs) = fixture();
    cs[0].account.health = Health::Cooling {
        reason: HealthReason::Quota,
        until: Utc::now() + chrono::Duration::seconds(10),
        scope: "account".into(),
    };
    cs[0].account.quota_cooling = None;
    assert!(!matches!(
        t.choose(
            Provider::Claude,
            Some("claude:a0"),
            None,
            None,
            &cs[..2],
            None,
            100
        ),
        Decision::Hold { .. }
    ));
}
