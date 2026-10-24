//! Live takeover phase replies; independent from journal/receipt decoding.
use serde::{Deserialize, Deserializer, Serialize};
fn required_nullable<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct TakeoverInspect {
    #[serde(deserialize_with = "required_nullable")]
    pub email: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub plan: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub provider_account_id: Option<String>,
    pub source_path: String,
    #[serde(deserialize_with = "required_nullable")]
    pub source_last_refresh: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub source_mtime: Option<String>,
    pub owner_alive: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub existing_id: Option<String>,
    pub inspect_digest: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct TakeoverRefresh {
    pub ok: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub token_expires_at: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct TakeoverCommit {
    pub id: String,
    #[serde(deserialize_with = "required_nullable")]
    pub renamed_to: Option<String>,
    // Successful live commits are finished/replayed only with journal.result Some.
    pub refresh: TakeoverRefresh,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commit_requires_ack_and_nullable_keys() {
        let raw = r#"{"id":"sample-acct","renamed_to":null,"refresh":{"ok":false,"token_expires_at":null}}"#;
        let reply: TakeoverCommit = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&reply).unwrap(), raw);
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        for pointer in ["", "/refresh"] {
            for key in value.pointer(pointer).unwrap().as_object().unwrap().keys() {
                let mut missing = value.clone();
                missing
                    .pointer_mut(pointer)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(key);
                assert!(serde_json::from_value::<TakeoverCommit>(missing).is_err());
            }
        }
        assert!(
            serde_json::from_str::<TakeoverCommit>(
                r#"{"id":"sample-acct","renamed_to":null,"refresh":null}"#
            )
            .is_err()
        );
    }
}
