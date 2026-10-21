//! Dedicated account aliases use ordinary journal publication and removal.
use super::*;
use qb_core::config::Config;
use std::collections::BTreeSet;

/// Explicit metadata is required: a codex-*.cmd filename grants no ownership.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Record {
    pub short: String,
    pub account_id: String,
}

#[derive(Clone, Debug)]
pub(super) struct Planned {
    pub record: Record,
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

fn validate_record(record: &Record) -> Result<()> {
    anyhow::ensure!(
        record.short.len() == 2
            && record.short.bytes().all(|b| b.is_ascii_lowercase())
            && record.short != "qb",
        "invalid account alias suffix"
    );
    // Fail closed on batch metacharacters instead of attempting CALL/reparse
    // escaping. Spaces are safe inside quotes; backslashes/quotes are excluded
    // so Windows argument decoding cannot turn the closing quote into data.
    anyhow::ensure!(
        !record.account_id.is_empty()
            && record.account_id.len() <= 256
            && !record.account_id.starts_with('-')
            && record.account_id.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@' | b' ')
            }),
        "account id cannot be represented safely by an alias"
    );
    Ok(())
}

fn path(npm: &Path, record: &Record) -> Result<PathBuf> {
    validate_record(record)?;
    anyhow::ensure!(npm.is_absolute(), "alias npm path must be absolute");
    anyhow::ensure!(
        !npm.components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "alias npm path must not contain parent traversal"
    );
    Ok(npm.join(format!("codex-{}.cmd", record.short)))
}

pub(super) fn launcher_bytes(record: &Record) -> Result<Vec<u8>> {
    validate_record(record)?;
    // Tail-chain to the sibling batch file: no CALL or extra cmd /c reparsing.
    // %~dp0 is quoted, independent of PATH, and expands only once. Disable
    // delayed expansion before both the account operand and caller arguments.
    // The established codex-qb launcher owns CODEX_HOME, codepage, title, exit.
    Ok(format!(
        concat!(
            "@echo off\r\n",
            "setlocal DisableDelayedExpansion\r\n",
            "rem quota-baton account preference alias\r\n",
            "\"%~dp0codex-qb.cmd\" --account \"{}\" %*\r\n",
        ),
        record.account_id
    )
    .into_bytes())
}

/// Pure desired plan. Even programmatically constructed Config must validate.
/// No aliases outside dedicated mode; no credentials, homes or native commands.
pub(super) fn plan(config: &Config, npm: &Path, mode: CodexMode) -> Result<Vec<Planned>> {
    config.validate_account_aliases()?;
    if mode != CodexMode::Dedicated {
        return Ok(Vec::new());
    }
    config
        .accounts
        .iter()
        .filter_map(|(id, account)| {
            account.short.as_ref().map(|short| Record {
                short: short.clone(),
                account_id: id.clone(),
            })
        })
        .map(|record| {
            Ok(Planned {
                path: path(npm, &record)?,
                bytes: launcher_bytes(&record)?,
                record,
            })
        })
        .collect()
}

/// Read public metadata only; never acquire a store lease or decrypt tokens.
pub(super) fn desired(paths: &CutoverPaths, mode: CodexMode) -> Result<Vec<Planned>> {
    let plans = plan(&Config::load(&paths.qb_home)?, &paths.npm_dir, mode)?;
    if !plans.is_empty() {
        validate_accounts(&plans, &AccountStore::list_metadata(&paths.qb_home)?)?;
    }
    Ok(plans)
}

fn validate_accounts(plans: &[Planned], accounts: &[qb_core::accounts::Account]) -> Result<()> {
    for item in plans {
        let mut found = accounts.iter().filter(|a| a.id == item.record.account_id);
        let account = found.next().context("alias account does not exist")?;
        anyhow::ensure!(
            found.next().is_none()
                && account.provider == qb_core::accounts::Provider::Codex
                && account.identity.provider() == qb_core::accounts::Provider::Codex,
            "alias requires an unambiguous Codex account"
        );
    }
    Ok(())
}

/// Check every alias before wire/unwire writes, including interrupted objects.
pub(super) fn preflight_saved(m: &Manifest, force: bool) -> Result<()> {
    for e in m
        .entries
        .iter()
        .filter(|e| allowed(&m.npm, &m.account_aliases, &e.path))
    {
        ancestors_safe(&e.path)?;
        if e.removed {
            continue;
        }
        if e.upgrade.is_some() {
            upgrade_state(e)?;
        } else if exists(&e.path)? {
            anyhow::ensure!(
                (e.identity.is_some() || e.pending_identity.is_some()) && matches_entry(e, force)?,
                "account alias changed"
            );
        } else {
            anyhow::ensure!(!e.done, "owned account alias is missing");
        }
        if let Some(tmp) = &e.pending {
            if exists(tmp)? {
                if e.pending_identity.is_some() {
                    // An unfinished owned scratch may still be empty. The
                    // installer rewrites it from the protected saved payload.
                    anyhow::ensure!(
                        pending_matches(e, force || !e.done || e.upgrade.is_some())?,
                        "account alias scratch changed"
                    );
                } else {
                    // Existing recovery preserves unproven random scratch and
                    // creates a fresh sibling; it never adopts or deletes it.
                    anyhow::ensure!(
                        (!e.done || e.upgrade.is_some()) && scratch_scoped(e, tmp),
                        "unowned alias scratch"
                    );
                }
            } else if !exists(&e.path)? {
                anyhow::ensure!(
                    e.pending_identity.is_none(),
                    "owned alias scratch is missing"
                );
            }
        }
    }
    Ok(())
}

/// Exact allow-list member; only call after validate_records succeeds.
pub(super) fn allowed(npm: &Path, records: &[Record], candidate: &Path) -> bool {
    records
        .iter()
        .any(|r| path(npm, r).is_ok_and(|p| p == candidate))
}

/// Validate persisted metadata and entry shape without reading current config.
/// Removed records remain valid for interrupted unwire recovery. The existing
/// manifest validator must still check all identities, scratch and upgrade data.
pub(super) fn validate_records(
    mode: CodexMode,
    npm: &Path,
    records: &[Record],
    entries: &[Entry],
) -> Result<()> {
    anyhow::ensure!(
        mode == CodexMode::Dedicated || records.is_empty(),
        "aliases require dedicated mode"
    );
    let mut seen = BTreeSet::new();
    let mut accounts = BTreeSet::new();
    for record in records {
        let p = path(npm, record)?;
        anyhow::ensure!(
            seen.insert(p.clone()) && accounts.insert(&record.account_id),
            "duplicate alias metadata"
        );
        let mut found = entries.iter().filter(|e| e.path == p);
        let entry = found.next().context("alias has no journal entry")?;
        anyhow::ensure!(found.next().is_none(), "duplicate alias entry");
        anyhow::ensure!(
            entry.kind == Kind::File
                && entry.target.is_none()
                && entry.original.is_none()
                && entry.image_base.is_none(),
            "invalid alias journal entry shape"
        );
        // Generated payloads may be versioned later. Recovery must use saved
        // bytes/hash, never regenerate an interrupted install from config.
        if let Some(payload) = &entry.payload {
            anyhow::ensure!(
                hash_bytes(&unprotect(payload)?) == entry.hash,
                "alias payload hash mismatch"
            );
        }
    }
    Ok(())
}

/// Read-only full-batch conflict preflight for additions. Never adopts even
/// byte-identical files. exists() uses symlink_metadata, including dangling links.
/// A coordinator must run this BEFORE any existing upgrade/recovery writes too.
pub(super) fn preflight_new(plans: &[Planned]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for item in plans {
        // Recheck caller-provided plans: these fields are not an authority.
        let parent = item.path.parent().context("alias parent missing")?;
        anyhow::ensure!(
            path(parent, &item.record)? == item.path
                && launcher_bytes(&item.record)? == item.bytes
                && seen.insert(item.path.clone()),
            "invalid alias plan"
        );
        ancestors_safe(&item.path)?;
        anyhow::ensure!(
            !exists(&item.path)?,
            "account alias destination already exists"
        );
        anyhow::ensure!(
            !exists(&item.path.with_extension("qb-pending"))?,
            "account alias pending path already exists"
        );
    }
    Ok(())
}

/// Produce ordinary File entries for existing exclusive scratch publication,
/// journal intent/recovery, identity checking and unwire. No filesystem writes.
pub(super) fn entries_for_new(plans: &[Planned], apply: bool) -> Result<Vec<Entry>> {
    preflight_new(plans)?;
    plans
        .iter()
        .map(|item| {
            new_entry(
                item.path.clone(),
                Kind::File,
                None,
                Some(&item.bytes),
                apply,
            )
        })
        .collect()
}

#[cfg(test)]
/// Completed aliases are not adopted by filename or byte equality. Check the
/// original object AND hash; force must never bypass this upgrade preflight.
pub(super) fn preflight_owned(npm: &Path, records: &[Record], entries: &[Entry]) -> Result<()> {
    validate_records(CodexMode::Dedicated, npm, records, entries)?;
    for record in records {
        let p = path(npm, record)?;
        let e = entries
            .iter()
            .find(|e| e.path == p)
            .context("alias entry missing")?;
        ancestors_safe(&e.path)?;
        anyhow::ensure!(
            e.done && !e.removed && exists(&e.path)? && matches_entry(e, false)?,
            "owned account alias changed"
        );
    }
    Ok(())
}

/// Conservative config changes: allow additions; refuse remapping/removal until
/// unwire. This prevents a command silently changing its stable account target.
pub(super) fn additions(desired: &[Planned], saved: &[Record]) -> Result<Vec<Planned>> {
    for record in saved {
        validate_record(record)?;
        anyhow::ensure!(
            desired.iter().any(|p| p.record == *record),
            "alias mapping changed; unwire first"
        );
    }
    Ok(desired
        .iter()
        .filter(|p| !saved.contains(&p.record))
        .cloned()
        .collect())
}

/// Hook for upgrade_path/upgrade_completed's explicit Codex alias dispatch.
/// Returns None for every unrelated command, never a Claude fallback.
pub(super) fn upgrade_bytes(
    npm: &Path,
    records: &[Record],
    candidate: &Path,
) -> Result<Option<Vec<u8>>> {
    for record in records {
        if path(npm, record)? == candidate {
            return launcher_bytes(record).map(Some);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(id: &str, short: &str) -> Config {
        let mut config = Config::default();
        config.accounts.insert(
            id.into(),
            qb_core::config::AccountConfig {
                short: Some(short.into()),
                ..Default::default()
            },
        );
        config
    }

    // Future test execution stays in the explicitly allowed ignored directory.
    fn scratch() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r11-alias-tests");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root.canonicalize().unwrap()).unwrap()
    }

    #[test]
    fn planner_and_batch_operand_are_exact() {
        let dir = scratch();
        let plans = plan(
            &config("stable id_1@example.com", "rh"),
            dir.path(),
            CodexMode::Dedicated,
        )
        .unwrap();
        assert_eq!(plans[0].path, dir.path().join("codex-rh.cmd"));
        assert_eq!(
            String::from_utf8(plans[0].bytes.clone()).unwrap(),
            concat!(
                "@echo off\r\nsetlocal DisableDelayedExpansion\r\n",
                "rem quota-baton account preference alias\r\n",
                "\"%~dp0codex-qb.cmd\" --account \"stable id_1@example.com\" %*\r\n"
            )
        );
        for mode in [CodexMode::None, CodexMode::Default] {
            assert!(
                plan(&config("one", "rh"), dir.path(), mode)
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            plan(&Config::default(), dir.path(), CodexMode::Dedicated)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malicious_ids_and_suffixes_fail_before_publication() {
        let dir = scratch();
        for id in [
            "",
            "-x",
            "x%PATH%",
            "x!PATH!",
            "x\" & echo bad",
            "x&echo bad",
            "x|y",
            "x>y",
            "x<y",
            "x^y",
            "x(y)",
            "x\ny",
            "x\ry",
            "x\0y",
            "x\\",
            "x/y",
        ] {
            assert!(plan(&config(id, "rh"), dir.path(), CodexMode::Dedicated).is_err());
        }
        assert!(
            plan(
                &config(&"x".repeat(257), "rh"),
                dir.path(),
                CodexMode::Dedicated
            )
            .is_err()
        );
        for short in ["qb", "RH", "r", "rhh", "../", "r%", "r&", "챕"] {
            assert!(plan(&config("one", short), dir.path(), CodexMode::Dedicated).is_err());
        }
        let mut cfg = config("one", "rh");
        cfg.accounts
            .insert("two".into(), cfg.accounts["one"].clone());
        assert!(plan(&cfg, dir.path(), CodexMode::Dedicated).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn late_collision_preserves_file_and_creates_nothing() {
        let dir = scratch();
        let mut cfg = config("a", "aa");
        cfg.accounts.insert(
            "z".into(),
            qb_core::config::AccountConfig {
                short: Some("zz".into()),
                ..Default::default()
            },
        );
        let plans = plan(&cfg, dir.path(), CodexMode::Dedicated).unwrap();
        fs::write(&plans[1].path, b"foreign file").unwrap();
        assert!(entries_for_new(&plans, false).is_err());
        assert!(!plans[0].path.exists());
        assert_eq!(fs::read(&plans[1].path).unwrap(), b"foreign file");
        // Same bytes grant no ownership either.
        fs::write(&plans[1].path, &plans[1].bytes).unwrap();
        assert!(entries_for_new(&plans, false).is_err());
        assert_eq!(fs::read(&plans[1].path).unwrap(), plans[1].bytes);
        fs::remove_file(&plans[1].path).unwrap();
        let pending = plans[1].path.with_extension("qb-pending");
        fs::write(&pending, b"foreign pending").unwrap();
        assert!(entries_for_new(&plans, false).is_err());
        assert_eq!(fs::read(&pending).unwrap(), b"foreign pending");
        assert!(!plans[0].path.exists());
    }

    #[test]
    fn journal_scope_and_identity_are_required() {
        let dir = scratch();
        let plans = plan(&config("one", "rh"), dir.path(), CodexMode::Dedicated).unwrap();
        let records = vec![plans[0].record.clone()];
        let mut entries = entries_for_new(&plans, false).unwrap();
        validate_records(CodexMode::Dedicated, dir.path(), &records, &entries).unwrap();
        assert!(!allowed(dir.path(), &[], &plans[0].path));
        assert!(allowed(dir.path(), &records, &plans[0].path));
        assert!(!allowed(
            dir.path(),
            &records,
            &dir.path().join("codex-qb.cmd")
        ));
        assert!(!allowed(
            dir.path(),
            &records,
            &dir.path().join("codex-ib.cmd")
        ));
        assert!(validate_records(CodexMode::None, dir.path(), &records, &entries).is_err());
        assert!(validate_records(CodexMode::Dedicated, dir.path(), &records, &[]).is_err());
        entries[0].kind = Kind::Hardlink;
        assert!(validate_records(CodexMode::Dedicated, dir.path(), &records, &entries).is_err());
        entries[0].kind = Kind::File;
        fs::write(&plans[0].path, &plans[0].bytes).unwrap();
        entries[0].done = true;
        // Matching bytes without a journaled identity remain foreign.
        assert!(preflight_owned(dir.path(), &records, &entries).is_err());
        entries[0].identity = Some(identity(&plans[0].path).unwrap());
        preflight_owned(dir.path(), &records, &entries).unwrap();
        fs::write(&plans[0].path, b"local edit").unwrap();
        assert!(preflight_owned(dir.path(), &records, &entries).is_err());
        fs::write(&plans[0].path, &plans[0].bytes).unwrap();
        // Hold old object under another name so its ID cannot be reused.
        fs::rename(&plans[0].path, dir.path().join("held-old.cmd")).unwrap();
        fs::write(&plans[0].path, &plans[0].bytes).unwrap();
        assert!(preflight_owned(dir.path(), &records, &entries).is_err());
    }

    #[test]
    fn mapping_changes_require_unwire_and_upgrades_ignore_unrelated_commands() {
        let dir = scratch();
        let original = plan(&config("one", "rh"), dir.path(), CodexMode::Dedicated).unwrap();
        let saved = vec![original[0].record.clone()];
        assert!(additions(&original, &saved).unwrap().is_empty());
        let changed = plan(&config("two", "rh"), dir.path(), CodexMode::Dedicated).unwrap();
        assert!(additions(&changed, &saved).is_err());
        assert!(additions(&[], &saved).is_err());
        assert_eq!(
            upgrade_bytes(dir.path(), &saved, &original[0].path).unwrap(),
            Some(original[0].bytes.clone())
        );
        for command in [
            "codex.cmd",
            "codex-qb.cmd",
            "codex-ib.cmd",
            "codex-mz.cmd",
            "codex-cpa.cmd",
            "claude-qb.cmd",
        ] {
            assert!(
                upgrade_bytes(dir.path(), &saved, &dir.path().join(command))
                    .unwrap()
                    .is_none()
            );
        }
    }
}
