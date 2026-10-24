//! Unix account encryption. Caller prepares a private home/accounts
//! and holds the account-store lease before constructing a protector.
use crate::{
    accounts::{StoreError, Tokens},
    config::{LinuxSecretBackend, SecretsConfig},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::{
    io::{self, Write},
    path::Path,
};
use zeroize::Zeroizing;

use super::private_fs as secure_key;

const PREFIX: &str = "qb-aes256gcm-v1:";
const DOMAIN: &[u8] = b"quota-baton/linux/account-secret/v1\0";
const MAX_PLAINTEXT: usize = 1024 * 1024;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MAX_BINARY: usize = NONCE_LEN + MAX_PLAINTEXT + TAG_LEN;
const MAX_BASE64: usize = MAX_BINARY.div_ceil(3) * 4;

/// Store-owned; neither Debug nor Clone can accidentally expose/copy the raw key.
/// ring's opaque expanded key schedule has no promised zeroization API.
pub(crate) struct SecretProtector {
    key: Zeroizing<[u8; 32]>,
}

pub(crate) fn account_secret_protector(home: &Path) -> Result<SecretProtector, StoreError> {
    let config = crate::config::Config::load(home).map_err(|_| StoreError::InvalidData)?;
    SecretProtector::new(home, &config.secrets)
}

impl SecretProtector {
    pub(crate) fn new(home: &Path, config: &SecretsConfig) -> Result<Self, StoreError> {
        // Read this one variable once, only at the production constructor boundary.
        let credentials_dir =
            std::env::var_os("CREDENTIALS_DIRECTORY").map(std::path::PathBuf::from);
        Self::new_with_credentials(home, config, credentials_dir.as_deref())
    }

    fn new_with_credentials(
        home: &Path,
        config: &SecretsConfig,
        credentials_dir: Option<&Path>,
    ) -> Result<Self, StoreError> {
        let home = secure_key::private_directory(home, false).map_err(|_| StoreError::Crypto)?;
        let accounts =
            secure_key::private_child(&home, "accounts", false).map_err(|_| StoreError::Crypto)?;
        let key = match &config.linux_backend {
            LinuxSecretBackend::SystemdCredential => {
                let directory = secure_key::private_directory(
                    credentials_dir
                        .filter(|path| path.is_absolute())
                        .ok_or(StoreError::Crypto)?,
                    true,
                )
                .map_err(|_| StoreError::Crypto)?;
                secure_key::read_key(&directory, "qb-master-key", true)
                    .map_err(|_| StoreError::Crypto)?
            }
            LinuxSecretBackend::File0600 => {
                secure_key::load_or_create_key(&home, &accounts).map_err(|_| StoreError::Crypto)?
            }
        };
        Ok(Self { key })
    }

    pub(crate) fn encrypt(&self, account_id: &str, tokens: &Tokens) -> Result<String, StoreError> {
        self.encrypt_with_nonce_source(account_id, tokens, |nonce| {
            SystemRandom::new()
                .fill(nonce)
                .map_err(|_| StoreError::Crypto)
        })
    }

    fn encrypt_with_nonce_source(
        &self,
        account_id: &str,
        tokens: &Tokens,
        fill_nonce: impl FnOnce(&mut [u8]) -> Result<(), StoreError>,
    ) -> Result<String, StoreError> {
        validate_id(account_id)?;
        let mut plaintext = BoundedPlaintext(Zeroizing::new(Vec::new()));
        serde_json::to_writer(&mut plaintext, tokens).map_err(|_| StoreError::InvalidData)?;
        let mut nonce = [0; NONCE_LEN];
        fill_nonce(&mut nonce)?;
        self.seal_bytes(account_id, plaintext.0, nonce)
    }

    pub(crate) fn decrypt(&self, account_id: &str, encoded: &str) -> Result<Tokens, StoreError> {
        validate_id(account_id)?;
        let payload = encoded.strip_prefix(PREFIX).ok_or(StoreError::Crypto)?;
        if payload.len() > MAX_BASE64 {
            return Err(StoreError::Crypto);
        }
        let mut bytes = Zeroizing::new(STANDARD.decode(payload).map_err(|_| StoreError::Crypto)?);
        if bytes.len() < NONCE_LEN + TAG_LEN || bytes.len() > MAX_BINARY {
            return Err(StoreError::Crypto);
        }
        let nonce: [u8; NONCE_LEN] = bytes[..NONCE_LEN]
            .try_into()
            .map_err(|_| StoreError::Crypto)?;
        let aad = account_aad(account_id);
        let key = self.aead()?;
        let plaintext = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut bytes[NONCE_LEN..],
            )
            .map_err(|_| StoreError::Crypto)?;
        serde_json::from_slice(plaintext).map_err(|_| StoreError::InvalidData)
    }

    fn aead(&self) -> Result<LessSafeKey, StoreError> {
        UnboundKey::new(&aead::AES_256_GCM, &self.key[..])
            .map(LessSafeKey::new)
            .map_err(|_| StoreError::Crypto)
    }

    // Private leaf permits deterministic synthetic-format tests without global RNG hooks.
    // Production calls this only with a fresh SystemRandom nonce.
    fn seal_bytes(
        &self,
        account_id: &str,
        mut plaintext: Zeroizing<Vec<u8>>,
        nonce: [u8; NONCE_LEN],
    ) -> Result<String, StoreError> {
        validate_id(account_id)?;
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(StoreError::InvalidData);
        }
        let aad = account_aad(account_id);
        self.aead()?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut *plaintext,
            )
            .map_err(|_| StoreError::Crypto)?;
        let mut envelope = Vec::with_capacity(NONCE_LEN + plaintext.len());
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&plaintext);
        Ok(format!("{PREFIX}{}", STANDARD.encode(envelope)))
    }
}

fn validate_id(id: &str) -> Result<(), StoreError> {
    if id.len() == 9
        && (id.starts_with("cl-") || id.starts_with("cx-"))
        && id.as_bytes()[3..]
            .iter()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(StoreError::Crypto)
    }
}

fn account_aad(id: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(DOMAIN.len() + id.len());
    aad.extend_from_slice(DOMAIN);
    aad.extend_from_slice(id.as_bytes());
    aad
}

struct BoundedPlaintext(Zeroizing<Vec<u8>>);
impl Write for BoundedPlaintext {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_PLAINTEXT.saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "account token payload exceeds limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
