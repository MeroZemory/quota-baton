//! Current history response field graphs. Domain aliases provide validated
//! timestamps, IDs and enums; no persistence defaults or presentation caches.
use serde::{Deserialize, Deserializer, Serialize};

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct ActivityPage<Event> {
    pub events: Vec<Event>,
    pub next_seq: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
#[serde(bound(
    deserialize = "Time: Deserialize<'de>, Kind: Deserialize<'de>, Provider: Deserialize<'de>, Account: Deserialize<'de>, Data: Deserialize<'de>"
))]
pub struct ActivityEvent<Time, Kind, Provider, Account, Data> {
    pub seq: u64,
    pub ts: Time,
    pub kind: Kind,
    pub provider: Provider,
    #[serde(deserialize_with = "required_nullable")]
    pub account: Option<Account>,
    #[serde(deserialize_with = "required_nullable")]
    pub account_to: Option<Account>,
    pub notable: bool,
    pub data: Data,
}

/// Sparse optional members omit None; present null is not a producer spelling.
/// The six counters/flags are always emitted and required when reading a response.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
#[serde(bound(
    deserialize = "Time: Deserialize<'de>, Session: Deserialize<'de>, Window: Deserialize<'de>, Reason: Deserialize<'de>, Attention: Deserialize<'de>, Outcome: Deserialize<'de>, Action: Deserialize<'de>"
))]
pub struct ActivityData<Time, Session, Window, Reason, Attention, Outcome, Action> {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub session: Option<Session>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<Session>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub window: Option<Window>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub reset_at: Option<Time>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub expires_at: Option<Time>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub reason: Option<Reason>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub attention: Option<Attention>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub outcome: Option<Outcome>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub action: Option<Action>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub credits_used: Option<f64>,
    pub count: u32,
    pub max_wait_ms: u64,
    pub relayed: u32,
    pub moved_groups: u32,
    pub sessions_truncated: bool,
    pub recovered: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub active: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub threshold: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub groups_hit: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct UsageHistory<Time, Account, Window> {
    pub accounts: Vec<UsageAccount<Time, Account, Window>>,
    pub truncated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct UsageAccount<Time, Account, Window> {
    pub id: Account,
    pub windows: Vec<UsageWindow<Time, Window>>,
    pub credits_spans: Vec<(Time, Option<Time>)>,
    pub truncated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct UsageWindow<Time, Window> {
    pub name: Window,
    pub points: Vec<(Time, Option<f64>)>,
    pub resets: Vec<Time>,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    type Data = ActivityData<String, String, String, String, String, String, String>;
    type Event = ActivityEvent<String, String, String, String, Data>;
    type Page = ActivityPage<Event>;
    type Usage = UsageHistory<String, String, String>;
    fn page() -> Value {
        json!({"events":[{"seq":1,"ts":"2030-01-01T00:00:00Z","kind":"burst",
            "provider":"codex","account":null,"account_to":null,"notable":true,
            "data":{"count":0,"max_wait_ms":0,"relayed":0,"moved_groups":0,
                "sessions_truncated":false,"recovered":false}}],
            "next_seq":2,"truncated":false})
    }
    fn usage() -> Value {
        json!({"accounts":[{"id":"sample-acct","windows":[{"name":"5h",
            "points":[["2030-01-01T00:00:00Z",null]],"resets":[],
            "truncated":true}],"credits_spans":[["2030-01-01T00:00:00Z",null]],
            "truncated":false}],"truncated":true})
    }
    #[test]
    fn required_activity_fields_are_not_archive_defaults_or_nullable_omissions() {
        let original = page();
        let decoded: Page = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), original);
        for pointer in ["", "/events/0", "/events/0/data"] {
            let keys: Vec<_> = original
                .pointer(pointer)
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            for key in keys {
                let mut missing = original.clone();
                missing
                    .pointer_mut(pointer)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(&key);
                assert!(
                    serde_json::from_value::<Page>(missing).is_err(),
                    "{pointer}/{key}"
                );
                let mut null = original.clone();
                null.pointer_mut(pointer).unwrap()[&key] = Value::Null;
                if !matches!(key.as_str(), "account" | "account_to") {
                    assert!(
                        serde_json::from_value::<Page>(null).is_err(),
                        "{pointer}/{key}"
                    );
                }
            }
        }
        for key in [
            "session",
            "sessions",
            "window",
            "reset_at",
            "expires_at",
            "reason",
            "attention",
            "outcome",
            "action",
            "credits_used",
            "active",
            "threshold",
            "groups_hit",
        ] {
            let mut null = original.clone();
            null["events"][0]["data"][key] = Value::Null;
            assert!(serde_json::from_value::<Page>(null).is_err(), "{key}");
        }
        let mut zero = original;
        zero["events"][0]["data"]["active"] = json!(0);
        let decoded: Page = serde_json::from_value(zero.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), zero);
    }
    #[test]
    fn nullable_tuple_values_and_required_usage_keys_survive() {
        let original = usage();
        let decoded: Usage = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), original);
        for pointer in ["", "/accounts/0", "/accounts/0/windows/0"] {
            let keys: Vec<_> = original
                .pointer(pointer)
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            for key in keys {
                let mut missing = original.clone();
                missing
                    .pointer_mut(pointer)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(&key);
                assert!(
                    serde_json::from_value::<Usage>(missing).is_err(),
                    "{pointer}/{key}"
                );
                let mut null = original.clone();
                null.pointer_mut(pointer).unwrap()[&key] = Value::Null;
                assert!(
                    serde_json::from_value::<Usage>(null).is_err(),
                    "{pointer}/{key}"
                );
            }
        }
        for bad in [
            json!(["2030-01-01T00:00:00Z"]),
            json!(["2030-01-01T00:00:00Z", null, 0]),
        ] {
            for pointer in [
                "/accounts/0/windows/0/points/0",
                "/accounts/0/credits_spans/0",
            ] {
                let mut invalid = original.clone();
                *invalid.pointer_mut(pointer).unwrap() = bad.clone();
                assert!(serde_json::from_value::<Usage>(invalid).is_err());
            }
        }
    }
    #[test]
    fn raw_duplicate_and_unknown_members_are_not_collapsed() {
        let raw = serde_json::to_string(&page()).unwrap();
        for first in ["1", "null", "\"bad\""] {
            let duplicate =
                raw.replacen("\"count\":0", &format!("\"count\":{first},\"count\":0"), 1);
            assert_ne!(duplicate, raw);
            assert!(serde_json::from_str::<Page>(&duplicate).is_err());
        }
        for pointer in ["", "/events/0", "/events/0/data"] {
            let mut value = page();
            value.pointer_mut(pointer).unwrap()["unknown_history_key"] = json!(true);
            assert!(serde_json::from_value::<Page>(value).is_err());
        }
        for pointer in ["", "/accounts/0", "/accounts/0/windows/0"] {
            let mut value = usage();
            value.pointer_mut(pointer).unwrap()["unknown_history_key"] = json!(true);
            assert!(serde_json::from_value::<Usage>(value).is_err());
        }
    }
}
