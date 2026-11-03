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
    fn fresh_fixture_and_builder_leaf_are_private_without_changing_neighbors() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let neighbor = parent.path().join("intentionally-loose");
        fs::create_dir(&neighbor).unwrap();
        fs::set_permissions(&neighbor, fs::Permissions::from_mode(0o755)).unwrap();
        let ordinary = crate::fixture::tempdir_in(parent.path()).unwrap();
        let builder = tempfile::Builder::new()
            .prefix("bindings-")
            .tempdir_in(parent.path())
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
}
