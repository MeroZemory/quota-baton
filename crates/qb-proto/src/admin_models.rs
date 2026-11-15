//! Additive cached-model discovery contract. Consumers ignore unknown fields.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminModels {
    pub providers: Vec<ProviderModels>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderModels {
    /// Open string so adding a provider does not require a consumer update.
    pub provider: String,
    pub models: Vec<AdminModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminModel {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    pub accounts: Vec<AdminModelAccount>,
    pub observed_at: DateTime<Utc>,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminModelAccount {
    pub id: String,
    pub usable_now: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limited_until: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discovery_accepts_new_providers_and_additive_fields_at_every_level() {
        let wire = json!({
            "providers": [{"provider": "future-provider", "future": 1, "models": [{
                "id": "new-model", "accounts": [{"id": "a", "usable_now": false,
                    "future": 1}], "observed_at": "2026-11-15T00:00:00Z",
                "stale": true, "future": 1
            }]}], "observed_at": "2026-11-15T00:00:01Z", "future": 1
        });
        let reply: AdminModels = serde_json::from_value(wire).unwrap();
        assert_eq!(reply.providers[0].provider, "future-provider");
        let model = &reply.providers[0].models[0];
        assert!(model.stale);
        assert!(!model.accounts[0].usable_now);
        let encoded = serde_json::to_value(&reply).unwrap();
        for field in ["display_name", "efforts", "default_effort"] {
            assert!(encoded["providers"][0]["models"][0].get(field).is_none());
        }
        assert!(
            encoded["providers"][0]["models"][0]["accounts"][0]
                .get("limited_until")
                .is_none()
        );
    }

    #[test]
    fn reported_metadata_and_limit_timestamps_roundtrip() {
        let wire = json!({
            "providers": [{"provider": "codex", "models": [{
                "id": "model-a", "display_name": "Model A", "efforts": ["low", "high"],
                "default_effort": "high", "accounts": [{"id": "a", "usable_now": false,
                    "limited_until": "2026-11-15T01:00:00Z"}],
                "observed_at": "2026-11-15T00:00:00Z", "stale": false
            }]}], "observed_at": "2026-11-15T00:00:01Z"
        });
        let reply: AdminModels = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(reply).unwrap(), wire);
    }
}
