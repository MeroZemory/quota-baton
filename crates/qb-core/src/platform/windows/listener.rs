use std::net::Ipv4Addr;
use tokio::net::TcpListener;

pub(crate) async fn bind_listener(port: u16) -> anyhow::Result<TcpListener> {
    use super::service::listener_bank::{self, Binding};
    let managed = match listener_bank::claim(port)? {
        Binding::Imported(listener) => return register(listener, port),
        Binding::New => true,
        Binding::Unmanaged => false,
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) if managed => return register(listener.into_std()?, port),
            Ok(listener) => return Ok(listener),
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn register(listener: std::net::TcpListener, port: u16) -> anyhow::Result<TcpListener> {
    listener.set_nonblocking(true).inspect_err(|error| {
        tracing::warn!(
            phase = "listener_nonblocking",
            code = error.raw_os_error(),
            "Native listener operation failed"
        );
    })?;
    let retained = listener.try_clone().inspect_err(|error| {
        tracing::warn!(
            phase = "listener_clone",
            code = error.raw_os_error(),
            "Native listener operation failed"
        );
    })?;
    let listener = TcpListener::from_std(listener).inspect_err(|error| {
        tracing::warn!(
            phase = "listener_registration",
            code = error.raw_os_error(),
            "Native listener operation failed"
        );
    })?;
    super::service::listener_bank::retain(port, retained)?;
    Ok(listener)
}
