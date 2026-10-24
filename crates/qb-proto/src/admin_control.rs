//! Response envelopes only; lifecycle and pin ownership remain in qb-core.
use serde::{Deserialize, Serialize};

/// Keep binding leaves on their existing domain serialization path.
/// This envelope is not a lease-release receipt or permission to restore state.
#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct LifecycleReply<Bindings> {
    pub bindings: Bindings,
    pub lease_released: bool,
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct DebugPinReply {
    pub ok: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn exact_order_and_binding_leaf_omissions_are_preserved() {
        for raw in [
            r#"{"bindings":[],"lease_released":true}"#,
            r#"{"bindings":[{"key":"codex:synthetic","account":"synthetic-account","last_used":"2026-10-24T00:00:00Z"}],"lease_released":true}"#,
        ] {
            let decoded: LifecycleReply<Value> = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_string(&decoded).unwrap(), raw);
        }
        let raw = r#"{"ok":true}"#;
        let decoded: DebugPinReply = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), raw);
    }

    #[test]
    fn known_envelope_fields_are_required_typed_and_not_duplicate_collapsed() {
        for raw in [
            r#"{}"#,
            r#"{"bindings":[]}"#,
            r#"{"lease_released":true}"#,
            r#"{"bindings":[],"lease_released":null}"#,
            r#"{"bindings":[],"lease_released":"true"}"#,
            r#"{"bindings":[],"bindings":[],"lease_released":true}"#,
            r#"{"bindings":[],"lease_released":false,"lease_released":true}"#,
        ] {
            assert!(serde_json::from_str::<LifecycleReply<Vec<Value>>>(raw).is_err());
        }
        for raw in [
            r#"{}"#,
            r#"{"ok":null}"#,
            r#"{"ok":1}"#,
            r#"{"ok":false,"ok":true}"#,
            r#"{"ok":true,"extra":true}"#,
        ] {
            assert!(serde_json::from_str::<DebugPinReply>(raw).is_err());
        }
        assert!(
            serde_json::from_str::<LifecycleReply<Value>>(
                r#"{"bindings":[],"lease_released":true,"extra":true}"#,
            )
            .is_err()
        );
    }
}
