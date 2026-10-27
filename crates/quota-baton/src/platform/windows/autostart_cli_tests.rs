use super::*;
use clap::Parser;

#[test]
fn r17_wire_cli_defaults_to_login_start_and_accepts_legacy_opt_in() {
    for flags in [vec![], vec!["--autostart"]] {
        let cli =
            crate::Cli::try_parse_from(["quota-baton", "wire"].into_iter().chain(flags)).unwrap();
        let crate::Command::Platform(PlatformCommand::Wire { no_autostart, .. }) = cli.command
        else {
            panic!("expected Windows wire command");
        };
        assert!(!no_autostart);
    }
    let cli = crate::Cli::try_parse_from(["quota-baton", "wire", "--no-autostart"]).unwrap();
    let crate::Command::Platform(PlatformCommand::Wire { no_autostart, .. }) = cli.command else {
        panic!("expected Windows wire command");
    };
    assert!(no_autostart);
    assert!(
        crate::Cli::try_parse_from(["quota-baton", "wire", "--autostart", "--no-autostart"])
            .is_err()
    );
    assert!(cutover::WireOptions::default().autostart);
}
