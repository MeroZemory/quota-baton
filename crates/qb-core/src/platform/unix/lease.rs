use crate::accounts::StoreError;
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(crate) struct Lease {
    _file: File,
}
impl Lease {
    pub fn acquire(dir: &Path) -> Result<Self, StoreError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(".lock"))
            .map_err(|_| StoreError::Io)?;
        FileExt::try_lock_exclusive(&file).map_err(|_| StoreError::LeaseBusy)?;
        Ok(Self { _file: file })
    }
}
