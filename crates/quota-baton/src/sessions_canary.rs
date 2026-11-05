//! Native session compatibility, observed only in disposable synthetic homes.
//! This does not extend historical picker/daemon or ancestor protection claims.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use crate::{image_canary as isolation, sessions_db as db, sessions_guard as guard};

const REVISION: &str = "session-native-compatibility-1";
const LIMIT: u64 = 8 * 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct Refusal(&'static str);
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Refusal {}

pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    error.downcast_ref::<Refusal>().map(ToString::to_string)
}

const INSPECTION: &str = "현재 Codex의 대화 호환성을 확인하지 못했습니다. 잠시 후 다시 시도하세요";
const INCOMPATIBLE: &str =
    "현재 Codex는 대화 이동 호환성 검사에 통과하지 못했습니다. 대화 원본은 변경하지 않았습니다";

fn stage<T>(result: Result<T>, message: &'static str) -> Result<T> {
    #[cfg(test)]
    if let Err(error) = &result {
        eprintln!("synthetic session probe: {error:#}");
    }
    result.map_err(|error| {
        if error
            .downcast_ref::<crate::sessions_progress::Interrupted>()
            .is_some()
        {
            error
        } else {
            anyhow::Error::new(Refusal(message))
        }
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    revision: String,
    version: String,
    fingerprint: String,
    pass: bool,
}

fn cache_key(version: &str, fingerprint: &str) -> String {
    let digest = Sha256::digest(format!(
        "{REVISION}\0{}\0{version}\0{fingerprint}",
        std::env::consts::OS
    ));
    format!("pass-{digest:x}.json")
}

fn receipt_matches(bytes: &[u8], version: &str, fingerprint: &str) -> bool {
    bytes.len() <= 4096
        && serde_json::from_slice::<Receipt>(bytes).is_ok_and(|receipt| {
            receipt.pass
                && receipt.revision == REVISION
                && receipt.version == version
                && receipt.fingerprint == fingerprint
        })
}

fn cached(root: &Path, path: &Path, version: &str, fingerprint: &str) -> Result<bool> {
    guard::safe_path(root, path)?;
    if !path.try_exists()? {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    Ok(receipt_matches(&bytes, version, fingerprint))
}

pub(crate) fn check(program: &Path, qb_home: &Path, version: &str) -> Result<()> {
    check_inner(program, qb_home, version).map_err(|error| {
        if error.downcast_ref::<Refusal>().is_some()
            || error
                .downcast_ref::<crate::sessions_progress::Interrupted>()
                .is_some()
        {
            error
        } else {
            anyhow::Error::new(Refusal(INSPECTION))
        }
    })
}

fn check_inner(program: &Path, qb_home: &Path, version: &str) -> Result<()> {
    ensure!(qb_home.is_absolute() && program.is_absolute() && program.is_file());
    let base = qb_home.join("state/session-canary");
    guard::safe_path(qb_home, &base)?;
    qb_core::ensure_private_dir_all(&base)?;
    let _pin = guard::prepare_parent(qb_home, &base)?;
    // One probe at a time per qb home. Never inherit another process's partial pass.
    let lock_path = base.join("probe.lock");
    guard::safe_path(qb_home, &lock_path)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    let start = Instant::now();
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock)
                if start.elapsed() < Duration::from_secs(120) =>
            {
                crate::sessions_progress::check()?;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(anyhow::anyhow!("{error}")),
        }
    }
    let fingerprint = crate::launcher_label::compatibility_fingerprint(program);
    let cache = fingerprint
        .as_ref()
        .map(|f| base.join(cache_key(version, f)));
    if let (Some(path), Some(fingerprint)) = (&cache, &fingerprint)
        && cached(qb_home, path, version, fingerprint)?
    {
        return Ok(());
    }
    crate::sessions_progress::phase(crate::sessions_progress::Phase::Compatibility, None)?;
    let scratch = Scratch::create(qb_home, &base)?;
    let outcome = probe(program, &scratch.path);
    #[cfg(test)]
    if let Err(error) = &outcome {
        eprintln!("synthetic probe failure: {error:#}");
    }
    let cleanup = scratch.cleanup();
    cleanup?;
    // Preserve capability failures; only successful probes can publish a pass.
    outcome?;
    if let (Some(path), Some(fingerprint)) = (cache, fingerprint) {
        ensure!(
            crate::launcher_label::compatibility_fingerprint(program).as_ref()
                == Some(&fingerprint)
        );
        let receipt = Receipt {
            revision: REVISION.into(),
            version: version.into(),
            fingerprint,
            pass: true,
        };
        guard::safe_path(qb_home, &path)?;
        if path.try_exists()? {
            ensure!(fs::symlink_metadata(&path)?.is_file());
            fs::remove_file(&path)?;
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(&serde_json::to_vec(&receipt)?)?;
        file.sync_all()?;
    }
    Ok(())
}

struct Scratch {
    root: PathBuf,
    path: PathBuf,
}
impl Scratch {
    fn create(root: &Path, base: &Path) -> Result<Self> {
        for _ in 0..32 {
            let path = base.join(format!(
                "probe-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            guard::safe_path(root, &path)?;
            match qb_core::create_private_directory_new(&path) {
                Ok(()) => {
                    return Ok(Self {
                        root: root.into(),
                        path,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("scratch collision")
    }
    fn remove(&self) -> Result<()> {
        ensure!(self.path.parent() == Some(self.root.join("state/session-canary").as_path()));
        ensure!(
            self.path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("probe-"))
        );
        if self.path.try_exists()? {
            isolation::remove_owned(&self.root, &self.path, 0)?;
        }
        Ok(())
    }
    fn cleanup(self) -> Result<()> {
        self.remove()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

fn contents(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= LIMIT);
    Ok(bytes)
}

fn native(
    program: &Path,
    root: &Path,
    home: &Path,
    operation: &[&str],
    success: bool,
) -> Result<()> {
    crate::sessions_progress::check()?;
    let server = isolation::Server::start()?;
    let mut environment = isolation::environment(root, server.port);
    for (key, value) in &mut environment {
        if *key == "CODEX_HOME" {
            *value = home.as_os_str().to_owned();
        }
        if isolation::PATH_ENV.contains(key) {
            qb_core::ensure_private_dir_all(Path::new(value))?;
        }
    }
    let mut settings = isolation::settings(server.port);
    for setting in &mut settings {
        if setting == "features.image_generation=true" {
            *setting = "features.image_generation=false".into();
        }
    }
    fs::write(home.join("config.toml"), settings.join("\n"))?;
    let mut args: Vec<OsString> = [
        "--no-daemon",
        "exec",
        "--ignore-user-config",
        "--ignore-rules",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    for setting in settings {
        args.extend(["-c".into(), setting.into()]);
    }
    args.extend(operation.iter().map(OsString::from));
    args.extend(["--".into(), "Reply OK without calling any tools.".into()]);
    let native = crate::launcher_label::canary_run(program, &args, &environment, root);
    #[cfg(test)]
    eprintln!("synthetic native operation {operation:?}: {native:?}");
    let served = server.finish();
    stage(served.map(|_| ()), INSPECTION)?;
    match native {
        Ok(()) => Ok(()),
        Err(crate::launcher_label::CanaryRunError::Exit) if !success => Ok(()),
        Err(crate::launcher_label::CanaryRunError::Exit) => Err(Refusal(INCOMPATIBLE).into()),
        Err(_) => Err(Refusal(INSPECTION).into()),
    }
}

fn unchanged(home: &Path, id: &str) -> Result<()> {
    ensure!(db::thread_path(home, id)?.is_none());
    let snapshot = db::source_snapshot(home, &[id.to_owned()])?;
    ensure!(
        snapshot
            .threads
            .iter()
            .all(|thread| thread.thread.is_none() && thread.goal.is_none())
    );
    // Reuse production header decoding, including native compressed rollouts.
    let catalog = crate::sessions_discovery::catalog(home)?;
    ensure!(!catalog.threads.contains_key(id) && catalog.unreadable.is_empty());
    Ok(())
}
fn probe(program: &Path, root: &Path) -> Result<()> {
    fs::write(root.join(".qb-canary-root"), [])?;
    fs::write(root.join("empty-git-config"), [])?;
    let source = root.join("source");
    let target = root.join("target");
    qb_core::ensure_private_dir_all(&source)?;
    qb_core::ensure_private_dir_all(&target)?;
    let source = source.canonicalize()?;
    let target = target.canonicalize()?;
    native(program, root, &source, &[], true)?;
    stage(db::check_layout(&source), INCOMPATIBLE)?;
    let threads = stage(db::threads(&source), INCOMPATIBLE)?;
    ensure!(threads.len() == 1, "native did not create one session");
    let session = &threads[0];
    ensure!(crate::sessions_discovery::valid_id(&session.id));
    guard::safe_path(&source, &session.rollout_path)?;
    let relative = session
        .rollout_path
        .canonicalize()?
        .strip_prefix(&source)?
        .to_owned();
    let before = contents(&session.rollout_path)?;
    let header: serde_json::Value = serde_json::from_slice(
        before
            .split(|byte| *byte == b'\n')
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty rollout"))?,
    )?;
    ensure!(header["type"] == "session_meta" && header["payload"]["id"] == session.id);
    let identity = guard::identity(&session.rollout_path)?;
    let ids = std::slice::from_ref(&session.id);
    // Target has the identical synthetic provider configuration, never a real config.
    fs::copy(source.join("config.toml"), target.join("config.toml"))?;
    let plan = stage(db::plan(&source, &target, ids), INCOMPATIBLE)?;
    let snapshot = db::source_snapshot(&source, ids)?;
    let stage_dir = target.join(".quota-baton/staging").join(&session.id);
    qb_core::ensure_private_dir_all(&stage_dir)?;
    let stage_path = stage_dir.join("private-stage.jsonl");
    fs::hard_link(&session.rollout_path, &stage_path)?;
    guard::unlink_counted(&source, &session.rollout_path, identity, 2)?;
    // Native must not use stale sqlite metadata to append the private alias.
    for operation in [
        vec!["resume", session.id.as_str()],
        vec!["resume", "--last"],
    ] {
        native(program, root, &source, &operation, false)?;
        stage(
            (|| {
                ensure!(contents(&stage_path)? == before);
                ensure!(db::source_snapshot(&source, ids)? == snapshot);
                Ok(())
            })(),
            INCOMPATIBLE,
        )?;
    }
    db::remove_exact(&snapshot)?;
    // The moved session has no public name in either home at this stage.
    for operation in [
        vec!["resume", session.id.as_str()],
        vec!["resume", "--last"],
    ] {
        native(program, root, &source, &operation, false)?;
        native(program, root, &target, &operation, false)?;
        stage(unchanged(&source, &session.id), INCOMPATIBLE)?;
        stage(unchanged(&target, &session.id), INCOMPATIBLE)?;
        stage(
            (|| {
                ensure!(contents(&stage_path)? == before);
                Ok(())
            })(),
            INCOMPATIBLE,
        )?;
    }
    let published = target.join(relative);
    fs::create_dir_all(
        published
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing parent"))?,
    )?;
    fs::hard_link(&stage_path, &published)?;
    db::apply(&plan)?;
    db::verify(&plan)?;
    native(program, root, &target, &["resume", &session.id], true)?;
    stage(db::check_layout(&target), INCOMPATIBLE)?;
    stage(
        (|| {
            ensure!(guard::identity(&published)? == identity);
            let resumed = contents(&published)?;
            ensure!(resumed.len() > before.len() && resumed.starts_with(&before));
            let actual = db::thread_path(&target, &session.id)?
                .ok_or_else(|| anyhow::anyhow!("missing resumed row"))?;
            ensure!(actual.canonicalize()? == published.canonicalize()?);
            ensure!(db::thread_provider(&target, &session.id)?.as_deref() == Some("qb"));
            Ok(())
        })(),
        INCOMPATIBLE,
    )?;
    let current = contents(&published)?;
    let target_rows = db::source_snapshot(&target, ids)?;
    for operation in [
        vec!["resume", session.id.as_str()],
        vec!["resume", "--last"],
        vec!["fork", session.id.as_str()],
    ] {
        native(program, root, &source, &operation, false)?;
        stage(
            (|| {
                unchanged(&source, &session.id)?;
                ensure!(contents(&published)? == current);
                ensure!(guard::identity(&published)? == identity);
                ensure!(db::source_snapshot(&target, ids)? == target_rows);
                Ok(())
            })(),
            INCOMPATIBLE,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn fixture() -> Result<tempfile::TempDir> {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/session-canary-tests");
        fs::create_dir_all(&base)?;
        Ok(qb_testkit::fixture::tempdir_in(base.canonicalize()?)?)
    }

    #[cfg(windows)]
    #[test]
    fn successful_stub_cannot_authorize_session_import() -> Result<()> {
        let home = fixture()?;
        let program = home.path().join("stub.cmd");
        fs::write(&program, b"@echo off\r\nexit /b 0\r\n")?;
        for _ in 0..2 {
            let error = check(&program, home.path(), "0.999.0").unwrap_err();
            assert_eq!(friendly_error(&error).as_deref(), Some(INSPECTION));
        }
        let base = home.path().join("state/session-canary");
        assert!(
            fs::read_dir(base)?
                .all(|entry| entry.is_ok_and(|entry| entry.file_name() == "probe.lock"))
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn cancellation_is_preserved_without_a_pass_receipt() -> Result<()> {
        let home = fixture()?;
        let program = home.path().join("stub.cmd");
        fs::write(&program, b"@echo off\r\nexit /b 0\r\n")?;
        let cancellation = crate::sessions_progress::test_cancellation(true);
        cancellation.request_cancel();
        let error = stage(crate::sessions_progress::check(), INCOMPATIBLE).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::sessions_progress::Interrupted>()
                .is_some()
        );
        let error = check(&program, home.path(), "0.999.0").unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::sessions_progress::Interrupted>()
                .is_some()
        );
        for entry in fs::read_dir(home.path().join("state/session-canary"))? {
            assert_eq!(entry?.file_name(), "probe.lock");
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "runs installed native Codex with a synthetic loopback provider"]
    fn installed_native_session_compatibility_and_cache() -> Result<()> {
        let home = fixture()?;
        let program = PathBuf::from(std::env::var_os("QB_SESSION_CANARY_PROGRAM").ok_or_else(
            || anyhow::anyhow!("set QB_SESSION_CANARY_PROGRAM to the native executable"),
        )?);
        let output = crate::launcher_label::version_output(&program)?;
        let version = std::str::from_utf8(&output)?
            .trim()
            .strip_prefix("codex-cli ")
            .ok_or_else(|| anyhow::anyhow!("unexpected native version output"))?;
        check(&program, home.path(), version)?;
        let fingerprint = crate::launcher_label::compatibility_fingerprint(&program);
        ensure!(
            fingerprint.is_some(),
            "native cache test requires a recognized launcher"
        );
        let cache = fingerprint.as_ref().map(|fingerprint| {
            home.path()
                .join("state/session-canary")
                .join(cache_key(version, fingerprint))
        });
        let first = cache
            .as_ref()
            .map(|path| fs::metadata(path).and_then(|metadata| metadata.modified()))
            .transpose()?;
        check(&program, home.path(), version)?;
        if let (Some(path), Some(first)) = (cache, first) {
            assert_eq!(
                fs::metadata(path)?.modified()?,
                first,
                "cache hit must avoid republishing"
            );
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "runs installed native Codex and deliberately mutates synthetic staging"]
    fn native_staging_append_is_rejected_without_pass_receipt() -> Result<()> {
        let home = fixture()?;
        let native = PathBuf::from(
            std::env::var_os("QB_SESSION_CANARY_PROGRAM")
                .ok_or_else(|| anyhow::anyhow!("set QB_SESSION_CANARY_PROGRAM"))?,
        );
        ensure!(native.is_absolute() && native.is_file());
        // CALL reparses batch arguments. Reject metacharacters in the only
        // interpolated paths; native arguments are the probe's fixed values.
        let batch_path = |path: &Path| -> Result<String> {
            let text = path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 test path"))?;
            let text = text.strip_prefix(r"\\?\").unwrap_or(text);
            ensure!(
                !text
                    .chars()
                    .any(|character| "%\r\n\"!^&|<>()".contains(character))
            );
            Ok(text.to_owned())
        };
        let native_text = batch_path(&native)?;
        let marker = home.path().join("mutation-observed.txt");
        let marker_text = batch_path(&marker)?;
        let probe_base = batch_path(&home.path().join("state/session-canary"))?;
        let wrapper = home.path().join("mutating-native.cmd");
        fs::write(
            &wrapper,
            format!(
                "@echo off\r\nsetlocal DisableDelayedExpansion\r\ncall \"{native_text}\" %*\r\nset \"QB_CANARY_EXIT=%ERRORLEVEL%\"\r\nfor /d %%D in (\"{probe_base}\\probe-*\") do (\r\n  for /d %%S in (\"%%D\\target\\.quota-baton\\staging\\*\") do (\r\n    if exist \"%%S\\private-stage.jsonl\" (\r\n      >>\"%%S\\private-stage.jsonl\" echo synthetic-incompatible-native-append\r\n      >>\"{marker_text}\" echo append-observed\r\n    )\r\n  )\r\n)\r\nexit /b %QB_CANARY_EXIT%\r\n"
            ),
        )?;
        let result = check(&wrapper, home.path(), "synthetic-native-mutation");
        assert!(
            marker.is_file(),
            "native must reach the planted staging append"
        );
        assert!(!fs::read(&marker)?.is_empty());
        let error = result.unwrap_err();
        assert_eq!(friendly_error(&error).as_deref(), Some(INCOMPATIBLE));
        for entry in fs::read_dir(home.path().join("state/session-canary"))? {
            assert_eq!(
                entry?.file_name(),
                "probe.lock",
                "failed probe retained a pass or scratch directory"
            );
        }
        Ok(())
    }

    #[test]
    fn receipt_requires_exact_probe_and_executable_identity() {
        let receipt = Receipt {
            revision: REVISION.into(),
            version: "0.166.0".into(),
            fingerprint: "native-bytes".into(),
            pass: true,
        };
        let bytes = serde_json::to_vec(&receipt).unwrap();
        assert!(receipt_matches(&bytes, "0.166.0", "native-bytes"));
        assert!(!receipt_matches(&bytes, "0.167.0", "native-bytes"));
        assert!(!receipt_matches(&bytes, "0.166.0", "changed-native"));
        assert!(!receipt_matches(
            b"{\"pass\":true}",
            "0.166.0",
            "native-bytes"
        ));
        assert!(!receipt_matches(
            &bytes[..bytes.len() - 1],
            "0.166.0",
            "native-bytes"
        ));
        assert_ne!(cache_key("0.166.0", "a"), cache_key("0.166.0", "b"));
    }
}
