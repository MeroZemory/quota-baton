use super::*;
use serde_json::json;

fn now() -> DateTime<Utc> {
    parse_timestamp("2026-11-11T10:00:00Z").unwrap()
}

fn row(id: &str) -> Value {
    json!({"id": id, "resets_left": 1, "usable_now": true,
        "ends_at": "2026-12-01T00:00:00Z", "clears":["five_hour"]})
}

fn inventory(rows: Vec<Value>) -> ResetCreditInventory {
    parse_details(
        &json!({"cedar_ember": {"eligible": true, "grants": rows}}),
        now(),
    )
    .unwrap()
}

#[test]
fn absent_null_and_malformed_status_are_unknown_never_zero() {
    for value in [
        json!({}),
        json!({"cedar_ember": null}),
        json!({"cedar_ember": {}}),
        json!({"cedar_ember": {"eligible": "true"}}),
    ] {
        let inv = parse_details(&value, now()).unwrap();
        assert_eq!(inv, ResetCreditInventory::default());
        assert_eq!(choose(&inv, None, now()), Err(Error::Unknown));
    }
    assert_eq!(parse_details(&Value::Null, now()), Err(Error::Malformed));
}

#[test]
fn zero_and_ineligible_are_distinct_and_keep_provider_reason() {
    for grants in [
        json!([]),
        json!(null),
        json!("wrong"),
        json!([{"id":"zero", "resets_left":0}]),
    ] {
        let inv = parse_details(
            &json!({"cedar_ember":{"eligible":true,"grants":grants}}),
            now(),
        )
        .unwrap();
        assert_eq!(inv.state, ResetCreditState::Fresh);
        assert_eq!(inv.available_count, Some(0));
        assert_eq!(inv.refusal, Some(Error::NoCredits));
    }
    let inv = parse_details(&json!({"cedar_ember":{"eligible":false,"ineligible_reason":"surface", "grants":[row("a")]}}), now()).unwrap();
    assert_eq!(inv.refusal, Some(Error::Ineligible));
    assert_eq!(
        inv.claude.as_ref().unwrap().ineligible_reason.as_deref(),
        Some("surface")
    );
    assert_eq!(choose(&inv, Some("a"), now()), Err(Error::Ineligible));
    let zero = parse_details(
        &json!({"cedar_ember":{"eligible":true,
        "cooldown_until":"2026-11-12T00:00:00Z", "grants":[]}}),
        now(),
    )
    .unwrap();
    assert_eq!(zero.refusal, Some(Error::NoCredits));
}

#[test]
fn native_defaults_and_dropping_preserve_safe_facts() {
    let inv = inventory(vec![
        json!(null),
        json!({"id":"BAD", "resets_left":1}),
        json!({"id":"negative", "resets_left":-1}),
        json!({"id":"fractional", "resets_left":1.5}),
        json!({"id":"missing"}),
        json!({"id":"safe", "resets_left":3, "resets_total":1,
            "label":"Fable from provider", "type":"future-kind",
            "clears":["seven_day_omelette", "future", 1, "five_hour", "five_hour"],
            "blocking":["future"], "unavailable_reason":"future",
            "percent_used":{"five_hour":100,"seven_day":0,"seven_day_opus":0.5,
                "seven_day_sonnet":101,"seven_day_cowork":-1,
                "seven_day_oauth_apps":"10","seven_day_omelette":false,"future":5}}),
    ]);
    assert_eq!(inv.available_count, Some(3));
    let facts = inv.claude.as_ref().unwrap();
    let g = &facts.grants[0];
    assert_eq!(facts.grants.len(), 1);
    assert_eq!(g.label, "Fable from provider");
    assert_eq!(g.resets_total, Some(1)); // Native imposes no left <= total rule.
    assert_eq!(g.grant_type, None);
    assert_eq!(g.clears, ["seven_day_omelette", "five_hour", "five_hour"]);
    assert!(g.blocking.is_empty());
    assert_eq!(g.percent_used.len(), 2);
    assert_eq!(g.percent_used["five_hour"], serde_json::Number::from(100));
    assert!(!g.percent_used.contains_key("seven_day_opus"));
    assert!(!g.usable_now);
    assert!(!g.paused);
    assert!(g.use_requires_limit);
    assert!(!g.gives_back_weekly);
    assert_eq!(g.unavailable_reason, None);
    assert_eq!(choose(&inv, None, now()), Err(Error::NotUsable));
    let defaults = inventory(vec![json!({"id":"defaults", "resets_left":1})]);
    assert_eq!(defaults.claude.unwrap().grants[0].resets_total, Some(0));
}

#[test]
fn next_is_a_recommendation_explicit_choice_never_substitutes() {
    let mut early = row("early");
    early["ends_at"] = json!("2026-11-12T00:00:00Z");
    let inv = parse_details(
        &json!({"cedar_ember":{"eligible":true,
        "next_grant_id":"later", "grants":[early, row("later")]}}),
        now(),
    )
    .unwrap();
    assert_eq!(choose(&inv, None, now()).unwrap().id, "later");
    assert_eq!(super::super::choose(&inv, now()).unwrap().id, "later");
    assert_eq!(choose(&inv, Some("early"), now()).unwrap().id, "early");
    assert_eq!(choose(&inv, Some("missing"), now()), Err(Error::NotUsable));
    let mut blocked = inv.clone();
    blocked.claude.as_mut().unwrap().grants[1].usable_now = false;
    assert_eq!(choose(&blocked, None, now()).unwrap().id, "early");
    assert_eq!(
        choose(&blocked, Some("later"), now()),
        Err(Error::NotUsable)
    );
}

#[test]
fn fallback_orders_valid_expiry_then_stable_id_then_never() {
    let mut never = row("a-never");
    never["ends_at"] = Value::Null;
    let inv = inventory(vec![never, row("z"), row("a")]);
    assert_eq!(choose(&inv, None, now()).unwrap().id, "a");
    assert_eq!(choose(&inv, Some("a-never"), now()).unwrap().id, "a-never");
    let inv = parse_details(
        &json!({"cedar_ember":{"eligible":true,
        "next_grant_id":"not-listed", "grants":[row("a")]}}),
        now(),
    )
    .unwrap();
    assert_eq!(inv.claude.unwrap().next_grant_id, None);
}

#[test]
fn invalid_expiry_strings_and_expired_dates_cannot_authorize() {
    for (expiry, expected) in [
        (json!("not-a-date"), Error::UnknownExpiry),
        (json!("2026-11-11T10:00:00Z"), Error::NotUsable),
    ] {
        let mut g = row("a");
        g["ends_at"] = expiry;
        let inv = inventory(vec![g]);
        assert_eq!(inv.grants.len(), 1);
        assert_eq!(choose(&inv, Some("a"), now()), Err(expected));
    }
    let mut future = row("future");
    future["starts_at"] = json!("2026-11-12T00:00:00Z");
    assert_eq!(
        choose(&inventory(vec![future]), None, now()),
        Err(Error::NotUsable)
    );
    let mut invalid = row("invalid");
    invalid["starts_at"] = json!("unknown");
    assert_eq!(
        choose(&inventory(vec![invalid]), None, now()),
        Err(Error::NotUsable)
    );
}

#[test]
fn native_null_date_normalization_allows_otherwise_usable_grants() {
    for expiry in [
        None,
        Some(Value::Null),
        Some(json!(23)),
        Some(json!({"invalid":true})),
    ] {
        let mut g = row("a");
        g.as_object_mut().unwrap().remove("ends_at");
        if let Some(expiry) = expiry {
            g["ends_at"] = expiry;
        }
        g.as_object_mut().unwrap().remove("starts_at");
        let inv = inventory(vec![g]);
        let parsed = &inv.claude.as_ref().unwrap().grants[0];
        assert_eq!(parsed.ends_at, None);
        assert_eq!(parsed.starts_at, None);
        assert_eq!(inv.grants[0].expires_at, ResetCreditExpiry::Never);
        assert_eq!(inv.available_count, Some(1));
        assert_eq!(inv.refusal, None);
        assert_eq!(choose(&inv, Some("a"), now()).unwrap().id, "a");
    }
}

#[test]
fn unusable_paused_blocked_and_cooling_grants_are_refused() {
    for (key, value) in [
        ("usable_now", json!(false)),
        ("paused", json!(true)),
        ("blocking", json!(["five_hour"])),
        ("unavailable_reason", json!("needs_limit")),
    ] {
        let mut g = row("a");
        g[key] = value;
        assert_eq!(
            choose(&inventory(vec![g]), Some("a"), now()),
            Err(Error::NotUsable)
        );
    }
    for until in ["2026-11-11T11:00:00Z", "invalid"] {
        let inv = parse_details(
            &json!({"cedar_ember":{"eligible":true,
            "cooldown_until":until, "grants":[row("a")]}}),
            now(),
        )
        .unwrap();
        assert_eq!(choose(&inv, None, now()), Err(Error::NotUsable));
    }
    // An independent client-side at_limit inference must not override usable_now.
    let inv = parse_details(
        &json!({"cedar_ember":{"eligible":true,
        "at_limit":false,"cooldown_until":"2026-11-11T09:00:00Z", "grants":[row("a")]}}),
        now(),
    )
    .unwrap();
    assert!(choose(&inv, None, now()).is_ok());
}

#[test]
fn duplicate_ids_overflow_and_bounds_are_safe() {
    assert_eq!(
        parse_details(
            &json!({"cedar_ember":{"eligible":true,"grants":[row("a"),row("a")]}}),
            now()
        ),
        Err(Error::DuplicateIds)
    );
    let mut max = row("max");
    max["resets_left"] = json!(u64::MAX);
    assert_eq!(
        parse_details(
            &json!({"cedar_ember":{"eligible":true,"grants":[max,row("b")]}}),
            now()
        ),
        Err(Error::Malformed)
    );
    let rows = vec![row("a"); MAX_ROWS + 1];
    assert_eq!(
        parse_details(
            &json!({"cedar_ember":{"eligible":true,"grants":rows}}),
            now()
        ),
        Err(Error::Malformed)
    );
    let mut oversized = row("oversized");
    oversized["label"] = json!("x".repeat(257));
    assert!(inventory(vec![oversized]).grants.is_empty());
    let mut oversized = row("oversized");
    oversized["ends_at"] = json!("x".repeat(129));
    assert!(inventory(vec![oversized]).grants.is_empty());
}

#[test]
fn cached_facts_recheck_time_duplicates_and_common_grant_consistency() {
    let inv = inventory(vec![row("a")]);
    let later = now() + chrono::Duration::seconds(INVENTORY_TTL_SECONDS);
    assert_eq!(choose(&inv, Some("a"), later), Err(Error::Stale));
    let mut duplicate = inv.clone();
    duplicate.grants.push(duplicate.grants[0].clone());
    duplicate
        .claude
        .as_mut()
        .unwrap()
        .grants
        .push(inv.claude.as_ref().unwrap().grants[0].clone());
    assert_eq!(choose(&duplicate, None, now()), Err(Error::DuplicateIds));
    let mut mismatch = inv.clone();
    mismatch.grants[0].id = "different".into();
    assert_eq!(choose(&mismatch, Some("a"), now()), Err(Error::Incomplete));
    let mut mismatch = inv;
    mismatch.grants[0].expires_at = ResetCreditExpiry::Never;
    assert_eq!(
        choose(&mismatch, Some("a"), now()),
        Err(Error::UnknownExpiry)
    );
}

#[test]
fn response_normalization_matches_native_without_negative_proof() {
    for result in RESULTS {
        let value = parse_result(&json!({"result":result,"resets_left":0,
            "cleared":["five_hour","future","five_hour"],
            "weekly_resets_at":"native date string", "cooldown_until":"another string"}))
        .unwrap();
        assert_eq!(value.result, *result);
        assert_eq!(value.resets_left, Some(0));
        assert_eq!(value.cleared, ["five_hour", "five_hour"]);
        assert_eq!(
            value.weekly_resets_at.as_deref(),
            Some("native date string")
        );
        assert_eq!(value.cooldown_until.as_deref(), Some("another string"));
    }
    for value in [
        json!({}),
        json!({"result":null}),
        json!({"result":"future"}),
        json!({"result":42}),
    ] {
        assert_eq!(parse_result(&value).unwrap().result, "unavailable");
    }
    for value in [json!(-1), json!(1.5), json!("2"), Value::Null] {
        let parsed = parse_result(&json!({"reason":"needs_limit", "resets_left":value})).unwrap();
        assert_eq!(parsed.resets_left, None);
        assert_eq!(parsed.reason.as_deref(), Some("unknown"));
    }
    assert_eq!(
        parse_result(&json!({"reason":"not_limited"}))
            .unwrap()
            .reason
            .as_deref(),
        Some("not_limited")
    );
    assert_eq!(parse_result(&json!({"reason":null})).unwrap().reason, None);
    assert_eq!(parse_result(&Value::Null), Err(Error::Malformed));
    assert_eq!(
        parse_result(&json!({"weekly_resets_at":"x".repeat(129)})),
        Err(Error::Malformed)
    );
}

#[test]
fn native_integer_numbers_accept_exponents_without_saturation() {
    let value: Value = serde_json::from_str(r#"{"result":"reset","resets_left":1e2}"#).unwrap();
    assert_eq!(parse_result(&value).unwrap().resets_left, Some(100));
    let inv = inventory(vec![json!({"id":"a", "resets_left":1.0,
        "percent_used":{"five_hour":50.0}})]);
    assert_eq!(inv.available_count, Some(1));
    assert_eq!(inv.claude.unwrap().grants[0].percent_used.len(), 1);
    for value in [json!(18_446_744_073_709_551_616.0), json!(-1.0), json!(0.5)] {
        assert_eq!(integer(&value), None);
    }
    assert_eq!(integer(&json!(u64::MAX)), Some(u64::MAX));
}

#[test]
fn fractional_percentages_are_dropped_and_integer_bounds_are_inclusive() {
    let mut g = row("a");
    g["percent_used"] = json!({"five_hour":12.375,"seven_day":0.0,
        "seven_day_opus":100.0,"seven_day_sonnet":100.01,
        "seven_day_cowork":-0.01,"seven_day_omelette":"25.5",
        "seven_day_oauth_apps":null,"future":50.5});
    let inv = inventory(vec![g]);
    let map = &inv.claude.as_ref().unwrap().grants[0].percent_used;
    assert_eq!(map.len(), 2);
    assert!(!map.contains_key("five_hour"));
    assert_eq!(map["seven_day"], serde_json::Number::from_f64(0.0).unwrap());
    assert_eq!(
        map["seven_day_opus"],
        serde_json::Number::from_f64(100.0).unwrap()
    );
    assert!(choose(&inv, None, now()).is_ok());
}

#[test]
fn at_limit_native_default_never_overrides_provider_authorization() {
    for (value, expected) in [
        (Value::Null, Some(false)),
        (json!("true"), Some(false)),
        (json!(false), Some(false)),
        (json!(true), Some(true)),
    ] {
        let inv = parse_details(
            &json!({"cedar_ember":{"eligible":true,
            "at_limit":value,"grants":[row("a")]}}),
            now(),
        )
        .unwrap();
        assert_eq!(inv.claude.as_ref().unwrap().at_limit, expected);
        assert!(choose(&inv, Some("a"), now()).is_ok());
    }
    let missing = inventory(vec![row("a")]);
    assert_eq!(missing.claude.as_ref().unwrap().at_limit, Some(false));
    assert!(choose(&missing, None, now()).is_ok());
}

#[test]
fn unknown_or_missing_effect_refuses_without_losing_provider_count() {
    for clears in [
        json!(["future", 42, null]),
        json!([]),
        Value::Null,
        json!("five_hour"),
    ] {
        let mut g = row("a");
        g["clears"] = clears;
        let inv = inventory(vec![g]);
        assert_eq!(inv.available_count, Some(1));
        assert_eq!(inv.grants.len(), 1);
        assert!(inv.claude.as_ref().unwrap().grants[0].clears.is_empty());
        assert_eq!(inv.refusal, Some(Error::NotUsable));
        assert_eq!(choose(&inv, Some("a"), now()), Err(Error::NotUsable));
    }
    let mut g = row("missing");
    g.as_object_mut().unwrap().remove("clears");
    assert_eq!(
        choose(&inventory(vec![g]), None, now()),
        Err(Error::NotUsable)
    );
    let mut cached = inventory(vec![row("cached")]);
    cached.claude.as_mut().unwrap().grants[0].clears = vec!["future".into()];
    assert_eq!(choose(&cached, None, now()), Err(Error::NotUsable));
    let mut unknown = row("unknown-effect");
    unknown["clears"] = json!(["future"]);
    let mixed = parse_details(
        &json!({"cedar_ember":{"eligible":true,
        "next_grant_id":"unknown-effect", "grants":[unknown,row("known-effect")]}}),
        now(),
    )
    .unwrap();
    assert_eq!(mixed.available_count, Some(2));
    assert_eq!(choose(&mixed, None, now()).unwrap().id, "known-effect");
    assert_eq!(
        choose(&mixed, Some("unknown-effect"), now()),
        Err(Error::NotUsable)
    );
}

#[test]
fn optional_defaults_match_schema_but_never_invent_usable_now() {
    for invalid in [Value::Null, json!("true"), json!(1)] {
        let mut g = row("a");
        g["usable_now"] = invalid.clone();
        assert_eq!(
            choose(&inventory(vec![g]), None, now()),
            Err(Error::NotUsable)
        );
    }
    let mut g = row("a");
    g.as_object_mut().unwrap().remove("usable_now");
    assert_eq!(
        choose(&inventory(vec![g]), None, now()),
        Err(Error::NotUsable)
    );

    // These catch/defaults are prescribed by the native schema. They are not
    // independent authorization: usable_now, known effect and expiry still hold.
    let mut g = row("a");
    for key in [
        "paused",
        "use_requires_limit",
        "gives_back_weekly",
        "resets_total",
        "starts_at",
        "type",
        "blocking",
        "unavailable_reason",
    ] {
        g[key] = json!({"invalid":true});
    }
    let inv = inventory(vec![g]);
    let parsed = &inv.claude.as_ref().unwrap().grants[0];
    assert!(!parsed.paused);
    assert!(parsed.use_requires_limit);
    assert!(!parsed.gives_back_weekly);
    assert_eq!(parsed.resets_total, Some(0));
    assert_eq!(parsed.starts_at, None);
    assert_eq!(parsed.grant_type, None);
    assert_eq!(parsed.unavailable_reason, None);
    assert!(parsed.blocking.is_empty());
    assert!(choose(&inv, None, now()).is_ok());
}
