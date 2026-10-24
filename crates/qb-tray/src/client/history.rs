//! Live history presentation boundary. Shared DTOs retain the original graph;
//! the existing display model keeps its renames and presentation-only defaults.
//! This byte bridge runs AFTER strict shared decoding, never before it.
use super::ClientError;
use serde::{Serialize, de::DeserializeOwned};

fn presentation<T: Serialize, U: DeserializeOwned>(wire: &T) -> Result<U, ClientError> {
    let invalid = || ClientError::AccountAction("대시보드 응답 형식이 올바르지 않습니다");
    let bytes = serde_json::to_vec(wire).map_err(|_| invalid())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}
pub(super) fn activity(
    wire: &qb_core::dashboard::history::response::ActivityPage,
) -> Result<qb_tray::dashboard_model::ActivityResponse, ClientError> {
    presentation(wire)
}
pub(super) fn usage(
    wire: &qb_core::dashboard::history::UsageView,
) -> Result<qb_tray::dashboard_model::UsageHistory, ClientError> {
    presentation(wire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::AdminClient;
    use qb_core::dashboard::history::{UsageView, response::ActivityPage};

    #[test]
    fn object_boundary_rejects_sequences_duplicates_missing_and_null_fields() {
        for bad in [
            r#"[[ ],2,false]"#,
            r#"{"events":[],"events":[],"next_seq":2,"truncated":false}"#,
            r#"{"events":[],"next_seq":2,"next_seq":3,"truncated":false}"#,
            r#"{"events":[],"truncated":false}"#,
            r#"{"events":null,"next_seq":2,"truncated":false}"#,
        ] {
            assert!(AdminClient::decode_dashboard_object::<ActivityPage>(bad.as_bytes()).is_err());
        }
        for bad in [
            r#"[[],false]"#,
            r#"{"accounts":[],"accounts":[],"truncated":false}"#,
            r#"{"accounts":null,"truncated":false}"#,
            r#"{"accounts":[]}"#,
        ] {
            assert!(AdminClient::decode_dashboard_object::<UsageView>(bad.as_bytes()).is_err());
        }
    }

    #[test]
    fn activity_bridge_preserves_all_current_rendering_inputs_and_sparse_omissions() {
        let raw = br#"{"events":[{"seq":1,"ts":"2030-01-01T00:00:00Z","kind":"burst","provider":"codex","account":"cx-000001","account_to":null,"notable":true,"data":{"session":"000000000001","sessions":["000000000001"],"window":"5h","reset_at":"2030-01-01T01:00:00Z","expires_at":"2030-01-01T02:00:00Z","reason":"forced","attention":"burst","outcome":"ok","action":"pause","credits_used":1.25,"count":3,"max_wait_ms":4,"relayed":5,"moved_groups":6,"sessions_truncated":true,"recovered":true,"active":0,"threshold":8,"groups_hit":9}},{"seq":2,"ts":"2030-01-01T00:00:00Z","kind":"burst","provider":"codex","account":null,"account_to":null,"notable":false,"data":{"count":0,"max_wait_ms":0,"relayed":0,"moved_groups":0,"sessions_truncated":false,"recovered":false}}],"next_seq":3,"truncated":true}"#;
        let wire: ActivityPage = AdminClient::decode_dashboard_object(raw).unwrap();
        let facts = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            facts,
            serde_json::from_slice::<serde_json::Value>(raw).unwrap()
        );
        assert_eq!(facts["events"][1]["data"].as_object().unwrap().len(), 6);
        let display = activity(&wire).unwrap();
        let old: qb_tray::dashboard_model::ActivityResponse = serde_json::from_slice(raw).unwrap();
        assert_eq!(
            serde_json::to_value(&display).unwrap(),
            serde_json::to_value(old).unwrap()
        );
        assert_eq!(display.events[0].data.active_sessions, Some(0));
        assert!(display.events[0].data.after_failure);
        assert!(display.events[0].data.label.is_none());
        assert!(!display.events[0].data.actionable);
    }

    #[test]
    fn usage_bridge_preserves_gaps_open_spans_order_and_every_truncation_flag() {
        let raw = br#"{"accounts":[{"id":"cx-000001","windows":[{"name":"5h","points":[["2030-01-01T00:00:00Z",null],["2030-01-01T00:01:00Z",0.0]],"resets":["2030-01-01T00:01:00Z"],"truncated":true}],"credits_spans":[["2030-01-01T00:00:00Z",null]],"truncated":true}],"truncated":true}"#;
        let wire: UsageView = AdminClient::decode_dashboard_object(raw).unwrap();
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            serde_json::from_slice::<serde_json::Value>(raw).unwrap()
        );
        let display = usage(&wire).unwrap();
        let old: qb_tray::dashboard_model::UsageHistory = serde_json::from_slice(raw).unwrap();
        assert_eq!(
            serde_json::to_value(&display).unwrap(),
            serde_json::to_value(old).unwrap()
        );
        assert!(display.accounts[0].windows[0].points[0].1.is_none());
        assert!(display.accounts[0].credits_spans[0].1.is_none());
        assert!(
            display.truncated
                && display.accounts[0].truncated
                && display.accounts[0].windows[0].truncated
        );
    }
}
