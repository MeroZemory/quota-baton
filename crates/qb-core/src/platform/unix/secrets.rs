use super::private_fs;
use crate::accounts::StoreError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    fs, io,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

pub(crate) fn configure_secret_open(options: &mut fs::OpenOptions) {
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
}

pub(crate) fn prepare_runtime_home(home: &Path) -> io::Result<()> {
    let directory = private_fs::prepare_private_directory(home)?;
    // Logging initializes before AccountStore and must not mkdir these at 0755.
    private_fs::prepare_private_child(&directory, "logs")?;
    Ok(())
}

pub(crate) fn prepare_secret_home(home: &Path) -> io::Result<()> {
    prepare_runtime_home(home)
}

pub(crate) fn prepare_account_directory(home: &Path) -> Result<PathBuf, StoreError> {
    let directory = private_fs::prepare_private_directory(home).map_err(|_| StoreError::Io)?;
    private_fs::prepare_private_child(&directory, "accounts").map_err(|_| StoreError::Io)?;
    Ok(home.join("accounts"))
}

pub(crate) fn read_private_bytes(path: &Path, max: usize) -> io::Result<Vec<u8>> {
    let (directory, name) = private_fs::parent_and_name(path)?;
    let mut bytes = private_fs::read_private_leaf(&directory, &name, max)?;
    Ok(std::mem::take(&mut *bytes))
}

pub(crate) fn read_private_text(path: &Path, max: usize) -> io::Result<Option<String>> {
    let bytes = match read_private_bytes(path, max) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // Convert by borrowing a zeroizing buffer so invalid UTF-8 is not retained
    // in a FromUtf8Error or included in a diagnostic.
    let bytes = Zeroizing::new(bytes);
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid private text encoding"))?;
    Ok(Some(text.to_owned()))
}

pub(crate) fn read_config(home: &Path) -> io::Result<Option<String>> {
    read_private_text(&home.join("config.toml"), 4 * 1024 * 1024)
}

pub(crate) fn create_private_secret(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (directory, name) = private_fs::parent_and_name(path)?;
    private_fs::create_private_leaf(&directory, &name, bytes)
}

pub(crate) fn validate_private_file(path: &Path) -> io::Result<()> {
    let (directory, name) = private_fs::parent_and_name(path)?;
    private_fs::validate_leaf(&directory, &name)
}

pub(crate) fn restrict(path: &Path) -> anyhow::Result<()> {
    validate_private_file(path)?;
    Ok(())
}

pub(crate) fn load_existing_secret_file(path: &Path) -> anyhow::Result<String> {
    load_existing_secret_file_if_present(path)?
        .ok_or_else(|| anyhow::anyhow!("local secret is missing"))
}

pub(crate) fn load_secret_file(path: &Path) -> anyhow::Result<String> {
    // Check the full safe read path before deciding whether creation is allowed.
    if let Some(value) = load_existing_secret_file_if_present(path)? {
        return Ok(value);
    }
    let mut bytes = Zeroizing::new([0u8; 32]);
    SystemRandom::new()
        .fill(&mut bytes[..])
        .map_err(|_| anyhow::anyhow!("secure random generation failed"))?;
    let value = Zeroizing::new(URL_SAFE_NO_PAD.encode(&bytes[..]));
    match create_private_secret(path, value.as_bytes()) {
        Ok(()) => Ok(value.to_string()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            load_existing_secret_file(path)
        }
        Err(error) => Err(error.into()),
    }
}

fn load_existing_secret_file_if_present(path: &Path) -> anyhow::Result<Option<String>> {
    // Keep NotFound distinct from malformed existing content; malformed files
    // must never be treated as an opportunity to replace their contents.
    match read_private_text(path, 128)? {
        Some(value) => {
            let trimmed = value.trim();
            let decoded = Zeroizing::new(
                URL_SAFE_NO_PAD
                    .decode(trimmed)
                    .map_err(|_| anyhow::anyhow!("invalid local secret encoding"))?,
            );
            anyhow::ensure!(
                decoded.len() == 32 && trimmed.len() == 43,
                "invalid local secret length"
            );
            Ok(Some(trimmed.to_owned()))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::super::private_fs::tests::fixture;
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    #[test]
    fn preparation_precedes_logging_and_creates_private_accounts() {
        let fixture = fixture();
        let home = fixture.path().join("home");
        prepare_runtime_home(&home).unwrap();
        assert_eq!(
            fs::metadata(home.join("logs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            prepare_account_directory(&home).unwrap(),
            home.join("accounts")
        );
        assert_eq!(
            fs::metadata(home.join("accounts"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn local_secret_reopens_readonly_and_refuses_broad_files_without_chmod() {
        let fixture = fixture();
        let path = fixture.path().join("client.key");
        assert!(load_existing_secret_file(&path).is_err());
        assert!(!path.exists());
        let first = load_secret_file(&path).unwrap();
        assert_eq!(first.len(), 43);
        assert_eq!(load_existing_secret_file(&path).unwrap(), first);
        assert_eq!(load_secret_file(&path).unwrap(), first);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_existing_secret_file(&path).is_err());
        assert!(load_secret_file(&path).is_err());
        assert!(restrict(&path).is_err());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
    }

    #[test]
    fn private_reader_preserves_missing_and_rejects_oversize_and_invalid_utf8() {
        let fixture = fixture();
        let missing = fixture.path().join("missing");
        assert_eq!(
            read_private_bytes(&missing, 4).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(read_private_text(&missing, 4).unwrap(), None);
        assert_eq!(read_config(fixture.path()).unwrap(), None);
        let path = fixture.path().join("config.toml");
        create_private_secret(&path, b"value").unwrap();
        assert!(read_private_bytes(&path, 4).is_err());
        assert_eq!(read_config(fixture.path()).unwrap().unwrap(), "value");
        let binary = fixture.path().join("binary");
        create_private_secret(&binary, &[0xff]).unwrap();
        assert_eq!(
            read_private_text(&binary, 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn malformed_long_or_symlinked_local_secrets_are_never_replaced() {
        for value in [b"malformed".to_vec(), vec![b'A'; 129]] {
            let fixture = fixture();
            let path = fixture.path().join("admin.token");
            create_private_secret(&path, &value).unwrap();
            assert!(load_secret_file(&path).is_err());
            assert_eq!(fs::read(path).unwrap(), value);
        }
        let fixture = fixture();
        let target = fixture.path().join("target");
        let original = load_secret_file(&target).unwrap();
        symlink(&target, fixture.path().join("alias")).unwrap();
        assert!(load_secret_file(&fixture.path().join("alias")).is_err());
        assert_eq!(load_existing_secret_file(&target).unwrap(), original);
    }
}
