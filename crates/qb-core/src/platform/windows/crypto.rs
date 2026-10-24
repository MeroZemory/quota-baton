use crate::accounts::{StoreError, Tokens};
use base64::{Engine, engine::general_purpose::STANDARD};
use windows::{
    Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    },
    core::PCWSTR,
};
use zeroize::{Zeroize, Zeroizing};

const ENTROPY: &[u8] = b"quota-baton/v1/account-secret";

fn transform(input: &mut [u8], protect: bool) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    let size = u32::try_from(input.len()).map_err(|_| StoreError::Crypto)?;
    let data = CRYPT_INTEGER_BLOB {
        cbData: size,
        pbData: input.as_mut_ptr(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // DPAPI가 할당한 버퍼는 복사 후 지우고 LocalFree로 반환한다.
    unsafe {
        let result = if protect {
            CryptProtectData(
                &data,
                PCWSTR::null(),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &data,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if result.is_err() {
            return Err(StoreError::Crypto);
        }
        let bytes = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let copied = Zeroizing::new(bytes.to_vec());
        bytes.zeroize();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(copied)
    }
}

pub(crate) fn encrypt_account(tokens: &Tokens) -> Result<String, StoreError> {
    let mut plaintext =
        Zeroizing::new(serde_json::to_vec(tokens).map_err(|_| StoreError::InvalidData)?);
    Ok(STANDARD.encode(transform(&mut plaintext, true)?))
}

pub(crate) fn decrypt_account(encoded: &str) -> Result<Tokens, StoreError> {
    let mut encrypted = Zeroizing::new(STANDARD.decode(encoded).map_err(|_| StoreError::Crypto)?);
    let plaintext = transform(&mut encrypted, false)?;
    serde_json::from_slice(&plaintext).map_err(|_| StoreError::InvalidData)
}

/// Store-scoped facade; Windows keeps its existing DPAPI envelope and entropy.
pub(crate) struct SecretProtector;
impl SecretProtector {
    pub(crate) fn encrypt(&self, _: &str, tokens: &Tokens) -> Result<String, StoreError> {
        encrypt_account(tokens)
    }
    pub(crate) fn decrypt(&self, _: &str, encoded: &str) -> Result<Tokens, StoreError> {
        decrypt_account(encoded)
    }
}
pub(crate) fn account_secret_protector(_: &std::path::Path) -> Result<SecretProtector, StoreError> {
    Ok(SecretProtector)
}
