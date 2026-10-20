//! Read-only import/resume guards for Codex's per-home writer protocol.
//!
//! See Codex `rollout/src/writer_lock.rs`: coordination precedes thread locks.
//! Keep `Guards` alive through publication/launch preparation. Dropping it closes
//! all handles; no source file is created, changed, unlocked by name, or removed.
//! These guards cannot protect a session after they are dropped, reserve absent
//! lock files, or make an unwrapped Codex launch in another home cooperate.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Windows volume serial and 64-bit file index, not a pathname or content hash.
/// Compare only while the relevant handles remain alive (file ids can be reused).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct FileIdentity {
    pub(crate) volume: u32,
    pub(crate) index: u64,
}

/// True for compressed input, an existing `.jsonl.zst` sibling, or an inspection
/// error. Fail closed: this bool must never turn an inaccessible sibling into an
/// apparently safe uncompressed rollout. Does not read conversation contents.
pub(crate) fn compressed_sibling(path: &Path) -> bool {
    if path
        .extension()
        .is_some_and(|ext| ext.as_encoded_bytes().eq_ignore_ascii_case(b"zst"))
    {
        return true;
    }
    let mut sibling = path.as_os_str().to_os_string();
    sibling.push(".zst");
    match std::fs::symlink_metadata(Path::new(&sibling)) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Safe to recognize by type at the CLI boundary; Debug never reveals home.
pub(crate) struct LiveWriter {
    home: PathBuf,
}

impl std::fmt::Debug for LiveWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LiveWriter")
    }
}

impl std::fmt::Display for LiveWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "이 대화는 다른 곳에서 실행 중입니다 ({})",
            self.home.display()
        )
    }
}

impl std::error::Error for LiveWriter {}

#[cfg(windows)]
mod platform {
    use super::*;
    use anyhow::{anyhow, bail};
    use std::{
        fs::{File, OpenOptions, TryLockError},
        io,
        os::windows::{
            ffi::{OsStrExt, OsStringExt},
            fs::OpenOptionsExt,
            io::AsRawHandle,
        },
        path::Component,
    };
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        },
    };

    // Never derive Debug: std::fs::File's Debug can expose paths.
    pub(crate) struct Guards {
        // Drop rollout/thread handles before coordination, ancestors last.
        _rollouts: Vec<File>,
        _threads: Vec<File>,
        _coordination: Vec<File>,
        _ancestors: Vec<File>,
    }

    fn inspection_error(error: io::Error) -> anyhow::Error {
        // Do not include arbitrary OS messages, filenames or file contents.
        anyhow!("세션 파일 검사 실패 (OS 코드 {:?})", error.raw_os_error())
    }

    fn live(home: &Path) -> anyhow::Error {
        anyhow!(LiveWriter {
            home: home.to_path_buf()
        })
    }

    fn information(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: File owns a live Windows handle; info is writable for this call.
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
            .map_err(|error| anyhow!("세션 파일 식별 실패 (OS 코드 {})", error.code().0))?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            bail!("세션 경로의 reparse 지점은 사용할 수 없습니다");
        }
        Ok(info)
    }

    fn file_identity(file: &File) -> Result<FileIdentity> {
        let info = information(file)?;
        Ok(FileIdentity {
            volume: info.dwVolumeSerialNumber,
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }

    fn options() -> OpenOptions {
        let mut options = OpenOptions::new();
        // Existing Codex lock files must not be replaced while we hold them.
        // Rollout/attribute callers override sharing to permit hardlink deletion.
        options
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0);
        options
    }

    fn absolute(path: &Path) -> Result<PathBuf> {
        // canonicalize returns \\?\C:\... for local paths. Strip only that
        // namespace marker; validate both spellings before any filesystem call.
        // Preserve native UTF-16 rather than converting the name lossily.
        let normalized = match path.components().next() {
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_)) =>
            {
                PathBuf::from(std::ffi::OsString::from_wide(
                    &path.as_os_str().encode_wide().skip(4).collect::<Vec<_>>(),
                ))
            }
            _ => path.to_path_buf(),
        };
        // Verbatim paths treat forward slashes differently. Check both forms
        // so normalization cannot hide parent traversal or trailing-name aliases.
        if path
            .components()
            .chain(normalized.components())
            .any(|c| matches!(c, Component::ParentDir))
        {
            bail!("세션 경로에 상위 디렉터리 이동을 사용할 수 없습니다");
        }
        for component in path.components().chain(normalized.components()) {
            if let Component::Normal(name) = component {
                let bytes = name.as_encoded_bytes();
                if bytes.contains(&b':') || matches!(bytes.last(), Some(b'.' | b' ')) {
                    bail!("세션 경로에 대체 스트림이나 모호한 이름을 사용할 수 없습니다");
                }
            }
        }
        let absolute = std::path::absolute(&normalized).map_err(inspection_error)?;
        match absolute.components().next() {
            // Refuse device namespaces, UNC shares and other verbatim aliases. This
            // product operates on local homes; do not widen that trust boundary.
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::Disk(_)) => {}
            _ => bail!("세션 경로는 로컬 드라이브 경로여야 합니다"),
        }
        Ok(absolute)
    }

    /// Walk from the volume root, opening every component without following a
    /// reparse point. Retaining the handles closes the check/open rename race for
    /// existing ancestors. Missing trailing components are allowed, never made.
    fn pin_path(root: &Path, path: &Path) -> Result<(Vec<File>, bool)> {
        let root = absolute(root)?;
        let path = absolute(path)?;
        if !path.starts_with(&root) {
            bail!("세션 경로가 지정한 홈 밖에 있습니다");
        }
        let mut pins = Vec::new();
        let mut current = PathBuf::new();
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            current.push(component.as_os_str());
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            let file = match options()
                .access_mode(FILE_READ_ATTRIBUTES.0)
                .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
                .open(&current)
            {
                Ok(file) => file,
                // Do not subsequently open through this unchecked missing
                // suffix: it could be created as a reparse ancestor meanwhile.
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((pins, false)),
                Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => {
                    return Err(live(&root));
                }
                Err(error) => return Err(inspection_error(error)),
            };
            let info = information(&file)?;
            if components.peek().is_some()
                && info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            {
                bail!("세션 경로의 조상이 디렉터리가 아닙니다");
            }
            if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                // Deny both rename/delete and in-place reparse tagging of each
                // ancestor. Do not retain attribute handles to regular files:
                // rollout names must remain deletable while their probe is held.
                let pinned = options()
                    .access_mode(FILE_READ_ATTRIBUTES.0)
                    .share_mode(FILE_SHARE_READ.0)
                    .open(&current)
                    .map_err(|error| {
                        if matches!(error.raw_os_error(), Some(32 | 33)) {
                            live(&root)
                        } else {
                            inspection_error(error)
                        }
                    })?;
                let info = information(&pinned)?;
                if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
                    bail!("세션 경로의 조상이 변경되었습니다");
                }
                pins.push(pinned);
            }
        }
        Ok((pins, true))
    }

    /// Lexical containment and no-reparse validation, including root's ancestors.
    /// Relative paths are relative to cwd, not root. Missing suffixes are allowed.
    /// A standalone check is not a reservation; probe_all retains its own pins.
    pub(crate) fn safe_path(root: &Path, path: &Path) -> Result<()> {
        pin_path(root, path).map(drop)
    }

    pub(crate) fn identity(path: &Path) -> Result<FileIdentity> {
        let absolute = absolute(path)?;
        let (_pins, exists) = pin_path(&absolute, &absolute)?;
        if !exists {
            bail!("세션 파일이 없습니다");
        }
        let file = options()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .open(&absolute)
            .map_err(inspection_error)?;
        let info = information(&file)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            bail!("rollout은 일반 파일이어야 합니다");
        }
        file_identity(&file)
    }

    struct PendingLock {
        identity: FileIdentity,
        file: File,
        home: PathBuf,
    }

    fn pending(home: &Path, path: &Path, pins: &mut Vec<File>) -> Result<Option<PendingLock>> {
        let (ancestors, exists) = pin_path(home, path)?;
        pins.extend(ancestors);
        if !exists {
            return Ok(None);
        }
        let file = match options().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => return Err(live(home)),
            Err(error) => return Err(inspection_error(error)),
        };
        let info = information(&file)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            bail!("Codex 잠금 경로가 파일이 아닙니다");
        }
        Ok(Some(PendingLock {
            identity: file_identity(&file)?,
            file,
            home: home.to_path_buf(),
        }))
    }

    fn take_locks(mut pending: Vec<PendingLock>) -> Result<Vec<File>> {
        // Numeric, alias-independent order. Dedup hardlinked/shared lock names so
        // we never contend with ourselves, even with repeated homes or ids.
        pending.sort_by_key(|lock| lock.identity);
        pending.dedup_by_key(|lock| lock.identity);
        let mut held = Vec::new();
        for lock in pending {
            match lock.file.try_lock() {
                Ok(()) => held.push(lock.file),
                Err(TryLockError::WouldBlock) => return Err(live(&lock.home)),
                Err(TryLockError::Error(error)) => return Err(inspection_error(error)),
            }
        }
        Ok(held)
    }

    /// Probe the Cartesian product of homes and ids; caller supplies every home
    /// containing these threads and every paginated/hardlinked rollout name.
    /// Acquire all existing coordination locks before inspecting thread locks.
    /// Missing lock files are skipped without creation (including dry runs).
    /// Rollout read handles deny write sharing through hardlinks, but allow
    /// delete sharing for link/unimport. Names can therefore be replaced: the
    /// planner must recheck identity and compression immediately before mutation.
    pub(crate) fn probe_all(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe(homes, ids, rollouts, false)
    }

    /// Unlink-only recovery: retain all writer/path/share checks, but leave any
    /// compressed sibling untouched. Inputs must still be plain rollout names.
    /// Caller must verify identities/ownership before deleting journaled links.
    pub(crate) fn probe_cleanup(
        homes: &[PathBuf],
        ids: &[String],
        plain_rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe(homes, ids, plain_rollouts, true)
    }

    fn probe(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
        allow_compressed_sibling: bool,
    ) -> Result<Guards> {
        if ids.iter().any(|id| {
            id.is_empty()
                || id.len() > 128
                || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        }) {
            bail!("잘못된 세션 식별자입니다");
        }
        let mut guards = Guards {
            _rollouts: Vec::new(),
            _threads: Vec::new(),
            _coordination: Vec::new(),
            _ancestors: Vec::new(),
        };
        let homes = homes
            .iter()
            .map(|home| absolute(home))
            .collect::<Result<Vec<_>>>()?;
        let mut locks = Vec::new();
        for home in &homes {
            let path = home.join("thread-writer-locks/.coordination.lock");
            if let Some(lock) = pending(home, &path, &mut guards._ancestors)? {
                locks.push(lock);
            }
        }
        guards._coordination = take_locks(locks)?;
        let mut locks = Vec::new();
        for home in &homes {
            for id in ids {
                let path = home.join("thread-writer-locks").join(format!("{id}.lock"));
                if let Some(lock) = pending(home, &path, &mut guards._ancestors)? {
                    locks.push(lock);
                }
            }
        }
        guards._threads = take_locks(locks)?;
        for path in rollouts {
            let path = absolute(path)?;
            let home = homes
                .iter()
                .filter(|home| path.starts_with(home))
                .max_by_key(|home| home.components().count())
                .ok_or_else(|| anyhow!("rollout 경로가 검사 대상 홈 밖에 있습니다"))?;
            let (ancestors, exists) = pin_path(home, &path)?;
            guards._ancestors.extend(ancestors);
            if !exists {
                bail!("rollout 파일이 없습니다");
            }
            let compressed_input = path
                .extension()
                .is_some_and(|ext| ext.as_encoded_bytes().eq_ignore_ascii_case(b"zst"));
            if compressed_input || (!allow_compressed_sibling && compressed_sibling(&path)) {
                bail!(
                    "압축된 rollout 또는 .zst 형제가 있습니다. sessions unimport 후 다시 가져오세요"
                );
            }
            let file = options()
                .read(true)
                .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_DELETE.0)
                .open(&path)
                .map_err(|error| {
                    if matches!(error.raw_os_error(), Some(32 | 33)) {
                        live(home)
                    } else {
                        inspection_error(error)
                    }
                })?;
            let info = information(&file)?;
            if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                bail!("rollout은 일반 파일이어야 합니다");
            }
            guards._rollouts.push(file);
        }
        Ok(guards)
    }
}

#[cfg(windows)]
pub(crate) use platform::{identity, probe_all, probe_cleanup, safe_path};

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub(crate) struct Guards;
    pub(crate) fn safe_path(_: &Path, _: &Path) -> Result<()> {
        anyhow::bail!("세션 가져오기 경로 검사는 Windows에서만 지원됩니다")
    }
    pub(crate) fn identity(_: &Path) -> Result<FileIdentity> {
        anyhow::bail!("세션 파일 식별은 Windows에서만 지원됩니다")
    }
    pub(crate) fn probe_all(_: &[PathBuf], _: &[String], _: &[PathBuf]) -> Result<Guards> {
        anyhow::bail!("세션 잠금 검사는 Windows에서만 지원됩니다")
    }
    pub(crate) fn probe_cleanup(
        homes: &[PathBuf],
        ids: &[String],
        plain_rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe_all(homes, ids, plain_rollouts)
    }
}
#[cfg(not(windows))]
pub(crate) use platform::{identity, probe_all, probe_cleanup, safe_path};

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{
        fs,
        fs::OpenOptions,
        os::windows::ffi::{OsStrExt, OsStringExt},
        process::Command,
    };

    const ID: &str = "00000000-0000-0000-0000-000000000001";

    fn fixture() -> tempfile::TempDir {
        // Never use system Temp, real homes, credentials or an upstream process.
        let scratch =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/session-guard-tests");
        fs::create_dir_all(&scratch).unwrap();
        let scratch = fs::canonicalize(scratch).unwrap();
        tempfile::tempdir_in(scratch).unwrap()
    }

    fn home(root: &Path, name: &str) -> PathBuf {
        let home = root.join(name);
        fs::create_dir_all(home.join("sessions")).unwrap();
        home
    }

    fn lock_path(home: &Path, name: &str) -> PathBuf {
        let dir = home.join("thread-writer-locks");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, []).unwrap();
        path
    }

    fn assert_live(result: Result<platform::Guards>, home: &Path) {
        let error = match result {
            Ok(_) => panic!("writer must be refused"),
            Err(error) => error,
        };
        let live_writer = error
            .downcast_ref::<LiveWriter>()
            .expect("typed live writer error");
        assert_eq!(format!("{live_writer:?}"), "LiveWriter");
        let expected_home = match home.components().next() {
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_)) =>
            {
                PathBuf::from(std::ffi::OsString::from_wide(
                    &home.as_os_str().encode_wide().skip(4).collect::<Vec<_>>(),
                ))
            }
            _ => home.to_path_buf(),
        };
        assert_eq!(
            error.to_string(),
            format!(
                "이 대화는 다른 곳에서 실행 중입니다 ({})",
                expected_home.display()
            )
        );
    }

    #[test]
    fn held_coordination_and_thread_locks_refuse() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        for name in [".coordination.lock".to_owned(), format!("{ID}.lock")] {
            let path = lock_path(&home, &name);
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            lock.try_lock().unwrap();
            assert_live(
                probe_all(std::slice::from_ref(&home), &[ID.into()], &[]),
                &home,
            );
            assert_live(
                probe_cleanup(std::slice::from_ref(&home), &[ID.into()], &[]),
                &home,
            );
            drop(lock);
            assert!(probe_all(std::slice::from_ref(&home), &[ID.into()], &[]).is_ok());
        }
    }

    #[test]
    fn writable_hardlink_refuses_and_unlocked_names_pass() {
        let temp = fixture();
        let a = home(temp.path(), "a");
        let b = home(temp.path(), "b");
        let source = a.join("sessions/test.jsonl");
        let link = b.join("sessions/test.jsonl");
        fs::write(&source, b"synthetic\n").unwrap();
        fs::hard_link(&source, &link).unwrap();
        let homes = [a.clone(), b.clone()];
        let writer = OpenOptions::new().append(true).open(&source).unwrap();
        assert_live(
            probe_all(&homes, &[ID.into()], std::slice::from_ref(&link)),
            &b,
        );
        drop(writer);
        let guards = probe_all(&homes, &[ID.into()], &[source.clone(), link.clone()]).unwrap();
        assert!(OpenOptions::new().append(true).open(&source).is_err());
        assert!(OpenOptions::new().append(true).open(&link).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"synthetic\n");
        drop(guards);
        assert!(OpenOptions::new().append(true).open(&source).is_ok());
        assert!(!a.join("thread-writer-locks").exists());
        assert!(!b.join("thread-writer-locks").exists());
    }

    #[test]
    fn identity_equal_for_links_different_after_replacement() {
        let temp = fixture();
        let source = temp.path().join("source.jsonl");
        let link = temp.path().join("link.jsonl");
        fs::write(&source, []).unwrap();
        fs::hard_link(&source, &link).unwrap();
        let before = identity(&source).unwrap();
        assert_eq!(before, identity(&link).unwrap());
        assert_eq!(
            before,
            serde_json::from_str::<FileIdentity>(&serde_json::to_string(&before).unwrap()).unwrap()
        );
        fs::remove_file(&link).unwrap();
        fs::write(&link, []).unwrap();
        assert_ne!(before, identity(&link).unwrap());
    }

    #[test]
    fn canonical_and_ordinary_local_paths_are_equivalent() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let rollout = home.join("sessions/test.jsonl");
        fs::write(&rollout, b"synthetic\n").unwrap();
        lock_path(&home, ".coordination.lock");
        lock_path(&home, &format!("{ID}.lock"));
        let canonical_home = fs::canonicalize(&home).unwrap();
        let canonical_rollout = fs::canonicalize(&rollout).unwrap();
        assert!(
            matches!(canonical_home.components().next(), Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_)))
        );
        let ordinary_home = PathBuf::from(std::ffi::OsString::from_wide(
            &canonical_home
                .as_os_str()
                .encode_wide()
                .skip(4)
                .collect::<Vec<_>>(),
        ));
        let ordinary_rollout = ordinary_home.join("sessions/test.jsonl");
        assert_eq!(
            identity(&canonical_rollout).unwrap(),
            identity(&ordinary_rollout).unwrap()
        );
        for root in [&canonical_home, &ordinary_home] {
            for path in [&canonical_rollout, &ordinary_rollout] {
                safe_path(root, path).unwrap();
                let guards = probe_all(
                    std::slice::from_ref(root),
                    &[ID.into()],
                    std::slice::from_ref(path),
                )
                .unwrap();
                assert!(
                    OpenOptions::new()
                        .append(true)
                        .open(&ordinary_rollout)
                        .is_err()
                );
                drop(guards);
            }
        }
        assert!(
            safe_path(
                &canonical_home,
                &canonical_home.join("sessions/test.jsonl:stream")
            )
            .is_err()
        );
        assert!(safe_path(&canonical_home, &canonical_home.join("sessions/trailing. ")).is_err());
        for rejected in [
            r"\\server\share\file.jsonl",
            r"\\?\UNC\server\share\file.jsonl",
            r"\\.\C:\file.jsonl",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\file.jsonl",
        ] {
            assert!(safe_path(&ordinary_home, Path::new(rejected)).is_err());
        }
    }

    #[test]
    fn link_creation_and_removal_work_while_probe_is_held() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let source = home.join("sessions/source.jsonl");
        let link = home.join("sessions/link.jsonl");
        fs::write(&source, b"synthetic\n").unwrap();
        let guards = probe_all(
            std::slice::from_ref(&home),
            &[ID.into()],
            std::slice::from_ref(&source),
        )
        .unwrap();
        fs::hard_link(&source, &link).unwrap();
        assert_eq!(identity(&source).unwrap(), identity(&link).unwrap());
        fs::remove_file(&link).unwrap();
        assert!(!link.exists());
        assert!(OpenOptions::new().append(true).open(&source).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"synthetic\n");
        drop(guards);
    }

    #[test]
    fn missing_home_and_lock_paths_are_not_created() {
        let temp = fixture();
        let missing = temp.path().join("missing");
        let guards = probe_all(std::slice::from_ref(&missing), &[ID.into()], &[]).unwrap();
        assert!(!missing.exists());
        drop(guards);
        assert!(!missing.exists());
    }

    #[test]
    fn repeated_homes_ids_and_hardlinked_locks_do_not_self_contend() {
        let temp = fixture();
        let a = home(temp.path(), "a");
        let b = home(temp.path(), "b");
        for name in [".coordination.lock".to_owned(), format!("{ID}.lock")] {
            let source = lock_path(&a, &name);
            fs::create_dir_all(b.join("thread-writer-locks")).unwrap();
            fs::hard_link(source, b.join("thread-writer-locks").join(name)).unwrap();
        }
        assert!(probe_all(&[b, a.clone(), a], &[ID.into(), ID.into()], &[]).is_ok());
    }

    #[test]
    fn escape_and_reparse_ancestors_refuse() {
        let temp = fixture();
        let root = home(temp.path(), "root");
        let outside = home(temp.path(), "outside");
        assert!(safe_path(&root, &outside).is_err());
        assert!(safe_path(&root, &root.join("../outside")).is_err());
        assert!(safe_path(&root, &root.join("sessions/file.jsonl:stream")).is_err());
        assert!(safe_path(&root, &root.join("sessions. /file.jsonl")).is_err());
        assert!(probe_all(std::slice::from_ref(&root), &["../escape".into()], &[]).is_err());
        assert!(probe_cleanup(std::slice::from_ref(&root), &["../escape".into()], &[]).is_err());
        let alias = root.join("alias");
        // Local NTFS junctions need no symlink privilege. Both arguments are
        // fixed-name children of this project-local synthetic fixture. mklink
        // expects ordinary local paths, not canonicalize's verbatim namespace.
        let ordinary = |path: &Path| {
            let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
            PathBuf::from(std::ffi::OsString::from_wide(&units[4..]))
        };
        let junction = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(ordinary(&alias))
            .arg(ordinary(&outside))
            .output()
            .expect("create synthetic junction");
        assert!(
            junction.status.success(),
            "junction creation failed: {:?}",
            junction.status.code()
        );
        assert!(safe_path(&root, &alias.join("missing.jsonl")).is_err());
        assert!(safe_path(&alias, &alias.join("missing.jsonl")).is_err());
        assert!(identity(&alias.join("missing.jsonl")).is_err());
        assert!(probe_cleanup(std::slice::from_ref(&alias), &[ID.into()], &[]).is_err());
        assert!(probe_all(&[alias], &[ID.into()], &[]).is_err());
    }

    #[test]
    fn cleanup_allows_compressed_sibling_but_refuses_compressed_input_and_writer() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let rollout = home.join("sessions/test.jsonl");
        let link = home.join("sessions/link.jsonl");
        let compressed = home.join("sessions/test.jsonl.zst");
        fs::write(&rollout, b"synthetic\n").unwrap();
        fs::write(&compressed, b"synthetic compressed sibling").unwrap();
        fs::hard_link(&rollout, &link).unwrap();
        let before = identity(&rollout).unwrap();
        assert!(
            probe_all(
                std::slice::from_ref(&home),
                &[ID.into()],
                std::slice::from_ref(&rollout)
            )
            .is_err()
        );
        let guards = probe_cleanup(
            std::slice::from_ref(&home),
            &[ID.into()],
            std::slice::from_ref(&rollout),
        )
        .unwrap();
        assert!(OpenOptions::new().append(true).open(&link).is_err());
        fs::remove_file(&link).unwrap();
        assert_eq!(
            fs::read(&compressed).unwrap(),
            b"synthetic compressed sibling"
        );
        assert_eq!(identity(&rollout).unwrap(), before);
        drop(guards);
        assert!(
            probe_cleanup(
                std::slice::from_ref(&home),
                &[ID.into()],
                std::slice::from_ref(&compressed)
            )
            .is_err()
        );
        fs::hard_link(&rollout, &link).unwrap();
        let writer = OpenOptions::new().append(true).open(&link).unwrap();
        assert_live(
            probe_cleanup(
                std::slice::from_ref(&home),
                &[ID.into()],
                std::slice::from_ref(&rollout),
            ),
            &home,
        );
        drop(writer);
        assert_eq!(fs::read(&rollout).unwrap(), b"synthetic\n");
        assert_eq!(
            fs::read(&compressed).unwrap(),
            b"synthetic compressed sibling"
        );
    }

    #[test]
    fn compressed_input_and_sibling_refuse() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let rollout = home.join("sessions/test.jsonl");
        fs::write(&rollout, []).unwrap();
        assert!(!compressed_sibling(&rollout));
        let compressed = home.join("sessions/test.jsonl.zst");
        fs::write(&compressed, []).unwrap();
        assert!(compressed_sibling(&rollout));
        assert!(compressed_sibling(&compressed));
        assert!(probe_all(&[home], &[ID.into()], &[rollout]).is_err());
    }
}
