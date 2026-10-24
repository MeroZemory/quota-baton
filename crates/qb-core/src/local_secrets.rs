use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::TryRngCore;

#[derive(Clone)]
pub struct LocalSecrets {
    pub client_key: String,
    pub admin_token: String,
}

// 디버그 출력에도 실제 비밀값을 노출하지 않는다.
impl std::fmt::Debug for LocalSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSecrets")
            .field("client_key", &"<redacted>")
            .field("admin_token", &"<redacted>")
            .finish()
    }
}

pub fn load_or_create(home: &Path) -> anyhow::Result<LocalSecrets> {
    fs::create_dir_all(home)?;
    let client_key = load_file(&home.join("client.key"))?;
    let admin_token = load_file(&home.join("admin.token"))?;
    anyhow::ensure!(client_key != admin_token, "local secrets must be distinct");
    Ok(LocalSecrets {
        client_key,
        admin_token,
    })
}

/// Read the local secret files without creating or changing anything.
pub fn load_existing(home: &Path) -> anyhow::Result<LocalSecrets> {
    let client_key = load_existing_file(&home.join("client.key"))?;
    let admin_token = load_existing_file(&home.join("admin.token"))?;
    anyhow::ensure!(client_key != admin_token, "local secrets must be distinct");
    Ok(LocalSecrets {
        client_key,
        admin_token,
    })
}

fn load_existing_file(path: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(path.exists(), "local secret is missing: {}", path.display());
    anyhow::ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "local secret must be a regular file"
    );
    let mut value = String::new();
    fs::File::open(path)?.take(128).read_to_string(&mut value)?;
    let value = value.trim();
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| anyhow::anyhow!("invalid local secret encoding"))?;
    anyhow::ensure!(
        decoded.len() == 32 && value.len() == 43,
        "invalid local secret length"
    );
    Ok(value.to_owned())
}

fn load_file(path: &Path) -> anyhow::Result<String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    crate::platform::configure_secret_open(&mut options);
    match options.open(path) {
        Ok(mut file) => {
            // 비밀값을 기록하기 전에 접근 권한부터 제한한다.
            let result = (|| -> anyhow::Result<String> {
                restrict(path)?;
                let mut bytes = [0u8; 32];
                rand::rngs::OsRng
                    .try_fill_bytes(&mut bytes)
                    .map_err(|_| anyhow::anyhow!("secure random generation failed"))?;
                let value = URL_SAFE_NO_PAD.encode(bytes);
                file.write_all(value.as_bytes())?;
                file.sync_all()?;
                Ok(value)
            })();
            if result.is_err() {
                drop(file);
                let _ = fs::remove_file(path);
            }
            result
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(
                !fs::symlink_metadata(path)?.file_type().is_symlink(),
                "local secret must be a regular file"
            );
            restrict(path)?;
            let mut value = String::new();
            // 잘못된 파일이 무제한 메모리를 사용하지 못하게 제한한다.
            fs::File::open(path)?.take(128).read_to_string(&mut value)?;
            let value = value.trim();
            let decoded = URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|_| anyhow::anyhow!("invalid local secret encoding"))?;
            anyhow::ensure!(
                decoded.len() == 32 && value.len() == 43,
                "invalid local secret length"
            );
            Ok(value.to_owned())
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) use crate::platform::restrict;
