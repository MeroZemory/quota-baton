use serde::{Deserialize, Serialize};

pub mod admin_account;
pub mod admin_action;
pub mod admin_adopt;
pub mod admin_control;
pub mod admin_error;
pub mod admin_import;
pub mod admin_launch;
pub mod admin_login;
pub mod admin_pool;
pub mod admin_routing;
pub mod admin_session_detail;
pub mod admin_sessions;
pub mod admin_state;
pub use admin_account::AdminAccount;
pub mod admin_warning;
pub mod history_response;
pub mod native_reset;
pub use admin_state::{AdminState, StateBindings, StateCapabilities, StateInflight, StateMemory};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub pid: u32,
    pub started_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DebugPinRequest {
    #[serde(default)]
    pub session_key: Option<String>,
    pub account_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportMode {
    Linked,
    Adopted,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CpaImportRequest {
    pub source: String,
    pub mode: ImportMode,
    #[serde(default)]
    pub dir: Option<std::path::PathBuf>,
    #[serde(default)]
    pub activate: Vec<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[cfg(test)]
mod health_contract_tests {
    use super::HealthResponse;

    #[test]
    fn health_contract_roundtrip_retains_current_wire_fields_and_order() {
        let wire =
            r#"{"ok":true,"version":"synthetic","pid":7,"started_at":"2026-10-24T00:00:00Z"}"#;
        let response: HealthResponse = serde_json::from_str(wire).unwrap();
        assert_eq!(serde_json::to_string(&response).unwrap(), wire);
        assert_eq!(response, response.clone());
    }

    #[test]
    fn health_contract_requires_every_field_and_rejects_duplicates() {
        let complete = serde_json::json!({"ok":true,"version":"synthetic","pid":7,"started_at":"2026-10-24T00:00:00Z"});
        for field in ["ok", "version", "pid", "started_at"] {
            let mut incomplete = complete.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<HealthResponse>(incomplete).is_err(),
                "accepted missing {field}"
            );
        }
        let duplicate = r#"{"ok":false,"ok":true,"version":"synthetic","pid":7,"started_at":"2026-10-24T00:00:00Z"}"#;
        assert!(serde_json::from_str::<HealthResponse>(duplicate).is_err());
    }

    #[test]
    fn health_contract_unit_strictness_rejects_unknown_fields() {
        let wire = r#"{"ok":true,"version":"synthetic","pid":7,"started_at":"2026-10-24T00:00:00Z","unexpected":true}"#;
        assert!(serde_json::from_str::<HealthResponse>(wire).is_err());
    }
}
