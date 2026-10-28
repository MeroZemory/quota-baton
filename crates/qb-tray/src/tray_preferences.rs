//! Worker-only, per-home tray preferences. No account data is persisted here.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Preferences {
    pub(crate) excluded_expanded: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    schema: u8,
    excluded_expanded: bool,
}

fn directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
}

pub(crate) fn load(home: &Path) -> Preferences {
    let read = || -> Option<Preferences> {
        let state = home.join("state");
        if !directory(home) || !directory(&state) {
            return None;
        }
        let path = state.join("tray-ui.json");
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return None;
        }
        let mut bytes = Vec::new();
        fs::File::open(path)
            .ok()?
            .take(4097)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 4096 {
            return None;
        }
        let file: File = serde_json::from_slice(&bytes).ok()?;
        (file.schema == 1).then_some(Preferences {
            excluded_expanded: file.excluded_expanded,
        })
    };
    read().unwrap_or_default()
}

pub(crate) fn save(home: &Path, preferences: Preferences) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    save_with_counter(home, preferences, &NEXT)
}

fn save_with_counter(
    home: &Path,
    preferences: Preferences,
    next: &AtomicU64,
) -> std::io::Result<()> {
    if !directory(home) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let state = home.join("state");
    match fs::create_dir(&state) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    if !directory(&state) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let path = state.join("tray-ui.json");
    if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let bytes = serde_json::to_vec(&File {
        schema: 1,
        excluded_expanded: preferences.excluded_expanded,
    })?;
    let mut created = None;
    for _ in 0..8 {
        let path = state.join(format!(
            ".tray-ui-{}-{}.tmp",
            std::process::id(),
            next.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                created = Some((path, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let (temporary, mut file) = created.ok_or(std::io::ErrorKind::AlreadyExists)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r19_disclosure_survives_restart_and_is_separate_per_home() -> anyhow::Result<()> {
        let one = tempfile::tempdir()?;
        let two = tempfile::tempdir()?;
        assert!(!load(one.path()).excluded_expanded);
        save(
            one.path(),
            Preferences {
                excluded_expanded: true,
            },
        )?;
        assert!(load(one.path()).excluded_expanded);
        assert!(!load(two.path()).excluded_expanded);
        save(one.path(), Preferences::default())?;
        assert!(!load(one.path()).excluded_expanded);
        assert_eq!(fs::read_dir(one.path().join("state"))?.count(), 1);
        Ok(())
    }

    #[test]
    fn r19_unknown_or_broken_preferences_collapse_without_overwriting() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        fs::create_dir(home.path().join("state"))?;
        let path = home.path().join("state/tray-ui.json");
        for bytes in [
            br#"{"schema":2,"excluded_expanded":true}"#.as_slice(),
            br#"{"schema":1,"excluded_expanded":true,"account":"unexpected"}"#,
            b"{",
        ] {
            fs::write(&path, bytes)?;
            assert!(!load(home.path()).excluded_expanded);
            assert_eq!(fs::read(&path)?, bytes);
        }
        Ok(())
    }

    #[test]
    fn r19_leftover_temporary_file_is_preserved_and_does_not_lose_the_choice() -> anyhow::Result<()>
    {
        let home = tempfile::tempdir()?;
        let state = home.path().join("state");
        fs::create_dir(&state)?;
        let leftover = state.join(format!(".tray-ui-{}-0.tmp", std::process::id()));
        fs::write(&leftover, b"earlier interrupted write")?;
        save_with_counter(
            home.path(),
            Preferences {
                excluded_expanded: true,
            },
            &AtomicU64::new(0),
        )?;
        assert!(load(home.path()).excluded_expanded);
        assert_eq!(fs::read(&leftover)?, b"earlier interrupted write");
        assert_eq!(fs::read_dir(state)?.count(), 2);
        Ok(())
    }
}
