//! Fixed-time differential coverage of raw calculations and row-based queries.
//! Native Data appears only in these reference fixtures.
use super::*;
use crate::accounts::Provider;
use crate::bindings::{self as native, AccountIndex, BindingTable, Candidate, Data};
use chrono::Duration;
use serde_json::json;

fn now() -> DateTime<Utc> {
    "2026-10-30T12:00:00Z".parse().unwrap()
}

fn candidate(provider: Provider, windows: Vec<Value>) -> Candidate {
    let mut c = native::tests::candidate("a", 0.0, 168, 5);
    c.account.provider = provider;
    c.quota = Some(json!({"windows": windows}));
    c
}

fn window(name: &str, used: Value, hours: i64) -> Value {
    json!({"name": name, "used_pct": used, "observed_at": now(), "reset_at": now() + Duration::hours(hours)})
}

fn raw_candidate(c: &Candidate) -> CandidateInput {
    CandidateInput {
        account: c.account.clone(),
        quota: c.quota.clone(),
        inflight: c.inflight,
    }
}

fn raw(d: &Data, config: RoutingConfig) -> MetricInputs {
    MetricInputs {
        capacity: Some(BTreeMap::new()),
        config,
        recent: d
            .accounts
            .iter()
            .map(|(id, a)| (id.clone(), a.recent.iter().cloned().collect()))
            .collect(),
        idle_placements: d
            .launches
            .keys()
            .filter(|key| native::launch::idle_placement(d, key))
            .cloned()
            .collect(),
        thresholds: BTreeMap::new(),
        overload_marks: d
            .overload_marks
            .iter()
            .map(|(g, m)| {
                (
                    g.clone(),
                    OverloadInput {
                        expires: m.expires,
                        from: m.from.clone(),
                        target: m.target.clone(),
                    },
                )
            })
            .collect(),
        pacing: d.pacing.clone(),
        placements: d
            .placements
            .iter()
            .map(|(id, p)| {
                (
                    id.clone(),
                    PlacementInput {
                        latest: p.latest,
                        total: p.total,
                        window_set_sequence: p.window_set_sequence,
                        windows: p
                            .windows
                            .iter()
                            .map(|(name, w)| {
                                (
                                    name.clone(),
                                    PlacementWindowInput {
                                        value: w.value.clone(),
                                        asof: w.asof.map(|a| PlacementAsOfInput {
                                            sequence: a.sequence,
                                            placed_count: a.placed_count,
                                        }),
                                        reported_sequence: w.reported_sequence,
                                        unresolved_equal_charge: w.unresolved_equal_charge,
                                    },
                                )
                            })
                            .collect(),
                    },
                )
            })
            .collect(),
    }
}

fn result(c: &Candidate, d: &Data, raw: &MetricInputs) -> Metrics {
    calculate(
        &raw_candidate(c),
        raw,
        &raw.config,
        d.pending.get(&c.account.id).copied().unwrap_or(0),
        now(),
        |w| native::legacy_stale_window(c, d, w, now()),
    )
}

fn assert_metrics(actual: &Metrics, expected: &native::Metrics) {
    assert_eq!(
        (actual.gate, actual.unknown, actual.weekly, actual.five),
        (
            expected.gate,
            expected.unknown,
            expected.weekly,
            expected.five
        )
    );
    assert_eq!(actual.usage.to_bits(), expected.usage.to_bits());
}

fn parity(c: &Candidate, d: &Data, cfg: &RoutingConfig) -> Metrics {
    let raw = raw(d, cfg.clone());
    let before = serde_json::to_value(&raw).unwrap();
    let m = result(c, d, &raw);
    assert_metrics(&m, &native::metrics(c, d, cfg, now()));
    assert_eq!(
        evidence(&raw_candidate(c), &raw, now()),
        native::placement_evidence(c, d, now())
    );
    let state = placement_state(&raw_candidate(c), &raw);
    let expected = native::placement_state(c, d);
    assert_eq!(state.count(), expected.count());
    assert_eq!(state.weak(now()), expected.weak(now()));
    for name in state.windows.keys().chain(expected.windows.keys()) {
        assert_eq!(state.charge_count(name), expected.charge_count(name));
    }
    assert_eq!(
        serde_json::to_value(&raw).unwrap(),
        before,
        "queries preserve every raw observation"
    );
    m
}

#[test]
fn known_windows_duration_slots_and_name_gates_match_native() {
    let cfg = RoutingConfig::default();
    for provider in [Provider::Claude, Provider::Codex] {
        for name in [
            "5h",
            "7d",
            "primary",
            "secondary",
            "43200m",
            "1440m",
            "1441m",
            "0m",
            "7d_oi",
            "60m_oi",
            "30s",
            "unknown",
        ] {
            for used in [json!(10), json!(90), json!(98), Value::Null, json!(-1)] {
                let c = candidate(provider, vec![window(name, used, 8)]);
                let m = parity(&c, &Data::default(), &cfg);
                assert_eq!(
                    m.unknown,
                    native::account_window(provider, &c.quota.as_ref().unwrap()["windows"][0])
                        .is_none()
                        || c.quota.as_ref().unwrap()["windows"][0]["used_pct"]
                            .as_f64()
                            .is_none_or(|n| n < 0.0)
                );
            }
        }
    }
}

#[test]
fn weekly_near_reset_and_slot_collisions_gate_every_window() {
    let cfg = RoutingConfig::default();
    for hours in [4, 8] {
        for used in [89, 90, 97, 98] {
            let c = candidate(
                Provider::Codex,
                vec![
                    window("7d", json!(used), hours),
                    window("43200m", json!(10), 30),
                    window("primary", json!(10), 2),
                ],
            );
            let m = parity(&c, &Data::default(), &cfg);
            assert_eq!(m.weekly, Some(now() + Duration::hours(hours)));
            assert_eq!(m.gate, used < if hours == 4 { 98 } else { 90 });
        }
    }
    let c = candidate(
        Provider::Codex,
        vec![window("5h", json!(50), 4), window("primary", json!(50), 2)],
    );
    assert_eq!(
        parity(&c, &Data::default(), &cfg).five,
        Some(now() + Duration::hours(2))
    );
}

#[test]
fn stale_900_901_traffic_pending_inflight_and_expired_windows() {
    let cfg = RoutingConfig::default();
    for age in [900, 901] {
        for traffic in [false, true] {
            for pending in [0, 2] {
                for inflight in [0, 3] {
                    for expired in [false, true] {
                        let mut w = window("5h", json!(99), if expired { 0 } else { 2 });
                        w["observed_at"] = json!(now() - Duration::seconds(age));
                        let mut c = candidate(Provider::Claude, vec![w]);
                        c.inflight = inflight;
                        let mut d = Data::default();
                        d.pending.insert("a".into(), pending);
                        if traffic {
                            d.traffic.insert("a".into(), now());
                        }
                        let m = parity(&c, &d, &cfg);
                        assert_eq!(
                            m.unknown,
                            !expired && age == 901 && (traffic || pending > 0 || inflight > 0)
                        );
                        if expired {
                            assert_eq!(
                                m.usage,
                                pending.max(inflight) as f64 * cfg.inflight_allowance_pct
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn absent_quota_and_no_gating_windows_remain_unknown() {
    let mut c = candidate(Provider::Claude, vec![]);
    assert!(parity(&c, &Data::default(), &RoutingConfig::default()).unknown);
    c.quota = None;
    let m = parity(&c, &Data::default(), &RoutingConfig::default());
    assert!(m.gate && m.unknown && m.usage.is_infinite());
}

#[test]
fn pending_inflight_max_and_placement_charge_can_be_removed() {
    let mut c = candidate(
        Provider::Claude,
        vec![window("5h", json!(80), 2), window("7d", json!(10), 20)],
    );
    c.inflight = 3;
    let mut d = Data::default();
    d.pending.insert("a".into(), 2);
    let mut state = PlacementState::default();
    state.observe(c.quota.as_ref().unwrap());
    state.placed(now(), 1);
    state.placed(now(), 2);
    d.placements.insert("a".into(), state);
    let cfg = RoutingConfig {
        placement_allowance_pct: 4.0,
        ..RoutingConfig::default()
    };
    let m = parity(&c, &d, &cfg);
    assert_eq!(m.usage, 91.0);
    assert!(!m.gate);
    let mut raw = raw(&d, cfg.clone());
    raw.config.placement_allowance_pct = 0.0;
    let uncharged = result(&c, &d, &raw);
    assert_metrics(
        &uncharged,
        &native::metrics_without_placement_charge(&c, &d, &cfg, now()),
    );
    assert_eq!(uncharged.usage, 83.0);
    assert!(uncharged.gate);
}

#[test]
fn equal_asof_keeps_charge_but_not_cap_and_newer_asof_covers_it() {
    let c = candidate(Provider::Claude, vec![window("5h", json!(10), 2)]);
    let mut d = Data::default();
    let mut state = PlacementState::default();
    state.placed(now(), 10);
    state.observe_asof(
        c.quota.as_ref().unwrap(),
        UsageObservationAsOf {
            sequence: 10,
            placed_count: 1,
        },
    );
    d.placements.insert("a".into(), state);
    let cfg = RoutingConfig::default();
    let m = parity(&c, &d, &cfg);
    assert_eq!(m.usage, 12.0);
    assert_eq!(
        native::placement_evidence(&c, &d, now()),
        (PlacementEvidence::Fresh, 0)
    );
    d.placements.get_mut("a").unwrap().placed(now(), 11);
    assert_eq!(parity(&c, &d, &cfg).usage, 14.0);
    d.placements.get_mut("a").unwrap().observe_asof(
        c.quota.as_ref().unwrap(),
        UsageObservationAsOf {
            sequence: 12,
            placed_count: 2,
        },
    );
    assert_eq!(parity(&c, &d, &cfg).usage, 10.0);
}

#[test]
fn weak_new_reset_and_complete_window_sequence_never_resurrect_old_windows() {
    let mut c = candidate(
        Provider::Codex,
        vec![window("5h", json!(10), 0), window("7d", json!(20), 20)],
    );
    c.quota.as_mut().unwrap()["windows"][0]["observed_at"] = json!(now() - Duration::seconds(1));
    let mut state = PlacementState::default();
    state.placed(now(), 2);
    state.observe_complete_asof(
        c.quota.as_ref().unwrap(),
        UsageObservationAsOf {
            sequence: 3,
            placed_count: 1,
        },
    );
    let mut d = Data::default();
    d.placements.insert("a".into(), state);
    parity(&c, &d, &RoutingConfig::default());
    assert_eq!(
        native::placement_evidence(&c, &d, now()),
        (PlacementEvidence::Weak, 0)
    );
    let complete = json!({"windows": [window("7d", json!(20), 20)]});
    d.placements.get_mut("a").unwrap().observe_complete_asof(
        &complete,
        UsageObservationAsOf {
            sequence: 4,
            placed_count: 1,
        },
    );
    parity(&c, &d, &RoutingConfig::default());
    let raw = raw(&d, RoutingConfig::default());
    assert!(
        !placement_state(&raw_candidate(&c), &raw)
            .windows
            .contains_key("5h")
    );
    assert_eq!(
        evidence(&raw_candidate(&c), &raw, now()),
        (PlacementEvidence::Fresh, 0)
    );
    d.placements.get_mut("a").unwrap().observe_complete_asof(
        &json!({"windows": []}),
        UsageObservationAsOf {
            sequence: 5,
            placed_count: 1,
        },
    );
    parity(&c, &d, &RoutingConfig::default());
    let raw = self::raw(&d, RoutingConfig::default());
    assert!(placement_state(&raw_candidate(&c), &raw).windows.is_empty());
    assert_eq!(
        evidence(&raw_candidate(&c), &raw, now()),
        (PlacementEvidence::Weak, 1)
    );
}

#[test]
fn missing_asof_stays_weak_and_weak_cap_uses_configured_count() {
    let c = candidate(
        Provider::Claude,
        vec![json!({"name": "5h", "used_pct": 10})],
    );
    let mut d = Data::default();
    let mut state = PlacementState::default();
    state.placed(now(), 1);
    d.placements.insert("a".into(), state);
    for cap in [0, 1, 2] {
        let cfg = RoutingConfig {
            weak_evidence_new_sessions: cap,
            ..RoutingConfig::default()
        };
        parity(&c, &d, &cfg);
        let raw = raw(&d, cfg.clone());
        let (e, count) = evidence(&raw_candidate(&c), &raw, now());
        assert_eq!((e, count), (PlacementEvidence::Weak, 1));
        assert_eq!(
            e == PlacementEvidence::Weak && count >= cap,
            native::placement_capped(&c, &d, &cfg, now())
        );
    }
}

#[test]
fn account_order_ties_preserve_native_order_after_id_projection() {
    let a = candidate(
        Provider::Claude,
        vec![window("5h", json!(10), 2), window("7d", json!(10), 20)],
    );
    let mut b = a.clone();
    b.account.id = "b".into();
    let d = Data::default();
    let cfg = RoutingConfig::default();
    let x = parity(&a, &d, &cfg);
    let y = parity(&b, &d, &cfg);
    let order = vec!["hash-z".into(), "hash-a".into()];
    for anchor in [None, Some(now() + Duration::hours(19))] {
        assert_eq!(
            compare_in_order(&order, "hash-z", &x, "hash-a", &y, anchor),
            Some(native::compare_gated(
                &a,
                &native::metrics(&a, &d, &cfg, now()),
                &b,
                &native::metrics(&b, &d, &cfg, now()),
                anchor
            ))
        );
    }
    assert_eq!(
        compare_in_order(&order, "hash-z", &x, "missing", &y, None),
        None
    );
    assert_eq!(
        compare_in_order(&order, "hash-a", &y, "hash-z", &x, None),
        Some(Ordering::Greater)
    );
}

#[test]
fn activity_idle_placements_live_override_reservations_and_thresholds_match_native() {
    let root = native::tests::fixture_dir();
    let cfg = RoutingConfig {
        active_window_secs: 30,
        spread_threshold: 7,
        ..RoutingConfig::default()
    };
    let table = BindingTable::new(root.path(), cfg.clone()).unwrap();
    let mut d = Data::default();
    d.accounts.insert(
        "a".into(),
        AccountIndex {
            recent: [
                (now() - Duration::seconds(30), "edge"),
                (now() - Duration::seconds(31), "old"),
                (now(), "idle"),
                (now() - Duration::hours(2), "live-idle"),
            ]
            .into_iter()
            .map(|(t, g)| (t, g.into()))
            .collect(),
            ..AccountIndex::default()
        },
    );
    for g in ["idle", "live-idle"] {
        d.launches.insert(g.into(), serde_json::from_value(json!({"current_model": null, "advertised_models": [], "policy_key": g, "idle_placement": true})).unwrap());
    }
    d.active.insert("live-idle".into(), 1);
    for (g, target, seconds) in [("live", "a", 1), ("expired", "a", 0), ("other", "b", 1)] {
        d.overload_marks.insert(
            g.into(),
            native::OverloadMark {
                expires: now() + Duration::seconds(seconds),
                from: "source".into(),
                target: target.into(),
            },
        );
    }
    let mut raw = raw(&d, cfg);
    let active = d.active.keys().cloned().collect();
    assert_eq!(activity(&raw, &active, "a", now()), 2);
    for id in ["a", "b", "missing"] {
        assert_eq!(
            activity(&raw, &active, id, now()),
            table.active_sessions_in(&d, id, now())
        );
        for except in [None, Some("live"), Some("expired")] {
            assert_eq!(
                reservations(&raw, id, except, now()),
                table.reserved_in(&d, id, except, now())
            );
        }
        assert_eq!(threshold(&raw, id), table.spread_threshold_for(id));
    }
    for (key, expected) in [("live", true), ("expired", false), ("missing", false)] {
        assert_eq!(
            raw.overload_marks
                .get(super::super::group(key))
                .is_some_and(|m| m.expires > now()),
            table.has_overload_mark(&d, key, now())
        );
        assert_eq!(table.has_overload_mark(&d, key, now()), expected);
    }
    table.set_account_threshold("a", Some(3));
    raw.thresholds.insert("a".into(), 3);
    assert_eq!(threshold(&raw, "a"), table.spread_threshold_for("a"));
    assert_eq!(threshold(&raw, "b"), 7);
    assert!(!raw.pacing.get("a").copied().unwrap_or(false));
}

#[test]
fn raw_serde_retains_absence_and_rejects_unknown_fields() {
    let raw = raw(&Data::default(), RoutingConfig::default());
    let mut value = serde_json::to_value(&raw).unwrap();
    let decoded: MetricInputs = serde_json::from_value(value.clone()).unwrap();
    assert!(
        decoded.placements.is_empty() && decoded.recent.is_empty() && decoded.pacing.is_empty()
    );
    value["gate"] = json!(true);
    assert!(serde_json::from_value::<MetricInputs>(value).is_err());
    assert!(serde_json::from_value::<PlacementInput>(json!({"latest": null, "total": 0, "windows": {}, "window_set_sequence": null, "count": 0})).is_err());
}

#[test]
fn rebalance_waiting_base_and_cap_match_native_balance_and_expiry_branches() {
    let root = native::tests::fixture_dir();
    let cfg = RoutingConfig {
        spread_threshold: 3,
        ..RoutingConfig::default()
    };
    let table = BindingTable::new(root.path(), cfg.clone()).unwrap();
    for source_count in [0, 1, 2, 4] {
        for target_count in [0, 1, 3] {
            for except in [None, Some("live"), Some("expired"), Some("absent")] {
                for drain in [false, true] {
                    for weak in [false, true] {
                        let mut c = candidate(Provider::Claude, vec![window("5h", json!(10), 2)]);
                        c.account.drain = drain;
                        let mut d = Data::default();
                        for (id, count) in [("source", source_count), ("a", target_count)] {
                            d.accounts.insert(
                                id.into(),
                                AccountIndex {
                                    recent: (0..count)
                                        .map(|i| (now(), format!("{id}-{i}")))
                                        .collect(),
                                    ..AccountIndex::default()
                                },
                            );
                        }
                        for (g, seconds) in [("live", 1), ("expired", 0)] {
                            d.overload_marks.insert(
                                g.into(),
                                native::OverloadMark {
                                    expires: now() + Duration::seconds(seconds),
                                    from: "source".into(),
                                    target: "a".into(),
                                },
                            );
                        }
                        d.pacing.insert("a".into(), true);
                        if weak {
                            let mut state = PlacementState::default();
                            state.observe(c.quota.as_ref().unwrap());
                            state.placed(now(), 1);
                            d.placements.insert("a".into(), state);
                        }
                        let raw = raw(&d, cfg.clone());
                        let active = d.active.keys().cloned().collect();
                        let m = result(&c, &d, &raw);
                        let eligible = native::effective_account(&c, &d, &cfg, None, now())
                            .eligible_at(None, now());
                        let actual = waiting(
                            &raw,
                            &raw_candidate(&c),
                            &m,
                            eligible,
                            activity(&raw, &active, "a", now())
                                + reservations(&raw, "a", except, now()),
                            activity(&raw, &active, "source", now()),
                            except,
                            now(),
                        );
                        assert_eq!(
                            actual,
                            table.rebalance_target_waiting(&d, "source", &c, except, now())
                        );
                        assert_eq!(
                            actual,
                            table.rebalance_target_base(&d, "source", &c, except, now())
                        );
                        let (e, count) = evidence(&raw_candidate(&c), &raw, now());
                        let capped =
                            e == PlacementEvidence::Weak && count >= cfg.weak_evidence_new_sessions;
                        assert_eq!(
                            actual && !capped,
                            table.rebalance_target(&d, "source", &c, except, now())
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn comparator_matches_native_unknown_cohort_reset_and_usage_ordering() {
    let d = Data::default();
    let cfg = RoutingConfig::default();
    let mut candidates = Vec::new();
    for (i, windows) in [
        vec![window("5h", json!(10), 2), window("7d", json!(10), 20)],
        vec![window("5h", json!(20), 2), window("7d", json!(10), 22)],
        vec![window("5h", json!(10), 1), window("7d", json!(10), 24)],
        vec![window("5h", json!(10), 2)],
        vec![window("7d", json!(10), 20)],
        vec![window("5h", Value::Null, 2), window("7d", json!(10), 20)],
        vec![],
    ]
    .into_iter()
    .enumerate()
    {
        let mut c = candidate(Provider::Claude, windows);
        c.account.id = format!("account-{i}");
        candidates.push(c);
    }
    // Reversed projected lexical order must not alter the original tie order.
    let order: Vec<String> = (0..candidates.len())
        .rev()
        .map(|i| format!("hash-{i}"))
        .collect();
    for (i, a) in candidates.iter().enumerate() {
        for (j, b) in candidates.iter().enumerate() {
            let x = result(a, &d, &raw(&d, cfg.clone()));
            let y = result(b, &d, &raw(&d, cfg.clone()));
            for anchor in [
                None,
                Some(now() + Duration::hours(19)),
                Some(now() + Duration::hours(21)),
            ] {
                assert_eq!(
                    compare_in_order(&order, &order[i], &x, &order[j], &y, anchor),
                    Some(native::compare_gated(
                        a,
                        &native::metrics(a, &d, &cfg, now()),
                        b,
                        &native::metrics(b, &d, &cfg, now()),
                        anchor
                    ))
                );
            }
        }
    }
}

// The convenience account map deliberately contains only the first row. Keep
// the ordered rows separate to catch any accidental same-ID substitution.
fn selection_context(table: &BindingTable, d: &Data, rows: &[Candidate]) -> DecisionInputs {
    let mut inputs = native::kernel_shell::candidate_inputs(d, &rows[0], None, None, now());
    let rows: Vec<_> = rows.iter().map(raw_candidate).collect();
    inputs.selection = Some(Box::new(super::super::SelectionInputs {
        entry: super::super::selection::SelectionEntry::Ordered,
        provider: rows[0].account.provider,
        parent: None,
        pinned: None,
        hold_allowed: true,
        preference: None,
        root_hint: None,
        candidates: rows.clone(),
        source_accounts: rows,
        account_order: vec!["a".into()],
        affinity: super::super::selection_affinity::AffinityInput {
            bindings: BTreeMap::new(),
            groups: BTreeMap::new(),
            binding_count: 0,
            lru: Vec::new(),
            active: d.active.keys().cloned().collect(),
            pins: BTreeSet::new(),
            launch_retention: BTreeMap::new(),
            launch_anchors: BTreeMap::new(),
            session_binding_max_secs: table.config.session_binding_max_secs,
        },
        metrics: raw(d, table.config.clone()),
        policy: table.policies.selection_inputs(None, &["a".into()]),
        launches: BTreeMap::new(),
        launch_aliases: BTreeSet::new(),
        unknown_cost: BTreeSet::new(),
        last_request_bytes: BTreeMap::new(),
        outside_unknown: false,
        outside_used: BTreeSet::new(),
        entitled_targets: None,
        return_guard: None,
    }));
    inputs
}

#[test]
fn missing_source_candidate_still_reads_activity_reservations_and_thresholds() {
    let root = native::tests::fixture_dir();
    let cfg = RoutingConfig {
        spread_threshold: 7,
        ..RoutingConfig::default()
    };
    let table = BindingTable::new(root.path(), cfg).unwrap();
    let c = candidate(Provider::Claude, vec![window("5h", json!(10), 2)]);
    let mut d = Data::default();
    d.accounts.insert(
        "old-anchor".into(),
        AccountIndex {
            recent: [(now(), "old-root".into())].into_iter().collect(),
            ..AccountIndex::default()
        },
    );
    d.overload_marks.insert(
        "reserved-root".into(),
        native::OverloadMark {
            expires: now() + Duration::seconds(1),
            from: "a".into(),
            target: "old-anchor".into(),
        },
    );
    table.set_account_threshold("old-anchor", Some(4));
    let mut inputs = selection_context(&table, &d, std::slice::from_ref(&c));
    inputs
        .selection
        .as_mut()
        .unwrap()
        .metrics
        .thresholds
        .insert("old-anchor".into(), 4);
    assert!(!inputs.accounts.contains_key("old-anchor"));
    let selection = inputs.selection.as_ref().unwrap();
    assert!(
        selection
            .candidates
            .iter()
            .chain(&selection.source_accounts)
            .all(|c| c.account.id != "old-anchor")
    );
    for id in ["old-anchor", "absent-everywhere"] {
        assert_eq!(
            active_sessions(&inputs, id),
            Some(table.active_sessions_in(&d, id, now()))
        );
        assert_eq!(
            spread_threshold(&inputs, id),
            Some(table.spread_threshold_for(id))
        );
        for except in [None, Some("reserved-root")] {
            assert_eq!(
                reserved(&inputs, id, except),
                Some(table.reserved_in(&d, id, except, now()))
            );
        }
    }
    assert_eq!(active_sessions(&inputs, "old-anchor"), Some(1));
    assert_eq!(reserved(&inputs, "old-anchor", None), Some(1));
    assert_eq!(spread_threshold(&inputs, "old-anchor"), Some(4));
    assert_eq!(active_sessions(&inputs, "absent-everywhere"), Some(0));
    assert_eq!(reserved(&inputs, "absent-everywhere", None), Some(0));
    assert_eq!(spread_threshold(&inputs, "absent-everywhere"), Some(7));
    // Missing sources still supply native zero to the balance calculation.
    for except in [None, Some("absent")] {
        assert_eq!(
            rebalance_target_waiting_for(&inputs, "absent-everywhere", &raw_candidate(&c), except),
            Some(table.rebalance_target_waiting(&d, "absent-everywhere", &c, except, now()))
        );
    }
    inputs.selection = None;
    assert_eq!(active_sessions(&inputs, "old-anchor"), None);
    assert_eq!(reserved(&inputs, "old-anchor", None), None);
    assert_eq!(spread_threshold(&inputs, "old-anchor"), None);
}

#[test]
fn duplicate_id_rows_keep_their_own_quota_inflight_and_placement_evidence() {
    let root = native::tests::fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    let first = candidate(Provider::Claude, vec![window("5h", json!(10), 2)]);
    let mut second = candidate(Provider::Claude, vec![window("5h", json!(40), 2)]);
    second.inflight = 3;
    second.account.health = crate::accounts::Health::NeedsRelogin {
        reason: "synthetic-row-health".into(),
        since: now(),
    };
    let d = Data::default();
    let inputs = selection_context(&table, &d, &[first.clone(), second.clone()]);
    let first_raw = raw_candidate(&first);
    let second_raw = raw_candidate(&second);
    let a = metrics_for(&inputs, &first_raw, false).unwrap();
    let b = metrics_for(&inputs, &second_raw, false).unwrap();
    assert_eq!((a.usage, b.usage), (10.0, 43.0));
    assert_metrics(&a, &native::metrics(&first, &d, &table.config, now()));
    assert_metrics(&b, &native::metrics(&second, &d, &table.config, now()));
    assert_eq!(metrics(&inputs, "a").unwrap().usage, a.usage);
    assert_eq!(
        rebalance_target_waiting_for(&inputs, "missing-source", &first_raw, Some("absent")),
        Some(true)
    );
    assert_eq!(
        rebalance_target_waiting_for(&inputs, "missing-source", &second_raw, Some("absent")),
        Some(false)
    );
    for c in [&first, &second] {
        assert_eq!(
            rebalance_target_waiting_for(
                &inputs,
                "missing-source",
                &raw_candidate(c),
                Some("absent")
            ),
            Some(table.rebalance_target_waiting(&d, "missing-source", c, Some("absent"), now()))
        );
    }

    assert_metrics(
        &metrics_for(&inputs, &second_raw, true).unwrap(),
        &native::metrics_without_placement_charge(&second, &d, &table.config, now()),
    );
    // Same quota/ID, different inflight: the stale-window query must see row 2.
    second.quota.as_mut().unwrap()["windows"][0]["observed_at"] =
        json!(now() - Duration::seconds(901));
    let mut old_first = first.clone();
    old_first.quota = second.quota.clone();
    let inputs = selection_context(&table, &d, &[old_first.clone(), second.clone()]);
    assert!(
        !metrics_for(&inputs, &raw_candidate(&old_first), false)
            .unwrap()
            .unknown
    );
    let stale = metrics_for(&inputs, &raw_candidate(&second), false).unwrap();
    assert!(stale.unknown);
    assert_metrics(&stale, &native::metrics(&second, &d, &table.config, now()));
    // Missing observed_at changes placement evidence for just this row.
    second.quota.as_mut().unwrap()["windows"][0]["observed_at"] = Value::Null;
    for c in [&first, &second] {
        let row = raw_candidate(c);
        assert_eq!(
            placement_evidence_for(&inputs, &row),
            Some(native::placement_evidence(c, &d, now()))
        );
        assert_eq!(
            placement_capped_for(&inputs, &row),
            Some(native::placement_capped(c, &d, &table.config, now()))
        );
        for except in [None, Some("absent")] {
            assert_eq!(
                rebalance_target_waiting_for(&inputs, "old-anchor", &row, except),
                Some(table.rebalance_target_waiting(&d, "old-anchor", c, except, now()))
            );
            assert_eq!(
                rebalance_target_base_for(&inputs, "old-anchor", &row, except),
                Some(table.rebalance_target_base(&d, "old-anchor", c, except, now()))
            );
            assert_eq!(
                rebalance_target_for(&inputs, "old-anchor", &row, except),
                Some(table.rebalance_target(&d, "old-anchor", c, except, now()))
            );
        }
    }
    assert_eq!(
        placement_evidence_for(&inputs, &first_raw).unwrap().0,
        PlacementEvidence::Fresh
    );
    assert_eq!(
        placement_evidence_for(&inputs, &raw_candidate(&second))
            .unwrap()
            .0,
        PlacementEvidence::Weak
    );
}
