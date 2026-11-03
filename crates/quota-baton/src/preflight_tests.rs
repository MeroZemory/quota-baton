use super::*;

#[test]
fn serve_preflight_is_explicit_and_conflicts_with_takeover() {
    assert!(matches!(
        Cli::try_parse_from(["quota-baton", "serve", "--check"])
            .unwrap()
            .command,
        Command::Serve {
            check: true,
            takeover: false,
            ..
        }
    ));
    assert!(Cli::try_parse_from(["quota-baton", "serve", "--check", "--takeover"]).is_err());
}

#[test]
fn arbitrary_permission_error_contents_stay_behind_cli_redaction() {
    let error = anyhow::Error::from(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "synthetic-secret-never-a-diagnostic",
    ));
    assert!(!friendly_error(&error).contains("synthetic-secret"));
}
