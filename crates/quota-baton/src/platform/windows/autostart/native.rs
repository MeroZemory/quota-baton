use super::*;
use windows::{
    Win32::{
        Foundation::ERROR_FILE_NOT_FOUND,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE,
            REG_SAM_FLAGS, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
            RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
        },
    },
    core::PCWSTR,
};

pub(super) struct NativeRegistry;
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn open(access: REG_SAM_FLAGS, create: bool) -> Result<Option<Key>> {
    let path = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let mut key = HKEY::default();
    let result = unsafe {
        if create {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                access,
                None,
                &mut key,
                None,
            )
        } else {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                None,
                access,
                &mut key,
            )
        }
    };
    if !create && result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if !result.is_ok() {
        bail!("cannot open autostart registry key");
    }
    Ok(Some(Key(key)))
}

impl Registry for NativeRegistry {
    fn read(&mut self, name: &str) -> Result<(Option<String>, u32)> {
        let Some(key) = open(KEY_QUERY_VALUE, false)? else {
            return Ok((None, STRING_TYPE));
        };
        let name = wide(name);
        // A single bounded query avoids trusting a size supplied by registry data.
        let mut bytes = vec![0u8; MAX_VALUE_BYTES];
        let mut length = MAX_VALUE_BYTES as u32;
        let mut kind = REG_VALUE_TYPE(0);
        let result = unsafe {
            RegQueryValueExW(
                key.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(bytes.as_mut_ptr()),
                Some(&mut length),
            )
        };
        if result == ERROR_FILE_NOT_FOUND {
            return Ok((None, STRING_TYPE));
        }
        if result == windows::Win32::Foundation::ERROR_MORE_DATA {
            bail!(MALFORMED);
        }
        if !result.is_ok() {
            bail!("cannot read autostart registry value");
        }
        let length = length as usize;
        if length > bytes.len() {
            bail!(MALFORMED);
        }
        let value = decode(&bytes[..length], kind.0)?;
        Ok((Some(value), kind.0))
    }

    fn set(&mut self, name: &str, command: &str, value_type: u32) -> Result<()> {
        validate(command, value_type)?;
        let key = open(KEY_SET_VALUE, true)?.expect("created Run key");
        let name = wide(name);
        let bytes: Vec<u8> = wide(command)
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        let result = unsafe {
            RegSetValueExW(
                key.0,
                PCWSTR(name.as_ptr()),
                None,
                REG_VALUE_TYPE(value_type),
                Some(&bytes),
            )
        };
        if !result.is_ok() {
            bail!("cannot write autostart registry value");
        }
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Result<()> {
        let Some(key) = open(KEY_SET_VALUE, false)? else {
            return Ok(());
        };
        let name = wide(name);
        let result = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };
        if !result.is_ok() && result != ERROR_FILE_NOT_FOUND {
            bail!("cannot remove autostart registry value");
        }
        Ok(())
    }
}
