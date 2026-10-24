//! CPA-linked sources are unavailable on Linux; never read client credentials.

use crate::accounts::linked::LinkError;
use crate::accounts::{Account, AccountStore, Identity, Tokens};

pub(crate) fn resolve_identity(_account: &Account) -> Result<Identity, LinkError> {
    Err(LinkError::NotLinked)
}
pub fn reload(_account: &Account) -> Result<Tokens, LinkError> {
    Err(LinkError::NotLinked)
}
pub fn reload_and_store(_store: &AccountStore, _account: &Account) -> Result<Account, LinkError> {
    Err(LinkError::NotLinked)
}
