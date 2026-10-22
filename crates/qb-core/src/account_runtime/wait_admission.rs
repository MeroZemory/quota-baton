//! Revocable HTTP wait inspection under the native runtime authority gates.
use super::AccountRuntime;
use super::wait_owner::{AdmittedQuotaWait, WaitOwner};
use crate::accounts::Account;
use crate::bindings::{BindingTable, HttpWaitRequest, WaitIntent};
use chrono::Utc;

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

    pub(crate) fn http_quota_wait_changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.wait_owner.notified()
    }

    fn with_current_http_wait<T>(
        &self,
        bindings: &BindingTable,
        request: HttpWaitRequest<'_>,
        inspect: impl FnOnce(&WaitOwner, &WaitIntent<'_>, &Account) -> Option<T>,
    ) -> Result<Option<T>, ()> {
        let _transaction = self.store.transaction().map_err(|_| ())?;
        let released = self.released_quota.lock().map_err(|_| ())?;
        let probes = self.probes.lock().map_err(|_| ())?;
        let volatile = self.volatile.lock().map_err(|_| ())?;
        Ok(bindings.try_current_quota_wait(request, |intent| {
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
        }))
    }
}
