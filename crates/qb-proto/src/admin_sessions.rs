//! Exact current GET /admin/v1/sessions wire contract. No routing/proof authority.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

fn required_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

/// Validated wire identity only; not a proof or action authorization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);
impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for SessionId {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() == 12
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(s))
        } else {
            Err("invalid SessionId")
        }
    }
}
impl From<SessionId> for String {
    fn from(v: SessionId) -> Self {
        v.0
    }
}

/// Validated wire identity only; not a proof or action authorization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AccountId(String);
impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for AccountId {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() == 9
            && (s.starts_with("cl-") || s.starts_with("cx-"))
            && s[3..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(s))
        } else {
            Err("invalid AccountId")
        }
    }
}
impl From<AccountId> for String {
    fn from(v: AccountId) -> Self {
        v.0
    }
}

/// Validated wire identity only; not a proof or action authorization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RoutingActionId(String);
impl RoutingActionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for RoutingActionId {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(s))
        } else {
            Err("invalid RoutingActionId")
        }
    }
}
impl From<RoutingActionId> for String {
    fn from(v: RoutingActionId) -> Self {
        v.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    CodexCli,
    CodexVscode,
    CodexExec,
    CodexDesktop,
    ClaudeCli,
    ClaudeVscode,
    ClaudeSdk,
    Unknown,
}
impl ClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CodexCli => "codex_cli",
            Self::CodexVscode => "codex_vscode",
            Self::CodexExec => "codex_exec",
            Self::CodexDesktop => "codex_desktop",
            Self::ClaudeCli => "claude_cli",
            Self::ClaudeVscode => "claude_vscode",
            Self::ClaudeSdk => "claude_sdk",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveReason {
    New,
    LaunchModel,
    ModelChoice,
    Forced,
    FiveHourFarMove,
    ColdRebalance,
    ResetHoldTimeout,
    Overload,
}
impl MoveReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::LaunchModel => "launch_model",
            Self::ModelChoice => "model_choice",
            Self::Forced => "forced",
            Self::FiveHourFarMove => "five_hour_far_move",
            Self::ColdRebalance => "cold_rebalance",
            Self::ResetHoldTimeout => "reset_hold_timeout",
            Self::Overload => "overload",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheResult {
    Hit,
    Miss,
}
impl CacheResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    Launcher,
    CodexWorkspace,
    ClaudeProject,
}
impl LabelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launcher => "launcher",
            Self::CodexWorkspace => "codex_workspace",
            Self::ClaudeProject => "claude_project",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
}
impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum On5hLimit {
    Wait,
    Move,
}
impl On5hLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Move => "move",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Strict,
    Recover,
    Rebalance,
}
impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Recover => "recover",
            Self::Rebalance => "rebalance",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct ClientView {
    pub kind: ClientKind,
    #[serde(deserialize_with = "required_nullable")]
    pub product: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct MoveView {
    pub at: DateTime<Utc>,
    #[serde(deserialize_with = "required_nullable")]
    pub from: Option<AccountId>,
    pub to: AccountId,
    pub reason: MoveReason,
    #[serde(deserialize_with = "required_nullable")]
    pub next_turn_cache: Option<CacheResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct InflightView {
    pub requests: usize,
    #[serde(deserialize_with = "required_nullable")]
    pub oldest_started_at: Option<DateTime<Utc>>,
    pub ws_open: usize,
    pub ws_response_in_flight: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct PacingWait {
    pub waiting: usize,
    pub since: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct RateWaitToday {
    pub count: u64,
    pub total_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct SessionRouting {
    pub action_id: RoutingActionId,
    pub on_5h_limit: On5hLimit,
    pub mode: Mode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct QuotaWaitView {
    pub waiting: u32,
    pub reset_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct SessionSummary {
    pub id: SessionId,
    pub provider: Provider,
    pub client: ClientView,
    #[serde(deserialize_with = "required_nullable")]
    pub label: Option<LabelView>,
    pub account: AccountId,
    #[serde(deserialize_with = "required_nullable")]
    pub first_seen_at: Option<DateTime<Utc>>,
    #[serde(deserialize_with = "required_nullable")]
    pub bound_since: Option<DateTime<Utc>>,
    pub last_used_at: DateTime<Utc>,
    pub active: bool,
    pub inflight: InflightView,
    pub inflight_known: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub pacing_wait: Option<PacingWait>,
    #[serde(deserialize_with = "required_nullable")]
    pub routing: Option<SessionRouting>,
    #[serde(deserialize_with = "required_nullable")]
    pub quota_wait: Option<QuotaWaitView>,
    pub quota_wait_known: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub model: Option<String>,
    pub subagents_today: usize,
    pub requests_today: u64,
    pub failures_today: u64,
    pub rate_wait_today: RateWaitToday,
    pub moves_today: Vec<MoveView>,
    #[serde(deserialize_with = "required_nullable")]
    pub first_account_today: Option<AccountId>,
    pub marked_for_move: bool,
    pub counters_complete: bool,
    pub truncated: bool,
    pub subagents_known: bool,
    pub moves_known: bool,
    pub metadata_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct SessionsView {
    #[serde(deserialize_with = "required_nullable")]
    pub move_cost: Option<Snapshot>,
    pub now: DateTime<Utc>,
    pub active_window_secs: u64,
    pub sessions: Vec<SessionSummary>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct LabelView {
    pub text: String,
    pub source: LabelSource,
    /// Filename slug reconstruction is advisory, including unique local matches.
    pub best_effort: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct Aggregate {
    pub tokens: u64,
    pub known_count: u64,
    pub unknown_count: u64,
    /// Start of this reducer's continuous observation, NOT the start of a full day.
    pub observed_since: DateTime<Utc>,
    /// False after any capacity/ID rejection; restart with a new observation epoch to recover.
    pub recording_complete: bool,
    pub tokens_saturated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "contract-tests"), serde(deny_unknown_fields))]
pub struct Snapshot {
    pub hour: Aggregate,
    pub rolling24h: Aggregate,
    /// Wall times were clamped to processing order (clock regression or racing callers).
    pub clock_adjusted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn fixture() -> Value {
        serde_json::from_str(r#"{"move_cost":{"hour":{"tokens":90,"known_count":1,"unknown_count":1,"observed_since":"2026-10-24T00:00:00Z","recording_complete":false,"tokens_saturated":false},"rolling24h":{"tokens":90,"known_count":1,"unknown_count":1,"observed_since":"2026-10-24T00:00:00Z","recording_complete":false,"tokens_saturated":false},"clock_adjusted":true},"now":"2026-10-24T00:00:00Z","active_window_secs":60,"sessions":[{"id":"123456abcdef","provider":"codex","client":{"kind":"codex_cli","product":"synthetic","version":"1"},"label":{"text":"synthetic-folder","source":"claude_project","best_effort":true},"account":"cx-000001","first_seen_at":"2026-10-24T00:00:00Z","bound_since":"2026-10-24T00:00:00Z","last_used_at":"2026-10-24T00:00:00Z","active":true,"inflight":{"requests":2,"oldest_started_at":"2026-10-24T00:00:00Z","ws_open":1,"ws_response_in_flight":true},"inflight_known":false,"pacing_wait":{"waiting":2,"since":"2026-10-24T00:00:00Z"},"routing":{"action_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","on_5h_limit":"move","mode":"recover"},"quota_wait":{"waiting":1,"reset_at":"2026-10-24T00:00:00Z"},"quota_wait_known":true,"model":"synthetic","subagents_today":1,"requests_today":5,"failures_today":2,"rate_wait_today":{"count":1,"total_ms":9},"moves_today":[{"at":"2026-10-24T00:00:00Z","from":"cl-000002","to":"cx-000001","reason":"reset_hold_timeout","next_turn_cache":"miss"}],"first_account_today":"cl-000002","marked_for_move":true,"counters_complete":false,"truncated":true,"subagents_known":false,"moves_known":false,"metadata_truncated":true}],"truncated":true}"#).unwrap()
    }

    #[test]
    fn sessions_exact_populated_and_empty_wire_roundtrip() {
        let full = fixture();
        let decoded: SessionsView = serde_json::from_value(full.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), full);
        let mut empty = fixture();
        empty["sessions"] = json!([]);
        empty["move_cost"] = Value::Null;
        empty["truncated"] = json!(false);
        let decoded: SessionsView = serde_json::from_value(empty.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), empty);
    }

    // Recurse through this populated graph: every producer key is required,
    // including nullable members. No serde(default) or Option omission loophole.
    fn missing_keys(root: &Value, node: &Value, path: &str) -> usize {
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
                        serde_json::from_value::<SessionsView>(missing).is_err(),
                        "missing {path}/{key}"
                    );
                    let mut extra = root.clone();
                    extra
                        .pointer_mut(path)
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .insert("unexpected".into(), json!(true));
                    assert!(
                        serde_json::from_value::<SessionsView>(extra).is_err(),
                        "unknown at {path}"
                    );
                    count += 1 + missing_keys(root, child, &format!("{path}/{key}"));
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    count += missing_keys(root, child, &format!("{path}/{index}"));
                }
            }
            _ => {}
        }
        count
    }

    #[test]
    fn sessions_requires_every_nested_key_and_rejects_unknowns() {
        let full = fixture();
        assert_eq!(missing_keys(&full, &full, ""), 72);
    }

    #[test]
    fn sessions_nullable_is_present_null_not_missing() {
        let mut value = fixture();
        for path in [
            "/sessions/0/client/product",
            "/sessions/0/client/version",
            "/sessions/0/label",
            "/sessions/0/first_seen_at",
            "/sessions/0/bound_since",
            "/sessions/0/inflight/oldest_started_at",
            "/sessions/0/pacing_wait",
            "/sessions/0/routing",
            "/sessions/0/quota_wait",
            "/sessions/0/model",
            "/sessions/0/first_account_today",
            "/sessions/0/moves_today/0/from",
            "/sessions/0/moves_today/0/next_turn_cache",
            "/move_cost",
        ] {
            *value.pointer_mut(path).unwrap() = Value::Null;
        }
        let decoded: SessionsView = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    }

    #[test]
    fn sessions_duplicate_fields_are_rejected_before_value_normalization() {
        let encoded = serde_json::to_string(&fixture()).unwrap();
        for (key, duplicate) in [
            ("active", "false"),
            ("best_effort", "false"),
            (
                "action_id",
                "\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"",
            ),
            ("tokens", "0"),
            ("truncated", "false"),
        ] {
            let needle = format!("\"{key}\":");
            let duplicate = encoded.replacen(&needle, &format!("{needle}{duplicate},{needle}"), 1);
            assert!(
                serde_json::from_str::<SessionsView>(&duplicate).is_err(),
                "duplicate {key}"
            );
        }
    }

    #[test]
    fn sessions_ids_enums_numbers_and_timestamps_preserve_validation() {
        for (path, bad) in [
            ("/sessions/0/id", json!("private-native-key")),
            ("/sessions/0/id", json!("ABCDEF123456")),
            ("/sessions/0/account", json!("cx-00000z")),
            ("/sessions/0/routing/action_id", json!("a".repeat(63))),
            ("/sessions/0/routing/action_id", json!("A".repeat(64))),
            ("/sessions/0/provider", json!("other")),
            ("/sessions/0/client/kind", json!("future")),
            ("/sessions/0/label/source", json!("other")),
            ("/sessions/0/routing/on_5h_limit", json!("future")),
            ("/sessions/0/routing/mode", json!("future")),
            ("/sessions/0/moves_today/0/reason", json!("future")),
            ("/sessions/0/moves_today/0/next_turn_cache", json!("future")),
            ("/sessions/0/quota_wait/waiting", json!(4294967296_u64)),
            ("/sessions/0/requests_today", json!(-1)),
            ("/sessions/0/inflight/requests", json!(1.5)),
            ("/sessions/0/last_used_at", json!("not-a-time")),
            ("/sessions/0/active", Value::Null),
            ("/move_cost/hour/observed_since", Value::Null),
        ] {
            let mut value = fixture();
            *value.pointer_mut(path).unwrap() = bad;
            assert!(
                serde_json::from_value::<SessionsView>(value).is_err(),
                "invalid {path}"
            );
        }
        let mut value = fixture();
        value["sessions"][0]["requests_today"] = json!(u64::MAX);
        let decoded: SessionsView = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.sessions[0].requests_today, u64::MAX);
    }
}
