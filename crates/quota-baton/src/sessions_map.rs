//! Per-UUID herdr map changes. All locations are supplied by the caller.
//!
//! Persist MapPlan in the held operation journal BEFORE apply/restore. This is
//! not a transaction with the session databases. Backups are encrypted deltas,
//! never snapshots of unrelated entries, and remain available after recovery.
//! Existing maps are compared and updated under a TxF writer transaction, not
//! replaced after a racy pathname check. Unsupported TxF fails without fallback.
//! New maps use atomic no-replace publication. Neither operation loses a map
//! that an external writer published before our exclusive update began.

use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

const FORMAT: &str = "herdr 세션 홈 파일 형식을 확인할 수 없습니다";
const CONFLICT: &str = "herdr 세션 홈 연결이 변경되어 작업을 중단했습니다";
const BACKUP: &str = "herdr 세션 홈 백업을 확인할 수 없습니다";
#[cfg(windows)]
const TRANSACTION: &str = "herdr 세션 홈 파일을 안전한 트랜잭션으로 저장하지 못했습니다";
const LIMIT: u64 = 4 * 1024 * 1024;

// No Debug: plans contain home paths. Only the selected UUID is journaled.
#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct MapPlan {
    change: Change,
    backup_dir: PathBuf,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct Change {
    version: u32,
    path: PathBuf,
    id: String,
    before: Option<String>,
    after: String,
    map_was_missing: bool,
}

struct Document {
    value: Value,
    bytes: Option<Vec<u8>>,
    identity: Option<crate::sessions_guard::FileIdentity>,
    // Retain until deletion or acquisition of a transactional writer. The
    // latter rechecks this snapshot while holding actual filesystem exclusion.
    _file: Option<File>,
}

fn uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn explicit(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "herdr 세션 홈 경로는 절대 경로여야 합니다"
    );
    crate::sessions_guard::safe_path(path, path)
}

fn entry<'a>(value: &'a Value, id: &str) -> Result<Option<&'a str>> {
    let sessions = value
        .as_object()
        .and_then(|root| root.get("sessions"))
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!(FORMAT))?;
    match sessions.get(id) {
        None => Ok(None),
        Some(Value::String(path)) => Ok(Some(path)),
        Some(_) => Err(anyhow!(FORMAT)),
    }
}

#[cfg(windows)]
fn handle_identity(file: &File) -> Result<crate::sessions_guard::FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            GetFileInformationByHandle,
        },
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: File owns the live handle and info is a writable initialized buffer.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(|_| anyhow!(FORMAT))?;
    ensure!(
        info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | FILE_ATTRIBUTE_REPARSE_POINT.0) == 0,
        "{FORMAT}"
    );
    Ok(crate::sessions_guard::FileIdentity {
        volume: info.dwVolumeSerialNumber,
        index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

#[cfg(not(windows))]
fn handle_identity(_: &File) -> Result<crate::sessions_guard::FileIdentity> {
    Err(anyhow!("herdr 세션 홈 변경은 Windows에서만 지원됩니다"))
}

fn read(path: &Path) -> Result<Document> {
    explicit(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        };
        options
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_DELETE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Document {
                value: json!({"sessions": {}}),
                bytes: None,
                identity: None,
                _file: None,
            });
        }
        Err(_) => return Err(anyhow!("herdr 세션 홈 파일을 읽을 수 없습니다")),
    };
    let identity = handle_identity(&file)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("herdr 세션 홈 파일을 읽을 수 없습니다"))?;
    ensure!(bytes.len() as u64 <= LIMIT, "{FORMAT}");
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| anyhow!(FORMAT))?;
    ensure!(
        value
            .as_object()
            .and_then(|root| root.get("sessions"))
            .is_some_and(Value::is_object),
        "{FORMAT}"
    );
    Ok(Document {
        value,
        bytes: Some(bytes),
        identity: Some(identity),
        _file: Some(file),
    })
}

fn validate(plan: &MapPlan) -> Result<()> {
    ensure!(
        plan.change.version == 1 && uuid(&plan.change.id),
        "{BACKUP}"
    );
    ensure!(
        !plan.change.map_was_missing || plan.change.before.is_none(),
        "{BACKUP}"
    );
    explicit(&plan.change.path)?;
    explicit(Path::new(&plan.change.after))?;
    explicit(&plan.backup_dir)
}

fn backup_path(plan: &MapPlan) -> Result<PathBuf> {
    let bytes = serde_json::to_vec(&plan.change).map_err(|_| anyhow!(BACKUP))?;
    let hash = format!("{:x}", Sha256::digest(bytes));
    Ok(plan
        .backup_dir
        .join(format!("session-map-{}-{hash}.dpapi", plan.change.id)))
}

fn check_backup(plan: &MapPlan) -> Result<bool> {
    let path = backup_path(plan)?;
    crate::sessions_guard::safe_path(&plan.backup_dir, &path)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(anyhow!(BACKUP)),
    };
    ensure!(metadata.is_file() && metadata.len() <= LIMIT, "{BACKUP}");
    let mut encoded = String::new();
    File::open(path)
        .map_err(|_| anyhow!(BACKUP))?
        .take(LIMIT + 1)
        .read_to_string(&mut encoded)
        .map_err(|_| anyhow!(BACKUP))?;
    ensure!(encoded.len() as u64 <= LIMIT, "{BACKUP}");
    let bytes = crate::cutover::unprotect(&encoded).map_err(|_| anyhow!(BACKUP))?;
    let saved: Change = serde_json::from_slice(&bytes).map_err(|_| anyhow!(BACKUP))?;
    ensure!(saved == plan.change, "{BACKUP}");
    Ok(true)
}

fn save_backup(plan: &MapPlan) -> Result<()> {
    if !check_backup(plan)? {
        let bytes = serde_json::to_vec(&plan.change).map_err(|_| anyhow!(BACKUP))?;
        let protected = crate::cutover::protect(&bytes).map_err(|_| anyhow!(BACKUP))?;
        crate::cutover::atomic_bytes(&backup_path(plan)?, protected.as_bytes())
            .map_err(|_| anyhow!(BACKUP))?;
        ensure!(check_backup(plan)?, "{BACKUP}");
    }
    Ok(())
}

fn unchanged(plan: &MapPlan, original: &Document) -> Result<()> {
    let current = read(&plan.change.path)?;
    ensure!(
        current.identity == original.identity && current.bytes == original.bytes,
        "{CONFLICT}"
    );
    Ok(())
}

// Low-level handle rename; production publication below always passes flags 0.
#[cfg(windows)]
fn rename_file(file: &File, path: &Path, flags: u32) -> Result<()> {
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_RENAME_INFO, FileRenameInfoEx, SetFileInformationByHandle},
    };
    let name = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let name_bytes = name.len().checked_mul(2).ok_or_else(|| anyhow!(FORMAT))?;
    let length = u32::try_from(name_bytes)?;
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = offset
        .checked_add(name_bytes)
        .and_then(|size| size.checked_add(2))
        .ok_or_else(|| anyhow!(FORMAT))?;
    let buffer_size = u32::try_from(size)?;
    let mut buffer = vec![
        std::mem::MaybeUninit::<FILE_RENAME_INFO>::zeroed();
        size.div_ceil(std::mem::size_of::<FILE_RENAME_INFO>())
    ];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: aligned, zeroed storage covers the header and complete UTF-16
    // name. Only our create_new scratch handle is renamed, never reopened.
    unsafe {
        (*info).Anonymous.Flags = flags;
        (*info).RootDirectory = HANDLE::default();
        (*info).FileNameLength = length;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            info.cast::<u8>().add(offset).cast::<u16>(),
            name.len(),
        );
    }
    // SAFETY: file still owns the scratch handle with DELETE access and
    // the initialized rename buffer remains alive for the entire call.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfoEx,
            info.cast(),
            buffer_size,
        )
    }
    .map_err(|_| anyhow!("herdr 세션 홈 파일을 원자적으로 저장하지 못했습니다"))?;
    Ok(())
}

// Publish a missing map only; never replace an existing destination.
#[cfg(windows)]
fn publish(path: &Path, bytes: &[u8], before_publish: impl FnOnce() -> Result<()>) -> Result<()> {
    use std::{
        io::Write,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        sync::atomic::{AtomicU64, Ordering},
    };
    use windows::Win32::{
        Foundation::{GENERIC_WRITE, HANDLE},
        Storage::FileSystem::{
            DELETE, FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
        },
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    explicit(path)?;
    let parent = path.parent().ok_or_else(|| anyhow!(FORMAT))?;
    fs::create_dir_all(parent).map_err(|_| anyhow!("herdr 세션 홈 폴더를 만들 수 없습니다"))?;
    explicit(path)?;
    let mut file = None;
    for _ in 0..64 {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("qb-map-{}-{serial}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_WRITE.0 | DELETE.0)
            .share_mode(0)
            .open(&tmp)
        {
            Ok(created) => {
                file = Some(created);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(anyhow!("herdr 세션 홈 임시 파일을 만들 수 없습니다")),
        }
    }
    let mut file = file.ok_or_else(|| anyhow!("herdr 세션 홈 임시 파일 이름이 충돌했습니다"))?;
    let result = (|| {
        file.write_all(bytes)
            .map_err(|_| anyhow!("herdr 세션 홈 임시 파일을 쓰지 못했습니다"))?;
        file.sync_all()
            .map_err(|_| anyhow!("herdr 세션 홈 임시 파일을 저장하지 못했습니다"))?;
        before_publish()?;
        rename_file(&file, path, 0)?;
        Ok(())
    })();
    if result.is_err() {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: failure leaves our exclusive scratch handle unrenamed. Clean
        // up by handle, so no replacement at its former pathname is deleted.
        unsafe {
            SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileDispositionInfo,
                std::ptr::from_ref(&disposition).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        }
        .map_err(|_| anyhow!("herdr 세션 홈 저장 실패 후 임시 파일을 정리하지 못했습니다"))?;
    }
    result
}

#[cfg(not(windows))]
fn publish(_: &Path, _: &[u8], _: impl FnOnce() -> Result<()>) -> Result<()> {
    Err(anyhow!("herdr 세션 홈 변경은 Windows에서만 지원됩니다"))
}

// A deny-delete handle cannot protect an ordinary replacement by its own
// holder. Instead TxF supplies exclusion AND atomic multi-sector visibility.
// https://learn.microsoft.com/en-us/windows/win32/fileio/txf-basic-concepts
// No normal write/replace fallback is safe if this filesystem lacks TxF.
#[cfg(windows)]
fn update_existing(
    path: &Path,
    original: &mut Document,
    bytes: &[u8],
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use std::{
        io::{Seek, SeekFrom, Write},
        os::windows::{ffi::OsStrExt, io::FromRawHandle},
    };
    use windows::{
        Win32::{
            Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE},
            Storage::FileSystem::{
                CommitTransaction, CreateFileTransactedW, CreateTransaction,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, OPEN_EXISTING, RollbackTransaction,
            },
        },
        core::PCWSTR,
    };

    struct Transaction {
        handle: HANDLE,
        committed: bool,
    }
    impl Drop for Transaction {
        fn drop(&mut self) {
            // SAFETY: this object exclusively owns the valid KTM handle. Any
            // transacted File has been closed before this destructor runs.
            unsafe {
                if !self.committed {
                    let _ = RollbackTransaction(self.handle);
                }
                let _ = CloseHandle(self.handle);
            }
        }
    }

    explicit(path)?;
    // Release only our deny-write reader. A race in this gap is detected by
    // the ID + byte comparison INSIDE the transaction, before any modification.
    drop(original._file.take());
    // SAFETY: optional pointers are null, reserved fields are zero; bounded
    // five-second timeout. Existing windows features already expose KTM APIs.
    let handle = unsafe {
        CreateTransaction(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            0,
            0,
            5_000,
            PCWSTR::null(),
        )
    }
    .map_err(|_| anyhow!(TRANSACTION))?;
    let mut transaction = Transaction {
        handle,
        committed: false,
    };
    let name = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: NUL-terminated name lives through the call. OPEN_EXISTING never
    // creates/truncates. A writer locks against external modifications until
    // commit/rollback, including after the file handle is closed below.
    let handle = unsafe {
        CreateFileTransactedW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            None,
            transaction.handle,
            None,
            None,
        )
    }
    .map_err(|_| anyhow!(TRANSACTION))?;
    // SAFETY: successful CreateFileTransactedW transfers one owned handle.
    let mut file = unsafe { File::from_raw_handle(handle.0) };
    ensure!(
        Some(handle_identity(&file)?) == original.identity,
        "{CONFLICT}"
    );
    let mut current = Vec::new();
    Read::by_ref(&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut current)
        .map_err(|_| anyhow!(TRANSACTION))?;
    ensure!(
        original.bytes.as_deref() == Some(current.as_slice()),
        "{CONFLICT}"
    );
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(bytes))
        .and_then(|_| file.set_len(bytes.len() as u64))
        .and_then(|_| file.sync_all())
        .map_err(|_| anyhow!(TRANSACTION))?;
    // TxF requires closing file handles before commit. The transaction still
    // owns the filesystem lock: this is the crucial post-check race boundary.
    drop(file);
    before_commit()?;
    // SAFETY: the live transaction exclusively owns the validated update.
    unsafe { CommitTransaction(transaction.handle) }.map_err(|_| anyhow!(TRANSACTION))?;
    transaction.committed = true;
    Ok(())
}

#[cfg(not(windows))]
fn update_existing(
    _: &Path,
    _: &mut Document,
    _: &[u8],
    _: impl FnOnce() -> Result<()>,
) -> Result<()> {
    Err(anyhow!("herdr 세션 홈 변경은 Windows에서만 지원됩니다"))
}

fn write(plan: &MapPlan, original: &mut Document, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| anyhow!(FORMAT))?;
    bytes.push(b'\n');
    unchanged(plan, original)?;
    if original.identity.is_some() {
        update_existing(&plan.change.path, original, &bytes, || Ok(()))?;
    } else {
        publish(&plan.change.path, &bytes, || unchanged(plan, original))?;
    }
    let current = read(&plan.change.path)?;
    ensure!(current.value == *value, "{CONFLICT}");
    Ok(())
}

/// Pure planning: no map, directory, or backup is created. Existing files must
/// use {"sessions": {uuid: path}}; unknown sibling fields/entries are retained.
pub(crate) fn plan(path: &Path, id: &str, destination: &Path, backupdir: &Path) -> Result<MapPlan> {
    ensure!(uuid(id), "대화 ID는 UUID여야 합니다");
    explicit(destination)?;
    explicit(backupdir)?;
    let document = read(path)?;
    let before = entry(&document.value, id)?.map(str::to_owned);
    let plan = MapPlan {
        change: Change {
            version: 1,
            path: path.to_path_buf(),
            id: id.to_owned(),
            before,
            after: destination
                .to_str()
                .ok_or_else(|| anyhow!(FORMAT))?
                .to_owned(),
            map_was_missing: document.bytes.is_none(),
        },
        backup_dir: backupdir.to_path_buf(),
    };
    validate(&plan)?;
    Ok(plan)
}

/// Requires a persisted caller journal intent. Durable encrypted backup always
/// precedes publication. Replaying after either write is safe; a changed target
/// UUID is refused. Unrelated changes completed before this call are preserved.
/// Updating an existing map requires available local TxF support; otherwise
/// this returns an error without attempting an unprotected map write.
pub(crate) fn apply(plan: &MapPlan) -> Result<()> {
    validate(plan)?;
    let mut document = read(&plan.change.path)?;
    ensure!(
        plan.change.map_was_missing || document.bytes.is_some(),
        "{CONFLICT}"
    );
    let current = entry(&document.value, &plan.change.id)?;
    if current == Some(plan.change.after.as_str()) && current != plan.change.before.as_deref() {
        ensure!(check_backup(plan)?, "{BACKUP}");
        return verify(plan);
    }
    ensure!(current == plan.change.before.as_deref(), "{CONFLICT}");
    save_backup(plan)?;
    if current != Some(plan.change.after.as_str()) {
        let mut value = document.value.clone();
        value["sessions"].as_object_mut().unwrap().insert(
            plan.change.id.clone(),
            Value::String(plan.change.after.clone()),
        );
        write(plan, &mut document, &value)?;
    }
    verify(plan)
}

/// Restore this UUID alone, never a whole historical map. No-op before apply
/// and after completed restore. An originally absent map is removed only if
/// nothing except our generated empty sessions object remains.
pub(crate) fn restore(plan: &MapPlan) -> Result<()> {
    validate(plan)?;
    let mut document = read(&plan.change.path)?;
    ensure!(
        plan.change.map_was_missing || document.bytes.is_some(),
        "{CONFLICT}"
    );
    let current = entry(&document.value, &plan.change.id)?;
    if current == plan.change.before.as_deref() {
        return Ok(());
    }
    ensure!(current == Some(plan.change.after.as_str()), "{CONFLICT}");
    ensure!(check_backup(plan)?, "{BACKUP}");
    let mut value = document.value.clone();
    let sessions = value["sessions"].as_object_mut().unwrap();
    if let Some(before) = &plan.change.before {
        sessions.insert(plan.change.id.clone(), Value::String(before.clone()));
    } else {
        sessions.remove(&plan.change.id);
    }
    if plan.change.map_was_missing && value == json!({"sessions": {}}) {
        unchanged(plan, &document)?;
        crate::sessions_guard::remove_verified(
            plan.change.path.parent().ok_or_else(|| anyhow!(FORMAT))?,
            &plan.change.path,
            document.identity.ok_or_else(|| anyhow!(CONFLICT))?,
        )?;
    } else {
        write(plan, &mut document, &value)?;
    }
    let restored = read(&plan.change.path)?;
    ensure!(
        entry(&restored.value, &plan.change.id)? == plan.change.before.as_deref(),
        "{CONFLICT}"
    );
    Ok(())
}

/// Verify applied ownership and its durable delta backup. Does not modify files.
pub(crate) fn verify(plan: &MapPlan) -> Result<()> {
    validate(plan)?;
    let document = read(&plan.change.path)?;
    ensure!(
        document.bytes.is_some()
            && entry(&document.value, &plan.change.id)? == Some(plan.change.after.as_str()),
        "{CONFLICT}"
    );
    ensure!(check_backup(plan)?, "{BACKUP}");
    unchanged(plan, &document)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    const A: &str = "11111111-1111-1111-1111-111111111111";
    const B: &str = "22222222-2222-2222-2222-222222222222";
    const C: &str = "33333333-3333-3333-3333-333333333333";

    struct Fixture {
        _root: tempfile::TempDir,
        path: PathBuf,
        backup: PathBuf,
        destination: PathBuf,
        original_home: String,
    }

    impl Fixture {
        fn new() -> Self {
            let base =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/session-map-tests");
            fs::create_dir_all(&base).unwrap();
            let root = tempfile::tempdir_in(fs::canonicalize(base).unwrap()).unwrap();
            let path = root.path().join("appdata/herdr/session-homes.json");
            let backup = root.path().join("journal/map-backups");
            let destination = root.path().join("qb-home");
            let original_home = root.path().join("source-home").to_str().unwrap().to_owned();
            Self {
                _root: root,
                path,
                backup,
                destination,
                original_home,
            }
        }

        fn write(&self, value: &Value) {
            fs::create_dir_all(self.path.parent().unwrap()).unwrap();
            fs::write(&self.path, serde_json::to_vec(value).unwrap()).unwrap();
        }

        fn value(&self) -> Value {
            serde_json::from_slice(&fs::read(&self.path).unwrap()).unwrap()
        }

        fn plan(&self) -> MapPlan {
            plan(&self.path, A, &self.destination, &self.backup).unwrap()
        }

        fn original(&self) -> Value {
            json!({"sessions": {A: self.original_home, B: "SYNTHETIC_UNRELATED_PRIVATE_HOME"}, "future": {"keep": [1, true]}})
        }
    }

    // An independent noncooperating application, not a production safety bypass.
    fn publish_foreign(
        path: &Path,
        bytes: &[u8],
        before: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::DELETE;
        let scratch = tempfile::tempdir_in(path.parent().unwrap())?;
        let staged = scratch.path().join("foreign.json");
        publish(&staged, bytes, || Ok(()))?;
        let file = OpenOptions::new()
            .read(true)
            .access_mode(DELETE.0)
            .share_mode(0)
            .open(&staged)?;
        before()?;
        rename_file(&file, path, 3)
    }

    #[test]
    fn apply_verify_restore_changes_one_uuid_and_encrypts_only_delta() {
        let f = Fixture::new();
        let original = f.original();
        f.write(&original);
        let before = fs::read(&f.path).unwrap();
        let plan = f.plan();
        assert_eq!(fs::read(&f.path).unwrap(), before);
        assert!(!f.backup.exists());
        apply(&plan).unwrap();
        verify(&plan).unwrap();
        let applied = f.value();
        assert_eq!(applied["sessions"][A], f.destination.to_str().unwrap());
        assert_eq!(applied["sessions"][B], original["sessions"][B]);
        assert_eq!(applied["future"], original["future"]);
        let protected = fs::read_to_string(backup_path(&plan).unwrap()).unwrap();
        assert!(!protected.contains(&f.original_home));
        assert!(!protected.contains("SYNTHETIC_UNRELATED_PRIVATE_HOME"));
        let decrypted = crate::cutover::unprotect(&protected).unwrap();
        let saved: Value = serde_json::from_slice(&decrypted).unwrap();
        assert_eq!(saved["id"], A);
        assert_eq!(saved["before"], f.original_home);
        assert!(
            !String::from_utf8(decrypted)
                .unwrap()
                .contains("SYNTHETIC_UNRELATED_PRIVATE_HOME")
        );
        restore(&plan).unwrap();
        assert_eq!(f.value(), original);
        assert!(verify(&plan).is_err());
    }

    #[test]
    fn missing_map_is_created_only_on_apply_and_removed_on_restore() {
        let f = Fixture::new();
        let plan = f.plan();
        assert!(!f.path.exists());
        assert!(!f.path.parent().unwrap().exists());
        assert!(!f.backup.exists());
        restore(&plan).unwrap();
        assert!(!f.path.exists());
        apply(&plan).unwrap();
        assert_eq!(
            f.value(),
            json!({"sessions": {A: f.destination.to_str().unwrap()}})
        );
        restore(&plan).unwrap();
        assert!(!f.path.exists());
        restore(&plan).unwrap();
        assert!(check_backup(&plan).unwrap());
    }

    #[test]
    fn missing_uuid_and_unrelated_future_changes_survive_apply_and_restore() {
        let f = Fixture::new();
        f.write(&json!({"sessions": {B: "original-other"}, "future": {"revision": 1}}));
        let plan = f.plan();
        // An unrelated update completed after planning is merged on apply.
        f.write(&json!({"sessions": {B: "new-other", C: "new-third"}, "future": {"revision": 2}}));
        apply(&plan).unwrap();
        let mut latest = f.value();
        latest["sessions"][B] = json!("changed-again");
        latest["future"]["extra"] = json!(["preserve", {"unknown": true}]);
        f.write(&latest);
        apply(&plan).unwrap();
        verify(&plan).unwrap();
        restore(&plan).unwrap();
        latest["sessions"].as_object_mut().unwrap().remove(A);
        assert_eq!(f.value(), latest);
    }

    #[test]
    fn restore_new_map_keeps_unrelated_entries_and_unknown_fields() {
        let f = Fixture::new();
        let plan = f.plan();
        apply(&plan).unwrap();
        let mut latest = f.value();
        latest["sessions"][B] = json!("unrelated");
        latest["future"] = json!({"keep": true});
        f.write(&latest);
        restore(&plan).unwrap();
        latest["sessions"].as_object_mut().unwrap().remove(A);
        assert_eq!(f.value(), latest);
        restore(&plan).unwrap();
        assert_eq!(f.value(), latest);
    }

    #[test]
    fn unknown_structure_and_target_value_types_fail_without_writes() {
        let f = Fixture::new();
        for invalid in [
            json!([]),
            json!({}),
            json!({"sessions": null}),
            json!({"sessions": []}),
            json!({"sessions": {A: {"home": "unknown"}}}),
            json!({"sessions": {A: null}}),
        ] {
            f.write(&invalid);
            let bytes = fs::read(&f.path).unwrap();
            assert!(plan(&f.path, A, &f.destination, &f.backup).is_err());
            assert_eq!(fs::read(&f.path).unwrap(), bytes);
            assert!(!f.backup.exists());
        }
        fs::write(&f.path, b"{invalid json").unwrap();
        assert!(plan(&f.path, A, &f.destination, &f.backup).is_err());
        assert!(!f.backup.exists());
    }

    #[test]
    fn target_uuid_conflicts_and_missing_existing_map_fail_closed() {
        let f = Fixture::new();
        f.write(&f.original());
        let plan = f.plan();
        let conflicting = json!({"sessions": {A: "conflicting-home", B: "keep"}, "new": true});
        f.write(&conflicting);
        assert!(apply(&plan).is_err());
        assert!(!f.backup.exists());
        assert!(restore(&plan).is_err());
        assert_eq!(f.value(), conflicting);
        f.write(&f.original());
        apply(&plan).unwrap();
        f.write(&conflicting);
        assert!(apply(&plan).is_err());
        assert!(restore(&plan).is_err());
        assert!(verify(&plan).is_err());
        assert_eq!(f.value(), conflicting);
        fs::remove_file(&f.path).unwrap();
        assert!(apply(&plan).is_err());
        assert!(restore(&plan).is_err());
        assert!(!f.path.exists());
    }

    #[test]
    fn journal_reload_retries_each_backup_publication_and_restore_boundary() {
        let f = Fixture::new();
        let original = f.original();
        f.write(&original);
        let plan = f.plan();
        let journal = serde_json::to_vec(&plan.clone()).unwrap();
        let recovered: MapPlan = serde_json::from_slice(&journal).unwrap();
        restore(&recovered).unwrap(); // Interrupted before backup publication.
        assert_eq!(f.value(), original);
        save_backup(&recovered).unwrap(); // Interrupted after backup, before map.
        let encrypted = fs::read(backup_path(&recovered).unwrap()).unwrap();
        assert_eq!(f.value(), original);
        apply(&recovered).unwrap();
        let applied_id = crate::sessions_guard::identity(&f.path).unwrap();
        apply(&recovered).unwrap(); // Publication succeeded, journal ack was lost.
        assert_eq!(
            crate::sessions_guard::identity(&f.path).unwrap(),
            applied_id
        );
        assert_eq!(
            fs::read(backup_path(&recovered).unwrap()).unwrap(),
            encrypted
        );
        restore(&recovered).unwrap();
        let restored_id = crate::sessions_guard::identity(&f.path).unwrap();
        restore(&recovered).unwrap(); // Restore succeeded, journal ack was lost.
        assert_eq!(
            crate::sessions_guard::identity(&f.path).unwrap(),
            restored_id
        );
        assert_eq!(f.value(), original);
    }

    #[test]
    fn missing_or_corrupted_backup_never_authorizes_a_replay() {
        let f = Fixture::new();
        f.write(&f.original());
        let plan = f.plan();
        apply(&plan).unwrap();
        let applied = fs::read(&f.path).unwrap();
        let backup = backup_path(&plan).unwrap();
        fs::remove_file(&backup).unwrap();
        assert!(apply(&plan).is_err());
        assert!(restore(&plan).is_err());
        assert!(verify(&plan).is_err());
        fs::write(&backup, b"not DPAPI").unwrap();
        assert!(apply(&plan).is_err());
        assert!(restore(&plan).is_err());
        assert_eq!(fs::read(&f.path).unwrap(), applied);
        assert_eq!(fs::read(&backup).unwrap(), b"not DPAPI");
    }

    #[test]
    fn publication_refuses_document_replaced_after_read_snapshot() {
        let f = Fixture::new();
        f.write(&f.original());
        let plan = f.plan();
        let mut snapshot = read(&f.path).unwrap();
        let mut proposed = snapshot.value.clone();
        proposed["sessions"][A] = json!(plan.change.after);
        let mut concurrent = snapshot.value.clone();
        concurrent["sessions"][B] = json!("concurrent-other-home");
        publish_foreign(
            &f.path,
            &serde_json::to_vec(&concurrent).unwrap(),
            || Ok(()),
        )
        .unwrap();
        // Call the protected publisher directly, bypassing write's advisory
        // precheck: the comparison under transaction must detect replacement.
        let error = update_existing(
            &f.path,
            &mut snapshot,
            &serde_json::to_vec(&proposed).unwrap(),
            || panic!("changed snapshot must never reach commit"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), CONFLICT);
        assert_eq!(f.value(), concurrent);
        assert!(!f.backup.exists());
    }

    #[test]
    fn transaction_excludes_foreign_replace_after_final_check_through_commit_or_rollback() {
        for abort in [false, true] {
            let f = Fixture::new();
            let original = f.original();
            f.write(&original);
            let plan = f.plan();
            let mut snapshot = read(&f.path).unwrap();
            let mut proposed = snapshot.value.clone();
            proposed["sessions"][A] = json!(plan.change.after);
            proposed["future"]["multi_sector"] = json!("synthetic".repeat(2048));
            let mut concurrent = snapshot.value.clone();
            concurrent["sessions"][B] = json!("late-concurrent-other-home");
            concurrent["future"]["new"] = json!("preserve");
            let mut reached = false;
            let result = update_existing(
                &f.path,
                &mut snapshot,
                &serde_json::to_vec(&proposed).unwrap(),
                || {
                    reached = true;
                    // AFTER the final snapshot comparison and after closing the
                    // transacted file, immediately BEFORE commit. No extra path
                    // precheck can mask a successful competing atomic replacement.
                    assert!(
                        publish_foreign(&f.path, &serde_json::to_vec(&concurrent).unwrap(), || Ok(
                            ()
                        ),)
                        .is_err(),
                        "TxF must exclude a foreign POSIX replacement"
                    );
                    // Neither staged bytes nor a partially written JSON are visible.
                    assert_eq!(f.value(), original);
                    if abort {
                        Err(anyhow!("synthetic precommit interruption"))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(
                reached,
                "synthetic NTFS must support the protected publisher"
            );
            if abort {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "synthetic precommit interruption"
                );
                assert_eq!(f.value(), original);
            } else {
                result.unwrap();
                assert_eq!(f.value(), proposed);
            }
            // Rollback/commit released exclusion; a foreign writer can now publish.
            // Its committed B update must survive our stale retry, not be overwritten.
            publish_foreign(
                &f.path,
                &serde_json::to_vec(&concurrent).unwrap(),
                || Ok(()),
            )
            .unwrap();
            assert!(
                update_existing(
                    &f.path,
                    &mut snapshot,
                    &serde_json::to_vec(&proposed).unwrap(),
                    || Ok(())
                )
                .is_err()
            );
            assert_eq!(f.value(), concurrent);
            assert_eq!(fs::read_dir(f.path.parent().unwrap()).unwrap().count(), 1);
            assert!(!f.backup.exists());
        }
    }

    #[test]
    fn transaction_compares_bytes_even_when_file_identity_is_unchanged() {
        let f = Fixture::new();
        f.write(&f.original());
        let mut snapshot = read(&f.path).unwrap();
        let mut foreign = snapshot.value.clone();
        foreign["sessions"][B] = json!("in-place-foreign-update");
        drop(snapshot._file.take());
        f.write(&foreign);
        assert_eq!(
            Some(crate::sessions_guard::identity(&f.path).unwrap()),
            snapshot.identity
        );
        let error = update_existing(&f.path, &mut snapshot, b"{}", || {
            panic!("changed bytes must refuse")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), CONFLICT);
        assert_eq!(f.value(), foreign);
    }

    #[test]
    fn unavailable_transaction_writer_never_falls_back_to_replace() {
        let f = Fixture::new();
        f.write(&f.original());
        let original = f.value();
        let mut snapshot = read(&f.path).unwrap();
        // Our independent deny-write reader makes a TxF writer unavailable,
        // just as unsupported TxF must fail before any map modification.
        let blocker = read(&f.path).unwrap();
        assert!(
            update_existing(&f.path, &mut snapshot, b"{}", || panic!(
                "writer unavailable"
            ))
            .is_err()
        );
        assert_eq!(f.value(), original);
        drop(blocker);
        assert_eq!(fs::read_dir(f.path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn first_publication_never_overwrites_a_racing_map_creator() {
        let f = Fixture::new();
        fs::create_dir_all(f.path.parent().unwrap()).unwrap();
        let foreign = json!({"sessions":{B:"foreign-home"},"future":"preserve"});
        let result = publish(&f.path, b"{\"sessions\":{}}", || {
            fs::write(&f.path, serde_json::to_vec(&foreign).unwrap())?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(f.value(), foreign);
        assert_eq!(fs::read_dir(f.path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn already_correct_mapping_is_not_rewritten_on_apply_or_restore() {
        let f = Fixture::new();
        f.write(
            &json!({"sessions": {A: f.destination.to_str().unwrap(), B: "keep"}, "future": true}),
        );
        let before = fs::read(&f.path).unwrap();
        let identity = crate::sessions_guard::identity(&f.path).unwrap();
        let plan = f.plan();
        apply(&plan).unwrap();
        verify(&plan).unwrap();
        restore(&plan).unwrap();
        assert_eq!(fs::read(&f.path).unwrap(), before);
        assert_eq!(crate::sessions_guard::identity(&f.path).unwrap(), identity);
    }
}
