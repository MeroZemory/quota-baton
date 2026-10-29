use super::*;
use crate::accounts::{HealthReason, ImportMode, SourceMatch};
use crate::bindings::kernel::permit::{IncidentInput, MovementInput, SoleTargetInput};
use crate::bindings::move_guard::Limits;
use chrono::Duration;
use serde_json::json;

fn now() -> DateTime<Utc> {
    "2026-10-22T12:01:00Z".parse().unwrap()
}

fn account(id: &str) -> Account {
    let mut a = AccountFacts {
        id: id.into(),
        generation: 0,
        provider: Provider::Claude,
        plan: Some(Plan::Pro),
        user_state: UserState::Active,
        health: HealthFacts::Ready,
        auth_failures: 3,
        payment_failures: 2,
        payment_until: None,
        auth_probe_pending: true,
        quota_cooling: None,
        quota_credit_releasable: false,
        drain: false,
        allow_credits: false,
    }
    .account()
    .unwrap();
    a.label = "private-label-canary".into();
    a.email = "private-email-canary@example.test".into();
    a.identity = Identity::Claude {
        account_uuid: "private-uuid-canary".into(),
        organization_uuid: Some("private-organization-canary".into()),
        email: a.email.clone(),
    };
    a.source = Source::Cpa {
        mode: ImportMode::Linked,
        dir: "C:/private-path-canary".into(),
        r#match: SourceMatch::Claude {
            account_uuid: "private-match-canary".into(),
            organization_uuid: None,
            email: a.email.clone(),
        },
    };
    a.token_meta.expires_at = Some(now() + Duration::hours(9));
    a.generation = 987654321;
    a
}

fn inputs() -> DecisionInputs {
    let source = "private-source-id";
    let target = "private-target-id";
    let root = "claude:private-root-id";
    let child = "claude:private-root-id:agent:private-child-id";
    DecisionInputs {
        selection: None,
        now: now(),
        model: Some("opus".into()),
        key: Some(child.into()),
        from: Some(source.into()),
        to: target.into(),
        accounts: [(source, account(source)), (target, account(target))]
            .into_iter()
            .map(|(id, a)| {
                (
                    id.into(),
                    CandidateInput {
                        account: a,
                        quota: None,
                        inflight: 0,
                    },
                )
            })
            .collect(),
        traffic: BTreeMap::from([(source.into(), now() - Duration::seconds(1))]),
        pending: BTreeMap::from([(source.into(), 0)]),
        groups: BTreeMap::from([(
            root.into(),
            AcceptanceGroup {
                account: source.into(),
                keys: BTreeSet::from([child.into()]),
            },
        )]),
        bindings: BTreeMap::from([(
            child.into(),
            AcceptanceBinding {
                accepted: Some(true),
            },
        )]),
        accepted_unbound: BTreeSet::new(),
        sticky: BTreeMap::new(),
        outside_failure: true,
        permit: Some(PermitInputs {
            limits: Limits::default(),
            now: now(),
            history: vec![],
            global: vec![],
            session_count: 1,
            session_exists: true,
            tripped_until: None,
            revision: 7,
            source_observation_age_ns: Some(10),
            target_observation_age_ns: Some(20),
            sole_target: None,
        }),
    }
}

fn capture_input(input: &DecisionInputs) -> PermitRecord {
    let mut ids = BTreeMap::new();
    let actual = kernel::decide_move(input);
    capture(input, &actual, |kind, raw| {
        let next = ids.len();
        ids.entry((kind, raw.to_owned()))
            .or_insert_with(|| format!("p{next}"))
            .clone()
    })
    .unwrap()
}

#[test]
fn serialized_record_excludes_identity_paths_and_arbitrary_quota_payloads() {
    let mut input = inputs();
    input.accounts.get_mut("private-source-id").unwrap().quota = Some(json!({
        "windows": [{"name":"5h", "used_pct": 10.25, "observed_at":now(),
            "reset_at":now()+Duration::hours(1), "payload":"private-window-canary"}],
        "credits":{"has_credits":null, "balance":12, "payload":"private-credit-canary"},
        "signals":{"reason":"private-signal-canary"}, "payload":"private-quota-canary"
    }));
    let record = capture_input(&input);
    let text = serde_json::to_string(&record).unwrap();
    for forbidden in [
        "private-",
        "example.test",
        "token_meta",
        "subscription",
        "identity",
        "email",
    ] {
        assert!(!text.contains(forbidden), "leaked {forbidden}");
    }
    let restored: PermitRecord = serde_json::from_str(&text).unwrap();
    assert_eq!(replay(&restored).unwrap(), restored.actual);
    assert_eq!(record.schema, SCHEMA);
    assert_eq!(record.ordinal, 0);
}

#[test]
fn accepted_child_keeps_native_root_group_and_every_cross_reference() {
    let mut input = inputs();
    input
        .accounts
        .get_mut("private-source-id")
        .unwrap()
        .account
        .drain = true;
    input.accepted_unbound.insert("private-source-id".into());
    input.sticky.insert(
        "claude:private-root-id:agent:another-child".into(),
        "private-source-id".into(),
    );
    let record = capture_input(&input);
    let wire = record.inputs.as_ref().unwrap().to_inputs().unwrap();
    let source = wire.from.as_deref().unwrap();
    let child = wire.key.as_deref().unwrap();
    let root = child.split_once(":agent:").unwrap().0;
    assert_eq!(wire.groups[root].account, source);
    assert!(wire.groups[root].keys.contains(child));
    assert!(wire.bindings.contains_key(child));
    assert!(wire.traffic.contains_key(source));
    assert!(wire.pending.contains_key(source));
    assert!(wire.accepted_unbound.contains(source));
    assert!(wire.sticky.values().any(|s| s == source));
    assert!(kernel::accepted_in(&input, "private-source-id"));
    assert!(kernel::accepted_in(&wire, source));
    assert!(kernel::move_source_usable(&wire, source));
    assert_eq!(replay(&record).unwrap(), record.actual);
    if let PermitDecision::Allowed(p) = &record.actual {
        assert_eq!(p.from, source);
        assert_eq!(p.to, wire.to);
        assert_eq!(p.key, child);
    } else {
        panic!("fixture must allow the move");
    }
    let mut unbound = wire;
    unbound.key = None;
    assert!(kernel::accepted_in(
        &unbound,
        unbound.from.as_deref().unwrap()
    ));
}

#[test]
fn native_and_wire_preserve_raw_health_quota_free_rule_and_staleness() {
    for provider in [Provider::Claude, Provider::Codex] {
        for plan in [None, Some("free"), Some("pro")] {
            for health in [
                Health::Ready,
                Health::PlanWithoutClaudeCode,
                Health::Cooling {
                    reason: HealthReason::Quota,
                    until: now() + Duration::hours(1),
                    scope: "model:sonnet".into(),
                },
                Health::Cooling {
                    reason: HealthReason::Rate,
                    until: now() - Duration::seconds(1),
                    scope: "account".into(),
                },
                Health::NeedsRelogin {
                    reason: "invalid_grant".into(),
                    since: now(),
                },
            ] {
                for age in [0, 900, 901] {
                    for pending in [0, 1] {
                        let mut input = inputs();
                        let c = input.accounts.get_mut("private-source-id").unwrap();
                        c.account.provider = provider;
                        c.account.plan = plan.map(str::to_owned);
                        c.account.health = health.clone();
                        c.account.quota_cooling = Some(QuotaCooling {
                            window: "5h".into(),
                            reset_at: now() + Duration::hours(1),
                            started_at: now() - Duration::minutes(3),
                        });
                        c.quota = Some(json!({"windows":[{"name":"5h", "used_pct":100,
                            "reset_at":now()+Duration::hours(1), "observed_at":now()-Duration::seconds(age)}],
                            "credits":{"has_credits":null,"unlimited":false,"balance":null}}));
                        input.pending.insert("private-source-id".into(), pending);
                        let record = capture_input(&input);
                        let wire = record.inputs.as_ref().unwrap().to_inputs().unwrap();
                        let id = wire.from.as_deref().unwrap();
                        assert_eq!(wire.accounts[id].account.health, health);
                        assert_eq!(wire.outside_failure, input.outside_failure);
                        assert_eq!(
                            kernel::semantic_evidence(&input, input.from.as_deref()),
                            kernel::semantic_evidence(&wire, Some(id))
                        );
                        assert_eq!(
                            kernel::move_source_usable(&input, "private-source-id"),
                            kernel::move_source_usable(&wire, id)
                        );
                        assert_eq!(replay(&record).unwrap(), record.actual);
                        if provider == Provider::Claude && plan == Some("free") {
                            assert!(
                                !kernel::effective_account(&wire, id)
                                    .unwrap()
                                    .eligible_at(Some("opus"), now())
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn history_evidence_incidents_sole_target_and_observation_order_are_exact() {
    let mut input = inputs();
    input
        .accounts
        .get_mut("private-source-id")
        .unwrap()
        .account
        .user_state = UserState::Disabled;
    let departure = kernel::semantic_evidence(&input, input.from.as_deref());
    let target_evidence = kernel::semantic_evidence(&input, Some(&input.to));
    let p = input.permit.as_mut().unwrap();
    p.history = vec![MovementInput {
        at: now() - Duration::minutes(1),
        committed_age_ns: 100,
        from: "private-target-id".into(),
        to: "private-source-id".into(),
        departure_evidence: target_evidence,
        destination_evidence: departure.wrapping_add(1),
        emergency_return: false,
    }];
    p.global = vec![IncidentInput {
        at: now(),
        shared: Some(("private-source-id".into(), departure)),
    }];
    p.sole_target = Some(SoleTargetInput {
        from: "private-source-id".into(),
        to: "private-target-id".into(),
        model: input.model.clone(),
        at: now(),
    });
    // Strict native age ordering: smaller age means a later observation.
    for age in [99, 100, 101] {
        input.permit.as_mut().unwrap().source_observation_age_ns = Some(age);
        let record = capture_input(&input);
        let wire = record.inputs.as_ref().unwrap().to_inputs().unwrap();
        let p = wire.permit.as_ref().unwrap();
        assert_eq!(p.history[0].departure_evidence, target_evidence);
        assert_eq!(p.history[0].destination_evidence, departure.wrapping_add(1));
        assert_eq!(p.history[0].committed_age_ns, 100);
        assert_eq!(p.source_observation_age_ns, Some(age));
        assert_eq!(
            p.global[0].shared,
            Some((wire.from.clone().unwrap(), departure))
        );
        assert_eq!(
            p.sole_target.as_ref().unwrap().from,
            wire.from.clone().unwrap()
        );
        assert_eq!(p.sole_target.as_ref().unwrap().to, wire.to);
        assert_eq!(replay(&record).unwrap(), record.actual);
        match &record.actual {
            PermitDecision::Allowed(p) if age == 99 => {
                assert!(p.emergency_return);
                assert_eq!(p.shared_incident, Some((wire.from.unwrap(), departure)));
            }
            PermitDecision::Denied(p) if age >= 100 => {
                assert_eq!(p.reason, crate::bindings::move_guard::Refusal::PingPong)
            }
            _ => panic!("unexpected observation-order decision"),
        }
    }
}

#[test]
fn absent_null_and_numeric_quota_facts_are_not_filled() {
    for quota in [
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(json!({"windows":null})),
        Some(json!({"windows":[{"name":null,"used_pct":null,"observed_at":null}],"credits":null})),
        Some(
            json!({"windows":[{"name":"5h","used_pct":99.125}],"credits":{"has_credits":false,"balance":0}}),
        ),
    ] {
        let mut input = inputs();
        input.accounts.get_mut("private-source-id").unwrap().quota = quota.clone();
        let record = capture_input(&input);
        let wire = record.inputs.as_ref().unwrap().to_inputs().unwrap();
        assert_eq!(wire.accounts[wire.from.as_deref().unwrap()].quota, quota);
        assert_eq!(replay(&record).unwrap(), record.actual);
    }
}

#[test]
fn unsafe_health_unknown_models_and_mapper_errors_refuse_without_raw_text() {
    let mut input = inputs();
    input
        .accounts
        .get_mut("private-source-id")
        .unwrap()
        .account
        .health = Health::NeedsRelogin {
        reason: "private-email-canary@example.test".into(),
        since: now(),
    };
    assert_eq!(
        capture(&input, &PermitDecision::NotRequired, |_, _| "x".into()).unwrap_err(),
        WireRefusal::Health
    );
    input
        .accounts
        .get_mut("private-source-id")
        .unwrap()
        .account
        .health = Health::Cooling {
        reason: HealthReason::Quota,
        until: now(),
        scope: "model:private-model-canary".into(),
    };
    assert_eq!(
        capture(&input, &PermitDecision::NotRequired, |_, _| "x".into()).unwrap_err(),
        WireRefusal::Health
    );
    input = inputs();
    input.model = Some("private-model-canary".into());
    let mut n = 0;
    assert_eq!(
        capture(&input, &PermitDecision::NotRequired, |_, _| {
            n += 1;
            format!("x{n}")
        })
        .unwrap_err(),
        WireRefusal::Model
    );
    input = inputs();
    assert_eq!(
        capture(&input, &PermitDecision::NotRequired, |_, _| "collision"
            .into())
        .unwrap_err(),
        WireRefusal::Collision
    );
    assert_eq!(
        capture(&input, &PermitDecision::NotRequired, |_, _| {
            "private@example.test".into()
        })
        .unwrap_err(),
        WireRefusal::Identity
    );
}

#[test]
fn reader_refuses_unsupported_schema_and_health_vocabulary() {
    let mut record = capture_input(&inputs());
    record.schema += 1;
    assert_eq!(replay(&record).unwrap_err(), WireRefusal::Schema);
    record.schema = SCHEMA;
    let source = record.inputs.as_ref().unwrap().from.clone().unwrap();
    let mut value = serde_json::to_value(&record).unwrap();
    value["inputs"]["accounts"][&source]["account"]["health"] = json!({
        "kind":"needs_relogin", "reason":"private-health-canary", "since":now()
    });
    assert!(serde_json::from_value::<PermitRecord>(value).is_err());
}

#[test]
fn refused_record_retains_mapped_actual_and_replays_as_explicit_refusal() {
    let input = inputs();
    let actual = kernel::decide_move(&input);
    let mut ids = BTreeMap::new();
    let mut record = refused_record(&actual, WireRefusal::Health, |kind, raw| {
        let next = ids.len();
        ids.entry((kind, raw.to_owned()))
            .or_insert_with(|| format!("p{next}"))
            .clone()
    })
    .unwrap();
    assert!(record.inputs.is_none());
    assert_eq!(record.refusal, Some(WireRefusal::Health));
    assert!(matches!(record.actual, PermitDecision::Allowed(_)));
    assert!(!serde_json::to_string(&record).unwrap().contains("private-"));
    assert_eq!(replay(&record).unwrap_err(), WireRefusal::Health);
    record.refusal = None;
    assert_eq!(replay(&record).unwrap_err(), WireRefusal::Shape);
}

#[test]
fn free_with_expired_raw_cooling_and_absent_quota_stays_native_unusable() {
    let mut input = inputs();
    let a = &mut input.accounts.get_mut("private-source-id").unwrap().account;
    a.plan = Some("free".into());
    a.health = Health::Cooling {
        reason: HealthReason::Quota,
        until: now() - Duration::seconds(1),
        scope: "account".into(),
    };
    let record = capture_input(&input);
    let restored = record.inputs.as_ref().unwrap().to_inputs().unwrap();
    let source = restored.from.as_deref().unwrap();
    assert_eq!(restored.accounts[source].quota, None);
    assert_eq!(
        restored.accounts[source].account.plan.as_deref(),
        Some("free")
    );
    assert_eq!(
        restored.accounts[source].account.health,
        input.accounts["private-source-id"].account.health
    );
    assert!(!kernel::move_source_usable(&restored, source));
    assert_eq!(
        kernel::semantic_evidence(&restored, Some(source)),
        kernel::semantic_evidence(&input, input.from.as_deref())
    );
    assert_eq!(replay(&record).unwrap(), record.actual);
}

#[test]
fn unsupported_quota_shapes_and_oversized_read_sets_refuse_explicitly() {
    for (quota, expected) in [
        (
            json!({"windows":[{"name":"private-window"}]}),
            WireRefusal::QuotaWindowName,
        ),
        (
            json!({"windows":[{"name":"5h", "used_pct":"private-number"}]}),
            WireRefusal::QuotaWindowUsed,
        ),
        (
            json!({"windows":[{"observed_at":"private-date"}]}),
            WireRefusal::QuotaWindowObserved,
        ),
        (json!({"windows":{}}), WireRefusal::QuotaWindows),
        (json!([]), WireRefusal::QuotaRoot),
        (json!({"windows":[false]}), WireRefusal::QuotaWindow),
        (
            json!({"windows":[{"status":"private-status"}]}),
            WireRefusal::QuotaWindowStatus,
        ),
        (
            json!({"windows":[{"reset_at":"private-date"}]}),
            WireRefusal::QuotaWindowReset,
        ),
        (json!({"credits":false}), WireRefusal::QuotaCredits),
        (
            json!({"credits":{"has_credits":"private-flag"}}),
            WireRefusal::QuotaCreditFlag,
        ),
        (
            json!({"credits":{"balance":"private-balance"}}),
            WireRefusal::QuotaCreditBalance,
        ),
        (
            json!({"paid_serving_observed_at":"private-date"}),
            WireRefusal::QuotaPaidServing,
        ),
    ] {
        let mut input = inputs();
        input.accounts.get_mut("private-source-id").unwrap().quota = Some(quota);
        let mut ids = BTreeMap::new();
        // Deliberately malformed quota is not a valid input to the native
        // evaluator. Exercise the wire refusal independently of evaluation.
        let error = capture(&input, &PermitDecision::NotRequired, |kind, raw| {
            let next = ids.len();
            ids.entry((kind, raw.to_owned()))
                .or_insert_with(|| format!("p{next}"))
                .clone()
        })
        .unwrap_err();
        assert_eq!(error, expected);
        assert!(!serde_json::to_string(&error).unwrap().contains("private-"));
    }
    let mut input = inputs();
    input.bindings = (0..=MAX_ROWS)
        .map(|n| (format!("session{n}"), AcceptanceBinding { accepted: None }))
        .collect();
    assert_eq!(
        capture(&input, &PermitDecision::NotRequired, |_, _| panic!(
            "must bound before mapping"
        ))
        .unwrap_err(),
        WireRefusal::Bounds
    );
    assert_eq!(
        bounded(&"x".repeat(MAX_BYTES)).unwrap_err(),
        WireRefusal::Bounds
    );
}

#[test]
fn literal_native_duration_and_pinned_model_names_preserve_semantic_evidence() {
    for name in [
        "0m",
        "43200m",
        "1s",
        "61s",
        "00060m",
        "18446744073709551615s",
    ] {
        for model in ["gpt-5.4-codex", "gpt-6-luna", "claude-opus-4-6-20261029"] {
            let mut input = inputs();
            input.model = Some(model.into());
            let c = input.accounts.get_mut("private-source-id").unwrap();
            c.account.health = Health::Cooling {
                reason: HealthReason::Quota,
                until: now() + Duration::hours(1),
                scope: format!("model:{model}"),
            };
            c.account.quota_cooling = Some(QuotaCooling {
                window: name.into(),
                reset_at: now() + Duration::hours(1),
                started_at: now(),
            });
            c.quota = Some(json!({"windows":[{"name":name, "used_pct":100}]}));
            let record = capture_input(&input);
            let restored = record.inputs.as_ref().unwrap().to_inputs().unwrap();
            let source = restored.from.as_deref().unwrap();
            assert_eq!(restored.model.as_deref(), Some(model));
            assert_eq!(
                restored.accounts[source].quota.as_ref().unwrap()["windows"][0]["name"],
                name
            );
            assert_eq!(
                restored.accounts[source]
                    .account
                    .quota_cooling
                    .as_ref()
                    .unwrap()
                    .window,
                name
            );
            assert_eq!(
                restored.accounts[source].account.health,
                input.accounts["private-source-id"].account.health
            );
            assert_eq!(
                kernel::semantic_evidence(&restored, Some(source)),
                kernel::semantic_evidence(&input, input.from.as_deref())
            );
        }
    }
    for text in [
        "gpt-5.4-private-person",
        "gpt-5.4-user@example.test",
        "claude-opus-",
        "private model",
    ] {
        assert!(Model::parse(text).is_err());
    }
    for text in ["18446744073709551616m", "private-window", "5h_private"] {
        assert!(Window::parse(text).is_err());
    }
}

fn quota_record() -> PermitRecord {
    let mut input = inputs();
    for c in input.accounts.values_mut() {
        c.quota = Some(
            json!({"windows":[{"name":"43200m", "used_pct":12.5, "observed_at":now()}], "credits":{"balance":null}}),
        );
    }
    capture_input(&input)
}

#[test]
fn quota_interning_deduplicates_only_published_account_revision_content_keys() {
    let mut record = quota_record();
    let original = record.inputs.as_ref().unwrap().to_inputs().unwrap();
    let wire = record.inputs.as_mut().unwrap();
    let snapshots = wire.intern_quotas(&BTreeSet::new()).unwrap();
    assert_eq!(snapshots.len(), 2); // Same content across accounts is separately bound.
    assert_ne!(snapshots[0].digest, snapshots[1].digest);
    assert_eq!(wire.to_inputs().unwrap_err(), WireRefusal::QuotaReference);
    assert_eq!(
        wire.intern_quotas(&BTreeSet::new()).unwrap_err(),
        WireRefusal::QuotaReference
    );
    let dictionary: BTreeMap<_, _> = snapshots.into_iter().map(|s| (s.key(), s)).collect();
    let seen = dictionary.keys().cloned().collect();
    assert!(wire.intern_quotas(&seen).unwrap().is_empty());
    wire.resolve_quotas(&dictionary).unwrap();
    let restored = wire.to_inputs().unwrap();
    for (id, c) in &original.accounts {
        assert_eq!(
            restored.accounts[id].account.generation,
            c.account.generation
        );
        assert_eq!(restored.accounts[id].quota, c.quota);
    }
    assert_eq!(replay(&record).unwrap(), record.actual);
    let wire = record.inputs.as_mut().unwrap();
    assert!(wire.intern_quotas(&seen).unwrap().is_empty());
    wire.resolve_quotas(&dictionary).unwrap();
    let c = wire.accounts.values_mut().next().unwrap();
    c.revision += 1;
    c.account.generation += 1;
    let fresh = wire.intern_quotas(&seen).unwrap();
    assert_eq!(fresh.len(), 1);
    assert!(!seen.contains(&fresh[0].key()));
}

#[test]
fn quota_resolution_rejects_missing_or_tampered_binding_and_content_atomically() {
    let mut record = quota_record();
    let wire = record.inputs.as_mut().unwrap();
    let snapshots = wire.intern_quotas(&BTreeSet::new()).unwrap();
    let dictionary: BTreeMap<_, _> = snapshots.into_iter().map(|s| (s.key(), s)).collect();
    let before = serde_json::to_vec(wire).unwrap();
    for defect in 0..5 {
        let mut bad = dictionary.clone();
        let key = bad.keys().next_back().unwrap().clone();
        match defect {
            0 => {
                bad.remove(&key);
            }
            1 => bad.get_mut(&key).unwrap().account = "a:other".into(),
            2 => bad.get_mut(&key).unwrap().revision += 1,
            3 => bad.get_mut(&key).unwrap().digest.0[0] ^= 1,
            _ => bad.get_mut(&key).unwrap().quota.windows = Field::Null,
        }
        assert_eq!(
            wire.resolve_quotas(&bad).unwrap_err(),
            WireRefusal::QuotaReference
        );
        assert_eq!(serde_json::to_vec(&*wire).unwrap(), before);
    }
    wire.resolve_quotas(&dictionary).unwrap();
    assert_eq!(replay(&record).unwrap(), record.actual);
}

#[test]
fn validator_accepts_retained_refusals_but_rejects_malformed_actual_ids() {
    let mut record = capture_input(&inputs());
    record.inputs = None;
    record.refusal = Some(WireRefusal::Health);
    assert!(validate_record(&record).is_ok());
    assert_eq!(replay(&record).unwrap_err(), WireRefusal::Health);
    for field in 0..4 {
        let mut malformed = record.clone();
        let PermitDecision::Allowed(actual) = &mut malformed.actual else {
            panic!("allowed fixture");
        };
        match field {
            0 => actual.key = "private-raw-session".into(),
            1 => actual.from = "private-raw-source".into(),
            2 => actual.to = "private-raw-target".into(),
            _ => actual.shared_incident = Some(("private-raw-account".into(), 42)),
        }
        assert_eq!(
            validate_record(&malformed).unwrap_err(),
            WireRefusal::Identity
        );
        assert_eq!(replay(&malformed).unwrap_err(), WireRefusal::Identity);
    }
    let mut comparable = capture_input(&inputs());
    comparable.actual = PermitDecision::NotRequired;
    assert!(validate_record(&comparable).is_ok()); // Validation is not comparison.
    assert_ne!(replay(&comparable).unwrap(), comparable.actual);
    comparable.refusal = Some(WireRefusal::Health);
    assert_eq!(
        validate_record(&comparable).unwrap_err(),
        WireRefusal::Shape
    );
    let mut referenced = quota_record();
    referenced
        .inputs
        .as_mut()
        .unwrap()
        .intern_quotas(&BTreeSet::new())
        .unwrap();
    assert_eq!(
        validate_record(&referenced).unwrap_err(),
        WireRefusal::QuotaReference
    );
}

#[test]
fn native_codex_boolean_header_status_survives_wire_without_becoming_rejection() {
    for reached in [false, true] {
        let home = crate::bindings::tests::fixture_dir();
        let obs = crate::observability::Observability::new(home.path(), None).unwrap();
        let mut headers = axum::http::HeaderMap::new();
        for (key, value) in [
            ("x-codex-primary-window-minutes", "300".to_owned()),
            ("x-codex-primary-used-percent", "25".to_owned()),
            ("x-codex-primary-reset-after-seconds", "3600".to_owned()),
            ("x-codex-secondary-window-minutes", "0".to_owned()),
            ("x-codex-secondary-used-percent", "0".to_owned()),
            ("x-codex-limit-reached", reached.to_string()),
        ] {
            headers.insert(key, value.parse().unwrap());
        }
        let native = obs
            .observe_headers_snapshot("cx-native-status", Provider::Codex, &headers)
            .unwrap();
        assert_eq!(native["signals"]["limit_reached"], reached);
        assert_eq!(native["windows"][0]["status"], reached.to_string());
        assert_eq!(native["windows"][1]["name"], "0m");
        let projected = quota(&native).expect("native boolean status is a valid raw fact");
        let restored_windows = projected.value()["windows"].clone();
        let native_windows = native["windows"].as_array().unwrap();
        let restored_windows = restored_windows.as_array().unwrap();
        assert_eq!(restored_windows.len(), native_windows.len());
        for (native, restored) in native_windows.iter().zip(restored_windows) {
            for field in ["name", "used_pct", "status"] {
                assert_eq!(restored[field], native[field]);
            }
            // Date facts preserve the exact instant and precision; Z and
            // +00:00 are equivalent RFC3339 spellings of the same UTC value.
            for field in ["reset_at", "observed_at"] {
                let parsed = |value: &Value| {
                    value
                        .as_str()
                        .map(|s| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc))
                };
                assert_eq!(parsed(&restored[field]), parsed(&native[field]));
                assert_eq!(restored[field].is_null(), native[field].is_null());
            }
        }

        let mut input = inputs();
        input.now = Utc::now();
        input.model = None;
        input.permit.as_mut().unwrap().now = input.now;
        let c = input.accounts.get_mut("private-source-id").unwrap();
        c.account.provider = Provider::Codex;
        c.account.plan = Some("prolite".into());
        c.quota = Some(native);
        let native_health = kernel::effective_account(&input, "private-source-id")
            .unwrap()
            .health;
        let record = capture_input(&input);
        let decoded = record.inputs.as_ref().unwrap().to_inputs().unwrap();
        let source = decoded.from.as_deref().unwrap();
        assert_eq!(
            decoded.accounts[source].quota.as_ref().unwrap()["windows"][0]["status"],
            reached.to_string()
        );
        assert_eq!(
            kernel::effective_account(&decoded, source).unwrap().health,
            native_health
        );
        assert_eq!(replay(&record).unwrap(), record.actual);
        assert!(record.refusal.is_none());
    }
}
