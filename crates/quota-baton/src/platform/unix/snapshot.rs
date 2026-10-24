//! Bounded, typed headless display export; credentials and CPA source paths are excluded.
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
#[derive(Deserialize, Serialize)]
struct Account {
    id: String,
    provider: qb_proto::admin_account::AdminProvider,
    label: String,
    email: String,
    user_state: qb_proto::admin_account::AdminUserState,
    health: qb_proto::admin_account::AdminHealth,
    windows: Vec<qb_proto::admin_account::AdminWindow>,
}
#[derive(Serialize)]
struct Snapshot {
    format: &'static str,
    captured_at: chrono::DateTime<chrono::Utc>,
    health: qb_proto::HealthResponse,
    state: qb_proto::AdminState<
        Account,
        qb_proto::admin_warning::AdminWarning,
        Option<qb_proto::admin_pool::ReadPools>,
    >,
}
async fn read(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut response = client
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
            "Snapshot response exceeds its size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
pub(crate) async fn capture_debug_snapshot(home: &Path) -> anyhow::Result<Vec<u8>> {
    let config = qb_core::config::Config::load(home)?;
    let secrets = qb_core::local_secrets::load_existing(home)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let base = format!("http://127.0.0.1:{}/admin/v1", config.server.admin_port);
    let health =
        serde_json::from_slice(&read(&client, &base, &secrets.admin_token, "/health").await?)?;
    let state_bytes = read(&client, &base, &secrets.admin_token, "/state").await?;
    let mut deserializer = serde_json::Deserializer::from_slice(&state_bytes);
    let state = qb_proto::admin_state::deserialize_compat(&mut deserializer, true)?;
    deserializer.end()?;
    Ok(serde_json::to_vec_pretty(&Snapshot {
        format: "quota-baton.headless.snapshot.v1",
        captured_at: chrono::Utc::now(),
        health,
        state,
    })?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn headless_snapshot_projects_display_fields_without_tokens_or_source_paths() {
        let account: super::Account = serde_json::from_value(serde_json::json!({
            "id":"synthetic", "provider":"codex", "label":"sample", "email":"sample@example.test",
            "user_state":"active", "health":{"kind":"ready"}, "windows":[],
            "access_token":"synthetic-private-token", "refresh_token":"synthetic-refresh",
            "source":{"kind":"cpa", "dir":"synthetic-private-path"}
        }))
        .unwrap();
        let text = serde_json::to_string(&account).unwrap();
        assert!(text.contains("synthetic"));
        assert!(!text.contains("token"));
        assert!(!text.contains("source"));
        assert!(!text.contains("synthetic-private"));
    }
}
