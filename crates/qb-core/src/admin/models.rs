//! Cached catalog facts enriched with the relay's model-specific admission view.
use super::*;
use crate::accounts::Provider;
use qb_proto::admin_models::{AdminModel, AdminModelAccount, AdminModels, ProviderModels};

pub(super) async fn cached(State(state): State<RuntimeAdmin>) -> Response {
    let result = crate::boxes::spawn_blocking(move || {
        let accounts = state.relay.accounts.store.list().map_err(|_| ())?;
        let observed_at = chrono::Utc::now();
        let mut providers = Vec::new();
        for provider in [Provider::Claude, Provider::Codex] {
            let accounts: std::collections::BTreeMap<_, _> = accounts
                .iter()
                .filter(|account| {
                    account.provider == provider && crate::relay::catalog_assignable(account)
                })
                .map(|account| (account.id.clone(), account))
                .collect();
            let ids = accounts.keys().cloned().collect::<Vec<_>>();
            let models = state.relay.catalogs.union_models(&ids).map_err(|_| ())?;
            let models = models
                .into_iter()
                .map(|model| AdminModel {
                    accounts: model
                        .accounts
                        .into_iter()
                        .map(|id| {
                            let (usable_now, limited_until) = state
                                .relay
                                .admin_model_account_status(accounts[&id], &model.id, observed_at);
                            AdminModelAccount {
                                id,
                                usable_now,
                                limited_until,
                            }
                        })
                        .collect(),
                    id: model.id,
                    display_name: model.display_name,
                    efforts: model.efforts,
                    default_effort: model.default_effort,
                    observed_at: model.observed_at,
                    stale: model.stale,
                })
                .collect();
            providers.push(ProviderModels {
                provider: provider.as_str().to_owned(),
                models,
            });
        }
        Ok::<_, ()>(AdminModels {
            providers,
            observed_at,
        })
    })
    .await;
    match result {
        Ok(Ok(reply)) => Json(reply).into_response(),
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot read model catalogs",
        ),
    }
}

#[cfg(test)]
mod tests;
