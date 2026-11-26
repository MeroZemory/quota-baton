use super::*;

#[test]
fn claude_complete_requires_explicit_account_inventory_and_valid_numeric_windows() {
    let complete =
        json!({"five_hour":null,"seven_day":{"utilization":51,"resets_at":"2099-11-07T00:00:00Z"}});
    assert!(is_complete_snapshot(
        Provider::Claude,
        &serde_json::to_vec(&complete).unwrap()
    ));
    for malformed in [
        json!({}),
        json!(false),
        json!({"utilization":51}),
        json!({"utilization":-1,"resets_at":"2099-11-07T00:00:00Z"}),
        json!({"utilization":"NaN","resets_at":"2099-11-07T00:00:00Z"}),
        json!({"utilization":51,"resets_at":"invalid"}),
    ] {
        let mut body = complete.clone();
        body["five_hour"] = malformed;
        assert!(
            !is_complete_snapshot(Provider::Claude, &serde_json::to_vec(&body).unwrap()),
            "{body}"
        );
    }
    for missing in ["five_hour", "seven_day"] {
        let mut body = complete.clone();
        body.as_object_mut().unwrap().remove(missing);
        assert!(!is_complete_snapshot(
            Provider::Claude,
            &serde_json::to_vec(&body).unwrap()
        ));
    }
}

#[test]
fn claude_complete_accepts_unstarted_zero_utilization_window_with_null_resets_at() {
    for body in [
        json!({
            "five_hour": {"utilization": 0, "resets_at": null},
            "seven_day": {"utilization": 0.0, "resets_at": null}
        }),
        json!({
            "five_hour": {"utilization": 0.0, "resets_at": "2099-11-07T00:00:00Z"},
            "seven_day": {"utilization": 0.0, "resets_at": null}
        }),
        json!({
            "five_hour": {"utilization": 0.0, "reset_at": null},
            "seven_day": {"utilization": 45.0, "resets_at": "2099-11-07T00:00:00Z"}
        }),
        json!({
            "limits": [
                {"kind": "session", "percent": 0.0, "resets_at": null, "scope": null},
                {"kind": "weekly_all", "percent": 12.0, "resets_at": "2099-11-07T00:00:00Z", "scope": null}
            ]
        }),
    ] {
        assert!(
            is_complete_snapshot(Provider::Claude, &serde_json::to_vec(&body).unwrap()),
            "{body}"
        );
    }
    for malformed in [
        json!({"utilization": 0}),
        json!({"utilization": 0, "resets_at": "invalid"}),
        json!({"utilization": 0, "resets_at": null, "reset_at": "invalid"}),
        json!({"utilization": 0, "resets_at": null, "reset_after_seconds": -1}),
        json!({"utilization": 0.1, "resets_at": null}),
        json!({"utilization": 51, "resets_at": null}),
    ] {
        let body = json!({
            "five_hour": {"utilization": 0.0, "resets_at": "2099-11-07T00:00:00Z"},
            "seven_day": malformed
        });
        assert!(
            !is_complete_snapshot(Provider::Claude, &serde_json::to_vec(&body).unwrap()),
            "{body}"
        );
    }
}

#[test]
fn r18_explicit_pro_categories_are_retained_without_numeric_tier_inference() {
    for tier in ["pro", "prolite", "promax"] {
        let mut body = report();
        body["plan_type"] = json!(tier);
        let raw = serde_json::to_vec(&body).unwrap();
        assert!(is_complete_snapshot(Provider::Codex, &raw));
        let snapshot = snapshot(Provider::Codex, &raw, Utc::now()).unwrap();
        assert_eq!(snapshot["plan"], tier);
        let event: crate::dashboard::history::PlanName = tier.to_owned().try_into().unwrap();
        assert_eq!(event.as_str(), tier);
    }
    assert!(plan(&json!("pro_10x")).is_none());
}

fn report() -> Value {
    json!({"plan_type":"pro", "rate_limit":{
        "limit_reached":false,
        "primary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_after_seconds":3600},
        "secondary_window":null
    }})
}

#[test]
fn complete_requires_explicit_slot_inventory_and_each_window_field() {
    let complete = report();
    assert!(is_complete_snapshot(
        Provider::Codex,
        &serde_json::to_vec(&complete).unwrap()
    ));
    for field in [
        "used_percent",
        "limit_window_seconds",
        "reset_after_seconds",
    ] {
        let mut partial = complete.clone();
        partial["rate_limit"]["primary_window"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            !is_complete_snapshot(Provider::Codex, &serde_json::to_vec(&partial).unwrap()),
            "missing {field}"
        );
    }
    for slot in ["primary_window", "secondary_window"] {
        let mut partial = complete.clone();
        partial["rate_limit"].as_object_mut().unwrap().remove(slot);
        assert!(!is_complete_snapshot(
            Provider::Codex,
            &serde_json::to_vec(&partial).unwrap()
        ));
    }
    for invalid in [
        json!({}),
        json!(false),
        json!({"used_percent":0,"limit_window_seconds":0,"reset_after_seconds":3600}),
    ] {
        let mut partial = complete.clone();
        partial["rate_limit"]["secondary_window"] = invalid;
        assert!(!is_complete_snapshot(
            Provider::Codex,
            &serde_json::to_vec(&partial).unwrap()
        ));
    }
    let mut unknown = complete;
    unknown["plan_type"] = json!("synthetic_unknown_plan");
    assert!(!is_complete_snapshot(
        Provider::Codex,
        &serde_json::to_vec(&unknown).unwrap()
    ));
}
