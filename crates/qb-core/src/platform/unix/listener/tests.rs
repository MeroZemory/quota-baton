use super::*;
use std::net::{TcpStream, UdpSocket};

#[tokio::test]
async fn direct_standalone_facade_binds_without_publishing_activation() {
    // The native unit runner has no activation metadata. Do not mutate env or
    // initialize process-global state to make this library call work.
    assert!(ACTIVATION.get().is_none());
    assert!(
        Environment::read()
            .roles(std::process::id())
            .unwrap()
            .is_none()
    );
    let listener = bind_listener(0).await.unwrap();
    let address = listener.local_addr().unwrap();
    assert_eq!(address.ip(), Ipv4Addr::LOCALHOST);
    assert_ne!(address.port(), 0);
    let client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (_accepted, _) = listener.accept().await.unwrap();
    drop(client);
    assert!(ACTIVATION.get().is_none());
}

fn environment(
    marker: Option<&str>,
    pid: Option<&str>,
    fds: Option<&str>,
    names: Option<&str>,
) -> Environment {
    Environment {
        marker: marker.map(Into::into),
        pid: pid.map(Into::into),
        fds: fds.map(Into::into),
        names: names.map(Into::into),
    }
}
fn listening() -> StdListener {
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

#[test]
fn standalone_requires_absent_activation_metadata() {
    assert_eq!(
        environment(None, None, None, None).roles(123).unwrap(),
        None
    );
    for env in [
        environment(None, Some("123"), None, None),
        environment(None, None, Some("2"), None),
        environment(None, None, None, Some("data:admin")),
    ] {
        assert!(env.roles(123).is_err());
    }
}

#[test]
fn managed_names_map_both_orders_and_nondefault_ports() {
    let ports = ListenerPorts {
        data: 20200,
        admin: 20201,
    };
    for (names, expected) in [
        ("data:admin", [20200, 20201]),
        ("admin:data", [20201, 20200]),
    ] {
        let roles = environment(Some("1"), Some("123"), Some("2"), Some(names))
            .roles(123)
            .unwrap()
            .unwrap();
        assert_eq!(roles.map(|role| role.port(ports)), expected);
    }
}

#[test]
fn malformed_partial_and_foreign_pid_activation_are_refused() {
    for env in [
        environment(Some("0"), Some("123"), Some("2"), Some("data:admin")),
        environment(Some("1"), Some("124"), Some("2"), Some("data:admin")),
        environment(Some("1"), Some("0"), Some("2"), Some("data:admin")),
        environment(Some("1"), Some("0123"), Some("2"), Some("data:admin")),
        environment(Some("1"), Some("123"), Some("1"), Some("data:admin")),
        environment(Some("1"), Some("123"), Some("3"), Some("data:admin")),
        environment(Some("1"), None, Some("2"), Some("data:admin")),
        environment(Some("1"), Some("123"), None, Some("data:admin")),
        environment(Some("1"), Some("123"), Some("2"), None),
    ] {
        assert!(env.roles(123).is_err());
    }
    for names in [
        "data:data",
        "admin:admin",
        "data:admin:other",
        "admin",
        "data:",
        "other:admin",
    ] {
        assert!(
            environment(Some("1"), Some("123"), Some("2"), Some(names))
                .roles(123)
                .is_err()
        );
    }
}

#[test]
fn non_utf8_activation_is_refused() {
    use std::os::unix::ffi::OsStringExt;
    let mut env = environment(Some("1"), Some("123"), Some("2"), Some("data:admin"));
    env.names = Some(OsString::from_vec(vec![0xff]));
    assert!(env.roles(123).is_err());
}

#[test]
fn socket_shape_requires_listening_nonblocking_exact_address() {
    let listener = listening();
    let port = listener.local_addr().unwrap().port();
    validate_socket(listener.as_raw_fd(), port).unwrap();
    assert!(validate_socket(listener.as_raw_fd(), port.wrapping_add(1)).is_err());
    listener.set_nonblocking(false).unwrap();
    assert!(validate_socket(listener.as_raw_fd(), port).is_err());
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    udp.set_nonblocking(true).unwrap();
    assert!(validate_socket(udp.as_raw_fd(), udp.local_addr().unwrap().port()).is_err());
    let any = StdListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    any.set_nonblocking(true).unwrap();
    assert!(validate_socket(any.as_raw_fd(), any.local_addr().unwrap().port()).is_err());
}

#[test]
fn connected_stream_is_not_an_activation_listener() {
    let listener = listening();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    client.set_nonblocking(true).unwrap();
    assert!(validate_socket(client.as_raw_fd(), client.local_addr().unwrap().port()).is_err());
}

#[test]
fn cloexec_duplicate_drop_preserves_original_listener() {
    let listener = listening();
    let port = listener.local_addr().unwrap().port();
    set_cloexec(listener.as_raw_fd()).unwrap();
    let copy = duplicate_listener(listener.as_raw_fd(), port).unwrap();
    assert_ne!(listener.as_raw_fd(), copy.as_raw_fd());
    require_cloexec(copy.as_raw_fd()).unwrap();
    require_cloexec(listener.as_raw_fd()).unwrap();
    drop(copy);
    validate_socket(listener.as_raw_fd(), port).unwrap();
    let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    listener.accept().unwrap();
}

#[tokio::test]
async fn tokio_wrap_keeps_duplicate_and_original_cloexec() {
    let listener = listening();
    let port = listener.local_addr().unwrap().port();
    set_cloexec(listener.as_raw_fd()).unwrap();
    let duplicate = duplicate_listener(listener.as_raw_fd(), port).unwrap();
    let wrapped = TcpListener::from_std(duplicate).unwrap();
    require_cloexec(wrapped.as_raw_fd()).unwrap();
    validate_socket(wrapped.as_raw_fd(), port).unwrap();
    drop(wrapped);
    require_cloexec(listener.as_raw_fd()).unwrap();
    validate_socket(listener.as_raw_fd(), port).unwrap();
}

#[test]
fn exec_arguments_require_installed_absolute_name_and_serve_without_takeover() {
    let path = Path::new("/usr/local/bin/quota-baton");
    validate_exec_arguments(path, &["serve".into()]).unwrap();
    for path in ["quota-baton", "/", "/usr/local/../bin/quota-baton"] {
        assert!(validate_exec_arguments(Path::new(path), &["serve".into()]).is_err());
    }
    for args in [
        vec![],
        vec!["status".into()],
        vec!["serve".into(), "--takeover".into()],
        vec!["serve".into(), "--takeover=true".into()],
    ] {
        assert!(validate_exec_arguments(path, &args).is_err());
    }
}

#[test]
fn close_on_exec_commit_flags_can_be_restored_on_owned_descriptor() {
    let listener = listening();
    let fd = listener.as_raw_fd();
    set_cloexec(fd).unwrap();
    clear_cloexec(fd).unwrap();
    assert!(require_cloexec(fd).is_err());
    set_cloexec(fd).unwrap();
    require_cloexec(fd).unwrap();
    validate_socket(fd, listener.local_addr().unwrap().port()).unwrap();
}

#[test]
fn exec_metadata_retains_original_descriptor_role_order() {
    for (roles, expected) in [
        ([Role::Data, Role::Admin], "data:admin"),
        ([Role::Admin, Role::Data], "admin:data"),
    ] {
        let data = listening();
        let admin = listening();
        let bank = Bank {
            ports: ListenerPorts {
                data: data.local_addr().unwrap().port(),
                admin: admin.local_addr().unwrap().port(),
            },
            originals: [data.into(), admin.into()],
            roles,
            boxes: Mutex::new(box_bank::BoxBank::inherit(None).unwrap()),
        };
        assert_eq!(bank.names(), expected);
    }
}
