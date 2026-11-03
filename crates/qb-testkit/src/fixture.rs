//! Positive filesystem fixtures. Never use these to normalize an intentionally
//! unsafe path, replace an invalid-permission record, or bypass a security test.
use std::{io, io::Write, path::Path};
use tempfile::{NamedTempFile, TempDir};

/// The caller creates the parent; missing parents remain errors. Return physical
/// paths so a manifest-relative `..` does not become a credential-store input.
pub fn tempdir_in(parent: impl AsRef<Path>) -> io::Result<TempDir> {
    prepare_tempdir(tempfile::tempdir_in(parent.as_ref().canonicalize()?)?)
}

/// Prepare only a newly created, empty owned fixture directory at a canonical
/// path. Canonicalize a Builder's parent before creating it. Validation failures
/// retain the input directory unchanged; the caller remains responsible for its
/// cleanup. Never pass a populated home or an unsafe-path fixture.
pub fn prepare_tempdir(home: TempDir) -> io::Result<TempDir> {
    let validation = (|| -> io::Result<()> {
        if !std::fs::symlink_metadata(home.path())?.file_type().is_dir()
            || home.path() != home.path().canonicalize()?
            || std::fs::read_dir(home.path())?
                .next()
                .transpose()?
                .is_some()
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(())
    })();
    if let Err(error) = validation {
        let path: String = home
            .path()
            .to_string_lossy()
            .chars()
            .take(256)
            .flat_map(char::escape_default)
            .collect();
        let _retained_path = home.keep();
        return Err(io::Error::new(
            error.kind(),
            format!(
                "fixture directory must be a canonical, empty real directory; input retained at '{path}'"
            ),
        ));
    }
    crate::platform::prepare_new_fixture_directory(&home)?;
    Ok(home)
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

    #[test]
    fn builder_prefix_and_canonical_path_are_preserved() {
        let parent = tempfile::tempdir().unwrap();
        let builder = tempfile::Builder::new()
            .prefix("positive-builder-")
            .tempdir_in(parent.path().canonicalize().unwrap())
            .unwrap();
        let original = builder.path().to_owned();
        let prepared = prepare_tempdir(builder).unwrap();
        assert_eq!(prepared.path(), original);
        assert_eq!(prepared.path(), prepared.path().canonicalize().unwrap());
        assert!(
            prepared
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("positive-builder-")
        );
        assert_eq!(std::fs::read_dir(prepared.path()).unwrap().count(), 0);
    }

    #[test]
    fn populated_inputs_are_retained_without_preparation() {
        let parent = tempfile::tempdir().unwrap();
        let real_parent = parent.path().canonicalize().unwrap().join("real-parent");
        std::fs::create_dir(&real_parent).unwrap();
        let populated = tempfile::tempdir_in(&real_parent).unwrap();
        let path = populated.path().to_owned();
        std::fs::write(path.join("keep"), b"synthetic-owned-data").unwrap();
        let error = prepare_tempdir(populated).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("input retained at"));
        assert_eq!(
            std::fs::read(path.join("keep")).unwrap(),
            b"synthetic-owned-data"
        );
    }

    #[test]
    fn configured_home_contains_only_config_and_private_writes_replace_records() {
        let parent = tempfile::tempdir().unwrap();
        let home = home_in(parent.path()).unwrap();
        let entries: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from("config.toml")]);
        let record = home.path().join("record");
        write_private(&record, b"first").unwrap();
        write_private(&record, b"second").unwrap();
        assert_eq!(std::fs::read(record).unwrap(), b"second");
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 2);
    }
}
