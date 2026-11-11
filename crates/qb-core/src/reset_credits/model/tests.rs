use super::*;
use serde_json::json;

#[test]
fn claude_extensions_preserve_old_wire_defaults_and_request_strictness() {
    use qb_proto::reset_credit_action::{ResetCreditPreviewRequest, ResetCreditResult};
    let inventory = ResetCreditInventory::default();
    let wire = serde_json::to_value(&inventory).unwrap();
    assert!(wire.get("claude").is_none());
    assert_eq!(
        serde_json::from_value::<ResetCreditInventory>(wire.clone())
            .unwrap()
            .claude,
        None
    );
    let request: ResetCreditPreviewRequest = serde_json::from_value(json!({})).unwrap();
    assert_eq!(request, ResetCreditPreviewRequest::default());
    assert!(serde_json::from_value::<ResetCreditPreviewRequest>(json!({"other":true})).is_err());
    let result: ResetCreditResult = serde_json::from_value(json!({
        "account":"test", "outcome":"refused", "inventory":wire, "refresh_failed":false
    }))
    .unwrap();
    assert_eq!(result.claude, None);
    assert!(
        serde_json::to_value(result)
            .unwrap()
            .get("claude")
            .is_none()
    );
}

fn now() -> DateTime<Utc> {
    parse_timestamp("2026-11-07T10:00:00Z").unwrap()
}

fn grant(id: &str, expiry: Value) -> Value {
    json!({
        "id": id, "reset_type": "codex_rate_limits", "status": "available",
        "granted_at": "2026-11-01T00:00:00Z", "expires_at": expiry,
        "title": "discard this", "description": "discard this too"
    })
}

fn details(count: u64, grants: Vec<Value>) -> ResetCreditInventory {
    parse_details(&json!({"available_count": count, "credits": grants}), now()).unwrap()
}

#[test]
fn explicit_zero_is_fresh_and_not_unknown() {
    let inventory = details(0, vec![]);
    assert_eq!(inventory.state, ResetCreditState::Fresh);
    assert_eq!(inventory.available_count, Some(0));
    assert_eq!(inventory.refusal, Some(Error::NoCredits));
    assert_eq!(choose(&inventory, now()), Err(Error::NoCredits));
}

#[test]
fn absent_and_null_summary_or_count_are_unknown_not_zero() {
    for value in [json!({}), json!({"rate_limit_reset_credits": null})] {
        assert_eq!(
            parse_summary(&value, now()).unwrap(),
            ResetCreditInventory::default()
        );
    }
    for value in [json!({}), json!({"available_count": null})] {
        let inventory = parse_details(&value, now()).unwrap();
        assert_eq!(inventory.state, ResetCreditState::Unknown);
        assert_eq!(inventory.available_count, None);
        assert_eq!(choose(&inventory, now()), Err(Error::Unknown));
    }
}

#[test]
fn count_only_summary_never_proves_positive_inventory_complete() {
    let inventory = parse_summary(
        &json!({"rate_limit_reset_credits": {"available_count": 2}}),
        now(),
    )
    .unwrap();
    assert_eq!(inventory.available_count, Some(2));
    assert_eq!(inventory.refusal, Some(Error::Incomplete));
    let zero = parse_summary(
        &json!({"rate_limit_reset_credits": {"available_count": 0}}),
        now(),
    )
    .unwrap();
    assert_eq!(zero.refusal, Some(Error::NoCredits));
    let unexpected_details = parse_summary(
        &json!({"rate_limit_reset_credits": {
            "available_count": 1, "credits": [grant("unexpected", Value::Null)]
        }}),
        now(),
    )
    .unwrap();
    assert!(unexpected_details.grants.is_empty());
    assert_eq!(choose(&unexpected_details, now()), Err(Error::Incomplete));
}

#[test]
fn malformed_counts_and_containers_reject() {
    for count in [
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(true),
        json!({}),
        json!(u64::MAX),
    ] {
        assert_eq!(
            parse_details(&json!({"available_count": count}), now()),
            Err(Error::Malformed)
        );
    }
    for value in [
        json!(null),
        json!([]),
        json!({"credits": null}),
        json!({"credits": {}}),
    ] {
        assert_eq!(parse_details(&value, now()), Err(Error::Malformed));
    }
    assert_eq!(
        parse_summary(&json!({"rate_limit_reset_credits": 1}), now()),
        Err(Error::Malformed)
    );
}

#[test]
fn earliest_is_chronological_not_input_or_lexicographic_order() {
    let inventory = details(
        3,
        vec![
            grant("late", json!("2026-11-10T00:00:00Z")),
            grant("early", json!("2026-11-08T01:00:00+02:00")),
            grant("middle", json!("2026-11-08T00:00:00Z")),
        ],
    );
    assert_eq!(choose(&inventory, now()).unwrap().id, "early");
    assert!(std::ptr::eq(
        choose(&inventory, now()).unwrap(),
        &inventory.grants[1]
    ));
}

#[test]
fn tied_expiry_has_stable_identifier_order() {
    let a = grant("a", json!("2026-11-08T00:00:00Z"));
    let b = grant("b", json!("2026-11-08T01:00:00+01:00"));
    for rows in [vec![a.clone(), b.clone()], vec![b, a]] {
        assert_eq!(choose(&details(2, rows), now()).unwrap().id, "a");
    }
}

#[test]
fn explicit_null_is_nonexpiring_and_sorts_last() {
    let inventory = details(
        2,
        vec![
            grant("a-never", Value::Null),
            grant("z-dated", json!("2026-12-01T00:00:00Z")),
        ],
    );
    assert_eq!(inventory.grants[0].expires_at, ResetCreditExpiry::Never);
    assert_eq!(choose(&inventory, now()).unwrap().id, "z-dated");
    let never = details(2, vec![grant("b", Value::Null), grant("a", Value::Null)]);
    assert_eq!(choose(&never, now()).unwrap().id, "a");
}

#[test]
fn missing_expiry_is_unknown_and_blocks_known_candidate() {
    let mut unknown = grant("unknown", Value::Null);
    unknown.as_object_mut().unwrap().remove("expires_at");
    let inventory = details(
        2,
        vec![grant("dated", json!("2026-11-08T00:00:00Z")), unknown],
    );
    assert_eq!(inventory.grants[1].expires_at, ResetCreditExpiry::Unknown);
    assert_eq!(choose(&inventory, now()), Err(Error::UnknownExpiry));
}

#[test]
fn malformed_expiry_and_granted_dates_reject_including_numeric_epochs() {
    for field in ["expires_at", "granted_at"] {
        for value in [json!(0), json!("0"), json!("bad"), json!(true), json!({})] {
            let mut row = grant("a", Value::Null);
            row[field] = value;
            assert_eq!(
                parse_details(&json!({"available_count":1,"credits":[row]}), now()),
                Err(Error::Malformed)
            );
        }
    }
    for value in [Value::Null, json!("2026-11-08T00:00:00Z")] {
        let mut row = grant("a", Value::Null);
        row["granted_at"] = value;
        assert_eq!(
            parse_details(&json!({"available_count":1,"credits":[row]}), now()),
            Err(Error::Malformed)
        );
    }
}

#[test]
fn malformed_required_facts_reject() {
    for field in ["id", "status", "reset_type", "granted_at"] {
        let mut row = grant("a", Value::Null);
        row.as_object_mut().unwrap().remove(field);
        assert_eq!(
            parse_details(&json!({"available_count":1,"credits":[row]}), now()),
            Err(Error::Malformed)
        );
    }
    for id in ["", "  ", "a\nb"] {
        assert_eq!(
            parse_details(
                &json!({"available_count":1,"credits":[grant(id, Value::Null)]}),
                now()
            ),
            Err(Error::Malformed)
        );
    }
}

#[test]
fn duplicate_identifiers_reject_even_used_or_expired_rows() {
    let available = grant("same", Value::Null);
    let mut used = available.clone();
    used["status"] = json!("redeemed");
    for other in [used, grant("same", json!("2026-11-06T00:00:00Z"))] {
        assert_eq!(
            parse_details(
                &json!({"available_count":1,"credits":[available.clone(),other]}),
                now()
            ),
            Err(Error::DuplicateIds)
        );
    }
}

#[test]
fn used_redeeming_and_expired_rows_are_excluded_from_selectable_count() {
    let mut used = grant("used", Value::Null);
    used["status"] = json!("redeemed");
    let mut redeeming = grant("redeeming", Value::Null);
    redeeming["status"] = json!("redeeming");
    let rows = vec![
        used,
        redeeming,
        grant("expired", json!("2026-11-07T09:00:00Z")),
        grant("at-now", json!("2026-11-07T10:00:00Z")),
        grant("available", Value::Null),
    ];
    assert_eq!(
        choose(&details(1, rows.clone()), now()).unwrap().id,
        "available"
    );
    assert_eq!(choose(&details(5, rows), now()), Err(Error::Incomplete));
}

#[test]
fn unknown_eligibility_blocks_potentially_available_grants() {
    for field in ["status", "reset_type"] {
        let mut unknown = grant("unknown", Value::Null);
        unknown[field] = json!("future-provider-value");
        let inventory = details(2, vec![grant("known", Value::Null), unknown]);
        assert_eq!(choose(&inventory, now()), Err(Error::UnknownEligibility));
    }
}

#[test]
fn definitely_ineligible_unknown_facts_do_not_block_other_grants() {
    let mut used = grant("used", Value::Null);
    used["status"] = json!("redeemed");
    used["reset_type"] = json!("future-type");
    used.as_object_mut().unwrap().remove("expires_at");
    let mut expired = grant("expired", json!("2026-11-06T00:00:00Z"));
    expired["status"] = json!("future-status");
    assert_eq!(
        choose(
            &details(1, vec![used, expired, grant("known", Value::Null)]),
            now()
        )
        .unwrap()
        .id,
        "known"
    );
}

#[test]
fn capped_and_inconsistent_details_refuse_in_both_directions() {
    for count in [0, 2, 100] {
        assert_eq!(
            choose(&details(count, vec![grant("a", Value::Null)]), now()),
            Err(Error::Incomplete)
        );
    }
    assert_eq!(choose(&details(1, vec![]), now()), Err(Error::Incomplete));
}

#[test]
fn stale_unknown_unsupported_and_future_observation_refuse_without_losing_facts() {
    let fresh = details(1, vec![grant("a", Value::Null)]);
    for (state, error) in [
        (ResetCreditState::Stale, Error::Stale),
        (ResetCreditState::Unknown, Error::Unknown),
        (ResetCreditState::Unsupported, Error::Unsupported),
    ] {
        let mut inventory = fresh.clone();
        inventory.state = state;
        assert_eq!(choose(&inventory, now()), Err(error));
        assert_eq!(inventory.available_count, Some(1));
        assert_eq!(inventory.grants, fresh.grants);
    }
    let mut future = fresh;
    future.observed_at = Some("2026-11-08T00:00:00Z".into());
    assert_eq!(choose(&future, now()), Err(Error::Stale));
}

#[test]
fn selection_rechecks_expiry_and_untrusted_dto_facts() {
    let inventory = details(1, vec![grant("a", json!("2026-11-07T10:01:00Z"))]);
    assert!(choose(&inventory, now()).is_ok());
    assert_eq!(
        choose(&inventory, parse_timestamp("2026-11-07T10:01:00Z").unwrap()),
        Err(Error::Incomplete)
    );
    let mut duplicate = inventory.clone();
    duplicate.grants.push(duplicate.grants[0].clone());
    assert_eq!(choose(&duplicate, now()), Err(Error::DuplicateIds));
    let mut malformed = inventory;
    malformed.grants[0].expires_at = ResetCreditExpiry::At { at: "bad".into() };
    assert_eq!(choose(&malformed, now()), Err(Error::Malformed));
}

#[test]
fn inventory_freshness_expires_at_five_minute_boundary() {
    let inventory = details(1, vec![grant("a", Value::Null)]);
    assert!(choose(&inventory, now() + chrono::Duration::seconds(299)).is_ok());
    assert!(choose(&inventory, now() + chrono::Duration::milliseconds(299_999)).is_ok());
    for seconds in [300, 301] {
        assert_eq!(
            choose(&inventory, now() + chrono::Duration::seconds(seconds)),
            Err(Error::Stale)
        );
    }
    assert_eq!(inventory.state, ResetCreditState::Fresh);
    assert_eq!(inventory.available_count, Some(1));
}

#[test]
fn parser_does_not_mutate_input_or_retain_paid_credits_windows_or_display_text() {
    let input = json!({"available_count":1,"credits":[grant("a", Value::Null)],
        "paid_credits":{"balance":42,"allow_credits":true},
        "rate_limit":{"primary_window":{"used_percent":87}}});
    let before = input.clone();
    let inventory = parse_details(&input, now()).unwrap();
    assert_eq!(input, before);
    let wire = serde_json::to_value(&inventory).unwrap();
    assert!(wire.get("paid_credits").is_none());
    assert!(wire.get("rate_limit").is_none());
    assert!(wire["grants"][0].get("title").is_none());
    assert!(wire["grants"][0].get("description").is_none());
    assert_eq!(wire["grants"][0]["expires_at"], json!({"type":"never"}));
    assert_eq!(
        serde_json::from_value::<ResetCreditInventory>(wire).unwrap(),
        inventory
    );
}
