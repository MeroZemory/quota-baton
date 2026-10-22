//! Bounded capture/import of the real admin DTOs. Offline decoding performs no IO.

use std::{io::Write, time::SystemTime};

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AdminClient, HealthResponse, TraySnapshot};
use qb_tray::dashboard_model::{
    ActivityResponse, DashboardSnapshot, SessionsResponse, UsageHistory,
};

pub const MAX_COMPONENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_BUNDLE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotBundleV1 {
    pub schema_version: u32,
    pub capture_started_at: DateTime<Utc>,
    pub capture_completed_at: DateTime<Utc>,
    pub utc_offset_seconds: i32,
    pub consistency: Consistency,
    pub session_details: DetailCoverage,
    pub query: Query,
    pub health: HealthResponse,
    pub state: TraySnapshot,
    pub sessions: SessionsResponse,
    pub activity: ActivityResponse,
    pub usage_history: UsageHistory,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    IndependentReads,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailCoverage {
    NotCaptured,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub sessions_range: String,
    pub activity_days: u8,
    pub activity_limit: u16,
    pub usage_hours: u8,
}

#[derive(Debug, thiserror::Error)]
enum SnapshotError {
    #[error("Snapshot exceeds its size limit")]
    TooLarge,
    #[error("Snapshot format is invalid")]
    Invalid,
    #[error("Snapshot schema version is unsupported")]
    UnsupportedVersion,
    #[error("Snapshot contains a disallowed field")]
    DisallowedField,
    #[error("Unable to capture a required core response")]
    Request,
}

/// Read the same five admin DTOs used by the live tray/dashboard. Each read is
/// bounded; these independent requests do not form a transactional core cut.
pub async fn capture(client: &AdminClient) -> anyhow::Result<SnapshotBundleV1> {
    let started = Local::now();
    let health = async {
        let value = client
            .dashboard_value("/admin/v1/health")
            .await
            .map_err(|_| SnapshotError::Request)?;
        required_health(&value)?;
        serde_json::from_value(value).map_err(|_| SnapshotError::Invalid)
    };
    let state = async {
        let value = client
            .dashboard_value("/admin/v1/state")
            .await
            .map_err(|_| SnapshotError::Request)?;
        AdminClient::decode_dashboard(value, "accounts").map_err(|_| SnapshotError::Invalid)
    };
    let sessions = async {
        client
            .dashboard_sessions(qb_tray::state::Range::All)
            .await
            .map_err(|_| SnapshotError::Request)
    };
    let activity = async {
        client
            .dashboard_activity(7)
            .await
            .map_err(|_| SnapshotError::Request)
    };
    let usage = async {
        client
            .dashboard_usage_history()
            .await
            .map_err(|_| SnapshotError::Request)
    };
    let (health, mut state, sessions, activity, usage_history) =
        tokio::try_join!(health, state, sessions, activity, usage)?;
    allowlist_state(&mut state)?;
    let bundle = SnapshotBundleV1 {
        schema_version: 1,
        capture_started_at: started.to_utc(),
        capture_completed_at: Utc::now(),
        utc_offset_seconds: started.offset().local_minus_utc(),
        consistency: Consistency::IndependentReads,
        session_details: DetailCoverage::NotCaptured,
        query: Query {
            sessions_range: "all".into(),
            activity_days: 7,
            activity_limit: 2000,
            usage_hours: 48,
        },
        health,
        state,
        sessions,
        activity,
        usage_history,
    };
    // Check total encoded size and the actual serialized privacy boundary.
    encode(&bundle)?;
    Ok(bundle)
}

/// Pure bounded bytes -> the existing AdminClient DTOs. No home or client is opened.
pub fn decode(bytes: &[u8]) -> anyhow::Result<SnapshotBundleV1> {
    if bytes.len() > MAX_BUNDLE_BYTES {
        return Err(SnapshotError::TooLarge.into());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| SnapshotError::Invalid)?;
    reject_credentials(&value)?;
    required_health(&value["health"])?;
    for (component, field) in [
        ("state", "accounts"),
        ("sessions", "sessions"),
        ("activity", "events"),
        ("usage_history", "accounts"),
    ] {
        if !value[component][field].is_array() {
            return Err(SnapshotError::Invalid.into());
        }
    }
    let bundle: SnapshotBundleV1 =
        serde_json::from_value(value).map_err(|_| SnapshotError::Invalid)?;
    bundle.validate()?;
    Ok(bundle)
}

/// Serialize only the decoded DTO bundle, never raw endpoint JSON or a client.
pub fn encode(bundle: &SnapshotBundleV1) -> anyhow::Result<Vec<u8>> {
    bundle.validate()?;
    let bytes = serialize_limited(bundle, MAX_BUNDLE_BYTES, true)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| SnapshotError::Invalid)?;
    reject_credentials(&value)?;
    Ok(bytes)
}

impl SnapshotBundleV1 {
    /// The exact TraySnapshot serialization -> v2 path used by live polling.
    pub fn project(&self, now: SystemTime) -> anyhow::Result<DashboardSnapshot> {
        self.validate()?;
        let bytes = serialize_limited(&self.state, MAX_COMPONENT_BYTES, true)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| SnapshotError::Invalid)?;
        let accounts = qb_tray::v2::project(text, now).map_err(|_| SnapshotError::Invalid)?;
        let mut data = DashboardSnapshot::new(accounts);
        data.data_at = Some(self.capture_completed_at);
        data.sessions = self.sessions.clone();
        data.activity = self.activity.events.clone();
        data.activity_truncated = self.activity.truncated;
        data.usage = self.usage_history.clone();
        data.connection.core_ok = self.health.ok;
        // The projection retains its initial presentation state; this file
        // does not claim a live SSE connection or historical stream status.
        Ok(data)
    }

    fn validate(&self) -> Result<(), SnapshotError> {
        if self.schema_version != 1 {
            return Err(SnapshotError::UnsupportedVersion);
        }
        if chrono::FixedOffset::east_opt(self.utc_offset_seconds).is_none()
            || self.query.sessions_range != "all"
            || self.query.activity_days != 7
            || self.query.activity_limit != 2000
            || self.query.usage_hours != 48
        {
            return Err(SnapshotError::Invalid);
        }
        validate_state(&self.state)?;
        for component in [
            serialize_limited(&self.health, MAX_COMPONENT_BYTES, false),
            serialize_limited(&self.state, MAX_COMPONENT_BYTES, false),
            serialize_limited(&self.sessions, MAX_COMPONENT_BYTES, false),
            serialize_limited(&self.activity, MAX_COMPONENT_BYTES, false),
            serialize_limited(&self.usage_history, MAX_COMPONENT_BYTES, false),
        ] {
            component?;
        }
        Ok(())
    }
}

fn required_health(value: &Value) -> Result<(), SnapshotError> {
    if !value["ok"].is_boolean()
        || !value["version"].is_string()
        || value["pid"].as_u64().is_none()
        || !value["started_at"].is_string()
    {
        return Err(SnapshotError::Invalid);
    }
    Ok(())
}

const HEALTH: &[&str] = &["kind", "reason", "scope", "until", "since"];
const CREDITS: &[&str] = &["has_credits", "unlimited", "balance", "serving"];
const QUOTA_COOLING: &[&str] = &["window", "reset_at", "started_at"];
const PROVIDERS: &[&str] = &["claude", "codex"];
const POOL: &[&str] = &["eligible", "unavailable", "cooling"];
const REASONS: &[&str] = &[
    "quota",
    "rate",
    "auth",
    "permission",
    "payment",
    "store_error",
    "link_stale",
    "unknown",
];

fn retain_fields(value: &mut Value, fields: &[&str]) -> Result<(), SnapshotError> {
    if value.is_null() {
        return Ok(());
    }
    let object = value.as_object_mut().ok_or(SnapshotError::Invalid)?;
    object.retain(|key, _| fields.contains(&key.as_str()));
    Ok(())
}

fn allowlist_state(state: &mut TraySnapshot) -> Result<(), SnapshotError> {
    retain_fields(&mut state.pools, PROVIDERS)?;
    if let Some(pools) = state.pools.as_object_mut() {
        for pool in pools.values_mut() {
            retain_fields(pool, POOL)?;
            if let Some(cooling) = pool.get_mut("cooling") {
                retain_fields(cooling, REASONS)?;
            }
        }
    }
    for account in &mut state.accounts {
        retain_fields(&mut account.health, HEALTH)?;
        if let Some(credits) = &mut account.credits {
            retain_fields(credits, CREDITS)?;
        }
        if let Some(cooling) = &mut account.quota_cooling {
            retain_fields(cooling, QUOTA_COOLING)?;
        }
    }
    validate_state(state)
}

fn object_fields<'a>(
    value: &'a Value,
    allowed: &[&str],
) -> Result<Option<&'a serde_json::Map<String, Value>>, SnapshotError> {
    if value.is_null() {
        return Ok(None);
    }
    let object = value.as_object().ok_or(SnapshotError::Invalid)?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(SnapshotError::DisallowedField);
    }
    Ok(Some(object))
}

fn text_fields(value: &Value, allowed: &[&str]) -> Result<(), SnapshotError> {
    if let Some(object) = object_fields(value, allowed)?
        && object.values().any(|v| !v.is_null() && !v.is_string())
    {
        return Err(SnapshotError::Invalid);
    }
    Ok(())
}

fn validate_state(state: &TraySnapshot) -> Result<(), SnapshotError> {
    if let Some(pools) = object_fields(&state.pools, PROVIDERS)? {
        for pool in pools.values() {
            if let Some(pool) = object_fields(pool, POOL)? {
                for (name, value) in pool {
                    if name == "cooling" {
                        if let Some(reasons) = object_fields(value, REASONS)?
                            && reasons.values().any(|v| v.as_u64().is_none())
                        {
                            return Err(SnapshotError::Invalid);
                        }
                    } else if !value.is_null() && value.as_u64().is_none() {
                        return Err(SnapshotError::Invalid);
                    }
                }
            }
        }
    }
    for account in &state.accounts {
        text_fields(&account.health, HEALTH)?;
        if let Some(credits) = &account.credits
            && let Some(fields) = object_fields(credits, CREDITS)?
        {
            for (name, value) in fields {
                if !value.is_null()
                    && !(if name == "balance" {
                        value.is_number()
                    } else {
                        value.is_boolean()
                    })
                {
                    return Err(SnapshotError::Invalid);
                }
            }
        }
        if let Some(cooling) = &account.quota_cooling {
            text_fields(cooling, QUOTA_COOLING)?;
        }
    }
    Ok(())
}

fn reject_credentials(value: &Value) -> Result<(), SnapshotError> {
    match value {
        Value::Object(object) => {
            for (name, value) in object {
                let key: String = name
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .map(|ch| ch.to_ascii_lowercase())
                    .collect();
                if matches!(
                    key.as_str(),
                    "accesstoken"
                        | "refreshtoken"
                        | "idtoken"
                        | "admintoken"
                        | "authtoken"
                        | "token"
                        | "apikey"
                        | "clientkey"
                        | "privatekey"
                        | "clientsecret"
                        | "authorization"
                        | "cookie"
                        | "setcookie"
                        | "password"
                        | "credentials"
                ) {
                    return Err(SnapshotError::DisallowedField);
                }
                reject_credentials(value)?;
            }
        }
        Value::Array(array) => {
            for value in array {
                reject_credentials(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

struct LimitedWriter {
    bytes: Vec<u8>,
    remaining: usize,
    exceeded: bool,
    retain: bool,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            self.exceeded = true;
            return Err(std::io::Error::other("snapshot size limit"));
        }
        self.remaining -= bytes.len();
        if self.retain {
            self.bytes.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_limited(
    value: &impl Serialize,
    limit: usize,
    retain: bool,
) -> Result<Vec<u8>, SnapshotError> {
    let mut writer = LimitedWriter {
        bytes: Vec::new(),
        remaining: limit,
        exceeded: false,
        retain,
    };
    if serde_json::to_writer(&mut writer, value).is_err() || writer.write_all(b"\n").is_err() {
        return Err(if writer.exceeded {
            SnapshotError::TooLarge
        } else {
            SnapshotError::Invalid
        });
    }
    Ok(writer.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;

    fn wire_bundle() -> Value {
        let at = "2026-10-23T12:00:00Z";
        let usage_forecast = json!({"as_of":at,"observed_at":at,"earliest_window":"5h",
        "credit":null,"truncated":false,"windows":[{
            "name":"5h","window_seconds":18000,"observed_at":at,
            "used_pct":70.0,"reset_at":"2026-10-23T16:00:00Z",
            "awaiting_usage":false,"recent_pct_per_hour":12.0,"basis":"recent",
            "exhaustion_at":"2026-10-23T14:30:00Z","seconds_to_exhaustion":9000,
            "outcome":"exhaustion","changing":true,"truncated":false
        }]});
        let state = json!({
            "version":"synthetic","refresh_token":"private-canary-root",
            "pools":{"codex":{"eligible":1,"unavailable":0,"cooling":{"auth":1},
                "access_token":"private-canary-pool"}},
            "accounts":[{"id":"one","provider":"codex","user_state":"active",
                "label":"Synthetic account","plan":"plus",
                "health":{"kind":"ready","access_token":"private-canary-health"},
                "source":{"kind":"oauth","dir":"private-canary-source"},
                "credits":{"has_credits":true,"unlimited":false,"balance":80.0,
                    "api_key":"private-canary-credit"},
                "credits_in_use":true,"allow_credits":true,
                "token_expires_at":"2026-10-24T12:00:00Z",
                "pacing":{"backoff_until":"2026-10-23T12:00:20Z","waiting":2},
                "quota_cooling":{"window":"5h","reset_at":"2026-10-23T16:00:00Z",
                    "started_at":at,"authorization":"private-canary-cooling"},
                "windows":[{"name":"5h","used_pct":70.0,"reset_at":"2026-10-23T16:00:00Z"}],
                "usage_forecast":usage_forecast
            }]
        });
        let sessions = json!({"now":at,"active_window_secs":60,"truncated":true,
                "sessions":[{"id":"123456abcdef","provider":"codex","account":"one",
                    "inflight_known":false,"counters_complete":false,"metadata_truncated":true}],
                "move_cost":{"hour":{"tokens":90,"known_count":1,"unknown_count":1,
                    "recording_complete":false},
                    "rolling24h":{"tokens":90,"known_count":1,"unknown_count":1,
                    "recording_complete":false},"clock_adjusted":true}});
        let activity = json!({"events":[{"seq":4,"ts":at,"kind":"request_failed",
                "provider":"codex","account":"one","data":{"active":2,"recovered":true,
                    "label":"presentation-injection"}}],"next_seq":4,"truncated":true});
        let usage_history = json!({"accounts":[{"id":"one","windows":[{"name":"5h",
                "points":[[at,70.0]],"resets":["2026-10-23T16:00:00Z"],"truncated":true}],
                "credits_spans":[[at,null]],"truncated":true}],"truncated":true});
        json!({
            "schema_version":1, "capture_started_at":at, "capture_completed_at":at,
            "utc_offset_seconds":32400, "consistency":"independent_reads",
            "session_details":"not_captured",
            "query":{"sessions_range":"all","activity_days":7,"activity_limit":2000,"usage_hours":48},
            "health":{"ok":true,"version":"synthetic","pid":7,"started_at":at},
            "state":state,
            "sessions":sessions,
            "activity":activity,
            "usage_history":usage_history
        })
    }

    fn bundle() -> SnapshotBundleV1 {
        let mut bundle: SnapshotBundleV1 = serde_json::from_value(wire_bundle()).unwrap();
        allowlist_state(&mut bundle.state).unwrap();
        bundle
    }

    fn server(wire: &Value) -> (AdminClient, std::thread::JoinHandle<()>) {
        let routes: std::collections::BTreeMap<_, _> = [
            ("/admin/v1/health", "health"),
            ("/admin/v1/state", "state"),
            ("/admin/v1/sessions?range=all", "sessions"),
            ("/admin/v1/activity?days=7&limit=2000", "activity"),
            ("/admin/v1/usage-history?hours=48", "usage_history"),
        ]
        .into_iter()
        .map(|(route, field)| (route.to_owned(), serde_json::to_vec(&wire[field]).unwrap()))
        .collect();
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(![10100, 10101].contains(&port));
        let worker = std::thread::spawn(move || {
            let mut routes = routes;
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                    assert!(headers.len() < 16 * 1024);
                }
                let headers = String::from_utf8(headers).unwrap();
                let mut request = headers.lines().next().unwrap().split_whitespace();
                assert_eq!(request.next(), Some("GET"));
                let body = routes
                    .remove(request.next().unwrap())
                    .expect("unexpected route");
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("authorization: bearer synthetic-r16")
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
            assert!(routes.is_empty());
        });
        (AdminClient::new(port, "synthetic-r16").unwrap(), worker)
    }

    #[tokio::test]
    async fn r16_snapshot_actual_admin_dto_roundtrip_preserves_rendered_facts() {
        let (client, server) = server(&wire_bundle());
        let captured = capture(&client).await.unwrap();
        server.join().unwrap();
        drop(client);
        let bytes = encode(&captured).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.state.accounts[0].source, "oauth");
        assert_eq!(decoded.state.accounts[0].pacing.waiting, 2);
        assert_eq!(
            decoded.state.accounts[0].credits.as_ref().unwrap()["balance"],
            80.0
        );
        assert!(decoded.sessions.truncated);
        assert!(!decoded.sessions.sessions[0].inflight_known);
        assert!(!decoded.sessions.sessions[0].counters_complete);
        let cost = &decoded.sessions.move_cost.as_ref().unwrap().rolling24h;
        assert_eq!(
            (cost.tokens, cost.known_count, cost.unknown_count),
            (90, 1, 1)
        );
        assert!(!cost.recording_complete);
        assert!(decoded.activity.truncated);
        assert_eq!(decoded.activity.next_seq, 4);
        assert_eq!(decoded.activity.events[0].data.active_sessions, Some(2));
        assert!(decoded.activity.events[0].data.after_failure);
        assert!(decoded.activity.events[0].data.label.is_none());
        assert_eq!(
            decoded.usage_history.accounts[0].windows[0].points[0].1,
            Some(70.0)
        );
        assert!(decoded.usage_history.accounts[0].windows[0].truncated);
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("private-canary"));
        assert!(!text.contains("presentation-injection"));
        assert!(text.contains("token_expires_at"));
        assert!(text.contains("\"tokens\":90"));
        assert!(text.contains("\"active\":2"));
        assert!(text.contains("\"recovered\":true"));
        let now = DateTime::parse_from_rfc3339("2026-10-23T12:00:00Z").unwrap();
        let projected = decoded.project(now.to_utc().into()).unwrap();
        let account = projected
            .accounts
            .sections
            .iter()
            .flat_map(|s| &s.rows)
            .find(|a| a.raw.id == "one")
            .unwrap();
        assert!(
            !account.forecast_lines.is_empty(),
            "shared DTO forecast must survive real capture"
        );
        assert_eq!(projected.data_at, Some(decoded.capture_completed_at));
        assert_eq!(projected.activity.len(), 1);
        assert!(projected.activity_truncated);
    }

    #[test]
    fn r16_snapshot_wire_schema_empty_and_missing_are_distinct() {
        let bytes = encode(&bundle()).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        for (component, field) in [
            ("state", "accounts"),
            ("sessions", "sessions"),
            ("activity", "events"),
            ("usage_history", "accounts"),
        ] {
            let mut empty = value.clone();
            empty[component][field] = json!([]);
            assert!(decode(&serde_json::to_vec(&empty).unwrap()).is_ok());
            empty[component].as_object_mut().unwrap().remove(field);
            assert!(decode(&serde_json::to_vec(&empty).unwrap()).is_err());
        }
        let mut newer = value;
        newer["schema_version"] = json!(2);
        assert!(decode(&serde_json::to_vec(&newer).unwrap()).is_err());
    }

    #[test]
    fn r16_snapshot_recursive_credentials_and_open_value_shapes_fail_closed() {
        let bytes = encode(&bundle()).unwrap();
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["activity"]["events"][0]["ignored"] = json!([{"Set-Cookie":"private-canary"}]);
        let error = decode(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(!error.to_string().contains("private-canary"));
        let mut unsafe_bundle = bundle();
        unsafe_bundle.state.accounts[0].health["reason"] = json!({"access_token":"private-canary"});
        assert!(encode(&unsafe_bundle).is_err());
        unsafe_bundle.state.accounts[0].health =
            json!({"kind":"ready","future":{"api_key":"private-canary"}});
        assert!(encode(&unsafe_bundle).is_err());
    }

    #[test]
    fn r16_snapshot_serialization_limit_stops_before_overflow() {
        let bundle = bundle();
        let bytes = encode(&bundle).unwrap();
        assert!(serialize_limited(&bundle, bytes.len(), true).is_ok());
        assert!(matches!(
            serialize_limited(&bundle, bytes.len() - 1, true),
            Err(SnapshotError::TooLarge)
        ));
        assert!(matches!(
            serialize_limited(&json!({"value":"x".repeat(100)}), 32, false),
            Err(SnapshotError::TooLarge)
        ));
    }
}
