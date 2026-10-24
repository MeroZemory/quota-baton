use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};
pub(crate) fn configure_secret_open(options: &mut fs::OpenOptions) {
    options.mode(0o600);
}
pub(crate) fn restrict(path: &Path) -> anyhow::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}
