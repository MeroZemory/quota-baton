use std::net::Ipv4Addr;
use tokio::net::TcpListener;

pub(crate) async fn bind_listener(port: u16) -> anyhow::Result<TcpListener> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
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
