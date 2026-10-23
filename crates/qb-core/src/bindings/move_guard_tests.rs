use super::tests::{candidate, fixture_dir};
use super::*;
use std::sync::atomic::AtomicUsize;

fn pick(
    table: &Arc<BindingTable>,
    key: &str,
    candidates: &[Candidate],
    pin: &str,
    now: DateTime<Utc>,
    admit: impl FnOnce(&str) -> bool,
) -> Decision {
    // This direct fixture models a current observation at each call.
    table.choose_with_hold_at_admitted_evidence(
        Provider::Claude,
        Some(key),
        None,
        None,
        candidates,
        candidates,
        MoveObservations::same(MoveObservation::before_collection()),
        Some(pin),
        100,
        false,
        false,
        now,
        None,
        admit,
    )
}
fn selected(decision: Decision) -> String {
    match decision {
        Decision::Selected { account, .. } => account,
        other => panic!("expected placement: {other:?}"),
    }
}

fn pick_snapshot(
    table: &Arc<BindingTable>,
    key: &str,
    candidates: &[Candidate],
    evidence: &[Candidate],
    observations: MoveObservations,
    now: DateTime<Utc>,
    calls: &AtomicUsize,
) -> Decision {
    table.choose_with_hold_at_admitted_evidence(
        Provider::Claude,
        Some(key),
        None,
        None,
        candidates,
        evidence,
        observations,
        Some(&candidates[0].account.id),
        100,
        false,
        false,
        now,
        None,
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            true
        },
    )
}

#[test]
fn r14_old_cooling_snapshot_before_arrival_cannot_authorize_another_move() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let now = Utc::now();
    let cs = [
        candidate("cl-111111", 10., 10, 2),
        candidate("cl-222222", 20., 20, 3),
        candidate("cl-333333", 30., 30, 4),
    ];
    let key = "claude:prearrival-source";
    selected(pick(&table, key, &cs, &cs[0].account.id, now, |_| true));
    let old_observed = MoveObservation::before_collection();
    let mut old = cs.clone();
    old[1].account.apply_health_signal(
        HealthSignal::Cooling {
            reason: HealthReason::Payment,
            until: now + chrono::Duration::hours(6),
            scope: "account".into(),
        },
        now,
    );
    // A different request observes B recovered Ready and commits A -> B.
    selected(pick(&table, key, &cs, &cs[1].account.id, now, |_| true));
    let calls = AtomicUsize::new(0);
    assert!(matches!(
        pick_snapshot(
            &table,
            key,
            &old[2..],
            &old,
            MoveObservations::same(old_observed),
            now,
            &calls
        ),
        Decision::MoveHeld { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(table.current(Some(key)), Some(cs[1].account.id.clone()));

    // A real new failure after arrival still qualifies at the default budget.
    let fresh_observed = MoveObservation::before_collection();
    let mut fresh = cs.clone();
    fresh[1].account.apply_health_signal(
        HealthSignal::Cooling {
            reason: HealthReason::Payment,
            until: now + chrono::Duration::hours(6),
            scope: "account".into(),
        },
        now,
    );
    assert_eq!(
        selected(pick_snapshot(
            &table,
            key,
            &fresh[2..],
            &fresh,
            MoveObservations::same(fresh_observed),
            now,
            &calls
        )),
        cs[2].account.id
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn r14_old_ready_destination_before_departure_cannot_prove_recovery() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let now = Utc::now();
    let mut cs = [
        candidate("cl-111111", 10., 10, 2),
        candidate("cl-222222", 20., 20, 3),
    ];
    let key = "claude:predeparture-target";
    selected(pick(&table, key, &cs, &cs[0].account.id, now, |_| true));
    let old_observed = MoveObservation::before_collection();
    let old_ready = [cs[0].clone()];
    cs[0].account.apply_health_signal(
        HealthSignal::Cooling {
            reason: HealthReason::Payment,
            until: now + chrono::Duration::hours(6),
            scope: "account".into(),
        },
        now,
    );
    selected(pick(&table, key, &cs, &cs[1].account.id, now, |_| true));
    // Isolate return protection after the ten-minute session budget expires.
    let later = now + chrono::Duration::minutes(11);
    let calls = AtomicUsize::new(0);
    let source_observed = MoveObservation::before_collection();
    assert!(matches!(
        pick_snapshot(
            &table,
            key,
            &old_ready,
            &cs,
            MoveObservations {
                source: source_observed,
                target: old_observed
            },
            later,
            &calls
        ),
        Decision::MoveHeld { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(table.current(Some(key)), Some(cs[1].account.id.clone()));

    let recovered_observed = MoveObservation::before_collection();
    cs[0].account.health = Health::Ready;
    cs[0].account.payment_until = None;
    cs[0].account.payment_failures = 0;
    let recovered_ready = [cs[0].clone()];
    assert_eq!(
        selected(pick_snapshot(
            &table,
            key,
            &recovered_ready,
            &cs,
            MoveObservations {
                source: source_observed,
                target: recovered_observed
            },
            later,
            &calls
        )),
        cs[0].account.id
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn r14_real_selection_checks_before_target_admission_and_preserves_affinity() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let now = Utc::now();
    let cs = [
        candidate("cl-111111", 10., 10, 2),
        candidate("cl-222222", 20., 20, 3),
        candidate("cl-333333", 30., 30, 4),
    ];
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    table.set_move_guard_observer(Arc::new(move |e| observed.lock().unwrap().push(e.clone())));
    let key = "claude:guard-test";
    assert_eq!(
        selected(pick(&table, key, &cs, &cs[0].account.id, now, |_| true)),
        cs[0].account.id
    );
    assert_eq!(
        selected(pick(&table, key, &cs, &cs[1].account.id, now, |_| true)),
        cs[1].account.id
    );
    // Child aliases share the root's successful move budget.
    assert_eq!(
        selected(pick(
            &table,
            "claude:guard-test:agent:child",
            &cs,
            &cs[2].account.id,
            now,
            |id| {
                assert_eq!(id, cs[1].account.id);
                true
            }
        )),
        cs[1].account.id
    );
    assert_eq!(
        table.current(Some(key)).as_deref(),
        Some(cs[1].account.id.as_str())
    );
    assert_eq!(
        events.lock().unwrap().last().unwrap().reason,
        "session_ten_minutes"
    );
    assert_eq!(
        selected(pick(&table, key, &cs, &cs[0].account.id, now, |id| {
            assert_eq!(id, cs[1].account.id);
            true
        })),
        cs[1].account.id
    );
    assert_eq!(events.lock().unwrap().last().unwrap().reason, "ping_pong");
}

#[test]
fn r14_failed_admission_never_consumes_budget_and_forced_refusal_never_admits() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let now = Utc::now();
    let cs = [
        candidate("cl-111111", 10., 10, 2),
        candidate("cl-222222", 20., 20, 3),
        candidate("cl-333333", 30., 30, 4),
    ];
    let key = "claude:admit-test";
    selected(pick(&table, key, &cs, &cs[0].account.id, now, |_| true));
    assert!(matches!(
        pick(&table, key, &cs, &cs[1].account.id, now, |_| false),
        Decision::None
    ));
    assert_eq!(
        selected(pick(&table, key, &cs, &cs[1].account.id, now, |_| true)),
        cs[1].account.id
    );
    let calls = AtomicUsize::new(0);
    // Source was excluded for this retry: absence cannot grant fresh evidence.
    assert!(matches!(
        pick(&table, key, &cs[2..], &cs[2].account.id, now, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            true
        }),
        Decision::MoveHeld { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        table.current(Some(key)).as_deref(),
        Some(cs[1].account.id.as_str())
    );
}

#[test]
fn r14_no_source_evidence_cannot_prove_changed_return_destination() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let now = Utc::now();
    let a = candidate("cl-111111", 10., 10, 2);
    let b = candidate("cl-222222", 20., 20, 3);
    let key = "claude:missing-evidence";
    selected(pick(
        &table,
        key,
        std::slice::from_ref(&a),
        &a.account.id,
        now,
        |_| true,
    ));
    selected(pick(
        &table,
        key,
        std::slice::from_ref(&b),
        &b.account.id,
        now,
        |_| true,
    ));
    let later = now + chrono::Duration::minutes(11);
    assert!(matches!(
        pick(
            &table,
            key,
            std::slice::from_ref(&a),
            &a.account.id,
            later,
            |_| panic!("must refuse")
        ),
        Decision::MoveHeld { .. }
    ));
}

#[test]
fn r14_observed_limit_is_fresh_unusable_evidence_not_guard_keep() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let now = Utc::now();
    let mut cs = [
        candidate("cl-111111", 10., 10, 2),
        candidate("cl-222222", 20., 20, 3),
        candidate("cl-333333", 30., 30, 4),
    ];
    let key = "claude:fresh-quota";
    selected(pick(&table, key, &cs, &cs[0].account.id, now, |_| true));
    selected(pick(&table, key, &cs, &cs[1].account.id, now, |_| true));
    cs[1].quota.as_mut().unwrap()["windows"][0]["used_pct"] = serde_json::json!(100);
    // Metadata remains Ready. The same effective limit used by selection must
    // grant the fresh-unusable exception rather than keep an exhausted source.
    assert_eq!(
        selected(pick(&table, key, &cs, &cs[2].account.id, now, |id| {
            assert_eq!(id, cs[2].account.id);
            true
        })),
        cs[2].account.id
    );
}

#[test]
fn r14_excluded_source_needs_fresh_unusable_evidence() {
    for state in ["unknown", "ready", "ready_changed", "payment", "quota"] {
        let root = fixture_dir();
        let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
        let now = Utc::now();
        let mut cs = [
            candidate("cl-111111", 10., 10, 2),
            candidate("cl-222222", 20., 20, 3),
            candidate("cl-333333", 30., 30, 4),
        ];
        let key = "claude:excluded-evidence";
        selected(pick(&table, key, &cs, &cs[0].account.id, now, |_| true));
        selected(pick(&table, key, &cs, &cs[1].account.id, now, |_| true));
        match state {
            "ready_changed" => {
                // A changed fingerprint without actual unusability cannot
                // authorize another move, even with a fresh source collection.
                let before = semantic_evidence(Some(&cs[1].account));
                cs[1].account.allow_credits = !cs[1].account.allow_credits;
                assert_ne!(semantic_evidence(Some(&cs[1].account)), before);
            }
            "payment" => {
                cs[1].account.apply_health_signal(
                    HealthSignal::Cooling {
                        reason: HealthReason::Payment,
                        until: now + chrono::Duration::hours(6),
                        scope: "account".into(),
                    },
                    now,
                );
            }
            "quota" => {
                cs[1].quota.as_mut().unwrap()["windows"][0]["used_pct"] = serde_json::json!(100);
            }
            _ => {}
        }
        let evidence = if state == "unknown" { &[][..] } else { &cs[..] };
        assert_eq!(semantic_evidence(None), 0);
        let calls = AtomicUsize::new(0);
        let choice = table.choose_with_hold_at_admitted_evidence(
            Provider::Claude,
            Some(key),
            None,
            None,
            &cs[2..], // The actual source B remains excluded from eligibility.
            evidence,
            MoveObservations::same(MoveObservation::before_collection()),
            Some(&cs[2].account.id),
            100,
            false,
            false,
            now,
            None,
            |id| {
                assert_eq!(id, cs[2].account.id);
                calls.fetch_add(1, Ordering::SeqCst);
                true
            },
        );
        if matches!(state, "payment" | "quota") {
            assert_eq!(selected(choice), cs[2].account.id, "{state}");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        } else {
            assert!(matches!(choice, Decision::MoveHeld { .. }), "{state}");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(table.current(Some(key)), Some(cs[1].account.id.clone()));
        }
    }
}

#[test]
fn r14_global_refusal_does_not_keep_effectively_exhausted_source() {
    let root = fixture_dir();
    let mut cfg = RoutingConfig::default();
    cfg.move_guard.global_ten_minutes = 1;
    let table = BindingTable::new(root.path(), cfg).unwrap();
    let now = Utc::now();
    let mut cs = [
        candidate("cl-111111", 10., 10, 2),
        candidate("cl-222222", 20., 20, 3),
        candidate("cl-333333", 30., 30, 4),
    ];
    let key = "claude:global-quota";
    selected(pick(&table, key, &cs, &cs[0].account.id, now, |_| true));
    selected(pick(&table, key, &cs, &cs[1].account.id, now, |_| true));
    cs[1].quota.as_mut().unwrap()["windows"][0]["used_pct"] = serde_json::json!(100);
    assert!(matches!(
        pick(&table, key, &cs, &cs[2].account.id, now, |_| panic!(
            "no source or target admission"
        )),
        Decision::MoveHeld { .. }
    ));
}

#[test]
fn r14_guard_veto_preserves_overload_mark() {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let now = Utc::now();
    let cs = [
        candidate("cl-111111", 10., 10, 2),
        candidate("cl-222222", 20., 20, 3),
        candidate("cl-333333", 30., 30, 4),
    ];
    let key = "claude:marked-guard";
    selected(pick(&table, key, &cs, &cs[0].account.id, now, |_| true));
    selected(pick(&table, key, &cs, &cs[1].account.id, now, |_| true));
    table.data.lock().unwrap().overload_marks.insert(
        key.into(),
        OverloadMark {
            expires: now + chrono::Duration::minutes(5),
            from: cs[1].account.id.clone(),
            target: cs[2].account.id.clone(),
        },
    );
    assert_eq!(
        selected(pick(&table, key, &cs, &cs[2].account.id, now, |_| true)),
        cs[1].account.id
    );
    assert!(table.data.lock().unwrap().overload_marks.contains_key(key));
}
