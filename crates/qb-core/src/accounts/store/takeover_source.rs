//! Native source mutations, selected at the store module boundary.
use super::*;

impl AccountStore {
    pub(crate) fn takeover_intent_reusable(
        &self,
        journal: &crate::accounts::takeover::Journal,
    ) -> Result<bool, StoreError> {
        let _guard = self.transaction()?;
        if journal.result.is_some() || journal.retired {
            return Ok(true);
        }
        match self.read(&journal.account.id) {
            Ok(record) => Ok(record
                .takeover_receipt
                .as_ref()
                .is_some_and(|r| r.operation == journal.operation)
                || journal.expected == Some(record.account.generation)),
            Err(StoreError::NotFound) => Ok(journal.expected.is_none()),
            Err(error) => Err(error),
        }
    }

    /// Synchronous file-only boundary shared with fresh-login supersession.
    /// Caller holds the async account gate; no HTTP/profile work occurs here.
    pub(crate) fn retire_takeover(
        &self,
        supplied: &crate::accounts::takeover::Journal,
        deps: &crate::accounts::takeover::Dependencies,
    ) -> Result<crate::accounts::takeover::Journal, crate::accounts::takeover::TakeoverError> {
        use crate::accounts::takeover::{Receipt, ReceiptState, TakeoverError};
        let _guard = self.transaction()?;
        self.before_mutation(BeforeAccountMutation::Retire {
            id: &supplied.account.id,
        });
        if self.writes_stopped() {
            return Err(TakeoverError::Unavailable);
        }
        let mut journal = self.read_takeover_unlocked(&supplied.operation)?;
        self.before_mutation(BeforeAccountMutation::Retire {
            id: &journal.account.id,
        });
        let record = self.read(&journal.account.id)?;
        let receipt = record
            .takeover_receipt
            .as_ref()
            .ok_or(TakeoverError::Invalid)?;
        if receipt.operation != journal.operation {
            return Err(TakeoverError::Conflict);
        }
        if !receipt.fenced() {
            self.complete_takeover_journal_unlocked(&journal.account.id, receipt)?;
            return Ok(self.read_takeover_unlocked(&journal.operation)?);
        }
        if !journal.retired {
            if receipt.state != ReceiptState::Installed {
                return Err(TakeoverError::Invalid);
            }
            crate::accounts::takeover::validate_source(deps, journal.provider, &journal.source)?;
            journal.retirement_started = true;
            self.write_takeover_journal_unlocked(&journal)?;
            deps.source.retire(
                journal.provider,
                &journal.source,
                &journal.destination,
                journal.keep_source,
            )?;
            journal.retired = true;
            self.write_takeover_journal_unlocked(&journal)?;
        }
        if receipt.state == ReceiptState::Installed {
            let tokens = self
                .protector
                .decrypt(&record.account.id, &record.secret_dpapi_b64)?;
            let ready = Receipt {
                operation: journal.operation.clone(),
                state: ReceiptState::Ready,
            };
            self.save_record_unlocked(
                &record.account,
                &tokens,
                Some(record.account.generation),
                Some((Some(receipt), &ready)),
            )?;
        }
        Ok(journal)
    }

    pub(crate) fn finalize_superseded_takeover(
        &self,
        id: &str,
        operation: &str,
    ) -> Result<crate::accounts::takeover::Journal, StoreError> {
        let _guard = self.transaction()?;
        let receipt = self
            .read(id)?
            .takeover_receipt
            .ok_or(StoreError::InvalidData)?;
        if receipt.operation != operation
            || receipt.state != crate::accounts::takeover::ReceiptState::Superseded
        {
            return Err(StoreError::InvalidData);
        }
        self.complete_takeover_journal_unlocked(id, &receipt)?;
        self.read_takeover_unlocked(operation)
    }
}

macro_rules! takeover_source_api {
    ($visibility:vis) => {
        #[doc(hidden)]
        impl AccountStore {
    $visibility fn write_takeover_journal(&self, journal: &crate::accounts::takeover::Journal) -> Result<(), StoreError> {
        let _guard = self.transaction()?;
        self.write_takeover_journal_unlocked(journal)
    }
    $visibility fn protect_takeover_tokens(&self, id: &str, tokens: &Tokens) -> Result<String, StoreError> { self.path(id)?; self.protector.encrypt(id, tokens) }
        }
    };
}
#[cfg(feature = "testkit")]
takeover_source_api!(pub);
#[cfg(not(feature = "testkit"))]
takeover_source_api!(pub(crate));
