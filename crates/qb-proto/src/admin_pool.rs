//! Provider counts. Preserve cooling-map encounter order and every reason key.
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeMap};

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminPools<Counts = AdminPoolCounts> {
    pub claude: Counts,
    pub codex: Counts,
}

impl<Counts> AdminPools<Counts> {
    pub fn get(&self, provider: &str) -> Option<&Counts> {
        match provider {
            "claude" => Some(&self.claude),
            "codex" => Some(&self.codex),
            _ => None,
        }
    }
}

/// Explicit legacy read projection of the same fields. Absent/null providers,
/// counters and cooling maps stay unknown; only the producer uses known zeros.
pub type ReadPoolCounts = AdminPoolCounts<Option<u64>, Option<CoolingCounts>>;
pub type ReadPools = AdminPools<Option<ReadPoolCounts>>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminPoolCounts<Count = u64, Cooling = CoolingCounts> {
    pub eligible: Count,
    pub cooling: Cooling,
    pub unavailable: Count,
}

/// A typed JSON map with the producer's insertion order. A sorted map would
/// reorder the current wire response. Unknown future reason names remain data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoolingCounts(Vec<(String, u64)>);

impl CoolingCounts {
    pub fn increment(&mut self, reason: &str) {
        if let Some((_, count)) = self.0.iter_mut().find(|(name, _)| name == reason) {
            *count += 1;
        } else {
            self.0.push((reason.to_owned(), 1));
        }
    }

    pub fn get(&self, reason: &str) -> Option<u64> {
        self.0
            .iter()
            .find(|(name, _)| name == reason)
            .map(|(_, count)| *count)
    }

    /// Capture allowlisting must still remove unrecognized reason names.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.0.retain(|(reason, _)| keep(reason));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.0.iter().map(|(name, count)| (name.as_str(), *count))
    }
}

impl Serialize for CoolingCounts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (reason, count) in &self.0 {
            map.serialize_entry(reason, count)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for CoolingCounts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CountsVisitor;
        impl<'de> serde::de::Visitor<'de> for CountsVisitor {
            type Value = CoolingCounts;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a map of cooling reasons to unsigned counts")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut counts = CoolingCounts::default();
                while let Some((reason, count)) = map.next_entry::<String, u64>()? {
                    if counts.get(&reason).is_some() {
                        return Err(serde::de::Error::custom("duplicate cooling reason"));
                    }
                    counts.0.push((reason, count));
                }
                Ok(counts)
            }
        }
        deserializer.deserialize_map(CountsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pools_keep_current_field_order_and_cooling_encounter_order() {
        let raw = r#"{"claude":{"eligible":1,"cooling":{"rate":2,"auth":1,"unknown":1},"unavailable":3},"codex":{"eligible":0,"cooling":{},"unavailable":0}}"#;
        let pools: AdminPools = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&pools).unwrap(), raw);
        assert_eq!(pools.claude.cooling.get("rate"), Some(2));
    }

    #[test]
    fn cooling_counts_preserve_future_reason_names_but_reject_invalid_counts_and_duplicates() {
        let raw = r#"{"future_reason":2,"rate":1}"#;
        let counts: CoolingCounts = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&counts).unwrap(), raw);
        for invalid in [
            r#"{"rate":1,"rate":2}"#,
            r#"{"rate":-1}"#,
            r#"{"rate":null}"#,
            r#"{"rate":"1"}"#,
            "null",
            "[]",
        ] {
            assert!(
                serde_json::from_str::<CoolingCounts>(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn pool_contract_requires_provider_and_counter_fields() {
        let raw = serde_json::json!({"claude":{"eligible":0,"cooling":{},"unavailable":0},"codex":{"eligible":0,"cooling":{},"unavailable":0}});
        for provider in ["claude", "codex"] {
            let mut missing = raw.clone();
            missing.as_object_mut().unwrap().remove(provider);
            assert!(serde_json::from_value::<AdminPools>(missing).is_err());
            for field in ["eligible", "cooling", "unavailable"] {
                let mut missing = raw.clone();
                missing[provider].as_object_mut().unwrap().remove(field);
                assert!(serde_json::from_value::<AdminPools>(missing).is_err());
            }
        }
    }

    #[test]
    fn legacy_projection_never_invents_counts_or_providers() {
        let pools: ReadPools = serde_json::from_str(r#"{"codex":{"eligible":2}}"#).unwrap();
        assert!(pools.claude.is_none());
        let codex = pools.codex.unwrap();
        assert_eq!(codex.eligible, Some(2));
        assert!(codex.unavailable.is_none());
        assert!(codex.cooling.is_none());
        let empty: ReadPools = serde_json::from_str("{}").unwrap();
        assert!(empty.claude.is_none() && empty.codex.is_none());
        for raw in [r#"{"codex":null}"#, r#"{"codex":{"eligible":null}}"#] {
            let pools: ReadPools = serde_json::from_str(raw).unwrap();
            assert_eq!(pools.codex.as_ref().and_then(|p| p.eligible), None);
        }
        for raw in [
            r#"{"codex":{"eligible":null,"eligible":2}}"#,
            r#"{"codex":{"eligible":1,"eligible":2}}"#,
            r#"{"codex":null,"codex":{"eligible":2}}"#,
        ] {
            assert!(serde_json::from_str::<ReadPools>(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn known_pool_objects_reject_unknown_fields_in_schema_tests() {
        let raw = r#"{"claude":{"eligible":0,"cooling":{},"unavailable":0,"unexpected":1},"codex":{"eligible":0,"cooling":{},"unavailable":0}}"#;
        assert!(serde_json::from_str::<AdminPools>(raw).is_err());
    }
}
