//! Focused C7.1 session spreading and bounded overload tests.
use chrono::{Duration, Utc};
use qb_core::{
    accounts::{Account, Health, Identity, Provider, Source, TokenMeta, UserState},
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
        auth_probe_pending: false,
        quota_cooling: None,
        drain: false,
        allow_credits: false,
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
