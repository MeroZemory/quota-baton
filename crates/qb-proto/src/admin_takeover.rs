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
    #[test]
    fn inspect_exact_required_nullable_duplicate_and_unknown_members() {
        for raw in [
            r#"{"email":null,"plan":null,"provider_account_id":null,"source_path":"C:/synthetic/home","source_last_refresh":null,"source_mtime":null,"owner_alive":false,"existing_id":null,"inspect_digest":"synthetic-digest"}"#,
            r#"{"email":"fixture@example.invalid","plan":"plus","provider_account_id":"synthetic-provider","source_path":"C:/synthetic/home","source_last_refresh":"2026-01-01T00:00:00Z","source_mtime":"2026-01-01T00:00:01Z","owner_alive":true,"existing_id":"synthetic-existing","inspect_digest":"synthetic-digest"}"#,
        ] {
            let reply: TakeoverInspect = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_string(&reply).unwrap(), raw);
            let value: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(value.as_object().unwrap().len(), 9);
            for (key, field) in value.as_object().unwrap() {
                let mut missing = value.clone();
                missing.as_object_mut().unwrap().remove(key);
                assert!(serde_json::from_value::<TakeoverInspect>(missing).is_err());
                let duplicate = format!(
                    "{},{}:{}}}",
                    &raw[..raw.len() - 1],
                    serde_json::to_string(key).unwrap(),
                    field
                );
                assert!(serde_json::from_str::<TakeoverInspect>(&duplicate).is_err());
            }
            let mut unknown = value;
            unknown["unexpected"] = serde_json::json!(true);
            assert!(serde_json::from_value::<TakeoverInspect>(unknown).is_err());
        }
        for raw in ["null", "[]", r#"{"source_path":null}"#] {
            assert!(serde_json::from_str::<TakeoverInspect>(raw).is_err());
        }
    }
}
