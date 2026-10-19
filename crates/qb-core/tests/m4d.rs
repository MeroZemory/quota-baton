//! Focused C7.1 session spreading and bounded overload tests.
use chrono::{Duration, Utc};
use qb_core::{
    accounts::{Account, Health, HealthReason, Identity, Provider, Source, TokenMeta, UserState},
    bindings::{BindingTable, Candidate, Decision, PreviewDecision},
    config::RoutingConfig,
};
use serde_json::json;

fn account(id: &str) -> Account {
    Account {
        schema: 1,
        id: id.into(),
        provider: Provider::Codex,
        label: id.into(),
        email: format!("{id}@example.invalid"),
        plan: None,
        identity: Identity::Codex {
            account_id: id.into(),
            email: format!("{id}@example.invalid"),
            user_id: None,
        },
        source: Source::Oauth,
        user_state: UserState::Active,
        health: Health::Ready,
        auth_failures: 0,
        payment_failures: 0,
        payment_until: None,
        auth_probe_pending: false,
        quota_cooling: None,
        drain: false,
        allow_credits: false,
        quota_credit_releasable: false,
        generation: 0,
        token_meta: TokenMeta::default(),
    }
}

fn candidates(ids: &[&str]) -> Vec<Candidate> {
    let reset = (Utc::now() + Duration::hours(24)).to_rfc3339();
    ids.iter()
        .map(|id| Candidate {
            account: account(id),
            quota: Some(json!({
                "windows": [
                    {"name": "primary", "used_pct": 0.0, "reset_at": reset, "observed_at": reset},
                    {"name": "secondary", "used_pct": 0.0, "reset_at": reset, "observed_at": reset}
                ]
            })),
            inflight: 0,
        })
        .collect()
}

fn table() -> (tempfile::TempDir, std::sync::Arc<BindingTable>) {
    let root = tempfile::tempdir().unwrap();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    (root, table)
}

fn table_with(config: RoutingConfig) -> (tempfile::TempDir, std::sync::Arc<BindingTable>) {
    let root = tempfile::tempdir().unwrap();
    let table = BindingTable::new(root.path(), config).unwrap();
    (root, table)
}

fn choose(
    table: &std::sync::Arc<BindingTable>,
    key: &str,
    candidates: &[Candidate],
    bytes: u64,
) -> String {
    match table.choose(
        Provider::Codex,
        Some(key),
        None,
        None,
        candidates,
        None,
        bytes,
    ) {
        Decision::Selected { account, lease, .. } => {
            drop(lease);
            account
        }
        other => panic!("expected selected account, got {other:?}"),
    }
}

fn choose_provider(
    table: &std::sync::Arc<BindingTable>,
    provider: Provider,
    key: &str,
    candidates: &[Candidate],
) -> String {
    match table.choose(provider, Some(key), None, None, candidates, None, 1) {
        Decision::Selected { account, lease, .. } => {
            drop(lease);
            account
        }
        other => panic!("expected selected account, got {other:?}"),
    }
}

#[test]
fn c7_1_placement_fills_soonest_reset_until_threshold_then_spills() {
    let (_root, table) = table();
    let candidates = candidates(&["codex:127.0.0.1:41001", "codex:127.0.0.1:41002"]);
    table.set_account_threshold("codex:127.0.0.1:41001", Some(2));
    table.set_account_threshold("codex:127.0.0.1:41002", Some(2));
    for id in ["s0", "s1", "s2", "s3"] {
        choose(&table, &format!("codex:{id}"), &candidates, 10);
    }

    assert_eq!(
        table.active_sessions("codex:127.0.0.1:41001", Utc::now()),
        2
    );
    assert_eq!(
        table.active_sessions("codex:127.0.0.1:41002", Utc::now()),
        2
    );
    assert_eq!(table.spread_threshold_for("codex:127.0.0.1:41001"), 2);
}

#[test]
fn c7_1_marked_group_moves_at_next_request_with_ledger_move_and_after_move() {
    let (_root, table) = table();
    let source = "codex:127.0.0.1:41001";
    let target = "codex:127.0.0.1:41002";
    let source_only = candidates(&[source]);
    let both = candidates(&[source, target]);
    for (key, bytes) in [("g0", 400), ("g1", 100), ("g2", 300), ("g3", 200)] {
        assert_eq!(
            choose(&table, &format!("codex:{key}"), &source_only, bytes),
            source
        );
    }
    table.set_account_threshold(source, Some(2));
    let _ = table.preview(Provider::Codex, None, None, None, &both, None, 0);

    for key in ["codex:g0", "codex:g1", "codex:g2"] {
        let (active, threshold, groups_hit, marked) = table.note_client_rate(
            Provider::Codex,
            source,
            Some(key),
            &candidates(&[source, target]),
            Utc::now(),
        );
        assert_eq!((active, threshold), (4, 2));
        if key == "codex:g2" {
            assert_eq!((groups_hit, marked), (3, 1));
        }
    }

    assert!(table.overload_marked(Some("codex:g1"), Utc::now()));
    assert_eq!(choose(&table, "codex:g1", &both, 100), target);
    assert_eq!(choose(&table, "codex:g3", &both, 200), source);
    assert_eq!(table.active_sessions(source, Utc::now()), 3);
    assert_eq!(table.active_sessions(target, Utc::now()), 1);
}

fn mark_overloaded_group(table: &std::sync::Arc<BindingTable>, source: &str, ids: &[Candidate]) {
    let source_only = candidates(&[source]);
    for (n, bytes) in [10, 20, 30, 40].into_iter().enumerate() {
        assert_eq!(
            choose(table, &format!("codex:g{n}"), &source_only, bytes),
            source
        );
    }
    table.set_account_threshold(source, Some(2));
    let _ = table.preview(Provider::Codex, None, None, None, ids, None, 0);
    for key in ["codex:g0", "codex:g1", "codex:g2"] {
        table.note_client_rate(Provider::Codex, source, Some(key), ids, Utc::now());
    }
    assert!(table.overload_marked(Some("codex:g0"), Utc::now()));
}

#[test]
fn c7_1_rebalance_never_targets_disabled_or_cooling_account() {
    for cooling in [false, true] {
        let (_root, table) = table();
        let source = "codex:z-source";
        let target = "codex:b-ready";
        let mut ids = candidates(&["codex:a-ineligible", target, source]);
        mark_overloaded_group(&table, source, &ids);
        if cooling {
            ids[0].account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: Utc::now() + Duration::hours(1),
                scope: "account".into(),
            };
        } else {
            ids[0].account.user_state = UserState::Disabled;
        }

        assert_eq!(choose(&table, "codex:g0", &ids, 10), target);
        assert_eq!(table.current(Some("codex:g0")), Some(target.into()));
        assert!(!table.overload_marked(Some("codex:g0"), Utc::now()));
        assert_eq!(table.active_sessions("codex:a-ineligible", Utc::now()), 0);
        assert_eq!(table.active_sessions(source, Utc::now()), 3);
        assert_eq!(table.active_sessions(target, Utc::now()), 1);
    }
}

#[test]
fn c7_1_rebalance_mark_cleared_when_no_eligible_account_has_room() {
    for unavailable in ["disabled", "cooling", "drained", "gate", "full"] {
        let (_root, table) = table();
        let source = "codex:z-source";
        let target = "codex:a-target";
        let mut ids = candidates(&[target, source]);
        mark_overloaded_group(&table, source, &ids);
        match unavailable {
            "disabled" => ids[0].account.user_state = UserState::Disabled,
            "cooling" => {
                ids[0].account.health = Health::Cooling {
                    reason: HealthReason::Quota,
                    until: Utc::now() + Duration::hours(1),
                    scope: "account".into(),
                };
            }
            "drained" => ids[0].account.drain = true,
            "gate" => {
                for window in ids[0].quota.as_mut().unwrap()["windows"]
                    .as_array_mut()
                    .unwrap()
                {
                    window["used_pct"] = json!(95.0);
                }
            }
            "full" => {
                table.set_account_threshold(target, Some(1));
                assert_eq!(choose(&table, "codex:target", &ids[..1], 1), target);
            }
            _ => unreachable!(),
        }

        assert_eq!(
            choose(&table, "codex:g0", &ids, 10),
            source,
            "{unavailable}"
        );
        assert_eq!(table.current(Some("codex:g0")), Some(source.into()));
        assert!(!table.overload_marked(Some("codex:g0"), Utc::now()));
        assert_eq!(table.active_sessions(source, Utc::now()), 4);
        // Restoring room must not resurrect the consumed overload mark.
        let restored = candidates(&[target, source]);
        table.set_account_threshold(target, Some(5));
        assert_eq!(choose(&table, "codex:g0", &restored, 10), source);
    }
}

#[test]
fn c7_1_rebalance_target_follows_c7_order() {
    for order in ["weekly", "five_hour", "usage", "id", "pacing"] {
        let (_root, table) = table();
        let source = "codex:z-source";
        // Start reversed so the id tie-break also tests input-order independence.
        let mut ids = candidates(&["codex:b", "codex:a", source]);
        let now = Utc::now();
        for (index, candidate) in ids[..2].iter_mut().enumerate() {
            let (weekly, five, used) = match (order, index) {
                ("weekly", 0) => (8, 4, 20.0),
                ("weekly", _) => (12, 1, 0.0),
                ("five_hour", 0) => (10, 2, 20.0),
                ("five_hour", _) => (8, 4, 0.0),
                ("usage", 0) => (24, 4, 0.0),
                ("usage", _) => (24, 4, 20.0),
                _ => (24, 4, 0.0),
            };
            candidate.quota = Some(json!({"windows": [
                {"name": "primary", "used_pct": used,
                 "reset_at": now + Duration::hours(five), "observed_at": now},
                {"name": "secondary", "used_pct": used,
                 "reset_at": now + Duration::hours(weekly), "observed_at": now}
            ]}));
        }
        // Every case must select a candidate other than the first input entry.
        if order != "id" {
            ids.swap(0, 1);
        }
        mark_overloaded_group(&table, source, &ids);
        if order == "pacing" {
            table.set_pacing("codex:a", true);
        }
        let expected = if order == "id" { "codex:a" } else { "codex:b" };
        assert_eq!(
            table.preview(Provider::Codex, None, None, None, &ids[..2], None, 0),
            PreviewDecision::Selected {
                account: expected.into()
            },
            "{order}"
        );
        assert_eq!(choose(&table, "codex:g0", &ids, 10), expected, "{order}");
        assert_eq!(table.current(Some("codex:g0")), Some(expected.into()));
        assert!(!table.overload_marked(Some("codex:g0"), Utc::now()));
        assert_eq!(table.active_sessions(source, Utc::now()), 3);
        assert_eq!(table.active_sessions(expected, Utc::now()), 1);
    }
}

#[test]
fn c7_1_target_never_exceeds_threshold_no_ping_pong() {
    let (_root, table) = table();
    let ids = candidates(&["codex:a", "codex:b"]);
    table.set_account_threshold("codex:a", Some(2));
    table.set_account_threshold("codex:b", Some(2));
    for n in 0..4 {
        choose(&table, &format!("codex:g{n}"), &ids, 1);
    }
    assert!(table.active_sessions("codex:a", Utc::now()) <= 2);
    assert!(table.active_sessions("codex:b", Utc::now()) <= 2);
}

#[test]
fn c7_1_cold_group_replaced_when_account_at_threshold() {
    let (_root, table) = table();
    let ids = candidates(&["codex:a", "codex:b"]);
    table.set_account_threshold("codex:a", Some(1));
    choose(&table, "codex:g0", &ids, 1);
    assert_eq!(choose(&table, "codex:g1", &ids, 1), "codex:b");
}

#[test]
fn c7_1_mark_cleared_when_target_room_gone_or_expired() {
    let config = RoutingConfig {
        overload_window_secs: 1,
        ..RoutingConfig::default()
    };
    let (_root, table) = table_with(config);
    let source = "codex:a";
    let target = "codex:b";
    let source_only = candidates(&[source]);
    let ids = candidates(&[source, target]);
    for key in ["codex:g0", "codex:g1", "codex:g2", "codex:g3"] {
        choose(&table, key, &source_only, 1);
    }
    table.set_account_threshold(source, Some(2));
    let _ = table.preview(Provider::Codex, None, None, None, &ids, None, 0);
    for key in ["codex:g0", "codex:g1", "codex:g2"] {
        table.note_client_rate(
            Provider::Codex,
            source,
            Some(key),
            &candidates(&[source, target]),
            Utc::now(),
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(!table.overload_marked(Some("codex:g1"), Utc::now()));
}

#[test]
fn c7_1_storm_concentrated_then_spread() {
    let (_root, table) = table();
    let source = "codex:a";
    let target = "codex:b";
    let source_only = candidates(&[source]);
    let both = candidates(&[source, target]);
    for n in 0..10 {
        choose(&table, &format!("codex:storm-{n}"), &source_only, n + 1);
    }
    table.set_account_threshold(source, Some(5));
    let _ = table.preview(Provider::Codex, None, None, None, &both, None, 0);
    let mut moved = 0;
    for key in ["codex:storm-0", "codex:storm-1", "codex:storm-2"] {
        let marked = table
            .note_client_rate(Provider::Codex, source, Some(key), &both, Utc::now())
            .3;
        moved += marked;
    }
    assert!(moved <= 2);
    for key in ["codex:storm-0", "codex:storm-1"] {
        let _ = choose(&table, key, &both, 1);
    }
    assert!(table.active_sessions(source, Utc::now()) >= 8);
    assert!(table.active_sessions(target, Utc::now()) <= 2);
}

#[test]
fn c7_1_well_spread_no_move() {
    let (_root, table) = table();
    let source = "codex:127.0.0.1:41001";
    let target = "codex:127.0.0.1:41002";
    let both = candidates(&[source, target]);
    choose(&table, "codex:source", &both, 10);
    choose(&table, "codex:target", &candidates(&[target]), 10);

    for key in ["codex:source", "codex:other", "codex:third"] {
        let (_, _, _, marked) = table.note_client_rate(
            Provider::Codex,
            source,
            Some(key),
            &candidates(&[source, target]),
            Utc::now(),
        );
        assert_eq!(marked, 0);
    }
    assert_eq!(
        table.preview(
            Provider::Codex,
            Some("codex:source"),
            None,
            None,
            &both,
            None,
            10
        ),
        PreviewDecision::Selected {
            account: source.into()
        }
    );
    assert_eq!(table.active_sessions(source, Utc::now()), 1);
    assert_eq!(table.active_sessions(target, Utc::now()), 1);
}

#[test]
fn c7_1_placement_all_full_uses_lowest_ratio_never_refuses() {
    let (_root, table) = table();
    let source = "codex:127.0.0.1:41001";
    let target = "codex:127.0.0.1:41002";
    let both = candidates(&[source, target]);
    table.set_account_threshold(source, Some(1));
    table.set_account_threshold(target, Some(1));
    choose(&table, "codex:source", &both, 10);
    choose(&table, "codex:target", &both, 10);

    let mut marked = 0;
    for key in ["codex:source", "codex:other", "codex:third"] {
        marked = table
            .note_client_rate(
                Provider::Codex,
                source,
                Some(key),
                &candidates(&[source, target]),
                Utc::now(),
            )
            .3;
    }
    assert_eq!(marked, 0);
    assert_eq!(table.active_sessions(source, Utc::now()), 1);
    assert_eq!(table.active_sessions(target, Utc::now()), 1);
}

#[test]
fn c7_1_placement_per_account_threshold_override() {
    let (_root, table) = table();
    let ids = candidates(&["codex:a", "codex:b"]);
    table.set_account_threshold("codex:a", Some(1));
    table.set_account_threshold("codex:b", Some(3));
    for n in 0..4 {
        let _ = choose(&table, &format!("codex:g{n}"), &ids, 1);
    }
    assert_eq!(table.active_sessions("codex:a", Utc::now()), 1);
    assert_eq!(table.active_sessions("codex:b", Utc::now()), 3);
}

#[test]
fn c7_1_group_counts_once_claude_agents_and_codex_root() {
    let (_root, table) = table();
    let codex = candidates(&["codex:a"]);
    choose(&table, "codex:root", &codex, 1);
    let mut claude = candidates(&["claude:a"]);
    claude[0].account.provider = Provider::Claude;
    claude[0].account.id = "claude:a".into();
    choose_provider(&table, Provider::Claude, "claude:root", &claude);
    choose_provider(&table, Provider::Claude, "claude:root:agent:one", &claude);
    assert_eq!(table.active_sessions("codex:a", Utc::now()), 1);
    assert_eq!(table.active_sessions("claude:a", Utc::now()), 1);
}

#[test]
fn c7_1_active_count_includes_inflight_and_pinned_and_expires_after_window() {
    let config = RoutingConfig {
        active_window_secs: 1,
        ..RoutingConfig::default()
    };
    let (_root, table) = table_with(config);
    let ids = candidates(&["codex:a"]);
    choose(&table, "codex:g", &ids, 1);
    let pin = table.pin(Some("codex:g"));
    assert_eq!(table.active_sessions("codex:a", Utc::now()), 1);
    drop(pin);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(table.active_sessions("codex:a", Utc::now()), 0);
}

#[test]
fn c7_1_rate_429_alone_never_moves_bound_session() {
    let (_root, table) = table();
    let ids = candidates(&["codex:a", "codex:b"]);
    choose(&table, "codex:g", &ids, 1);
    let (_, _, _, marked) = table.note_client_rate(
        Provider::Codex,
        "codex:a",
        Some("codex:g"),
        &ids,
        Utc::now(),
    );
    assert_eq!(marked, 0);
    assert_eq!(
        table.preview(Provider::Codex, Some("codex:g"), None, None, &ids, None, 1),
        PreviewDecision::Selected {
            account: "codex:a".into()
        }
    );
}

#[test]
fn c7_1_imbalance_floor_blocks_move_at_6_vs_4() {
    let (_root, table) = table();
    let source = "codex:a";
    let target = "codex:b";
    let ids = candidates(&[source, target]);
    for n in 0..6 {
        choose(&table, &format!("codex:g{n}"), &candidates(&[source]), 1);
    }
    for n in 0..4 {
        choose(&table, &format!("codex:h{n}"), &candidates(&[target]), 1);
    }
    let _ = table.preview(Provider::Codex, None, None, None, &ids, None, 0);
    for key in ["codex:g0", "codex:g1", "codex:g2"] {
        table.note_client_rate(
            Provider::Codex,
            source,
            Some(key),
            &candidates(&[source, target]),
            Utc::now(),
        );
    }
    assert!(!table.overload_marked(Some("codex:g0"), Utc::now()));
}

#[test]
fn c7_1_inflight_request_or_streaming_socket_never_victim() {
    let (_root, table) = table();
    let source = "codex:a";
    let target = "codex:b";
    let source_only = candidates(&[source]);
    let ids = candidates(&[source, target]);
    for key in ["codex:g0", "codex:g1", "codex:g2", "codex:g3"] {
        choose(&table, key, &source_only, 1);
    }
    table.set_account_threshold(source, Some(2));
    let lease = table.begin_request(Some("codex:g0"), source);
    let _ = table.preview(Provider::Codex, None, None, None, &ids, None, 0);
    for key in ["codex:g0", "codex:g1", "codex:g2"] {
        table.note_client_rate(
            Provider::Codex,
            source,
            Some(key),
            &candidates(&[source, target]),
            Utc::now(),
        );
    }
    assert!(!table.overload_marked(Some("codex:g0"), Utc::now()));
    drop(lease);
}

#[test]
fn c7_1_concentrated_marks_cheapest_groups_two_per_window() {
    let (_root, table) = table();
    let source = "codex:a";
    let target = "codex:b";
    let source_only = candidates(&[source]);
    let both = candidates(&[source, target]);
    for (n, bytes) in [10, 90, 20, 80, 30, 70, 40, 60].into_iter().enumerate() {
        assert_eq!(
            choose(&table, &format!("codex:g{n}"), &source_only, bytes),
            source
        );
    }
    table.set_account_threshold(source, Some(5));
    let _ = table.preview(Provider::Codex, None, None, None, &both, None, 0);
    assert_eq!(
        table
            .note_client_rate(Provider::Codex, source, Some("codex:g0"), &both, Utc::now())
            .3,
        0
    );
    assert_eq!(
        table
            .note_client_rate(Provider::Codex, source, Some("codex:g1"), &both, Utc::now())
            .3,
        0
    );
    let marked =
        table.note_client_rate(Provider::Codex, source, Some("codex:g2"), &both, Utc::now());
    assert_eq!(marked.2, 3);
    assert_eq!(marked.3, 2);
    assert!(table.overload_marked(Some("codex:g0"), Utc::now()));
    assert!(table.overload_marked(Some("codex:g2"), Utc::now()));
    assert_eq!(
        table
            .note_client_rate(Provider::Codex, source, Some("codex:g3"), &both, Utc::now())
            .3,
        0
    );
}

#[test]
fn c7_1_in_place_retry_and_reset_hold_are_not_boundaries() {
    let (_root, table) = table();
    let source = "codex:a";
    let target = "codex:b";
    let only_source = candidates(&[source]);
    let both = candidates(&[source, target]);
    for n in 0..6 {
        choose(&table, &format!("codex:g{n}"), &only_source, 1);
    }
    table.set_account_threshold(source, Some(5));
    let _ = table.preview(Provider::Codex, None, None, None, &both, None, 0);
    for key in ["codex:g0", "codex:g1", "codex:g2"] {
        table.note_client_rate(Provider::Codex, source, Some(key), &both, Utc::now());
    }
    assert!(table.overload_marked(Some("codex:g0"), Utc::now()));
    // C6.3 in-place/reset-hold retries reuse the reserved account and do not
    // ask the binding selector for a new placement.
    let retry = table.begin_request(Some("codex:g0"), source);
    assert_eq!(table.current(Some("codex:g0")), Some(source.to_owned()));
    drop(retry);
    assert_eq!(table.current(Some("codex:g0")), Some(source.to_owned()));
    assert_eq!(choose(&table, "codex:g0", &both, 1), target);
}

#[test]
fn c7_1_row10_and_row2_outcomes_do_not_count() {
    let (_root, table) = table();
    let source = "codex:a";
    let target = "codex:b";
    let ids = candidates(&[source, target]);
    for n in 0..6 {
        choose(&table, &format!("codex:g{n}"), &candidates(&[source]), 1);
    }
    table.set_account_threshold(source, Some(5));
    // 529, 5xx, model-capacity, and quota 429 are relay classifications that
    // deliberately never call note_client_rate; absence of a client-rate
    // trigger leaves every binding unmarked.
    for _outcome in [529, 500, 429, 429] {
        assert!(!table.overload_marked(Some("codex:g0"), Utc::now()));
    }
    assert_eq!(
        table
            .note_client_rate(Provider::Codex, source, Some("codex:g0"), &ids, Utc::now())
            .2,
        1
    );
    assert!(!table.overload_marked(Some("codex:g0"), Utc::now()));
}
