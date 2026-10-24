use super::private_fs;
use crate::accounts::StoreError;
use fs2::FileExt;
use std::{fs::File, io, path::Path};

pub(crate) struct Lease {
    _file: File,
}
impl Lease {
    pub(crate) fn retain_for_service(&self) -> anyhow::Result<()> {
        super::service::retain_lease_file(&self._file)
    }

    pub fn acquire(dir: &Path) -> Result<Self, StoreError> {
        let directory =
            private_fs::private_directory(dir, false).map_err(|_| StoreError::LeaseUnavailable)?;
        let file = private_fs::open_private_lock(&directory, c".lock")
            .map_err(|_| StoreError::LeaseUnavailable)?;
        FileExt::try_lock_exclusive(&file).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                StoreError::LeaseBusy
            } else {
                StoreError::LeaseUnavailable
            }
        })?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{private_fs::tests::fixture, secrets::prepare_account_directory};
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn contention_and_release_use_same_checked_lock_file() {
        let fixture = fixture();
        let directory = prepare_account_directory(fixture.path()).unwrap();
        let first = Lease::acquire(&directory).unwrap();
        assert!(matches!(
            Lease::acquire(&directory),
            Err(StoreError::LeaseBusy)
        ));
        drop(first);
        assert!(Lease::acquire(&directory).is_ok());
    }

    #[test]
    fn unsafe_existing_lock_is_unavailable_and_not_chmod_repaired() {
        let fixture = fixture();
        let directory = prepare_account_directory(fixture.path()).unwrap();
        drop(Lease::acquire(&directory).unwrap());
        let path = directory.join(".lock");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(
            Lease::acquire(&directory),
            Err(StoreError::LeaseUnavailable)
        ));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o666
        );
    }
}
