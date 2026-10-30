use super::*;

struct Fixture {
    home: std::path::PathBuf,
    config: Config,
    admin: std::net::TcpListener,
    _data: std::net::TcpListener,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let base = executable
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sandbox/r20-service-bootstrap");
        std::fs::create_dir_all(&base).unwrap();
        let directory = tempfile::tempdir_in(base).unwrap();
        let home = directory.path().join("home");
        crate::platform::create_private_directory_new(&home).unwrap();
        let admin = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        admin.set_nonblocking(true).unwrap();
        let data = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut config = Config::default();
        config.server.data_port = data.local_addr().unwrap().port();
        config.server.admin_port = admin.local_addr().unwrap().port();
        Self {
            home,
            config,
            admin,
            _data: data,
            _directory: directory,
        }
    }
}

#[tokio::test]
async fn r20_takeover_free_lease_does_not_contact_an_unowned_admin_listener() {
    let fixture = Fixture::new();
    assert!(
        !handle_takeover(&fixture.home, &fixture.config, true)
            .await
            .unwrap()
    );
    assert!(matches!(
        fixture.admin.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert!(!fixture.home.join("admin.token").exists());
    let accounts = fixture.home.join("accounts");
    let _lease = crate::platform::Lease::acquire(&accounts).unwrap();
}

#[tokio::test]
async fn r20_takeover_busy_lease_still_requires_authenticated_listener_export() {
    let fixture = Fixture::new();
    let accounts = crate::platform::prepare_account_directory(&fixture.home).unwrap();
    let _owner = crate::platform::Lease::acquire(&accounts).unwrap();
    let error = handle_takeover(&fixture.home, &fixture.config, true)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "listener export authentication unavailable"
    );
    assert!(matches!(
        crate::platform::Lease::acquire(&accounts),
        Err(crate::accounts::StoreError::LeaseBusy)
    ));
}
