//! Local account admin errors only; not upstream errors or mutation authority.
use serde::de::{MapAccess, Visitor, value::MapAccessDeserializer};
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, marker::PhantomData};

fn object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    struct V<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = T;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<T, M::Error> {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }
    d.deserialize_map(V(PhantomData))
}
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Debug, Serialize)]
pub struct AccountErrorReply {
    pub error: AccountErrorDetail,
}
impl<'de> Deserialize<'de> for AccountErrorReply {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
        struct Fields {
            #[serde(deserialize_with = "object")]
            error: AccountErrorDetail,
        }
        let fields: Fields = object(d)?;
        Ok(Self {
            error: fields.error,
        })
    }
}

/// Declaration order preserves code/outcome/message and code/message/retry_after.
/// Absent optional keys stay absent; explicit null is not an observed producer value.
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AccountErrorDetail {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub code: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub outcome: Option<RefreshTerminal>,
    pub message: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub retry_after: Option<u64>,
}
impl AccountErrorDetail {
    pub fn coded(code: &str, message: &str) -> Self {
        Self {
            code: Some(code.to_owned()),
            outcome: None,
            message: message.to_owned(),
            retry_after: None,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshTerminal {
    RefreshTokenInvalidated,
    RefreshTokenReused,
    RefreshTokenExpired,
    RefreshTokenRevoked,
    InvalidGrant,
    #[serde(rename = "token_endpoint_401")]
    TokenEndpoint401,
}

/// Deliberate classification projection, NOT a whole response validation receipt.
/// Ignore all non-code fields even under contract-tests: the old readers did too.
#[derive(Debug)]
pub struct AccountErrorSignal {
    pub code: Option<String>,
}
impl<'de> Deserialize<'de> for AccountErrorSignal {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Detail {
            code: Option<String>,
        }
        #[derive(Deserialize)]
        struct Fields {
            #[serde(deserialize_with = "object")]
            error: Detail,
        }
        let fields: Fields = object(d)?;
        Ok(Self {
            code: fields.error.code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_errors_exact_presence_order_and_terminal_outcomes() {
        for raw in [
            r#"{"error":{"message":"synthetic"}}"#,
            r#"{"error":{"code":"not_linked","message":"synthetic"}}"#,
            r#"{"error":{"code":"poll_too_soon","message":"synthetic","retry_after":0}}"#,
            r#"{"error":{"code":"poll_too_soon","message":"synthetic","retry_after":18446744073709551615}}"#,
        ] {
            let reply: AccountErrorReply = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_string(&reply).unwrap(), raw);
        }
        for outcome in [
            "refresh_token_invalidated",
            "refresh_token_reused",
            "refresh_token_expired",
            "refresh_token_revoked",
            "invalid_grant",
            "token_endpoint_401",
        ] {
            let raw = format!(
                r#"{{"error":{{"code":"terminal","outcome":"{outcome}","message":"synthetic"}}}}"#
            );
            let reply: AccountErrorReply = serde_json::from_str(&raw).unwrap();
            assert_eq!(serde_json::to_string(&reply).unwrap(), raw);
        }
    }
    #[test]
    fn account_errors_reject_null_optional_fields_duplicates_and_invalid_types() {
        for raw in [
            r#"{}"#,
            r#"{"error":{}}"#,
            r#"{"error":null}"#,
            r#"{"error":{"message":null}}"#,
            r#"{"error":{"message":"a","message":"b"}}"#,
            r#"{"error":{"message":"a","code":null}}"#,
            r#"{"error":{"message":"a","outcome":null}}"#,
            r#"{"error":{"message":"a","retry_after":null}}"#,
            r#"{"error":{"message":"a","outcome":"success"}}"#,
            r#"{"error":{"message":"a","retry_after":-1}}"#,
            r#"{"error":{"message":"a","retry_after":1.5}}"#,
            r#"{"error":{"message":"a","retry_after":"1"}}"#,
            r#"{"error":{"message":"a","code":"a","code":"b"}}"#,
            r#"{"error":{"message":"a","retry_after":1,"retry_after":2}}"#,
            r#"{"error":{"message":"a","outcome":"invalid_grant","outcome":"invalid_grant"}}"#,
            r#"{"error":{"message":"a"},"error":{"message":"b"}}"#,
            r#"{"error":{"message":"a","unknown":true}}"#,
            r#"{"error":["linked",null,"a",null]}"#,
            r#"[{"message":"a"}]"#,
        ] {
            assert!(
                serde_json::from_str::<AccountErrorReply>(raw).is_err(),
                "{raw}"
            );
        }
    }
    #[test]
    fn account_error_signal_is_only_a_safe_code_projection() {
        for raw in [
            r#"{"error":{"code":"linked"}}"#,
            r#"{"error":{"code":"linked","message":"synthetic-secret","future":true}}"#,
        ] {
            assert_eq!(
                serde_json::from_str::<AccountErrorSignal>(raw)
                    .unwrap()
                    .code
                    .as_deref(),
                Some("linked")
            );
        }
        for raw in [r#"{"error":{}}"#, r#"{"error":{"code":null}}"#] {
            assert!(
                serde_json::from_str::<AccountErrorSignal>(raw)
                    .unwrap()
                    .code
                    .is_none()
            );
        }
        for raw in [
            r#"{}"#,
            r#"{"error":{"code":3}}"#,
            r#"{"error":{"code":"a","code":"b"}}"#,
            r#"{"error":["linked"]}"#,
            r#"[{"code":"linked"}]"#,
        ] {
            assert!(serde_json::from_str::<AccountErrorSignal>(raw).is_err());
        }
    }
}
