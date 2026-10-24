use super::{Path, io, private_fs};

pub(crate) fn validate_private_file(path: &Path) -> io::Result<()> {
    let (directory, name) = private_fs::parent_and_name(path)?;
    private_fs::validate_leaf(&directory, &name)
}

pub(crate) fn restrict(path: &Path) -> anyhow::Result<()> {
    validate_private_file(path)?;
    Ok(())
}
