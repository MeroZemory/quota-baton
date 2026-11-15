use super::*;

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
            guard == "healthy",
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
        if guard == "healthy" {
            assert!(merged["windows"][0]["status"].is_null());
            assert!(merged["signals"]["unified_reset_at"].is_null());
            assert!(merged["signals"]["retry_after_seconds"].is_null());
        }
        if guard == "paid" {
            assert_eq!(merged["signals"]["active_limit"], "credits");
            assert_eq!(merged["signals"]["serving_from_credits"], true);
        }
    }
}
