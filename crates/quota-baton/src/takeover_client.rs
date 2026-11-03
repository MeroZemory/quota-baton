//! Bounded authenticated loopback handover, read directly into wiping storage.
//! This intentionally handles only our fixed-length lifecycle response. No
//! application-owned transport chunk or growing buffer holds launch grants.
use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

const HEADER_LIMIT: usize = 16 * 1024;
const BODY_LIMIT: usize = qb_core::relay::MAX_TAKEOVER_REPLY_BYTES;

pub(super) async fn request(
    port: u16,
    token: &str,
    drain_timeout_secs: u64,
) -> Result<Zeroizing<Vec<u8>>> {
    tokio::time::timeout(std::time::Duration::from_secs(35), async {
        ensure!(
            !token.is_empty()
                && token.len() <= 256
                && token.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
            "Invalid local handover credential"
        );
        let body = serde_json::to_vec(&serde_json::json!({
            "drain_timeout_secs": drain_timeout_secs
        }))?;
        // Fixed capacity: extending these bounded fields never reallocates a
        // credential-bearing request buffer. No formatted token String exists.
        let mut request = Zeroizing::new(Vec::with_capacity(1024));
        request.extend_from_slice(b"POST /admin/v1/lifecycle/drain HTTP/1.1\r\nHost: 127.0.0.1:");
        request.extend_from_slice(port.to_string().as_bytes());
        request.extend_from_slice(b"\r\nAuthorization: Bearer ");
        request.extend_from_slice(token.as_bytes());
        request.extend_from_slice(b"\r\nx-qb-launch-handover: 1\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: ");
        request.extend_from_slice(body.len().to_string().as_bytes());
        request.extend_from_slice(b"\r\n\r\n");
        request.extend_from_slice(&body);
        let mut stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .context("Local handover connection failed")?;
        stream.write_all(&request).await.context("Local handover request failed")?;
        receive(&mut stream).await
    })
    .await
    .context("Local handover deadline elapsed")?
}

async fn receive(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<Zeroizing<Vec<u8>>> {
    // Initialized once, never grown. Reads land directly in the final wiping
    // allocation, including rejected/truncated responses and cancellation.
    let mut bytes = Zeroizing::new(vec![0; BODY_LIMIT + HEADER_LIMIT]);
    let mut used = 0;
    let header_end = loop {
        if let Some(end) = bytes[..used]
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
        {
            break end + 4;
        }
        ensure!(
            used < HEADER_LIMIT,
            "Local handover header exceeds its bound"
        );
        let count = stream.read(&mut bytes[used..HEADER_LIMIT]).await?;
        ensure!(count != 0, "Local handover header was truncated");
        used += count;
    };
    let size = response_size(&bytes[..header_end])?;
    let end = header_end
        .checked_add(size)
        .context("Local handover size overflow")?;
    ensure!(used <= end, "Local handover response has trailing data");
    while used < end {
        let count = stream.read(&mut bytes[used..end]).await?;
        ensure!(count != 0, "Local handover body was truncated");
        used += count;
    }
    // Move within the same allocation, then wipe the overlapping old body and
    // header tail *before* reducing len (Zeroizing only owns the live Vec len).
    bytes.copy_within(header_end..end, 0);
    bytes[size..].zeroize();
    bytes.truncate(size);
    Ok(bytes)
}

fn response_size(header: &[u8]) -> Result<usize> {
    let header = std::str::from_utf8(header).context("Invalid local handover header")?;
    let mut lines = header.split("\r\n");
    let mut status = lines
        .next()
        .context("Missing local handover status")?
        .split(' ');
    ensure!(
        matches!(status.next(), Some("HTTP/1.1" | "HTTP/1.0")),
        "Invalid local handover protocol"
    );
    let status = status
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .context("Invalid local handover status")?;
    ensure!(
        (200..300).contains(&status),
        "Local handover refused ({status})"
    );
    let mut length = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .context("Invalid local handover header")?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            bail!("Unsupported local handover framing");
        }
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(length.is_none(), "Duplicate local handover length");
            let value = value.trim();
            ensure!(
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
                "Invalid local handover length"
            );
            let value = value
                .parse::<usize>()
                .context("Invalid local handover length")?;
            ensure!(value <= BODY_LIMIT, "Local handover body exceeds its bound");
            length = Some(value);
        }
    }
    length.context("Missing local handover length")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takeover_framing_is_bounded_and_unambiguous() {
        assert_eq!(
            response_size(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n").unwrap(),
            2
        );
        for header in [
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\ncontent-length: 1\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n".to_owned(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                BODY_LIMIT + 1
            ),
        ] {
            assert!(response_size(header.as_bytes()).is_err());
        }
    }

    #[tokio::test]
    async fn takeover_direct_read_retains_only_body_and_rejects_truncation() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let write = tokio::spawn(async move {
            for part in [
                b"HTTP/1.1 200 OK\r\n".as_slice(),
                b"Content-Length: 6\r\n\r\n",
                b"abc",
                b"def",
            ] {
                writer.write_all(part).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let body = receive(&mut reader).await.unwrap();
        assert_eq!(body.as_slice(), b"abcdef");
        assert!(body.capacity() >= BODY_LIMIT + HEADER_LIMIT);
        write.await.unwrap();
        let mut truncated = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nabc".as_slice();
        assert!(receive(&mut truncated).await.is_err());
    }
}
