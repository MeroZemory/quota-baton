use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::body::Body;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

type InputStream = Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>;

pub(super) struct Upload {
    pub prefix: Bytes,
    pub overflow: Option<InputStream>,
}

impl Upload {
    pub fn into_body(self) -> reqwest::Body {
        let prefix =
            futures_util::stream::once(std::future::ready(Ok::<_, axum::Error>(self.prefix)));
        match self.overflow {
            Some(rest) => reqwest::Body::wrap_stream(prefix.chain(rest)),
            None => reqwest::Body::wrap_stream(prefix),
        }
    }
}

pub(super) async fn read(
    body: Body,
    cap: usize,
    count: Arc<AtomicU64>,
) -> Result<Upload, axum::Error> {
    let mut stream: InputStream = Box::pin(body.into_data_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            count.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        chunk
    }));
    let mut prefix = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let take = chunk.len().min(cap - prefix.len());
        prefix.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            // 한도까지만 모으고 이미 읽은 청크의 나머지와 클라이언트 스트림을 이어 보낸다.
            let remainder = futures_util::stream::once(std::future::ready(Ok(chunk.slice(take..))));
            return Ok(Upload {
                prefix: prefix.into(),
                overflow: Some(Box::pin(remainder.chain(stream))),
            });
        }
    }
    Ok(Upload {
        prefix: prefix.into(),
        overflow: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn overflow_read_bounds_prefix_and_leaves_tail_unpolled() {
        let count = Arc::new(AtomicU64::new(0));
        let chunks = [
            Bytes::from_static(b"12345678"),
            Bytes::from_static(b"abcdefghijklmnop"),
            Bytes::from_static(b"tail"),
        ];
        let body = Body::from_stream(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, std::io::Error>),
        ));
        let upload = read(body, 16, count.clone()).await.unwrap();
        assert_eq!(&upload.prefix[..], b"12345678abcdefgh");
        assert_eq!(count.load(Ordering::Relaxed), 24);
        let mut tail = upload.overflow.unwrap();
        assert_eq!(tail.next().await.unwrap().unwrap(), "ijklmnop");
        assert_eq!(tail.next().await.unwrap().unwrap(), "tail");
        assert!(tail.next().await.is_none());
        assert_eq!(count.load(Ordering::Relaxed), 28);
    }
    #[tokio::test]
    async fn exact_cap_retains_and_zero_cap_streams() {
        let upload = read(Body::from("1234"), 4, Arc::new(AtomicU64::new(0)))
            .await
            .unwrap();
        assert_eq!(upload.prefix.len(), 4);
        assert!(upload.overflow.is_none());
        let upload = read(Body::from("1234"), 0, Arc::new(AtomicU64::new(0)))
            .await
            .unwrap();
        assert!(upload.prefix.is_empty());
        assert!(upload.overflow.is_some());
    }
}
