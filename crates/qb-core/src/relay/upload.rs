use std::{
    io::{self, Cursor, Read},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::guard::{InflightBudget, Reservation};

const COMPRESS_AFTER: usize = 1024 * 1024;
pub(super) const PEEK_LIMIT: usize = 1024 * 1024;
const FORWARD_BATCH: usize = 256 * 1024;

#[cfg(test)]
static TEST_COMPRESS_DELAY_MS: AtomicU64 = AtomicU64::new(0);

pub(super) struct Upload {
    pub prefix: Bytes,
    pub body: reqwest::Body,
    pub retained: Option<RetainedHandle>,
}

#[derive(Clone)]
pub(super) struct RetainedHandle {
    state: Arc<Mutex<State>>,
    done: Arc<Notify>,
}

pub(super) struct RetainedUpload {
    bytes: Bytes,
    _reservations: Vec<Reservation>,
    compressed: bool,
}

impl RetainedUpload {
    #[cfg(test)]
    pub fn bytes(&self) -> Bytes {
        self.bytes.clone()
    }

    pub fn retry_body(&self) -> reqwest::Body {
        if !self.compressed {
            return reqwest::Body::wrap_stream(stream::once(std::future::ready(
                Ok::<_, io::Error>(self.bytes.clone()),
            )));
        }
        let decoder = zstd::stream::read::Decoder::new(Cursor::new(self.bytes.clone()))
            .expect("retained zstd bytes are produced locally");
        let stream = stream::unfold(
            (decoder, vec![0_u8; 32 * 1024]),
            |(mut decoder, mut buffer)| async move {
                match decoder.read(&mut buffer) {
                    Ok(0) => None,
                    Ok(size) => Some((
                        Ok::<_, io::Error>(Bytes::copy_from_slice(&buffer[..size])),
                        (decoder, buffer),
                    )),
                    Err(error) => Some((Err(error), (decoder, buffer))),
                }
            },
        );
        reqwest::Body::wrap_stream(stream)
    }
}

struct State {
    retainer: Option<Retainer>,
    held: Vec<Reservation>,
    cached: Option<Bytes>,
    cached_compressed: bool,
    complete: bool,
    error: bool,
    head: Vec<u8>,
    tail: Vec<u8>,
}

struct Retainer {
    bytes: Vec<u8>,
    reservations: Vec<Reservation>,
    zstd_input: bool,
    compressed: bool,
    cap: usize,
    budget: Arc<InflightBudget>,
    input_bytes: usize,
}

impl Retainer {
    fn new(cap: usize, budget: Arc<InflightBudget>, zstd_input: bool) -> Self {
        Self {
            bytes: Vec::new(),
            reservations: Vec::new(),
            zstd_input,
            compressed: zstd_input,
            cap,
            budget,
            input_bytes: 0,
        }
    }

    fn append(&mut self, chunk: &[u8]) -> bool {
        self.input_bytes = self.input_bytes.saturating_add(chunk.len());
        if !self.zstd_input && self.input_bytes > self.cap {
            return false;
        }
        if self.zstd_input {
            return self.append_bytes(chunk);
        }
        if self.compressed {
            let encoded = match zstd::stream::encode_all(chunk, 1) {
                Ok(encoded) => encoded,
                Err(_) => return false,
            };
            return self.append_bytes(&encoded);
        }
        if self.bytes.len().saturating_add(chunk.len()) <= COMPRESS_AFTER {
            return self.append_bytes(chunk);
        }

        // Stage only the first MiB. Once crossed, replace it with a level-1
        // zstd frame and retain subsequent chunks as independent frames.
        let mut first = self.bytes.clone();
        first.extend_from_slice(chunk);
        let encoded = match zstd::stream::encode_all(first.as_slice(), 1) {
            Ok(v) => v,
            Err(_) => return false,
        };
        if encoded.len() > self.cap {
            return false;
        }
        let Some(reservation) = self.budget.try_reserve(encoded.len()) else {
            return false;
        };
        self.bytes = encoded;
        self.reservations.clear();
        self.reservations.push(reservation);
        self.compressed = true;
        true
    }

    fn append_bytes(&mut self, chunk: &[u8]) -> bool {
        if self.bytes.len().saturating_add(chunk.len()) > self.cap {
            return false;
        }
        let Some(reservation) = self.budget.try_reserve(chunk.len()) else {
            return false;
        };
        self.bytes.extend_from_slice(chunk);
        self.reservations.push(reservation);
        true
    }

    fn finish(self) -> RetainedUpload {
        RetainedUpload {
            bytes: self.bytes.into(),
            _reservations: self.reservations,
            compressed: self.compressed && !self.zstd_input,
        }
    }
}

impl RetainedHandle {
    pub fn metadata(&self) -> Bytes {
        let state = self.state.lock().expect("upload state poisoned");
        if state.tail.len() <= PEEK_LIMIT && state.head.len() == state.tail.len() {
            return state.head.clone().into();
        }
        let mut bytes = state.head.clone();
        if !state.tail.is_empty() {
            bytes.extend_from_slice(&state.tail);
        }
        bytes.into()
    }
    pub fn has_retention(&self) -> bool {
        let state = self.state.lock().expect("upload state poisoned");
        state.retainer.is_some() || state.cached.is_some()
    }
    pub async fn complete(&self) -> Option<RetainedUpload> {
        loop {
            let complete = self.state.lock().expect("upload state poisoned").complete;
            if complete {
                let mut state = self.state.lock().expect("upload state poisoned");
                if state.error {
                    return None;
                }
                if let Some(bytes) = &state.cached {
                    return Some(RetainedUpload {
                        bytes: bytes.clone(),
                        _reservations: Vec::new(),
                        compressed: state.cached_compressed,
                    });
                }
                let retainer = state.retainer.take()?;
                let retained = retainer.finish();
                state.cached = Some(retained.bytes.clone());
                state.cached_compressed = retained.compressed;
                state.held = retained._reservations;
                return Some(RetainedUpload {
                    bytes: retained.bytes,
                    _reservations: Vec::new(),
                    compressed: retained.compressed,
                });
            }
            self.done.notified().await;
        }
    }

    pub async fn retry_body(&self) -> Option<reqwest::Body> {
        self.complete().await.map(|upload| upload.retry_body())
    }
}

pub(super) async fn read(
    body: Body,
    cap: usize,
    count: Arc<AtomicU64>,
    budget: Arc<InflightBudget>,
    zstd_input: bool,
) -> Result<Upload, axum::Error> {
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(8);
    let queue_budget = budget.clone();
    let (retain_tx, mut retain_rx) = mpsc::channel::<(Bytes, Reservation)>(256);
    let (prefix_tx, prefix_rx) = oneshot::channel();
    let state = Arc::new(Mutex::new(State {
        retainer: (cap > 0).then(|| Retainer::new(cap, budget, zstd_input)),
        held: Vec::new(),
        cached: None,
        cached_compressed: false,
        complete: false,
        error: false,
        head: Vec::new(),
        tail: Vec::new(),
    }));
    let done = Arc::new(Notify::new());
    let task_state = state.clone();
    let task_done = done.clone();
    let retain_state = state.clone();
    let retention_task = tokio::spawn(async move {
        while let Some((chunk, queued)) = retain_rx.recv().await {
            if retain_state
                .lock()
                .expect("upload state poisoned")
                .retainer
                .is_none()
            {
                break;
            }
            let state = retain_state.clone();
            let retained = tokio::task::spawn_blocking(move || {
                drop(queued);
                #[cfg(test)]
                {
                    let delay = TEST_COMPRESS_DELAY_MS.load(Ordering::Relaxed);
                    if delay != 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay));
                    }
                }
                let mut state = state.lock().expect("upload state poisoned");
                state
                    .retainer
                    .as_mut()
                    .is_some_and(|retainer| retainer.append(&chunk))
            })
            .await
            .unwrap_or(false);
            if !retained {
                retain_state.lock().expect("upload state poisoned").retainer = None;
            }
        }
    });
    tokio::spawn(async move {
        let mut input = body.into_data_stream();
        let mut input = Box::pin(async_stream::stream! {
            let mut pending = BytesMut::new();
            while let Some(chunk) = input.next().await {
                match chunk {
                    Ok(chunk) => {
                        pending.extend_from_slice(&chunk);
                        while pending.len() >= FORWARD_BATCH {
                            yield Ok::<Bytes, axum::Error>(pending.split().freeze());
                        }
                    }
                    Err(error) => {
                        if !pending.is_empty() {
                            yield Ok::<Bytes, axum::Error>(pending.split().freeze());
                        }
                        yield Err(error);
                        return;
                    }
                }
            }
            if !pending.is_empty() {
                yield Ok::<Bytes, axum::Error>(pending.freeze());
            }
        });
        let mut prefix = Vec::new();
        let mut prefix_tx = Some(prefix_tx);
        while let Some(chunk) = input.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    {
                        let mut state = task_state.lock().expect("upload state poisoned");
                        state.error = true;
                    }
                    let _ = tx.send(Err(io::Error::other(error.to_string()))).await;
                    break;
                }
            };
            count.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            if prefix.len() < PEEK_LIMIT {
                let take = (PEEK_LIMIT - prefix.len()).min(chunk.len());
                prefix.extend_from_slice(&chunk[..take]);
            }
            {
                let mut state = task_state.lock().expect("upload state poisoned");
                if state.head.len() < PEEK_LIMIT {
                    let take = (PEEK_LIMIT - state.head.len()).min(chunk.len());
                    state.head.extend_from_slice(&chunk[..take]);
                }
                state.tail.extend_from_slice(&chunk);
                if state.tail.len() > PEEK_LIMIT {
                    let excess = state.tail.len() - PEEK_LIMIT;
                    state.tail.drain(..excess);
                }
            }
            if let Some(sender) = prefix_tx.take() {
                let _ = sender.send(Bytes::from(prefix.clone()));
            }
            let forwarded = tx.send(Ok(chunk.clone())).await.is_ok();
            let Some(queued) = queue_budget.try_reserve(chunk.len()) else {
                task_state.lock().expect("upload state poisoned").retainer = None;
                continue;
            };
            if retain_tx.try_send((chunk, queued)).is_err() {
                task_state.lock().expect("upload state poisoned").retainer = None;
            }
            if !forwarded {
                // The upstream response arrived early; continue draining the
                // client body so retention can finish without blocking it.
                continue;
            }
        }
        if let Some(sender) = prefix_tx {
            let _ = sender.send(Bytes::from(prefix));
        }
        drop(tx);
        drop(retain_tx);
        let _ = retention_task.await;
        task_state.lock().expect("upload state poisoned").complete = true;
        task_done.notify_waiters();
    });
    let body = reqwest::Body::wrap_stream(stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let prefix = prefix_rx
        .await
        .map_err(|_| axum::Error::new("body producer stopped"))?;
    Ok(Upload {
        prefix,
        body,
        retained: (cap > 0).then_some(RetainedHandle { state, done }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::time::Duration;

    async fn collect(body: reqwest::Body) -> Vec<u8> {
        let mut out = Vec::new();
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn tee_forwards_and_retains_small_plain_body() {
        let budget = InflightBudget::new(1024);
        let upload = read(
            Body::from("onetwo"),
            1024,
            Arc::new(AtomicU64::new(0)),
            budget.clone(),
            false,
        )
        .await
        .unwrap();
        let retained_handle = upload.retained.unwrap();
        let retained = retained_handle.complete();
        let (forwarded, retained) = tokio::join!(collect(upload.body), retained);
        assert_eq!(forwarded, b"onetwo");
        assert_eq!(retained.unwrap().bytes(), Bytes::from_static(b"onetwo"));
        assert_eq!(budget.used(), 6);
    }

    #[tokio::test]
    async fn budget_drop_still_forwards_oversized_plain_body() {
        let body = Bytes::from(vec![b'x'; COMPRESS_AFTER + 1]);
        let upload = read(
            Body::from(body.clone()),
            body.len(),
            Arc::new(AtomicU64::new(0)),
            InflightBudget::new(1),
            false,
        )
        .await
        .unwrap();
        let retained_handle = upload.retained.unwrap();
        let retained = retained_handle.complete();
        let (forwarded, retained) = tokio::join!(collect(upload.body), retained);
        assert_eq!(forwarded, body);
        assert!(retained.is_none());
    }

    #[tokio::test]
    async fn incoming_zstd_is_retained_byte_identically() {
        let input = Bytes::from_static(b"already compressed bytes");
        let upload = read(
            Body::from(input.clone()),
            1024,
            Arc::new(AtomicU64::new(0)),
            InflightBudget::new(1024),
            true,
        )
        .await
        .unwrap();
        let retained_handle = upload.retained.unwrap();
        let retained = retained_handle.complete();
        let (forwarded, retained) = tokio::join!(collect(upload.body), retained);
        assert_eq!(forwarded, input);
        assert_eq!(retained.unwrap().bytes(), input);
    }

    #[tokio::test]
    async fn cap_drop_forwards_and_plain_retry_decompresses() {
        let input = Bytes::from(vec![b'x'; COMPRESS_AFTER + 1]);
        let upload = read(
            Body::from(input.clone()),
            2,
            Arc::new(AtomicU64::new(0)),
            InflightBudget::new(1024),
            false,
        )
        .await
        .unwrap();
        let retained_handle = upload.retained.unwrap();
        let retained = retained_handle.complete();
        let (forwarded, retained) = tokio::join!(collect(upload.body), retained);
        assert_eq!(forwarded, input);
        assert!(retained.is_none());

        let input = Bytes::from(vec![b'y'; COMPRESS_AFTER + 1]);
        let upload = read(
            Body::from(input.clone()),
            input.len(),
            Arc::new(AtomicU64::new(0)),
            InflightBudget::new(input.len() + 1024),
            false,
        )
        .await
        .unwrap();
        let retained_handle = upload.retained.unwrap();
        let retained = retained_handle.complete();
        let (forwarded, retained) = tokio::join!(collect(upload.body), retained);
        assert_eq!(forwarded, input);
        assert_eq!(collect(retained.unwrap().retry_body()).await, input);
    }

    #[tokio::test]
    async fn m7b_plain_body_over_1mib_retained_compressed_and_zstd_retained_as_is() {
        let plain = Bytes::from(vec![b'p'; COMPRESS_AFTER + 4096]);
        let budget = InflightBudget::new(plain.len() * 2);
        let upload = read(
            Body::from(plain.clone()),
            plain.len() * 2,
            Arc::new(AtomicU64::new(0)),
            budget.clone(),
            false,
        )
        .await
        .unwrap();
        let retained = upload.retained.as_ref().unwrap().complete().await.unwrap();
        assert_eq!(collect(upload.body).await, plain);
        assert!(retained.compressed);
        assert!(retained.bytes.len() < plain.len());
        assert_eq!(
            zstd::stream::decode_all(&retained.bytes[..]).unwrap(),
            plain
        );

        let encoded = Bytes::from(zstd::stream::encode_all(&plain[..], 1).unwrap());
        let upload = read(
            Body::from(encoded.clone()),
            encoded.len() + 1,
            Arc::new(AtomicU64::new(0)),
            InflightBudget::new(encoded.len() + 1),
            true,
        )
        .await
        .unwrap();
        let retained = upload.retained.as_ref().unwrap().complete().await.unwrap();
        assert_eq!(collect(upload.body).await, encoded);
        assert!(!retained.compressed);
        assert_eq!(retained.bytes, encoded);
    }

    #[tokio::test]
    async fn m7b_over_cap_body_forwarded_without_failover() {
        let input = Bytes::from(vec![b'x'; COMPRESS_AFTER + 8192]);
        let upload = read(
            Body::from(input.clone()),
            COMPRESS_AFTER,
            Arc::new(AtomicU64::new(0)),
            InflightBudget::new(input.len()),
            false,
        )
        .await
        .unwrap();
        let handle = upload.retained.as_ref().unwrap().clone();
        assert_eq!(collect(upload.body).await, input);
        assert!(handle.complete().await.is_none());
        assert!(handle.retry_body().await.is_none());
    }

    #[tokio::test]
    async fn m7b_budget_exhaustion_mid_body_drops_retention_without_stall() {
        let input = Bytes::from(vec![b'b'; COMPRESS_AFTER + 8192]);
        let chunks = input
            .chunks(4096)
            .map(|chunk| Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();
        let upload = read(
            Body::from_stream(futures_util::stream::iter(chunks)),
            input.len(),
            Arc::new(AtomicU64::new(0)),
            InflightBudget::new(1024),
            false,
        )
        .await
        .unwrap();
        let handle = upload.retained.as_ref().unwrap().clone();
        assert_eq!(collect(upload.body).await, input);
        assert!(handle.complete().await.is_none());
    }

    #[tokio::test]
    async fn m7b_retry_invariance_plain_and_zstd() {
        let plain = Bytes::from(vec![b'a'; COMPRESS_AFTER + 1234]);
        for (input, zstd_input) in [
            (plain.clone(), false),
            (
                Bytes::from(zstd::stream::encode_all(&plain[..], 1).unwrap()),
                true,
            ),
        ] {
            let upload = read(
                Body::from(input.clone()),
                input.len() * 2,
                Arc::new(AtomicU64::new(0)),
                InflightBudget::new(input.len() * 2),
                zstd_input,
            )
            .await
            .unwrap();
            let handle = upload.retained.as_ref().unwrap().clone();
            let first = collect(upload.body).await;
            let retry = collect(handle.retry_body().await.unwrap()).await;
            assert_eq!(first, input);
            assert_eq!(retry, input);
        }
    }

    #[tokio::test]
    async fn m7b_slow_compressor_never_delays_forwarding_and_drops_only_retention() {
        TEST_COMPRESS_DELAY_MS.store(250, Ordering::Relaxed);
        let chunks = (0..512)
            .map(|_| Ok::<_, io::Error>(Bytes::from(vec![b'x'; 64 * 1024])))
            .collect::<Vec<_>>();
        let input = chunks
            .iter()
            .flat_map(|chunk| chunk.as_ref().unwrap().iter().copied())
            .collect::<Vec<_>>();
        let upload = read(
            Body::from_stream(stream::iter(chunks)),
            8 * 1024 * 1024,
            Arc::new(AtomicU64::new(0)),
            InflightBudget::new(8 * 1024 * 1024),
            false,
        )
        .await
        .unwrap();
        let forwarded = tokio::time::timeout(Duration::from_millis(100), collect(upload.body))
            .await
            .expect("forwarding must not wait for compression");
        assert_eq!(forwarded, input);
        assert!(upload.retained.unwrap().complete().await.is_none());
        TEST_COMPRESS_DELAY_MS.store(0, Ordering::Relaxed);
    }
}
