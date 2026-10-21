//! Per-UUID herdr map changes. All locations are supplied by the caller.
//!
//! Persist MapPlan in the held operation journal BEFORE apply/restore. This is
//! not a transaction with the session databases. Backups are encrypted deltas,
//! never snapshots of unrelated entries, and remain available after recovery.
//! Existing maps are compared and updated under a TxF writer transaction, not
//! replaced after a racy pathname check. Unsupported TxF fails without fallback.
//! New maps use atomic no-replace publication. Neither operation loses a map
//! that an external writer published before our exclusive update began.
//!
//! Only typed, post-decision resolution may use apply_resolution instead of
//! TxF. Persist replan_resolution's result first, then select the apply path
//! with a persisted caller mode (also after a caller-journal reload). Resolution
//! retains SHARE_NONE through the last write and journals the entire intended
//! document with DPAPI before truncating. A killed writer can leave a prefix;
//! repeat resolution to repair it. Replacement or unrecognized bytes refuse.
//! This relies on Windows file identity/creation time and successful flushes,
//! not on protection against a privileged writer forging identity/evidence.

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
const REPAIR_LIMIT: u64 = LIMIT * 4;
const REPAIR_INTENT: &str = "resolution-intent.dpapi";
const REPAIR_COMPLETE: &str = "resolution-complete.dpapi";
const RESOLUTION: &str = "herdr 세션 홈 명시적 복구 기록을 확인할 수 없습니다";

// No Debug: plans contain home paths. Plans contain selected-UUID deltas only;
// whole-document repair evidence lives separately, encrypted, in backup_dir.
#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct MapPlan {
    change: Change,
    backup_dir: PathBuf,
    // Preserve the original intent and every intervening encrypted delta.
    // Absence keeps old persisted plans on the ordinary TxF-only path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolution: Option<Box<MapPlan>>,
}

impl MapPlan {
    pub(crate) fn is_resolution(&self) -> bool {
        self.resolution.is_some()
    }
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct RepairIdentity {
    file: crate::sessions_guard::FileIdentity,
    created: u64,
}

#[derive(Deserialize, Serialize)]
struct RepairIntent {
    version: u8,
    plan_sha256: String,
    identity: RepairIdentity,
    original_len: u64,
    original_sha256: String,
    intended: String,
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
    let mut current = plan;
    let mut depth = 0;
    loop {
        validate_change(current)?;
        let Some(previous) = &current.resolution else {
            return Ok(());
        };
        depth += 1;
        ensure!(depth <= 32, "{RESOLUTION}");
        ensure!(
            current.change.path == previous.change.path
                && current.change.id == previous.change.id
                && current.change.after == previous.change.after
                && current.backup_dir.parent() == Some(previous.backup_dir.as_path()),
            "{RESOLUTION}"
        );
        current = previous;
    }
}

fn validate_change(plan: &MapPlan) -> Result<()> {
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
        let backup = backup_path(plan)?;
        let parent = backup.parent().ok_or_else(|| anyhow!(BACKUP))?;
        fs::create_dir_all(parent).map_err(|_| anyhow!(BACKUP))?;
        // The handle-based publisher uses a native Windows rename API. Give
        // it the extended canonical namespace even when APPDATA/QB_HOME came
        // from ordinary DOS paths; UUID+hash backup names can exceed MAX_PATH.
        let destination = fs::canonicalize(parent)
            .map_err(|_| anyhow!(BACKUP))?
            .join(backup.file_name().ok_or_else(|| anyhow!(BACKUP))?);
        crate::cutover::atomic_bytes(&destination, protected.as_bytes())
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

    #[cfg(test)]
    resolution_checkpoint("map_txf_writer")?;

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
        resolution: None,
    };
    validate(&plan)?;
    Ok(plan)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(sha256(
        &serde_json::to_vec(value).map_err(|_| anyhow!(RESOLUTION))?,
    ))
}

fn read_repair_marker<T: serde::de::DeserializeOwned>(
    plan: &MapPlan,
    name: &str,
) -> Result<Option<T>> {
    let path = plan.backup_dir.join(name);
    crate::sessions_guard::safe_path(&plan.backup_dir, &path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
        options
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(anyhow!(RESOLUTION)),
    };
    handle_identity(&file)?;
    let mut encoded = String::new();
    file.by_ref()
        .take(REPAIR_LIMIT + 1)
        .read_to_string(&mut encoded)
        .map_err(|_| anyhow!(RESOLUTION))?;
    ensure!(encoded.len() as u64 <= REPAIR_LIMIT, "{RESOLUTION}");
    let bytes = crate::cutover::unprotect(&encoded).map_err(|_| anyhow!(RESOLUTION))?;
    ensure!(bytes.len() as u64 <= REPAIR_LIMIT, "{RESOLUTION}");
    Ok(Some(
        serde_json::from_slice(&bytes).map_err(|_| anyhow!(RESOLUTION))?,
    ))
}

fn save_repair_marker(plan: &MapPlan, name: &str, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|_| anyhow!(RESOLUTION))?;
    ensure!(bytes.len() as u64 <= REPAIR_LIMIT, "{RESOLUTION}");
    let protected = crate::cutover::protect(&bytes).map_err(|_| anyhow!(RESOLUTION))?;
    ensure!(protected.len() as u64 <= REPAIR_LIMIT, "{RESOLUTION}");
    let path = fs::canonicalize(&plan.backup_dir)
        .map_err(|_| anyhow!(RESOLUTION))?
        .join(name);
    // Immutable, no-replace evidence. Never use the map's pathname for this
    // publication: the map itself stays open SHARE_NONE during every write.
    publish(&path, protected.as_bytes(), || {
        resolution_checkpoint(if name == REPAIR_INTENT {
            "resolution_intent_staged"
        } else {
            "resolution_complete_staged"
        })
    })
    .map_err(|error| error.context(RESOLUTION))
}

fn repair_intent(plan: &MapPlan) -> Result<Option<(RepairIntent, bool)>> {
    let intent: Option<RepairIntent> = read_repair_marker(plan, REPAIR_INTENT)?;
    let complete: Option<String> = read_repair_marker(plan, REPAIR_COMPLETE)?;
    let Some(intent) = intent else {
        ensure!(complete.is_none(), "{RESOLUTION}");
        return Ok(None);
    };
    ensure!(
        intent.version == 1
            && intent.plan_sha256 == fingerprint(plan)?
            && intent.original_len <= LIMIT
            && intent.original_sha256.len() == 64
            && intent
                .original_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            && intent.intended.len() as u64 <= LIMIT,
        "{RESOLUTION}"
    );
    let value: Value = serde_json::from_str(&intent.intended).map_err(|_| anyhow!(RESOLUTION))?;
    ensure!(
        entry(&value, &plan.change.id)? == Some(plan.change.after.as_str()),
        "{RESOLUTION}"
    );
    if let Some(saved) = &complete {
        ensure!(*saved == fingerprint(&intent)?, "{RESOLUTION}");
    }
    Ok(Some((intent, complete.is_some())))
}

fn save_resolution_backups(plan: &MapPlan) -> Result<()> {
    let mut current = Some(plan);
    while let Some(plan) = current {
        save_backup(plan)?;
        current = plan.resolution.as_deref();
    }
    Ok(())
}

fn check_resolution_backups(plan: &MapPlan) -> Result<()> {
    let mut current = Some(plan);
    while let Some(plan) = current {
        ensure!(check_backup(plan)?, "{BACKUP}");
        current = plan.resolution.as_deref();
    }
    Ok(())
}

/// Call only after typed `resolve`, after the publication decision. Re-read
/// the selected entry and allocate a fresh evidence namespace; no writes here.
/// Persist the returned plan in Move BEFORE apply_resolution. An interrupted
/// in-place write returns the existing plan so its full-document intent cannot
/// be discarded by replanning from a truncated or subsequently replaced map.
pub(crate) fn replan_resolution(old: &MapPlan) -> Result<MapPlan> {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    validate(old)?;
    if old.is_resolution() && repair_intent(old)?.is_some_and(|(_, complete)| !complete) {
        return Ok(old.clone());
    }
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!(RESOLUTION))?
        .as_nanos();
    for _ in 0..64 {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = old.backup_dir.join(format!(
            "resolve-{}-{time:x}-{serial:x}",
            std::process::id()
        ));
        if directory.try_exists().map_err(|_| anyhow!(RESOLUTION))? {
            continue;
        }
        let mut new = plan(
            &old.change.path,
            &old.change.id,
            Path::new(&old.change.after),
            &directory,
        )?;
        new.resolution = Some(Box::new(old.clone()));
        validate(&new)?;
        return Ok(new);
    }
    Err(anyhow!(RESOLUTION))
}

#[cfg(test)]
type ResolutionHook = Box<dyn FnMut(&str) -> Result<()>>;
#[cfg(test)]
thread_local! {
    static RESOLUTION_HOOK: std::cell::RefCell<Option<ResolutionHook>> = std::cell::RefCell::new(None);
}

fn resolution_checkpoint(label: &str) -> Result<()> {
    #[cfg(test)]
    RESOLUTION_HOOK.with(|hook| -> Result<()> {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(label)?;
        }
        Ok(())
    })?;
    #[cfg(not(test))]
    let _ = label;
    Ok(())
}

#[cfg(windows)]
fn repair_identity(file: &File) -> Result<RepairIdentity> {
    use std::os::windows::fs::MetadataExt;
    let identity = RepairIdentity {
        file: handle_identity(file)?,
        created: file
            .metadata()
            .map_err(|_| anyhow!(RESOLUTION))?
            .creation_time(),
    };
    ensure!(
        identity.file.index != 0 && identity.created != 0,
        "{RESOLUTION}"
    );
    Ok(identity)
}

#[cfg(windows)]
fn locked_bytes(file: &mut File) -> Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))
        .map_err(|_| anyhow!(RESOLUTION))?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!(RESOLUTION))?;
    ensure!(bytes.len() as u64 <= LIMIT, "{FORMAT}");
    Ok(bytes)
}

#[cfg(windows)]
fn prepare_resolution_parent(parent: &Path) -> Result<crate::sessions_guard::PublicationGuard> {
    explicit(parent)?;
    let mut root = parent;
    loop {
        match fs::symlink_metadata(root) {
            Ok(metadata) => {
                ensure!(metadata.is_dir(), "{RESOLUTION}");
                // Root must already exist: new map parents and backup trees
                // may have several missing components. The guard revalidates
                // every component by handle, pins the existing ancestors, and
                // creates only the suffix of this caller-authorized path. It
                // rejects reparse points and a drive-root ownership boundary.
                return crate::sessions_guard::prepare_parent(root, parent);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                root = root.parent().ok_or_else(|| anyhow!(RESOLUTION))?;
            }
            Err(error) => {
                return Err(anyhow!(
                    "herdr 세션 홈 복구 폴더 검사 실패 (OS 코드 {:?})",
                    error.raw_os_error()
                ));
            }
        }
    }
}

/// Explicit, forward-only repair when TxF is unavailable or the selected UUID
/// changed after the decision. Ordinary apply remains TxF-only. All unrelated
/// entries are read under SHARE_NONE and included in the durable DPAPI intent.
/// Process termination after truncation leaves only an intended prefix. Retry
/// with the same persisted plan; a different file identity or unexpected bytes
/// refuses without changing the map. Completed repairs never replay a snapshot
/// over subsequent unrelated edits. File identity plus creation time is used
/// across restarts; privileged identity/evidence forgery is outside this model.
#[cfg(windows)]
pub(crate) fn apply_resolution(plan: &MapPlan) -> Result<()> {
    use std::{
        io::{Seek, SeekFrom, Write},
        os::windows::fs::OpenOptionsExt,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
    };
    validate(plan)?;
    ensure!(plan.is_resolution(), "{RESOLUTION}");
    let parent = plan.change.path.parent().ok_or_else(|| anyhow!(FORMAT))?;
    let _map_parent = prepare_resolution_parent(parent)
        .map_err(|error| error.context("herdr 세션 홈 복구 대상 폴더 준비 실패"))?;
    let _backup_parent = prepare_resolution_parent(&plan.backup_dir)
        .map_err(|error| error.context("herdr 세션 홈 복구 백업 폴더 준비 실패"))?;
    let saved = repair_intent(plan)?;
    if saved.as_ref().is_some_and(|(_, complete)| *complete) {
        return verify(plan);
    }
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_WRITE_THROUGH.0)
        .open(&plan.change.path);
    let mut file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ensure!(
                saved.is_none() && plan.change.map_was_missing && plan.change.before.is_none(),
                "{CONFLICT}"
            );
            save_resolution_backups(plan)?;
            let mut value = json!({"sessions": {}});
            value["sessions"][&plan.change.id] = json!(plan.change.after);
            let bytes = serde_json::to_vec(&value).map_err(|_| anyhow!(FORMAT))?;
            publish(&plan.change.path, &bytes, || {
                resolution_checkpoint("resolution_before_publish")
            })?;
            resolution_checkpoint("resolution_published")?;
            return verify(plan);
        }
        Err(_) => return Err(anyhow!(CONFLICT)),
    };
    resolution_checkpoint("resolution_locked")?;
    let identity = repair_identity(&file)?;
    let current = locked_bytes(&mut file)?;
    let intent = if let Some((intent, _)) = saved {
        check_resolution_backups(plan)?;
        ensure!(intent.identity == identity, "{CONFLICT}");
        intent
    } else {
        let mut value: Value = serde_json::from_slice(&current).map_err(|_| anyhow!(FORMAT))?;
        let selected = entry(&value, &plan.change.id)?;
        if selected == Some(plan.change.after.as_str()) {
            if selected == plan.change.before.as_deref() {
                save_resolution_backups(plan)?;
            } else {
                check_resolution_backups(plan)?;
            }
            drop(file);
            return verify(plan);
        }
        ensure!(selected == plan.change.before.as_deref(), "{CONFLICT}");
        save_resolution_backups(plan)?;
        resolution_checkpoint("resolution_backups")?;
        value["sessions"][&plan.change.id] = json!(plan.change.after);
        let mut intended = serde_json::to_string_pretty(&value).map_err(|_| anyhow!(FORMAT))?;
        intended.push('\n');
        ensure!(intended.len() as u64 <= LIMIT, "{FORMAT}");
        let intent = RepairIntent {
            version: 1,
            plan_sha256: fingerprint(plan)?,
            identity,
            original_len: current.len() as u64,
            original_sha256: sha256(&current),
            intended,
        };
        save_repair_marker(plan, REPAIR_INTENT, &intent)?;
        resolution_checkpoint("resolution_journal")?;
        intent
    };
    let intended = intent.intended.as_bytes();
    let original =
        current.len() as u64 == intent.original_len && sha256(&current) == intent.original_sha256;
    ensure!(original || intended.starts_with(&current), "{CONFLICT}");
    let mut offset = current.len();
    if original && current.as_slice() != intended {
        // Truncate FIRST: a partial write can then only be an intended prefix,
        // never an ambiguous mix of intended bytes and old unrelated entries.
        file.set_len(0).map_err(|_| anyhow!(RESOLUTION))?;
        resolution_checkpoint("resolution_truncated")?;
        file.sync_all().map_err(|_| anyhow!(RESOLUTION))?;
        resolution_checkpoint("resolution_truncate_synced")?;
        offset = 0;
    }
    file.seek(SeekFrom::Start(offset as u64))
        .map_err(|_| anyhow!(RESOLUTION))?;
    while offset < intended.len() {
        let end = (offset + 4096).min(intended.len());
        let written = match file.write(&intended[offset..end]) {
            Ok(written) => written,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(anyhow!(RESOLUTION)),
        };
        ensure!(written > 0, "{RESOLUTION}");
        offset += written;
        resolution_checkpoint("resolution_write")?;
        file.sync_all().map_err(|_| anyhow!(RESOLUTION))?;
        resolution_checkpoint("resolution_write_synced")?;
    }
    file.sync_all().map_err(|_| anyhow!(RESOLUTION))?;
    ensure!(
        locked_bytes(&mut file)?.as_slice() == intended,
        "{CONFLICT}"
    );
    resolution_checkpoint("resolution_flushed")?;
    save_repair_marker(plan, REPAIR_COMPLETE, &fingerprint(&intent)?)?;
    resolution_checkpoint("resolution_completed")?;
    drop(file);
    verify(plan)
}

#[cfg(not(windows))]
pub(crate) fn apply_resolution(_: &MapPlan) -> Result<()> {
    Err(anyhow!("herdr 세션 홈 변경은 Windows에서만 지원됩니다"))
}

/// Requires a persisted caller journal intent. Durable encrypted backup always
/// precedes publication. Replaying after either write is safe; a changed target
/// UUID is refused. Unrelated changes completed before this call are preserved.
/// Updating an existing map requires available local TxF support; otherwise
/// this returns an error without attempting an unprotected map write.
pub(crate) fn apply(plan: &MapPlan) -> Result<()> {
    validate(plan)?;
    ensure!(!plan.is_resolution(), "{RESOLUTION}");
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
    ensure!(!plan.is_resolution(), "{RESOLUTION}");
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
    if plan.is_resolution() {
        if let Some((_, complete)) = repair_intent(plan)? {
            ensure!(complete, "{RESOLUTION}");
        }
        check_resolution_backups(plan)?;
    }
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
#[path = "sessions_map_resolve_tests.rs"]
mod resolve_tests;

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
    fn existing_map_accepts_nonverbatim_windows_paths() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut f = Fixture::new();
        let normal = |p: &Path| -> PathBuf {
            let w = p.as_os_str().encode_wide().collect::<Vec<_>>();
            assert!(p.to_string_lossy().starts_with(r"\\?\"));
            std::ffi::OsString::from_wide(&w[4..]).into()
        };
        f.path = normal(&f.path);
        f.backup = normal(&f.backup).join("synthetic-nested-backup-directory".repeat(3));
        f.destination = normal(&f.destination);
        f.write(&f.original());
        let p = f.plan();
        apply(&p).unwrap();
        verify(&p).unwrap();
        restore(&p).unwrap();
        assert_eq!(f.value(), f.original());
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
