//! Current adoption reply; not an account archive or persistence schema.
use serde::{Deserialize, Deserializer, Serialize};
fn required_nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdoptWarning {
    CpaFileMissing,
    CpaFileUnreadable,
    BackupMissing,
    BackupUnreadable,
    OlderOrUnorderedCandidateIgnored,
    EmailUpdated,
    StoredTokensMayNeedRelogin,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
#[serde(bound(deserialize = "Time: Deserialize<'de>"))]
pub struct AdoptReply<Time> {
    pub id: String,
    pub generation: u64,
    pub from: String,
    #[serde(deserialize_with = "required_nullable")]
    pub token_expires_at: Option<Time>,
    #[serde(deserialize_with = "required_nullable")]
    pub last_refresh_at: Option<Time>,
    pub warnings: Vec<AdoptWarning>,
}
#[cfg(test)]
mod tests {
    use super::*;
    type Reply = AdoptReply<String>;
    #[test]
    fn current_keys_nullable_presence_duplicates_and_warning_codes() {
        let raw = r#"{"id":"sample-acct","generation":2,"from":"store","token_expires_at":null,"last_refresh_at":null,"warnings":["stored_tokens_may_need_relogin"]}"#;
        let reply: Reply = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&reply).unwrap(), raw);
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        for key in value.as_object().unwrap().keys() {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<Reply>(missing).is_err(), "{key}");
            let duplicate = format!(
                "{},{}:{}}}",
                &raw[..raw.len() - 1],
                serde_json::to_string(key).unwrap(),
                value[key]
            );
            assert!(serde_json::from_str::<Reply>(&duplicate).is_err(), "{key}");
        }
        let mut unknown = value.clone();
        unknown["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Reply>(unknown).is_err());
        let mut invalid = value;
        invalid["warnings"] = serde_json::json!(["sample-acct-unknown"]);
        assert!(serde_json::from_value::<Reply>(invalid).is_err());
    }
}
