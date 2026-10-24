//! Routing GET wire graph. Separate from persisted policy and edit defaults.
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeMap};

/// JSON object encounter order is wire-visible; duplicate IDs are never merged.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingMap<T>(Vec<(String, T)>);
impl<T: Serialize> Serialize for RoutingMap<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for RoutingMap<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
            type Value = RoutingMap<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object with unique routing IDs")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut seen = std::collections::BTreeSet::new();
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, T>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate routing ID"));
                    }
                    entries.push((key, value));
                }
                Ok(RoutingMap(entries))
            }
        }
        deserializer.deserialize_map(Visitor(std::marker::PhantomData))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedBool<const VALUE: bool>;
impl<const VALUE: bool> Serialize for FixedBool<VALUE> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(VALUE)
    }
}
impl<'de, const VALUE: bool> Deserialize<'de> for FixedBool<VALUE> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? == VALUE {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("unexpected routing forecast flag"))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    Strict,
    Recover,
    Rebalance,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum On5hLimit {
    Wait,
    Move,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct RoutingSettings {
    pub mode: RoutingMode,
    pub reset_wait_secs: u64,
    pub on_5h_limit: On5hLimit,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct RoutingProject {
    pub name: String,
    pub settings: RoutingSettings,
}
fn required_nullable<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct RoutingPlan {
    pub settings: RoutingSettings,
    #[serde(deserialize_with = "required_nullable")]
    pub anchor: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub project: Option<String>,
    pub source: String,
    pub revision: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct ObservedSession {
    pub id: String,
    pub account: String,
    pub policy: RoutingPlan,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct RoutingGet {
    pub revision: u64,
    pub defaults: RoutingSettings,
    pub projects: RoutingMap<RoutingProject>,
    pub sessions: RoutingMap<RoutingPlan>,
    pub observed_sessions: Vec<ObservedSession>,
    pub forecast_shadow: Vec<AccountForecast>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AccountForecast {
    pub account: String,
    pub forecast: Forecast,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Forecast {
    Disabled(DisabledForecast),
    Enabled(EnabledForecast),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct DisabledForecast {
    pub enabled: FixedBool<false>,
    pub placement_effect: FixedBool<false>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct EnabledForecast {
    pub enabled: FixedBool<true>,
    pub placement_effect: FixedBool<false>,
    pub active_groups: u64,
    pub external_traffic: ExternalTraffic,
    pub new_demand_reservation: NewDemandReservation,
    pub windows: Vec<ForecastWindow>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalTraffic {
    NotAttributed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewDemandReservation {
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ForecastWindow {
    Fallback(FallbackWindow),
    Shadow(ShadowWindow),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackStatus {
    Fallback,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowStatus {
    Shadow,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Calibration {
    Uncalibrated,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    ExistingDemandExhaustionRisk,
    PossibleSpareCapacity,
    Uncertain,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct FallbackWindow {
    pub window: String,
    pub status: FallbackStatus,
    pub reason: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct ShadowWindow {
    pub window: String,
    pub status: ShadowStatus,
    pub calibration: Calibration,
    pub classification: Classification,
    pub remaining_pp: f64,
    pub demand_lower_pp: f64,
    pub demand_upper_pp: f64,
    pub margin_pp: u64,
    pub horizon_secs: i64,
    pub samples: u64,
    pub evaluated: u64,
    pub covered: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn fixture() -> Value {
        let settings = json!({"mode":"recover","reset_wait_secs":7,"on_5h_limit":"move"});
        let plan = json!({"settings":settings,"anchor":null,"project":null,"source":"session","revision":2});
        let fallback = json!({"window":"7d","status":"fallback","reason":"unsupported_window"});
        let shadow = json!({"window":"5h","status":"shadow","calibration":"uncalibrated","classification":"uncertain","remaining_pp":20.0,"demand_lower_pp":1.0,"demand_upper_pp":4.0,"margin_pp":3,"horizon_secs":150,"samples":9,"evaluated":0,"covered":0});
        json!({"revision":2,"defaults":settings,
            "projects":{"z":{"name":"synthetic-z","settings":settings},"a":{"name":"synthetic-a","settings":settings}},
            "sessions":{"z":plan,"a":plan},
            "observed_sessions":[{"id":"synthetic","account":"synthetic-account","policy":plan}],
            "forecast_shadow":[{"account":"synthetic-disabled","forecast":{"enabled":false,"placement_effect":false}},
            {"account":"synthetic-enabled","forecast":{"enabled":true,"placement_effect":false,"active_groups":2,"external_traffic":"not_attributed","new_demand_reservation":"unknown","windows":[fallback,shadow]}}]})
    }
    fn check<T: serde::de::DeserializeOwned + Serialize>(wire: Value) {
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
    fn complete_graph_preserves_field_map_order_and_explicit_nulls() {
        let wire = fixture();
        let raw = serde_json::to_string(&wire).unwrap();
        let decoded: RoutingGet = serde_json::from_str(&raw).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), raw);
        assert_eq!(decoded.projects.0[0].0, "z");
        assert!(decoded.sessions.0[0].1.anchor.is_none());
        assert!(decoded.sessions.0[0].1.project.is_none());
    }

    #[test]
    fn every_object_requires_keys_and_rejects_unknowns_and_duplicates() {
        let v = fixture();
        check::<RoutingGet>(v.clone());
        check::<RoutingSettings>(v["defaults"].clone());
        check::<RoutingProject>(v["projects"]["z"].clone());
        check::<RoutingPlan>(v["sessions"]["z"].clone());
        check::<ObservedSession>(v["observed_sessions"][0].clone());
        check::<AccountForecast>(v["forecast_shadow"][0].clone());
        check::<DisabledForecast>(v["forecast_shadow"][0]["forecast"].clone());
        check::<EnabledForecast>(v["forecast_shadow"][1]["forecast"].clone());
        check::<FallbackWindow>(v["forecast_shadow"][1]["forecast"]["windows"][0].clone());
        check::<ShadowWindow>(v["forecast_shadow"][1]["forecast"]["windows"][1].clone());
    }

    #[test]
    fn ordered_maps_reject_duplicate_ids_and_nonobjects() {
        for raw in [r#"{"z":1,"a":2}"#, "{}"] {
            let map: RoutingMap<u64> = serde_json::from_str(raw).unwrap();
            assert_eq!(serde_json::to_string(&map).unwrap(), raw);
        }
        for raw in [r#"{"same":1,"same":2}"#, "[]", "null", r#"{"a":null}"#] {
            assert!(serde_json::from_str::<RoutingMap<u64>>(raw).is_err());
        }
    }

    #[test]
    fn nullable_plan_keys_are_required_and_settings_never_default() {
        let mut v = fixture()["sessions"]["z"].clone();
        for key in ["anchor", "project"] {
            v[key] = json!("synthetic");
            let decoded: RoutingPlan = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), v);
            v[key] = json!(7);
            assert!(serde_json::from_value::<RoutingPlan>(v.clone()).is_err());
            v[key] = Value::Null;
        }
        let mut settings = fixture()["defaults"].clone();
        settings.as_object_mut().unwrap().remove("on_5h_limit");
        assert!(serde_json::from_value::<RoutingSettings>(settings).is_err());
        for key in ["mode", "on_5h_limit"] {
            let mut settings = fixture()["defaults"].clone();
            settings[key] = json!("future-setting");
            assert!(serde_json::from_value::<RoutingSettings>(settings).is_err());
        }
    }

    #[test]
    fn forecast_variants_cannot_collapse_or_gain_placement_authority() {
        let v = fixture();
        for wire in [
            v["forecast_shadow"][0]["forecast"].clone(),
            v["forecast_shadow"][1]["forecast"].clone(),
        ] {
            let raw = serde_json::to_string(&wire).unwrap();
            let parsed: Forecast = serde_json::from_str(&raw).unwrap();
            assert_eq!(serde_json::to_string(&parsed).unwrap(), raw);
            let mut invalid = wire;
            invalid["placement_effect"] = json!(true);
            assert!(serde_json::from_value::<Forecast>(invalid).is_err());
        }
        let mut mixed = v["forecast_shadow"][1]["forecast"].clone();
        mixed["enabled"] = json!(false);
        assert!(serde_json::from_value::<Forecast>(mixed).is_err());
        assert!(
            serde_json::from_value::<Forecast>(json!({"enabled":true,"placement_effect":false}))
                .is_err()
        );
        for window in v["forecast_shadow"][1]["forecast"]["windows"]
            .as_array()
            .unwrap()
        {
            let raw = serde_json::to_string(window).unwrap();
            let decoded: ForecastWindow = serde_json::from_str(&raw).unwrap();
            assert_eq!(serde_json::to_string(&decoded).unwrap(), raw);
            let mut mixed = window.clone();
            mixed["status"] = json!("different-status");
            assert!(serde_json::from_value::<ForecastWindow>(mixed).is_err());
        }
        let mut shadow = v["forecast_shadow"][1]["forecast"]["windows"][1].clone();
        shadow["remaining_pp"] = Value::Null;
        assert!(serde_json::from_value::<ForecastWindow>(shadow).is_err());
    }
}
