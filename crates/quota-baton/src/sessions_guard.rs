//! Import/resume guards and identity-bound cleanup for Codex's writer protocol.
//!
//! See Codex `rollout/src/writer_lock.rs`: coordination precedes thread locks.
//! Keep `Guards` alive through publication/launch preparation. Dropping it closes
//! all guard handles. Read-only probes never create files. Mutating reservations
//! create Codex locks and temporary empty anchors, never change rollout bytes.
//! Read-only probes cannot reserve absent lock files. No guard protects a session
//! after drop or makes an unwrapped Codex launch in another home cooperate.

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

pub(crate) use platform::PublicationGuard;

/// Mutating namespace guard. Keep alive around directory preparation and the
/// subsequent hardlink/rename. Read-only validation must use `safe_path` instead.
/// Missing directories within root persist; only the marker is removed on drop.
/// NTFS refuses directory reparse points on nonempty directories. A retained
/// nondeletable child pin or marker maintains that condition while allowing
/// ordinary publication. See Microsoft's FileIO "Reparse point restrictions".
/// https://learn.microsoft.com/en-us/windows/win32/fileio/reparse-points
pub(crate) fn prepare_parent(root: &Path, parent: &Path) -> Result<PublicationGuard> {
    platform::prepare_parent(root, parent)
}

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
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
            FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
            FILE_DISPOSITION_INFO_EX_FLAGS, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_DELETE_ON_CLOSE,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
            FILE_READ_DATA, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_NONE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FileDispositionInfoEx, FileRenameInfoEx, GetFileInformationByHandle,
            SetFileInformationByHandle,
        },
    };

    // Never derive Debug: std::fs::File's Debug can expose paths.
    pub(crate) struct Guards {
        // Drop rollout/thread handles before coordination, ancestors last.
        _rollouts: Vec<File>,
        _threads: Vec<File>,
        _coordination: Vec<File>,
        _publications: Vec<PublicationGuard>,
        _ancestors: Vec<File>,
    }

    impl Guards {
        /// Retain thread/coordination locks and directory pins while allowing
        /// unlink_validated to acquire SHARE_NONE and recheck the same object.
        pub(crate) fn release_rollouts_for_unlink(&mut self) {
            self._rollouts.clear();
        }
    }

    // No Debug: owned handles can reveal paths. Drop the marker before pins.
    pub(crate) struct PublicationGuard {
        _anchor: File,
        _directories: Vec<File>,
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

    fn directory_open(path: &Path, sharing: u32) -> io::Result<File> {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let result = options()
                .access_mode(FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0)
                .share_mode(sharing)
                .open(path);
            match result {
                Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        return Err(error);
                    };
                    // Strict ancestor opens can briefly conflict with unrelated
                    // child-name operations. Retry directory pins only; never
                    // retry rollout writer checks or Codex lock acquisition.
                    std::thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                result => return result,
            }
        }
    }

    fn directory(root: &Path, path: &Path, sharing: u32) -> Result<File> {
        let file = directory_open(path, sharing).map_err(|error| {
            if matches!(error.raw_os_error(), Some(32 | 33)) {
                live(root)
            } else {
                inspection_error(error)
            }
        })?;
        if information(&file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            bail!("대상 경로는 폴더여야 합니다");
        }
        Ok(file)
    }

    pub(crate) fn prepare_parent(root: &Path, parent: &Path) -> Result<PublicationGuard> {
        anchored_parent(root, parent, true)
    }

    fn anchored_parent(root: &Path, parent: &Path, create: bool) -> Result<PublicationGuard> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = absolute(root)?;
        let parent = absolute(parent)?;
        if !parent.starts_with(&root) {
            bail!("세션 경로가 지정한 홈 밖에 있습니다");
        }
        if !root.components().any(|c| matches!(c, Component::Normal(_))) {
            bail!("홈 루트는 드라이브 루트일 수 없습니다");
        }
        let mut pins = Vec::new();
        let mut strict: Option<(PathBuf, File)> = None;
        let mut current = PathBuf::new();
        for component in parent.components() {
            current.push(component.as_os_str());
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            // The previous directory is still strict while creating/opening its
            // child. Never follow an unchecked missing suffix with create_dir_all.
            let child = match directory_open(&current, FILE_SHARE_READ.0) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !create || !current.starts_with(&root) {
                        bail!("대상 폴더가 없습니다");
                    }
                    match std::fs::create_dir(&current) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(inspection_error(error)),
                    }
                    directory(&root, &current, FILE_SHARE_READ.0)?
                }
                Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => {
                    return Err(live(&root));
                }
                Err(error) => return Err(inspection_error(error)),
            };
            if information(&child)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
                bail!("대상 경로는 폴더여야 합니다");
            }
            if let Some((previous, held)) = strict.take() {
                // The child now denies DELETE, keeping its parent nonempty.
                // Only then allow parent WRITE sharing for child publication.
                pins.push(directory(
                    &root,
                    &previous,
                    FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0,
                )?);
                drop(held);
            }
            strict = Some((current.clone(), child));
        }
        let (_, held) = strict.ok_or_else(|| anyhow!("대상 폴더가 없습니다"))?;
        let mut anchor = None;
        for _ in 0..64 {
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let marker = parent.join(format!(
                ".quota-baton-parent-pin-{}-{serial}",
                std::process::id()
            ));
            // Only the final directory, inside root, receives an empty marker.
            // It cannot be deleted/renamed while held, even through an alias.
            match options()
                .write(true) // permit CREATE_NEW; access_mode overrides access rights
                .create_new(true)
                .access_mode(FILE_READ_DATA.0 | DELETE.0 | FILE_READ_ATTRIBUTES.0)
                .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_DELETE_ON_CLOSE.0)
                .open(marker)
            {
                Ok(file) => {
                    anchor = Some(file);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(inspection_error(error)),
            }
        }
        let anchor = anchor.ok_or_else(|| anyhow!("세션 경로 보호용 파일을 만들 수 없습니다"))?;
        pins.push(directory(
            &root,
            &parent,
            FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0,
        )?);
        // NTFS rejects attaching a directory reparse point to a nonempty
        // directory. The anchor replaces strict WRITE denial without making
        // hardlink/rename publication fail. Ancestors have pinned child dirs.
        drop(held);
        Ok(PublicationGuard {
            _anchor: anchor,
            _directories: pins,
        })
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
                // Deny rename/delete of the directory itself. WRITE sharing is
                // necessary for child-name publication; this pin does not deny
                // in-place reparse tagging. Every traversal still checks tags.
                // Do not retain handles to regular rollout names here.
                // Attribute-only handles do not participate in share exclusion;
                // FILE_LIST_DIRECTORY gives the pin effective read access.
                let pinned = directory(&root, &current, FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)?;
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

    /// Compare an existing regular file with an existing destination directory
    /// by handle volume serials, without creating a journal, directory or link.
    /// Reject reparse points/ancestors and retain both handles/pins through the
    /// comparison. This is admission only, not a reservation for a later link.
    pub(crate) fn same_volume(file: &Path, existing_dir: &Path) -> Result<bool> {
        let file = absolute(file)?;
        let directory = absolute(existing_dir)?;
        let (_file_pins, file_exists) = pin_path(&file, &file)?;
        if !file_exists {
            bail!("세션 파일이 없습니다");
        }
        let (_directory_pins, directory_exists) = pin_path(&directory, &directory)?;
        if !directory_exists {
            bail!("대상 폴더가 없습니다");
        }
        let source = options()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .open(&file)
            .map_err(inspection_error)?;
        let target = options()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .open(&directory)
            .map_err(inspection_error)?;
        let source_info = information(&source)?;
        let target_info = information(&target)?;
        if source_info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            bail!("rollout은 일반 파일이어야 합니다");
        }
        if target_info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            bail!("대상 경로는 폴더여야 합니다");
        }
        Ok(source_info.dwVolumeSerialNumber == target_info.dwVolumeSerialNumber)
    }

    /// Snapshot of the handle's hardlink count, not a directory/name estimate.
    /// Callers compare against their complete inventory; this does not reserve
    /// the count. `unlink_validated` checks the never-last condition under exclusive access.
    pub(crate) fn hardlink_count(path: &Path) -> Result<u32> {
        let path = absolute(path)?;
        let (_pins, exists) = pin_path(&path, &path)?;
        if !exists {
            bail!("세션 파일이 없습니다");
        }
        let file = options()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            .open(&path)
            .map_err(inspection_error)?;
        let info = information(&file)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            bail!("rollout은 일반 파일이어야 합니다");
        }
        Ok(info.nNumberOfLinks)
    }

    /// Best-effort admission check of the pinned Codex version's known DBs.
    /// A daemon/picker's existing DB read/write handle causes LiveWriter refusal.
    /// Missing files are normal. No SQL, contents, locks, or directories are
    /// created; every exclusive inspection handle is closed before returning.
    /// This is not a race fence: another process can open a DB after this check.
    #[cfg(test)]
    pub(crate) fn check_home_idle(home: &Path) -> Result<()> {
        let home = absolute(home)?;
        for name in ["state_5.sqlite", "goals_1.sqlite"] {
            let path = home.join(name);
            let (_pins, exists) = pin_path(&home, &path)?;
            if !exists {
                continue;
            }
            let file = options()
                .access_mode(FILE_READ_ATTRIBUTES.0 | FILE_READ_DATA.0)
                .share_mode(FILE_SHARE_NONE.0)
                .open(&path)
                .map_err(|error| {
                    if matches!(error.raw_os_error(), Some(32 | 33)) {
                        live(&home)
                    } else {
                        inspection_error(error)
                    }
                })?;
            if information(&file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                bail!("Codex 데이터베이스 경로가 파일이 아닙니다");
            }
        }
        Ok(())
    }

    // Deliberately no Debug: the file handle and directory pins are private.
    // Parent visibility permits a deterministic test of replacement after open.
    pub(super) struct VerifiedRemoval {
        file: File,
        _pins: PublicationGuard,
    }

    impl VerifiedRemoval {
        pub(super) fn remove(self) -> Result<()> {
            let disposition = FILE_DISPOSITION_INFO_EX {
                Flags: FILE_DISPOSITION_INFO_EX_FLAGS(
                    FILE_DISPOSITION_FLAG_DELETE.0 | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0,
                ),
            };
            // SAFETY: self owns a live, identity-checked file handle with DELETE
            // access. The initialized fixed-size buffer lives through this call.
            // POSIX unlink removes this handle's link while probe/read handles
            // stay open. It never resolves the caller's pathname a second time.
            unsafe {
                SetFileInformationByHandle(
                    HANDLE(self.file.as_raw_handle()),
                    FileDispositionInfoEx,
                    std::ptr::from_ref(&disposition).cast(),
                    std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
                )
            }
            .map_err(|error| anyhow!("검증한 세션 파일 삭제 실패 (OS 코드 {})", error.code().0))?;
            Ok(())
        }

        pub(super) fn rename(self, root_to: &Path, target: &Path) -> Result<()> {
            let root_to = absolute(root_to)?;
            let target = absolute(target)?;
            let parent = target
                .parent()
                .ok_or_else(|| anyhow!("대상 폴더가 없습니다"))?;
            let _parent_pins = anchored_parent(&root_to, parent, false)?;
            let (_target_pins, _) = pin_path(&root_to, &target)?;
            let name = target.as_os_str().encode_wide().collect::<Vec<_>>();
            let name_bytes = name
                .len()
                .checked_mul(2)
                .ok_or_else(|| anyhow!("대상 경로가 너무 깁니다"))?;
            let length = u32::try_from(name_bytes)?;
            let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
            let size = offset
                .checked_add(name_bytes)
                .and_then(|size| size.checked_add(2))
                .ok_or_else(|| anyhow!("대상 경로가 너무 깁니다"))?;
            let buffer_size = u32::try_from(size)?;
            // FILE_RENAME_INFO ends in a flexible UTF-16 array. Allocate an
            // aligned, zeroed buffer large enough for the complete destination.
            let mut buffer = vec![
                std::mem::MaybeUninit::<FILE_RENAME_INFO>::zeroed();
                size.div_ceil(std::mem::size_of::<FILE_RENAME_INFO>())
            ];
            let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            // SAFETY: buffer has FILE_RENAME_INFO alignment and enough bytes
            // for all fields and the UTF-16 name. self and both sets of directory
            // pins live through the call. No source pathname is re-resolved.
            unsafe {
                (*info).Anonymous.Flags = 0; // NO REPLACE: collision is an atomic failure.
                (*info).RootDirectory = HANDLE::default();
                (*info).FileNameLength = length;
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    info.cast::<u8>().add(offset).cast::<u16>(),
                    name.len(),
                );
                SetFileInformationByHandle(
                    HANDLE(self.file.as_raw_handle()),
                    FileRenameInfoEx,
                    info.cast(),
                    buffer_size,
                )
            }
            .map_err(|error| anyhow!("검증한 세션 파일 이동 실패 (OS 코드 {})", error.code().0))?;
            Ok(())
        }
    }

    pub(super) fn verified_removal(
        root: &Path,
        path: &Path,
        expected: FileIdentity,
    ) -> Result<VerifiedRemoval> {
        verified_handle(root, path, expected, false, None)
    }

    fn verified_handle(
        root: &Path,
        path: &Path,
        expected: FileIdentity,
        exclusive: bool,
        expected_links: Option<u32>,
    ) -> Result<VerifiedRemoval> {
        let root = absolute(root)?;
        let path = absolute(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("대상 폴더가 없습니다"))?;
        let pins = anchored_parent(&root, parent, false)?;
        let file = options()
            .access_mode(
                DELETE.0 | FILE_READ_ATTRIBUTES.0 | if exclusive { FILE_READ_DATA.0 } else { 0 },
            )
            .share_mode(if exclusive {
                FILE_SHARE_NONE.0 // Readers/writers/deleters through every alias conflict.
            } else {
                FILE_SHARE_READ.0 | FILE_SHARE_DELETE.0
            })
            .open(&path)
            .map_err(|error| {
                if matches!(error.raw_os_error(), Some(32 | 33)) {
                    live(&root)
                } else {
                    inspection_error(error)
                }
            })?;
        let info = information(&file)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            bail!("rollout은 일반 파일이어야 합니다");
        }
        if file_identity(&file)? != expected {
            bail!("대상 파일 소유권이 달라 되돌릴 수 없습니다");
        }
        if exclusive && info.nNumberOfLinks <= 1 {
            bail!("세션 파일의 마지막 하드링크는 삭제할 수 없습니다");
        }
        if expected_links.is_some_and(|count| count != info.nNumberOfLinks) {
            bail!("세션 파일의 하드링크 수가 변경되어 작업을 중단했습니다");
        }
        Ok(VerifiedRemoval { file, _pins: pins })
    }

    /// Panel 2 unlink: require an exclusive READ_DATA | DELETE | READ_ATTRIBUTES
    /// handle, matching identity and at least one other hardlink. Existing
    /// readers/writers through any alias cause typed LiveWriter refusal. Pins
    /// and SHARE_NONE remain held until handle-bound POSIX disposition finishes.
    /// No pathname deletion, missing-file success, or sharing fallback.
    /// Caller journals the step and rejects links outside its known inventory.
    #[cfg(test)]
    pub(crate) fn unlink_exclusive(root: &Path, path: &Path, expected: FileIdentity) -> Result<()> {
        exclusive_unlink(root, path, expected, None, |_| Ok(()))
    }

    /// Validate the caller's complete known-name inventory under SHARE_NONE,
    /// closing the gap between an earlier count snapshot and exclusive open.
    /// Refuse both extra and missing links, and never unlink a last name even
    /// when expected_links is one. Retain exclusion/pins through disposition.
    pub(crate) fn unlink_counted(
        root: &Path,
        path: &Path,
        expected: FileIdentity,
        expected_links: u32,
    ) -> Result<()> {
        unlink_validated(root, path, expected, expected_links, |_| Ok(()))
    }

    /// Check identity, exact count and never-last under SHARE_NONE, then validate
    /// through the SAME readable handle before unlinking. The callback borrows
    /// the handle only for this call; it must not reopen names or retain clones.
    /// A callback/read/permission error closes the handle without disposition.
    /// Caller must hash/size-check here to catch an append completed after its
    /// earlier snapshot but before exclusive acquisition. Directory pins and
    /// exclusive access remain alive from inspection through final disposition.
    pub(crate) fn unlink_validated(
        root: &Path,
        path: &Path,
        expected: FileIdentity,
        expected_links: u32,
        validate: impl FnOnce(&File) -> Result<()>,
    ) -> Result<()> {
        exclusive_unlink(root, path, expected, Some(expected_links), validate)
    }

    fn exclusive_unlink(
        root: &Path,
        path: &Path,
        expected: FileIdentity,
        expected_links: Option<u32>,
        validate: impl FnOnce(&File) -> Result<()>,
    ) -> Result<()> {
        let removal = verified_handle(root, path, expected, true, expected_links)?;
        validate(&removal.file)?;
        removal.remove()
    }

    /// Remove only the opened name of the expected file, never a replacement.
    /// Caller retains coordination/thread guards and decides journal ownership;
    /// this primitive does not infer whether a matching hardlink is borrowed.
    /// Ancestors remain pinned through the identity check and handle-bound unlink.
    /// Requires Windows/FS support for POSIX FileDispositionInfoEx; no path-based
    /// or readonly-attribute-overriding fallback. A missing name is an error.
    pub(crate) fn remove_verified(root: &Path, path: &Path, expected: FileIdentity) -> Result<()> {
        verified_removal(root, path, expected)?.remove()
    }

    /// Rename the expected source object with both roots pinned. Destination
    /// collisions fail atomically; no replacement, directory creation, or
    /// cross-volume copy fallback. Caller retains writer guards and journals it.
    pub(crate) fn rename_verified(
        root_from: &Path,
        path: &Path,
        root_to: &Path,
        target: &Path,
        expected: FileIdentity,
    ) -> Result<()> {
        verified_removal(root_from, path, expected)?.rename(root_to, target)
    }

    struct PendingLock {
        identity: FileIdentity,
        file: File,
        home: PathBuf,
    }

    fn pending(
        home: &Path,
        path: &Path,
        pins: &mut Vec<File>,
        create: bool,
    ) -> Result<Option<PendingLock>> {
        let (ancestors, exists) = pin_path(home, path)?;
        pins.extend(ancestors);
        if !exists && !create {
            return Ok(None);
        }
        if create {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow!("Codex 잠금 폴더가 없습니다"))?;
            std::fs::create_dir_all(parent)?;
            let (checked, _) = pin_path(home, path)?;
            pins.extend(checked);
        }
        let file = match options()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(path)
        {
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
    #[cfg(test)]
    pub(crate) fn probe_all(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe(homes, ids, rollouts, false, false, true, false)
    }

    /// Unlink-only recovery: retain all writer/path/share checks, but leave any
    /// compressed sibling untouched. Inputs must still be plain rollout names.
    /// Caller must verify identities/ownership before deleting journaled links.
    pub(crate) fn probe_cleanup(
        homes: &[PathBuf],
        ids: &[String],
        plain_rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe(homes, ids, plain_rollouts, true, false, true, false)
    }

    /// Applying operations reserve even absent per-home Codex locks. Existing
    /// rollouts are pinned against writers first; all coordination locks are
    /// acquired before creating/taking thread locks, and retained through commit.
    #[cfg(test)]
    pub(crate) fn reserve_commit(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe(homes, ids, rollouts, false, true, true, false)
    }

    /// Reserve coordination/thread locks and directory pins for panel 2 commit.
    /// Rollouts undergo the same path/compression/writer checks, but their read
    /// handles are dropped before returning so `unlink_validated` can acquire SHARE_NONE.
    /// This does NOT exclude raw rollout readers/writers after the transient
    /// check: every destructive step must use `unlink_validated` to check exclusively.
    #[cfg(test)]
    pub(crate) fn reserve_commit_exclusive(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        reserve_exclusive(homes, ids, rollouts, false)
    }
    /// Call only after a complete metadata catalog checked compressed headers.
    pub(crate) fn reserve_decoded_exclusive(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        reserve_exclusive(homes, ids, rollouts, true)
    }
    pub(crate) fn probe_decoded(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe(homes, ids, rollouts, true, false, true, true)
    }
    pub(crate) fn reserve_decoded(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe(homes, ids, rollouts, true, true, true, true)
    }
    fn reserve_exclusive(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
        decoded: bool,
    ) -> Result<Guards> {
        check_ids(ids)?;
        let homes = homes
            .iter()
            .map(|home| absolute(home))
            .collect::<Result<Vec<_>>>()?;
        let mut parents = Vec::new();
        for path in rollouts {
            let path = absolute(path)?;
            let home = homes
                .iter()
                .filter(|home| path.starts_with(home))
                .max_by_key(|home| home.components().count())
                .ok_or_else(|| anyhow!("rollout 경로가 검사 대상 홈 밖에 있습니다"))?;
            let parent = path
                .parent()
                .ok_or_else(|| anyhow!("대상 폴더가 없습니다"))?;
            parents.push(anchored_parent(home, parent, false)?);
        }
        for home in &homes {
            parents.push(anchored_parent(home, home, false)?);
        }
        let mut guards = probe(&homes, ids, rollouts, decoded, true, false, decoded)?;
        guards._publications.extend(parents);
        Ok(guards)
    }

    pub(crate) fn reserve_cleanup(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe(homes, ids, rollouts, true, true, true, false)
    }

    fn check_ids(ids: &[String]) -> Result<()> {
        if ids.iter().any(|id| {
            id.is_empty()
                || id.len() > 128
                || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        }) {
            bail!("잘못된 세션 식별자입니다");
        }
        Ok(())
    }

    fn probe(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
        allow_compressed_sibling: bool,
        reserve: bool,
        retain_rollouts: bool,
        decoded_input: bool,
    ) -> Result<Guards> {
        check_ids(ids)?;
        let mut guards = Guards {
            _rollouts: Vec::new(),
            _threads: Vec::new(),
            _coordination: Vec::new(),
            _publications: Vec::new(),
            _ancestors: Vec::new(),
        };
        let homes = homes
            .iter()
            .map(|home| absolute(home))
            .collect::<Result<Vec<_>>>()?;
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
            if (compressed_input && !decoded_input)
                || (!allow_compressed_sibling && compressed_sibling(&path))
            {
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
            if retain_rollouts {
                guards._rollouts.push(file);
            }
        }
        // The caller already anchors exclusive reservations' homes/source
        // parents. Prepare lock directories only AFTER rollout writer checks,
        // so a refused existing writer leaves no newly created lock directory.
        if reserve && !retain_rollouts {
            for home in &homes {
                guards
                    ._publications
                    .push(prepare_parent(home, &home.join("thread-writer-locks"))?);
            }
        }
        let mut locks = Vec::new();
        for home in &homes {
            let path = home.join("thread-writer-locks/.coordination.lock");
            if let Some(lock) = pending(home, &path, &mut guards._ancestors, reserve)? {
                locks.push(lock);
            }
        }
        guards._coordination = take_locks(locks)?;
        let mut locks = Vec::new();
        for home in &homes {
            for id in ids {
                let path = home.join("thread-writer-locks").join(format!("{id}.lock"));
                if let Some(lock) = pending(home, &path, &mut guards._ancestors, reserve)? {
                    locks.push(lock);
                }
            }
        }
        guards._threads = take_locks(locks)?;
        Ok(guards)
    }
}

#[cfg(windows)]
pub(crate) use platform::{
    hardlink_count, identity, probe_cleanup, probe_decoded, remove_verified, rename_verified,
    reserve_cleanup, reserve_decoded, reserve_decoded_exclusive, safe_path, same_volume,
    unlink_counted, unlink_validated,
};

// Optional admission/uncounted compatibility APIs are for qualification tests.
#[cfg(test)]
pub(crate) use platform::{check_home_idle, unlink_exclusive, unlink_exclusive as unlink};
#[cfg(all(test, windows))]
pub(crate) use platform::{probe_all, reserve_commit, reserve_commit_exclusive};

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub(crate) struct Guards;
    impl Guards {
        pub(crate) fn release_rollouts_for_unlink(&mut self) {}
    }
    pub(crate) struct PublicationGuard;
    pub(crate) fn prepare_parent(_: &Path, _: &Path) -> Result<PublicationGuard> {
        anyhow::bail!("세션 경로 보호는 Windows에서만 지원됩니다")
    }
    pub(crate) fn safe_path(_: &Path, _: &Path) -> Result<()> {
        anyhow::bail!("세션 가져오기 경로 검사는 Windows에서만 지원됩니다")
    }
    pub(crate) fn identity(_: &Path) -> Result<FileIdentity> {
        anyhow::bail!("세션 파일 식별은 Windows에서만 지원됩니다")
    }
    pub(crate) fn hardlink_count(_: &Path) -> Result<u32> {
        anyhow::bail!("세션 파일 식별은 Windows에서만 지원됩니다")
    }
    pub(crate) fn same_volume(_: &Path, _: &Path) -> Result<bool> {
        anyhow::bail!("세션 파일 식별은 Windows에서만 지원됩니다")
    }
    #[cfg(test)]
    pub(crate) fn check_home_idle(_: &Path) -> Result<()> {
        anyhow::bail!("세션 잠금 검사는 Windows에서만 지원됩니다")
    }
    #[cfg(test)]
    pub(crate) fn unlink_exclusive(_: &Path, _: &Path, _: FileIdentity) -> Result<()> {
        anyhow::bail!("검증한 세션 파일 삭제는 Windows에서만 지원됩니다")
    }
    pub(crate) fn unlink_counted(_: &Path, _: &Path, _: FileIdentity, _: u32) -> Result<()> {
        anyhow::bail!("검증한 세션 파일 삭제는 Windows에서만 지원됩니다")
    }
    pub(crate) fn unlink_validated(
        _: &Path,
        _: &Path,
        _: FileIdentity,
        _: u32,
        _: impl FnOnce(&std::fs::File) -> Result<()>,
    ) -> Result<()> {
        anyhow::bail!("검증한 세션 파일 삭제는 Windows에서만 지원됩니다")
    }
    pub(crate) fn remove_verified(_: &Path, _: &Path, _: FileIdentity) -> Result<()> {
        anyhow::bail!("검증한 세션 파일 삭제는 Windows에서만 지원됩니다")
    }
    pub(crate) fn rename_verified(
        _: &Path,
        _: &Path,
        _: &Path,
        _: &Path,
        _: FileIdentity,
    ) -> Result<()> {
        anyhow::bail!("검증한 세션 파일 이동은 Windows에서만 지원됩니다")
    }
    pub(crate) fn probe_all(_: &[PathBuf], _: &[String], _: &[PathBuf]) -> Result<Guards> {
        anyhow::bail!("세션 잠금 검사는 Windows에서만 지원됩니다")
    }
    pub(crate) fn reserve_commit(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe_all(homes, ids, rollouts)
    }
    pub(crate) fn reserve_cleanup(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe_all(homes, ids, rollouts)
    }
    pub(crate) fn reserve_commit_exclusive(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe_all(homes, ids, rollouts)
    }
    pub(crate) fn reserve_decoded_exclusive(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe_all(homes, ids, rollouts)
    }
    pub(crate) fn probe_decoded(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe_all(homes, ids, rollouts)
    }
    pub(crate) fn reserve_decoded(
        homes: &[PathBuf],
        ids: &[String],
        rollouts: &[PathBuf],
    ) -> Result<Guards> {
        probe_all(homes, ids, rollouts)
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
pub(crate) use platform::{
    hardlink_count, identity, probe_all, probe_cleanup, probe_decoded, remove_verified,
    rename_verified, reserve_cleanup, reserve_commit, reserve_commit_exclusive, reserve_decoded,
    reserve_decoded_exclusive, safe_path, same_volume, unlink_counted, unlink_validated,
};

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

    fn assert_live<T>(result: Result<T>, home: &Path) {
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

    // Test only: attach mount-point/symlink tags without shell quoting or a
    // path-following helper. Targets and handles are exclusively synthetic.
    fn directory_tag(path: &Path, target: &Path, symlink: bool) -> std::io::Result<()> {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        #[link(name = "kernel32")]
        unsafe extern "system" {
            #[link_name = "DeviceIoControl"]
            fn device_io_control(
                device: *mut std::ffi::c_void,
                code: u32,
                input: *const u8,
                input_len: u32,
                output: *mut u8,
                output_len: u32,
                returned: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
        }
        let writer = OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)?;
        let units = target.as_os_str().encode_wide().collect::<Vec<_>>();
        let ordinary = if units.starts_with(&[92, 92, 63, 92]) {
            &units[4..]
        } else {
            &units
        };
        let mut substitute = r"\??\".encode_utf16().collect::<Vec<_>>();
        substitute.extend_from_slice(ordinary);
        let sub_len = u16::try_from(substitute.len() * 2).unwrap();
        let print_len = u16::try_from(ordinary.len() * 2).unwrap();
        let fixed = if symlink { 12 } else { 8 };
        let data_len = fixed + sub_len + 2 + print_len + 2;
        let tag: u32 = if symlink { 0xA000000C } else { 0xA0000003 };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        for value in [0, sub_len, sub_len + 2, print_len] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        if symlink {
            bytes.extend_from_slice(&0u32.to_le_bytes());
        }
        for unit in substitute
            .iter()
            .copied()
            .chain(std::iter::once(0))
            .chain(ordinary.iter().copied())
            .chain(std::iter::once(0))
        {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let mut returned = 0;
        // SAFETY: writer owns the directory handle; the complete initialized
        // reparse buffer and writable result count live through this sync call.
        let result = unsafe {
            device_io_control(
                writer.as_raw_handle(),
                0x900A4,
                bytes.as_ptr().cast(),
                bytes.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if result == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn marker_names(parent: &Path) -> Vec<PathBuf> {
        fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .as_encoded_bytes()
                    .starts_with(b".quota-baton-parent-pin-")
            })
            .collect()
    }

    // Deny creation only on this synthetic directory, not inherited children.
    // Restore the explicit test ACE even if the guarded operation panics.
    struct DenyCreation(PathBuf);

    impl DenyCreation {
        fn new(path: &Path) -> Self {
            let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
            let ordinary = if units.starts_with(&[92, 92, 63, 92]) {
                &units[4..]
            } else {
                &units
            };
            let path = PathBuf::from(std::ffi::OsString::from_wide(ordinary));
            let result = Command::new("icacls.exe")
                .arg(&path)
                .args(["/deny", "*S-1-1-0:(WD,AD)"])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "set synthetic directory creation denial"
            );
            Self(path)
        }
    }

    impl Drop for DenyCreation {
        fn drop(&mut self) {
            let restored = Command::new("icacls.exe")
                .arg(&self.0)
                .args(["/remove:d", "*S-1-1-0"])
                .output()
                .is_ok_and(|result| result.status.success());
            if !std::thread::panicking() {
                assert!(restored, "restore synthetic directory permissions");
            }
        }
    }

    #[test]
    fn publication_parent_needs_no_creation_permission_above_home_and_refuses_denied_tree() {
        let temp = fixture();
        let root = home(temp.path(), "home");
        let parent = root.join("new/staging/leaf");
        // Above-home writes are denied. Creation strictly inside root must work.
        let ancestor_denial = DenyCreation::new(temp.path());
        let held = prepare_parent(&root, &parent).unwrap();
        assert!(marker_names(temp.path()).is_empty());
        assert!(marker_names(&root).is_empty());
        assert_eq!(marker_names(&parent).len(), 1);
        drop(held);
        drop(ancestor_denial);
        assert!(marker_names(&parent).is_empty());
        // Existing leaf has list permission but forbids the anchor file.
        let leaf_denial = DenyCreation::new(&parent);
        let result = prepare_parent(&root, &parent);
        drop(leaf_denial);
        assert!(result.is_err());
        assert!(marker_names(&parent).is_empty());
        // Missing child creation under a denied owned root must fail closed.
        let root_denial = DenyCreation::new(&root);
        let missing = root.join("must-not-create/leaf");
        let result = prepare_parent(&root, &missing);
        drop(root_denial);
        assert!(result.is_err());
        assert!(!root.join("must-not-create").exists());
        assert!(marker_names(&root).is_empty());
        let held = prepare_parent(&root, &parent).unwrap();
        drop(held);
        assert!(marker_names(&parent).is_empty());
    }

    #[test]
    fn publication_parent_retries_transient_directory_writer_but_refuses_a_persistent_one() {
        use std::{
            os::windows::fs::OpenOptionsExt,
            time::{Duration, Instant},
        };
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        let temp = fixture();
        let root = home(temp.path(), "home");
        let parent = root.join("publication");
        fs::create_dir(&parent).unwrap();
        let writer = || {
            OpenOptions::new()
                .write(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
                .open(&parent)
                .unwrap()
        };
        let blocker = writer();
        let held = std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                drop(blocker);
            });
            prepare_parent(&root, &parent).unwrap()
        });
        // A lasting writer must not be accommodated with weaker sharing.
        let blocker = writer();
        let started = Instant::now();
        assert_live(prepare_parent(&root, &parent), &root);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "directory retry must be bounded"
        );
        drop(blocker);
        drop(held);
        assert!(marker_names(&parent).is_empty());
    }

    #[test]
    fn publication_parent_creates_coupled_directories_supports_nested_publication_and_cleans_markers()
     {
        let temp = fixture();
        let root = home(temp.path(), "home");
        let parent = root.join("private/staging/leaf");
        assert!(!parent.exists());
        let outer: PublicationGuard = prepare_parent(&root, &parent).unwrap();
        let markers = marker_names(&parent);
        assert_eq!(markers.len(), 1);
        assert_eq!(fs::metadata(&markers[0]).unwrap().len(), 0);
        assert_eq!(
            fs::remove_file(&markers[0]).unwrap_err().raw_os_error(),
            Some(32)
        );
        assert!(fs::rename(&parent, root.join("renamed-leaf")).is_err());
        let inner = prepare_parent(&root, &parent).unwrap();
        assert_eq!(marker_names(&parent).len(), 2);
        let source = root.join("sessions/source.jsonl");
        let stage = parent.join("stage.jsonl");
        let published = parent.join("published.jsonl");
        fs::write(&source, b"synthetic anchored publication").unwrap();
        fs::hard_link(&source, &stage).unwrap();
        let expected = identity(&source).unwrap();
        rename_verified(&root, &stage, &root, &published, expected).unwrap();
        assert_eq!(identity(&published).unwrap(), expected);
        drop(inner);
        assert_eq!(marker_names(&parent), markers);
        drop(outer);
        assert!(marker_names(&parent).is_empty());
        assert!(marker_names(temp.path()).is_empty());
        fs::rename(&parent, root.join("renamed-leaf")).unwrap();
    }

    #[test]
    fn publication_parent_anchor_refuses_junction_and_supported_symlink_retag() {
        let temp = fixture();
        let root = home(temp.path(), "home");
        let outside = home(temp.path(), "outside");
        let baseline = root.join("empty-baseline");
        fs::create_dir(&baseline).unwrap();
        directory_tag(&baseline, &outside, false).unwrap();
        fs::remove_dir(&baseline).unwrap();
        let parent = root.join("anchored/leaf");
        let held = prepare_parent(&root, &parent).unwrap();
        for candidate in [&parent, parent.parent().unwrap(), &root] {
            assert_eq!(
                directory_tag(candidate, &outside, false)
                    .unwrap_err()
                    .raw_os_error(),
                Some(145)
            );
        }
        let symlink_baseline = root.join("symlink-baseline");
        fs::create_dir(&symlink_baseline).unwrap();
        match directory_tag(&symlink_baseline, &outside, true) {
            Ok(()) => {
                fs::remove_dir(&symlink_baseline).unwrap();
                assert_eq!(
                    directory_tag(&parent, &outside, true)
                        .unwrap_err()
                        .raw_os_error(),
                    Some(145)
                );
            }
            // Some Windows installations do not grant symlink privilege.
            // Junction protection above is always exercised, never skipped.
            Err(error) => assert_eq!(error.raw_os_error(), Some(1314)),
        }
        assert!(prepare_parent(&root, &outside).is_err());
        assert!(marker_names(&outside).is_empty());
        drop(held);
        assert!(marker_names(&parent).is_empty());
        directory_tag(&parent, &outside, false).unwrap();
        fs::remove_dir(&parent).unwrap();
        assert!(prepare_parent(&root, &baseline).is_ok());
    }

    #[test]
    fn publication_parent_refuses_preexisting_reparse_and_readonly_checks_make_no_markers() {
        let temp = fixture();
        let root = home(temp.path(), "home");
        let outside = home(temp.path(), "outside");
        let alias = root.join("alias");
        fs::create_dir(&alias).unwrap();
        directory_tag(&alias, &outside, false).unwrap();
        assert!(prepare_parent(&root, &alias.join("must-not-create")).is_err());
        assert!(!outside.join("must-not-create").exists());
        assert!(marker_names(&outside).is_empty());
        let before = fs::read_dir(&root).unwrap().count();
        safe_path(&root, &root.join("missing/leaf")).unwrap();
        let probe = probe_all(std::slice::from_ref(&root), &[ID.into()], &[]).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), before);
        assert!(!root.join("missing").exists());
        assert!(!root.join("thread-writer-locks").exists());
        assert!(marker_names(&root).is_empty());
        drop(probe);
        fs::remove_dir(&alias).unwrap();
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
            assert_live(
                reserve_commit_exclusive(std::slice::from_ref(&home), &[ID.into()], &[]),
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
        assert_live(
            reserve_commit_exclusive(&homes, &[ID.into()], std::slice::from_ref(&link)),
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
    fn same_volume_accepts_existing_source_and_directory_without_creating_names() {
        let temp = fixture();
        let source_home = home(temp.path(), "source");
        let target_home = home(temp.path(), "target");
        let source = source_home.join("sessions/source.jsonl");
        fs::write(&source, b"synthetic source").unwrap();
        assert!(same_volume(&source, &target_home).unwrap());
        assert!(same_volume(&source, &source_home).unwrap());
        let ordinary_target = PathBuf::from(std::ffi::OsString::from_wide(
            &target_home
                .as_os_str()
                .encode_wide()
                .skip(4)
                .collect::<Vec<_>>(),
        ));
        assert!(same_volume(&source, &ordinary_target).unwrap());
        let missing = temp.path().join("not-created");
        assert!(same_volume(&source, &missing).is_err());
        assert!(!missing.exists());
        assert!(same_volume(&missing, &target_home).is_err());
        assert!(same_volume(&source_home, &target_home).is_err());
        assert!(same_volume(&source, &source).is_err());
        assert!(!target_home.join("journal").exists());
        assert_eq!(fs::read(&source).unwrap(), b"synthetic source");
    }

    #[test]
    fn exclusive_unlink_counts_links_preserves_object_and_never_removes_last_name() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let source = home.join("sessions/source.jsonl");
        let staged = home.join("staged.jsonl");
        fs::write(&source, b"synthetic shared object").unwrap();
        let expected = identity(&source).unwrap();
        assert_eq!(hardlink_count(&source).unwrap(), 1);
        assert_eq!(
            unlink(&home, &source, expected).unwrap_err().to_string(),
            "세션 파일의 마지막 하드링크는 삭제할 수 없습니다"
        );
        fs::hard_link(&source, &staged).unwrap();
        assert_eq!(hardlink_count(&source).unwrap(), 2);
        assert_eq!(hardlink_count(&staged).unwrap(), 2);
        unlink_exclusive(&home, &source, expected).unwrap();
        assert!(!source.exists());
        assert_eq!(identity(&staged).unwrap(), expected);
        assert_eq!(fs::read(&staged).unwrap(), b"synthetic shared object");
        assert_eq!(hardlink_count(&staged).unwrap(), 1);
        assert!(unlink_exclusive(&home, &staged, expected).is_err());
        assert_eq!(identity(&staged).unwrap(), expected);
        assert!(hardlink_count(&source).is_err());
        assert!(hardlink_count(&home).is_err());
    }

    #[test]
    fn exclusive_unlink_refuses_fully_shared_reader_and_writer_through_another_alias() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let temp = fixture();
        let home = home(temp.path(), "home");
        let source = home.join("sessions/source.jsonl");
        let alias = home.join("staged.jsonl");
        fs::write(&source, b"synthetic shared object").unwrap();
        fs::hard_link(&source, &alias).unwrap();
        let expected = identity(&source).unwrap();
        for writer in [false, true] {
            // The holder permits every sharing mode: refusal must come from
            // unlink's SHARE_NONE, not an incidental deny-delete/read option.
            let holder = OpenOptions::new()
                .read(!writer)
                .write(writer)
                .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
                .open(&alias)
                .unwrap();
            assert_live(unlink_exclusive(&home, &source, expected), &home);
            assert_live(unlink_counted(&home, &source, expected, 2), &home);
            assert_eq!(identity(&source).unwrap(), expected);
            assert_eq!(hardlink_count(&alias).unwrap(), 2);
            drop(holder);
        }
        unlink_exclusive(&home, &source, expected).unwrap();
        assert_eq!(fs::read(&alias).unwrap(), b"synthetic shared object");
    }

    #[test]
    fn exclusive_unlink_refuses_replacement_without_removing_either_object() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let source = home.join("sessions/source.jsonl");
        let original = home.join("original.jsonl");
        let replacement_link = home.join("replacement.jsonl");
        fs::write(&source, b"synthetic original").unwrap();
        let expected = identity(&source).unwrap();
        fs::rename(&source, &original).unwrap();
        fs::write(&source, b"synthetic replacement").unwrap();
        fs::hard_link(&source, &replacement_link).unwrap();
        assert_eq!(
            unlink_exclusive(&home, &source, expected)
                .unwrap_err()
                .to_string(),
            "대상 파일 소유권이 달라 되돌릴 수 없습니다"
        );
        assert_eq!(identity(&original).unwrap(), expected);
        assert_eq!(fs::read(&source).unwrap(), b"synthetic replacement");
        assert_eq!(hardlink_count(&source).unwrap(), 2);
    }

    #[test]
    fn counted_unlink_refuses_late_unknown_alias_and_preserves_every_name() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let source = home.join("sessions/source.jsonl");
        let stage = home.join("staged.jsonl");
        let unknown = temp.path().join("late-unknown-alias.jsonl");
        fs::write(&source, b"synthetic shared object").unwrap();
        fs::hard_link(&source, &stage).unwrap();
        let expected = identity(&source).unwrap();
        let known_links = hardlink_count(&source).unwrap();
        assert_eq!(known_links, 2);
        // Deterministic boundary: admission counted two names, but an unknown
        // third name appears before unlink obtains its exclusive file handle.
        fs::hard_link(&source, &unknown).unwrap();
        assert!(
            unlink_validated(&home, &source, expected, known_links, |_| {
                panic!("count mismatch must precede content validation")
            })
            .is_err()
        );
        for wrong_count in [known_links, 4] {
            assert_eq!(
                unlink_counted(&home, &source, expected, wrong_count)
                    .unwrap_err()
                    .to_string(),
                "세션 파일의 하드링크 수가 변경되어 작업을 중단했습니다"
            );
            for name in [&source, &stage, &unknown] {
                assert_eq!(identity(name).unwrap(), expected);
                assert_eq!(hardlink_count(name).unwrap(), 3);
                assert_eq!(fs::read(name).unwrap(), b"synthetic shared object");
            }
        }
        unlink_counted(&home, &source, expected, 3).unwrap();
        assert!(!source.exists());
        assert_eq!(hardlink_count(&stage).unwrap(), 2);
        unlink_counted(&home, &stage, expected, 2).unwrap();
        assert_eq!(hardlink_count(&unknown).unwrap(), 1);
        assert!(unlink_counted(temp.path(), &unknown, expected, 1).is_err());
        assert_eq!(fs::read(&unknown).unwrap(), b"synthetic shared object");
    }

    #[test]
    fn validated_unlink_hashes_borrowed_exclusive_handle_and_preserves_late_append_on_error() {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        let temp = fixture();
        let home = home(temp.path(), "home");
        let source = home.join("sessions/source.jsonl");
        let staged = home.join("staged.jsonl");
        let original = b"synthetic original\n";
        fs::write(&source, original).unwrap();
        fs::hard_link(&source, &staged).unwrap();
        let expected = identity(&source).unwrap();
        let prior_hash = Sha256::digest(original);
        // A raw writer appends and closes AFTER the engine's earlier hash.
        {
            let mut writer = OpenOptions::new().append(true).open(&staged).unwrap();
            writer.write_all(b"synthetic late append\n").unwrap();
        }
        let mut called = false;
        let error = unlink_validated(&home, &source, expected, 2, |file| {
            called = true;
            // A fresh pathname open cannot succeed while the validator borrows
            // SHARE_NONE. Read and metadata must use only the borrowed handle.
            assert!(fs::File::open(&staged).is_err());
            assert!(OpenOptions::new().append(true).open(&staged).is_err());
            let mut borrowed = file;
            let mut contents = Vec::new();
            borrowed.read_to_end(&mut contents)?;
            assert_eq!(file.metadata()?.len(), contents.len() as u64);
            anyhow::ensure!(
                Sha256::digest(&contents) == prior_hash,
                "synthetic content changed"
            );
            Ok(())
        })
        .unwrap_err();
        assert!(called);
        assert_eq!(error.to_string(), "synthetic content changed");
        let current = b"synthetic original\nsynthetic late append\n";
        for name in [&source, &staged] {
            assert_eq!(identity(name).unwrap(), expected);
            assert_eq!(hardlink_count(name).unwrap(), 2);
            assert_eq!(fs::read(name).unwrap(), current);
        }
        // A validator accepting the current contents permits handle-bound
        // removal; it cannot consume or replace ownership of the File.
        unlink_validated(&home, &source, expected, 2, |file| {
            let mut borrowed = file;
            let mut contents = Vec::new();
            borrowed.read_to_end(&mut contents)?;
            anyhow::ensure!(
                Sha256::digest(&contents) == Sha256::digest(current),
                "synthetic mismatch"
            );
            Ok(())
        })
        .unwrap();
        assert!(!source.exists());
        assert_eq!(identity(&staged).unwrap(), expected);
        assert_eq!(fs::read(&staged).unwrap(), current);
    }

    #[test]
    fn idle_home_check_refuses_known_db_readers_writers_and_retains_no_handles() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let temp = fixture();
        let missing = temp.path().join("missing-home");
        check_home_idle(&missing).unwrap();
        assert!(!missing.exists());
        let home = home(temp.path(), "home");
        for name in ["state_5.sqlite", "goals_1.sqlite"] {
            let path = home.join(name);
            fs::write(&path, b"synthetic database placeholder").unwrap();
            for writer in [false, true] {
                let holder = OpenOptions::new()
                    .read(!writer)
                    .write(writer)
                    .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
                    .open(&path)
                    .unwrap();
                assert_live(check_home_idle(&home), &home);
                drop(holder);
            }
            check_home_idle(&home).unwrap();
            // Admission must not block our subsequent real SQL operations.
            let writer = OpenOptions::new()
                .write(true)
                .share_mode(0)
                .open(&path)
                .unwrap();
            drop(writer);
            assert_eq!(fs::read(&path).unwrap(), b"synthetic database placeholder");
        }
    }

    #[test]
    fn exclusive_commit_retains_thread_coordination_and_directory_pins_but_no_rollout_reads() {
        let temp = fixture();
        let source_home = home(temp.path(), "source");
        let target_home = home(temp.path(), "target");
        let source = source_home.join("sessions/source.jsonl");
        let staging = target_home.join("non-discoverable-staging");
        fs::create_dir_all(&staging).unwrap();
        let staged = staging.join("staged.jsonl");
        fs::write(&source, b"synthetic shared object").unwrap();
        fs::hard_link(&source, &staged).unwrap();
        let expected = identity(&source).unwrap();
        let homes = [source_home.clone(), target_home.clone()];
        let ids = [ID.to_owned()];
        let held =
            reserve_commit_exclusive(&homes, &ids, &[source.clone(), staged.clone()]).unwrap();
        for home in &homes {
            for name in [".coordination.lock".to_owned(), format!("{ID}.lock")] {
                let contender = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(home.join("thread-writer-locks").join(name))
                    .unwrap();
                assert!(matches!(
                    contender.try_lock(),
                    Err(std::fs::TryLockError::WouldBlock)
                ));
            }
        }
        assert!(fs::rename(&staging, target_home.join("renamed-staging")).is_err());
        assert!(
            fs::rename(
                source_home.join("sessions"),
                source_home.join("renamed-sessions")
            )
            .is_err()
        );
        // Directory pins must stop moving the directory, yet still permit the
        // child-name rename used to publish a staged rollout into that directory.
        let publish_source = staging.join("publish-source.jsonl");
        let publish_target = staging.join("publish-target.jsonl");
        fs::write(&publish_source, b"synthetic publication").unwrap();
        let publish_id = identity(&publish_source).unwrap();
        rename_verified(
            &target_home,
            &publish_source,
            &target_home,
            &publish_target,
            publish_id,
        )
        .unwrap();
        assert!(!publish_source.exists());
        assert_eq!(identity(&publish_target).unwrap(), publish_id);
        // Reservations intentionally leave raw rollout handles possible. The
        // exclusive destructive boundary still refuses a late writer.
        let writer = OpenOptions::new().append(true).open(&staged).unwrap();
        assert_live(
            unlink_exclusive(&source_home, &source, expected),
            &source_home,
        );
        drop(writer);
        // This succeeds while locks/pins are alive only if no read probe lingers.
        unlink_exclusive(&source_home, &source, expected).unwrap();
        assert!(!source.exists());
        assert_eq!(identity(&staged).unwrap(), expected);
        drop(held);
        for home in &homes {
            for name in [".coordination.lock".to_owned(), format!("{ID}.lock")] {
                let contender = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(home.join("thread-writer-locks").join(name))
                    .unwrap();
                contender.try_lock().unwrap();
            }
        }
    }

    #[test]
    fn verified_unlink_refuses_leaf_replaced_after_probe() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let path = home.join("sessions/owned.jsonl");
        let parked = home.join("sessions/parked.jsonl");
        fs::write(&path, b"synthetic original").unwrap();
        let expected = identity(&path).unwrap();
        let guards = probe_cleanup(
            std::slice::from_ref(&home),
            &[ID.into()],
            std::slice::from_ref(&path),
        )
        .unwrap();
        fs::rename(&path, &parked).unwrap();
        fs::write(&path, b"synthetic replacement").unwrap();
        assert!(remove_verified(&home, &path, expected).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"synthetic replacement");
        assert_eq!(fs::read(&parked).unwrap(), b"synthetic original");
        assert_eq!(identity(&parked).unwrap(), expected);
        drop(guards);
    }

    #[test]
    fn verified_unlink_removes_own_name_and_preserves_borrowed_hardlink() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let owned = home.join("sessions/owned.jsonl");
        let borrowed = home.join("sessions/borrowed.jsonl");
        fs::write(&owned, b"synthetic shared object").unwrap();
        fs::hard_link(&owned, &borrowed).unwrap();
        let expected = identity(&owned).unwrap();
        let guards = probe_cleanup(
            std::slice::from_ref(&home),
            &[ID.into()],
            &[owned.clone(), borrowed.clone()],
        )
        .unwrap();
        remove_verified(&home, &owned, expected).unwrap();
        assert!(!owned.exists());
        assert_eq!(identity(&borrowed).unwrap(), expected);
        assert_eq!(fs::read(&borrowed).unwrap(), b"synthetic shared object");
        assert!(OpenOptions::new().append(true).open(&borrowed).is_err());
        drop(guards);
        assert!(OpenOptions::new().append(true).open(&borrowed).is_ok());

        // Also cover a final owned link with a read probe still open, without
        // authorizing removal of the borrowed name above.
        let single = home.join("sessions/single-owned.jsonl");
        fs::write(&single, b"synthetic sole owned link").unwrap();
        let single_id = identity(&single).unwrap();
        let guards = probe_cleanup(
            std::slice::from_ref(&home),
            &[ID.into()],
            std::slice::from_ref(&single),
        )
        .unwrap();
        remove_verified(&home, &single, single_id).unwrap();
        assert!(!single.exists());
        assert_eq!(identity(&borrowed).unwrap(), expected);
        drop(guards);
    }

    #[test]
    fn verified_unlink_stays_bound_after_open_and_keeps_ancestors_pinned() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let path = home.join("sessions/owned.jsonl");
        let parked = home.join("sessions/parked.jsonl");
        let borrowed = home.join("sessions/borrowed.jsonl");
        fs::write(&path, b"synthetic original").unwrap();
        fs::hard_link(&path, &borrowed).unwrap();
        let expected = identity(&path).unwrap();
        let removal = platform::verified_removal(&home, &path, expected).unwrap();
        assert!(fs::rename(home.join("sessions"), home.join("renamed-sessions")).is_err());
        fs::rename(&path, &parked).unwrap();
        fs::write(&path, b"synthetic replacement after open").unwrap();
        removal.remove().unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            b"synthetic replacement after open"
        );
        assert_eq!(identity(&borrowed).unwrap(), expected);
        assert_eq!(fs::read(&borrowed).unwrap(), b"synthetic original");
    }

    #[test]
    fn verified_unlink_refuses_writer_through_hardlink() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let path = home.join("sessions/owned.jsonl");
        let alias = home.join("sessions/writer.jsonl");
        fs::write(&path, b"synthetic original").unwrap();
        fs::hard_link(&path, &alias).unwrap();
        let expected = identity(&path).unwrap();
        let writer = OpenOptions::new().append(true).open(&alias).unwrap();
        let error = remove_verified(&home, &path, expected).unwrap_err();
        assert!(error.downcast_ref::<LiveWriter>().is_some());
        assert_eq!(fs::read(&path).unwrap(), b"synthetic original");
        assert_eq!(identity(&alias).unwrap(), expected);
        drop(writer);
    }

    #[test]
    fn verified_rename_roundtrip_preserves_identity_and_borrowed_link_under_probes() {
        let temp = fixture();
        let source_home = home(temp.path(), "source");
        let backup_home = home(temp.path(), "backup");
        let source = source_home.join("sessions/owned.jsonl");
        let borrowed = source_home.join("sessions/borrowed.jsonl");
        let backup = backup_home.join("sessions/백업 name.jsonl");
        fs::write(&source, b"synthetic original").unwrap();
        fs::hard_link(&source, &borrowed).unwrap();
        let expected = identity(&source).unwrap();
        let guards = probe_cleanup(
            &[source_home.clone(), backup_home.clone()],
            &[ID.into()],
            &[source.clone(), borrowed.clone()],
        )
        .unwrap();
        rename_verified(&source_home, &source, &backup_home, &backup, expected).unwrap();
        assert!(!source.exists());
        assert_eq!(identity(&backup).unwrap(), expected);
        assert_eq!(identity(&borrowed).unwrap(), expected);
        assert!(OpenOptions::new().append(true).open(&backup).is_err());
        rename_verified(&backup_home, &backup, &source_home, &source, expected).unwrap();
        assert!(!backup.exists());
        assert_eq!(identity(&source).unwrap(), expected);
        assert_eq!(fs::read(&borrowed).unwrap(), b"synthetic original");
        drop(guards);
    }

    #[test]
    fn verified_rename_refuses_replaced_source_and_destination_collision() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let source = home.join("sessions/owned.jsonl");
        let parked = home.join("sessions/parked.jsonl");
        let target = home.join("sessions/backup.jsonl");
        fs::write(&source, b"synthetic original").unwrap();
        let expected = identity(&source).unwrap();
        fs::rename(&source, &parked).unwrap();
        fs::write(&source, b"synthetic source replacement").unwrap();
        assert!(rename_verified(&home, &source, &home, &target, expected).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"synthetic source replacement");
        assert!(!target.exists());

        // Insert a destination after opening/verifying the original source.
        // The OS must reject collision without overwriting either object.
        let operation = platform::verified_removal(&home, &parked, expected).unwrap();
        fs::write(&target, b"synthetic destination replacement").unwrap();
        let destination_id = identity(&target).unwrap();
        assert!(operation.rename(&home, &target).is_err());
        assert_eq!(identity(&target).unwrap(), destination_id);
        assert_eq!(
            fs::read(&target).unwrap(),
            b"synthetic destination replacement"
        );
        assert_eq!(identity(&parked).unwrap(), expected);
        assert_eq!(fs::read(&parked).unwrap(), b"synthetic original");
    }

    #[test]
    fn verified_rename_stays_bound_when_source_name_changes_after_open() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let source = home.join("sessions/owned.jsonl");
        let parked = home.join("sessions/parked.jsonl");
        let target = home.join("sessions/backup.jsonl");
        fs::write(&source, b"synthetic original").unwrap();
        let expected = identity(&source).unwrap();
        let operation = platform::verified_removal(&home, &source, expected).unwrap();
        fs::rename(&source, &parked).unwrap();
        fs::write(&source, b"synthetic replacement after open").unwrap();
        operation.rename(&home, &target).unwrap();
        assert_eq!(identity(&target).unwrap(), expected);
        assert_eq!(fs::read(&target).unwrap(), b"synthetic original");
        assert_eq!(
            fs::read(&source).unwrap(),
            b"synthetic replacement after open"
        );
    }

    #[test]
    fn verified_rename_refuses_writer_escape_and_missing_destination_parent() {
        let temp = fixture();
        let home = home(temp.path(), "home");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let source = home.join("sessions/owned.jsonl");
        let target = home.join("sessions/backup.jsonl");
        fs::write(&source, b"synthetic original").unwrap();
        let expected = identity(&source).unwrap();
        let writer = OpenOptions::new().append(true).open(&source).unwrap();
        let error = rename_verified(&home, &source, &home, &target, expected).unwrap_err();
        assert!(error.downcast_ref::<LiveWriter>().is_some());
        drop(writer);
        assert!(
            rename_verified(
                &home,
                &source,
                &home,
                &outside.join("backup.jsonl"),
                expected
            )
            .is_err()
        );
        assert!(
            rename_verified(
                &home,
                &source,
                &home,
                &home.join("missing/backup.jsonl"),
                expected
            )
            .is_err()
        );
        assert!(!home.join("missing").exists());
        assert_eq!(identity(&source).unwrap(), expected);
        assert_eq!(fs::read(&source).unwrap(), b"synthetic original");
        assert!(!target.exists());
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
    fn commit_reserves_missing_source_and_target_thread_locks_until_drop() {
        let temp = fixture();
        let source = home(temp.path(), "source");
        let target = home(temp.path(), "target");
        let rollout = source.join("sessions/rollout.jsonl");
        fs::write(&rollout, b"synthetic").unwrap();
        let homes = vec![source.clone(), target.clone()];
        let ids = vec![ID.to_string()];
        let held = reserve_commit(&homes, &ids, std::slice::from_ref(&rollout)).unwrap();
        for home in &homes {
            assert!(home.join("thread-writer-locks/.coordination.lock").exists());
            assert!(
                home.join("thread-writer-locks")
                    .join(format!("{ID}.lock"))
                    .exists()
            );
        }
        assert!(probe_all(&homes, &ids, std::slice::from_ref(&rollout)).is_err());
        drop(held);
        assert!(probe_all(&homes, &ids, std::slice::from_ref(&rollout)).is_ok());
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
        assert!(check_home_idle(&alias).is_err());
        assert!(probe_cleanup(std::slice::from_ref(&alias), &[ID.into()], &[]).is_err());
        let protected = outside.join("sessions/protected.jsonl");
        fs::write(&protected, b"synthetic outside root").unwrap();
        let expected = identity(&protected).unwrap();
        assert!(same_volume(&protected, &alias).is_err());
        assert!(same_volume(&alias.join("sessions/protected.jsonl"), &outside).is_err());
        assert!(hardlink_count(&alias.join("sessions/protected.jsonl")).is_err());
        assert!(unlink_exclusive(&root, &protected, expected).is_err());
        assert!(
            unlink_exclusive(&root, &alias.join("sessions/protected.jsonl"), expected).is_err()
        );
        assert!(
            reserve_commit_exclusive(
                std::slice::from_ref(&root),
                &[ID.into()],
                &[alias.join("sessions/protected.jsonl")],
            )
            .is_err()
        );
        assert!(remove_verified(&root, &protected, expected).is_err());
        assert!(remove_verified(&root, &alias.join("sessions/protected.jsonl"), expected).is_err());
        assert_eq!(fs::read(&protected).unwrap(), b"synthetic outside root");
        assert!(
            rename_verified(
                &outside,
                &protected,
                &root,
                &alias.join("sessions/backup.jsonl"),
                expected
            )
            .is_err()
        );
        assert_eq!(fs::read(&protected).unwrap(), b"synthetic outside root");
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
        assert!(
            reserve_commit_exclusive(
                std::slice::from_ref(&home),
                &[ID.into()],
                std::slice::from_ref(&rollout),
            )
            .is_err()
        );
        assert!(probe_all(&[home], &[ID.into()], &[rollout]).is_err());
    }
    #[test]
    fn releasing_rollout_handles_keeps_locks_and_rechecks_the_last_name() {
        let temp = fixture();
        let root = home(temp.path(), "release");
        let source = root.join("sessions/source.jsonl");
        let target = root.join("sessions/target.jsonl");
        fs::write(&source, b"SYNTHETIC never delete last name").unwrap();
        fs::hard_link(&source, &target).unwrap();
        let identity_before = identity(&target).unwrap();
        let mut held = reserve_commit(
            std::slice::from_ref(&root),
            &[ID.into()],
            &[source.clone(), target.clone()],
        )
        .unwrap();
        assert!(OpenOptions::new().append(true).open(&target).is_err());
        held.release_rollouts_for_unlink();
        assert!(reserve_commit_exclusive(std::slice::from_ref(&root), &[ID.into()], &[]).is_err());
        assert!(fs::rename(&root, temp.path().join("renamed")).is_err());
        fs::remove_file(&source).unwrap();
        let error = unlink_counted(&root, &target, identity_before, 1).unwrap_err();
        assert!(error.to_string().contains("마지막 하드링크"));
        assert_eq!(identity(&target).unwrap(), identity_before);
        assert_eq!(
            fs::read(&target).unwrap(),
            b"SYNTHETIC never delete last name"
        );
        assert_eq!(hardlink_count(&target).unwrap(), 1);
    }
}
