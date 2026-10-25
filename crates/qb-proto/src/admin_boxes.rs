//! Host-admin box wire contracts. Registry and key validation stay with callers.

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct BoxView {
    pub name: String,
    pub epoch: u64,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct BoxListReply {
    pub boxes: Vec<BoxView>,
}

/// Producer view borrowing the server-owned key. No Debug, Clone or Deserialize.
/// Serialization does not transfer ownership or promise wiping transport copies.
#[derive(serde::Serialize)]
pub struct AddReply<'a> {
    pub name: &'a str,
    pub epoch: u64,
    pub client_key: &'a str,
}

/// Shared consumer contract; the caller owns the decoded key allocation.
/// No Debug, Clone or Serialize. String is an
/// ordinary allocation, not a wiping buffer. Consumers must not log this body.
#[derive(serde::Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AddReplyReceived {
    pub name: String,
    pub epoch: u64,
    pub client_key: String,
}

#[cfg(test)]
mod tests {
    use super::{AddReply, AddReplyReceived, BoxListReply, BoxView};
    use serde::{Serialize, de::DeserializeOwned};
    use serde_json::{Value, json};

    fn view() -> Value {
        json!({"name":"box-a","epoch":1,"revoked":false})
    }

    fn add() -> Value {
        json!({"name":"box-a","epoch":1,"client_key":"synthetic-key"})
    }

    fn rejects_missing_duplicate_unknown<T: DeserializeOwned>(wire: Value) {
        assert!(serde_json::from_value::<T>(wire.clone()).is_ok());
        let raw = serde_json::to_string(&wire).unwrap();
        for (field, value) in wire.as_object().unwrap() {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<T>(missing).is_err());
            // Raw JSON is required: a Value object would erase duplicates.
            let duplicate = format!(
                "{},{}:{}}}",
                &raw[..raw.len() - 1],
                serde_json::to_string(field).unwrap(),
                value
            );
            assert!(serde_json::from_str::<T>(&duplicate).is_err());
        }
        let mut unknown = wire;
        unknown["unexpected"] = json!(true);
        assert!(serde_json::from_value::<T>(unknown).is_err());
    }

    fn exact<T: Serialize>(reply: &T, expected: &str) {
        // All values in these tests are synthetic, never an actual credential.
        assert_eq!(serde_json::to_string(reply).unwrap(), expected);
    }

    #[test]
    fn box_view_exact_shape_and_both_revocation_states() {
        for revoked in [false, true] {
            let reply = BoxView {
                name: "box-a".into(),
                epoch: 1,
                revoked,
            };
            let raw = format!("{{\"name\":\"box-a\",\"epoch\":1,\"revoked\":{revoked}}}");
            exact(&reply, &raw);
            assert_eq!(serde_json::from_str::<BoxView>(&raw).unwrap(), reply);
        }
    }

    #[test]
    fn list_exact_shape_empty_and_populated() {
        exact(&BoxListReply { boxes: vec![] }, r#"{"boxes":[]}"#);
        let reply = BoxListReply {
            boxes: vec![serde_json::from_value(view()).unwrap()],
        };
        let raw = r#"{"boxes":[{"name":"box-a","epoch":1,"revoked":false}]}"#;
        exact(&reply, raw);
        assert_eq!(serde_json::from_str::<BoxListReply>(raw).unwrap(), reply);
    }

    #[test]
    fn borrowed_add_exact_shape_shared_consumer_and_escaping() {
        let name = String::from("box-a");
        let key = String::from("synthetic-key");
        let reply = AddReply {
            name: &name,
            epoch: 1,
            client_key: &key,
        };
        let raw = serde_json::to_string(&reply).unwrap();
        assert_eq!(
            raw,
            r#"{"name":"box-a","epoch":1,"client_key":"synthetic-key"}"#
        );
        let received: AddReplyReceived = serde_json::from_str(&raw).unwrap();
        assert_eq!(received.name, name);
        assert_eq!(received.epoch, 1);
        assert_eq!(received.client_key, key);
        // The shared receive contract accepts valid JSON escapes too. Key/name
        // admission policy belongs to callers, not the wire schema.
        let escaped: AddReplyReceived =
            serde_json::from_str(r#"{"name":"box-a","epoch":1,"client_key":"synthetic\u002dkey"}"#)
                .unwrap();
        assert_eq!(escaped.client_key, key);
    }

    #[test]
    fn view_required_duplicate_unknown_fields() {
        rejects_missing_duplicate_unknown::<BoxView>(view());
    }

    #[test]
    fn list_required_duplicate_unknown_fields() {
        rejects_missing_duplicate_unknown::<BoxListReply>(json!({"boxes":[view()]}));
    }

    #[test]
    fn add_required_duplicate_unknown_fields_use_shared_consumer() {
        rejects_missing_duplicate_unknown::<AddReplyReceived>(add());
    }

    #[test]
    fn list_rejects_nested_missing_duplicate_unknown_fields() {
        for field in ["name", "epoch", "revoked"] {
            let mut nested = view();
            nested.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<BoxListReply>(json!({"boxes":[nested]})).is_err());
            let raw = format!(
                "{{\"boxes\":[{{\"name\":\"box-a\",\"epoch\":1,\"revoked\":false,{}:{}}}]}}",
                serde_json::to_string(field).unwrap(),
                view()[field],
            );
            assert!(serde_json::from_str::<BoxListReply>(&raw).is_err());
        }
        let mut nested = view();
        nested["unexpected"] = json!(true);
        assert!(serde_json::from_value::<BoxListReply>(json!({"boxes":[nested]})).is_err());
    }

    #[test]
    fn required_fields_reject_null_wrong_types_and_invalid_epochs() {
        for field in ["name", "epoch", "revoked"] {
            let mut value = view();
            value[field] = Value::Null;
            assert!(serde_json::from_value::<BoxView>(value).is_err());
        }
        for field in ["name", "epoch", "client_key"] {
            for invalid in [Value::Null, json!([]), json!({}), json!(false)] {
                let mut value = add();
                value[field] = invalid;
                assert!(serde_json::from_value::<AddReplyReceived>(value).is_err());
            }
        }
        for invalid in [
            Value::Null,
            json!({}),
            json!(false),
            json!("boxes"),
            json!([null]),
        ] {
            assert!(serde_json::from_value::<BoxListReply>(json!({"boxes":invalid})).is_err());
        }
        for invalid in [json!(-1), json!(1.5), json!("1"), json!(true)] {
            let mut box_value = view();
            box_value["epoch"] = invalid.clone();
            assert!(serde_json::from_value::<BoxView>(box_value).is_err());
            let mut add_value = add();
            add_value["epoch"] = invalid;
            assert!(serde_json::from_value::<AddReplyReceived>(add_value).is_err());
        }
        assert!(
            serde_json::from_str::<BoxView>(
                r#"{"name":"box-a","epoch":18446744073709551616,"revoked":false}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<AddReplyReceived>(
                r#"{"name":"box-a","epoch":18446744073709551616,"client_key":"synthetic-key"}"#,
            )
            .is_err()
        );
        let mut bad_name = view();
        bad_name["name"] = json!(1);
        assert!(serde_json::from_value::<BoxView>(bad_name).is_err());
        let mut bad_revoked = view();
        bad_revoked["revoked"] = json!("false");
        assert!(serde_json::from_value::<BoxView>(bad_revoked).is_err());
        let mut bad_key = add();
        bad_key["client_key"] = json!(1);
        assert!(serde_json::from_value::<AddReplyReceived>(bad_key).is_err());
    }
}
