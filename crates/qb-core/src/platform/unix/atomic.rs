use crate::accounts::StoreError;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("{:032x}.tmp", rand::random::<u128>()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        File::open(
            path.parent()
                .ok_or_else(|| io::Error::other("missing state directory"))?,
        )?
        .sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
pub(crate) fn atomic_write_account(
    _: &Path,
    destination: &Path,
    bytes: &[u8],
) -> Result<(), StoreError> {
    replace(destination, bytes).map_err(|_| StoreError::Io)
}
pub(crate) fn atomic_write_binding(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace(path, bytes)
}
pub(crate) fn atomic_write_observation(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace(path, bytes)
}
pub(crate) fn read_takeover_bytes(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(StoreError::Io),
    };
    let metadata = file.metadata().map_err(|_| StoreError::Io)?;
    if !metadata.is_file() || metadata.len() > 4 * 1024 * 1024 {
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
