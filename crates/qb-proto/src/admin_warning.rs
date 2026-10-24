//! Exact /state warning payloads. SSE notification context is a separate protocol.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub enum AdminWarning<Time = String> {
    Burst {
        burst_new_sessions: u64,
        observed_at: Time,
    },
    MoveReread {
        observed_at: Time,
        reread_tokens: u64,
        known_count: u64,
        unknown_count: u64,
    },
    MoveBreaker {
        observed_at: Time,
        move_list: Vec<AdminMovePair>,
        until: Time,
        session: String,
        from: String,
        to: String,
        reason: String,
        forced: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminMovePair {
    pub from: String,
    pub to: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn fixtures() -> Vec<Value> {
        vec![
            json!({"kind":"burst","burst_new_sessions":33,"observed_at":"2030-01-01T00:00:00Z"}),
            json!({"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100_001,"known_count":2,"unknown_count":1}),
            json!({"kind":"move_breaker","observed_at":"2030-01-01T00:00:00Z","move_list":[{"from":"sample-acct-a","to":"sample-acct-b"}],"until":"2030-01-01T00:04:00Z","session":"0123456789ab","from":"sample-acct-a","to":"sample-acct-b","reason":"global_trip","forced":true}),
        ]
    }

    #[test]
    fn every_current_warning_retains_all_wire_keys() {
        for wire in fixtures() {
            let warning: AdminWarning = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(warning).unwrap(), wire);
        }
    }

    #[test]
    fn current_warning_fields_are_required_and_nonnullable() {
        for wire in fixtures() {
            for key in wire
                .as_object()
                .unwrap()
                .keys()
                .filter(|k| k.as_str() != "kind")
            {
                let mut missing = wire.clone();
                missing.as_object_mut().unwrap().remove(key);
                assert!(
                    serde_json::from_value::<AdminWarning>(missing).is_err(),
                    "accepted missing {key}"
                );
                let mut null = wire.clone();
                null[key] = Value::Null;
                assert!(
                    serde_json::from_value::<AdminWarning>(null).is_err(),
                    "accepted null {key}"
                );
            }
        }
    }

    #[test]
    fn warning_unit_strictness_rejects_unknown_payload_and_pair_keys() {
        for mut wire in fixtures() {
            wire["unexpected"] = json!(true);
            assert!(serde_json::from_value::<AdminWarning>(wire).is_err());
        }
        let mut breaker = fixtures().pop().unwrap();
        breaker["move_list"][0]["unexpected"] = json!(true);
        assert!(serde_json::from_value::<AdminWarning>(breaker).is_err());
        assert!(serde_json::from_value::<AdminWarning>(json!({"kind":"unrecognized"})).is_err());
    }

    #[test]
    fn warnings_preserve_raw_duplicate_rejection_and_unknown_count() {
        for raw in [
            r#"{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":0,"unknown_count":1}"#,
            r#"{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":null,"unknown_count":1}"#,
        ] {
            assert!(serde_json::from_str::<AdminWarning>(raw).is_err());
        }
        let warning: AdminWarning = serde_json::from_value(fixtures()[1].clone()).unwrap();
        match warning {
            AdminWarning::MoveReread { unknown_count, .. } => assert_eq!(unknown_count, 1),
            _ => panic!("wrong warning variant"),
        }
    }
}

#[cfg(test)]
mod field_order_tests {
    use super::*;

    #[test]
    fn every_variant_keeps_the_existing_state_producer_field_order() {
        for raw in [
            r#"{"kind":"burst","burst_new_sessions":33,"observed_at":"2030-01-01T00:00:00Z"}"#,
            r#"{"kind":"move_reread","observed_at":"2030-01-01T00:00:00Z","reread_tokens":100001,"known_count":2,"unknown_count":1}"#,
            r#"{"kind":"move_breaker","observed_at":"2030-01-01T00:00:00Z","move_list":[{"from":"a","to":"b"}],"until":"2030-01-01T00:04:00Z","session":"0123456789ab","from":"a","to":"b","reason":"global_trip","forced":true}"#,
        ] {
            let warning: AdminWarning = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_string(&warning).unwrap(), raw);
        }
    }
}
