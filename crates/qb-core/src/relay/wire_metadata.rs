//! Bounded metadata only: serde skips input/output trees without materializing them.
use serde::{
    Deserialize,
    de::{self, Deserializer, Visitor},
};
use std::fmt;

#[derive(Default, Deserialize)]
pub(super) struct Metadata {
    #[serde(default, rename = "type")]
    kind: Option<Small>,
    #[serde(default)]
    model: Option<Small>,
    #[serde(default)]
    previous_response_id: Option<de::IgnoredAny>,
}
struct Small(String);
impl<'de> Deserialize<'de> for Small {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SmallVisitor;
        impl<'de> Visitor<'de> for SmallVisitor {
            type Value = Small;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a metadata string up to 256 bytes")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Small, E> {
                if value.len() > 256 {
                    return Err(E::custom("metadata string exceeds cap"));
                }
                Ok(Small(value.to_owned()))
            }
        }
        deserializer.deserialize_str(SmallVisitor)
    }
}
impl Metadata {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace())
            != Some(b'{')
        {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
    pub fn create(&self) -> bool {
        self.kind.as_ref().is_some_and(|s| s.0 == "response.create")
    }
    pub fn model(&self) -> Option<String> {
        self.model
            .as_ref()
            .and_then(|s| crate::observability::safe_identifier(&s.0))
    }
    pub fn continuation(&self) -> bool {
        self.previous_response_id.is_some()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_ignores_large_input_and_field_order_but_keeps_model_and_continuation() {
        let bytes = format!(
            r#"{{"input":"{}","model":"blocked","type":"response.create","previous_response_id":"prior"}}"#,
            "x".repeat(1024 * 1024 + 1)
        );
        let meta = Metadata::parse(bytes.as_bytes()).unwrap();
        assert!(meta.create());
        assert_eq!(meta.model().as_deref(), Some("blocked"));
        assert!(meta.continuation());
        assert!(Metadata::parse(br#"{"model":"partial""#).is_none());
        assert!(Metadata::parse(br#"["response.create","blocked",null]"#).is_none());
        assert!(
            !Metadata::parse(br#"{"previous_response_id":null}"#)
                .unwrap()
                .continuation()
        );
    }
}
