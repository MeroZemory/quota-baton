//! Explicit fixture-only store. Never opens the registry or creates a parent.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0},
        Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
        System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
    },
    core::PCWSTR,
};

const ERROR: &str = "test autostart registry unavailable";
const MAGIC: &str = "quota-baton-autostart-test";
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_ENTRIES: usize = 128;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    magic: String,
    version: u32,
    values: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Value {
    command: String,
    value_type: u32,
}

struct Lock(HANDLE);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

pub(super) struct FileRegistry {
    path: PathBuf,
    // Held across the entire autostart operation, including read/modify/write.
    _lock: Lock,
}

fn fixed<T, E>(result: std::result::Result<T, E>) -> Result<T> {
    result.map_err(|_| anyhow::anyhow!(ERROR))
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

impl FileRegistry {
    pub(super) fn open(marker: &OsStr) -> Result<Self> {
        let path = Path::new(marker);
        let text = marker.to_str().ok_or_else(|| anyhow::anyhow!(ERROR))?;
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or("");
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].into_iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            });
        if !path.is_absolute()
            || text.contains('\0')
            // Verbatim (`\\?\`) paths keep `..` as a normal component.
            || path.components().any(|part| {
                matches!(part, Component::ParentDir)
                    || matches!(part.as_os_str().to_str(), Some(".." | "."))
            })
            || name.is_empty()
            || device
            || name
                .chars()
                .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
            || name.ends_with([' ', '.'])
        {
            bail!(ERROR);
        }
        // Serialize the private fixture directory: even 8.3 filename aliases
        // must share the lock. No lock file or other persistent sidecar.
        let parent = fixed(path.parent().ok_or(()))?;
        let parent = fixed(parent.canonicalize())?;
        if !parent.is_dir() {
            bail!(ERROR);
        }
        // Stable FNV-1a; collisions only serialize unrelated fixtures.
        let mut hash = 0xcbf29ce484222325u64;
        for word in parent
            .as_os_str()
            .to_string_lossy()
            .to_lowercase()
            .encode_utf16()
        {
            for byte in word.to_le_bytes() {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
            }
        }
        let mutex = wide(OsStr::new(&format!("Local\\QBTestAutostart-{hash:016x}")));
        let handle = fixed(unsafe { CreateMutexW(None, false, PCWSTR(mutex.as_ptr())) })?;
        let wait = unsafe { WaitForSingleObject(handle, 30_000) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            unsafe {
                let _ = CloseHandle(handle);
            }
            bail!(ERROR);
        }
        let registry = Self {
            path: parent.join(name),
            _lock: Lock(handle),
        };
        // Reject malformed/foreign files before handing a backend to the caller.
        registry.load()?;
        Ok(registry)
    }

    fn load(&self) -> Result<Store> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Store {
                    magic: MAGIC.into(),
                    version: 1,
                    values: BTreeMap::new(),
                });
            }
            Err(_) => bail!(ERROR),
        };
        // Refuse reparse points (including symlinks) rather than following them.
        if !metadata.is_file()
            || metadata.file_attributes() & 0x400 != 0
            || metadata.len() > MAX_FILE_BYTES as u64
        {
            bail!(ERROR);
        }
        let mut bytes = Vec::new();
        fixed(
            fixed(File::open(&self.path))?
                .take(MAX_FILE_BYTES as u64 + 1)
                .read_to_end(&mut bytes),
        )?;
        if bytes.len() > MAX_FILE_BYTES {
            bail!(ERROR);
        }
        let store: Store = fixed(serde_json::from_slice(&bytes))?;
        if store.magic != MAGIC || store.version != 1 || store.values.len() > MAX_ENTRIES {
            bail!(ERROR);
        }
        for (name, value) in &store.values {
            if name.is_empty() || name.len() > 256 || name.contains('\0') {
                bail!(ERROR);
            }
            fixed(validate(&value.command, value.value_type))?;
        }
        Ok(store)
    }

    fn save(&self, store: &Store) -> Result<()> {
        // Revalidate the destination before any replacement or deletion.
        self.load()?;
        if store.values.is_empty() {
            return match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(anyhow::anyhow!(ERROR)),
            };
        }
        let bytes = fixed(serde_json::to_vec(store))?;
        if bytes.len() > MAX_FILE_BYTES || store.values.len() > MAX_ENTRIES {
            bail!(ERROR);
        }
        // Only a newly-created sibling is written; never truncate a destination.
        let temp = self
            .path
            .with_extension(format!("{:032x}.tmp", rand::random::<u128>()));
        let mut file = fixed(OpenOptions::new().write(true).create_new(true).open(&temp))?;
        let result = (|| {
            fixed(file.write_all(&bytes))?;
            fixed(file.sync_all())?;
            drop(file);
            self.load()?;
            let source = wide(temp.as_os_str());
            let destination = wide(self.path.as_os_str());
            fixed(unsafe {
                MoveFileExW(
                    PCWSTR(source.as_ptr()),
                    PCWSTR(destination.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

impl Registry for FileRegistry {
    fn read(&mut self, name: &str) -> Result<(Option<String>, u32)> {
        Ok(self
            .load()?
            .values
            .remove(name)
            .map_or((None, STRING_TYPE), |value| {
                (Some(value.command), value.value_type)
            }))
    }

    fn set(&mut self, name: &str, command: &str, value_type: u32) -> Result<()> {
        fixed(validate(command, value_type))?;
        if name.is_empty() || name.len() > 256 || name.contains('\0') {
            bail!(ERROR);
        }
        let mut store = self.load()?;
        let value = Value {
            command: command.into(),
            value_type,
        };
        if store.values.get(name) != Some(&value) {
            store.values.insert(name.into(), value);
            self.save(&store)?;
        }
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Result<()> {
        let mut store = self.load()?;
        if store.values.remove(name).is_some() {
            self.save(&store)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "test_file_tests.rs"]
mod tests;
