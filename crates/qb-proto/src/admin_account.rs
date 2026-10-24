//! Complete current /state.accounts wire schema, independent of presentation.
//! Timestamps/paths stay wire strings; validate/convert at existing domain boundaries.
//! No estimator, clock, IO, routing, or blanket compatibility defaults live here.
use serde::{Deserialize, Deserializer, Serialize};

// Option alone accepts an omitted key. Current nullable producer members are
// required keys: null is allowed, omission is not.
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn present_bool<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}
fn present_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminAccount {
    pub outside_used: bool,
    pub pool_eligible: bool,
    pub takeover_pending: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub quota_cooling: Option<AdminQuotaCooling>,
    pub quota_credit_releasable: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub observed_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub quota_notice: Option<AdminQuotaNotice>,
    pub credits_in_use: bool,
    pub usage_forecast: AccountUsageForecast,
    pub has_alternative: bool,
    pub id: String,
    pub generation: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub last_refresh_at: Option<String>,
    pub provider: AdminProvider,
    pub label: String,
    pub email: String,
    #[serde(deserialize_with = "required_nullable")]
    pub plan: Option<String>,
    pub user_state: AdminUserState,
    pub source: AdminSource,
    pub health: AdminHealth,
    pub probe_pending: bool,
    pub pacing: AdminPacing,
    pub windows: Vec<AdminWindow>,
    #[serde(deserialize_with = "required_nullable")]
    pub credits: Option<AdminCredits>,
    #[serde(deserialize_with = "required_nullable")]
    pub last_used_at: Option<String>,
    pub bound_sessions: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub token_expires_at: Option<String>,
    pub drain: bool,
    pub allow_credits: bool,
    pub refresh_attention: bool,
    pub active_sessions: u64,
    pub spread_threshold: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdminProvider {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdminUserState {
    Active,
    Paused,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub enum AdminSource {
    Oauth,
    Takeover {
        from: String,
        at: String,
    },
    Cpa {
        mode: crate::ImportMode,
        dir: String,
        r#match: AdminSourceMatch,
    },
}

/// Untagged alternatives must reject foreign keys even in production to avoid
/// silently accepting the wrong identity branch and discarding its members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum AdminSourceMatch {
    Claude {
        account_uuid: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_string"
        )]
        organization_uuid: Option<String>,
        email: String,
    },
    Codex {
        account_id: String,
        email: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminHealthReason {
    Quota,
    Rate,
    Auth,
    Permission,
    Payment,
    StoreError,
    LinkStale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub enum AdminHealth {
    Ready {},
    Cooling {
        reason: AdminHealthReason,
        until: String,
        scope: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_bool"
        )]
        probe_pending: Option<bool>,
    },
    NeedsRelogin {
        reason: String,
        since: String,
    },
    #[serde(untagged)]
    Unknown(AdminUnknownHealth),
}

/// Preserve a future health kind without treating it as a known usable state.
/// A malformed known kind must never fall through to this compatibility case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminUnknownHealth {
    pub kind: String,
    #[serde(flatten)]
    pub fields: std::collections::BTreeMap<String, serde_json::Value>,
}

impl<'de> Deserialize<'de> for AdminUnknownHealth {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = std::collections::BTreeMap<String, serde_json::Value>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a health object with unique fields")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut fields = Self::Value::new();
                while let Some((key, value)) = map.next_entry()? {
                    if fields.insert(key, value).is_some() {
                        return Err(M::Error::custom("duplicate health field"));
                    }
                }
                Ok(fields)
            }
        }
        let mut fields = deserializer.deserialize_map(Fields)?;
        let kind = fields
            .remove("kind")
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| D::Error::custom("health kind must be a string"))?;
        if matches!(kind.as_str(), "ready" | "cooling" | "needs_relogin") {
            return Err(D::Error::custom("invalid known health state"));
        }
        Ok(Self { kind, fields })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminQuotaCooling {
    pub window: String,
    pub reset_at: String,
    pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminWindow {
    pub name: String,
    #[serde(deserialize_with = "required_nullable")]
    pub used_pct: Option<f64>,
    #[serde(deserialize_with = "required_nullable")]
    pub reset_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub observed_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminCredits {
    #[serde(deserialize_with = "required_nullable")]
    pub has_credits: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub unlimited: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub balance: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminPacing {
    #[serde(deserialize_with = "required_nullable")]
    pub backoff_until: Option<String>,
    pub step: u8,
    pub release_interval_ms: u64,
    pub waiting: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdminQuotaAction {
    Credits,
    Failover,
    Exhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AdminQuotaNotice {
    pub action: AdminQuotaAction,
    #[serde(deserialize_with = "required_nullable")]
    pub window: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub reset_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub next_account: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub earliest_account: Option<String>,
    pub observed_at: String,
    pub model_limited: bool,
}

/// Wire facts corresponding exactly to core's existing forecast DTOs.
/// Numeric calculation remains in qb-core::usage_forecast. These neutral string
/// timestamp DTOs need explicit domain conversion before any caller migration.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForecastBasis {
    Recent,
    Average,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForecastOutcome {
    Measuring,
    ResetPending,
    Exhausted,
    NoFiniteForecast,
    NoExhaustionBeforeReset,
    Exhaustion,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct UsageProjection<Time = String> {
    pub pct_per_hour: f64,
    pub measurement_start: Time,
    pub exhaustion_at: Option<Time>,
    pub seconds_to_exhaustion: Option<i64>,
    pub outcome: ForecastOutcome,
    pub changing: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct WindowUsageForecast<Time = String> {
    pub name: String,
    pub window_seconds: i64,
    pub observed_at: Time,
    pub used_pct: f64,
    pub reset_at: Option<Time>,
    pub awaiting_usage: bool,
    pub recent_pct_per_hour: Option<f64>,
    pub average_pct_per_hour: Option<f64>,
    pub recent_span_start: Option<Time>,
    pub measurement_start: Option<Time>,
    pub basis: Option<ForecastBasis>,
    pub exhaustion_at: Option<Time>,
    pub seconds_to_exhaustion: Option<i64>,
    pub outcome: ForecastOutcome,
    pub changing: bool,
    pub secondary_recent: Option<UsageProjection<Time>>,
    /// Samples were compacted at capacity; interpolation is approximate but
    /// keeps the requested span covered. Reset/top-up starts a fresh track.
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct CreditUsageForecast<Time = String> {
    pub observed_at: Time,
    pub balance: f64,
    pub recent_credits_per_hour: Option<f64>,
    pub measurement_start: Option<Time>,
    pub exhaustion_at: Option<Time>,
    pub seconds_to_exhaustion: Option<i64>,
    pub outcome: ForecastOutcome,
    pub changing: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct AccountUsageForecast<Time = String> {
    pub as_of: Time,
    pub observed_at: Option<Time>,
    pub windows: Vec<WindowUsageForecast<Time>>,
    /// Only finite exhaustion before a known reset participates. Credits do not.
    pub earliest_window: Option<String>,
    pub credit: Option<CreditUsageForecast<Time>>,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn wire() -> Value {
        json!({
            "outside_used":true,"pool_eligible":false,"takeover_pending":true,
            "quota_cooling":{"window":"7d","reset_at":"2030-01-02T00:00:00Z","started_at":"2030-01-01T00:00:00Z"},
            "quota_credit_releasable":false,"observed_at":"2030-01-01T00:00:00Z",
            "quota_notice":{"action":"exhausted","window":"7d","reset_at":null,
                "next_account":null,"earliest_account":null,"observed_at":"2030-01-01T00:00:00Z","model_limited":true},
            "credits_in_use":false,
            "usage_forecast":{
                "as_of":"2030-01-01T00:00:00Z","observed_at":null,
                "windows":[{
                    "name":"7d","window_seconds":604800,"observed_at":"2030-01-01T00:00:00Z",
                    "used_pct":37.5,"reset_at":"2030-01-02T00:00:00Z","awaiting_usage":false,
                    "recent_pct_per_hour":1.5,"average_pct_per_hour":1.0,
                    "recent_span_start":"2029-12-31T23:30:00Z","measurement_start":"2029-12-31T22:00:00Z",
                    "basis":"recent","exhaustion_at":null,"seconds_to_exhaustion":null,
                    "outcome":"no_exhaustion_before_reset","changing":true,
                    "secondary_recent":{"pct_per_hour":1.5,"measurement_start":"2029-12-31T23:30:00Z",
                        "exhaustion_at":null,"seconds_to_exhaustion":null,"outcome":"no_finite_forecast","changing":false},
                    "truncated":true
                }],
                "earliest_window":null,
                "credit":{"observed_at":"2030-01-01T00:00:00Z","balance":12.5,
                    "recent_credits_per_hour":null,"measurement_start":null,"exhaustion_at":null,
                    "seconds_to_exhaustion":null,"outcome":"measuring","changing":false,"truncated":false},
                "truncated":true
            },
            "has_alternative":false,"id":"sample-acct-r15","generation":9,"last_refresh_at":null,
            "provider":"codex","label":"sample-acct-r15","email":"sample-acct-r15@example.test","plan":null,
            "user_state":"active",
            "source":{"kind":"cpa","mode":"linked","dir":"synthetic-source",
                "match":{"account_id":"sample-acct-origin","email":"sample-acct-r15@example.test"}},
            "health":{"kind":"cooling","reason":"auth","until":"2030-01-01T00:00:00Z","scope":"account","probe_pending":true},
            "probe_pending":true,
            "pacing":{"backoff_until":null,"step":2,"release_interval_ms":500,"waiting":3},
            "windows":[{"name":"7d","used_pct":37.5,"reset_at":null,"observed_at":null,"status":null}],
            "credits":{"has_credits":null,"unlimited":false,"balance":12.5},
            "last_used_at":null,"bound_sessions":4,"token_expires_at":null,"drain":false,
            "allow_credits":true,"refresh_attention":true,"active_sessions":2,"spread_threshold":3
        })
    }

    #[test]
    fn complete_account_roundtrip_keeps_previously_dropped_source_and_runtime_keys() {
        let original = wire();
        let account: AdminAccount = serde_json::from_value(original.clone()).unwrap();
        let serialized = serde_json::to_value(account).unwrap();
        assert_eq!(serialized, original);
        assert_eq!(serialized["source"]["dir"], "synthetic-source");
        assert_eq!(
            serialized["source"]["match"]["account_id"],
            "sample-acct-origin"
        );
        assert_eq!(serialized["generation"], 9);
        assert_eq!(serialized["takeover_pending"], true);
        assert_eq!(serialized["active_sessions"], 2);
        assert_eq!(
            serialized["usage_forecast"]["windows"][0]["secondary_recent"]["pct_per_hour"],
            1.5
        );
        // The incumbent TrayAccount decoder serializes source as "linked" and
        // omits generation/takeover_pending. This assertion must stay on the
        // wire DTO, never on a normalized presentation projection.
        assert!(serialized["source"].is_object());
    }

    #[test]
    fn future_health_roundtrip_preserves_fields_but_never_accepts_malformed_known_state() {
        let future =
            json!({"kind":"future_health","reason":{"detail":"sample-reason"},"since":null});
        let parsed: AdminHealth = serde_json::from_value(future.clone()).unwrap();
        assert!(matches!(&parsed, AdminHealth::Unknown(_)));
        assert_eq!(serde_json::to_value(parsed).unwrap(), future);
        for (case, invalid) in [
            json!({"kind":"cooling","reason":"quota"}),
            json!({"kind":"needs_relogin","reason":"sample-reason"}),
            json!({"kind":"ready","unexpected":true}),
            json!({"reason":"sample-reason"}),
            json!({"kind":null}),
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                serde_json::from_value::<AdminHealth>(invalid).is_err(),
                "case {case}"
            );
        }
        assert!(
            serde_json::from_str::<AdminHealth>(
                r#"{"kind":"future_health","reason":1,"reason":2}"#
            )
            .is_err()
        );
    }

    #[test]
    fn all_32_account_keys_are_required_even_when_nullable() {
        let original = wire();
        assert_eq!(original.as_object().unwrap().len(), 32);
        for key in original.as_object().unwrap().keys() {
            let mut missing = original.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<AdminAccount>(missing).is_err(),
                "accepted missing {key}"
            );
        }
        for key in [
            "quota_cooling",
            "observed_at",
            "quota_notice",
            "last_refresh_at",
            "plan",
            "credits",
            "last_used_at",
            "token_expires_at",
        ] {
            let mut nullable = original.clone();
            nullable[key] = Value::Null;
            let account: AdminAccount = serde_json::from_value(nullable.clone()).unwrap();
            assert_eq!(serde_json::to_value(account).unwrap(), nullable);
        }
        let mut invalid = original;
        invalid["usage_forecast"] = Value::Null;
        assert!(serde_json::from_value::<AdminAccount>(invalid).is_err());
    }

    #[test]
    fn source_variants_and_sparse_health_members_keep_wire_shape() {
        for source in [
            json!({"kind":"oauth"}),
            json!({"kind":"takeover","from":"synthetic-origin","at":"2030-01-01T00:00:00Z"}),
            json!({"kind":"cpa","mode":"adopted","dir":"synthetic-source","match":{"account_uuid":"sample-acct-uuid","email":"sample-acct-r15@example.test"}}),
            json!({"kind":"cpa","mode":"linked","dir":"synthetic-source","match":{"account_uuid":"sample-acct-uuid","organization_uuid":"sample-acct-org","email":"sample-acct-r15@example.test"}}),
        ] {
            let typed: AdminSource = serde_json::from_value(source.clone()).unwrap();
            assert_eq!(serde_json::to_value(typed).unwrap(), source);
        }
        for health in [
            json!({"kind":"ready"}),
            json!({"kind":"needs_relogin","reason":"synthetic-reason","since":"2030-01-01T00:00:00Z"}),
            json!({"kind":"cooling","reason":"quota","until":"2030-01-02T00:00:00Z","scope":"model:sample-model"}),
            json!({"kind":"cooling","reason":"auth","until":"2030-01-02T00:00:00Z","scope":"account","probe_pending":false}),
        ] {
            let typed: AdminHealth = serde_json::from_value(health.clone()).unwrap();
            assert_eq!(serde_json::to_value(typed).unwrap(), health);
        }
        assert!(serde_json::from_value::<AdminSource>(json!("linked")).is_err());
        assert!(serde_json::from_value::<AdminSourceMatch>(json!({
            "account_uuid":"sample-acct-uuid","account_id":"sample-acct-origin","email":"sample-acct-r15@example.test"
        })).is_err());
    }

    #[test]
    fn known_nested_fields_are_required_and_unknown_keys_fail_schema_unit_tests() {
        let original = wire();
        for pointer in [
            "/source",
            "/source/match",
            "/health",
            "/pacing",
            "/windows/0",
            "/credits",
            "/quota_cooling",
            "/quota_notice",
            "/usage_forecast",
            "/usage_forecast/windows/0",
            "/usage_forecast/windows/0/secondary_recent",
            "/usage_forecast/credit",
        ] {
            let object = original.pointer(pointer).unwrap().as_object().unwrap();
            for key in object.keys() {
                if (pointer == "/health" && key == "probe_pending") || key == "kind" {
                    continue;
                }
                let mut missing = original.clone();
                missing
                    .pointer_mut(pointer)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(key);
                let optional_forecast = match pointer {
                    "/usage_forecast" => {
                        matches!(key.as_str(), "observed_at" | "earliest_window" | "credit")
                    }
                    "/usage_forecast/windows/0" => matches!(
                        key.as_str(),
                        "reset_at"
                            | "recent_pct_per_hour"
                            | "average_pct_per_hour"
                            | "recent_span_start"
                            | "measurement_start"
                            | "basis"
                            | "exhaustion_at"
                            | "seconds_to_exhaustion"
                            | "secondary_recent"
                    ),
                    "/usage_forecast/windows/0/secondary_recent" => {
                        matches!(key.as_str(), "exhaustion_at" | "seconds_to_exhaustion")
                    }
                    "/usage_forecast/credit" => matches!(
                        key.as_str(),
                        "recent_credits_per_hour"
                            | "measurement_start"
                            | "exhaustion_at"
                            | "seconds_to_exhaustion"
                    ),
                    _ => false,
                };
                if optional_forecast {
                    let decoded: AdminAccount = serde_json::from_value(missing).unwrap();
                    let encoded = serde_json::to_value(decoded).unwrap();
                    assert!(encoded.pointer(pointer).unwrap()[key].is_null());
                } else {
                    assert!(
                        serde_json::from_value::<AdminAccount>(missing).is_err(),
                        "accepted missing {pointer}/{key}"
                    );
                }
            }
            let mut unknown = original.clone();
            unknown
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown_contract_key".into(), json!(true));
            assert!(
                serde_json::from_value::<AdminAccount>(unknown).is_err(),
                "accepted unknown at {pointer}"
            );
        }
        let mut unknown = original;
        unknown["unknown_contract_key"] = json!(true);
        assert!(serde_json::from_value::<AdminAccount>(unknown).is_err());
    }

    #[test]
    fn cold_forecast_and_unknown_vs_unlimited_credits_are_preserved() {
        for credits in [
            Value::Null,
            json!({"has_credits":null,"unlimited":null,"balance":null}),
            json!({"has_credits":false,"unlimited":false,"balance":0.0}),
            json!({"has_credits":null,"unlimited":true,"balance":null}),
        ] {
            let mut cold = wire();
            cold["credits"] = credits;
            cold["usage_forecast"] = json!({"as_of":"2030-01-01T00:00:00Z","observed_at":null,
                "windows":[],"earliest_window":null,"credit":null,"truncated":false});
            let typed: AdminAccount = serde_json::from_value(cold.clone()).unwrap();
            assert_eq!(serde_json::to_value(typed).unwrap(), cold);
        }
    }
}
