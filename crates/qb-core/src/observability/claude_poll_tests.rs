use super::*;

#[test]
fn ordinary_claude_poll_model_recovery_requires_fresh_complete_known_scopes() {
    for guard in [
        "recovered",
        "model_claim",
        "omitted",
        "null",
        "unknown_claim",
        "other_model",
        "partial",
        "incoming_rejection",
        "near_limit",
        "allowed_global",
    ] {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        let at = Utc::now() - Duration::seconds(10);
        let start = at + Duration::seconds(1);
        let reset = at + Duration::days(1);
        let mut old = json!({"source":"headers","observed_at":at,"windows":[
            {"name":"7d","used_pct":51,"reset_at":reset,"observed_at":at,"status":"allowed"},
            {"name":"7d_oi","used_pct":100,"reset_at":reset,"observed_at":at,"status":"rejected"}
        ],"signals":{"unified_status":"rejected"}});
        if guard == "allowed_global" {
            old["signals"]["unified_status"] = json!("allowed");
        }
        if guard == "unknown_claim" {
            old["signals"]["representative_claim"] = json!("opaque");
        }
        if guard == "model_claim" {
            old["signals"]["representative_claim"] = json!("seven_day_opus");
        }
        if guard == "other_model" {
            old["windows"].as_array_mut().unwrap().push(json!({"name":"7d_sonnet","used_pct":100,"reset_at":reset,"observed_at":at,"status":"rejected"}));
        }
        obs.observe_poll("cl-model-recovery", old.clone(), true)
            .unwrap();
        let mut fresh = json!({"source":"poll","observed_at":start,"windows":[
            {"name":"7d","used_pct":51,"reset_at":reset,"observed_at":start,"status":"allowed"},
            {"name":"7d_oi","used_pct":0,"reset_at":reset,"observed_at":start}
        ],"signals":{}});
        if guard == "omitted" || guard == "null" {
            fresh["windows"].as_array_mut().unwrap().pop();
        }
        if guard == "null" {
            fresh["windows"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"7d_oi","used_pct":null,"reset_at":reset,"observed_at":null}));
        }
        if guard == "incoming_rejection" {
            fresh["signals"]["unified_status"] = json!("rejected");
        }
        if guard == "near_limit" {
            fresh["windows"][1]["used_pct"] = json!(97);
        }
        let merged = obs
            .observe_claude_poll("cl-model-recovery", fresh, guard != "partial", start)
            .unwrap();
        let recovered = matches!(guard, "recovered" | "model_claim" | "allowed_global");
        if guard == "allowed_global" {
            assert_eq!(merged["signals"]["unified_status"], "allowed");
        } else {
            assert_eq!(
                merged["signals"]["unified_status"].is_null(),
                recovered,
                "{guard}"
            );
        }
        let model = merged["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == "7d_oi")
            .unwrap();
        assert_eq!(model["status"].is_null(), recovered, "{guard}");
        if matches!(guard, "omitted" | "null") {
            assert_eq!(model["used_pct"].as_f64(), Some(100.0));
            assert_eq!(model["observed_at"], old["windows"][1]["observed_at"]);
        }
        if guard == "other_model" {
            assert!(
                merged["windows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|w| w["name"] == "7d_sonnet" && w["status"] == "rejected")
            );
        }
    }
}

#[test]
fn ordinary_claude_poll_retirement_preserves_partial_rejected_and_unrelated_evidence() {
    for guard in [
        "healthy",
        "partial",
        "near_limit",
        "incoming_rejection",
        "incoming_limit",
        "unknown_claim",
        "unknown_window",
        "model_rejection",
        "paid",
        "equal_clock",
        "newer_header",
        "newer_window",
        "informational",
        "explicit_allowed",
        "unrelated_near_model",
    ] {
        let dir = directory();
        let obs = Observability::new(dir.path(), None).unwrap();
        let at = Utc::now() - Duration::seconds(10);
        let reset = at + Duration::hours(1);
        let mut old = json!({"source":"headers","observed_at":at,"windows":[
            {"name":"5h","used_pct":100.0,"reset_at":reset,"observed_at":at,"status":"rejected"},
            {"name":"7d","used_pct":7.0,"reset_at":reset+Duration::days(1),"observed_at":at}
        ],"signals":{"unified_status":"rejected","unified_reset_at":reset,
            "representative_claim":"five_hour","retry_after_seconds":3600.0},
            "credits":{"has_credits":true,"balance":42.0}});
        match guard {
            "unknown_claim" => old["signals"]["representative_claim"] = json!("opaque"),
            "unknown_window" => old["windows"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"opaque"})),
            "model_rejection" => old["windows"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"7d_oi","status":"rejected"})),
            "paid" => {
                old["signals"]["active_limit"] = json!("credits");
                old["signals"]["serving_from_credits"] = json!(true);
            }
            "newer_window" => old["windows"][0]["observed_at"] = json!(at + Duration::seconds(2)),
            "informational" => {
                old["signals"]["unified_status"] = json!("allowed");
                old["windows"][0]["status"] = json!("allowed");
            }
            _ => {}
        }
        obs.observe_poll("cl-synthetic-ordinary", old, true)
            .unwrap();
        let before = obs.quota_snapshot("cl-synthetic-ordinary").unwrap();
        let start = match guard {
            "equal_clock" => at,
            "newer_header" => at - Duration::seconds(1),
            _ => at + Duration::seconds(1),
        };
        let mut fresh = json!({"source":"poll","observed_at":start,"windows":[
            {"name":"5h","used_pct":0.0,"reset_at":reset,"observed_at":start},
            {"name":"7d","used_pct":7.0,"reset_at":reset+Duration::days(1),"observed_at":start}
        ],"signals":{}});
        match guard {
            "near_limit" => fresh["windows"][0]["used_pct"] = json!(95.1),
            "incoming_rejection" => fresh["signals"]["unified_status"] = json!("rejected"),
            "incoming_limit" => fresh["signals"]["limit_reached"] = json!(true),
            "explicit_allowed" => {
                fresh["signals"] = json!({"unified_status":"allowed", "limit_reached":false,"overage_status":"allowed","retry_after_seconds":12.0})
            }
            "unrelated_near_model" => {
                fresh["windows"].as_array_mut().unwrap().push(json!({"name":"7d_oi","used_pct":97,"reset_at":reset,"observed_at":start,"status":"allowed"}));
            }
            _ => {}
        }
        let merged =
            obs.observe_claude_poll("cl-synthetic-ordinary", fresh, guard != "partial", start);
        if matches!(guard, "equal_clock" | "newer_header" | "newer_window") {
            assert!(merged.is_none(), "{guard}");
            assert_eq!(obs.quota_snapshot("cl-synthetic-ordinary").unwrap(), before);
            continue;
        }
        let merged = merged.unwrap();
        assert_eq!(
            merged["signals"]["unified_status"].is_null(),
            matches!(guard, "healthy" | "unrelated_near_model"),
            "{guard}"
        );
        assert_eq!(merged["credits"], before["credits"], "{guard}");
        if guard == "informational" {
            assert_eq!(merged["signals"], before["signals"]);
        }
        if guard == "explicit_allowed" {
            assert_eq!(merged["signals"]["unified_status"], "allowed");
            assert_eq!(merged["signals"]["limit_reached"], false);
            assert_eq!(merged["signals"]["overage_status"], "allowed");
            assert_eq!(merged["signals"]["retry_after_seconds"], 12.0);
            assert!(merged["windows"][0]["status"].is_null());
        }
        if matches!(guard, "healthy" | "unrelated_near_model") {
            assert!(merged["windows"][0]["status"].is_null());
            assert!(merged["signals"]["unified_reset_at"].is_null());
            assert!(merged["signals"]["retry_after_seconds"].is_null());
        }
        if guard == "unrelated_near_model" {
            let window = merged["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["name"] == "7d_oi")
                .unwrap();
            assert_eq!(window["used_pct"].as_f64(), Some(97.0));
            assert_eq!(window["status"], "allowed");
        }
        if guard == "paid" {
            assert_eq!(merged["signals"]["active_limit"], "credits");
            assert_eq!(merged["signals"]["serving_from_credits"], true);
        }
    }
}
