//! Small successful admin acknowledgements, independent of mutation policy.
//! Account leaf generics preserve the producer's existing domain serializers.
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct UpdatedReply {
    pub updated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct ProjectReply {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct ClearedReply {
    pub cleared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoutingMutationReply {
    Updated(UpdatedReply),
    Project(ProjectReply),
}

fn required_plan<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AccountActionReply<P, U, H> {
    pub id: String,
    pub provider: P,
    pub label: String,
    pub email: String,
    #[serde(deserialize_with = "required_plan")]
    pub plan: Option<String>,
    pub user_state: U,
    pub health: H,
    pub drain: bool,
    pub allow_credits: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct ReleaseHoldReply<P, U, H> {
    pub id: String,
    pub released: bool,
    pub provider: P,
    pub label: String,
    pub email: String,
    #[serde(deserialize_with = "required_plan")]
    pub plan: Option<String>,
    pub user_state: U,
    pub health: H,
    pub drain: bool,
    pub allow_credits: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_account::{AdminHealth, AdminProvider, AdminUserState};
    use serde_json::{Value, json};
    type Account = AccountActionReply<AdminProvider, AdminUserState, AdminHealth>;
    type Released = ReleaseHoldReply<AdminProvider, AdminUserState, AdminHealth>;

    fn account(released: bool) -> Value {
        let mut value = json!({"id":"synthetic-account"});
        if released {
            value["released"] = json!(true);
        }
        for (key, field) in json!({"provider":"codex","label":"synthetic-label",
            "email":"synthetic@example.invalid","plan":null,"user_state":"active",
            "health":{"kind":"ready"},"drain":false,"allow_credits":false})
        .as_object()
        .unwrap()
        {
            value[key] = field.clone();
        }
        value
    }

    fn contract<T: serde::de::DeserializeOwned + Serialize>(wire: Value) {
        let decoded: T = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
        for key in wire.as_object().unwrap().keys() {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<T>(missing).is_err(),
                "missing {key}"
            );
            let raw = serde_json::to_string(&wire).unwrap();
            let duplicate = format!(
                "{},{}:{}}}",
                &raw[..raw.len() - 1],
                serde_json::to_string(key).unwrap(),
                wire[key]
            );
            assert!(
                serde_json::from_str::<T>(&duplicate).is_err(),
                "duplicate {key}"
            );
        }
        let mut extra = wire;
        extra["unexpected"] = json!(true);
        assert!(serde_json::from_value::<T>(extra).is_err());
    }

    #[test]
    fn routing_leaves_require_fields_reject_unknowns_and_duplicates() {
        contract::<UpdatedReply>(json!({"updated":true}));
        contract::<ProjectReply>(json!({"id":"synthetic-project"}));
        contract::<ClearedReply>(json!({"cleared":true}));
        for value in [Value::Null, json!(1), json!("true")] {
            assert!(serde_json::from_value::<UpdatedReply>(json!({"updated":value})).is_err());
            assert!(serde_json::from_value::<ClearedReply>(json!({"cleared":value})).is_err());
        }
        assert!(serde_json::from_value::<ProjectReply>(json!({"id":null})).is_err());
        assert!(serde_json::from_value::<ProjectReply>(json!({"id":1})).is_err());
    }

    #[test]
    fn routing_variants_keep_disjoint_exact_shapes() {
        for raw in [r#"{"updated":true}"#, r#"{"id":"synthetic-project"}"#] {
            let reply: RoutingMutationReply = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_string(&reply).unwrap(), raw);
        }
        assert!(
            serde_json::from_value::<RoutingMutationReply>(
                json!({"updated":true,"id":"ambiguous"})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<RoutingMutationReply>(json!({})).is_err());
    }

    #[test]
    fn account_shapes_require_every_key_and_preserve_release_ack() {
        contract::<Account>(account(false));
        contract::<Released>(account(true));
        assert!(serde_json::from_value::<Account>(account(true)).is_err());
        assert!(serde_json::from_value::<Released>(account(false)).is_err());
    }

    #[test]
    fn plan_is_required_nullable_and_other_field_types_stay_typed() {
        let mut wire = account(false);
        for plan in [Value::Null, json!("synthetic-plan")] {
            wire["plan"] = plan;
            let parsed: Account = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), wire);
        }
        for key in [
            "id",
            "provider",
            "label",
            "email",
            "plan",
            "user_state",
            "health",
            "drain",
            "allow_credits",
        ] {
            let mut invalid = account(false);
            invalid[key] = json!(17);
            assert!(serde_json::from_value::<Account>(invalid).is_err());
        }
        let mut invalid = account(true);
        invalid["released"] = json!("true");
        assert!(serde_json::from_value::<Released>(invalid).is_err());
    }

    #[test]
    fn account_wire_order_and_explicit_null_match_producer() {
        for (wire, released) in [(account(false), false), (account(true), true)] {
            let expected = serde_json::to_string(&wire).unwrap();
            let actual = if released {
                serde_json::to_string(&serde_json::from_value::<Released>(wire).unwrap()).unwrap()
            } else {
                serde_json::to_string(&serde_json::from_value::<Account>(wire).unwrap()).unwrap()
            };
            assert_eq!(actual, expected);
        }
    }
}
