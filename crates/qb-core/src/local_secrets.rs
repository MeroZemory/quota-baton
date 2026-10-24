use std::path::Path;

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
    crate::platform::prepare_secret_home(home)?;
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
    crate::platform::load_existing_secret_file(path)
}
fn load_file(path: &Path) -> anyhow::Result<String> {
    crate::platform::load_secret_file(path)
}

// Common unit fixtures validate permissions through the native helper.
#[cfg(test)]
pub(crate) use crate::platform::restrict;
