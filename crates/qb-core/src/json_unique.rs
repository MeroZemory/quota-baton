//! Reject ambiguous JSON before persisted-state or relay-side classification.
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use std::fmt;
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON with unique object keys")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(Value::Number(v.into())))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Unique, E> {
                Ok(Unique(Value::Number(v.into())))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Unique, E> {
                Number::from_f64(v)
                    .map(|n| Unique(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(Value::String(v.to_owned())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Unique, E> {
                Ok(Unique(Value::String(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Unique, A::Error> {
                let mut out = Vec::new();
                while let Some(value) = a.next_element::<Unique>()? {
                    out.push(value.0);
                }
                Ok(Unique(Value::Array(out)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Unique, A::Error> {
                let mut out = Map::new();
                while let Some(key) = a.next_key::<String>()? {
                    if out.contains_key(&key) {
                        return Err(de::Error::custom("duplicate persisted JSON key"));
                    }
                    out.insert(key, a.next_value::<Unique>()?.0);
                }
                Ok(Unique(Value::Object(out)))
            }
        }
        d.deserialize_any(UniqueVisitor)
    }
}
pub(crate) fn from_slice(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    let mut d = serde_json::Deserializer::from_slice(bytes);
    let value = Unique::deserialize(&mut d)?.0;
    d.end()?;
    Ok(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicates_at_every_object_depth_are_rejected_before_schema_conversion() {
        for raw in [
            r#"{"account":"a","account":"b"}"#,
            r#"{"schema":1,"schema":2}"#,
            r#"{"sessions":{"id":{},"id":{}}}"#,
            r#"[{"mode":"strict","mo\u0064e":"recover"}]"#,
        ] {
            assert!(from_slice(raw.as_bytes()).is_err(), "{raw}");
        }
        let deep = format!("{}0{}", "[".repeat(256), "]".repeat(256));
        assert!(from_slice(deep.as_bytes()).is_err());
        let raw = br#"{"null":null,"bool":true,"array":[-2,18446744073709551615,1.5,"text",{}]}"#;
        assert_eq!(
            from_slice(raw).unwrap(),
            serde_json::from_slice::<Value>(raw).unwrap()
        );
    }
}
