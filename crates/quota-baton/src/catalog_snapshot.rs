//! Exact authenticated catalog bytes, retained through the contained child tree.
//! The native catalog loader still needs the actual pinned-binary release gate.
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

#[cfg(windows)]
const MAX_BYTES: usize = 1024 * 1024;
const FAILURE: &str =
    "모델 목록 파일을 안전하게 준비하지 못했습니다. 전용 홈과 파일 접근 권한을 확인해 주세요.";

#[derive(Debug)]
struct SnapshotError;
impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(FAILURE)
    }
}
impl std::error::Error for SnapshotError {}

pub(crate) fn friendly_error(error: &anyhow::Error) -> Option<String> {
    error
        .downcast_ref::<SnapshotError>()
        .map(ToString::to_string)
}

pub(crate) struct CatalogSnapshot {
    path: PathBuf,
    // Neither a routing handle nor account identity is used in the filename.
    // Keep this private: callers cannot mutate the file after construction.
    _file: File,
    // Includes all ancestor pins plus a non-reparse publication anchor. File
    // closes/deletes first; containing namespace stays pinned until afterward.
    _parent: crate::sessions_guard::PublicationGuard,
}

impl CatalogSnapshot {
    pub(crate) fn create(qb_home: &Path, bytes: &[u8]) -> anyhow::Result<Self> {
        Self::create_inner(qb_home, bytes).map_err(|_| SnapshotError.into())
    }

    #[cfg(not(windows))]
    fn create_inner(_: &Path, _: &[u8]) -> io::Result<Self> {
        // The shared-file contract below is Windows-specific. Do not silently
        // expose a mutable pathname on other platforms.
        Err(io::ErrorKind::Unsupported.into())
    }

    #[cfg(windows)]
    fn create_inner(qb_home: &Path, bytes: &[u8]) -> io::Result<Self> {
        use std::{
            fs::OpenOptions,
            io::Write,
            sync::atomic::{AtomicU64, Ordering},
            time::{SystemTime, UNIX_EPOCH},
        };
        static NEXT: AtomicU64 = AtomicU64::new(0);
        if !qb_home.is_absolute() || bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let directory = qb_home.join("state").join("launch-catalogs");
        let parent = crate::sessions_guard::prepare_parent(qb_home, &directory)
            .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
        let directory = directory.canonicalize()?;
        for _ in 0..8 {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| io::ErrorKind::InvalidData)?
                .as_nanos();
            let path = directory.join(format!(
                "catalog-{}-{time}-{}.json",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = OpenOptions::new();
            options.create_new(true).read(true).write(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options
                    .access_mode(0xC001_0000) // GENERIC_READ | GENERIC_WRITE | DELETE
                    .share_mode(1) // readers only; deny writes and replacement
                    .custom_flags(0x0400_0000); // FILE_FLAG_DELETE_ON_CLOSE
            }
            let mut file = match options.open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            file.write_all(bytes)?;
            file.sync_all()?;
            return Ok(Self {
                path,
                _file: file,
                _parent: parent,
            });
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[test]
    fn exact_raw_bytes_and_separate_snapshots_are_retained_until_drop() {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r11-catalog-snapshot-tests");
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let raw = br#"{ "models":[], "capabilities": {"opaque": [1,2]} }"#;
        let first = CatalogSnapshot::create(home.path(), raw).unwrap();
        let second = CatalogSnapshot::create(home.path(), b"{}").unwrap();
        assert_ne!(first.path(), second.path());
        assert_eq!(std::fs::read(first.path()).unwrap(), raw);
        assert_eq!(std::fs::read(second.path()).unwrap(), b"{}");
        let path = first.path().to_owned();
        drop(first);
        assert!(!path.exists());
        assert!(second.path().exists());
    }

    #[cfg(windows)]
    #[test]
    fn live_snapshot_denies_writes_deletion_and_parent_replacement() {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r11-catalog-snapshot-tests");
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        let snapshot = CatalogSnapshot::create(home.path(), b"{}").unwrap();
        assert!(
            OpenOptions::new()
                .write(true)
                .open(snapshot.path())
                .is_err()
        );
        assert!(std::fs::remove_file(snapshot.path()).is_err());
        let parent = snapshot.path().parent().unwrap();
        assert!(std::fs::rename(parent, parent.with_file_name("moved")).is_err());
        assert_eq!(std::fs::read(snapshot.path()).unwrap(), b"{}");
    }
    #[test]
    fn invalid_inputs_are_fixed_errors_without_paths_or_sources() {
        let error = CatalogSnapshot::create(Path::new("sensitive-relative-home"), b"{}")
            .err()
            .unwrap();
        assert_eq!(error.to_string(), FAILURE);
        assert_eq!(error.chain().count(), 1);
        assert!(!format!("{error:?}").contains("sensitive"));
    }

    #[test]
    fn bounds_reject_empty_and_oversized_without_publishing_files() {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r11-catalog-snapshot-tests");
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        for bytes in [Vec::new(), vec![b' '; MAX_BYTES + 1]] {
            assert!(CatalogSnapshot::create(home.path(), &bytes).is_err());
        }
        assert!(!home.path().join("state/launch-catalogs").exists());
    }

    #[test]
    fn canonical_fixture_does_not_allow_parent_traversal() {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r11-catalog-snapshot-tests");
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let home = tempfile::tempdir_in(base).unwrap();
        // Joining onto a Windows verbatim path normalizes `..` in PathBuf,
        // before the API can inspect it. Supply the actual lexical traversal.
        let plain = home.path().to_str().unwrap().strip_prefix(r"\\?\").unwrap();
        let traversal = PathBuf::from(format!(r"{plain}\child\.."));
        assert!(
            traversal
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        );
        let error = CatalogSnapshot::create(&traversal, b"{}").err().unwrap();
        assert_eq!(error.to_string(), FAILURE);
        assert_eq!(error.chain().count(), 1);
        assert!(!home.path().join("child").exists());
        assert!(!home.path().join("state").exists());
    }
}
