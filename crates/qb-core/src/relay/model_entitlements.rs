//! Model union status uses the same native admission path as a real dispatch.
use super::*;
use chrono::{DateTime, Utc};

impl RelayState {
    pub(crate) fn admin_model_account_status(
        &self,
        account: &Account,
        model: &str,
        now: DateTime<Utc>,
    ) -> (bool, Option<DateTime<Utc>>) {
        let usable = catalog_assignable(account)
            && self
                .accounts
                .with_selection_admission(
                    &self.bindings,
                    account.provider,
                    Some(model),
                    |eligible| eligible(&account.id, false),
                )
                .unwrap_or(false);
        let limited_until = (!usable)
            .then(|| self.accounts.model_limited_until(account, Some(model), now))
            .flatten();
        (usable, limited_until)
    }
}

/// A catalog owner fact tied to one live request and its actual source. This
/// grants no policy override, breaker exception, credential or send authority.
pub(crate) struct ModelEntitlementEvidence {
    cache: Arc<crate::catalog::CatalogCache>,
    provider: Provider,
    key: Option<String>,
    from: String,
    generation: u64,
    to: String,
    model: String,
    deadline: Instant,
    lifetime: std::sync::Weak<AtomicBool>,
}

impl ModelEntitlementEvidence {
    pub(super) fn observe(
        cache: Arc<crate::catalog::CatalogCache>,
        key: Option<&str>,
        source: &Account,
        to: &str,
        model: Option<&str>,
        deadline: Instant,
        lifetime: &Arc<AtomicBool>,
    ) -> Option<Self> {
        let proof = Self {
            cache,
            provider: source.provider,
            key: key.map(str::to_owned),
            from: source.id.clone(),
            generation: source.generation,
            to: to.to_owned(),
            model: model?.to_owned(),
            deadline,
            lifetime: Arc::downgrade(lifetime),
        };
        proof
            .applies_to(source.provider, key, source, to, model, Instant::now())
            .then_some(proof)
    }

    pub(crate) fn applies_to(
        &self,
        provider: Provider,
        key: Option<&str>,
        source: &Account,
        to: &str,
        model: Option<&str>,
        now: Instant,
    ) -> bool {
        if self.provider != provider
            || source.provider != provider
            || self.key.as_deref() != key
            || source.id != self.from
            || source.generation < self.generation
            || self.from == to
            || self.to != to
            || model != Some(self.model.as_str())
            || now >= self.deadline
            || !self
                .lifetime
                .upgrade()
                .is_some_and(|alive| alive.load(Ordering::Acquire))
        {
            return false;
        }
        let source_excluded = self
            .cache
            .is_model_denied(&self.from, &self.model)
            .unwrap_or(false)
            || self
                .cache
                .view(&self.from)
                .ok()
                .and_then(|view| view.observation.last_known().cloned())
                .is_some_and(|known| !known.supported().contains(&self.model));
        source_excluded
            && self
                .cache
                .entitled_accounts(std::slice::from_ref(&self.to), &self.model, false)
                .is_ok_and(|ids| ids.contains(&self.to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::RefreshMode;

    fn catalog(cache: &Arc<crate::catalog::CatalogCache>, account: &str, model: &str) {
        cache
            .begin_refresh(account, RefreshMode::Force)
            .unwrap()
            .unwrap()
            .finish(Ok(serde_json::json!({"data":[{"id":model}]})))
            .unwrap();
    }

    #[test]
    fn r30_catalog_move_proof_is_bound_to_live_request_and_current_membership() {
        let cache = Arc::new(crate::catalog::CatalogCache::default());
        let mut source = crate::accounts::health::tests::fixture();
        source.id = "source".into();
        catalog(&cache, "source", "old");
        catalog(&cache, "target", "new");
        let alive = Arc::new(AtomicBool::new(true));
        let now = Instant::now();
        let deadline = now + Duration::from_secs(10);
        let proof = ModelEntitlementEvidence::observe(
            cache.clone(),
            Some("session"),
            &source,
            "target",
            Some("new"),
            deadline,
            &alive,
        )
        .unwrap();
        let valid = |key, target, model, at| {
            proof.applies_to(source.provider, key, &source, target, model, at)
        };
        assert!(valid(Some("session"), "target", Some("new"), now));
        assert!(!valid(Some("other"), "target", Some("new"), now));
        assert!(!valid(Some("session"), "other", Some("new"), now));
        assert!(!valid(Some("session"), "target", Some("old"), now));
        assert!(!valid(Some("session"), "target", Some("new"), deadline));
        cache.deny_model("target", "new").unwrap();
        assert!(!valid(Some("session"), "target", Some("new"), now));
        catalog(&cache, "target", "new");
        assert!(valid(Some("session"), "target", Some("new"), now));
        catalog(&cache, "source", "new");
        assert!(!valid(Some("session"), "target", Some("new"), now));
        cache.deny_model("source", "new").unwrap();
        assert!(valid(Some("session"), "target", Some("new"), now));
        alive.store(false, Ordering::Release);
        assert!(!valid(Some("session"), "target", Some("new"), now));
    }
}
