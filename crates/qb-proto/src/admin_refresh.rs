//! Current manual refresh reply, separate from engine/disk state.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshOutcome {
    Success,
    RefreshTokenInvalidated,
    RefreshTokenReused,
    RefreshTokenExpired,
    RefreshTokenRevoked,
    InvalidGrant,
    #[serde(rename = "token_endpoint_401")]
    TokenEndpoint401,
}
fn required_nullable<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
    Option::<DateTime<Utc>>::deserialize(d)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct ManualRefreshReply {
    pub outcome: RefreshOutcome,
    #[serde(deserialize_with = "required_nullable")]
    pub token_expires_at: Option<DateTime<Utc>>,
    #[serde(deserialize_with = "required_nullable")]
    pub last_refresh_at: Option<DateTime<Utc>>,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_outcomes_required_nullable_keys_and_duplicates() {
        for outcome in [
            "success",
            "refresh_token_invalidated",
            "refresh_token_reused",
            "refresh_token_expired",
            "refresh_token_revoked",
            "invalid_grant",
            "token_endpoint_401",
        ] {
            let raw = format!(
                r#"{{"outcome":"{outcome}","token_expires_at":null,"last_refresh_at":null}}"#
            );
            let reply: ManualRefreshReply = serde_json::from_str(&raw).unwrap();
            assert_eq!(serde_json::to_string(&reply).unwrap(), raw);
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            for key in ["outcome", "token_expires_at", "last_refresh_at"] {
                let mut missing = value.clone();
                missing.as_object_mut().unwrap().remove(key);
                assert!(serde_json::from_value::<ManualRefreshReply>(missing).is_err());
                let duplicate = format!(
                    "{},{}:{}}}",
                    &raw[..raw.len() - 1],
                    serde_json::to_string(key).unwrap(),
                    value[key]
                );
                assert!(serde_json::from_str::<ManualRefreshReply>(&duplicate).is_err());
            }
            let mut extra = value;
            extra["unknown"] = serde_json::json!(true);
            assert!(serde_json::from_value::<ManualRefreshReply>(extra).is_err());
        }
        assert!(
            serde_json::from_str::<ManualRefreshReply>(
                r#"{"outcome":"sample-acct","token_expires_at":null,"last_refresh_at":null}"#
            )
            .is_err()
        );
    }
}
