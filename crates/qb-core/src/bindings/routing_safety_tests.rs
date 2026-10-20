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
    let cfg = RoutingConfig {
        spread_threshold: 2,
        ..RoutingConfig::default()
    };
    let t = BindingTable::new(root.path(), cfg).unwrap();
    let cs = vec![
        candidate("cl-aaaaaa", 10., 8, 1),
        candidate("cl-bbbbbb", 10., 30, 2),
        candidate("cl-cccccc", 10., 40, 3),
    ];
    for n in 0..6 {
        place(&t, &format!("claude:a{n}"), &cs[..1], 100);
    }
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
fn routing_idle_target_needs_c7_non_stale_usage_not_success_or_quiet() {
    let (_root, t, mut cs) = fixture();
    let now = Utc::now();
    for w in cs[1].quota.as_mut().unwrap()["windows"]
        .as_array_mut()
        .unwrap()
    {
        w["observed_at"] = serde_json::json!(now - chrono::Duration::seconds(899));
    }
    t.set_pacing(&cs[1].account.id, true);
    t.note_rate(&cs[1].account.id, now);
    assert!(
        trigger(&t, &cs[0].account.id, "a", &cs[..2]) > 0,
        "fresh idle target needs no success or quiet period; pacing is a preference"
    );
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
    let sibling = t.begin_request(Some(&format!("{key}:agent:peer")), &cs[0].account.id);
    assert!(!t.overload_marked(Some(&key), Utc::now()));
    assert_eq!(place(&t, &key, &cs[..2], 100), cs[0].account.id);
    drop(sibling);
    // The other reservation remains available after the active peer cancels.
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
    fresh_observation(&t, &mut cs[..1], Utc::now());
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
    let request = t.begin_request(None, &cs[1].account.id);
    cs[1].quota.as_mut().unwrap()["windows"][0]["observed_at"] =
        serde_json::json!(Utc::now() - chrono::Duration::seconds(901));
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs[..2]), 0);
    drop(request);
}

#[test]
fn routing_four_forced_moves_do_not_spend_discretionary_budget() {
    let (_root, t, mut cs) = fixture();
    let now = Utc::now();
    for (from, to) in [
        ("cl-z", "cl-x"),
        ("cl-x", "cl-y"),
        ("cl-y", "cl-q"),
        ("cl-q", cs[0].account.id.as_str()),
    ] {
        t.policies
            .dispatch(Some("claude:a0"), to, Some(from), "forced", now)
            .unwrap();
    }
    assert!(
        t.policies
            .discretionary_allowed("claude:a0", Some("cl-unused"), now)
    );
    cs[0].account.user_state = UserState::Paused;
    assert_eq!(place(&t, "claude:a0", &cs[..2], 100), cs[1].account.id);
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
        assert!(
            t.overload_marked(Some(&key), Utc::now()),
            "unknown/oversized cost veto keeps the unexpired mark"
        );
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

fn observed_at(cs: &mut [Candidate], at: DateTime<Utc>) {
    for c in cs {
        let Some(q) = &mut c.quota else { continue };
        q["observed_at"] = serde_json::json!(at);
        for w in q["windows"].as_array_mut().unwrap() {
            w["observed_at"] = serde_json::json!(at);
        }
    }
}
fn fresh_observation(t: &BindingTable, cs: &mut [Candidate], now: DateTime<Utc>) {
    observed_at(cs, now);
    for c in cs {
        let asof = t.begin_usage_observation_for(&c.account.id);
        t.note_usage_observation_asof_token(&c.account.id, c.quota.as_ref().unwrap(), asof);
    }
}
fn decide_at(
    t: &Arc<BindingTable>,
    key: &str,
    cs: &[Candidate],
    now: DateTime<Utc>,
) -> (String, Vec<MoveEvent>) {
    match t.choose_with_hold_at(
        Provider::Claude,
        Some(key),
        None,
        None,
        cs,
        None,
        100,
        true,
        false,
        now,
    ) {
        Decision::Selected {
            account,
            lease,
            moves,
        } => {
            lease.dispatched();
            drop(lease);
            (account, moves)
        }
        other => panic!("expected placement: {other:?}"),
    }
}
fn place_at(t: &Arc<BindingTable>, key: &str, cs: &[Candidate], at: DateTime<Utc>) -> MoveEvent {
    let (_, mut moves) = decide_at(t, key, cs, at);
    assert_eq!(moves.len(), 1);
    moves.remove(0)
}
fn placement_fixture() -> (
    tempfile::TempDir,
    Arc<BindingTable>,
    Vec<Candidate>,
    DateTime<Utc>,
) {
    let root = fixture_dir();
    let t = BindingTable::new(
        root.path(),
        RoutingConfig {
            spread_threshold: 100,
            ..RoutingConfig::default()
        },
    )
    .unwrap();
    let now = Utc::now();
    let mut cs = vec![candidate("cl-a", 0., 8, 1), candidate("cl-b", 30., 30, 2)];
    observed_at(&mut cs, now);
    (root, t, cs, now)
}

#[test]
fn d20_no_observation_burst_is_five_five_and_gate_can_override_cap() {
    for used in [30., 87.] {
        let (_root, t, mut cs, now) = placement_fixture();
        for w in cs[1].quota.as_mut().unwrap()["windows"]
            .as_array_mut()
            .unwrap()
        {
            w["used_pct"] = serde_json::json!(used);
        }
        let mut counts = [0usize; 2];
        let mut proved_lower_count_failing_b = false;
        for n in 0..10 {
            let at = now + chrono::Duration::seconds(n + 1);
            let b_stopped_at_gate = used == 87. && counts[1] == 2;
            if b_stopped_at_gate {
                let data = t.data.lock().unwrap();
                let a = metrics(&cs[0], &data, &t.config, at);
                let b = metrics(&cs[1], &data, &t.config, at);
                assert!(a.gate, "A remains a gate-passing placement candidate");
                assert!(!b.gate, "B's two placement charges exhaust its gate");
                assert_eq!(b.usage, 91.);
                assert!(placement_capped(&cs[0], &data, &t.config, at));
                if counts[0] > counts[1] {
                    assert!(
                        placement_evidence(&cs[1], &data, at).1
                            < placement_evidence(&cs[0], &data, at).1
                    );
                    proved_lower_count_failing_b = true;
                }
            }
            let event = place_at(&t, &format!("claude:burst{n}"), &cs, at);
            if n == 0 {
                assert_eq!(event.to, "cl-a");
            }
            if b_stopped_at_gate {
                assert_eq!(
                    event.to, "cl-a",
                    "every remaining placement must stay in the passing set"
                );
            }
            counts[usize::from(event.to == "cl-b")] += 1;
            if used == 30. {
                assert!(counts[0].abs_diff(counts[1]) <= 1);
            }
            assert!(event.is_placement);
            assert_eq!(event.placement_charge_pct, 2.);
        }
        if used == 87. {
            assert!(
                proved_lower_count_failing_b,
                "regression must exercise B2 against a capped passing A3-or-more before choosing"
            );
        }
        assert_eq!(counts, if used == 30. { [5, 5] } else { [8, 2] });
        fresh_observation(&t, &mut cs[..1], now + chrono::Duration::seconds(20));
        let data = t.data.lock().unwrap();
        assert_eq!(metrics(&cs[0], &data, &t.config, now).usage, 0.);
        assert_eq!(
            placement_evidence(&cs[0], &data, now).0,
            PlacementEvidence::Fresh
        );
        drop(data);
        assert_eq!(
            place_at(
                &t,
                "claude:after-observation",
                &cs,
                now + chrono::Duration::seconds(21)
            )
            .to,
            "cl-a"
        );
    }
}

#[test]
fn d20_never_observed_accounts_round_robin_and_caps_never_refuse() {
    let (_root, t, mut cs, now) = placement_fixture();
    for c in &mut cs {
        c.quota = None;
    }
    for n in 0..10 {
        let event = place_at(
            &t,
            &format!("claude:rr{n}"),
            &cs,
            now + chrono::Duration::seconds(n),
        );
        assert_eq!(event.to, if n % 2 == 0 { "cl-a" } else { "cl-b" });
        assert_eq!(event.evidence, PlacementEvidence::Weak);
    }
}

#[test]
fn d20_old_request_and_receive_timestamp_cannot_clear_a_later_placement() {
    let (_root, t, mut cs, now) = placement_fixture();
    let sent_before = t.begin_usage_observation_for("cl-a");
    place_at(&t, "claude:late", &cs[..1], now);
    observed_at(&mut cs[..1], now + chrono::Duration::days(1));
    t.note_usage_observation_asof_token("cl-a", cs[0].quota.as_ref().unwrap(), sent_before);
    let data = t.data.lock().unwrap();
    assert_eq!(metrics(&cs[0], &data, &t.config, now).usage, 2.);
    assert_eq!(
        placement_evidence(&cs[0], &data, now),
        (PlacementEvidence::Weak, 1)
    );
    drop(data);
    // Even an arbitrarily newer merged candidate cannot invent send provenance.
    t.note_usage_observation("cl-a", cs[0].quota.as_ref().unwrap());
    assert_eq!(t.data.lock().unwrap().placements["cl-a"].count(), 1);
    fresh_observation(&t, &mut cs[..1], now - chrono::Duration::days(1));
    assert_eq!(
        t.data.lock().unwrap().placements["cl-a"].count(),
        0,
        "wall-clock rollback cannot defeat a later monotonic send token"
    );
}

#[test]
fn d20_partial_observation_and_equal_sequence_preserve_window_charge() {
    let (_root, t, cs, now) = placement_fixture();
    place_at(&t, "claude:equal", &cs[..1], now);
    let seq = t.data.lock().unwrap().placements["cl-a"].latest.unwrap();
    t.note_usage_observation_asof("cl-a", cs[0].quota.as_ref().unwrap(), seq);
    let data = t.data.lock().unwrap();
    assert_eq!(
        placement_evidence(&cs[0], &data, now),
        (PlacementEvidence::Fresh, 0)
    );
    assert_eq!(metrics(&cs[0], &data, &t.config, now).usage, 2.);
    drop(data);
    assert_eq!(
        t.due_placement_polls(now + chrono::Duration::seconds(120))
            .len(),
        1,
        "poll checks strictly newer, not the weak flag"
    );
    let newer = t.begin_usage_observation();
    let short_only =
        serde_json::json!({"windows":[cs[0].quota.as_ref().unwrap()["windows"][0].clone()]});
    t.note_usage_observation_asof("cl-a", &short_only, newer);
    assert_eq!(
        t.due_placement_polls(now + chrono::Duration::seconds(120))
            .len(),
        1
    );
    t.note_usage_observation_asof("cl-a", cs[0].quota.as_ref().unwrap(), newer);
    assert!(
        t.due_placement_polls(now + chrono::Duration::seconds(120))
            .is_empty()
    );
}

#[test]
fn d20_equal_sequence_charge_survives_next_placement_and_same_proof_replay() {
    let (_root, t, cs, now) = placement_fixture();
    place_at(&t, "claude:equal-first", &cs[..1], now);
    let first = t.data.lock().unwrap().placements["cl-a"].latest.unwrap();
    assert_eq!(first, 1);
    t.note_usage_observation_asof("cl-a", cs[0].quota.as_ref().unwrap(), first);
    assert_eq!(
        metrics(&cs[0], &t.data.lock().unwrap(), &t.config, now).usage,
        2.
    );

    place_at(&t, "claude:equal-second", &cs[..1], now);
    {
        let data = t.data.lock().unwrap();
        assert_eq!(data.placements["cl-a"].latest, Some(2));
        assert_eq!(
            data.placements["cl-a"].count(),
            1,
            "cap count excludes the equal first placement"
        );
        assert_eq!(data.placements["cl-a"].charge_count("5h"), 2);
        assert_eq!(data.placements["cl-a"].charge_count("7d"), 2);
        assert_eq!(
            metrics(&cs[0], &data, &t.config, now).usage,
            4.,
            "both placements remain charged because sequence 1 covers neither strictly"
        );
    }
    t.note_usage_observation_asof("cl-a", cs[0].quota.as_ref().unwrap(), first);
    assert_eq!(
        metrics(&cs[0], &t.data.lock().unwrap(), &t.config, now).usage,
        4.,
        "replaying the old proof cannot lose its equality state or change its captured count"
    );

    let newer = t.begin_usage_observation_for("cl-a");
    assert!(newer.sequence > 2);
    let short_only =
        serde_json::json!({"windows":[cs[0].quota.as_ref().unwrap()["windows"][0].clone()]});
    t.note_usage_observation_asof_token("cl-a", &short_only, newer);
    {
        let data = t.data.lock().unwrap();
        assert_eq!(data.placements["cl-a"].charge_count("5h"), 0);
        assert_eq!(data.placements["cl-a"].charge_count("7d"), 2);
        assert_eq!(metrics(&cs[0], &data, &t.config, now).usage, 4.);
    }
    t.note_usage_observation_asof_token("cl-a", cs[0].quota.as_ref().unwrap(), newer);
    let data = t.data.lock().unwrap();
    assert_eq!(data.placements["cl-a"].charge_count("7d"), 0);
    assert_eq!(metrics(&cs[0], &data, &t.config, now).usage, 0.);
}

#[test]
fn d20_room_and_pacing_precede_the_cap() {
    let (_root, t, mut cs, now) = placement_fixture();
    place_at(&t, "claude:a", &cs[..1], now);
    place_at(&t, "claude:b", &cs[1..], now);
    fresh_observation(&t, &mut cs[1..], now);
    t.set_account_threshold("cl-b", Some(1));
    assert_eq!(
        place_at(&t, "claude:room", &cs, now).to,
        "cl-a",
        "capped A has room; fresh B is full"
    );
    t.set_account_threshold("cl-b", Some(100));
    t.set_pacing("cl-b", true);
    assert_eq!(
        place_at(&t, "claude:pacing", &cs, now).to,
        "cl-a",
        "pacing B is skipped before A's cap is considered"
    );
}

#[test]
fn d20_fresh_target_reserves_two_but_second_move_keeps_mark_until_observed() {
    let (_root, t, mut cs) = fixture();
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs[..2]), 2);
    let keys: Vec<_> = t
        .data
        .lock()
        .unwrap()
        .overload_marks
        .keys()
        .cloned()
        .collect();
    assert_eq!(place(&t, &keys[0], &cs[..2], 100), cs[1].account.id);
    assert_eq!(place(&t, &keys[1], &cs[..2], 100), cs[0].account.id);
    assert!(
        t.overload_marked(Some(&keys[1]), Utc::now()),
        "capped destination preserves the mark"
    );
    fresh_observation(&t, &mut cs[1..2], Utc::now());
    assert_eq!(place(&t, &keys[1], &cs[..2], 100), cs[1].account.id);
}

#[test]
fn d20_weak_target_reserves_and_receives_at_most_one_group() {
    let (_root, t, mut cs) = fixture();
    cs[1].quota.as_mut().unwrap()["windows"][0]["reset_at"] = serde_json::json!(Utc::now());
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs[..2]), 1);
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
    assert_eq!(trigger(&t, &cs[0].account.id, "a", &cs[..2]), 0);
}

#[test]
fn d20_charge_cannot_cool_or_trigger_a_cold_point_move() {
    let (_root, t, cs, now) = placement_fixture();
    place_at(&t, "claude:bound", &cs[..1], now);
    {
        let mut data = t.data.lock().unwrap();
        for _ in 0..100 {
            data.placements
                .get_mut("cl-a")
                .unwrap()
                .placed(now, t.begin_usage_observation());
        }
    }
    {
        let mut data = t.data.lock().unwrap();
        data.bindings.get_mut("claude:bound").unwrap().last_used =
            now - chrono::Duration::seconds(3601);
        rebuild_indexes(&mut data);
    }
    let data = t.data.lock().unwrap();
    assert!(!metrics(&cs[0], &data, &t.config, now).gate);
    assert!(metrics_without_placement_charge(&cs[0], &data, &t.config, now).gate);
    assert!(effective_account(&cs[0], &data, &t.config, None, now).eligible_at(None, now));
    drop(data);
    let (account, moves) = decide_at(&t, "claude:bound", &cs, now);
    assert_eq!(account, "cl-a");
    assert!(moves.is_empty());
}

#[test]
fn d20_cold_move_waits_without_charge_until_every_target_window_is_newer() {
    let (_root, t, mut cs, now) = placement_fixture();
    let source_key = "claude:cold-source";
    place_at(&t, source_key, &cs[..1], now);
    place_at(&t, "claude:target-seed", &cs[1..], now);
    cs[0].account.drain = true;
    let make_source_cold = || {
        let mut data = t.data.lock().unwrap();
        data.bindings.get_mut(source_key).unwrap().last_used =
            now - chrono::Duration::seconds(3601);
        rebuild_indexes(&mut data);
    };
    {
        let data = t.data.lock().unwrap();
        assert!(t.rebalance_target_base(&data, "cl-a", &cs[1], Some(source_key), now));
        assert!(placement_capped(&cs[1], &data, &t.config, now));
        assert_eq!(data.placements["cl-b"].total, 1);
    }
    for partial in [false, true] {
        if partial {
            let asof = t.begin_usage_observation_for("cl-b");
            let short_only = serde_json::json!({"windows":[cs[1].quota.as_ref().unwrap()["windows"][0].clone()]});
            t.note_usage_observation_asof_token("cl-b", &short_only, asof);
        }
        make_source_cold();
        let sequence = t.usage_sequence.load(Ordering::Relaxed);
        let (account, moves) = decide_at(&t, source_key, &cs, now);
        assert_eq!(
            account, "cl-a",
            "capped target cannot receive a cold move, partial={partial}"
        );
        assert!(moves.is_empty());
        assert_eq!(
            t.usage_sequence.load(Ordering::Relaxed),
            sequence,
            "staying on the source is not a placement"
        );
        let data = t.data.lock().unwrap();
        assert_eq!(data.placements["cl-a"].total, 1);
        assert_eq!(
            data.placements["cl-b"].total, 1,
            "blocked cold moves add no charge"
        );
        assert_eq!(data.placements["cl-b"].charge_count("7d"), 1);
    }
    fresh_observation(&t, &mut cs[1..], now);
    make_source_cold();
    let (account, moves) = decide_at(&t, source_key, &cs, now);
    assert_eq!(account, "cl-b");
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].reason, "cold_rebalance");
    assert!(moves[0].is_placement);
    assert_eq!(moves[0].placement_charge_pct, 2.);
    assert_eq!(t.data.lock().unwrap().placements["cl-b"].total, 2);
}

#[test]
fn round2_debug_pin_is_exempt_but_new_and_unmatched_pin_recovery_take_charge() {
    let (_root, t, mut cs, now) = placement_fixture();
    let new = place_at(&t, "claude:normal", &cs[..1], now);
    assert!(new.is_placement);
    assert_eq!(new.placement_charge_pct, 2.);
    let before_pin = t.usage_sequence.load(Ordering::Relaxed);
    let pinned_choice = |key: &str, cs: &[Candidate]| match t.choose_with_hold_at(
        Provider::Claude,
        Some(key),
        None,
        None,
        cs,
        Some("cl-b"),
        100,
        true,
        false,
        now,
    ) {
        Decision::Selected {
            account,
            lease,
            mut moves,
        } => {
            lease.dispatched();
            drop(lease);
            assert_eq!(moves.len(), 1);
            (account, moves.remove(0))
        }
        other => panic!("expected selection: {other:?}"),
    };
    // Cover both a pinned first binding and a pin overriding an existing one.
    for key in ["claude:pinned-new", "claude:normal"] {
        let (account, event) = pinned_choice(key, &cs);
        assert_eq!(account, "cl-b");
        assert!(event.debug_pin);
        assert!(!event.is_placement);
        assert_eq!(event.placement_charge_pct, 0.);
        assert_eq!(t.usage_sequence.load(Ordering::Relaxed), before_pin);
        let data = t.data.lock().unwrap();
        assert_eq!(data.placements["cl-a"].total, 1);
        assert_eq!(data.placements["cl-b"].total, 0);
    }
    // A pin naming an ineligible candidate must not exempt real C5 recovery.
    cs[1].account.user_state = UserState::Paused;
    let (account, event) = pinned_choice("claude:normal", &cs);
    assert_eq!(account, "cl-a");
    assert_eq!(event.reason, "forced");
    assert!(!event.debug_pin);
    assert!(event.is_placement);
    assert_eq!(event.placement_charge_pct, 2.);
    assert_eq!(t.usage_sequence.load(Ordering::Relaxed), before_pin + 1);
    assert_eq!(t.data.lock().unwrap().placements["cl-a"].total, 2);
}

#[test]
fn d20_forced_move_adds_placement_but_inheritance_and_reuse_do_not() {
    let (_root, t, mut cs, now) = placement_fixture();
    place_at(&t, "claude:root", &cs[..1], now);
    let before = t.usage_sequence.load(Ordering::Relaxed);
    assert!(decide_at(&t, "claude:root", &cs, now).1.is_empty());
    cs[0].account.user_state = UserState::Paused;
    let event = place_at(&t, "claude:root", &cs, now);
    assert!(event.is_placement);
    assert_eq!(event.reason, "forced");
    assert_eq!(event.placement_charge_pct, 2.);
    let after_forced = t.usage_sequence.load(Ordering::Relaxed);
    assert!(after_forced > before);
    assert_eq!(t.data.lock().unwrap().placements[&event.to].total, 1);
    match t.choose_with_hold_at(
        Provider::Claude,
        Some("claude:child"),
        Some("claude:root"),
        None,
        &cs,
        None,
        100,
        true,
        false,
        now,
    ) {
        Decision::Selected { moves, .. } => {
            assert!(moves.iter().all(|m| !m.is_placement));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(t.usage_sequence.load(Ordering::Relaxed), after_forced);
}

#[test]
fn d20_exact_cumulative_counters_are_bounded_across_delayed_partial_observations() {
    let now = Utc::now();
    let mut state = PlacementState::default();
    for n in 0..100_000u64 {
        state.placed(now, n * 2 + 1);
    }
    let midway = UsageObservationAsOf {
        sequence: 100_000,
        placed_count: 50_000,
    };
    state.observe_asof(
        &serde_json::json!({"windows":[{"name":"5h","observed_at":now,
        "used_pct":10,"reset_at":now + chrono::Duration::hours(1)}]}),
        midway,
    );
    assert_eq!(state.count(), 50_000);
    assert_eq!(state.charge_count("5h"), 50_000);
    assert_eq!(
        state.windows.len(),
        1,
        "state has no per-placement allocation"
    );
    state.observe_asof(
        &serde_json::json!({"windows":[{"name":"5h","observed_at":now,
        "used_pct":10,"reset_at":now + chrono::Duration::hours(1)}]}),
        UsageObservationAsOf {
            sequence: 200_001,
            placed_count: 100_000,
        },
    );
    assert_eq!(state.count(), 0);
    assert_eq!(state.charge_count("5h"), 0);
}

#[test]
fn d20_poll_deadline_dedup_and_unknown_cost_drain() {
    let (_root, t, cs, now) = placement_fixture();
    place_at(&t, "claude:first", &cs[..1], now);
    let first = t.data.lock().unwrap().placements["cl-a"].latest.unwrap();
    place_at(
        &t,
        "claude:second",
        &cs[..1],
        now + chrono::Duration::seconds(100),
    );
    assert!(
        t.due_placement_polls(now + chrono::Duration::seconds(119))
            .is_empty()
    );
    assert_eq!(
        t.due_placement_polls(now + chrono::Duration::seconds(120))
            .len(),
        1
    );
    t.note_placement_poll_started("cl-a", first);
    assert!(
        t.due_placement_poll("cl-a", now + chrono::Duration::seconds(219), Some(first))
            .is_none()
    );
    assert!(
        t.due_placement_poll("cl-a", now + chrono::Duration::seconds(220), Some(first))
            .is_some()
    );
    let (_root, t, cs) = fixture();
    t.unknown_request_cost(Some("claude:a0"));
    trigger(&t, &cs[0].account.id, "a", &cs[..2]);
    assert!(t.take_unknown_cost_skips() > 0);
    assert_eq!(t.take_unknown_cost_skips(), 0);
}

#[test]
fn d20_delayed_partial_tokens_keep_exact_independent_window_counts() {
    let now = Utc::now();
    let mut state = PlacementState::default();
    state.observe(&serde_json::json!({"windows":[
        {"name":"5h","observed_at":now,"reset_at":now + chrono::Duration::hours(1)},
        {"name":"7d","observed_at":now,"reset_at":now + chrono::Duration::hours(24)}]}));
    for n in 0..100_000u64 {
        state.placed(now, n * 2 + 1);
    }
    state.observe_asof(
        &serde_json::json!({"windows":[{"name":"5h","observed_at":now}]}),
        UsageObservationAsOf {
            sequence: 100_000,
            placed_count: 50_000,
        },
    );
    assert_eq!(state.charge_count("5h"), 50_000);
    assert_eq!(state.charge_count("7d"), 100_000);
    assert_eq!(state.count(), 100_000);
    state.observe_asof(
        &serde_json::json!({"windows":[{"name":"7d","observed_at":now}]}),
        UsageObservationAsOf {
            sequence: 150_000,
            placed_count: 75_000,
        },
    );
    assert_eq!(state.charge_count("7d"), 25_000);
    assert_eq!(state.count(), 50_000);
    // An older delayed response cannot regress either counter.
    state.observe_asof(
        &serde_json::json!({"windows":[{"name":"5h","observed_at":now}]}),
        UsageObservationAsOf {
            sequence: 10_000,
            placed_count: 5_000,
        },
    );
    assert_eq!(state.count(), 50_000);
    assert_eq!(state.windows.len(), 2);
}

#[test]
fn d20_partial_ack_keeps_t20_deadline_at_t140_not_t220() {
    let t0 = Utc::now();
    let mut state = PlacementState::default();
    state.placed(t0, 1);
    let asof_between_t0_and_t20 = UsageObservationAsOf {
        sequence: 2,
        placed_count: 1,
    };
    state.placed(t0 + chrono::Duration::seconds(20), 3);
    state.placed(t0 + chrono::Duration::seconds(100), 4);
    state.observe_asof(
        &serde_json::json!({"windows":[
            {"name":"5h","observed_at":t0 + chrono::Duration::seconds(110)},
            {"name":"7d","observed_at":t0 + chrono::Duration::seconds(110)}]}),
        asof_between_t0_and_t20,
    );
    assert_eq!(state.count(), 2);
    assert_eq!(state.charge_count("5h"), 2);
    assert!(
        placement_poll(
            "cl-a",
            &state,
            t0 + chrono::Duration::seconds(139),
            None,
            120
        )
        .is_none()
    );
    let due = placement_poll(
        "cl-a",
        &state,
        t0 + chrono::Duration::seconds(140),
        None,
        120,
    )
    .unwrap();
    assert_eq!(due.due_at, t0 + chrono::Duration::seconds(140));
}

#[test]
fn d20_bounded_poll_deadlines_cannot_slide_under_repeated_partial_ack() {
    let t0 = Utc::now();
    let mut state = PlacementState::default();
    for n in 0..200u64 {
        state.placed(t0 + chrono::Duration::seconds(n as i64), n * 2 + 1);
    }
    assert!(state.poll_deadlines.len() <= PLACEMENT_POLL_DEADLINES);
    let received = t0 + chrono::Duration::seconds(1000);
    for covered in [1, 50, 100, 130, 150, 199] {
        state.observe_asof(
            &serde_json::json!({"windows":[
                {"name":"5h","observed_at":received},
                {"name":"7d","observed_at":received}]}),
            UsageObservationAsOf {
                sequence: covered * 2,
                placed_count: covered,
            },
        );
        assert_eq!(state.count(), (200 - covered) as usize);
        let due = placement_poll("cl-a", &state, received, None, 120).unwrap();
        assert!(
            due.due_at <= t0 + chrono::Duration::seconds(covered as i64 + 120),
            "coalescing may poll early but must not defer the oldest uncovered placement"
        );
        assert!(state.poll_deadlines.len() <= PLACEMENT_POLL_DEADLINES);
    }
    state.observe_asof(
        &serde_json::json!({"windows":[
            {"name":"5h","observed_at":received},
            {"name":"7d","observed_at":received}]}),
        UsageObservationAsOf {
            sequence: 400,
            placed_count: 200,
        },
    );
    assert!(state.poll_deadlines.is_empty());
    assert_eq!(state.count(), 0);
}

#[test]
fn d20_positive_duration_account_windows_exclude_subbuckets_and_placeholders() {
    for name in [
        "5h",
        "7d",
        "primary",
        "secondary",
        "43200m",
        "60m",
        "90s",
        "30d",
    ] {
        assert!(
            placement_account_window(&serde_json::json!({"name":name})),
            "{name}"
        );
    }
    for name in [
        "0m",
        "0h",
        "0d",
        "0s",
        "7d_oi",
        "7d_sonnet",
        "model:43200m",
        "+30m",
        "1e3m",
        "",
    ] {
        assert!(
            !placement_account_window(&serde_json::json!({"name":name})),
            "{name}"
        );
    }
    assert_eq!(
        account_window(Provider::Codex, &serde_json::json!({"name":"43200m"})),
        Some(true),
        "C7 maps nonstandard Codex windows over one day to the weekly slot"
    );
}

#[test]
fn d20_fresh_codex_30day_account_resets_and_clears_placement_charge() {
    let (_root, t, mut cs, now) = placement_fixture();
    let mut free = cs.remove(0);
    free.account.id = "cx-free".into();
    free.account.provider = Provider::Codex;
    free.account.identity = crate::accounts::Identity::Codex {
        account_id: "synthetic-free".into(),
        email: "free@example.invalid".into(),
        user_id: None,
    };
    free.quota = Some(
        serde_json::json!({"plan":"free","observed_at":now,"windows":[
            {"name":"43200m","used_pct":0.,"observed_at":now,"reset_at":now + chrono::Duration::days(30)},
            {"name":"0m","used_pct":0.,"observed_at":null},
            {"name":"7d_oi","used_pct":100.,"observed_at":null}
        ]}),
    );
    let asof = t.begin_usage_observation_for("cx-free");
    t.note_usage_observation_asof_token("cx-free", free.quota.as_ref().unwrap(), asof);
    assert_eq!(
        placement_evidence(&free, &t.data.lock().unwrap(), now),
        (PlacementEvidence::Fresh, 0)
    );
    for (key, at) in [
        ("codex:free-initial", now),
        (
            "codex:free-after-reset",
            now + chrono::Duration::days(30) + chrono::Duration::seconds(1),
        ),
    ] {
        if key == "codex:free-after-reset" {
            assert_eq!(
                placement_evidence(&free, &t.data.lock().unwrap(), at),
                (PlacementEvidence::Weak, 0),
                "a passed 30-day reset weakens evidence even before another placement"
            );
        }
        let Decision::Selected { lease, moves, .. } = t.choose_with_hold_at(
            Provider::Codex,
            Some(key),
            None,
            None,
            std::slice::from_ref(&free),
            None,
            100,
            true,
            false,
            at,
        ) else {
            panic!("free account must accept placement");
        };
        lease.dispatched();
        drop(lease);
        assert!(moves[0].is_placement);
        let data = t.data.lock().unwrap();
        let state = &data.placements["cx-free"];
        assert_eq!(
            state.windows.len(),
            1,
            "sub-buckets/0m cannot create missing evidence"
        );
        assert_eq!(state.charge_count("43200m"), 1);
        assert_eq!(
            placement_evidence(&free, &data, at),
            (PlacementEvidence::Weak, 1)
        );
        drop(data);
        let observed = at + chrono::Duration::seconds(1);
        let quota = free.quota.as_mut().unwrap();
        quota["observed_at"] = serde_json::json!(observed);
        quota["windows"][0]["observed_at"] = serde_json::json!(observed);
        quota["windows"][0]["reset_at"] = serde_json::json!(observed + chrono::Duration::days(30));
        let asof = t.begin_usage_observation_for("cx-free");
        t.note_usage_observation_asof_token("cx-free", quota, asof);
        let data = t.data.lock().unwrap();
        assert_eq!(data.placements["cx-free"].charge_count("43200m"), 0);
        assert_eq!(
            placement_evidence(&free, &data, observed),
            (PlacementEvidence::Fresh, 0)
        );
        drop(data);
        assert!(
            t.due_placement_poll("cx-free", observed + chrono::Duration::seconds(120), None)
                .is_none()
        );
    }
}

#[test]
fn d20_complete_poll_drops_old_plan_windows_but_partial_headers_preserve_them() {
    let now = Utc::now();
    let old_plan = serde_json::json!({"windows":[
        {"name":"5h","used_pct":10,"observed_at":now,"reset_at":now + chrono::Duration::hours(5)},
        {"name":"7d","used_pct":20,"observed_at":now,"reset_at":now + chrono::Duration::days(7)}]});
    let free_plan = serde_json::json!({"windows":[
        {"name":"43200m","used_pct":0,"observed_at":now,"reset_at":now + chrono::Duration::days(30)}]});
    let mut base = PlacementState::default();
    base.observe_asof(
        &old_plan,
        UsageObservationAsOf {
            sequence: 1,
            placed_count: 0,
        },
    );
    base.placed(now, 2);
    let token = UsageObservationAsOf {
        sequence: 3,
        placed_count: 1,
    };
    let mut partial = base.clone();
    partial.observe_asof(&free_plan, token);
    assert_eq!(partial.windows.len(), 3);
    assert!(partial.weak(now));
    assert_eq!(
        partial.count(),
        1,
        "unreported old windows are still awaiting evidence"
    );
    let mut complete = base;
    complete.observe_complete_asof(&free_plan, token);
    assert_eq!(
        complete
            .windows
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["43200m"]
    );
    assert!(!complete.weak(now));
    assert_eq!(complete.count(), 0);
    assert_eq!(complete.charge_count("43200m"), 0);
    assert!(complete.poll_deadlines.is_empty());
}

#[test]
fn d20_late_complete_poll_preserves_newer_window_and_cannot_restore_removed_plan() {
    let now = Utc::now();
    let old_plan = serde_json::json!({"windows":[
        {"name":"5h","used_pct":10,"observed_at":now},
        {"name":"7d","used_pct":20,"observed_at":now}]});
    let free_plan = serde_json::json!({"windows":[
        {"name":"43200m","used_pct":1,"observed_at":now,"reset_at":now + chrono::Duration::days(30)}]});
    let mut state = PlacementState::default();
    state.observe_asof(
        &old_plan,
        UsageObservationAsOf {
            sequence: 1,
            placed_count: 0,
        },
    );
    state.placed(now, 2);
    state.observe_asof(
        &serde_json::json!({"windows":[{"name":"7d","used_pct":75,"observed_at":now}]}),
        UsageObservationAsOf {
            sequence: 5,
            placed_count: 1,
        },
    );
    state.observe_complete_asof(
        &free_plan,
        UsageObservationAsOf {
            sequence: 3,
            placed_count: 1,
        },
    );
    assert!(!state.windows.contains_key("5h"));
    assert_eq!(state.windows["7d"].value["used_pct"], 75);
    assert_eq!(
        state.windows["7d"].asof.unwrap().sequence,
        5,
        "the late complete poll cannot delete newer per-window evidence"
    );
    state.observe_complete_asof(
        &free_plan,
        UsageObservationAsOf {
            sequence: 7,
            placed_count: 1,
        },
    );
    assert_eq!(state.windows.len(), 1);
    state.observe_complete_asof(
        &old_plan,
        UsageObservationAsOf {
            sequence: 3,
            placed_count: 1,
        },
    );
    state.observe_asof(
        &old_plan,
        UsageObservationAsOf {
            sequence: 5,
            placed_count: 1,
        },
    );
    state.observe(&old_plan);
    assert_eq!(
        state.windows.keys().map(String::as_str).collect::<Vec<_>>(),
        ["43200m"]
    );
    assert_eq!(state.windows["43200m"].asof.unwrap().sequence, 7);
    assert!(!state.weak(now));
    assert_eq!(state.count(), 0);
}

#[test]
fn d20_complete_poll_keeps_newer_unknown_window_without_inventing_observation() {
    let now = Utc::now();
    let mut state = PlacementState::default();
    state.observe_asof(
        &serde_json::json!({"windows":[{"name":"90s","observed_at":null,"used_pct":null}]}),
        UsageObservationAsOf {
            sequence: 5,
            placed_count: 0,
        },
    );
    state.observe_complete_asof(
        &serde_json::json!({"windows":[{"name":"43200m","observed_at":now,"used_pct":0}]}),
        UsageObservationAsOf {
            sequence: 3,
            placed_count: 0,
        },
    );
    assert!(state.windows.contains_key("90s"));
    assert!(state.windows["90s"].asof.is_none());
    assert!(state.weak(now));
}

#[test]
fn d20_passing_gate_beats_fresh_uncapped_non_gated_account_in_both_room_branches() {
    for full in [false, true] {
        for pacing in [false, true] {
            let (_root, t, mut cs, now) = placement_fixture();
            for w in cs[1].quota.as_mut().unwrap()["windows"]
                .as_array_mut()
                .unwrap()
            {
                w["used_pct"] = serde_json::json!(91.);
            }
            place_at(&t, "claude:gate-warm", &cs[..1], now);
            if full {
                t.set_account_threshold("cl-a", Some(1));
            }
            t.set_pacing("cl-a", pacing);
            let data = t.data.lock().unwrap();
            assert!(metrics(&cs[0], &data, &t.config, now).gate);
            assert!(!metrics(&cs[1], &data, &t.config, now).gate);
            assert!(placement_capped(&cs[0], &data, &t.config, now));
            assert!(!placement_capped(&cs[1], &data, &t.config, now));
            drop(data);
            assert_eq!(
                place_at(&t, "claude:gate-choice", &cs, now).to,
                "cl-a",
                "step 4 must beat non-gated B even with full={full}, pacing={pacing}"
            );
        }
    }
}

#[test]
fn d20_all_gates_failed_uses_step_five_cap_order_then_headroom() {
    let (_root, t, mut cs, now) = placement_fixture();
    for (candidate, used) in cs.iter_mut().zip([91., 96.]) {
        for w in candidate.quota.as_mut().unwrap()["windows"]
            .as_array_mut()
            .unwrap()
        {
            w["used_pct"] = serde_json::json!(used);
        }
    }
    for n in 0..3 {
        place_at(&t, &format!("claude:fallback-a{n}"), &cs[..1], now);
    }
    for n in 0..2 {
        place_at(&t, &format!("claude:fallback-b{n}"), &cs[1..], now);
    }
    let data = t.data.lock().unwrap();
    let a = metrics(&cs[0], &data, &t.config, now);
    let b = metrics(&cs[1], &data, &t.config, now);
    assert!(!a.gate && !b.gate);
    assert_eq!((a.usage, b.usage), (97., 100.));
    assert_eq!(placement_evidence(&cs[0], &data, now).1, 3);
    assert_eq!(placement_evidence(&cs[1], &data, now).1, 2);
    drop(data);
    assert_eq!(
        place_at(&t, "claude:fallback-choice", &cs, now).to,
        "cl-b",
        "only the all-gates-failed fallback may prefer the lower placement count here"
    );
}

#[test]
fn round2_default_limits_move_both_marked_groups_with_fresh_target_in_first_window() {
    let root = fixture_dir();
    let t = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let now = Utc::now();
    let mut cs = vec![candidate("cl-a", 10., 8, 1), candidate("cl-b", 10., 30, 2)];
    observed_at(&mut cs, now);
    for n in 0..8 {
        place_at(&t, &format!("claude:source{n}"), &cs[..1], now);
    }
    let mut marked = 0;
    for n in 0..3 {
        marked += t
            .note_client_rate(
                Provider::Claude,
                "cl-a",
                Some(&format!("claude:source{n}")),
                &cs,
                now,
            )
            .3;
    }
    assert_eq!(marked, 2);
    let keys: Vec<_> = t
        .data
        .lock()
        .unwrap()
        .overload_marks
        .keys()
        .cloned()
        .collect();
    let first = place_at(&t, &keys[0], &cs, now + chrono::Duration::seconds(1));
    assert_eq!(first.to, "cl-b");
    assert_eq!(first.reason, "overload");
    assert!(placement_capped(
        &cs[1],
        &t.data.lock().unwrap(),
        &t.config,
        now
    ));
    // The second mark waits for evidence, not an arrival/rate quiet timer.
    t.note_rate("cl-b", now + chrono::Duration::seconds(1));
    t.set_pacing("cl-b", true);
    let (account, moves) = decide_at(&t, &keys[1], &cs, now + chrono::Duration::seconds(2));
    assert_eq!(account, "cl-a");
    assert!(moves.is_empty());
    assert!(t.overload_marked(Some(&keys[1]), now + chrono::Duration::seconds(2)));
    let ready = now + chrono::Duration::seconds(3);
    fresh_observation(&t, &mut cs[1..], ready);
    let second = place_at(&t, &keys[1], &cs, ready);
    assert_eq!(second.to, "cl-b");
    assert_eq!(second.reason, "overload");
    assert!(second.is_placement);
    assert_eq!(second.placement_charge_pct, 2.);
    assert!(t.data.lock().unwrap().overload_marks.is_empty());
    assert_eq!(t.data.lock().unwrap().placements["cl-b"].total, 2);
    assert_eq!(t.active_sessions("cl-a", ready), 6);
    assert_eq!(t.active_sessions("cl-b", ready), 2);
}

#[test]
fn round2_forced_burst_takes_charge_and_cap_but_never_refuses() {
    for targets in [1usize, 2] {
        let (_root, t, mut cs, now) = placement_fixture();
        cs.push(candidate("cl-c", 30., 40, 3));
        observed_at(&mut cs, now);
        for n in 0..10 {
            place_at(&t, &format!("claude:forced{n}"), &cs[..1], now);
        }
        cs[0].account.user_state = UserState::Paused;
        let mut counts = [0usize; 2];
        let before = t.usage_sequence.load(Ordering::Relaxed);
        for n in 0..10 {
            let at = now + chrono::Duration::seconds(n + 1);
            let event = place_at(&t, &format!("claude:forced{n}"), &cs[..=targets], at);
            assert_eq!(event.reason, "forced");
            assert_eq!(event.from.as_deref(), Some("cl-a"));
            assert!(event.is_placement);
            assert_eq!(event.placement_charge_pct, 2.);
            let index = if event.to == "cl-b" {
                0
            } else {
                assert_eq!(event.to, "cl-c");
                1
            };
            if n >= targets as i64 {
                assert!(
                    event.capped,
                    "all-capped recovery still must select a target"
                );
            }
            counts[index] += 1;
            if targets == 2 {
                assert!(
                    counts[0].abs_diff(counts[1]) <= 1,
                    "forced target choice must obey weak-evidence round-robin"
                );
            }
        }
        assert_eq!(counts, if targets == 2 { [5, 5] } else { [10, 0] });
        assert_eq!(t.usage_sequence.load(Ordering::Relaxed), before + 10);
        let data = t.data.lock().unwrap();
        for (index, c) in cs[1..=targets].iter().enumerate() {
            assert_eq!(data.placements[&c.account.id].total, counts[index] as u64);
            assert_eq!(
                data.placements[&c.account.id].charge_count("7d"),
                counts[index]
            );
            assert_eq!(
                metrics(c, &data, &t.config, now).usage,
                30. + 2. * counts[index] as f64
            );
            assert_eq!(
                effective_account(c, &data, &t.config, None, now).health,
                Health::Ready
            );
        }
    }
}

#[test]
fn round2_any_discretionary_guard_veto_keeps_mark_on_healthy_source() {
    for guard in [
        "dwell", "revisit", "group", "pool", "cost", "unknown", "recover", "strict",
    ] {
        let root = fixture_dir();
        let mut cfg = RoutingConfig {
            spread_threshold: 2,
            ..RoutingConfig::default()
        };
        // Isolate group budget from dwell, which would independently veto it.
        if guard == "group" {
            cfg.churn.dwell_secs = 0;
            cfg.churn.max_group_moves = 1;
        }
        let t = BindingTable::new(root.path(), cfg).unwrap();
        let now = Utc::now();
        let cs = vec![candidate("cl-a", 10., 8, 1), candidate("cl-b", 10., 30, 2)];
        for n in 0..6 {
            place_at(&t, &format!("claude:guard{n}"), &cs[..1], now);
        }
        for n in 0..3 {
            t.note_client_rate(
                Provider::Claude,
                "cl-a",
                Some(&format!("claude:guard{n}")),
                &cs,
                now,
            );
        }
        let key = t
            .data
            .lock()
            .unwrap()
            .overload_marks
            .keys()
            .next()
            .unwrap()
            .clone();
        match guard {
            "dwell" | "group" => t
                .policies
                .dispatch(Some(&key), "cl-a", Some("cl-other"), "overload", now)
                .unwrap(),
            "revisit" => t
                .policies
                .dispatch(Some(&key), "cl-a", Some("cl-b"), "forced", now)
                .unwrap(),
            "pool" => {
                for n in 0..4 {
                    t.policies
                        .dispatch(
                            Some(&format!("claude:pool{n}")),
                            &format!("cl-other{n}"),
                            Some("cl-before"),
                            "overload",
                            now,
                        )
                        .unwrap();
                }
            }
            "cost" => t.request_bytes(Some(&key), t.config.churn.max_move_bytes + 1),
            "unknown" => t.unknown_request_cost(Some(&key)),
            "recover" | "strict" => t
                .policies
                .set_session(
                    &crate::routing::session_id(&key),
                    Some(crate::routing::Settings {
                        mode: if guard == "strict" {
                            crate::routing::Mode::Strict
                        } else {
                            crate::routing::Mode::Recover
                        },
                        reset_wait_secs: 60,
                    }),
                    None,
                    Some("cl-a"),
                )
                .unwrap(),
            _ => unreachable!(),
        }
        let sequence = t.usage_sequence.load(Ordering::Relaxed);
        for seconds in [1, 2] {
            let at = now + chrono::Duration::seconds(seconds);
            let (id, moves) = decide_at(&t, &key, &cs, at);
            assert_eq!(id, "cl-a", "{guard}");
            assert!(moves.is_empty(), "{guard}: veto is not a forced move");
            assert!(t.overload_marked(Some(&key), at), "{guard}: preserve mark");
            assert_eq!(t.usage_sequence.load(Ordering::Relaxed), sequence);
        }
    }
}
