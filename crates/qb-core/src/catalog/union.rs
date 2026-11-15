use std::collections::BTreeMap;

use super::*;

/// Catalog facts only. The caller supplies one provider's assignable accounts
/// and adds health/model-hold information when building the admin response.
pub struct UnionModel {
    pub id: String,
    pub display_name: Option<String>,
    pub efforts: Option<Vec<String>>,
    pub default_effort: Option<String>,
    pub accounts: Vec<String>,
    pub observed_at: DateTime<Utc>,
    pub stale: bool,
}

impl CatalogCache {
    /// A stable union of effective entitlements. Failed refreshes retain their
    /// previous facts; `stale` is age > twice the cadence, not refresh failure.
    /// Unknown catalogs contribute no model IDs. Overrides exclude the denied
    /// account/model pair without modifying the provider's original raw reply.
    pub fn union_models(&self, accounts: &[String]) -> Result<Vec<UnionModel>, CatalogError> {
        for account in accounts {
            validate_account(account)?;
        }
        let mut models: BTreeMap<String, (Arc<KnownCatalog>, Vec<String>)> = BTreeMap::new();
        let now = {
            let state = self.0.lock();
            let now = (self.0.clock)();
            // Deduplicate and order account IDs independently of input order.
            for account in accounts.iter().collect::<BTreeSet<_>>() {
                let Some(entry) = state.entries.get(account) else {
                    continue;
                };
                let Some(known) = entry.observation.last_known() else {
                    continue;
                };
                for id in known.supported.difference(&entry.denied_models) {
                    let (newest, contributors) = models
                        .entry(id.clone())
                        .or_insert_with(|| (Arc::clone(known), Vec::new()));
                    if known.observed_at > newest.observed_at {
                        *newest = Arc::clone(known);
                    }
                    contributors.push(account.clone());
                }
            }
            now
        };
        Ok(models
            .into_iter()
            .map(|(id, (newest, accounts))| {
                let (display_name, efforts, default_effort) = metadata(&newest.raw, &id);
                UnionModel {
                    id,
                    display_name,
                    efforts,
                    default_effort,
                    accounts,
                    observed_at: newest.observed_at_wallclock,
                    stale: now.saturating_duration_since(newest.observed_at) > REFRESH_INTERVAL * 2,
                }
            })
            .collect())
    }
}

// Closed provider fields only: Codex models[].slug/display_name,
// supported_reasoning_levels[].effort/default_reasoning_level; Claude
// data[].id/display_name. Unknown fields remain in KnownCatalog::raw().
fn metadata(raw: &Value, id: &str) -> (Option<String>, Option<Vec<String>>, Option<String>) {
    let codex = raw.get("models").is_some();
    let items = raw[if codex { "models" } else { "data" }].as_array();
    let item = items.and_then(|items| {
        items
            .iter()
            .find(|item| item[if codex { "slug" } else { "id" }].as_str() == Some(id))
    });
    let Some(item) = item else {
        return (None, None, None);
    };
    let display_name = item
        .get("display_name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if !codex {
        return (display_name, None, None);
    }
    let efforts = item
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .and_then(|levels| {
            levels
                .iter()
                .map(|level| {
                    level
                        .get("effort")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect::<Option<Vec<_>>>()
        });
    let default_effort = item
        .get("default_reasoning_level")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (display_name, efforts, default_effort)
}
