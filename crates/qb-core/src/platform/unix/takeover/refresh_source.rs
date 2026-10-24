use crate::accounts::takeover::Journal;
use std::io;

#[derive(Clone, Default)]
pub struct RefreshSourceContext;

impl RefreshSourceContext {
    pub fn verify_retired_source(&self, _journal: &Journal) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native takeover source unavailable",
        ))
    }
}
