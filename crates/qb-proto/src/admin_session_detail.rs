//! Exact current flat GET /admin/v1/sessions/{id} wire shape.
//! No proofs, routing actions, reducers or display defaults are constructed here.
use crate::admin_sessions::{
    AccountId, CacheResult, ClientView, InflightView, LabelView, MoveReason, MoveView, PacingWait,
    Provider, QuotaWaitView, RateWaitToday, SessionId, SessionRouting, SessionSummary,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

fn required_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    Ok,
    RateHoldOk,
    UpstreamError,
    ClientDrop,
    RelayedRate429,
    NetworkError,
}
impl RequestOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::RateHoldOk => "rate_hold_ok",
            Self::UpstreamError => "upstream_error",
            Self::ClientDrop => "client_drop",
            Self::RelayedRate429 => "relayed_rate429",
            Self::NetworkError => "network_error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct TimelineView {
    pub at: DateTime<Utc>,
    pub kind: String,
    pub account: AccountId,
    #[serde(deserialize_with = "required_nullable")]
    pub from: Option<AccountId>,
    pub reason: MoveReason,
    #[serde(deserialize_with = "required_nullable")]
    pub next_turn_cache: Option<CacheResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct FailoverView {
    pub from: AccountId,
    pub to: AccountId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct RecentRequest {
    pub ts: DateTime<Utc>,
    pub outcome: RequestOutcome,
    pub status: u16,
    pub ttfb_ms: u64,
    pub dur_ms: u64,
    pub rate_wait_ms: u64,
    pub rate_retries: u32,
    #[serde(deserialize_with = "required_nullable")]
    pub failover: Option<FailoverView>,
}

// Serialize retains the producer's flattened shape. Deserialize is deliberately
// NOT derived here: serde flatten must not absorb missing/duplicate/unknown keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub session: SessionSummary,
    pub original_short_id: Option<String>,
    pub recent_requests: Vec<RecentRequest>,
    pub timeline: Vec<TimelineView>,
    pub recent_requests_known: bool,
    pub timeline_known: bool,
}

#[derive(Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
struct FlatSessionDetail {
    id: SessionId,
    provider: Provider,
    client: ClientView,
    #[serde(deserialize_with = "required_nullable")]
    label: Option<LabelView>,
    account: AccountId,
    #[serde(deserialize_with = "required_nullable")]
    first_seen_at: Option<DateTime<Utc>>,
    #[serde(deserialize_with = "required_nullable")]
    bound_since: Option<DateTime<Utc>>,
    last_used_at: DateTime<Utc>,
    active: bool,
    inflight: InflightView,
    inflight_known: bool,
    #[serde(deserialize_with = "required_nullable")]
    pacing_wait: Option<PacingWait>,
    #[serde(deserialize_with = "required_nullable")]
    routing: Option<SessionRouting>,
    #[serde(deserialize_with = "required_nullable")]
    quota_wait: Option<QuotaWaitView>,
    quota_wait_known: bool,
    #[serde(deserialize_with = "required_nullable")]
    model: Option<String>,
    subagents_today: usize,
    requests_today: u64,
    failures_today: u64,
    rate_wait_today: RateWaitToday,
    moves_today: Vec<MoveView>,
    #[serde(deserialize_with = "required_nullable")]
    first_account_today: Option<AccountId>,
    marked_for_move: bool,
    counters_complete: bool,
    truncated: bool,
    subagents_known: bool,
    moves_known: bool,
    metadata_truncated: bool,
    #[serde(deserialize_with = "required_nullable")]
    original_short_id: Option<String>,
    recent_requests: Vec<RecentRequest>,
    timeline: Vec<TimelineView>,
    recent_requests_known: bool,
    timeline_known: bool,
}

impl<'de> Deserialize<'de> for SessionDetail {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let flat = FlatSessionDetail::deserialize(deserializer)?;
        Ok(Self {
            session: SessionSummary {
                id: flat.id,
                provider: flat.provider,
                client: flat.client,
                label: flat.label,
                account: flat.account,
                first_seen_at: flat.first_seen_at,
                bound_since: flat.bound_since,
                last_used_at: flat.last_used_at,
                active: flat.active,
                inflight: flat.inflight,
                inflight_known: flat.inflight_known,
                pacing_wait: flat.pacing_wait,
                routing: flat.routing,
                quota_wait: flat.quota_wait,
                quota_wait_known: flat.quota_wait_known,
                model: flat.model,
                subagents_today: flat.subagents_today,
                requests_today: flat.requests_today,
                failures_today: flat.failures_today,
                rate_wait_today: flat.rate_wait_today,
                moves_today: flat.moves_today,
                first_account_today: flat.first_account_today,
                marked_for_move: flat.marked_for_move,
                counters_complete: flat.counters_complete,
                truncated: flat.truncated,
                subagents_known: flat.subagents_known,
                moves_known: flat.moves_known,
                metadata_truncated: flat.metadata_truncated,
            },
            original_short_id: flat.original_short_id,
            recent_requests: flat.recent_requests,
            timeline: flat.timeline,
            recent_requests_known: flat.recent_requests_known,
            timeline_known: flat.timeline_known,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn fixture() -> Value {
        serde_json::from_str(r#"{"id":"123456abcdef","provider":"codex","client":{"kind":"codex_cli","product":"synthetic","version":"1"},"label":{"text":"synthetic-folder","source":"claude_project","best_effort":true},"account":"cx-000001","first_seen_at":"2026-10-24T00:00:00Z","bound_since":"2026-10-24T00:00:00Z","last_used_at":"2026-10-24T00:00:00Z","active":true,"inflight":{"requests":2,"oldest_started_at":"2026-10-24T00:00:00Z","ws_open":1,"ws_response_in_flight":true},"inflight_known":false,"pacing_wait":{"waiting":2,"since":"2026-10-24T00:00:00Z"},"routing":{"action_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","on_5h_limit":"move","mode":"recover"},"quota_wait":{"waiting":1,"reset_at":"2026-10-24T00:00:00Z"},"quota_wait_known":true,"model":"synthetic","subagents_today":1,"requests_today":5,"failures_today":2,"rate_wait_today":{"count":1,"total_ms":9},"moves_today":[{"at":"2026-10-24T00:00:00Z","from":"cl-000002","to":"cx-000001","reason":"reset_hold_timeout","next_turn_cache":"miss"}],"first_account_today":"cl-000002","marked_for_move":true,"counters_complete":false,"truncated":true,"subagents_known":false,"moves_known":false,"metadata_truncated":true,"original_short_id":"12345678","recent_requests":[{"ts":"2026-10-24T00:00:00Z","outcome":"relayed_rate429","status":429,"ttfb_ms":3,"dur_ms":17,"rate_wait_ms":10,"rate_retries":4294967295,"failover":{"from":"cl-000002","to":"cx-000001"}},{"ts":"2026-10-24T00:00:00Z","outcome":"ok","status":200,"ttfb_ms":1,"dur_ms":4,"rate_wait_ms":0,"rate_retries":0,"failover":null}],"timeline":[{"at":"2026-10-24T00:00:00Z","kind":"placed","account":"cl-000002","from":null,"reason":"new","next_turn_cache":null},{"at":"2026-10-24T00:00:00Z","kind":"move","account":"cx-000001","from":"cl-000002","reason":"reset_hold_timeout","next_turn_cache":"hit"}],"recent_requests_known":false,"timeline_known":false}"#).unwrap()
    }

    #[test]
    fn detail_exact_flat_populated_and_empty_roundtrip() {
        let full = fixture();
        assert_eq!(full.as_object().unwrap().len(), 33);
        assert!(full.get("session").is_none());
        let decoded: SessionDetail = serde_json::from_value(full.clone()).unwrap();
        assert_eq!(decoded.session.id.as_str(), "123456abcdef");
        assert!(!decoded.recent_requests_known && !decoded.timeline_known);
        assert_eq!(serde_json::to_value(decoded).unwrap(), full);
        let mut empty = fixture();
        empty["original_short_id"] = Value::Null;
        empty["recent_requests"] = json!([]);
        empty["timeline"] = json!([]);
        let decoded: SessionDetail = serde_json::from_value(empty.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), empty);
    }

    fn missing_and_unknown(root: &Value, node: &Value, path: &str) -> usize {
        let mut count = 0;
        match node {
            Value::Object(fields) => {
                for (key, child) in fields {
                    let mut missing = root.clone();
                    missing
                        .pointer_mut(path)
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .remove(key);
                    assert!(
                        serde_json::from_value::<SessionDetail>(missing).is_err(),
                        "missing {path}/{key}"
                    );
                    let mut unknown = root.clone();
                    unknown
                        .pointer_mut(path)
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .insert("unexpected".into(), json!(true));
                    assert!(
                        serde_json::from_value::<SessionDetail>(unknown).is_err(),
                        "unknown at {path}"
                    );
                    count += 1 + missing_and_unknown(root, child, &format!("{path}/{key}"));
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    count += missing_and_unknown(root, child, &format!("{path}/{index}"));
                }
            }
            _ => {}
        }
        count
    }

    #[test]
    fn detail_every_flat_and_nested_key_is_required_and_unknowns_rejected() {
        let full = fixture();
        assert_eq!(missing_and_unknown(&full, &full, ""), 87);
        let mut wrapped = fixture();
        let fields = serde_json::to_value(
            serde_json::from_value::<SessionDetail>(fixture())
                .unwrap()
                .session,
        )
        .unwrap();
        for key in fields.as_object().unwrap().keys() {
            wrapped.as_object_mut().unwrap().remove(key);
        }
        wrapped["session"] = fields;
        assert!(serde_json::from_value::<SessionDetail>(wrapped).is_err());
    }

    #[test]
    fn detail_present_null_retained_without_omitting_keys() {
        let mut value = fixture();
        for path in [
            "/original_short_id",
            "/recent_requests/0/failover",
            "/timeline/1/from",
            "/timeline/1/next_turn_cache",
            "/label",
            "/routing",
            "/quota_wait",
            "/model",
            "/first_seen_at",
            "/bound_since",
        ] {
            *value.pointer_mut(path).unwrap() = Value::Null;
        }
        let decoded: SessionDetail = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    }

    #[test]
    fn detail_raw_duplicate_flat_summary_detail_and_nested_keys_rejected() {
        let value = fixture();
        let encoded = serde_json::to_string(&value).unwrap();
        for (key, path) in [
            ("id", "/id"),
            ("original_short_id", "/original_short_id"),
            ("recent_requests", "/recent_requests"),
            ("timeline", "/timeline"),
            ("recent_requests_known", "/recent_requests_known"),
            ("timeline_known", "/timeline_known"),
            ("status", "/recent_requests/0/status"),
            ("best_effort", "/label/best_effort"),
            ("outcome", "/recent_requests/0/outcome"),
            ("to", "/moves_today/0/to"),
        ] {
            let needle = format!("\"{key}\":");
            let duplicate = encoded.replacen(
                &needle,
                &format!("{needle}{},{needle}", value.pointer(path).unwrap()),
                1,
            );
            assert!(
                serde_json::from_str::<SessionDetail>(&duplicate).is_err(),
                "duplicate {key}"
            );
        }
    }

    #[test]
    fn detail_nested_ids_enums_numbers_timestamps_and_nonnull_fields_validated() {
        for (path, bad) in [
            ("/id", json!("not-a-hash")),
            ("/provider", json!("future")),
            ("/routing/action_id", json!("A".repeat(64))),
            ("/label/source", json!("future")),
            ("/label/best_effort", Value::Null),
            ("/recent_requests/0/outcome", json!("future")),
            ("/recent_requests/0/status", json!(65536_u64)),
            ("/recent_requests/0/rate_retries", json!(4294967296_u64)),
            ("/recent_requests/0/dur_ms", json!(-1)),
            ("/recent_requests/0/ts", json!("not-a-time")),
            (
                "/recent_requests/0/failover/from",
                json!("upstream-private-account"),
            ),
            ("/timeline/0/account", json!("cx-00000z")),
            ("/timeline/0/reason", json!("future")),
            ("/timeline/0/next_turn_cache", json!("future")),
            ("/timeline/0/at", json!("not-a-time")),
            ("/timeline/0/kind", Value::Null),
            ("/recent_requests_known", Value::Null),
            ("/timeline", Value::Null),
        ] {
            let mut value = fixture();
            *value.pointer_mut(path).unwrap() = bad;
            assert!(
                serde_json::from_value::<SessionDetail>(value).is_err(),
                "invalid {path}"
            );
        }
        let mut max = fixture();
        max["recent_requests"][0]["dur_ms"] = json!(u64::MAX);
        let decoded: SessionDetail = serde_json::from_value(max).unwrap();
        assert_eq!(decoded.recent_requests[0].dur_ms, u64::MAX);
        assert_eq!(decoded.recent_requests[0].rate_retries, u32::MAX);
    }
}
