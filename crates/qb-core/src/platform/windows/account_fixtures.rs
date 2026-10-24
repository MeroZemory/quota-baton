use crate::accounts::{AccountStore, StoreError};
use std::path::Path;

pub(crate) fn seed_account_fixtures(
    store: &AccountStore,
    dir: &Path,
    selectors: &[String],
    dry_run: bool,
) -> Result<(), StoreError> {
    crate::accounts::import::run(store, dir, selectors, dry_run).map(|_| ())
}
