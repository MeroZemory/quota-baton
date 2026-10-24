//! Explicit derived account presentation and legacy archive types. Source
//! normalization here never replaces a raw AdminAccount's source object.
use super::{StatePools, StateWarning};
use serde::{Deserialize, Serialize, de::Deserializer};

type PresentationState =
    qb_proto::admin_state::AdminState<PresentationAccount, StateWarning, StatePools>;

/// Derived presentation/archive account shape; typed warning/pool contracts
/// remain shared. Sparse archive accounts do not prove current raw transport.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct PresentationSnapshot(pub PresentationState);

impl std::ops::Deref for PresentationSnapshot {
    type Target = PresentationState;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for PresentationSnapshot {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<'de> Deserialize<'de> for PresentationSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        qb_proto::admin_state::deserialize_legacy(deserializer, false).map(Self)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PresentationAccount {
    #[serde(default)]
    pub has_alternative: bool,
    #[serde(default)]
    pub quota_notice: Option<qb_tray::QuotaNotice>,
    #[serde(default)]
    pub last_relevant_state_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub user_state: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub health: serde_json::Value,
    #[serde(default)]
    pub probe_pending: bool,
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    #[serde(default)]
    pub usage_forecast: Option<qb_core::usage_forecast::AccountUsageForecast>,
    #[serde(default)]
    pub credits: Option<serde_json::Value>,
    #[serde(default)]
    pub credits_in_use: Option<bool>,
    #[serde(default)]
    pub pool_eligible: Option<bool>,
    #[serde(default)]
    pub quota_cooling: Option<serde_json::Value>,
    #[serde(default)]
    pub quota_credit_releasable: bool,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub bound_sessions: u64,
    #[serde(default)]
    pub active_sessions: u64,
    #[serde(default)]
    pub spread_threshold: u64,
    #[serde(default)]
    pub token_expires_at: Option<String>,
    #[serde(default)]
    pub last_refresh_at: Option<String>,
    #[serde(default)]
    pub drain: bool,
    #[serde(default)]
    pub allow_credits: bool,
    #[serde(default)]
    pub outside_used: bool,
    #[serde(default)]
    pub refresh_attention: bool,
    #[serde(default, deserialize_with = "decode_source")]
    pub source: String,
    #[serde(default)]
    pub pacing: qb_tray::Pacing,
}

fn decode_source<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(value) => {
            if value == "takeover" {
                "oauth".into()
            } else {
                value
            }
        }
        serde_json::Value::Object(mut object) => {
            let kind = object
                .remove("kind")
                .and_then(|kind| kind.as_str().map(str::to_owned));
            if kind.as_deref() == Some("cpa") {
                match object.get("mode").and_then(serde_json::Value::as_str) {
                    Some(mode @ ("linked" | "adopted")) => mode.to_owned(),
                    _ => "⚠ source decode error (unknown mode)".into(),
                }
            } else {
                kind.map(|kind| {
                    if kind == "takeover" {
                        "oauth".into()
                    } else {
                        kind
                    }
                })
                .unwrap_or_else(|| "⚠ source decode error (missing kind)".into())
            }
        }
        _ => "⚠ source decode error".into(),
    })
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct QuotaWindow {
    pub name: String,
    pub used_pct: Option<f64>,
    pub reset_at: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}
