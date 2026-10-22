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
    table.choose_with_hold_at_admitted(
        Provider::Claude,
        Some(key),
        None,
        None,
        candidates,
        Some(pin),
        100,
        false,
        false,
        now,
        admit,
    )
}
fn selected(decision: Decision) -> String {
    match decision {
        Decision::Selected { account, .. } => account,
        other => panic!("expected placement: {other:?}"),
    }
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
    for state in ["unknown", "ready", "payment", "quota"] {
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
