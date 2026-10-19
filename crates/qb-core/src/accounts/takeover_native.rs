//! Source retirement is handle-bound; process detection is deliberately best effort.
use super::takeover::{
    AsyncResult, Dependencies, Owner, ProfileIdentity, ProfileResolver, SourceAccess, SourceProof,
    SourceSnapshot, TakeoverError,
};
use super::{Identity, Provider};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use windows::Win32::{
    Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE},
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_RENAME_INFO, FileRenameInfo, GetFileInformationByHandle,
        SetFileInformationByHandle,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

const MAX_SOURCE: u64 = 2 * 1024 * 1024;
pub(crate) const PROFILE: &str = "https://api.anthropic.com/api/oauth/profile";

pub(crate) fn production(client: reqwest::Client) -> Dependencies {
    Dependencies {
        source: Arc::new(NativeSource),
        profile: Arc::new(SharedProfile {
            client,
            endpoint: PROFILE.into(),
        }),
    }
}

pub(crate) struct NativeSource;

/// Read-only recovery evidence. A fresh login must never finish a pending
/// rename merely to discover whether the previous attempt retired its source.
pub(crate) fn retirement_evidence(
    proof: &SourceProof,
    destination: &Path,
) -> Result<bool, TakeoverError> {
    if destination.parent() != proof.path.parent() {
        return Err(TakeoverError::Invalid);
    }
    match fs::symlink_metadata(destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(TakeoverError::Unavailable),
        Ok(_) => {}
    }
    let mut file = held(destination, false)?;
    let mut actual = snapshot(&mut file, destination)?.proof;
    actual.path = proof.path.clone();
    Ok(&actual == proof)
}

#[cfg(all(feature = "testkit", debug_assertions))]
pub(crate) fn synthetic(
    client: reqwest::Client,
    home: &Path,
    root: &Path,
    endpoint: String,
) -> Result<Dependencies, TakeoverError> {
    let root = validate_synthetic_inputs(home, root, &endpoint)?;
    Ok(Dependencies {
        source: Arc::new(SyntheticSource { root }),
        profile: Arc::new(SharedProfile { client, endpoint }),
    })
}

#[cfg(all(feature = "testkit", debug_assertions))]
pub(crate) fn validate_synthetic_inputs(
    home: &Path,
    root: &Path,
    endpoint: &str,
) -> Result<PathBuf, TakeoverError> {
    let sandbox = |path: &Path| {
        path.is_absolute()
            && path.components().any(|part| part.as_os_str() == ".sandbox")
            && !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
    };
    if !sandbox(home)
        || !sandbox(root)
        || !crate::oauth::claude_profile::synthetic_endpoint(endpoint)
    {
        return Err(TakeoverError::Invalid);
    }
    no_reparse(home)?;
    let canonical_home = fs::canonicalize(home).map_err(|_| TakeoverError::Invalid)?;
    if !sandbox(&canonical_home) || !canonical_home.is_dir() {
        return Err(TakeoverError::Invalid);
    }
    no_reparse(root)?;
    let root = fs::canonicalize(root).map_err(|_| TakeoverError::Invalid)?;
    if !sandbox(&root) || !root.is_dir() {
        return Err(TakeoverError::Invalid);
    }
    Ok(root)
}

#[cfg(all(feature = "testkit", debug_assertions))]
struct SyntheticSource {
    root: PathBuf,
}
#[cfg(all(feature = "testkit", debug_assertions))]
impl SourceAccess for SyntheticSource {
    fn snapshot(&self, provider: Provider, path: &Path) -> Result<SourceSnapshot, TakeoverError> {
        let requested = normalized(&path.to_string_lossy());
        let root = normalized(&self.root.to_string_lossy());
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            || !(requested == root || requested.starts_with(&(root + "\\")))
        {
            return Err(TakeoverError::Invalid);
        }
        no_reparse(path)?;
        let canonical = fs::canonicalize(path).map_err(|_| TakeoverError::Invalid)?;
        if !canonical.starts_with(&self.root) {
            return Err(TakeoverError::Invalid);
        }
        NativeSource.snapshot(provider, &canonical)
    }
    fn owner(&self, _provider: Provider, proof: &SourceProof) -> Owner {
        if proof.path.starts_with(&self.root) {
            Owner::Stopped
        } else {
            Owner::Unknown
        }
    }
    fn retire(
        &self,
        provider: Provider,
        proof: &SourceProof,
        destination: &Path,
        keep: bool,
    ) -> Result<(), TakeoverError> {
        if !proof.path.starts_with(&self.root) || !destination.starts_with(&self.root) {
            return Err(TakeoverError::Invalid);
        }
        NativeSource.retire(provider, proof, destination, keep)
    }
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

fn no_reparse(path: &Path) -> Result<(), TakeoverError> {
    if !path.is_absolute() {
        return Err(TakeoverError::Invalid);
    }
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|_| TakeoverError::Unavailable)?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(TakeoverError::Conflict);
        }
    }
    Ok(())
}

fn source_path(provider: Provider, path: &Path) -> Result<PathBuf, TakeoverError> {
    no_reparse(path)?;
    let name = if provider == Provider::Codex {
        "auth.json"
    } else {
        ".credentials.json"
    };
    let candidate = if path.is_dir() {
        path.join(name)
    } else {
        path.to_owned()
    };
    if candidate.file_name().is_none_or(|value| value != name) {
        return Err(TakeoverError::Invalid);
    }
    no_reparse(&candidate)?;
    let path = fs::canonicalize(candidate).map_err(|_| TakeoverError::Unavailable)?;
    no_reparse(&path)?;
    Ok(path)
}

fn held(path: &Path, delete: bool) -> Result<File, TakeoverError> {
    no_reparse(path)?;
    // Deny other writers and renamers while proving/retiring this exact object.
    OpenOptions::new()
        .read(true)
        .access_mode(0x8000_0000 | if delete { 0x0001_0000 } else { 0 })
        .share_mode(1)
        .custom_flags(0x0020_0000)
        .open(path)
        .map_err(|_| TakeoverError::Conflict)
}

fn snapshot(file: &mut File, path: &Path) -> Result<SourceSnapshot, TakeoverError> {
    let metadata = file.metadata().map_err(|_| TakeoverError::Unavailable)?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE {
        return Err(TakeoverError::Invalid);
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(|_| TakeoverError::Unavailable)?;
    if info.dwFileAttributes & 0x410 != 0 {
        return Err(TakeoverError::Conflict);
    }
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    file.take(MAX_SOURCE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| TakeoverError::Unavailable)?;
    if bytes.len() as u64 > MAX_SOURCE {
        return Err(TakeoverError::Invalid);
    }
    Ok(SourceSnapshot {
        proof: SourceProof {
            path: path.to_owned(),
            volume: info.dwVolumeSerialNumber,
            index: u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow),
            modified: metadata
                .modified()
                .map(DateTime::<Utc>::from)
                .map_err(|_| TakeoverError::Unavailable)?,
            sha256: format!("{:x}", Sha256::digest(bytes.as_slice())),
        },
        bytes,
    })
}

impl SourceAccess for NativeSource {
    fn snapshot(&self, provider: Provider, path: &Path) -> Result<SourceSnapshot, TakeoverError> {
        let path = source_path(provider, path)?;
        snapshot(&mut held(&path, false)?, &path)
    }
    fn owner(&self, provider: Provider, proof: &SourceProof) -> Owner {
        // Synthetic tests inject an owner probe and never enumerate real processes.
        if cfg!(any(test, feature = "testkit")) {
            return Owner::Unknown;
        }
        owner(provider, proof)
    }
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn retire(
        &self,
        _provider: Provider,
        proof: &SourceProof,
        destination: &Path,
        keep: bool,
    ) -> Result<(), TakeoverError> {
        if destination.parent() != proof.path.parent() {
            return Err(TakeoverError::Invalid);
        }
        if !proof
            .path
            .try_exists()
            .map_err(|_| TakeoverError::Unavailable)?
        {
            if keep {
                return Err(TakeoverError::Conflict);
            }
            let mut file = held(destination, false)?;
            let mut actual = snapshot(&mut file, destination)?.proof;
            actual.path = proof.path.clone();
            return if &actual == proof {
                Ok(())
            } else {
                Err(TakeoverError::Conflict)
            };
        }
        let mut file = held(&proof.path, !keep)?;
        if snapshot(&mut file, &proof.path)?.proof != *proof {
            return Err(TakeoverError::Conflict);
        }
        if keep {
            return Ok(());
        }
        // No replace: an existing destination, including an unrelated same-byte
        // object, survives. Rename through this validated source handle.
        let name: Vec<u16> = destination.as_os_str().encode_wide().collect();
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let size = offset
            // Win32 resolves the full path as a NUL-terminated string even
            // though FileNameLength excludes the terminator. Do not rely on
            // incidental padding from the u64 allocation for that NUL.
            .checked_add((name.len() + 1) * 2)
            .ok_or(TakeoverError::Invalid)?;
        let mut buffer = vec![0u64; size.div_ceil(8)];
        unsafe {
            let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            (*info).FileNameLength =
                u32::try_from(name.len() * 2).map_err(|_| TakeoverError::Invalid)?;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
                name.len(),
            );
            SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileRenameInfo,
                buffer.as_ptr().cast(),
                u32::try_from(size).map_err(|_| TakeoverError::Invalid)?,
            )
            .map_err(|error| {
                tracing::warn!(
                    windows_error = error.code().0,
                    "takeover source rename failed"
                );
                TakeoverError::Conflict
            })?;
        }
        Ok(())
    }
}

struct ProcessHandle(HANDLE);
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum: u16,
    buffer: *const u16,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        information: *mut std::ffi::c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
}
fn command_line(pid: u32) -> Option<String> {
    let process =
        ProcessHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? });
    let mut needed = 0;
    unsafe {
        NtQueryInformationProcess(process.0, 60, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed < std::mem::size_of::<UnicodeString>() as u32 || needed > 128 * 1024 {
        return None;
    }
    let mut data = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        NtQueryInformationProcess(process.0, 60, data.as_mut_ptr().cast(), needed, &mut needed)
    } < 0
    {
        return None;
    }
    let string = unsafe { &*data.as_ptr().cast::<UnicodeString>() };
    let start = data.as_ptr() as usize;
    let end = start.checked_add(data.len() * std::mem::size_of::<usize>())?;
    let address = string.buffer as usize;
    if string.length % 2 != 0
        || string.length > string.maximum
        || address < start
        || address.checked_add(string.length as usize)? > end
    {
        return None;
    }
    Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(string.buffer, string.length as usize / 2)
    }))
}
fn normalized(value: &str) -> String {
    value
        .replace("\\\\?\\", "")
        .replace('/', "\\")
        .to_lowercase()
}
fn owner(provider: Provider, proof: &SourceProof) -> Owner {
    let orca_home = proof.path.components().any(|part| {
        part.as_os_str()
            .to_string_lossy()
            .to_lowercase()
            .starts_with("orca")
    });
    let home = normalized(&proof.path.parent().unwrap_or(&proof.path).to_string_lossy());
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(handle) => ProcessHandle(handle),
        Err(_) => return Owner::Unknown,
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    let mut unknown = false;
    loop {
        if let Err(error) = next {
            return if error.code() == ERROR_NO_MORE_FILES.to_hresult() {
                if unknown {
                    Owner::Unknown
                } else {
                    Owner::Stopped
                }
            } else {
                Owner::Unknown
            };
        }
        let len = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
        if orca_home && name.starts_with("orca") {
            return Owner::Running;
        }
        let native = name == format!("{}.exe", provider.as_str());
        if native || name == "node.exe" {
            match command_line(entry.th32ProcessID) {
                Some(line) => {
                    let line = normalized(&line);
                    let is_client = native
                        || match provider {
                            Provider::Codex => {
                                line.contains("codex.js") || line.contains("@openai\\codex")
                            }
                            Provider::Claude => line.contains("claude-code\\cli.js"),
                        };
                    if is_client && line.contains(&home) {
                        return Owner::Running;
                    }
                    // A readable command line naming another home is not proof
                    // this process owns the selected one. Environment-only homes
                    // cannot be established by this best-effort probe; the user
                    // warning and 120-second source age guard still apply.
                }
                None if native => unknown = true,
                None => {}
            }
        }
        next = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
}

pub(crate) struct SharedProfile {
    pub client: reqwest::Client,
    pub endpoint: String,
}
impl ProfileResolver for SharedProfile {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }
    fn claude<'a>(&'a self, access: &'a str) -> AsyncResult<'a, ProfileIdentity> {
        Box::pin(async move {
            #[cfg(any(test, feature = "testkit"))]
            if !crate::oauth::claude_profile::synthetic_endpoint(&self.endpoint) {
                return Err(TakeoverError::Unavailable);
            }
            let mut response = self
                .client
                .get(&self.endpoint)
                .timeout(Duration::from_secs(10))
                .bearer_auth(access)
                .header(reqwest::header::ACCEPT, "application/json")
                .header("anthropic-beta", "oauth-2025-04-20")
                .send()
                .await
                .map_err(|_| TakeoverError::Unavailable)?;
            if !response.status().is_success() {
                return Err(TakeoverError::Invalid);
            }
            let mut bytes = zeroize::Zeroizing::new(Vec::new());
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| TakeoverError::Unavailable)?
            {
                if bytes.len() + chunk.len() > 1024 * 1024 {
                    return Err(TakeoverError::Invalid);
                }
                bytes.extend_from_slice(&chunk);
            }
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| TakeoverError::Invalid)?;
            let required = |object: &str, key: &str| {
                value[object][key]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 512)
                    .map(str::to_owned)
                    .ok_or(TakeoverError::Invalid)
            };
            let account_uuid = required("account", "uuid")?;
            let organization_uuid = required("organization", "uuid")?;
            let email = required("account", "email")?;
            Ok(ProfileIdentity {
                identity: Identity::Claude {
                    account_uuid,
                    organization_uuid: Some(organization_uuid),
                    email: email.clone(),
                },
                email,
                plan: crate::oauth::claude_profile::plan(&value).map(str::to_owned),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "testkit", debug_assertions))]
    fn junction(path: &Path, target: &Path) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn DeviceIoControl(
                handle: *mut std::ffi::c_void,
                code: u32,
                input: *const u8,
                size: u32,
                output: *mut u8,
                capacity: u32,
                returned: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
        }
        fs::create_dir(path).unwrap();
        let print = target
            .to_string_lossy()
            .trim_start_matches("\\\\?\\")
            .to_owned();
        let sub: Vec<u16> = format!("\\??\\{print}").encode_utf16().collect();
        let display: Vec<u16> = print.encode_utf16().collect();
        let mut data = Vec::new();
        data.extend_from_slice(&0xA0000003u32.to_le_bytes());
        data.extend_from_slice(&(8 + (sub.len() + display.len() + 2) as u16 * 2).to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        for n in [0, sub.len() * 2, (sub.len() + 1) * 2, display.len() * 2] {
            data.extend_from_slice(&(n as u16).to_le_bytes());
        }
        for word in sub.into_iter().chain([0]).chain(display).chain([0]) {
            data.extend_from_slice(&word.to_le_bytes());
        }
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(0x02200000)
            .open(path)
            .unwrap();
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                0x000900A4,
                data.as_ptr(),
                data.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "synthetic junction setup failed");
    }

    #[test]
    #[cfg(all(feature = "testkit", debug_assertions))]
    fn r6_test_hook_refuses_store_or_source_junction_and_ancestor_aliases_without_writes() {
        let (fixture, _, _) = fixture();
        let root = fs::canonicalize(fixture.path()).unwrap();
        let home = root.join("home");
        let source = root.join("source");
        let target = root.join("outside-fixture-allowlist");
        for path in [&home, &source, &target] {
            fs::create_dir(path).unwrap();
        }
        let link = root.join("alias");
        junction(&link, &target);
        fs::create_dir(target.join("child")).unwrap();
        let before: Vec<_> = fs::read_dir(&target)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let endpoint = "http://127.0.0.1:18432/profile";
        let outcomes = [
            validate_synthetic_inputs(&link, &source, endpoint).is_err(),
            validate_synthetic_inputs(&link.join("child"), &source, endpoint).is_err(),
            validate_synthetic_inputs(&home, &link, endpoint).is_err(),
            validate_synthetic_inputs(&home, &link.join("child"), endpoint).is_err(),
        ];
        let after: Vec<_> = fs::read_dir(&target)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        fs::remove_dir(&link).unwrap();
        assert!(outcomes.into_iter().all(|refused| refused));
        assert_eq!(before, after);
        assert!(validate_synthetic_inputs(&home, &source, endpoint).is_ok());
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, SourceSnapshot) {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r6-native-fixtures");
        fs::create_dir_all(&base).unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let path = home.path().join("auth.json");
        fs::write(&path, b"synthetic source bytes").unwrap();
        let before = NativeSource.snapshot(Provider::Codex, home.path()).unwrap();
        (home, path, before)
    }

    #[test]
    fn r6_native_inspection_is_readonly_and_retirement_renames_the_proven_handle() {
        let (home, source, before) = fixture();
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
        assert_eq!(fs::read(&source).unwrap(), before.bytes.as_slice());
        assert_eq!(
            NativeSource
                .snapshot(Provider::Codex, &source)
                .unwrap()
                .proof,
            before.proof
        );
        let destination = before
            .proof
            .path
            .with_file_name("auth.json.taken-over-by-quota-baton-20261019");
        NativeSource
            .retire(Provider::Codex, &before.proof, &destination, false)
            .unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), before.bytes.as_slice());
        // Resume after rename does not create another source or touch bytes.
        NativeSource
            .retire(Provider::Codex, &before.proof, &destination, false)
            .unwrap();
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    }

    #[test]
    fn r6_native_retirement_handles_a_name_without_incidental_buffer_padding() {
        let (_home, source, before) = fixture();
        let mut destination = before.proof.path.with_file_name("retired");
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        while (offset + destination.as_os_str().encode_wide().count() * 2) % 8 != 0 {
            let mut name = destination.file_name().unwrap().to_os_string();
            name.push("x");
            destination.set_file_name(name);
        }
        NativeSource
            .retire(Provider::Codex, &before.proof, &destination, false)
            .unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(destination).unwrap(), before.bytes.as_slice());
    }

    #[test]
    fn r6_native_retirement_never_overwrites_a_destination() {
        let (_home, source, before) = fixture();
        let destination = before
            .proof
            .path
            .with_file_name("auth.json.taken-over-by-quota-baton-20261019");
        fs::write(&destination, b"unrelated destination").unwrap();
        assert!(matches!(
            NativeSource.retire(Provider::Codex, &before.proof, &destination, false),
            Err(TakeoverError::Conflict)
        ));
        assert_eq!(fs::read(source).unwrap(), before.bytes.as_slice());
        assert_eq!(fs::read(destination).unwrap(), b"unrelated destination");
    }

    #[test]
    fn r6_retirement_recovery_evidence_never_renames_or_accepts_same_byte_substitution() {
        let (_home, source, before) = fixture();
        let destination = before
            .proof
            .path
            .with_file_name("auth.json.taken-over-by-quota-baton-20261019");
        assert!(!retirement_evidence(&before.proof, &destination).unwrap());
        assert!(source.exists());
        NativeSource
            .retire(Provider::Codex, &before.proof, &destination, false)
            .unwrap();
        assert!(retirement_evidence(&before.proof, &destination).unwrap());
        fs::write(&source, b"new independent source").unwrap();
        assert!(retirement_evidence(&before.proof, &destination).unwrap());
        assert_eq!(fs::read(&source).unwrap(), b"new independent source");
        let backup = destination.with_extension("proof-backup");
        fs::rename(&destination, backup).unwrap();
        fs::write(&destination, before.bytes.as_slice()).unwrap();
        assert!(!retirement_evidence(&before.proof, &destination).unwrap());
        assert_eq!(fs::read(&source).unwrap(), b"new independent source");
        assert_eq!(fs::read(&destination).unwrap(), before.bytes.as_slice());
    }

    #[test]
    fn r6_native_same_byte_substitution_and_post_retirement_new_source_are_preserved() {
        let (_home, source, before) = fixture();
        let destination = before
            .proof
            .path
            .with_file_name("auth.json.taken-over-by-quota-baton-20261019");
        let original = source.with_file_name("original-synthetic-backup");
        fs::rename(&source, &original).unwrap();
        fs::write(&source, before.bytes.as_slice()).unwrap();
        assert!(matches!(
            NativeSource.retire(Provider::Codex, &before.proof, &destination, false),
            Err(TakeoverError::Conflict)
        ));
        assert_eq!(fs::read(&source).unwrap(), before.bytes.as_slice());
        assert!(!destination.exists());
        fs::remove_file(&source).unwrap();
        fs::rename(&original, &source).unwrap();
        NativeSource
            .retire(Provider::Codex, &before.proof, &destination, false)
            .unwrap();
        fs::write(&source, b"new source family").unwrap();
        assert!(matches!(
            NativeSource.retire(Provider::Codex, &before.proof, &destination, false),
            Err(TakeoverError::Conflict)
        ));
        assert_eq!(fs::read(&source).unwrap(), b"new source family");
        assert_eq!(fs::read(destination).unwrap(), before.bytes.as_slice());
    }

    #[test]
    fn r6_native_keep_source_preserves_object_and_rechecks_its_contents() {
        let (_home, source, before) = fixture();
        let destination = before
            .proof
            .path
            .with_file_name("auth.json.taken-over-by-quota-baton-20261019");
        NativeSource
            .retire(Provider::Codex, &before.proof, &destination, true)
            .unwrap();
        assert_eq!(
            NativeSource
                .snapshot(Provider::Codex, &source)
                .unwrap()
                .proof,
            before.proof
        );
        assert!(!destination.exists());
        fs::write(&source, b"changed synthetic source").unwrap();
        assert!(matches!(
            NativeSource.retire(Provider::Codex, &before.proof, &destination, true),
            Err(TakeoverError::Conflict)
        ));
        assert_eq!(fs::read(source).unwrap(), b"changed synthetic source");
    }

    #[tokio::test]
    async fn r6_native_profile_production_endpoint_is_blocked_in_tests() {
        let profile = SharedProfile {
            client: reqwest::Client::new(),
            endpoint: PROFILE.into(),
        };
        assert!(matches!(
            profile.claude("synthetic-access").await,
            Err(TakeoverError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn r6_native_profile_uses_only_get_and_resolves_identity_from_a_bounded_loopback_response()
     {
        use axum::{Json, Router, http::HeaderMap, routing::get};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let app = Router::new().route("/profile", get(move |headers: HeaderMap| {
            let count = count.clone();
            async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer synthetic-access");
                assert_eq!(headers.get("anthropic-beta").unwrap(), "oauth-2025-04-20");
                count.fetch_add(1, Ordering::Relaxed);
                Json(serde_json::json!({"account":{"uuid":"synthetic-user","email":"example@example.invalid"},
                    "organization":{"uuid":"synthetic-org","rate_limit_tier":"default_claude_pro"}}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let profile = SharedProfile {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            endpoint: format!("http://{address}/profile"),
        };
        let result = profile.claude("synthetic-access").await.unwrap();
        assert_eq!(result.email, "example@example.invalid");
        assert_eq!(result.plan.as_deref(), Some("pro"));
        assert!(matches!(
            result.identity,
            Identity::Claude {
                organization_uuid: Some(_),
                ..
            }
        ));
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        server.abort();
    }
}
