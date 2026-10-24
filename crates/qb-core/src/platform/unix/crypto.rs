use crate::accounts::{StoreError, Tokens};

// P0 deliberately refuses credential persistence until the P1 backend is present.
pub(crate) fn encrypt_account(_: &Tokens) -> Result<String, StoreError> {
    Err(StoreError::Crypto)
}
pub(crate) fn decrypt_account(_: &str) -> Result<Tokens, StoreError> {
    Err(StoreError::Crypto)
}
