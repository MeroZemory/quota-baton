use std::{
    io::{self, Write},
    pin::Pin,
    time::Duration,
};

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

pub(super) const HOLD_LIMIT: usize = 64 * 1024;
pub(super) type RawStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitDecision {
    NormalData,
    FirstError,
    Status,
    HoldLimit,
    HoldTimeout,
    End,
    StreamError,
    Undecodable,
}

pub(super) struct HeldResponse {
    pub prefix: Bytes,
    pub remainder: Option<Bytes>,
    pub stream: RawStream,
    pub terminal: Option<io::Error>,
    pub decision: CommitDecision,
}

fn stream_error() -> io::Error {
    io::Error::other("업스트림 응답 스트림이 중단되었습니다.")
}

pub(super) async fn hold(
    status: StatusCode,
    headers: &HeaderMap,
    mut stream: RawStream,
    timeout: Duration,
) -> HeldResponse {
    let mut prefix = Vec::with_capacity(HOLD_LIMIT);
    let mut remainder = None;
    let mut terminal = None;
    let sse = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        });
    let encoding = side_encoding(headers);
    let mut decoder = if sse && status.is_success() {
        encoding.and_then(|encoding| encoding.map(SideDecoder::new).transpose().ok().flatten())
    } else {
        None
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let mut scanner = EventScanner::default();
    let decision = if !status.is_success() {
        CommitDecision::Status
    } else if sse && (encoding.is_none() || (encoding != Some(None) && decoder.is_none())) {
        // 지원하지 않는 인코딩은 첫 바이트를 기다리지 않고 원본 스트림을 넘긴다.
        CommitDecision::Undecodable
    } else {
        loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => break CommitDecision::HoldTimeout,
                Ok(None) => break CommitDecision::End,
                Ok(Some(Err(_))) => {
                    terminal = Some(stream_error());
                    break CommitDecision::StreamError;
                }
                Ok(Some(Ok(chunk))) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    let take = chunk.len().min(HOLD_LIMIT - prefix.len());
                    prefix.extend_from_slice(&chunk[..take]);
                    if take < chunk.len() {
                        remainder = Some(chunk.slice(take..));
                    }
                    if !sse {
                        break CommitDecision::NormalData;
                    }
                    if let Some(decoder) = decoder.as_mut() {
                        let result = decoder.feed(&chunk[..take]);
                        if let Some(decision) = scanner.scan(decoder.decoded()) {
                            break decision;
                        }
                        if decoder.decoded().len() == HOLD_LIMIT {
                            break CommitDecision::HoldLimit;
                        }
                        if result.is_err() {
                            break CommitDecision::Undecodable;
                        }
                    } else if let Some(decision) = scanner.scan(&prefix) {
                        break decision;
                    }
                    if prefix.len() == HOLD_LIMIT {
                        break CommitDecision::HoldLimit;
                    }
                }
            }
        }
    };
    HeldResponse {
        prefix: prefix.into(),
        remainder,
        stream,
        terminal,
        decision,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SideEncoding {
    Zstd,
    Gzip,
    Deflate,
    Brotli,
}

// 중복 헤더와 쉼표로 쌓인 인코딩도 지원 불가로 처리한다.
fn side_encoding(headers: &HeaderMap) -> Option<Option<SideEncoding>> {
    let mut values = headers.get_all("content-encoding").iter();
    let Some(value) = values.next() else {
        return Some(None);
    };
    if values.next().is_some() {
        return None;
    }
    let value = value.as_bytes().trim_ascii();
    if value.eq_ignore_ascii_case(b"identity") {
        Some(None)
    } else if value.eq_ignore_ascii_case(b"zstd") {
        Some(Some(SideEncoding::Zstd))
    } else if value.eq_ignore_ascii_case(b"gzip") {
        Some(Some(SideEncoding::Gzip))
    } else if value.eq_ignore_ascii_case(b"deflate") {
        Some(Some(SideEncoding::Deflate))
    } else if value.eq_ignore_ascii_case(b"br") {
        Some(Some(SideEncoding::Brotli))
    } else {
        None
    }
}

#[derive(Default)]
struct BoundedDecoded(Vec<u8>);

impl Write for BoundedDecoded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let take = bytes.len().min(HOLD_LIMIT - self.0.len());
        if take == 0 && !bytes.is_empty() {
            return Err(io::Error::other("압축 해제 검사 한도에 도달했습니다."));
        }
        self.0.extend_from_slice(&bytes[..take]);
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// 디코더 상태를 유지하며 새 압축 조각만 입력한다. 출력은 64 KiB에서 중단한다.
// zstd window는 8 MiB, brotli는 표준 window(최대 16 MiB)로 제한된다.
enum SideDecoder {
    Zstd(zstd::stream::write::Decoder<'static, BoundedDecoded>),
    Gzip(flate2::write::MultiGzDecoder<BoundedDecoded>),
    DeflateHeader(Vec<u8>),
    Zlib(flate2::write::ZlibDecoder<BoundedDecoded>),
    RawDeflate(flate2::write::DeflateDecoder<BoundedDecoded>),
    Brotli {
        decoder: Box<brotli::DecompressorWriter<BoundedDecoded>>,
        header_checked: bool,
    },
}

impl SideDecoder {
    fn new(encoding: SideEncoding) -> io::Result<Self> {
        let output = BoundedDecoded::default();
        Ok(match encoding {
            SideEncoding::Zstd => {
                let mut decoder = zstd::stream::write::Decoder::new(output)?;
                decoder.window_log_max(23)?;
                Self::Zstd(decoder)
            }
            SideEncoding::Gzip => Self::Gzip(flate2::write::MultiGzDecoder::new(output)),
            SideEncoding::Deflate => Self::DeflateHeader(Vec::with_capacity(2)),
            SideEncoding::Brotli => Self::Brotli {
                decoder: Box::new(brotli::DecompressorWriter::new(output, 4096)),
                header_checked: false,
            },
        })
    }

    fn feed(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if let Self::Brotli { header_checked, .. } = self
            && !*header_checked
            && let Some(first) = bytes.first()
        {
            // 라이브러리는 large-window 확장도 허용하므로 할당 전에 거부한다.
            if first & 0x7f == 0x11 {
                return Err(io::Error::other("brotli 확장 window는 검사하지 않습니다."));
            }
            *header_checked = true;
        }
        if let Self::DeflateHeader(header) = self {
            let take = bytes.len().min(2 - header.len());
            header.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if header.len() < 2 {
                return Ok(());
            }
            let initial = [header[0], header[1]];
            // HTTP deflate의 zlib wrapper 유무를 두 바이트로 판별한다.
            let zlib = initial[0] & 15 == 8
                && initial[0] >> 4 <= 7
                && u16::from_be_bytes(initial) % 31 == 0;
            *self = if zlib {
                Self::Zlib(flate2::write::ZlibDecoder::new(BoundedDecoded::default()))
            } else {
                Self::RawDeflate(flate2::write::DeflateDecoder::new(BoundedDecoded::default()))
            };
            self.feed(&initial)?;
        }
        let writer: &mut dyn Write = match self {
            Self::Zstd(decoder) => decoder,
            Self::Gzip(decoder) => decoder,
            Self::Zlib(decoder) => decoder,
            Self::RawDeflate(decoder) => decoder,
            Self::Brotli { decoder, .. } => decoder.as_mut(),
            Self::DeflateHeader(_) => unreachable!(),
        };
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn decoded(&self) -> &[u8] {
        match self {
            Self::Zstd(decoder) => &decoder.get_ref().0,
            Self::Gzip(decoder) => &decoder.get_ref().0,
            Self::Zlib(decoder) => &decoder.get_ref().0,
            Self::RawDeflate(decoder) => &decoder.get_ref().0,
            Self::Brotli { decoder, .. } => &decoder.get_ref().0,
            Self::DeflateHeader(_) => &[],
        }
    }
}

fn decoded_prefix(raw: &[u8], encoding: SideEncoding) -> Option<Vec<u8>> {
    let mut decoder = SideDecoder::new(encoding).ok()?;
    let _ = decoder.feed(raw);
    Some(decoder.decoded().to_vec())
}

pub(super) fn error_code(headers: &HeaderMap, raw: &[u8]) -> Option<String> {
    let decoded = decoded_side(headers, raw)?;
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&decoded) {
        return crate::observability::safe_error_code(&value);
    }
    for line in decoded.split(|b| *b == b'\n') {
        if let Some(data) = line.strip_prefix(b"data:")
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(data.trim_ascii())
            && let Some(code) = crate::observability::safe_error_code(&value)
        {
            return Some(code);
        }
    }
    None
}

pub(super) fn decoded_side(headers: &HeaderMap, raw: &[u8]) -> Option<Vec<u8>> {
    Some(match side_encoding(headers)? {
        Some(encoding) => decoded_prefix(&raw[..raw.len().min(HOLD_LIMIT)], encoding)?,
        None => raw[..raw.len().min(HOLD_LIMIT)].to_vec(),
    })
}

#[derive(Default)]
struct EventScanner {
    scanned: usize,
    event_start: usize,
    line_start: usize,
    after_cr: bool,
}

impl EventScanner {
    fn scan(&mut self, bytes: &[u8]) -> Option<CommitDecision> {
        while self.scanned < bytes.len() {
            let index = self.scanned;
            self.scanned += 1;
            let byte = bytes[index];
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                self.line_start = index + 1;
                if self.event_start == index {
                    self.event_start = index + 1;
                }
                continue;
            }
            self.after_cr = byte == b'\r';
            if !matches!(byte, b'\r' | b'\n') {
                continue;
            }
            let line = &bytes[self.line_start..index];
            self.line_start = index + 1;
            if line.is_empty() || line == b"\r" {
                let event = &bytes[self.event_start..self.line_start];
                self.event_start = self.line_start;
                if let Some(decision) = classify_event(event) {
                    return Some(decision);
                }
            }
        }
        None
    }
}

fn classify_event(event: &[u8]) -> Option<CommitDecision> {
    let mut error = false;
    let mut data = Vec::new();
    let mut has_data = false;
    for line in event.split(|b| matches!(*b, b'\r' | b'\n')) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(value) = line.strip_prefix(b"event:") {
            error |= matches!(
                value.trim_ascii(),
                b"error" | b"response.failed" | b"response.error"
            );
        }
        if line == b"data" || line.starts_with(b"data:") {
            let value = line.strip_prefix(b"data:").unwrap_or_default();
            let value = value.strip_prefix(b" ").unwrap_or(value);
            if has_data {
                data.push(b'\n');
            }
            data.extend_from_slice(value);
            has_data = true;
        }
    }
    #[derive(serde::Deserialize)]
    struct Kind {
        r#type: Option<String>,
    }
    error |= serde_json::from_slice::<Kind>(&data)
        .ok()
        .and_then(|v| v.r#type)
        .is_some_and(|kind| {
            matches!(
                kind.as_str(),
                "error" | "response.failed" | "response.error"
            )
        });
    if error {
        Some(CommitDecision::FirstError)
    } else if has_data {
        Some(CommitDecision::NormalData)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn error_code_is_bounded_decoded_and_never_message_text() {
        use axum::http::HeaderMap;
        let raw=br#"{"error":{"code":"invalid_encrypted_content","type":"invalid_request_error","message":"synthetic secret body"}}"#;
        let mut headers = HeaderMap::new();
        assert_eq!(
            super::error_code(&headers, raw).as_deref(),
            Some("invalid_encrypted_content")
        );
        for (name, kind) in [
            ("gzip", super::SideEncoding::Gzip),
            ("deflate", super::SideEncoding::Deflate),
            ("br", super::SideEncoding::Brotli),
            ("zstd", super::SideEncoding::Zstd),
        ] {
            headers.insert("content-encoding", name.parse().unwrap());
            assert_eq!(
                super::error_code(&headers, &compress(raw, kind)).as_deref(),
                Some("invalid_encrypted_content")
            );
        }
        headers.clear();
        assert_eq!(
            super::error_code(
                &headers,
                br#"{"error":{"type":"authentication_error","message":"never logged"}}"#
            )
            .as_deref(),
            Some("authentication_error")
        );
        assert!(
            super::error_code(
                &headers,
                br#"{"error":{"code":"Bearer sk-secret","message":"never logged"}}"#
            )
            .is_none()
        );
    }
    use super::*;

    fn compress(event: &[u8], encoding: SideEncoding) -> Vec<u8> {
        match encoding {
            SideEncoding::Gzip => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                encoder.write_all(event).unwrap();
                encoder.finish().unwrap()
            }
            SideEncoding::Deflate => {
                let mut encoder =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                encoder.write_all(event).unwrap();
                encoder.finish().unwrap()
            }
            SideEncoding::Zstd => zstd::stream::encode_all(event, 1).unwrap(),
            SideEncoding::Brotli => {
                let mut output = Vec::new();
                {
                    let mut encoder = brotli::CompressorWriter::new(&mut output, 4096, 3, 22);
                    encoder.write_all(event).unwrap();
                }
                output
            }
        }
    }

    #[tokio::test]
    async fn brotli_first_event_commits_without_waiting_and_preserves_raw() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        headers.insert("content-encoding", "br".parse().unwrap());
        for (event, expected) in [
            (
                b"data: {\"type\":\"response.created\"}\n\n".as_slice(),
                CommitDecision::NormalData,
            ),
            (
                b"event: error\ndata: {\"type\":\"error\"}\n\n".as_slice(),
                CommitDecision::FirstError,
            ),
        ] {
            // flush만 한 열린 압축 스트림도 첫 이벤트를 바로 분류해야 한다.
            let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 3, 22);
            encoder.write_all(event).unwrap();
            encoder.flush().unwrap();
            let raw = Bytes::copy_from_slice(encoder.get_ref());
            let stream = futures_util::stream::iter([Ok(raw.clone())])
                .chain(futures_util::stream::pending());
            let held = tokio::time::timeout(
                Duration::from_millis(250),
                hold(
                    StatusCode::OK,
                    &headers,
                    Box::pin(stream),
                    Duration::from_secs(10),
                ),
            )
            .await
            .expect("첫 이벤트에서 커밋해야 한다");
            assert_eq!(held.decision, expected);
            assert_eq!(held.prefix, raw);
        }
    }

    #[tokio::test]
    async fn brotli_tiny_fragments_normal_and_first_error_preserve_raw() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        headers.insert("content-encoding", "br".parse().unwrap());
        for (event, expected) in [
            (
                b": ping\n\ndata: {}\n\n".as_slice(),
                CommitDecision::NormalData,
            ),
            (
                b": ping\n\nevent: error\ndata: {\"type\":\"error\"}\n\n".as_slice(),
                CommitDecision::FirstError,
            ),
        ] {
            let raw = compress(event, SideEncoding::Brotli);
            let chunks: Vec<_> = raw
                .iter()
                .map(|byte| Ok(Bytes::copy_from_slice(&[*byte])))
                .collect();
            let stream = futures_util::stream::iter(chunks).chain(futures_util::stream::pending());
            let mut held = tokio::time::timeout(
                Duration::from_millis(250),
                hold(
                    StatusCode::OK,
                    &headers,
                    Box::pin(stream),
                    Duration::from_secs(10),
                ),
            )
            .await
            .unwrap();
            assert_eq!(held.decision, expected);
            let mut relayed = held.prefix.to_vec();
            while relayed.len() < raw.len() {
                relayed.extend_from_slice(&held.stream.next().await.unwrap().unwrap());
            }
            assert_eq!(relayed, raw);
        }
    }

    #[tokio::test]
    async fn unknown_and_stacked_encodings_commit_before_polling_delayed_stream() {
        for values in [
            vec!["snappy"],
            vec!["gzip, br"],
            vec!["identity, gzip"],
            vec!["gzip", "br"],
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", "text/event-stream".parse().unwrap());
            for value in values {
                headers.append("content-encoding", value.parse().unwrap());
            }
            let raw = Bytes::from_static(b"compressed synthetic bytes");
            let expected = raw.clone();
            let stream = async_stream::stream! {
                tokio::time::sleep(Duration::from_millis(10)).await;
                yield Ok(raw);
            };
            // poll 자체가 호출되지 않는지 별도 스트림으로도 확인한다.
            let never_poll = futures_util::stream::poll_fn(
                |_| -> std::task::Poll<Option<Result<Bytes, reqwest::Error>>> {
                    panic!("지원하지 않는 인코딩은 스트림을 읽으면 안 된다")
                },
            );
            let held = hold(
                StatusCode::OK,
                &headers,
                Box::pin(never_poll),
                Duration::from_secs(10),
            )
            .await;
            assert_eq!(held.decision, CommitDecision::Undecodable);
            assert!(held.prefix.is_empty());
            let mut held = tokio::time::timeout(
                Duration::from_millis(100),
                hold(
                    StatusCode::OK,
                    &headers,
                    Box::pin(stream),
                    Duration::from_secs(10),
                ),
            )
            .await
            .unwrap();
            assert_eq!(held.decision, CommitDecision::Undecodable);
            assert!(held.prefix.is_empty());
            assert!(held.remainder.is_none());
            assert_eq!(held.stream.next().await.unwrap().unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn malformed_compression_and_brotli_large_window_commit_without_waiting() {
        for (encoding, bytes) in [
            ("gzip", vec![0; 32]),
            ("deflate", vec![0xff; 32]),
            ("zstd", vec![0; 32]),
            // large-window 식별자는 첫 바이트에서 검사하여 큰 window 할당을 막는다.
            ("br", vec![0x11, 30]),
            ("br", vec![0x91, 30]),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", "text/event-stream".parse().unwrap());
            headers.insert("content-encoding", encoding.parse().unwrap());
            let raw = Bytes::from(bytes);
            let stream = futures_util::stream::iter([Ok(raw.clone())])
                .chain(futures_util::stream::pending());
            let held = tokio::time::timeout(
                Duration::from_millis(250),
                hold(
                    StatusCode::OK,
                    &headers,
                    Box::pin(stream),
                    Duration::from_secs(10),
                ),
            )
            .await
            .unwrap();
            assert_eq!(held.decision, CommitDecision::Undecodable, "{encoding}");
            assert_eq!(held.prefix, raw);
        }
    }

    #[test]
    fn compressed_fragments_decode_and_scan_incrementally_with_bounded_output() {
        // 잘 압축되지 않는 긴 주석을 한 바이트씩 입력해 전체 prefix 재해제를 회귀 검사한다.
        let mut event = b": ".to_vec();
        let mut state = 7u32;
        for _ in 0..32_000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            event.push(b'!' + (state % 90) as u8);
        }
        event.extend_from_slice(b"\n\ndata: {}\n\n");
        for encoding in [
            SideEncoding::Gzip,
            SideEncoding::Deflate,
            SideEncoding::Brotli,
            SideEncoding::Zstd,
        ] {
            let raw = compress(&event, encoding);
            let mut decoder = SideDecoder::new(encoding).unwrap();
            let mut scanner = EventScanner::default();
            let mut decision = None;
            let started = std::time::Instant::now();
            for byte in &raw {
                decoder.feed(std::slice::from_ref(byte)).unwrap();
                decision = decision.or_else(|| scanner.scan(decoder.decoded()));
                assert!(decoder.decoded().len() <= HOLD_LIMIT);
            }
            assert_eq!(decoder.decoded(), event);
            assert_eq!(decision, Some(CommitDecision::NormalData));
            assert_eq!(scanner.scanned, event.len());
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "조각마다 전체 prefix를 재해제하면 안 된다"
            );
            let bomb = compress(&vec![b'x'; HOLD_LIMIT * 2], encoding);
            let mut decoder = SideDecoder::new(encoding).unwrap();
            let _ = decoder.feed(&bomb);
            assert_eq!(decoder.decoded().len(), HOLD_LIMIT);
        }
    }

    #[tokio::test]
    async fn hold_first_error_normal_and_raw_cap() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        for (bytes, decision) in [
            (
                b": ping\n\nevent: error\ndata: {\"type\":\"error\"}\n\n".as_slice(),
                CommitDecision::FirstError,
            ),
            (
                b": ping\r\n\r\ndata: {\"type\":\"response.created\"}\r\n\r\n".as_slice(),
                CommitDecision::NormalData,
            ),
        ] {
            let chunks = bytes
                .iter()
                .map(|b| Ok(Bytes::copy_from_slice(&[*b])))
                .collect::<Vec<_>>();
            let mut held = hold(
                StatusCode::OK,
                &headers,
                Box::pin(futures_util::stream::iter(chunks)),
                Duration::from_secs(1),
            )
            .await;
            assert_eq!(held.decision, decision);
            let mut relayed = held.prefix.to_vec();
            if let Some(remainder) = held.remainder {
                relayed.extend_from_slice(&remainder);
            }
            while let Some(chunk) = held.stream.next().await {
                relayed.extend_from_slice(&chunk.unwrap());
            }
            assert_eq!(relayed, bytes);
        }
        let bytes = Bytes::from(vec![b'x'; HOLD_LIMIT + 100]);
        let held = hold(
            StatusCode::OK,
            &headers,
            Box::pin(futures_util::stream::iter([Ok(bytes.clone())])),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(held.decision, CommitDecision::HoldLimit);
        assert_eq!(held.prefix.len(), HOLD_LIMIT);
        assert_eq!(held.remainder.unwrap(), bytes.slice(HOLD_LIMIT..));
    }

    #[tokio::test]
    async fn timeout_and_non_success_do_not_consume_forever() {
        let held = hold(
            StatusCode::OK,
            &HeaderMap::new(),
            Box::pin(futures_util::stream::pending()),
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(held.decision, CommitDecision::HoldTimeout);
        let held = hold(
            StatusCode::UNAUTHORIZED,
            &HeaderMap::new(),
            Box::pin(futures_util::stream::pending()),
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(held.decision, CommitDecision::Status);
    }

    #[tokio::test]
    async fn compressed_sse_decision_and_raw_invariance() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        headers.insert("content-encoding", "zstd".parse().unwrap());
        for (event, decision) in [
            (
                b"data: {\"type\":\"response.created\"}\n\n".as_slice(),
                CommitDecision::NormalData,
            ),
            (
                b"event: error\ndata: {\"type\":\"error\"}\n\n".as_slice(),
                CommitDecision::FirstError,
            ),
        ] {
            let raw = Bytes::from(zstd::stream::encode_all(event, 1).unwrap());
            let chunks = futures_util::stream::iter([Ok(raw.clone())])
                .chain(futures_util::stream::pending());
            let held = hold(
                StatusCode::OK,
                &headers,
                Box::pin(chunks),
                Duration::from_millis(20),
            )
            .await;
            assert_eq!(held.decision, decision);
            assert_eq!(held.prefix, raw);
        }
        let raw = zstd::stream::encode_all(vec![b'x'; HOLD_LIMIT * 2].as_slice(), 1).unwrap();
        assert_eq!(
            decoded_prefix(&raw, SideEncoding::Zstd).unwrap().len(),
            HOLD_LIMIT
        );
    }

    #[tokio::test]
    async fn incomplete_zstd_frame_and_cr_only_events_commit() {
        use std::io::Write;
        let event = b"data: {\"type\":\"response.created\"}\r\r";
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        encoder.write_all(event).unwrap();
        encoder.flush().unwrap();
        let incomplete = Bytes::copy_from_slice(encoder.get_ref());
        assert_eq!(
            decoded_prefix(&incomplete, SideEncoding::Zstd).unwrap(),
            event
        );
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        headers.insert("content-encoding", "zstd".parse().unwrap());
        let chunks: Vec<_> = incomplete
            .iter()
            .map(|byte| Ok(Bytes::copy_from_slice(&[*byte])))
            .collect();
        let stream = futures_util::stream::iter(chunks).chain(futures_util::stream::pending());
        let held = hold(
            StatusCode::OK,
            &headers,
            Box::pin(stream),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(held.decision, CommitDecision::NormalData);
        assert_eq!(held.prefix, incomplete);
    }

    #[tokio::test]
    async fn gzip_deflate_normal_errors_and_bounded_decoded_size() {
        use std::io::Write;
        for encoding in ["gzip", "deflate", "raw-deflate"] {
            for (event, decision) in [
                (
                    b"data: {\"type\":\"response.created\"}\n\n".as_slice(),
                    CommitDecision::NormalData,
                ),
                (
                    b"event: error\ndata: {\"type\":\"error\"}\n\n".as_slice(),
                    CommitDecision::FirstError,
                ),
            ] {
                let compressed = match encoding {
                    "gzip" => {
                        let mut writer =
                            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                        writer.write_all(event).unwrap();
                        writer.finish().unwrap()
                    }
                    "deflate" => {
                        let mut writer = flate2::write::ZlibEncoder::new(
                            Vec::new(),
                            flate2::Compression::fast(),
                        );
                        writer.write_all(event).unwrap();
                        writer.finish().unwrap()
                    }
                    _ => {
                        let mut writer = flate2::write::DeflateEncoder::new(
                            Vec::new(),
                            flate2::Compression::fast(),
                        );
                        writer.write_all(event).unwrap();
                        writer.finish().unwrap()
                    }
                };
                let raw = Bytes::from(compressed);
                let mut headers = HeaderMap::new();
                headers.insert("content-type", "text/event-stream".parse().unwrap());
                headers.insert(
                    "content-encoding",
                    if encoding == "gzip" {
                        "gzip"
                    } else {
                        "deflate"
                    }
                    .parse()
                    .unwrap(),
                );
                let stream = futures_util::stream::iter([Ok(raw.clone())])
                    .chain(futures_util::stream::pending());
                let held = hold(
                    StatusCode::OK,
                    &headers,
                    Box::pin(stream),
                    Duration::from_millis(100),
                )
                .await;
                assert_eq!(held.decision, decision, "{encoding}");
                assert_eq!(held.prefix, raw);
            }
        }
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&vec![b'x'; HOLD_LIMIT * 2]).unwrap();
        let compressed = gzip.finish().unwrap();
        assert_eq!(
            decoded_prefix(&compressed, SideEncoding::Gzip)
                .unwrap()
                .len(),
            HOLD_LIMIT
        );
        let mut deflate = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        deflate.write_all(&vec![b'x'; HOLD_LIMIT * 2]).unwrap();
        let compressed = deflate.finish().unwrap();
        assert_eq!(
            decoded_prefix(&compressed, SideEncoding::Deflate)
                .unwrap()
                .len(),
            HOLD_LIMIT
        );
    }
}
