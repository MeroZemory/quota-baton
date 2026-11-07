use std::{fs::File, io, path::Path};

pub(crate) fn open_reset_credit_lock(path: &Path) -> io::Result<File> {
    let (directory, name) = super::private_fs::parent_and_name(path)?;
    super::private_fs::open_private_lock(&directory, &name)
}
