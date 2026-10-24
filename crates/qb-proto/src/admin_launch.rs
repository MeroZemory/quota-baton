//! Authenticated launch response shape; admission and URL validation stay with callers.

/// URLs contain a routing handle. Deliberately no Debug or Display.
#[derive(serde::Serialize, serde::Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct LaunchReply {
    pub base_url: String,
    pub catalog_url: String,
    pub account: String,
    pub model: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct LaunchErrorReply {
    pub error: LaunchErrorDetail,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct LaunchErrorDetail {
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::{LaunchErrorDetail, LaunchErrorReply, LaunchReply};
    use serde_json::{Value, json};

    #[test]
    fn launch_error_exact_shape_and_required_fields() {
        let raw = r#"{"error":{"code":"launch_unavailable","message":"synthetic message"}}"#;
        let reply = LaunchErrorReply {
            error: LaunchErrorDetail {
                code: "launch_unavailable".into(),
                message: "synthetic message".into(),
            },
        };
        assert_eq!(serde_json::to_string(&reply).unwrap(), raw);
        let decoded: LaunchErrorReply = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), raw);
        for bad in [
            r#"{}"#,
            r#"{"error":{"message":"synthetic"}}"#,
            r#"{"error":{"code":"synthetic"}}"#,
            r#"{"error":{"code":null,"message":"synthetic"}}"#,
            r#"{"error":{"code":"first","code":"second","message":"synthetic"}}"#,
            r#"{"error":{"code":"synthetic","message":"synthetic","extra":true}}"#,
        ] {
            assert!(serde_json::from_str::<LaunchErrorReply>(bad).is_err());
        }
    }

    fn wire(model: Value) -> Value {
        json!({"base_url":"http://127.0.0.1:1234/l/synthetic/v1",
            "catalog_url":"http://127.0.0.1:1234/l/synthetic/v1/models",
            "account":"synthetic-account", "model":model})
    }

    #[test]
    fn required_strings_reject_missing_null_and_wrong_type() {
        for key in ["base_url", "catalog_url", "account"] {
            let mut missing = wire(Value::Null);
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<LaunchReply>(missing).is_err());
            for invalid in [Value::Null, json!(1), json!(false), json!([]), json!({})] {
                let mut value = wire(Value::Null);
                value[key] = invalid;
                assert!(serde_json::from_value::<LaunchReply>(value).is_err());
            }
        }
    }

    #[test]
    fn model_preserves_missing_null_and_string_contract() {
        let mut missing = wire(Value::Null);
        missing.as_object_mut().unwrap().remove("model");
        let reply: LaunchReply = serde_json::from_value(missing).unwrap();
        assert!(reply.model.is_none());
        assert_eq!(serde_json::to_value(reply).unwrap(), wire(Value::Null));
        for model in [Value::Null, json!("synthetic-model")] {
            let value = wire(model);
            let reply: LaunchReply = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(reply).unwrap(), value);
        }
        for invalid in [json!(1), json!(false), json!([]), json!({})] {
            assert!(serde_json::from_value::<LaunchReply>(wire(invalid)).is_err());
        }
    }

    #[test]
    fn unknown_field_is_rejected_in_contract_tests() {
        let mut value = wire(Value::Null);
        value["unexpected"] = json!(true);
        assert!(serde_json::from_value::<LaunchReply>(value).is_err());
    }

    #[test]
    fn duplicate_fields_are_rejected_from_raw_json() {
        // A Value map would erase duplicates before the shared visitor sees them.
        let base = serde_json::to_string(&wire(Value::Null)).unwrap();
        for (key, value) in [
            ("base_url", json!("other")),
            ("catalog_url", json!("other")),
            ("account", json!("other")),
            ("model", Value::Null),
        ] {
            let raw = format!(
                "{},{}:{}}}",
                &base[..base.len() - 1],
                serde_json::to_string(key).unwrap(),
                value
            );
            assert!(serde_json::from_str::<LaunchReply>(&raw).is_err());
        }
    }

    #[test]
    fn roundtrip_preserves_wire_order_and_null_emission() {
        for model in [Value::Null, json!("synthetic-model")] {
            let reply: LaunchReply = serde_json::from_value(wire(model.clone())).unwrap();
            let raw = serde_json::to_string(&reply).unwrap();
            let expected = format!(
                "{{\"base_url\":\"http://127.0.0.1:1234/l/synthetic/v1\",\"catalog_url\":\"http://127.0.0.1:1234/l/synthetic/v1/models\",\"account\":\"synthetic-account\",\"model\":{model}}}"
            );
            assert_eq!(raw, expected);
            let parsed: LaunchReply = serde_json::from_str(&raw).unwrap();
            assert_eq!(serde_json::to_string(&parsed).unwrap(), raw);
        }
    }
}
