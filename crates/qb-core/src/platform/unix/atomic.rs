use super::{private_fs, secrets::read_private_bytes};
use crate::accounts::StoreError;
use std::{io, path::Path};

fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (directory, name) = private_fs::parent_and_name(path)?;
    private_fs::replace_private_leaf(&directory, &name, bytes)
}

pub(crate) fn atomic_write_account(
    _: &Path,
    destination: &Path,
    bytes: &[u8],
) -> Result<(), StoreError> {
    // Settings may target home/config.toml, not the supplied accounts directory.
    replace(destination, bytes).map_err(|_| StoreError::Io)
}
pub(crate) fn atomic_write_binding(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace(path, bytes)
}
pub(crate) fn atomic_write_observation(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace(path, bytes)
}
pub(crate) fn read_takeover_bytes(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match read_private_bytes(path, 4 * 1024 * 1024) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Err(StoreError::InvalidData),
        Err(_) => Err(StoreError::Io),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        private_fs::tests::fixture,
        secrets::{create_private_secret, prepare_account_directory},
    };
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    #[test]
    fn replacement_uses_actual_config_parent_and_preserves_0600() {
        let fixture = fixture();
        let accounts = prepare_account_directory(fixture.path()).unwrap();
        let config = fixture.path().join("config.toml");
        atomic_write_account(&accounts, &config, b"first").unwrap();
        atomic_write_account(&accounts, &config, b"second").unwrap();
        assert_eq!(fs::read(&config).unwrap(), b"second");
        assert_eq!(
            fs::metadata(config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 2);
    }

    #[test]
    fn replacement_refuses_public_or_symlink_destinations_without_modifying_them() {
        let fixture = fixture();
        let target = fixture.path().join("record");
        create_private_secret(&target, b"original").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(atomic_write_binding(&target, b"changed").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = fixture.path().join("alias");
        symlink(&target, &alias).unwrap();
        assert!(atomic_write_observation(&alias, b"changed").is_err());
        assert_eq!(fs::read(target).unwrap(), b"original");
    }
}
