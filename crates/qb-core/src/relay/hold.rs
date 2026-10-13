use std::{
    io::{self, Read},
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
pub(super) fn sanitized_stream_error(_: reqwest::Error) -> io::Error {
    stream_error()
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
    let identity = headers
        .get("content-encoding")
        .is_none_or(|v| v.as_bytes().eq_ignore_ascii_case(b"identity"));
    let encoding = headers.get("content-encoding").and_then(|value| {
        let value = value.as_bytes().trim_ascii();
        if value.eq_ignore_ascii_case(b"zstd") {
            Some(SideEncoding::Zstd)
        } else if value.eq_ignore_ascii_case(b"gzip") {
            Some(SideEncoding::Gzip)
        } else if value.eq_ignore_ascii_case(b"deflate") {
            Some(SideEncoding::Deflate)
        } else {
            None
        }
    });
    let deadline = tokio::time::Instant::now() + timeout;
    let mut scanner = EventScanner::default();
    let decision = if !status.is_success() {
        CommitDecision::Status
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
                    if identity && let Some(decision) = scanner.scan(&prefix) {
                        break decision;
                    }
                    if let Some(encoding) = encoding
                        && let Some(decoded) = decoded_prefix(&prefix, encoding)
                    {
                        if let Some(decision) = EventScanner::default().scan(&decoded) {
                            break decision;
                        }
                        if decoded.len() == HOLD_LIMIT {
                            break CommitDecision::HoldLimit;
                        }
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

// 불완전한 frame에서도 읽힌 부분만 검사한다. raw/decoded 출력은 각각 64 KiB 이하이다.
// decoder window는 8 MiB로 제한하여 작은 압축 입력의 거대한 window 요구를 거부한다.
#[derive(Clone, Copy)]
enum SideEncoding {
    Zstd,
    Gzip,
    Deflate,
}

fn decoded_prefix(raw: &[u8], encoding: SideEncoding) -> Option<Vec<u8>> {
    match encoding {
        SideEncoding::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::new(raw).ok()?;
            decoder.window_log_max(23).ok()?;
            Some(read_bounded(decoder))
        }
        SideEncoding::Gzip => Some(read_bounded(flate2::read::MultiGzDecoder::new(raw))),
        SideEncoding::Deflate => {
            let decoded = read_bounded(flate2::read::ZlibDecoder::new(raw));
            // HTTP deflate의 zlib wrapper가 없는 서버도 side scan만 허용한다.
            Some(if decoded.is_empty() {
                read_bounded(flate2::read::DeflateDecoder::new(raw))
            } else {
                decoded
            })
        }
    }
}

fn read_bounded(mut decoder: impl Read) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(HOLD_LIMIT);
    let mut scratch = [0u8; 4096];
    while decoded.len() < HOLD_LIMIT {
        let take = scratch.len().min(HOLD_LIMIT - decoded.len());
        match decoder.read(&mut scratch[..take]) {
            Ok(0) | Err(_) => break,
            Ok(count) => decoded.extend_from_slice(&scratch[..count]),
        }
    }
    decoded
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
    use super::*;

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
