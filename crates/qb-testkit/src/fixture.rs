//! Positive filesystem fixtures. Never use these to normalize an intentionally
//! unsafe path, replace an invalid-permission record, or bypass a security test.
use std::{io, io::Write, path::Path};
use tempfile::{NamedTempFile, TempDir};

/// The caller creates the parent; missing parents remain errors. Return physical
/// paths so a manifest-relative `..` does not become a credential-store input.
pub fn tempdir_in(parent: impl AsRef<Path>) -> io::Result<TempDir> {
    tempfile::tempdir_in(parent.as_ref().canonicalize()?)
}

/// A fresh owned home explicitly opts into local file key storage. The product's
/// Unix default remains systemd credentials; no machine credential is fabricated.
pub fn home_in(parent: impl AsRef<Path>) -> io::Result<TempDir> {
    let home = tempdir_in(parent)?;
    configure_home(home.path())?;
    Ok(home)
}

/// Configure a prepared new private home, refusing to overwrite any config.
pub fn configure_home(home: impl AsRef<Path>) -> io::Result<()> {
    let home = home.as_ref().canonicalize()?;
    let mut config = NamedTempFile::new_in(&home)?;
    config.write_all(b"[secrets]\nlinux_backend = \"file-0600\"\n")?;
    config
        .persist_noclobber(home.join("config.toml"))
        .map_err(|error| error.error)?;
    Ok(())
}

/// Write an ordinary positive record privately. The parent must already exist;
/// deliberate aliases, hardlinks and malformed-permission fixtures stay explicit.
pub fn write_private(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
        .canonicalize()?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut file = NamedTempFile::new_in(&parent)?;
    file.write_all(bytes)?;
    file.persist(parent.join(name))
        .map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_home_refuses_existing_config_and_missing_parent() {
        let parent = tempfile::tempdir().unwrap();
        assert_eq!(
            home_in(parent.path().join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let home = home_in(parent.path()).unwrap();
        let path = home.path().join("config.toml");
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            configure_home(home.path()).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn ordinary_fixture_paths_are_physical_before_native_validation() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::create_dir(parent.path().join("leaf")).unwrap();
        let home = home_in(parent.path().join("leaf/../leaf")).unwrap();
        assert_eq!(home.path(), home.path().canonicalize().unwrap());
        assert!(
            !home
                .path()
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        );
    }
}
