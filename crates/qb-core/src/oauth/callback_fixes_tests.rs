use super::*;

#[test]
fn bind_errors_distinguish_busy_denied_and_unavailable() {
    use std::io::{Error, ErrorKind};
    assert!(matches!(
        bind_error(&Error::from(ErrorKind::AddrInUse), 23456),
        LoginError::CallbackPortBusy { port: 23456 }
    ));
    for kind in [
        ErrorKind::PermissionDenied,
        ErrorKind::AddrNotAvailable,
        ErrorKind::Unsupported,
    ] {
        assert!(matches!(
            bind_error(&Error::from(kind), 23456),
            LoginError::CallbackBindFailed { port: 23456 }
        ));
    }
    assert!(ipv6_unavailable(&Error::from(ErrorKind::AddrNotAvailable)));
    assert!(ipv6_unavailable(&Error::from(ErrorKind::Unsupported)));
    assert!(!ipv6_unavailable(&Error::from(ErrorKind::PermissionDenied)));
    assert!(!ipv6_unavailable(&Error::from(ErrorKind::AddrInUse)));
}

async fn ipv4_only() -> Listeners {
    loop {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = v4.local_addr().unwrap().port();
        if !matches!(port, 10100 | 10101) {
            return Listeners { v4, v6: None, port };
        }
    }
}

async fn request(port: u16, target: &str) -> String {
    let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    socket
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(2), socket.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    response
}

#[tokio::test]
async fn bad_states_and_duplicate_parameters_do_not_end_ipv4_only_login() {
    let listeners = ipv4_only().await;
    let port = listeners.port();
    let login = tokio::spawn(listeners.callback("/auth/callback", "synthetic-state"));
    for target in [
        "/auth/callback?code=synthetic-code&state=wrong",
        "/auth/callback?code=synthetic-code",
        "/auth/callback?error=denied&state=wrong",
        "/auth/callback?code=one&code=two&state=synthetic-state",
    ] {
        let response = request(port, target).await;
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(!response.contains("synthetic"));
        assert!(!login.is_finished());
    }
    assert!(
        request(
            port,
            "/auth/callback?code=synthetic%2Bcode&state=synthetic-state"
        )
        .await
        .starts_with("HTTP/1.1 200")
    );
    let code = tokio::time::timeout(Duration::from_secs(2), login)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(code.as_str(), "synthetic+code");
    assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await.is_ok());
}

#[tokio::test]
async fn preconnect_socket_does_not_delay_a_valid_callback() {
    let listeners = ipv4_only().await;
    let port = listeners.port();
    let preconnect = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let login = tokio::spawn(listeners.callback("/callback", "synthetic-state"));
    assert!(
        request(port, "/callback?code=synthetic-code&state=synthetic-state")
            .await
            .starts_with("HTTP/1.1 200")
    );
    drop(preconnect);
    let code = tokio::time::timeout(Duration::from_secs(2), login)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(code.as_str(), "synthetic-code");
}

#[test]
fn callback_read_cap_is_fixed() {
    assert_eq!(MAX_CALLBACK_READS, 16);
}

#[tokio::test]
async fn only_matching_state_provider_error_ends_login() {
    let listeners = ipv4_only().await;
    let port = listeners.port();
    let login = tokio::spawn(listeners.callback("/callback", "synthetic-state"));
    assert!(
        request(
            port,
            "/callback?error=synthetic-denied&state=synthetic-state"
        )
        .await
        .starts_with("HTTP/1.1 400")
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), login)
            .await
            .unwrap()
            .unwrap(),
        Err(LoginError::ExchangeFailed)
    ));
}

#[tokio::test]
async fn occupied_numeric_loopback_port_is_busy() {
    let occupied = ipv4_only().await;
    let port = occupied.port();
    assert!(
        matches!(Listeners::bind(port).await, Err(LoginError::CallbackPortBusy { port: p }) if p == port)
    );
}

#[test]
fn borrowed_callback_parser_decodes_and_rejects_malformed_input() {
    let code = parse_target(
        "/callback?co%64e=synthetic%26code+value&st%61te=synthetic-state",
        "/callback",
        "synthetic-state",
    )
    .unwrap()
    .unwrap();
    assert_eq!(code.as_str(), "synthetic&code value");
    for target in [
        "/callback?code=%ZZ&state=synthetic-state",
        "/callback?code=%FF&state=synthetic-state",
        "/callback?code=&state=synthetic-state",
    ] {
        assert!(matches!(
            parse_target(target, "/callback", "synthetic-state"),
            Err(LoginError::InvalidCallback)
        ));
    }
}
