//! Differential tests call the original private bindings helpers, not a second
//! transcription of their rules. All fixture identities are synthetic.
use super::*;
use crate::accounts::{HealthReason, Provider, UserState};
use crate::bindings::{self as native, BindingTable, Candidate, Data, Group, RoutingConfig};
use chrono::Duration;
use serde_json::json;

fn now() -> DateTime<Utc> {
    "2026-10-22T12:01:00Z".parse().unwrap()
}

fn candidate() -> Candidate {
    let mut c = native::tests::candidate("raw-source", 0.0, 168, 5);
    c.quota = Some(json!({"windows": [{
        "name": "5h", "used_pct": 100.0,
        "observed_at": now(), "reset_at": now() + Duration::hours(1)
    }]}));
    c
}

fn inputs(c: &Candidate, d: &Data, key: Option<&str>, model: Option<&str>) -> DecisionInputs {
    DecisionInputs {
        selection: None,
        now: now(),
        model: model.map(str::to_owned),
        key: key.map(str::to_owned),
        from: Some(c.account.id.clone()),
        to: "raw-target".into(),
        accounts: BTreeMap::from([(
            c.account.id.clone(),
            CandidateInput {
                account: c.account.clone(),
                quota: c.quota.clone(),
                inflight: c.inflight,
            },
        )]),
        traffic: d.traffic.clone(),
        pending: d.pending.clone(),
        groups: d
            .groups
            .iter()
            .map(|(key, g)| {
                (
                    key.clone(),
                    AcceptanceGroup {
                        account: g.account.clone(),
                        keys: g.keys.clone(),
                    },
                )
            })
            .collect(),
        bindings: d
            .bindings
            .iter()
            .map(|(key, b)| {
                (
                    key.clone(),
                    AcceptanceBinding {
                        accepted: b.accepted,
                    },
                )
            })
            .collect(),
        accepted_unbound: d.accepted_unbound.clone(),
        sticky: d.sticky.clone(),
        outside_failure: d
            .outside_failure
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)),
        permit: None,
    }
}

fn compare(c: &Candidate, d: &Data, model: Option<&str>) -> Account {
    let input = inputs(c, d, Some("claude:raw"), model);
    let old = native::legacy_effective_account(c, d, &RoutingConfig::default(), model, now());
    assert_eq!(effective_account(&input, &c.account.id), Some(old.clone()));
    assert_eq!(
        semantic_evidence(&input, Some(&c.account.id)),
        native::legacy_semantic_evidence(Some(&old))
    );
    old
}

#[test]
fn stale_window_matches_native_at_age_and_activity_boundaries() {
    for age_ms in [-1_000, 900_000, 900_999, 901_000] {
        for traffic_delta in [None, Some(-1), Some(0), Some(1)] {
            for pending in [0, 1] {
                for inflight in [0, 1] {
                    let mut c = candidate();
                    c.inflight = inflight;
                    let observed = now() - Duration::milliseconds(age_ms);
                    let w = json!({"observed_at": observed});
                    let mut d = Data::default();
                    d.pending.insert(c.account.id.clone(), pending);
                    if let Some(delta) = traffic_delta {
                        d.traffic.insert(
                            c.account.id.clone(),
                            observed + Duration::milliseconds(delta),
                        );
                    }
                    let input = inputs(&c, &d, None, None);
                    assert_eq!(
                        stale_window(&input, &c.account.id, &w),
                        Some(native::legacy_stale_window(&c, &d, &w, now()))
                    );
                    for malformed in [
                        json!({}),
                        json!({"observed_at": 42}),
                        json!({"observed_at": "bad"}),
                    ] {
                        assert_eq!(
                            stale_window(&input, &c.account.id, &malformed),
                            Some(native::legacy_stale_window(&c, &d, &malformed, now()))
                        );
                    }
                    let expected = age_ms >= 901_000
                        && (traffic_delta.is_some_and(|v| v > 0) || pending > 0 || inflight > 0);
                    assert_eq!(stale_window(&input, &c.account.id, &w), Some(expected));
                    c.quota.as_mut().unwrap()["windows"][0]["observed_at"] = json!(observed);
                    let account = compare(&c, &d, None);
                    assert_eq!(account.eligible_at(None, now()), expected);
                }
            }
        }
    }
}

#[test]
fn raw_quota_and_health_match_native_without_healing_free_accounts() {
    let root = native::tests::fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    for provider in [Provider::Claude, Provider::Codex] {
        for plan in [None, Some("free"), Some("pro")] {
            for health in [
                Health::Ready,
                Health::PlanWithoutClaudeCode,
                Health::Cooling {
                    reason: HealthReason::Rate,
                    until: now() - Duration::seconds(1),
                    scope: "account".into(),
                },
                Health::Cooling {
                    reason: HealthReason::Quota,
                    until: now() + Duration::hours(1),
                    scope: "model:opus".into(),
                },
                Health::NeedsRelogin {
                    reason: "synthetic".into(),
                    since: now(),
                },
            ] {
                for model in [None, Some("opus"), Some("sonnet")] {
                    for quota in [
                        None,
                        Some(json!({"windows": []})),
                        candidate().quota,
                        Some(
                            json!({"signals": {"limit_reached": true, "unified_status": "rejected"}, "windows": []}),
                        ),
                    ] {
                        let mut c = candidate();
                        c.account.provider = provider;
                        c.account.plan = plan.map(str::to_owned);
                        c.account.health = health.clone();
                        c.quota = quota;
                        let old = compare(&c, &Data::default(), model);
                        let input = inputs(&c, &Data::default(), Some("claude:raw"), model);
                        assert_eq!(
                            move_source_usable(&input, &c.account.id),
                            table.legacy_move_source_usable(
                                &Data::default(),
                                "claude:raw",
                                &c,
                                model,
                                now()
                            )
                        );
                        if provider == Provider::Claude && plan == Some("free") {
                            assert!(!old.eligible_at(model, now()));
                            assert_eq!(old, c.account, "quota must not heal Free");
                        }
                    }
                }
            }
        }
    }
    let mut c = candidate();
    c.account.user_state = UserState::Paused;
    assert_eq!(compare(&c, &Data::default(), None), c.account);
    c.account.user_state = UserState::Active;
    c.account.payment_until = Some(now() + Duration::hours(1));
    assert_eq!(compare(&c, &Data::default(), None), c.account);
    c.account.payment_until = Some(now());
    assert!(!compare(&c, &Data::default(), None).eligible_at(None, now()));
    c.quota = Some(json!({"windows": [], "signals": {"unified_status": "rejected"}}));
    assert!(
        compare(&c, &Data::default(), None).eligible_at(None, now()),
        "native ignores snapshot signals in this read path"
    );
}

#[test]
fn acceptance_and_drain_match_native_for_group_and_unbound_rows() {
    let root = native::tests::fixture_dir();
    let table = BindingTable::new(root.path(), RoutingConfig::default()).unwrap();
    for key in [
        "claude:raw",
        "claude:raw:agent:child",
        "codex:raw:agent:child",
    ] {
        for accepted in [None, Some(false), Some(true)] {
            for row_present in [false, true] {
                for group_matches in [false, true] {
                    for drain in [false, true] {
                        let mut c = candidate();
                        c.quota = None;
                        c.account.drain = drain;
                        let mut d = Data::default();
                        d.groups.insert(
                            native::group(key).into(),
                            Group {
                                account: if group_matches {
                                    c.account.id.clone()
                                } else {
                                    "other".into()
                                },
                                keys: BTreeSet::from(["member".into()]),
                                last_used: now(),
                            },
                        );
                        if row_present {
                            d.bindings.insert(
                                "member".into(),
                                native::Binding::new(
                                    "member".into(),
                                    c.account.id.clone(),
                                    now(),
                                    accepted,
                                    None,
                                    None,
                                    None,
                                    false,
                                ),
                            );
                        }
                        let input = inputs(&c, &d, Some(key), None);
                        let expected = group_matches && row_present && accepted != Some(false);
                        assert_eq!(
                            accepted_in(&input, &c.account.id),
                            native::BindingTable::legacy_accepted_in(&d, Some(key), &c.account.id)
                        );
                        assert_eq!(accepted_in(&input, &c.account.id), expected);
                        assert_eq!(
                            move_source_usable(&input, &c.account.id),
                            table.legacy_move_source_usable(&d, key, &c, None, now())
                        );
                        assert_eq!(
                            move_source_usable(&input, &c.account.id),
                            !drain || expected
                        );
                    }
                }
            }
        }
    }
    for accepted in [false, true] {
        for sticky in [false, true] {
            let c = candidate();
            let mut d = Data::default();
            if accepted {
                d.accepted_unbound.insert(c.account.id.clone());
            }
            if sticky {
                d.sticky.insert("session".into(), c.account.id.clone());
            }
            let input = inputs(&c, &d, None, None);
            assert_eq!(
                accepted_in(&input, &c.account.id),
                native::BindingTable::legacy_accepted_in(&d, None, &c.account.id)
            );
            assert_eq!(accepted_in(&input, &c.account.id), accepted && sticky);
        }
    }
    // Acceptance is existential across members, including legacy None rows.
    let mut c = candidate();
    c.quota = None;
    c.account.drain = true;
    let mut d = Data::default();
    d.groups.insert(
        "claude:raw".into(),
        Group {
            account: c.account.id.clone(),
            keys: BTreeSet::from(["rejected".into(), "legacy".into(), "missing".into()]),
            last_used: now(),
        },
    );
    for (key, accepted) in [("rejected", Some(false)), ("legacy", None)] {
        d.bindings.insert(
            key.into(),
            native::Binding::new(
                key.into(),
                c.account.id.clone(),
                now(),
                accepted,
                None,
                None,
                None,
                false,
            ),
        );
    }
    let mut input = inputs(&c, &d, Some("claude:raw:agent:child"), None);
    assert!(accepted_in(&input, &c.account.id));
    assert_eq!(
        accepted_in(&input, &c.account.id),
        BindingTable::legacy_accepted_in(&d, input.key.as_deref(), &c.account.id)
    );
    let usable =
        table.legacy_move_source_usable(&d, input.key.as_deref().unwrap(), &c, None, now());
    assert_eq!(move_source_usable(&input, &c.account.id), usable);
    input.outside_failure = !input.outside_failure;
    assert_eq!(
        move_source_usable(&input, &c.account.id),
        usable,
        "this selection observation is not a raw-helper dependency"
    );
}

#[test]
fn missing_evidence_never_reads_an_owner_and_api_is_input_only() {
    let input = inputs(&candidate(), &Data::default(), None, None);
    assert_eq!(effective_account(&input, "missing"), None);
    assert_eq!(stale_window(&input, "missing", &json!({})), None);
    assert!(!move_source_usable(&input, "missing"));
    assert_eq!(
        semantic_evidence(&input, Some("missing")),
        native::legacy_semantic_evidence(None)
    );
    assert_eq!(semantic_evidence(&input, None), 0);
    let _: fn(&DecisionInputs, &str) -> Option<Account> = effective_account;
    let _: fn(&DecisionInputs, &str, &Value) -> Option<bool> = stale_window;
    let _: fn(&DecisionInputs, &str) -> bool = accepted_in;
    let _: fn(&DecisionInputs, Option<&str>) -> u64 = semantic_evidence;
    let _: fn(&DecisionInputs, &str) -> bool = move_source_usable;
}

#[test]
fn semantic_evidence_matches_native_and_excludes_poll_and_identity_noise() {
    let mut c = candidate();
    c.quota = None;
    c.account.quota_cooling = Some(crate::accounts::QuotaCooling {
        window: "5h".into(),
        reset_at: now() + Duration::hours(1),
        started_at: now(),
    });
    let d = Data::default();
    let baseline = semantic_evidence(&inputs(&c, &d, None, None), Some(&c.account.id));
    assert_eq!(baseline, native::legacy_semantic_evidence(Some(&c.account)));
    let mut noisy = c.clone();
    noisy.account.generation += 1;
    noisy.account.label = "synthetic-noise".into();
    noisy.account.email = "noise@example.invalid".into();
    noisy.account.token_meta.last_refresh_at = Some(now() + Duration::seconds(1));
    noisy.account.plan_observed_at = Some(now() + Duration::seconds(1));
    noisy.account.quota_cooling.as_mut().unwrap().started_at -= Duration::seconds(1);
    let input = inputs(&noisy, &d, None, None);
    assert_eq!(semantic_evidence(&input, Some(&noisy.account.id)), baseline);
    assert_eq!(
        semantic_evidence(&input, Some(&noisy.account.id)),
        native::legacy_semantic_evidence(Some(&noisy.account))
    );
    for field in [
        "user_state",
        "health",
        "payment",
        "drain",
        "credits",
        "reset",
        "window",
    ] {
        let mut changed = c.clone();
        match field {
            "user_state" => changed.account.user_state = UserState::Disabled,
            "health" => changed.account.health = Health::PlanWithoutClaudeCode,
            "payment" => changed.account.payment_until = Some(now() + Duration::hours(1)),
            "drain" => changed.account.drain = true,
            "credits" => changed.account.allow_credits = true,
            "reset" => {
                changed.account.quota_cooling.as_mut().unwrap().reset_at += Duration::seconds(1)
            }
            "window" => changed.account.quota_cooling.as_mut().unwrap().window = "7d".into(),
            _ => unreachable!(),
        }
        let hash = semantic_evidence(&inputs(&changed, &d, None, None), Some(&changed.account.id));
        assert_eq!(
            hash,
            native::legacy_semantic_evidence(Some(&changed.account))
        );
        assert_ne!(hash, baseline, "semantic change: {field}");
    }
}
