use crate::accounts::StoreError;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::Path,
};
use windows::{
    Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FlushFileBuffers, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
    },
    core::PCWSTR,
};

pub(crate) fn atomic_write_account(
    dir: &Path,
    destination: &Path,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let path = dir.join(format!(".write-{:032x}.tmp", rand::random::<u128>()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| StoreError::Io)?;
    let result = (|| {
        file.write_all(bytes).map_err(|_| StoreError::Io)?;
        // 이동 전 파일 핸들을 명시적으로 flush한다.
        unsafe {
            FlushFileBuffers(HANDLE(file.as_raw_handle())).map_err(|_| StoreError::Io)?;
        }
        drop(file);
        let source: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
            .map_err(|_| StoreError::Io)
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result
}

pub(crate) fn read_takeover_bytes(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    use std::{
        io::Read,
        os::windows::fs::{MetadataExt, OpenOptionsExt},
    };
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(0x00200000)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(StoreError::Io),
    };
    let meta = file.metadata().map_err(|_| StoreError::Io)?;
    if !meta.is_file() || meta.file_attributes() & 0x410 != 0 || meta.len() > 4 * 1024 * 1024 {
        return Err(StoreError::InvalidData);
    }
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| StoreError::Io)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(StoreError::InvalidData);
    }
    Ok(Some(bytes))
}

pub(crate) fn atomic_write_binding(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{:032x}.tmp", rand::random::<u128>()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        let mut file = options.open(&tmp)?;
        crate::local_secrets::restrict(&tmp).map_err(std::io::Error::other)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::{
                Win32::Storage::FileSystem::{
                    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
                },
                core::PCWSTR,
            };
            let src: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
            let dst: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(src.as_ptr()),
                    PCWSTR(dst.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            }
            .map_err(std::io::Error::other)
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub(crate) fn atomic_write_observation(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("{:032x}.tmp", rand::random::<u128>()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::{
                Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW},
                core::PCWSTR,
            };
            let src: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
            let dst: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(src.as_ptr()),
                    PCWSTR(dst.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING,
                )
            }
            .map_err(std::io::Error::other)
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
