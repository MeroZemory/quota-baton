pub(super) fn status_without_platform_trailer(status: &str) -> &str {
    status
}

pub(super) fn configure_test_registry(_command: &mut std::process::Command) -> std::io::Result<()> {
    Ok(())
}

pub(super) fn real_run_snapshot() -> std::io::Result<()> {
    Ok(())
}

pub(super) fn verify_real_run_unchanged(_before: &()) -> std::io::Result<()> {
    Ok(())
}
