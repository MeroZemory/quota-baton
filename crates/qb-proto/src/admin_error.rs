//! Existing local error wire shapes only; no admission or error classification.
//! The admin handler and HTTP middleware envelopes deliberately remain distinct.
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminErrorReply {
    pub error: AdminErrorDetail,
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminErrorDetail {
    pub message: String,
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct HttpErrorReply {
    pub error: HttpErrorDetail,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct HttpErrorDetail {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_existing_object_shapes_and_order_roundtrip() {
        let admin = r#"{"error":{"message":"synthetic message"}}"#;
        let value: AdminErrorReply = serde_json::from_str(admin).unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), admin);
        for raw in [
            r#"{"error":{"type":"authentication_error","message":"synthetic message"}}"#,
            r#"{"error":{"type":"invalid_request_error","message":"synthetic message"},"type":"error"}"#,
        ] {
            let value: HttpErrorReply = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_string(&value).unwrap(), raw);
        }
    }

    #[test]
    fn required_fields_types_and_raw_duplicates_are_not_normalized() {
        for raw in [
            r#"{}"#,
            r#"{"error":null}"#,
            r#"{"error":{}}"#,
            r#"{"error":{"message":null}}"#,
            r#"{"error":{"message":7}}"#,
            r#"{"error":{"message":"first","message":"second"}}"#,
            r#"{"error":{"message":"first"},"error":{"message":"second"}}"#,
        ] {
            assert!(serde_json::from_str::<AdminErrorReply>(raw).is_err());
        }
        for raw in [
            r#"{"error":{"message":"synthetic"}}"#,
            r#"{"error":{"type":null,"message":"synthetic"}}"#,
            r#"{"error":{"type":"first","type":"second","message":"synthetic"}}"#,
            r#"{"error":{"type":"synthetic","message":"first","message":"second"}}"#,
            r#"{"error":{"type":"synthetic","message":"synthetic"},"type":null,"type":"error"}"#,
        ] {
            assert!(serde_json::from_str::<HttpErrorReply>(raw).is_err());
        }
    }

    #[test]
    fn contract_tests_keep_admin_and_http_fields_distinct() {
        assert!(
            serde_json::from_str::<AdminErrorReply>(
                r#"{"error":{"type":"synthetic","message":"synthetic"}}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<AdminErrorReply>(
                r#"{"error":{"message":"synthetic"},"type":"error"}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<HttpErrorReply>(
                r#"{"error":{"type":"synthetic","message":"synthetic","param":null}}"#,
            )
            .is_err()
        );
    }
}
