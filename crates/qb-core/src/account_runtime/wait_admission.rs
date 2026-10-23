//! Revocable HTTP wait inspection under the native runtime authority gates.
use super::AccountRuntime;
use super::wait_owner::{AdmittedQuotaWait, InitialFiveHourLimit, ObservedHttp429, WaitOwner};
use crate::accounts::Account;
use crate::bindings::{BindingTable, HttpWaitRequest, WaitIntent};
use chrono::Utc;

/// Fully assembled qb response from a coherent native inspection. Fields are
/// private; it is neither provider bytes, an admission receipt nor send authority.
pub(crate) struct InitialQuotaFallback {
    response: axum::response::Response,
    limit: InitialFiveHourLimit,
    owner: std::sync::Weak<WaitOwner>,
}

impl InitialQuotaFallback {
    pub(crate) fn into_response(
        self,
        policies: &crate::routing::PolicyStore,
    ) -> Option<axum::response::Response> {
        let owner = self.owner.upgrade()?;
        self.limit
            .reply_current(&owner, policies)
            .then_some(self.response)
    }
}

impl AccountRuntime {
    pub(crate) fn current_http_quota_wait(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
    ) -> Result<Option<AdmittedQuotaWait>, ()> {
        self.with_current_http_wait(bindings, request, |owner, intent, account| {
            owner.admit_current_unlocked(intent, account)
        })
    }

    #[cfg(test)]
    pub(crate) fn current_http_quota_wait_is_current(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        receipt: &AdmittedQuotaWait,
    ) -> Result<bool, ()> {
        self.with_current_http_wait(bindings, request, |owner, intent, account| {
            owner
                .revalidate_current_unlocked(receipt, intent, account)
                .then_some(())
        })
        .map(|result| result.is_some())
    }

    pub(crate) fn renew_current_http_quota_wait(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        receipt: AdmittedQuotaWait,
    ) -> Result<Option<AdmittedQuotaWait>, ()> {
        self.with_current_http_wait(bindings, request, |owner, intent, account| {
            owner.renew_current_unlocked(receipt, intent, account)
        })
    }

    /// No admission, renewal, rotation or native mutation. The exact physical
    /// report must still be current under all existing runtime/binding gates.
    pub(crate) fn observed_http_far_wait(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        observed: &ObservedHttp429,
    ) -> Result<bool, ()> {
        self.with_current_http_condition(bindings, request, true, |owner, intent, account| {
            owner.observed_http_far_wait_unlocked(observed, intent, account)
        })
        .map(|result| result.is_some())
    }

    /// Native gates remain held through response assembly and exact-ticket
    /// revalidation. No provider response cache or pool-wide unavailable scan.
    pub(crate) fn current_http_initial_far_wait(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
    ) -> Result<Option<InitialQuotaFallback>, ()> {
        self.with_current_http_condition(bindings, request, true, |owner, intent, account| {
            use axum::response::IntoResponse;
            let limit = owner.initial_far_wait_unlocked(intent, account)?;
            let reset = limit.reset_at();
            let millis = reset.signed_duration_since(Utc::now()).num_milliseconds();
            if millis <= 0 {
                return None;
            }
            let seconds = (millis / 1000 + i64::from(millis % 1000 != 0)).max(1);
            let mut response = (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                axum::Json(serde_json::json!({"error": {
                    "type":"quota_baton_wait", "source":"quota-baton", "window":"5h",
                    "message":"quota-baton: this session is waiting for its five-hour quota reset",
                    "on_5h_limit":"wait", "resets_at":reset.timestamp(),
                    "resets_in_seconds":seconds
                }})),
            )
                .into_response();
            response.headers_mut().insert(
                "retry-after",
                axum::http::HeaderValue::from_str(&seconds.to_string()).ok()?,
            );
            response.headers_mut().insert(
                "cache-control",
                axum::http::HeaderValue::from_static("no-store"),
            );
            if !owner.initial_far_wait_is_current_unlocked(&limit, intent, account) {
                return None;
            }
            Some(InitialQuotaFallback {
                response,
                limit,
                owner: std::sync::Arc::downgrade(&self.wait_owner),
            })
        })
    }

    pub(crate) fn http_quota_wait_changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.wait_owner.notified()
    }

    fn with_current_http_wait<T>(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        inspect: impl FnOnce(&WaitOwner, &WaitIntent<'_>, &Account) -> Option<T>,
    ) -> Result<Option<T>, ()> {
        self.with_current_http_condition(bindings, request, false, inspect)
    }

    fn with_current_http_condition<T>(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        fallback: bool,
        inspect: impl FnOnce(&WaitOwner, &WaitIntent<'_>, &Account) -> Option<T>,
    ) -> Result<Option<T>, ()> {
        let _transaction = self.store.transaction().map_err(|_| ())?;
        let released = self.released_quota.lock().map_err(|_| ())?;
        let probes = self.probes.lock().map_err(|_| ())?;
        let volatile = self.volatile.lock().map_err(|_| ())?;
        let inspect = |intent: &WaitIntent<'_>| {
            let id = intent.account();
            if self.store.writes_stopped()
                || probes.contains(id)
                || volatile.contains_key(id)
                || released.get(id).is_some_and(|release| release.pending)
            {
                return None;
            }
            let account = self.store.read_account_unlocked(id).ok()?;
            if account.id != id
                || account.provider != intent.provider()
                || !self
                    .store
                    .tokens_unlocked(&account)
                    .is_ok_and(|tokens| !tokens.access_token.is_empty())
                || account
                    .token_meta
                    .expires_at
                    .is_some_and(|expiry| expiry <= Utc::now())
            {
                return None;
            }
            // Sample expiry after blocking credentials work. The owner then
            // checks current quota/proof/model, clocks, caller and ticket last.
            inspect(&self.wait_owner, intent, &account)
        };
        Ok(if fallback {
            bindings.try_current_quota_fallback(request, inspect)
        } else {
            bindings.try_current_quota_wait(request, inspect)
        })
    }
}
