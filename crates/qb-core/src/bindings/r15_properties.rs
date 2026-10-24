//! Bounded R15 properties with synthetic inputs and real binding transactions.
//! C7 eligible fallback remains distinct from observed quota exhaustion.
use super::tests::{candidate, fixture_dir};
use super::*;
use crate::accounts::Identity;
use chrono::Duration as Delta;
use serde_json::json;

#[derive(Clone, Copy, Debug)]
enum PathKind {
    New,
    Health,
    Capacity,
}

fn key(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude:r15-root",
        Provider::Codex => "codex:r15-root",
    }
}

fn config() -> RoutingConfig {
    RoutingConfig {
        gate_5h_pct: 80.0,
        gate_weekly_pct: 90.0,
        gate_weekly_near_reset_pct: 98.0,
        inflight_allowance_pct: 1.0,
        placement_allowance_pct: 2.0,
        // Isolate the gate/charge rule from the independent weak-evidence cap.
        weak_evidence_new_sessions: 16,
        ..RoutingConfig::default()
    }
}

fn fresh_table(cfg: RoutingConfig) -> (tempfile::TempDir, Arc<BindingTable>) {
    let root = fixture_dir();
    let table = BindingTable::new(root.path(), cfg).unwrap();
    (root, table)
}

fn account(
    id: &str,
    provider: Provider,
    names: &[(&str, bool)],
    now: DateTime<Utc>,
    weekly_seconds: i64,
    short_seconds: i64,
) -> Candidate {
    let mut c = candidate(id, 10.0, 24, 4);
    c.account.provider = provider;
    if provider == Provider::Codex {
        c.account.identity = Identity::Codex {
            user_id: None,
            account_id: id.into(),
            email: "synthetic@example.invalid".into(),
        };
    }
    c.quota = Some(json!({
        "observed_at": now - Delta::seconds(1),
        "windows": names.iter().map(|(name, weekly)| {
            let minutes = match *name {
                "5h" | "primary" => 300,
                "7d" | "secondary" => 10080,
                other => other.strip_suffix('m').unwrap().parse::<u64>().unwrap(),
            };
            json!({
                "name": name, "window_minutes": minutes, "used_pct": 10.0,
                "reset_at": now + Delta::seconds(if *weekly {weekly_seconds} else {short_seconds}),
                "observed_at": now - Delta::seconds(1),
            })
        }).collect::<Vec<_>>()
    }));
    c
}

fn used(c: &mut Candidate, index: usize, pct: f64) {
    c.quota.as_mut().unwrap()["windows"][index]["used_pct"] = json!(pct);
}

fn selected(decision: Decision) -> (String, Vec<MoveEvent>) {
    match decision {
        Decision::Selected {
            account,
            lease,
            moves,
        } => {
            lease.dispatched(); // Actual publication/receipt; drop releases pending.
            drop(lease);
            (account, moves)
        }
        other => panic!("expected real selection, got {other:?}"),
    }
}

fn place(
    table: &Arc<BindingTable>,
    provider: Provider,
    session: &str,
    c: &Candidate,
    now: DateTime<Utc>,
) {
    let (id, moves) = selected(table.choose_with_hold_at(
        provider,
        Some(session),
        None,
        None,
        std::slice::from_ref(c),
        None,
        64,
        false,
        false,
        now,
    ));
    assert_eq!(id, c.account.id);
    assert_eq!(moves.len(), 1);
    assert!(moves[0].from.is_none());
}

fn total(table: &BindingTable, id: &str) -> u64 {
    table
        .data
        .lock()
        .unwrap()
        .placements
        .get(id)
        .map_or(0, |p| p.total)
}

// Each possible target gets a real owner fixture, not an expected-target pin.
// The fixtures retain occupied source and reserved destination slots until the
// native selector has validated admission and completed rebind. The existing
// owner helper fast-forwards its own synthetic wait; no public proof constructor.
fn capacity(
    table: &Arc<BindingTable>,
    provider: Provider,
    source: &str,
    targets: &[Candidate],
    sources: &[Candidate],
    now: DateTime<Utc>,
) -> Decision {
    let mut owners: Vec<_> = targets
        .iter()
        .map(|target| {
            assert_ne!(target.account.id, source);
            let (life, proof) = crate::relay::test_move_evidence(source, &target.account.id);
            (life, Some(proof))
        })
        .collect();
    let decision = table.choose_with_hold_at_admitted_proof(
        provider,
        Some(key(provider)),
        None,
        None,
        targets,
        sources,
        MoveObservations::untracked(),
        None,
        64,
        false,
        false,
        now,
        None,
        |chosen| {
            let index = targets.iter().position(|c| c.account.id == chosen)?;
            Some(DispatchAdmission::capacity(owners[index].1.take().unwrap()))
        },
    );
    drop(owners);
    decision
}

fn move_or_place(
    table: &Arc<BindingTable>,
    provider: Provider,
    path: PathKind,
    targets: &[Candidate],
    sources: &[Candidate],
    now: DateTime<Utc>,
) -> Decision {
    match path {
        PathKind::Capacity => capacity(table, provider, "a", targets, sources, now),
        PathKind::New | PathKind::Health => table.choose_with_hold_at_admitted_evidence(
            provider,
            Some(key(provider)),
            None,
            None,
            targets,
            sources,
            MoveObservations::untracked(),
            None,
            64,
            false,
            false,
            now,
            None,
            |_| true,
        ),
    }
}

fn assert_move(moves: &[MoveEvent], target: &str) {
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].from.as_deref(), Some("a"));
    assert_eq!(moves[0].to, target);
    assert!(moves[0].is_placement);
    assert_eq!(moves[0].placement_charge_pct, 2.0);
}

#[test]
fn r15_all_small_gate_boundaries_use_a_passing_target_when_one_exists() {
    let layouts: &[(Provider, &[(&str, bool)])] = &[
        (Provider::Claude, &[("5h", false), ("7d", true)]),
        (Provider::Codex, &[("5h", false), ("7d", true)]),
        (Provider::Codex, &[("primary", false), ("secondary", true)]),
        (
            Provider::Codex,
            &[
                ("60m", false),
                ("1440m", false),
                ("10080m", true),
                ("43200m", true),
            ],
        ),
        (
            Provider::Codex,
            &[
                ("5h", false),
                ("60m", false),
                ("7d", true),
                ("43200m", true),
            ],
        ),
    ];
    let mut cases = 0;
    for &(provider, names) in layouts {
        for bad_window in 0..names.len() {
            for near in [false, true] {
                for delta in [-0.25, 0.0, 0.25] {
                    for inflight in [0, 1] {
                        for precharges in [0, 1] {
                            for reverse in [false, true] {
                                for path in [PathKind::Health, PathKind::Capacity] {
                                    let now = Utc::now();
                                    let cfg = config();
                                    let (_root, table) = fresh_table(cfg.clone());
                                    let mut a = account("a", provider, names, now, 172800, 14400);
                                    let mut b = account(
                                        "b",
                                        provider,
                                        names,
                                        now,
                                        cfg.near_reset_secs as i64 + if near { -1 } else { 1 },
                                        7200,
                                    );
                                    let c = account("c", provider, names, now, 432000, 28800);
                                    if precharges == 1 {
                                        let warm = if provider == Provider::Codex {
                                            "codex:r15-charge"
                                        } else {
                                            "claude:r15-charge"
                                        };
                                        place(&table, provider, warm, &b, now);
                                    }
                                    place(&table, provider, key(provider), &a, now);
                                    let weekly = names[bad_window].1;
                                    let threshold = if !weekly {
                                        cfg.gate_5h_pct
                                    } else if near {
                                        cfg.gate_weekly_near_reset_pct
                                    } else {
                                        cfg.gate_weekly_pct
                                    };
                                    b.inflight = inflight;
                                    // Independent arithmetic oracle; no call to metrics/selector.
                                    used(
                                        &mut b,
                                        bad_window,
                                        threshold + delta
                                            - inflight as f64 * cfg.inflight_allowance_pct
                                            - precharges as f64 * cfg.placement_allowance_pct,
                                    );
                                    if matches!(path, PathKind::Health) {
                                        a.account.user_state = UserState::Paused;
                                    }
                                    let sources = [a, b.clone(), c.clone()];
                                    let targets = if reverse { vec![c, b] } else { vec![b, c] };
                                    // Keep evaluation at the exact reset +/-1 second boundary.
                                    let (id, moves) = selected(move_or_place(
                                        &table, provider, path, &targets, &sources, now,
                                    ));
                                    let expected = if delta < 0.0 { "b" } else { "c" };
                                    assert_eq!(
                                        id, expected,
                                        "{provider:?} {names:?} window={bad_window} near={near} delta={delta} inflight={inflight} charges={precharges} reverse={reverse} path={path:?}"
                                    );
                                    assert_move(&moves, expected);
                                    assert_eq!(
                                        total(&table, expected),
                                        1 + if expected == "b" { precharges } else { 0 }
                                    );
                                    assert_eq!(
                                        table.current(Some(key(provider))).as_deref(),
                                        Some(expected)
                                    );
                                    cases += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 1344);
}

#[test]
fn r15_c7_2b_forced_fallback_remains_eligible_when_every_projected_gate_fails() {
    for provider in [Provider::Claude, Provider::Codex] {
        for path in [PathKind::Health, PathKind::Capacity] {
            let now = Utc::now();
            let (_root, table) = fresh_table(config());
            let names = [("5h", false), ("7d", true)];
            let mut a = account("a", provider, &names, now, 172800, 14400);
            let mut b = account("b", provider, &names, now, 172800, 14400);
            place(&table, provider, key(provider), &a, now);
            used(&mut b, 0, 79.0); // 79 + 2 inflight >= 80 short gate.
            used(&mut b, 1, 89.0); // 89 + 2 inflight >= 90 weekly gate.
            b.inflight = 2; // Neither observed value is at its C5 limit.
            if matches!(path, PathKind::Health) {
                a.account.user_state = UserState::Paused;
            }
            let sources = [a, b.clone()];
            let (id, moves) = selected(move_or_place(&table, provider, path, &[b], &sources, now));
            assert_eq!(id, "b");
            assert_move(&moves, "b");
        }
    }
}

#[test]
fn r15_observed_weekly_exhaustion_is_never_new_or_moved_placement() {
    for (provider, name) in [
        (Provider::Claude, "7d"),
        (Provider::Codex, "7d"),
        (Provider::Codex, "secondary"),
        (Provider::Codex, "43200m"),
    ] {
        for pct in [100.0, 101.0] {
            for near in [false, true] {
                for path in [PathKind::New, PathKind::Health, PathKind::Capacity] {
                    for safe in [false, true] {
                        let now = Utc::now();
                        let (_root, table) = fresh_table(config());
                        let names = [("5h", false), (name, true)];
                        let mut a = account("a", provider, &names, now, 172800, 14400);
                        let mut b = account(
                            "b",
                            provider,
                            &names,
                            now,
                            if near { 10 } else { 172800 },
                            14400,
                        );
                        let c = account("c", provider, &names, now, 432000, 28800);
                        if !matches!(path, PathKind::New) {
                            place(&table, provider, key(provider), &a, now);
                        }
                        if matches!(path, PathKind::Health) {
                            a.account.user_state = UserState::Paused;
                        }
                        used(&mut b, 1, pct);
                        assert!(!b.account.allow_credits);
                        let sources = [a, b.clone(), c.clone()];
                        let targets = if safe { vec![b, c] } else { vec![b] };
                        let decision =
                            move_or_place(&table, provider, path, &targets, &sources, now);
                        if safe {
                            let (id, moves) = selected(decision);
                            assert_eq!(id, "c");
                            if matches!(path, PathKind::New) {
                                assert!(moves[0].from.is_none());
                            } else {
                                assert_move(&moves, "c");
                            }
                        } else {
                            assert!(matches!(decision, Decision::None), "{decision:?}");
                        }
                        assert_eq!(
                            total(&table, "b"),
                            0,
                            "exhausted destination acquired a placement"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn r15_past_reset_weekly_observation_is_not_current_exhaustion() {
    for provider in [Provider::Claude, Provider::Codex] {
        let now = Utc::now();
        let (_root, table) = fresh_table(config());
        let mut b = account("b", provider, &[("7d", true)], now, -1, 14400);
        used(&mut b, 0, 100.0);
        let (id, _) = selected(table.choose_with_hold_at(
            provider,
            Some(key(provider)),
            None,
            None,
            &[b],
            None,
            64,
            false,
            false,
            now,
        ));
        assert_eq!(id, "b");
    }
}

#[test]
fn r15_single_capacity_rejection_has_one_move_across_all_minute_retries() {
    for provider in [Provider::Claude, Provider::Codex] {
        let now = Utc::now();
        let (_root, table) = fresh_table(RoutingConfig::default());
        let names = [("5h", false), ("7d", true)];
        let a = account("a", provider, &names, now, 86400, 14400);
        let b = account("b", provider, &names, now, 172800, 14400);
        let c = account("c", provider, &names, now, 259200, 14400);
        place(&table, provider, key(provider), &a, now);
        let sources = [a, b.clone(), c];
        // Exactly one owner rejection fixture, one spill and one real move.
        let (id, moves) = selected(capacity(
            &table,
            provider,
            "a",
            std::slice::from_ref(&b),
            &sources,
            now,
        ));
        assert_eq!(id, "b");
        assert_move(&moves, "b");
        let before = total(&table, "b");
        for elapsed in 0..=60 {
            // No additional rejection/proof/pin: retry normal native selection.
            let (id, moves) = selected(table.choose_with_hold_at(
                provider,
                Some(key(provider)),
                None,
                None,
                &sources,
                None,
                64,
                false,
                false,
                now + Delta::seconds(elapsed),
            ));
            assert_eq!(id, "b", "retry second={elapsed}");
            assert!(
                moves.is_empty(),
                "second move from one rejection at {elapsed}s"
            );
        }
        assert_eq!(total(&table, "b"), before);
        assert_eq!(total(&table, "c"), 0);
    }
}

#[test]
fn r15_capacity_cannot_evade_default_session_budget_at_any_minute_offset() {
    for provider in [Provider::Claude, Provider::Codex] {
        for elapsed in 0..=60 {
            let now = Utc::now();
            let (_root, table) = fresh_table(RoutingConfig::default());
            let events = Arc::new(Mutex::new(Vec::<MoveGuardEvent>::new()));
            let sink = events.clone();
            table.set_move_guard_observer(Arc::new(move |event| {
                sink.lock().unwrap().push(event.clone())
            }));
            let names = [("5h", false), ("7d", true)];
            let a = account("a", provider, &names, now, 86400, 14400);
            let b = account("b", provider, &names, now, 172800, 14400);
            let c = account("c", provider, &names, now, 259200, 14400);
            place(&table, provider, key(provider), &a, now);
            let sources = [a, b.clone(), c.clone()];
            let (_, moves) = selected(capacity(&table, provider, "a", &[b], &sources, now));
            assert_move(&moves, "b");
            let before = table.export().unwrap();
            // A *second* genuine rejection is a stronger, separate guard probe;
            // unchanged Ready health cannot authorize a fresh-unusable exception.
            let decision = capacity(
                &table,
                provider,
                "b",
                &[c],
                &sources,
                now + Delta::seconds(elapsed),
            );
            assert!(
                matches!(decision, Decision::MoveHeld { .. }),
                "{decision:?}"
            );
            assert_eq!(table.export().unwrap(), before);
            assert_eq!(
                events.lock().unwrap().last().unwrap().reason,
                "session_ten_minutes"
            );
            assert_eq!(total(&table, "c"), 0);
        }
    }
}

#[test]
fn r15_a_b_a_ping_pong_veto_survives_relaxed_ordinary_session_budget() {
    for provider in [Provider::Claude, Provider::Codex] {
        for elapsed in [0, 1, 30, 59, 60] {
            let now = Utc::now();
            let mut cfg = RoutingConfig::default();
            cfg.move_guard.session_ten_minutes = 16;
            cfg.move_guard.session_hour = 64;
            cfg.move_guard.global_ten_minutes = 128;
            cfg.churn.dwell_secs = 0;
            cfg.churn.revisit_secs = 0; // Isolate the independent D29 return guard.
            let (_root, table) = fresh_table(cfg);
            let events = Arc::new(Mutex::new(Vec::<MoveGuardEvent>::new()));
            let sink = events.clone();
            table.set_move_guard_observer(Arc::new(move |event| {
                sink.lock().unwrap().push(event.clone())
            }));
            let names = [("5h", false), ("7d", true)];
            let a = account("a", provider, &names, now, 86400, 14400);
            let b = account("b", provider, &names, now, 172800, 14400);
            place(&table, provider, key(provider), &a, now);
            let sources = [a.clone(), b.clone()];
            let (_, moves) = selected(capacity(&table, provider, "a", &[b], &sources, now));
            assert_move(&moves, "b");
            let before = table.export().unwrap();
            // Synthetic attempted A -> B -> A. The return has a real capacity
            // owner proof but no changed destination evidence since departure.
            let decision = capacity(
                &table,
                provider,
                "b",
                &[a],
                &sources,
                now + Delta::seconds(elapsed),
            );
            assert!(
                matches!(decision, Decision::MoveHeld { .. }),
                "{decision:?}"
            );
            assert_eq!(table.current(Some(key(provider))).as_deref(), Some("b"));
            assert_eq!(table.export().unwrap(), before);
            assert_eq!(events.lock().unwrap().last().unwrap().reason, "ping_pong");
            assert_eq!(
                total(&table, "a"),
                1,
                "return became a second placement on A"
            );
        }
    }
}
