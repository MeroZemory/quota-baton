use std::{fs, io, os::unix::fs::PermissionsExt};
use tempfile::TempDir;

/// The caller just created and checked this empty owned TempDir. This is fixture
/// preparation, never a permission-repair path for an existing product home.
pub(crate) fn prepare_new_fixture_directory(home: &TempDir) -> io::Result<()> {
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noncanonical_builder_input_is_retained_without_preparation() {
        // Unix tempfile preserves the lexical parent; Windows normalizes this
        // input before prepare_tempdir receives it, so this case lives here.
        let parent = tempfile::tempdir().unwrap();
        let real_parent = parent.path().canonicalize().unwrap().join("real-parent");
        fs::create_dir(&real_parent).unwrap();
        let aliased = tempfile::tempdir_in(real_parent.join("../real-parent")).unwrap();
        let original = aliased.path().to_owned();
        let error = crate::fixture::prepare_tempdir(aliased).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("input retained at"));
        assert!(original.is_dir());
        assert_eq!(fs::read_dir(&original).unwrap().count(), 0);
    }

    #[test]
    fn fresh_fixture_and_builder_leaf_are_private_without_changing_neighbors() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let neighbor = parent.path().join("intentionally-loose");
        fs::create_dir(&neighbor).unwrap();
        fs::set_permissions(&neighbor, fs::Permissions::from_mode(0o755)).unwrap();
        let ordinary = crate::fixture::tempdir_in(parent.path()).unwrap();
        let builder = tempfile::Builder::new()
            .prefix("bindings-")
            .tempdir_in(parent.path().canonicalize().unwrap())
            .unwrap();
        let original_path = builder.path().to_owned();
        let builder = crate::fixture::prepare_tempdir(builder).unwrap();
        assert_eq!(builder.path(), original_path);
        for home in [ordinary.path(), builder.path()] {
            assert_eq!(
                fs::symlink_metadata(home).unwrap().permissions().mode() & 0o7777,
                0o700
            );
        }
        for untouched in [parent.path(), neighbor.as_path()] {
            assert_eq!(
                fs::symlink_metadata(untouched)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                0o755
            );
        }
    }

    #[test]
    fn populated_fixture_retains_its_existing_mode_and_contents() {
        let parent = tempfile::tempdir().unwrap();
        let populated = tempfile::tempdir_in(parent.path().canonicalize().unwrap()).unwrap();
        let path = populated.path().to_owned();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(path.join("keep"), b"synthetic-owned-data").unwrap();
        let error = crate::fixture::prepare_tempdir(populated).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        assert_eq!(
            fs::read(path.join("keep")).unwrap(),
            b"synthetic-owned-data"
        );
    }
}
