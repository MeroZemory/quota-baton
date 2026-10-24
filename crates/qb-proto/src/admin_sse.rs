//! Exact SSE burst data only. State warnings and ledger envelopes are separate.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WarningKind {
    #[serde(rename = "burst")]
    Burst,
}

/// Zero is a valid observation; the tray retains its positive-count UI gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct BurstWarning {
    pub kind: WarningKind,
    pub burst_new_sessions: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_burst_exact_two_keys_required_count_and_discriminator() {
        for count in [0, 7, u32::MAX] {
            let raw = format!(r#"{{"kind":"burst","burst_new_sessions":{count}}}"#);
            let value: BurstWarning = serde_json::from_str(&raw).unwrap();
            assert_eq!(value.kind, WarningKind::Burst);
            assert_eq!(value.burst_new_sessions, count);
            assert_eq!(serde_json::to_string(&value).unwrap(), raw);
        }
        for raw in [
            r#"{}"#,
            r#"{"kind":"burst"}"#,
            r#"{"burst_new_sessions":3}"#,
            r#"{"kind":"quota_notice","burst_new_sessions":3}"#,
            r#"{"kind":null,"burst_new_sessions":3}"#,
            r#"{"kind":"burst","burst_new_sessions":null}"#,
            r#"{"kind":"burst","burst_new_sessions":-1}"#,
            r#"{"kind":"burst","burst_new_sessions":1.5}"#,
            r#"{"kind":"burst","burst_new_sessions":"3"}"#,
            r#"{"kind":"burst","burst_new_sessions":4294967296}"#,
            r#"{"kind":"burst","kind":"burst","burst_new_sessions":3}"#,
            r#"{"kind":"burst","burst_new_sessions":3,"burst_new_sessions":4}"#,
            r#"{"kind":"burst","burst_new_sessions":3,"observed_at":null}"#,
        ] {
            assert!(serde_json::from_str::<BurstWarning>(raw).is_err(), "{raw}");
        }
    }
}
