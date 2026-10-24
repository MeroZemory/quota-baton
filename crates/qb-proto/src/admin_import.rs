//! CPA import reply rows; the route returns an array, with all ten keys present.
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportAction {
    Created,
    Updated,
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    UnknownType,
    InvalidData,
    OlderToken,
    OwnedAccount,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(deserialize = "P: Deserialize<'de>, U: Deserialize<'de>, T: Deserialize<'de>"))]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct ImportRow<P, U, T> {
    #[serde(deserialize_with = "required_nullable")]
    pub id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub provider: Option<P>,
    #[serde(deserialize_with = "required_nullable")]
    pub email: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub plan: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub expires_at: Option<T>,
    #[serde(deserialize_with = "required_nullable")]
    pub user_state: Option<U>,
    pub action: ImportAction,
    #[serde(deserialize_with = "required_nullable")]
    pub reason: Option<SkipReason>,
    #[serde(deserialize_with = "required_nullable")]
    pub stored_timestamp: Option<T>,
    #[serde(deserialize_with = "required_nullable")]
    pub file_timestamp: Option<T>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_account::{AdminProvider, AdminUserState};
    use chrono::{DateTime, Utc};
    use serde_json::{Value, json};
    type Row = ImportRow<AdminProvider, AdminUserState, DateTime<Utc>>;
    const EMPTY: &str = r#"{"id":null,"provider":null,"email":null,"plan":null,"expires_at":null,"user_state":null,"action":"skipped","reason":"invalid_data","stored_timestamp":null,"file_timestamp":null}"#;
    fn populated() -> Value {
        json!({"id":"synthetic","provider":"codex","email":"synthetic@example.invalid","plan":"synthetic-plan","expires_at":"2030-01-01T00:00:00Z","user_state":"active","action":"skipped","reason":"older_token","stored_timestamp":"2030-01-02T00:00:00Z","file_timestamp":"2030-01-01T00:00:00Z"})
    }
    #[test]
    fn exact_order_all_nulls_and_populated_array_roundtrip() {
        let row: Row = serde_json::from_str(EMPTY).unwrap();
        assert_eq!(serde_json::to_string(&row).unwrap(), EMPTY);
        let raw = serde_json::to_string(&vec![
            populated(),
            serde_json::from_str::<Value>(EMPTY).unwrap(),
        ])
        .unwrap();
        let rows: Vec<Row> = serde_json::from_str(&raw).unwrap();
        assert_eq!(serde_json::to_string(&rows).unwrap(), raw);
        assert!(serde_json::from_str::<Vec<Row>>("[]").unwrap().is_empty());
    }
    #[test]
    fn every_key_required_and_unknowns_duplicates_rejected() {
        let value: Value = serde_json::from_str(EMPTY).unwrap();
        for key in value.as_object().unwrap().keys() {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<Row>(missing).is_err(),
                "missing {key}"
            );
            let raw = format!(
                "{},{}:{}}}",
                &EMPTY[..EMPTY.len() - 1],
                serde_json::to_string(key).unwrap(),
                value[key]
            );
            assert!(
                serde_json::from_str::<Row>(&raw).is_err(),
                "duplicate {key}"
            );
        }
        let mut extra = value;
        extra["unexpected"] = json!(true);
        assert!(serde_json::from_value::<Row>(extra).is_err());
    }
    #[test]
    fn all_action_reason_and_domain_leaf_spellings_are_retained() {
        for (action, spelling) in [
            (ImportAction::Created, "created"),
            (ImportAction::Updated, "updated"),
            (ImportAction::Skipped, "skipped"),
        ] {
            assert_eq!(serde_json::to_value(action).unwrap(), spelling);
            assert_eq!(
                serde_json::from_value::<ImportAction>(json!(spelling)).unwrap(),
                action
            );
        }
        for (reason, spelling) in [
            (SkipReason::UnknownType, "unknown_type"),
            (SkipReason::InvalidData, "invalid_data"),
            (SkipReason::OlderToken, "older_token"),
            (SkipReason::OwnedAccount, "owned_account"),
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), spelling);
            assert_eq!(
                serde_json::from_value::<SkipReason>(json!(spelling)).unwrap(),
                reason
            );
        }
        for provider in ["claude", "codex"] {
            for state in ["active", "paused", "disabled"] {
                let mut value = populated();
                value["provider"] = json!(provider);
                value["user_state"] = json!(state);
                let row: Row = serde_json::from_value(value.clone()).unwrap();
                assert_eq!(serde_json::to_value(row).unwrap(), value);
            }
        }
    }
    #[test]
    fn invalid_types_timestamps_and_unknown_enum_values_fail() {
        for key in populated().as_object().unwrap().keys() {
            let mut invalid = populated();
            invalid[key] = json!(17);
            assert!(
                serde_json::from_value::<Row>(invalid).is_err(),
                "wrong type {key}"
            );
        }
        for key in [
            "provider",
            "user_state",
            "action",
            "reason",
            "expires_at",
            "stored_timestamp",
            "file_timestamp",
        ] {
            let mut invalid = populated();
            invalid[key] = json!("invalid-leaf");
            assert!(serde_json::from_value::<Row>(invalid).is_err());
        }
        let mut null_action = populated();
        null_action["action"] = Value::Null;
        assert!(serde_json::from_value::<Row>(null_action).is_err());
    }
}
