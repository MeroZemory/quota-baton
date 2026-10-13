use std::{
    io::{self, Write},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
};
use bytes::Bytes;
use futures_util::{FutureExt, Stream, StreamExt};
use tokio::sync::mpsc;

use crate::relay::{
    hold::{HOLD_LIMIT, HeldResponse},
    ledger::Completion,
};

const EOF_GRACE: Duration = Duration::from_millis(50);

struct Receiver(mpsc::Receiver<Result<Bytes, io::Error>>);

impl Stream for Receiver {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

pub(in crate::relay) fn body(
    mut completion: Completion,
    held: HeldResponse,
    status: StatusCode,
    response_headers: &HeaderMap,
    success_outcome: &str,
) -> Body {
    completion.record.status = status.as_u16();
    let success_outcome = success_outcome.to_owned();
    let mut observer = Observer::new(response_headers);
    // 한 조각만 대기시킨다. EOF 확인은 소비자의 다음 body poll과 독립적이다.
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let HeldResponse {
            prefix,
            remainder,
            mut stream,
            terminal,
            ..
        } = held;
        let initial = (!prefix.is_empty())
            .then_some(prefix)
            .into_iter()
            .chain(remainder);
        let mut pending = initial.map(Ok).collect::<std::collections::VecDeque<_>>();
        let mut terminal = terminal;
        let mut deadline = None;
        loop {
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                return;
            }
            let next = if let Some(chunk) = pending.pop_front() {
                Some(chunk)
            } else if let Some(error) = terminal.take() {
                Some(Err(error))
            } else if tx.is_closed() && !observer.terminal() {
                // 즉시 준비된 EOF만 확인한다. 종료 전 client drop은 더 읽지 않는다.
                match stream.next().now_or_never() {
                    Some(None) => None,
                    _ => return,
                }
            } else {
                if tx.is_closed() {
                    deadline.get_or_insert_with(|| tokio::time::Instant::now() + EOF_GRACE);
                }
                if let Some(deadline) = deadline {
                    match tokio::time::timeout_at(deadline, stream.next()).await {
                        Ok(next) => next.map(|item| item.map_err(|_| stream_error())),
                        Err(_) => return,
                    }
                } else {
                    tokio::select! {
                        biased;
                        _ = tx.closed() => {
                            if observer.terminal() {
                                deadline = Some(tokio::time::Instant::now() + EOF_GRACE);
                                continue;
                            }
                            // 마지막 바이트와 EOF가 동시에 준비된 비 SSE 응답도 완료한다.
                            if matches!(stream.next().now_or_never(), Some(None)) {
                                completion.finish(&success_outcome);
                            }
                            return;
                        }
                        next = stream.next() => next.map(|item| item.map_err(|_| stream_error())),
                    }
                }
            };
            let Some(chunk) = next else {
                completion.finish(&success_outcome);
                return;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    completion.finish("upstream_stream_error");
                    let _ = tx.send(Err(stream_error())).await;
                    return;
                }
            };
            observer.feed(&chunk);
            if tx.is_closed() {
                if !observer.terminal() {
                    return;
                }
                deadline.get_or_insert_with(|| tokio::time::Instant::now() + EOF_GRACE);
                // 종료 이벤트 뒤의 EOF만 관찰하며 원본 데이터를 별도로 보관하지 않는다.
                continue;
            }
            let len = chunk.len() as u64;
            if tx.send(Ok(chunk)).await.is_err() {
                if !observer.terminal() {
                    return;
                }
                deadline = Some(tokio::time::Instant::now() + EOF_GRACE);
            } else {
                completion.record.bytes_out += len;
            }
        }
    });
    Body::from_stream(Receiver(rx))
}

fn stream_error() -> io::Error {
    io::Error::other("Upstream response stream interrupted")
}

struct Observer {
    decoder: Option<Decoder>,
}

impl Observer {
    fn new(headers: &HeaderMap) -> Self {
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
        let mut encodings = headers.get_all("content-encoding").iter();
        let encoding = match encodings.next() {
            Some(value) => value.to_str().unwrap_or("unsupported"),
            None => "identity",
        };
        let decoder = if sse && encodings.next().is_none() {
            Decoder::new(encoding.trim()).ok()
        } else {
            None
        };
        Self { decoder }
    }

    fn feed(&mut self, bytes: &[u8]) {
        if let Some(decoder) = &mut self.decoder
            && decoder.feed(bytes).is_err()
        {
            self.decoder = None;
        }
    }

    fn terminal(&self) -> bool {
        self.decoder
            .as_ref()
            .is_some_and(|decoder| decoder.sink().terminal)
    }
}

#[derive(Default)]
struct EventSink {
    event: Vec<u8>,
    line_len: usize,
    after_cr: bool,
    oversized: bool,
    terminal: bool,
}

impl EventSink {
    fn classify(&mut self) {
        // 큰 완료 이벤트도 접두부의 event 이름은 확인한다. JSON은 완전한 이벤트만 파싱한다.
        for line in self.event.split(|b| matches!(b, b'\r' | b'\n')) {
            if let Some(value) = line.strip_prefix(b"event:") {
                self.terminal |= terminal_kind(value.trim_ascii());
            }
        }
        if !self.oversized {
            let mut data = Vec::new();
            for line in self.event.split(|b| matches!(b, b'\r' | b'\n')) {
                if let Some(value) = line.strip_prefix(b"data:") {
                    if !data.is_empty() {
                        data.push(b'\n');
                    }
                    data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
                }
            }
            #[derive(serde::Deserialize)]
            struct Kind {
                r#type: Option<String>,
            }
            if let Ok(kind) = serde_json::from_slice::<Kind>(&data) {
                self.terminal |= kind
                    .r#type
                    .is_some_and(|kind| terminal_kind(kind.as_bytes()));
            }
        }
        self.event.clear();
        self.oversized = false;
    }
}

fn terminal_kind(kind: &[u8]) -> bool {
    matches!(
        kind,
        b"response.completed" | b"response.done" | b"message_stop"
    )
}

impl Write for EventSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for &byte in bytes {
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if self.event.len() < HOLD_LIMIT {
                self.event.push(byte);
            } else {
                self.oversized = true;
            }
            if matches!(byte, b'\r' | b'\n') {
                if self.line_len == 0 {
                    self.classify();
                }
                self.line_len = 0;
            } else {
                self.line_len = self.line_len.saturating_add(1);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// 압축 출력은 이벤트 scanner로 바로 보내며 전체 스트림을 누적하지 않는다.
enum Decoder {
    Identity(EventSink),
    Gzip(flate2::write::MultiGzDecoder<EventSink>),
    DeflateHeader(Vec<u8>),
    Zlib(flate2::write::ZlibDecoder<EventSink>),
    RawDeflate(flate2::write::DeflateDecoder<EventSink>),
    Brotli {
        decoder: Box<brotli::DecompressorWriter<EventSink>>,
        checked: bool,
    },
    Zstd(zstd::stream::write::Decoder<'static, EventSink>),
}

impl Decoder {
    fn new(encoding: &str) -> io::Result<Self> {
        let sink = EventSink::default();
        Ok(match encoding.to_ascii_lowercase().as_str() {
            "identity" => Self::Identity(sink),
            "gzip" => Self::Gzip(flate2::write::MultiGzDecoder::new(sink)),
            "deflate" => Self::DeflateHeader(Vec::with_capacity(2)),
            "br" => Self::Brotli {
                decoder: Box::new(brotli::DecompressorWriter::new(sink, 4096)),
                checked: false,
            },
            "zstd" => {
                let mut decoder = zstd::stream::write::Decoder::new(sink)?;
                decoder.window_log_max(23)?;
                Self::Zstd(decoder)
            }
            _ => return Err(io::Error::other("Unsupported side encoding")),
        })
    }

    fn feed(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if let Self::Brotli { checked, .. } = self
            && !*checked
            && let Some(first) = bytes.first()
        {
            // large-window 확장은 할당 전에 거부한다.
            if first & 0x7f == 0x11 {
                return Err(io::Error::other("Unsupported Brotli window"));
            }
            *checked = true;
        }
        if let Self::DeflateHeader(header) = self {
            let take = bytes.len().min(2 - header.len());
            header.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if header.len() < 2 {
                return Ok(());
            }
            let initial = [header[0], header[1]];
            let zlib = initial[0] & 15 == 8
                && initial[0] >> 4 <= 7
                && u16::from_be_bytes(initial) % 31 == 0;
            *self = if zlib {
                Self::Zlib(flate2::write::ZlibDecoder::new(EventSink::default()))
            } else {
                Self::RawDeflate(flate2::write::DeflateDecoder::new(EventSink::default()))
            };
            self.feed(&initial)?;
        }
        let writer: &mut dyn Write = match self {
            Self::Identity(sink) => sink,
            Self::Gzip(decoder) => decoder,
            Self::Zlib(decoder) => decoder,
            Self::RawDeflate(decoder) => decoder,
            Self::Brotli { decoder, .. } => decoder.as_mut(),
            Self::Zstd(decoder) => decoder,
            Self::DeflateHeader(_) => unreachable!(),
        };
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn sink(&self) -> &EventSink {
        match self {
            Self::Identity(sink) => sink,
            Self::Gzip(decoder) => decoder.get_ref(),
            Self::Zlib(decoder) => decoder.get_ref(),
            Self::RawDeflate(decoder) => decoder.get_ref(),
            Self::Brotli { decoder, .. } => decoder.get_ref(),
            Self::Zstd(decoder) => decoder.get_ref(),
            Self::DeflateHeader(_) => {
                static EMPTY: EventSink = EventSink {
                    event: Vec::new(),
                    line_len: 0,
                    after_cr: false,
                    oversized: false,
                    terminal: false,
                };
                &EMPTY
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::Provider,
        observability::Observability,
        relay::hold::{CommitDecision, RawStream},
    };
    use axum::http::Method;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct Fixture {
        dir: tempfile::TempDir,
        obs: Arc<Observability>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.sandbox/a1");
            std::fs::create_dir_all(&root).unwrap();
            let dir = tempfile::tempdir_in(root).unwrap();
            let obs = Arc::new(Observability::new(dir.path(), None).unwrap());
            Self { dir, obs }
        }

        fn body(&self, stream: RawStream, headers: &HeaderMap, outcome: &str) -> Body {
            body(
                Completion::new(
                    self.obs.clone(),
                    &Method::POST,
                    "/v1/responses",
                    Provider::Codex,
                    &HeaderMap::new(),
                ),
                HeldResponse {
                    prefix: Bytes::new(),
                    remainder: None,
                    stream,
                    terminal: None,
                    decision: CommitDecision::NormalData,
                },
                StatusCode::OK,
                headers,
                outcome,
            )
        }

        fn row(&self) -> Option<serde_json::Value> {
            self.obs.flush().unwrap();
            std::fs::read_dir(self.dir.path().join("ledger"))
                .unwrap()
                .filter_map(Result::ok)
                .find_map(|entry| {
                    let text = std::fs::read_to_string(entry.path()).unwrap();
                    text.lines()
                        .next()
                        .map(|line| serde_json::from_str(line).unwrap())
                })
        }

        async fn outcome(&self) -> String {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(row) = self.row() {
                        return row["outcome"].as_str().unwrap().to_owned();
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap()
        }
    }

    fn sse(encoding: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        headers.insert("content-encoding", encoding.parse().unwrap());
        headers
    }

    fn compress(bytes: &[u8], encoding: &str) -> Vec<u8> {
        match encoding {
            "identity" => bytes.to_vec(),
            "gzip" => {
                let mut writer =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                writer.write_all(bytes).unwrap();
                writer.finish().unwrap()
            }
            "deflate" => {
                let mut writer =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                writer.write_all(bytes).unwrap();
                writer.finish().unwrap()
            }
            "raw-deflate" => {
                let mut writer =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
                writer.write_all(bytes).unwrap();
                writer.finish().unwrap()
            }
            "zstd" => zstd::stream::encode_all(bytes, 1).unwrap(),
            "br" => {
                let mut output = Vec::new();
                {
                    let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 3, 22);
                    writer.write_all(bytes).unwrap();
                }
                output
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn oversized_named_terminal_event_keeps_bounded_observation_and_eof_grace() {
        let fixture = Fixture::new();
        let mut event = b"event: response.completed\ndata: ".to_vec();
        event.extend(std::iter::repeat_n(b'x', HOLD_LIMIT * 2));
        event.extend_from_slice(b"\n\n");
        let raw = Bytes::from(event);
        let first = raw.clone();
        let stream = async_stream::stream! {
            yield Ok(first);
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let mut body = fixture
            .body(Box::pin(stream), &sse("identity"), "ok")
            .into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap(), raw);
        drop(body);
        assert_eq!(fixture.outcome().await, "ok");
    }

    #[tokio::test]
    async fn last_chunk_then_drop_records_normal_eof_without_another_body_poll() {
        for kind in ["response.completed", "response.done", "message_stop"] {
            for encoding in ["identity", "gzip", "deflate", "raw-deflate", "br", "zstd"] {
                let fixture = Fixture::new();
                let raw = Bytes::from(compress(
                    format!("data: {{\"type\":\"{kind}\"}}\n\n").as_bytes(),
                    encoding,
                ));
                let headers = sse(if encoding == "raw-deflate" {
                    "deflate"
                } else {
                    encoding
                });
                let upstream = futures_util::stream::iter([Ok(raw.clone())]);
                let mut stream = fixture
                    .body(Box::pin(upstream), &headers, "ok")
                    .into_data_stream();
                assert_eq!(stream.next().await.unwrap().unwrap(), raw);
                drop(stream);
                assert_eq!(fixture.outcome().await, "ok", "{kind}/{encoding}");
            }
        }
        let fixture = Fixture::new();
        let upstream = futures_util::stream::iter([Ok(Bytes::from_static(b"plain response"))]);
        let mut stream = fixture
            .body(Box::pin(upstream), &HeaderMap::new(), "ok")
            .into_data_stream();
        stream.next().await.unwrap().unwrap();
        drop(stream);
        assert_eq!(fixture.outcome().await, "ok");
    }

    struct TrackedStream {
        inner: RawStream,
        dropped: Arc<AtomicBool>,
    }

    impl Stream for TrackedStream {
        type Item = Result<Bytes, reqwest::Error>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.inner.as_mut().poll_next(cx)
        }
    }
    impl Drop for TrackedStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn drop_before_terminal_and_stalled_terminal_cancel_upstream() {
        for terminal in [false, true] {
            let fixture = Fixture::new();
            let chunk = Bytes::from_static(if terminal {
                b"event: response.done\ndata: {}\n\n"
            } else {
                b"data: {}\n\n"
            });
            let dropped = Arc::new(AtomicBool::new(false));
            let inner = futures_util::stream::iter([Ok(chunk.clone())])
                .chain(futures_util::stream::pending());
            let upstream = TrackedStream {
                inner: Box::pin(inner),
                dropped: dropped.clone(),
            };
            let mut stream = fixture
                .body(Box::pin(upstream), &sse("identity"), "ok")
                .into_data_stream();
            assert_eq!(stream.next().await.unwrap().unwrap(), chunk);
            drop(stream);
            assert_eq!(fixture.outcome().await, "client_drop");
            assert!(dropped.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn terminal_drop_allows_only_short_eof_grace() {
        let fixture = Fixture::new();
        let upstream = async_stream::stream! {
            yield Ok(Bytes::from_static(b"event: message_stop\ndata: {}\n\n"));
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let mut stream = fixture
            .body(Box::pin(upstream), &sse("identity"), "ok")
            .into_data_stream();
        stream.next().await.unwrap().unwrap();
        drop(stream);
        assert_eq!(fixture.outcome().await, "ok");
    }

    #[tokio::test]
    async fn compressed_full_stream_scanner_is_bounded_and_raw_bytes_are_unchanged() {
        // 64 KiB를 넘는 전체 스트림과 단일 과대 이벤트 뒤에도 terminal을 찾는다.
        let mut events = b"data: ".to_vec();
        events.extend(vec![b'x'; HOLD_LIMIT + 100]);
        events.extend_from_slice(b"\r\n\r\n");
        for _ in 0..10_000 {
            events.extend_from_slice(b": ping\r\n\r\n");
        }
        events.extend_from_slice(b"event: response.completed\r\ndata: {}\r\n\r\n");
        for encoding in ["identity", "gzip", "deflate", "raw-deflate", "br", "zstd"] {
            let raw = compress(&events, encoding);
            let headers = sse(if encoding == "raw-deflate" {
                "deflate"
            } else {
                encoding
            });
            let mut observer = Observer::new(&headers);
            for byte in &raw {
                observer.feed(std::slice::from_ref(byte));
                assert!(observer.decoder.as_ref().unwrap().sink().event.len() <= HOLD_LIMIT);
            }
            assert!(observer.terminal(), "{encoding}");
            let fixture = Fixture::new();
            let chunks: Vec<_> = raw
                .chunks(7)
                .map(|bytes| Ok(Bytes::copy_from_slice(bytes)))
                .collect();
            let mut stream = fixture
                .body(Box::pin(futures_util::stream::iter(chunks)), &headers, "ok")
                .into_data_stream();
            let mut received = Vec::new();
            while received.len() < raw.len() {
                received.extend_from_slice(&stream.next().await.unwrap().unwrap());
            }
            drop(stream);
            assert_eq!(received, raw, "{encoding}");
            assert_eq!(fixture.outcome().await, "ok", "{encoding}");
        }
    }

    #[tokio::test]
    async fn eof_records_ledger_before_body_eof_and_preserves_first_error_outcome() {
        for outcome in ["ok", "first_event_error", "upstream_status_error"] {
            let fixture = Fixture::new();
            let upstream = futures_util::stream::iter([Ok(Bytes::from_static(b"data: {}\n\n"))]);
            let mut stream = fixture
                .body(Box::pin(upstream), &sse("identity"), outcome)
                .into_data_stream();
            while stream.next().await.is_some() {}
            assert_eq!(fixture.row().unwrap()["outcome"], outcome);
        }
    }

    #[tokio::test]
    async fn unpolled_body_drop_cancels_pending_upstream() {
        let fixture = Fixture::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let upstream = TrackedStream {
            inner: Box::pin(futures_util::stream::pending()),
            dropped: dropped.clone(),
        };
        drop(fixture.body(Box::pin(upstream), &sse("identity"), "ok"));
        assert_eq!(fixture.outcome().await, "client_drop");
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn producer_backpressure_bounds_upstream_reads() {
        let fixture = Fixture::new();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = polls.clone();
        let upstream = futures_util::stream::poll_fn(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Some(Ok(Bytes::from_static(b"data: {}\n\n"))))
        });
        let body = fixture.body(Box::pin(upstream), &sse("identity"), "ok");
        tokio::time::sleep(Duration::from_millis(10)).await;
        // 채널의 한 조각과 send 대기 중인 한 조각까지만 upstream에서 읽는다.
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        drop(body);
        assert_eq!(fixture.outcome().await, "client_drop");
        assert_eq!(polls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn post_commit_errors_are_sanitized_and_recorded_before_body_eof() {
        let fixture = Fixture::new();
        let error = reqwest::Client::new()
            .get("http://[private-upstream-secret")
            .build()
            .unwrap_err();
        let upstream =
            futures_util::stream::iter([Ok(Bytes::from_static(b"data: {}\n\n")), Err(error)]);
        let mut stream = fixture
            .body(Box::pin(upstream), &sse("identity"), "ok")
            .into_data_stream();
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            b"data: {}\n\n".as_slice()
        );
        let error = stream.next().await.unwrap().unwrap_err().to_string();
        assert!(error.contains("Upstream response stream interrupted"));
        assert!(!error.contains("private-upstream-secret"));
        assert!(stream.next().await.is_none());
        assert_eq!(fixture.row().unwrap()["outcome"], "upstream_stream_error");
    }

    #[tokio::test]
    async fn held_prefix_remainder_and_terminal_error_preserve_wire_order() {
        let fixture = Fixture::new();
        let held = HeldResponse {
            prefix: Bytes::from_static(b"prefix"),
            remainder: Some(Bytes::from_static(b"remainder")),
            stream: Box::pin(futures_util::stream::pending()),
            terminal: Some(io::Error::other("private held upstream error")),
            decision: CommitDecision::StreamError,
        };
        let completion = Completion::new(
            fixture.obs.clone(),
            &Method::POST,
            "/v1/responses",
            Provider::Codex,
            &HeaderMap::new(),
        );
        let mut stream =
            body(completion, held, StatusCode::OK, &HeaderMap::new(), "ok").into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap(), b"prefix".as_slice());
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            b"remainder".as_slice()
        );
        assert!(
            !stream
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("private")
        );
        assert!(stream.next().await.is_none());
        let row = fixture.row().unwrap();
        assert_eq!(row["outcome"], "upstream_stream_error");
        assert_eq!(row["bytes_out"], 15);
    }
}
