//! Staged /state envelope. Account payload migration remains separate; pool and
//! warning contracts live in admin_pool and admin_warning.
use serde::{Deserialize, Serialize};

// Field order matches the existing json! response with serde_json preserve_order.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminState<AccountPayload, WarningPayload, PoolPayload> {
    pub pools: PoolPayload,
    pub warnings: Vec<WarningPayload>,
    /// Absent in older captures/clients: unknown, never an invented false.
    pub draining: Option<bool>,
    /// Absent in older captures/clients: capabilities are unknown.
    pub capabilities: Option<StateCapabilities>,
    pub accounts: Vec<AccountPayload>,
    pub inflight: StateInflight,
    pub memory: StateMemory,
    pub bindings: StateBindings,
    pub data_port: u16,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct StateCapabilities {
    pub account_adopt: bool,
    pub manual_refresh: bool,
    pub account_takeover: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct StateInflight {
    pub requests: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct StateMemory {
    pub private_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct StateBindings {
    pub count: u64,
}

/// Explicit legacy read boundary. The wire type itself keeps required fields.
/// Reads raw fields so duplicate known keys cannot be hidden by a Value map.
/// Pool defaults are unknown with Option<ReadPools>. Nested contracts are
/// selected by each transport; account migration remains separate.
pub fn deserialize_compat<'de, D, A, W, P>(
    deserializer: D,
    require_accounts: bool,
) -> Result<AdminState<A, W, P>, D::Error>
where
    D: serde::Deserializer<'de>,
    A: Deserialize<'de>,
    W: Deserialize<'de>,
    P: Deserialize<'de> + Default,
{
    deserialize_compat_mode(
        deserializer,
        require_accounts,
        cfg!(any(test, feature = "contract-tests")),
    )
}

/// Archived presentation has a separate, deliberate future-field compatibility
/// policy. Live consumers use deserialize_compat, including strict gate builds.
pub fn deserialize_legacy<'de, D, A, W, P>(
    deserializer: D,
    require_accounts: bool,
) -> Result<AdminState<A, W, P>, D::Error>
where
    D: serde::Deserializer<'de>,
    A: Deserialize<'de>,
    W: Deserialize<'de>,
    P: Deserialize<'de> + Default,
{
    deserialize_compat_mode(deserializer, require_accounts, false)
}

fn deserialize_compat_mode<'de, D, A, W, P>(
    deserializer: D,
    require_accounts: bool,
    reject_unknown_fields: bool,
) -> Result<AdminState<A, W, P>, D::Error>
where
    D: serde::Deserializer<'de>,
    A: Deserialize<'de>,
    W: Deserialize<'de>,
    P: Deserialize<'de> + Default,
{
    struct StateVisitor<A, W, P> {
        reject_unknown_fields: bool,
        require_accounts: bool,
        marker: std::marker::PhantomData<(A, W, P)>,
    }
    impl<'de, A, W, P> serde::de::Visitor<'de> for StateVisitor<A, W, P>
    where
        A: Deserialize<'de>,
        W: Deserialize<'de>,
        P: Deserialize<'de> + Default,
    {
        type Value = AdminState<A, W, P>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an admin state object")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut state = AdminState {
                pools: P::default(),
                warnings: Vec::new(),
                draining: None,
                capabilities: None,
                accounts: Vec::new(),
                inflight: StateInflight::default(),
                memory: StateMemory::default(),
                bindings: StateBindings::default(),
                data_port: 0,
                version: String::new(),
            };
            let mut seen = std::collections::BTreeSet::new();
            macro_rules! read {
                ($field:ident, $value:expr) => {{
                    if !seen.insert(stringify!($field)) {
                        return Err(serde::de::Error::duplicate_field(stringify!($field)));
                    }
                    state.$field = $value;
                }};
            }
            macro_rules! metric {
                ($ty:ident) => {
                    if self.reject_unknown_fields {
                        map.next_value::<CompatMetric<$ty, true>>()?.0
                    } else {
                        map.next_value::<CompatMetric<$ty, false>>()?.0
                    }
                };
            }
            while let Some(field) = map.next_key::<String>()? {
                match field.as_str() {
                    "pools" => read!(pools, map.next_value()?),
                    "warnings" => read!(warnings, map.next_value()?),
                    "draining" => read!(draining, map.next_value()?),
                    "capabilities" => read!(capabilities, map.next_value()?),
                    "accounts" => read!(accounts, map.next_value()?),
                    "inflight" => {
                        read!(inflight, metric!(StateInflight))
                    }
                    "memory" => read!(memory, metric!(StateMemory)),
                    "bindings" => {
                        read!(bindings, metric!(StateBindings))
                    }
                    "data_port" => read!(data_port, map.next_value()?),
                    "version" => read!(version, map.next_value()?),
                    _ => {
                        if self.reject_unknown_fields {
                            return Err(serde::de::Error::unknown_field(
                                &field,
                                &[
                                    "pools",
                                    "warnings",
                                    "draining",
                                    "capabilities",
                                    "accounts",
                                    "inflight",
                                    "memory",
                                    "bindings",
                                    "data_port",
                                    "version",
                                ],
                            ));
                        }
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
            }
            if self.require_accounts && !seen.contains("accounts") {
                return Err(serde::de::Error::missing_field("accounts"));
            }
            Ok(state)
        }
    }
    deserializer.deserialize_map(StateVisitor {
        require_accounts,
        reject_unknown_fields,
        marker: std::marker::PhantomData,
    })
}

// Decode the existing metric types with their historical missing-member defaults
// only at the compatibility boundary; no second metric contract struct is kept.
struct CompatMetric<T, const REJECT_UNKNOWN: bool>(T);
macro_rules! compat_metric {
    ($ty:ident { $($field:ident),+ }) => {
        impl<'de, const REJECT_UNKNOWN: bool> Deserialize<'de> for CompatMetric<$ty, REJECT_UNKNOWN> {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct MetricVisitor<const REJECT_UNKNOWN: bool>;
                impl<'de, const REJECT_UNKNOWN: bool> serde::de::Visitor<'de> for MetricVisitor<REJECT_UNKNOWN> {
                    type Value = $ty;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str(concat!("a ", stringify!($ty), " object"))
                    }
                    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<$ty, M::Error> {
                        let mut metric = $ty::default();
                        let mut seen = std::collections::BTreeSet::new();
                        while let Some(field) = map.next_key::<String>()? {
                            match field.as_str() {
                                $(stringify!($field) => {
                                    if !seen.insert(stringify!($field)) {
                                        return Err(serde::de::Error::duplicate_field(stringify!($field)));
                                    }
                                    metric.$field = map.next_value()?;
                                },)+
                                _ => {
                                    if REJECT_UNKNOWN { return Err(serde::de::Error::unknown_field(&field, &[$(stringify!($field)),+])); }
                                    map.next_value::<serde::de::IgnoredAny>()?;
                                }
                            }
                        }
                        Ok(metric)
                    }
                }
                deserializer.deserialize_map(MetricVisitor::<REJECT_UNKNOWN>).map(Self)
            }
        }
    };
}
compat_metric!(StateInflight { requests, bytes });
compat_metric!(StateMemory { private_bytes });
compat_metric!(StateBindings { count });

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    type State = AdminState<Value, Value, Value>;

    fn wire() -> Value {
        json!({
            "pools":{"codex":{"eligible":1,"cooling":{},"unavailable":0}},
            "warnings":[],
            "draining":false,
            "capabilities":{"account_adopt":true,"manual_refresh":true,"account_takeover":true},
            "accounts":[{"id":"sample-acct","email":"sample-acct@example.test","opaque_nested":{"kept":true}}],
            "inflight":{"requests":0,"bytes":0},
            "memory":{"private_bytes":0},
            "bindings":{"count":0},
            "data_port":12345,
            "version":"synthetic"
        })
    }

    #[test]
    fn state_envelope_roundtrip_keeps_all_keys_and_generic_payloads() {
        let original = wire();
        let state: State = serde_json::from_value(original.clone()).unwrap();
        let encoded = serde_json::to_value(state).unwrap();
        assert_eq!(encoded, original);
        assert_eq!(
            serde_json::to_vec(&encoded).unwrap(),
            serde_json::to_vec(&original).unwrap()
        );
    }

    #[test]
    fn state_envelope_requires_current_fields_and_complete_metric_leaves() {
        for key in [
            "pools",
            "warnings",
            "accounts",
            "inflight",
            "memory",
            "bindings",
            "data_port",
            "version",
        ] {
            let mut bad = wire();
            bad.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<State>(bad).is_err(),
                "accepted missing {key}"
            );
        }
        for (parent, field) in [
            ("inflight", "requests"),
            ("inflight", "bytes"),
            ("memory", "private_bytes"),
            ("bindings", "count"),
            ("capabilities", "account_adopt"),
            ("capabilities", "manual_refresh"),
            ("capabilities", "account_takeover"),
        ] {
            let mut bad = wire();
            bad[parent].as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<State>(bad).is_err(),
                "accepted missing {parent}.{field}"
            );
        }
    }

    #[test]
    fn state_envelope_unknown_keys_are_rejected_in_proto_unit_tests() {
        // cfg(test) is enabled for THIS crate's unit tests, not when qb-proto
        // is merely a dependency of another crate's integration tests.
        for parent in [
            None,
            Some("inflight"),
            Some("memory"),
            Some("bindings"),
            Some("capabilities"),
        ] {
            let mut bad = wire();
            match parent {
                Some(parent) => {
                    bad[parent]["unexpected_envelope_field"] = json!(1);
                }
                None => {
                    bad["unexpected_envelope_field"] = json!(1);
                }
            }
            assert!(serde_json::from_value::<State>(bad).is_err());
        }
    }

    #[test]
    fn older_capability_and_drain_absence_stays_unknown() {
        let mut old = wire();
        old.as_object_mut().unwrap().remove("draining");
        old.as_object_mut().unwrap().remove("capabilities");
        let decoded: State = serde_json::from_value(old).unwrap();
        assert_eq!(decoded.draining, None);
        assert_eq!(decoded.capabilities, None);
    }
}

#[cfg(test)]
mod live_unknown_field_tests {
    use super::*;
    type Wire = AdminState<serde_json::Value, serde_json::Value, serde_json::Value>;
    #[test]
    fn actual_compat_path_rejects_unknown_root_and_each_metric_in_gate() {
        for raw in [
            r#"{"accounts":[],"unknown":1}"#,
            r#"{"accounts":[],"inflight":{"unknown":1}}"#,
            r#"{"accounts":[],"memory":{"unknown":1}}"#,
            r#"{"accounts":[],"bindings":{"unknown":1}}"#,
        ] {
            let mut decoder = serde_json::Deserializer::from_str(raw);
            let result: Result<Wire, _> = deserialize_compat(&mut decoder, true);
            assert!(result.unwrap_err().to_string().contains("unknown field"));
            let mut legacy = serde_json::Deserializer::from_str(raw);
            let result: Wire = deserialize_legacy(&mut legacy, true).unwrap();
            assert!(result.accounts.is_empty());
        }
    }
}
