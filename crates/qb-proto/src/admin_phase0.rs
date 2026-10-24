//! Exact redacted Phase 0 replay response, separate from routing replay tapes.
use serde::de::{self, MapAccess, Visitor, value::MapAccessDeserializer};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, marker::PhantomData};

// Structs normally accept positional arrays. These route objects do not.
fn object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct ObjectVisitor<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
        type Value = T;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a named JSON object")
        }
        fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<T, M::Error> {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(ObjectVisitor(PhantomData))
}

struct Object<T>(T);
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        object(deserializer).map(Self)
    }
}
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
fn required_nullable_object<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Object<T>>::deserialize(deserializer).map(|value| value.map(|value| value.0))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReplaySummary {
    pub status: u16,
    pub account: String,
    pub error: Option<ReplayError>,
    pub usage: ReplayUsage,
    pub strip_encrypted: ReplayStripMode,
    pub removed_counts: RemovedCounts,
    pub tampered_type: Option<String>,
}
impl<'de> Deserialize<'de> for ReplaySummary {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            status: u16,
            account: String,
            #[serde(deserialize_with = "required_nullable_object")]
            error: Option<ReplayError>,
            usage: ReplayUsage,
            strip_encrypted: ReplayStripMode,
            #[serde(deserialize_with = "object")]
            removed_counts: RemovedCounts,
            #[serde(deserialize_with = "required_nullable")]
            tampered_type: Option<String>,
        }
        let fields: Fields = object(deserializer)?;
        Ok(Self {
            status: fields.status,
            account: fields.account,
            error: fields.error,
            usage: fields.usage,
            strip_encrypted: fields.strip_encrypted,
            removed_counts: fields.removed_counts,
            tampered_type: fields.tampered_type,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayError {
    #[serde(rename = "type", deserialize_with = "required_nullable")]
    pub kind: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub code: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayStripMode {
    None,
    Reasoning,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RemovedCounts {
    Codex(CodexRemovedCounts),
    Claude(ClaudeRemovedCounts),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexRemovedCounts {
    pub reasoning: u64,
    pub compaction: u64,
    pub message: u64,
    pub other: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeRemovedCounts {
    pub thinking: u64,
    pub redacted_thinking: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachedTokens {
    pub cached_tokens: u64,
}

// At most four fields. Keep first-observed order across successive upstream
// events, including when a later event adds a previously absent counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayUsage(Vec<UsageField>);
#[derive(Clone, Debug, PartialEq, Eq)]
enum UsageField {
    Input(u64),
    CacheRead(u64),
    CacheCreation(u64),
    InputDetails(CachedTokens),
}
impl Serialize for ReplayUsage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for field in &self.0 {
            match field {
                UsageField::Input(n) => map.serialize_entry("input_tokens", n)?,
                UsageField::CacheRead(n) => map.serialize_entry("cache_read_input_tokens", n)?,
                UsageField::CacheCreation(n) => {
                    map.serialize_entry("cache_creation_input_tokens", n)?
                }
                UsageField::InputDetails(n) => map.serialize_entry("input_tokens_details", n)?,
            }
        }
        map.end()
    }
}
impl<'de> Deserialize<'de> for ReplayUsage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UsageVisitor;
        impl<'de> Visitor<'de> for UsageVisitor {
            type Value = ReplayUsage;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("the bounded redacted usage object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut fields = Vec::with_capacity(4);
                let mut seen = 0u8;
                while let Some(key) = map.next_key::<String>()? {
                    let bit = match key.as_str() {
                        "input_tokens" => 1,
                        "cache_read_input_tokens" => 2,
                        "cache_creation_input_tokens" => 4,
                        "input_tokens_details" => 8,
                        _ => {
                            return Err(de::Error::unknown_field(
                                &key,
                                &[
                                    "input_tokens",
                                    "cache_read_input_tokens",
                                    "cache_creation_input_tokens",
                                    "input_tokens_details",
                                ],
                            ));
                        }
                    };
                    if seen & bit != 0 {
                        return Err(de::Error::custom("duplicate replay usage field"));
                    }
                    seen |= bit;
                    fields.push(match bit {
                        1 => UsageField::Input(map.next_value()?),
                        2 => UsageField::CacheRead(map.next_value()?),
                        4 => UsageField::CacheCreation(map.next_value()?),
                        _ => UsageField::InputDetails(map.next_value::<Object<CachedTokens>>()?.0),
                    });
                }
                Ok(ReplayUsage(fields))
            }
        }
        deserializer.deserialize_map(UsageVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn complete() -> Value {
        json!({"status":200,"account":"synthetic-account",
            "error":{"type":null,"code":null,"message":null},
            "usage":{"input_tokens":0,"cache_read_input_tokens":1,
                "cache_creation_input_tokens":2,"input_tokens_details":{"cached_tokens":3}},
            "strip_encrypted":"none",
            "removed_counts":{"reasoning":0,"compaction":0,"message":0,"other":0},
            "tampered_type":null})
    }
    fn rejected(value: Value) {
        assert!(serde_json::from_str::<ReplaySummary>(&value.to_string()).is_err());
    }
    #[test]
    fn exact_roundtrip_preserves_nulls_omissions_provider_counts_and_usage_order() {
        for counts in [
            json!({"reasoning":0,"compaction":1,"message":2,"other":3}),
            json!({"thinking":0,"redacted_thinking":1}),
        ] {
            for usage in [
                json!({}),
                json!({"input_tokens_details":{"cached_tokens":0},
                "cache_creation_input_tokens":4,"input_tokens":2,"cache_read_input_tokens":3}),
            ] {
                for error in [
                    Value::Null,
                    json!({"type":null,"code":"synthetic_code","message":null}),
                ] {
                    for strip in ["none", "reasoning", "all"] {
                        let mut value = complete();
                        value["removed_counts"] = counts.clone();
                        value["usage"] = usage.clone();
                        value["error"] = error.clone();
                        value["strip_encrypted"] = json!(strip);
                        let tampered = if counts.get("thinking").is_some() {
                            "thinking"
                        } else {
                            "compaction"
                        };
                        for tamper in [Value::Null, json!(tampered)] {
                            value["tampered_type"] = tamper;
                            let raw = value.to_string();
                            let reply: ReplaySummary = serde_json::from_str(&raw).unwrap();
                            assert_eq!(serde_json::to_string(&reply).unwrap(), raw);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn required_and_nullable_fields_are_not_defaults() {
        for field in [
            "status",
            "account",
            "error",
            "usage",
            "strip_encrypted",
            "removed_counts",
            "tampered_type",
        ] {
            let mut value = complete();
            value.as_object_mut().unwrap().remove(field);
            rejected(value);
        }
        for (path, fields) in [
            ("/error", &["type", "code", "message"][..]),
            (
                "/removed_counts",
                &["reasoning", "compaction", "message", "other"][..],
            ),
            ("/usage/input_tokens_details", &["cached_tokens"][..]),
        ] {
            for field in fields {
                let mut value = complete();
                value
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(*field);
                rejected(value);
            }
        }
    }
    #[test]
    fn every_object_boundary_rejects_unknowns_arrays_and_duplicate_members() {
        for path in [
            "",
            "/error",
            "/usage",
            "/usage/input_tokens_details",
            "/removed_counts",
        ] {
            let mut value = complete();
            value
                .pointer_mut(path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unexpected".into(), json!(0));
            rejected(value);
            let mut value = complete();
            let positional = value
                .pointer(path)
                .unwrap()
                .as_object()
                .unwrap()
                .values()
                .cloned()
                .collect::<Vec<_>>();
            *value.pointer_mut(path).unwrap() = Value::Array(positional);
            rejected(value);
            let value = complete();
            let target = value.pointer(path).unwrap();
            let (key, member) = target.as_object().unwrap().iter().next().unwrap();
            let target_raw = target.to_string();
            let duplicate = format!(
                "{{{}:{},{}",
                serde_json::to_string(key).unwrap(),
                member,
                &target_raw[1..]
            );
            let raw = value.to_string().replacen(&target_raw, &duplicate, 1);
            assert!(serde_json::from_str::<ReplaySummary>(&raw).is_err());
        }
        let mut value = complete();
        value["removed_counts"] = json!({"thinking":0,"redacted_thinking":0});
        for field in ["thinking", "redacted_thinking"] {
            let mut missing = value.clone();
            missing["removed_counts"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            rejected(missing);
        }
        let mixed = value
            .to_string()
            .replace("\"thinking\":0", "\"thinking\":0,\"reasoning\":0");
        assert!(serde_json::from_str::<ReplaySummary>(&mixed).is_err());
        let duplicate = value
            .to_string()
            .replace("\"thinking\":0", "\"thinking\":0,\"thinking\":1");
        assert!(serde_json::from_str::<ReplaySummary>(&duplicate).is_err());
    }
    #[test]
    fn invalid_numbers_types_and_trailing_json_fail() {
        for path in [
            "/usage/input_tokens",
            "/usage/cache_read_input_tokens",
            "/usage/cache_creation_input_tokens",
            "/usage/input_tokens_details/cached_tokens",
            "/removed_counts/reasoning",
        ] {
            for invalid in [Value::Null, json!(-1), json!(1.5), json!("1"), json!(true)] {
                let mut value = complete();
                *value.pointer_mut(path).unwrap() = invalid;
                rejected(value);
            }
        }
        for path in ["/error/type", "/error/code", "/error/message"] {
            let mut value = complete();
            *value.pointer_mut(path).unwrap() = json!(0);
            rejected(value);
        }
        for (field, invalid) in [
            ("status", json!(65536)),
            ("account", json!(0)),
            ("strip_encrypted", json!("unsupported")),
            ("tampered_type", json!(true)),
        ] {
            let mut value = complete();
            value[field] = invalid;
            rejected(value);
        }
        let overflow = complete().to_string().replacen(
            "\"input_tokens\":0",
            "\"input_tokens\":18446744073709551616",
            1,
        );
        assert!(serde_json::from_str::<ReplaySummary>(&overflow).is_err());
        assert!(serde_json::from_str::<ReplaySummary>(&format!("{} {{}}", complete())).is_err());
    }
}
