//! Filesystem-only preservation of a caller-validated empty failed capture.
//! The caller holds the native Store lease after the previous writer closed.
//! This helper neither interprets the checkpoint nor prepares a replacement root.
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{ffi::OsStr, fs, io::ErrorKind, path::Path};

const PREFIX: &str = "forecast-log.failed-";
const MAX_ARCHIVES: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub(super) archive: String,
    pub(super) at: i64,
    pub(super) prior_losses: u64,
}

pub(super) fn archive_empty(root: &Path, prior_losses: u64) -> Result<Receipt> {
    archive_empty_at(root, prior_losses, Utc::now(), rand::random())
}

fn plain_directory(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).context("forecast_log_recovery_directory_metadata")?;
    ensure!(
        metadata.file_type().is_dir()
            && !metadata.file_type().is_symlink()
            && !crate::platform::metadata_is_reparse(&metadata),
        "forecast_log_recovery_directory_not_plain"
    );
    Ok(())
}

fn checkpoint_only(root: &Path) -> Result<()> {
    plain_directory(root)?;
    let mut entries = fs::read_dir(root).context("forecast_log_recovery_directory_read")?;
    let entry = entries
        .next()
        .transpose()?
        .context("forecast_log_recovery_checkpoint_missing")?;
    ensure!(
        entry.file_name().as_os_str() == OsStr::new("checkpoint.json")
            && entries.next().transpose()?.is_none(),
        "forecast_log_recovery_not_checkpoint_only"
    );
    let metadata =
        fs::symlink_metadata(entry.path()).context("forecast_log_recovery_checkpoint_metadata")?;
    ensure!(
        metadata.file_type().is_file()
            && !metadata.file_type().is_symlink()
            && !crate::platform::metadata_is_reparse(&metadata),
        "forecast_log_recovery_checkpoint_not_plain"
    );
    Ok(())
}

fn archive_empty_at(
    root: &Path,
    prior_losses: u64,
    at: DateTime<Utc>,
    nonce: u128,
) -> Result<Receipt> {
    ensure!(
        root.file_name() == Some(OsStr::new("forecast-log"))
            && !root
                .components()
                .any(|c| c == std::path::Component::ParentDir),
        "forecast_log_recovery_root_name"
    );
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    plain_directory(parent)?;
    checkpoint_only(root)?;

    // Count every occupied failure-prefix name, without opening prior archives.
    // No retention deletion or permission repair is part of recovery.
    let mut archives = 0;
    for entry in fs::read_dir(parent).context("forecast_log_recovery_parent_read")? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(PREFIX) {
            archives += 1;
            ensure!(archives < MAX_ARCHIVES, "forecast_log_recovery_archive_cap");
        }
    }
    let archive = format!("{PREFIX}{}-{nonce:032x}", at.format("%Y%m%dT%H%M%SZ"));
    let destination = parent.join(&archive);
    match fs::symlink_metadata(&destination) {
        Ok(_) => bail!("forecast_log_recovery_destination_exists"),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("forecast_log_recovery_destination_metadata"),
    }
    // Recheck immediately before the one rename. Store-lease ownership serializes
    // cooperating recovery writers; the 128-bit nonce avoids shared-name reuse.
    // Never remove a destination or fall back to copy/delete/overwrite on failure.
    checkpoint_only(root)?;
    fs::rename(root, &destination).context("forecast_log_recovery_rename")?;
    // No fallible writes after rename: a later caller failure leaves this archive
    // intact. Main persists the returned lineage when it prepares the new root.
    Ok(Receipt {
        archive,
        at: at.timestamp(),
        prior_losses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
        tempfile::Builder::new()
            .prefix("qb-forecast-recovery-")
            .tempdir_in(executable.parent().unwrap())
            .unwrap()
    }

    fn empty_capture(home: &Path) -> std::path::PathBuf {
        let root = home.join("forecast-log");
        crate::platform::prepare_observation_directory(&root).unwrap();
        // Deliberately opaque: recovery must not parse, normalize, or rewrite it.
        crate::platform::atomic_write_observation(
            &root.join("checkpoint.json"),
            b"opaque legacy checkpoint\r\n\0unchanged",
        )
        .unwrap();
        root
    }

    #[test]
    fn archives_exact_checkpoint_bytes_and_returns_lineage_without_new_root() {
        let home = fixture();
        let root = empty_capture(home.path());
        let checkpoint = root.join("checkpoint.json");
        let before = fs::read(&checkpoint).unwrap();
        let permissions = fs::metadata(&checkpoint).unwrap().permissions();
        let receipt = archive_empty(&root, 7).unwrap();
        assert!(receipt.archive.starts_with(PREFIX));
        assert_eq!(receipt.prior_losses, 7);
        assert!(receipt.at > 0);
        assert!(!root.exists(), "replacement root belongs to Main");
        let archive = home.path().join(&receipt.archive);
        assert_eq!(fs::read(archive.join("checkpoint.json")).unwrap(), before);
        assert_eq!(fs::read_dir(&archive).unwrap().count(), 1);
        assert_eq!(
            fs::metadata(archive.join("checkpoint.json"))
                .unwrap()
                .permissions(),
            permissions
        );
        let encoded = serde_json::to_value(&receipt).unwrap();
        let decoded: Receipt = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded.archive, receipt.archive);
        assert_eq!(decoded.at, receipt.at);
        assert_eq!(decoded.prior_losses, receipt.prior_losses);
        let mut unknown = encoded;
        unknown["unexpected"] = true.into();
        assert!(serde_json::from_value::<Receipt>(unknown).is_err());
    }

    #[test]
    fn extra_journal_or_unrecognized_entry_refuses_without_changes() {
        for extra in ["day-1.jsonl", "unrecognized"] {
            let home = fixture();
            let root = empty_capture(home.path());
            fs::write(root.join(extra), b"keep extra bytes").unwrap();
            let checkpoint = fs::read(root.join("checkpoint.json")).unwrap();
            assert!(archive_empty(&root, 1).is_err());
            assert_eq!(fs::read(root.join("checkpoint.json")).unwrap(), checkpoint);
            assert_eq!(fs::read(root.join(extra)).unwrap(), b"keep extra bytes");
            assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn occupied_destination_is_preserved_without_overwrite_or_fallback() {
        for nonempty in [false, true] {
            let home = fixture();
            let root = empty_capture(home.path());
            let at = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
            let destination = home.path().join(format!(
                "{PREFIX}{}-{:032x}",
                at.format("%Y%m%dT%H%M%SZ"),
                5
            ));
            crate::platform::prepare_observation_directory(&destination).unwrap();
            if nonempty {
                fs::write(destination.join("sentinel"), b"original archive").unwrap();
            }
            let checkpoint = fs::read(root.join("checkpoint.json")).unwrap();
            assert!(archive_empty_at(&root, 1, at, 5).is_err());
            assert!(destination.is_dir());
            if nonempty {
                assert_eq!(
                    fs::read(destination.join("sentinel")).unwrap(),
                    b"original archive"
                );
            } else {
                assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
            }
            assert_eq!(fs::read(root.join("checkpoint.json")).unwrap(), checkpoint);
        }
    }

    #[test]
    fn sixteen_archive_limit_refuses_without_removing_any_prior_capture() {
        let home = fixture();
        let root = empty_capture(home.path());
        for n in 0..MAX_ARCHIVES {
            let path = home.path().join(format!("{PREFIX}{n}"));
            crate::platform::prepare_observation_directory(&path).unwrap();
            fs::write(path.join("sentinel"), [n as u8]).unwrap();
        }
        assert!(archive_empty(&root, 1).is_err());
        assert!(root.join("checkpoint.json").is_file());
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), MAX_ARCHIVES + 1);
        for n in 0..MAX_ARCHIVES {
            assert_eq!(
                fs::read(home.path().join(format!("{PREFIX}{n}/sentinel"))).unwrap(),
                [n as u8]
            );
        }
    }

    #[test]
    fn fifteenth_prior_archive_allows_sixteenth_and_never_reuses_a_name() {
        let home = fixture();
        let root = empty_capture(home.path());
        for n in 0..MAX_ARCHIVES - 1 {
            crate::platform::prepare_observation_directory(
                &home.path().join(format!("{PREFIX}{n}")),
            )
            .unwrap();
        }
        let receipt = archive_empty(&root, 3).unwrap();
        assert!(
            home.path()
                .join(receipt.archive)
                .join("checkpoint.json")
                .is_file()
        );
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), MAX_ARCHIVES);
    }

    #[test]
    fn nonplain_checkpoint_and_wrong_root_name_are_refused() {
        let home = fixture();
        let root = home.path().join("forecast-log");
        crate::platform::prepare_observation_directory(&root).unwrap();
        crate::platform::prepare_observation_directory(&root.join("checkpoint.json")).unwrap();
        assert!(archive_empty(&root, 1).is_err());
        assert!(root.join("checkpoint.json").is_dir());
        let wrong = home.path().join("other-log");
        crate::platform::prepare_observation_directory(&wrong).unwrap();
        assert!(archive_empty(&wrong, 1).is_err());
        assert!(wrong.is_dir());
    }
}
