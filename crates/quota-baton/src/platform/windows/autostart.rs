//! Current-user login autostart. Tests cannot construct the native backend.
use anyhow::{Result, bail};

pub(crate) const VALUE_NAME: &str = "quota-baton";
const LEGACY_VALUE_NAME: &str = "quota-baton-tray";
const STRING_TYPE: u32 = 1;
const EXPAND_STRING_TYPE: u32 = 2;
const MAX_VALUE_BYTES: usize = 64 * 1024;
const MALFORMED: &str = "malformed autostart registry value";

trait Registry {
    fn read(&mut self, name: &str) -> Result<(Option<String>, u32)>;
    fn set(&mut self, name: &str, command: &str, value_type: u32) -> Result<()>;
    fn remove(&mut self, name: &str) -> Result<()>;
}

pub(crate) fn read() -> Result<(Option<String>, u32)> {
    with_registry(|registry| registry.read(VALUE_NAME))
}

pub(crate) fn set(command: &str, value_type: u32) -> Result<()> {
    with_registry(|registry| register(registry, command, value_type))
}

pub(crate) fn remove_owned(apply: bool) -> Result<Vec<String>> {
    with_registry(|registry| remove_from(registry, apply))
}

pub(crate) fn status(english: bool) -> Result<String> {
    let (value, legacy) = with_registry(|registry| {
        Ok((
            registry.read(VALUE_NAME)?.0,
            registry.read(LEGACY_VALUE_NAME)?.0,
        ))
    })?;
    let label = if english {
        "Login autostart"
    } else {
        "로그인 시 자동 시작"
    };
    let registered = value.as_deref().is_some_and(owned) || legacy.as_deref().is_some_and(owned);
    let state = match (english, registered) {
        (true, true) => "registered",
        (true, false) => "not registered",
        (false, true) => "등록됨",
        (false, false) => "등록 안 됨",
    };
    let foreign = if english {
        "foreign value"
    } else {
        "다른 값"
    };
    let legacy_label = if english { "legacy" } else { "이전 항목" };
    let mut details = Vec::new();
    if let Some(value) = value {
        details.push(if !owned(&value) {
            format!("{VALUE_NAME}: {foreign} = {}", one_line(&value))
        } else if legacy.is_some() {
            format!("{VALUE_NAME} = {}", one_line(&value))
        } else {
            one_line(&value)
        });
    }
    if let Some(legacy) = legacy {
        let annotation = if owned(&legacy) {
            "".to_owned()
        } else {
            format!(" ({foreign})")
        };
        details.push(format!(
            "{legacy_label} {LEGACY_VALUE_NAME}{annotation} = {}",
            one_line(&legacy)
        ));
    }
    Ok(if details.is_empty() {
        format!("{label}: {state}")
    } else {
        format!("{label}: {state} ({})", details.join("; "))
    })
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn validate(command: &str, value_type: u32) -> Result<()> {
    if !matches!(value_type, STRING_TYPE | EXPAND_STRING_TYPE)
        || command.contains('\0')
        || command.encode_utf16().count() >= MAX_VALUE_BYTES / 2
    {
        bail!(MALFORMED);
    }
    Ok(())
}

fn decode(bytes: &[u8], value_type: u32) -> Result<String> {
    if !matches!(value_type, STRING_TYPE | EXPAND_STRING_TYPE)
        || bytes.len() < 2
        || bytes.len() > MAX_VALUE_BYTES
        || !bytes.len().is_multiple_of(2)
    {
        bail!(MALFORMED);
    }
    let words: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if words.last() != Some(&0) || words[..words.len() - 1].contains(&0) {
        bail!(MALFORMED);
    }
    String::from_utf16(&words[..words.len() - 1]).map_err(|_| anyhow::anyhow!(MALFORMED))
}

fn register(registry: &mut impl Registry, command: &str, value_type: u32) -> Result<()> {
    validate(command, value_type)?;
    let (current, current_type) = registry.read(VALUE_NAME)?;
    if current.as_deref() != Some(command) || current_type != value_type {
        registry.set(VALUE_NAME, command, value_type)?;
    }
    // Older opt-in releases used this name. Keep only one owned login entry.
    if let (Some(legacy), _) = registry.read(LEGACY_VALUE_NAME)?
        && owned(&legacy)
    {
        registry.remove(LEGACY_VALUE_NAME)?;
    }
    Ok(())
}

fn owned(command: &str) -> bool {
    let path = if command.starts_with('"') && command.ends_with('"') && command.len() >= 2 {
        &command[1..command.len() - 1]
    } else {
        if command.chars().any(char::is_whitespace) {
            return false;
        }
        command
    };
    if path
        .chars()
        .any(|c| c.is_control() || "\"|&<>^`%*?".contains(c))
    {
        return false;
    }
    let bytes = path.as_bytes();
    let drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
        && !path[2..].contains(':');
    let unc_absolute = path.strip_prefix(r"\\").is_some_and(|tail| {
        let parts: Vec<_> = tail.split(['\\', '/']).collect();
        parts.len() >= 3 && parts.iter().all(|part| !part.is_empty()) && !tail.contains(':')
    });
    let absolute = drive_absolute || unc_absolute;
    absolute
        && path
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("quota-baton-tray.exe"))
}

fn remove_from(registry: &mut impl Registry, apply: bool) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for name in [VALUE_NAME, LEGACY_VALUE_NAME] {
        if let (Some(command), _) = registry.read(name)? {
            if owned(&command) {
                lines.push(format!("autostart: remove {name} = {}", one_line(&command)));
                if apply {
                    registry.remove(name)?;
                }
            } else {
                lines.push(format!(
                    "autostart: preserve {name} = {}",
                    one_line(&command)
                ));
            }
        }
    }
    Ok(lines)
}

#[cfg(not(test))]
fn with_registry<T>(f: impl FnOnce(&mut native::NativeRegistry) -> Result<T>) -> Result<T> {
    f(&mut native::NativeRegistry)
}

#[cfg(test)]
fn with_registry<T>(f: impl FnOnce(&mut FakeRegistry) -> Result<T>) -> Result<T> {
    FAKE.with(|registry| f(&mut registry.borrow_mut()))
}

#[cfg(test)]
#[derive(Default)]
struct FakeRegistry {
    values: std::collections::BTreeMap<String, (String, u32)>,
    writes: usize,
}

#[cfg(test)]
impl Registry for FakeRegistry {
    fn read(&mut self, name: &str) -> Result<(Option<String>, u32)> {
        Ok(self
            .values
            .get(name)
            .map_or((None, STRING_TYPE), |(value, kind)| {
                (Some(value.clone()), *kind)
            }))
    }
    fn set(&mut self, name: &str, command: &str, value_type: u32) -> Result<()> {
        self.values
            .insert(name.into(), (command.into(), value_type));
        self.writes += 1;
        Ok(())
    }
    fn remove(&mut self, name: &str) -> Result<()> {
        self.values.remove(name);
        self.writes += 1;
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static FAKE: std::cell::RefCell<FakeRegistry> = std::cell::RefCell::new(FakeRegistry::default());
}

/// Reset this thread's backend, optionally seeding the canonical value.
#[cfg(test)]
pub(crate) fn set_fake(value: Option<(String, u32)>) {
    FAKE.with(|registry| {
        let mut registry = registry.borrow_mut();
        *registry = FakeRegistry::default();
        if let Some(value) = value {
            registry.values.insert(VALUE_NAME.into(), value);
        }
    });
}

#[cfg(test)]
pub(crate) fn reset_fake() {
    set_fake(None);
}

/// Reset this thread's backend and seed a canonical REG_SZ value without a write.
#[cfg(test)]
pub(crate) fn test_set(value: Option<&str>) {
    set_fake(value.map(|value| (value.to_owned(), STRING_TYPE)));
}

#[cfg(test)]
pub(crate) fn fake_writes() -> usize {
    FAKE.with(|registry| registry.borrow().writes)
}

#[cfg(not(test))]
#[path = "autostart/native.rs"]
mod native;

#[cfg(test)]
#[path = "autostart/tests.rs"]
mod tests;
