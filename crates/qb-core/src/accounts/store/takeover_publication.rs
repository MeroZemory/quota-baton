//! Pure journal/account publication; no client-source access.
use super::*;

macro_rules! takeover_publication_api {
    ($visibility:vis) => {
        #[doc(hidden)]
        impl AccountStore {
    $visibility fn publish_takeover(&self, journal: &crate::accounts::takeover::Journal) -> Result<Account, StoreError> {
        use crate::accounts::takeover::{Receipt, ReceiptState};
        let _guard = self.transaction()?;
        self.before_mutation(BeforeAccountMutation::FullSave { id: &journal.account.id });
        journal.validate()?;
        let old = match self.read(&journal.account.id) {
            Ok(record) => Some(record), Err(StoreError::NotFound) => None, Err(e) => return Err(e),
        };
        Self::check_generation(journal.expected, old.as_ref().map(|r| r.account.generation))?;
        Self::validate(&journal.account)?;
        if old.as_ref().is_some_and(|r| r.account.identity.canonical_key() != journal.account.identity.canonical_key()) {
            return Err(StoreError::IdentityCollision);
        }
        if old.as_ref().is_some_and(|r| journal.account.token_meta.compare(&r.account.token_meta).is_none_or(|o| o.is_lt())) {
            return Err(StoreError::OlderToken);
        }
        for other in Self::read_metadata_dir(&self.dir)? {
            if other.id == journal.account.id { continue; }
            let ambiguous = match (&other.identity, &journal.account.identity) {
                (crate::accounts::Identity::Codex { account_id:a,user_id:x,.. }, crate::accounts::Identity::Codex { account_id:b,user_id:y,.. }) =>
                    a == b && (x.is_none() || y.is_none() || x == y),
                (crate::accounts::Identity::Claude { account_uuid:a,organization_uuid:x,.. }, crate::accounts::Identity::Claude { account_uuid:b,organization_uuid:y,.. }) =>
                    a == b && (x.is_none() || y.is_none() || x == y),
                _ => false,
            };
            if ambiguous { return Err(StoreError::IdentityCollision); }
        }
        let before = old.as_ref().and_then(|r| r.takeover_receipt.as_ref());
        if before.is_some_and(|r| r.fenced()) { return Err(StoreError::InvalidData); }
        if let Some(before) = before {
            // Preserve the previous operation's dedupe proof before replacing
            // its account receipt. A failed tombstone write blocks publication.
            self.complete_takeover_journal_unlocked(&journal.account.id, before)?;
        }
        let tokens = self.protector.decrypt(&journal.account.id, journal.protected_tokens.as_deref().ok_or(StoreError::InvalidData)?)?;
        // Keep identity/store CAS and intent publication in one short transaction.
        // The existing account gate excludes competing token-consuming work.
        self.write_takeover_journal_unlocked(journal)?;
        let after = Receipt { operation:journal.operation.clone(), state:ReceiptState::Installed };
        self.save_record_unlocked(&journal.account, &tokens, journal.expected, Some((before, &after)))
    }

        }
    };
}
#[cfg(feature = "testkit")]
takeover_publication_api!(pub);
#[cfg(not(feature = "testkit"))]
takeover_publication_api!(pub(crate));
