//! Durable Codex client high-water mark. Call blocking I/O from spawn_blocking.
use std::{
    fmt,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        fn component(text: &str) -> Option<u64> {
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            text.parse().ok()
        }
        let mut parts = text.split('.');
        let version = Self {
            major: component(parts.next()?)?,
            minor: component(parts.next()?)?,
            patch: component(parts.next()?)?,
        };
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

pub(crate) const CODEX_CATALOG_CLIENT_VERSION_FLOOR: Version = Version {
    major: 0,
    minor: 170,
    patch: 0,
};
const RECORD: &str = "codex-catalog-client-version.txt";
const LOCK: &str = "codex-catalog-client-version.lock";

pub(crate) struct CatalogClientVersion {
    directory: PathBuf,
    highest: Mutex<Option<Version>>,
}

impl CatalogClientVersion {
    pub(crate) fn open(home: &Path) -> io::Result<Self> {
        let directory = home.join("state");
        let _lock = lock(&directory)?;
        let highest = read_record(&directory.join(RECORD))?;
        Ok(Self {
            directory,
            highest: Mutex::new(highest),
        })
    }

    pub(crate) fn effective(&self) -> Version {
        self.highest
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unwrap_or(CODEX_CATALOG_CLIENT_VERSION_FLOOR)
            .max(CODEX_CATALOG_CLIENT_VERSION_FLOOR)
    }

    /// True when the durable or this handle's actual high-water mark increases,
    /// including below floor and learning a newer record from another writer.
    /// Publish the in-memory value only after the atomic write succeeds.
    pub(crate) fn observe(&self, version: Version) -> io::Result<bool> {
        self.observe_with(version, crate::platform::atomic_write_binding)
    }

    fn observe_with(
        &self,
        version: Version,
        write: impl FnOnce(&Path, &[u8]) -> io::Result<()>,
    ) -> io::Result<bool> {
        let mut highest = self
            .highest
            .lock()
            .map_err(|_| io::Error::other("catalog client version lock poisoned"))?;
        let _lock = lock(&self.directory)?;
        let path = self.directory.join(RECORD);
        let recorded = read_record(&path)?;
        if recorded.is_none() && highest.is_some() {
            return Err(invalid_record());
        }
        let next = version
            .max(recorded.unwrap_or(version))
            .max(highest.unwrap_or(version));
        let changed = Some(next) > recorded;
        let learned = Some(next) > *highest;
        if changed {
            write(&path, next.to_string().as_bytes())?;
        }
        *highest = Some(next);
        Ok(changed || learned)
    }
}

fn lock(directory: &Path) -> io::Result<File> {
    crate::platform::ensure_private_dir_all(directory)?;
    // The existing native opener validates a private regular leaf.
    // Use our own leaf, independent of reset-credit operations and account leases.
    let file = crate::platform::open_reset_credit_lock(&directory.join(LOCK))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn read_record(path: &Path) -> io::Result<Option<Version>> {
    let Some(bytes) = crate::platform::read_takeover_bytes(path).map_err(io::Error::other)? else {
        return Ok(None);
    };
    // Three canonical u64 components (20 digits each) and two separators.
    if bytes.len() > 62 {
        return Err(invalid_record());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid_record())?;
    Version::parse(text).map(Some).ok_or_else(invalid_record)
}

fn invalid_record() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid catalog client version record",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/r31-version-tests");
        std::fs::create_dir_all(&base).unwrap();
        qb_testkit::fixture::tempdir_in(base).unwrap()
    }

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn strict_parser_rejects_incomplete_suffixes_whitespace_non_digits_and_overflow() {
        for text in [
            "",
            "0",
            "0.170",
            "0.170.0.1",
            ".170.0",
            "0..0",
            "0.170.",
            "v0.170.0",
            "0.170.0-beta",
            "0.170.0+build",
            " 0.170.0",
            "0.170.0 ",
            "0.170.0\n",
            "0.\t170.0",
            "+0.170.0",
            "0.-170.0",
            "0.170.０",
            "0.170.0\0",
            "18446744073709551616.0.0",
            "0.18446744073709551616.0",
            "0.0.18446744073709551616",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
        assert!(
            Version::parse("18446744073709551615.18446744073709551615.18446744073709551615")
                .is_some()
        );
    }

    #[test]
    fn ordering_is_numeric_and_display_is_canonical() {
        assert!(version("0.9.99") < version("0.10.0"));
        assert!(version("0.170.9") < version("0.170.10"));
        assert!(version("0.999.999") < version("1.0.0"));
        assert_eq!(version("000.0170.000").to_string(), "0.170.0");
        assert_eq!(version("0.0.0").to_string(), "0.0.0");
    }

    #[test]
    fn absent_record_uses_floor_but_below_floor_actual_observation_is_persisted() {
        let home = home();
        let store = CatalogClientVersion::open(home.path()).unwrap();
        assert_eq!(store.effective(), CODEX_CATALOG_CLIENT_VERSION_FLOOR);
        let path = store.directory.join(RECORD);
        assert!(!path.exists());
        assert!(store.observe(version("0.160.0")).unwrap());
        assert_eq!(read_record(&path).unwrap(), Some(version("0.160.0")));
        assert_eq!(store.effective(), CODEX_CATALOG_CLIENT_VERSION_FLOOR);
        assert!(!store.observe(version("0.159.9")).unwrap());
        assert!(store.observe(version("0.169.0")).unwrap());
        let reopened = CatalogClientVersion::open(home.path()).unwrap();
        assert_eq!(read_record(&path).unwrap(), Some(version("0.169.0")));
        assert_eq!(reopened.effective(), CODEX_CATALOG_CLIENT_VERSION_FLOOR);
    }

    #[test]
    fn restart_keeps_highest_and_older_or_equal_requests_do_not_rewrite() {
        let home = home();
        let store = CatalogClientVersion::open(home.path()).unwrap();
        assert!(store.observe(version("0.171.2")).unwrap());
        let reopened = CatalogClientVersion::open(home.path()).unwrap();
        assert_eq!(reopened.effective(), version("0.171.2"));
        for old in ["0.170.0", "0.171.1", "0.171.2"] {
            assert!(
                !reopened
                    .observe_with(version(old), |_, _| panic!("must not rewrite"))
                    .unwrap()
            );
        }
        assert_eq!(
            std::fs::read_to_string(store.directory.join(RECORD)).unwrap(),
            "0.171.2"
        );
        crate::platform::test_assert_snapshot_owner_only(&store.directory.join(RECORD));
        crate::platform::test_assert_snapshot_owner_only(&store.directory.join(LOCK));
    }

    #[test]
    fn corrupt_record_is_never_replaced_by_floor_or_new_observation() {
        let home = home();
        let store = CatalogClientVersion::open(home.path()).unwrap();
        store.observe(version("0.171.0")).unwrap();
        let path = store.directory.join(RECORD);
        for bytes in [b"".as_slice(), b"0.172.0\n", b"garbage", b"\xff"] {
            qb_testkit::fixture::write_private(&path, bytes).unwrap();
            assert_eq!(
                CatalogClientVersion::open(home.path())
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                store.observe(version("0.173.0")).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(store.effective(), version("0.171.0"));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn failed_atomic_write_does_not_publish_and_can_retry() {
        let home = home();
        let store = CatalogClientVersion::open(home.path()).unwrap();
        store.observe(version("0.171.0")).unwrap();
        assert!(
            store
                .observe_with(version("0.172.0"), |_, _| Err(
                    io::ErrorKind::PermissionDenied.into()
                ))
                .is_err()
        );
        assert_eq!(store.effective(), version("0.171.0"));
        assert_eq!(
            CatalogClientVersion::open(home.path()).unwrap().effective(),
            version("0.171.0")
        );
        assert!(store.observe(version("0.172.0")).unwrap());
        assert_eq!(
            CatalogClientVersion::open(home.path()).unwrap().effective(),
            version("0.172.0")
        );
    }

    #[test]
    fn record_larger_than_canonical_u64_triplet_is_rejected() {
        let home = home();
        let store = CatalogClientVersion::open(home.path()).unwrap();
        let bytes = format!("{}.0.0", "0".repeat(63));
        assert!(Version::parse(&bytes).is_some());
        let path = store.directory.join(RECORD);
        qb_testkit::fixture::write_private(&path, bytes.as_bytes()).unwrap();
        assert_eq!(
            CatalogClientVersion::open(home.path())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(store.observe(version("0.171.0")).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes.as_bytes());
    }

    #[test]
    fn disappearance_after_observation_is_an_error_without_memory_downgrade() {
        let home = home();
        let store = CatalogClientVersion::open(home.path()).unwrap();
        store.observe(version("0.171.0")).unwrap();
        std::fs::remove_file(store.directory.join(RECORD)).unwrap();
        assert_eq!(
            store.observe(version("0.170.0")).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(store.effective(), version("0.171.0"));
    }

    #[test]
    fn concurrent_independent_handles_cannot_overwrite_a_higher_version() {
        let home = home();
        let stale = CatalogClientVersion::open(home.path()).unwrap();
        let stores: Vec<_> = (171..179)
            .map(|minor| (minor, CatalogClientVersion::open(home.path()).unwrap()))
            .collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(stores.len()));
        let threads: Vec<_> = stores
            .into_iter()
            .map(|(minor, store)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.observe(version(&format!("0.{minor}.0"))).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            CatalogClientVersion::open(home.path()).unwrap().effective(),
            version("0.178.0")
        );
        assert!(stale.observe(version("0.171.0")).unwrap());
        assert_eq!(stale.effective(), version("0.178.0"));
        assert!(!stale.observe(version("0.171.0")).unwrap());
    }

    #[cfg(unix)]
    mod unix_tests;
}
