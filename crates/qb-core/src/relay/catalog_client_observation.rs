//! Only bounded, strictly parsed Codex protocol versions enter durable state.
use super::catalog_client_version::Version;
use super::*;

fn observed_version(uri: &axum::http::Uri, headers: &HeaderMap) -> Option<Version> {
    let mut newest = None;
    if uri.path() == "/v1/models" {
        #[derive(serde::Deserialize)]
        struct Query {
            client_version: Option<String>,
        }
        if let Ok(axum::extract::Query(query)) = axum::extract::Query::<Query>::try_from_uri(uri) {
            newest = query.client_version.as_deref().and_then(Version::parse);
        }
    }
    for header in headers.get_all("user-agent") {
        let Ok(agent) = header.to_str() else { continue };
        for product in agent.split_ascii_whitespace() {
            let Some((name, version)) = product.split_once('/') else {
                continue;
            };
            if name == "codex" || name.starts_with("codex-") || name.starts_with("codex_") {
                newest = newest.into_iter().chain(Version::parse(version)).max();
            }
        }
    }
    newest
}

impl RelayState {
    pub(super) async fn observe_codex_catalog_client(
        &self,
        uri: &axum::http::Uri,
        headers: &HeaderMap,
    ) -> Result<(), ()> {
        if headers.contains_key("anthropic-version") || uri.path().starts_with("/v1/messages") {
            return Ok(());
        }
        let Some(version) = observed_version(uri, headers) else {
            return Ok(());
        };
        let record = self.codex_catalog_version.clone();
        let raised = crate::boxes::spawn_blocking(move || record.observe(version))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        if raised {
            // Wake promptly even if an account-list read fails. Each catalog read
            // independently compares its recorded fetch version before serving.
            self.catalog_version_changed.notify_one();
            let store = self.store.clone();
            let accounts = crate::boxes::spawn_blocking(move || store.list())
                .await
                .map_err(|_| ())?
                .map_err(|_| ())?;
            for account in accounts
                .into_iter()
                .filter(|a| a.provider == Provider::Codex)
            {
                self.catalogs.mark_due(&account.id).map_err(|_| ())?;
            }
            self.catalog_version_changed.notify_one();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_query_and_codex_products_only() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "user-agent",
            HeaderValue::from_static("codex-tui/0.171.0 (Windows) codex_exec/0.172.1"),
        );
        assert_eq!(
            observed_version(
                &"/v1/models?client_version=0.170.0".parse().unwrap(),
                &headers
            ),
            Version::parse("0.172.1")
        );
        for value in [
            "codex-tui/0.171.0-beta",
            "other/9.0.0",
            "codex_exec/1.2",
            "codex_cli_rs/1.2.3.4",
        ] {
            headers.insert("user-agent", HeaderValue::from_str(value).unwrap());
            assert_eq!(
                observed_version(&"/v1/responses".parse().unwrap(), &headers),
                None
            );
        }
        headers.clear();
        assert_eq!(
            observed_version(
                &"/v1/models?client_version=0.173.0".parse().unwrap(),
                &headers
            ),
            Version::parse("0.173.0")
        );
    }
}
