//! Shared wire responses for the local admin API.
//!
//! Optional login fields retain the server's omission rule. Display labels and
//! authorization URL validation belong to the consumers.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct LoginStart {
    pub login_id: String,
    pub auth_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<DevicePrompt>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct DevicePrompt {
    pub verification_url: String,
    pub user_code: String,
    pub expires_in: u64,
}

impl std::fmt::Debug for DevicePrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePrompt")
            .field("verification_url", &self.verification_url)
            .field("user_code", &"[redacted]")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    Pending,
    Completed,
    Failed,
    Cancelled,
    Expired,
}

impl LoginState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct LoginStatus {
    pub status: LoginState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{LoginStart, LoginState, LoginStatus};
    use serde_json::{Value, json};

    fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(wire: Value) -> T {
        let decoded: T = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), wire);
        decoded
    }

    #[test]
    fn login_start_preserves_required_fields() {
        let start: LoginStart = round_trip(json!({
            "login_id":"synthetic-login", "auth_url":"synthetic-url"
        }));
        assert_eq!(start.login_id, "synthetic-login");
        assert_eq!(start.auth_url, "synthetic-url");
        for wire in [
            json!({}),
            json!({"login_id":"synthetic-login"}),
            json!({"auth_url":"synthetic-url"}),
        ] {
            assert!(serde_json::from_value::<LoginStart>(wire).is_err());
        }
    }

    #[test]
    fn device_prompt_roundtrips_and_debug_redacts_code() {
        let start: LoginStart = round_trip(json!({
            "login_id":"synthetic-login",
            "auth_url":"https://auth.openai.com/codex/device",
            "device": {
                "verification_url":"https://auth.openai.com/codex/device",
                "user_code":"SYNTHETIC-SECRET",
                "expires_in":899
            }
        }));
        assert!(!format!("{start:?}").contains("SYNTHETIC-SECRET"));
        for invalid in [
            json!({}),
            json!({"verification_url":"synthetic", "user_code":"synthetic", "expires_in":-1}),
        ] {
            let wire = json!({"login_id":"synthetic", "auth_url":"synthetic", "device":invalid});
            assert!(serde_json::from_value::<LoginStart>(wire).is_err());
        }
        let duplicate =
            r#"{"login_id":"synthetic","auth_url":"synthetic","device":null,"device":null}"#;
        assert!(serde_json::from_str::<LoginStart>(duplicate).is_err());
    }

    #[test]
    fn every_status_preserves_spelling_and_optional_field_omissions() {
        for (state, spelling) in [
            (LoginState::Pending, "pending"),
            (LoginState::Completed, "completed"),
            (LoginState::Failed, "failed"),
            (LoginState::Cancelled, "cancelled"),
            (LoginState::Expired, "expired"),
        ] {
            assert_eq!(state.as_str(), spelling);
            for account in [false, true] {
                for error in [false, true] {
                    let mut wire = json!({"status":spelling});
                    if account {
                        wire["account_id"] = json!("synthetic-account");
                    }
                    if error {
                        wire["error"] = json!("synthetic-error");
                    }
                    let status: LoginStatus = round_trip(wire);
                    assert_eq!(status.status, state);
                    assert_eq!(status.account_id.is_some(), account);
                    assert_eq!(status.error.is_some(), error);
                }
            }
        }
        let status: LoginStatus = serde_json::from_value(json!({
            "status":"pending", "account_id":null, "error":null
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({"status":"pending"})
        );
    }

    #[test]
    fn unit_test_contract_rejects_unknown_fields_and_invalid_status() {
        let start = serde_json::from_value::<LoginStart>(json!({
            "login_id":"synthetic-login", "auth_url":"synthetic-url", "extra":true
        }))
        .unwrap_err();
        assert!(start.to_string().contains("unknown field `extra`"));
        let status = serde_json::from_value::<LoginStatus>(json!({
            "status":"pending", "extra":true
        }))
        .unwrap_err();
        assert!(status.to_string().contains("unknown field `extra`"));
        for wire in [
            json!({}),
            json!({"status":"unknown"}),
            json!({"status":"Pending"}),
            json!({"status":null}),
            json!({"status":0}),
        ] {
            assert!(serde_json::from_value::<LoginStatus>(wire).is_err());
        }
    }

    #[test]
    fn duplicate_keys_are_rejected_before_map_normalization() {
        for raw in [
            r#"{"login_id":"synthetic","login_id":"other","auth_url":"synthetic-url"}"#,
            r#"{"login_id":"synthetic","auth_url":"synthetic-url","auth_url":"other"}"#,
        ] {
            assert!(serde_json::from_str::<LoginStart>(raw).is_err());
        }
        for (field, value) in [
            ("status", json!("pending")),
            ("account_id", Value::Null),
            ("error", Value::Null),
        ] {
            let raw = format!(
                "{{\"status\":\"pending\",\"account_id\":null,\"error\":null,{}:{value}}}",
                serde_json::to_string(field).unwrap()
            );
            assert!(serde_json::from_str::<LoginStatus>(&raw).is_err());
        }
    }

    #[test]
    fn invalid_field_types_fail_and_wire_order_is_preserved() {
        for field in ["login_id", "auth_url"] {
            for invalid in [Value::Null, json!(7), json!(true), json!([]), json!({})] {
                let mut wire = json!({"login_id":"synthetic", "auth_url":"synthetic-url"});
                wire[field] = invalid;
                assert!(serde_json::from_value::<LoginStart>(wire).is_err());
            }
        }
        for field in ["account_id", "error"] {
            for invalid in [json!(7), json!(true), json!([]), json!({})] {
                let mut wire = json!({"status":"pending"});
                wire[field] = invalid;
                assert!(serde_json::from_value::<LoginStatus>(wire).is_err());
            }
        }
        let start = LoginStart {
            login_id: "synthetic".into(),
            auth_url: "synthetic-url".into(),
            device: None,
        };
        assert_eq!(
            serde_json::to_string(&start).unwrap(),
            r#"{"login_id":"synthetic","auth_url":"synthetic-url"}"#
        );
        let status = LoginStatus {
            status: LoginState::Completed,
            account_id: Some("synthetic-account".into()),
            error: Some("synthetic-error".into()),
        };
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"status":"completed","account_id":"synthetic-account","error":"synthetic-error"}"#
        );
    }
}
