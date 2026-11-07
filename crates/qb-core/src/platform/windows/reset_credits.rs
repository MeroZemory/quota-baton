use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

pub(crate) fn open_reset_credit_lock(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    super::fs_identity::configure_snapshot_open(&mut options);
    let file = options.open(path)?;
    if !file.metadata()?.is_file()
        || !super::fs_identity::binding_snapshot_handle_trusted(&file.metadata()?)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "reset-credit lock must be a regular private file",
        ));
    }
    super::secrets::restrict(path).map_err(io::Error::other)?;
    Ok(file)
}
