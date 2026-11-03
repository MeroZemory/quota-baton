use qb_core::accounts::{AccountStore, import};
use std::path::Path;

pub fn seed_account(store: &AccountStore, external: &Path) {
    import::run(
        store,
        external,
        &["codex:r101@example.invalid".into()],
        false,
    )
    .unwrap();
}

pub fn configure_command(command: &mut tokio::process::Command) {
    command.creation_flags(0x08000000);
}
