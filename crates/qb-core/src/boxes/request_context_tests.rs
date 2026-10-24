//! Focused context propagation tests; no native transport or service claim.
use super::*;
use crate::boxes::{BoxAuthority, BoxName, BoxRegistry, BoxScope};
use axum::{body::Body, http::HeaderMap, response::Response};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Frame, SizeHint};
use std::{
    collections::VecDeque,
    io,
    path::Path,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::oneshot;

fn fixture() -> (tempfile::TempDir, BoxRegistry, BoxAuthority) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/tests/box-request-context");
    std::fs::create_dir_all(&root).unwrap();
    let home = tempfile::tempdir_in(root).unwrap();
    let registry = BoxRegistry::open(home.path()).unwrap();
    let (scope, key) = registry
        .add(BoxName::parse("synthetic-context").unwrap())
        .unwrap()
        .into_parts();
    let authority = registry.try_authenticate(&scope, &key).unwrap();
    (home, registry, authority)
}

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .unwrap()
}

#[tokio::test]
async fn detached_spawn_blocking_and_nested_spawn_retain_exact_scope_after_caller_cancel() {
    let (_home, registry, authority) = fixture();
    let expected = authority.scope().clone();
    let request = authority.admit().unwrap();
    let (entered, started) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (finished, completed) = oneshot::channel();
    let caller = tokio::spawn(scope_request(request, async move {
        spawn(async move {
            assert_eq!(current().unwrap().scope(), &expected);
            spawn_blocking(move || {
                assert_eq!(current().unwrap().scope(), &expected);
                assert!(dispatch_allowed());
                entered.send(()).unwrap();
                // Native channel deliberately keeps the actual blocking work
                // alive after its requesting future and join handle disappear.
                released.recv().unwrap();
                assert!(!dispatch_allowed());
                let nested = spawn(async move {
                    assert_eq!(current().unwrap().scope(), &expected);
                    assert!(!dispatch_allowed());
                });
                nested
            })
            .await
            .unwrap()
            .await
            .unwrap();
            finished.send(()).unwrap();
        })
        .await
        .unwrap();
    }));
    bounded(started).await.unwrap();
    caller.abort();
    assert!(bounded(caller).await.unwrap_err().is_cancelled());
    registry.revoke(authority.scope().name()).unwrap();
    assert!(
        dispatch_allowed(),
        "ordinary caller context must remain unbound"
    );
    let mut wait = Box::pin(authority.wait_quiescent());
    assert!(futures_util::poll!(wait.as_mut()).is_pending());
    release.send(()).unwrap();
    bounded(completed).await.unwrap();
    bounded(wait).await;
}

#[tokio::test]
async fn unbound_wrappers_preserve_none_through_blocking_and_nested_tasks() {
    assert!(current().is_none());
    spawn(async {
        assert!(current().is_none());
        spawn_blocking(|| {
            assert!(current().is_none());
            spawn(async {
                assert!(current().is_none());
                assert!(dispatch_allowed());
            })
        })
        .await
        .unwrap()
        .await
        .unwrap();
    })
    .await
    .unwrap();
    assert!(current().is_none());
}

struct Frames {
    expected: BoxScope,
    frames: VecDeque<Frame<Bytes>>,
}

impl hyper::body::Body for Frames {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        assert_eq!(current().unwrap().scope(), &self.expected);
        Poll::Ready(self.frames.pop_front().map(Ok))
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(
            self.frames
                .iter()
                .filter_map(|frame| frame.data_ref())
                .map(|bytes| bytes.len() as u64)
                .sum(),
        )
    }
}

#[tokio::test]
async fn retained_body_scopes_polling_and_preserves_data_frames_trailers_and_size_hint() {
    let (_home, _registry, authority) = fixture();
    let mut trailers = HeaderMap::new();
    trailers.append("x-synthetic-trailer", "first".parse().unwrap());
    trailers.append("x-synthetic-trailer", "second".parse().unwrap());
    let first = Bytes::from_static(b"\0synthetic\n");
    let second = Bytes::from_static(b"tail");
    let length = first.len() + second.len();
    let mut response = Response::new(Body::new(Frames {
        expected: authority.scope().clone(),
        frames: VecDeque::from([
            Frame::data(first.clone()),
            Frame::data(second.clone()),
            Frame::trailers(trailers.clone()),
        ]),
    }));
    retain_response(&mut response, authority.admit().unwrap());
    assert_eq!(
        hyper::body::Body::size_hint(response.body()).exact(),
        Some(length as u64)
    );
    let mut wait = Box::pin(authority.wait_quiescent());
    assert!(futures_util::poll!(wait.as_mut()).is_pending());
    assert_eq!(
        bounded(response.body_mut().frame())
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        first
    );
    assert!(current().is_none());
    assert_eq!(
        bounded(response.body_mut().frame())
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        second
    );
    let actual = bounded(response.body_mut().frame())
        .await
        .unwrap()
        .unwrap()
        .into_trailers()
        .unwrap();
    assert_eq!(actual, trailers);
    assert!(bounded(response.body_mut().frame()).await.is_none());
    bounded(wait).await;
    assert!(current().is_none());
}

struct DropCleanup {
    expected: BoxScope,
    entered: Option<oneshot::Sender<bool>>,
    release: Option<std::sync::mpsc::Receiver<()>>,
}

impl hyper::body::Body for DropCleanup {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        Poll::Ready(None)
    }
}

impl Drop for DropCleanup {
    fn drop(&mut self) {
        let expected = self.expected.clone();
        let entered = self.entered.take().unwrap();
        let release = self.release.take().unwrap();
        spawn_blocking(move || {
            let inherited = current()
                .is_some_and(|request| request.scope() == &expected && request.is_revoked());
            let _ = entered.send(inherited);
            let _ = release.recv();
        });
    }
}

#[tokio::test]
async fn body_drop_cleanup_inherits_scope_and_holds_drain_before_and_after_eof() {
    // Regression: an inner body's destructor can spawn mandatory cleanup.
    // Neither an unpolled body nor a body retained after EOF may lose context.
    for poll_eof in [false, true] {
        let (_home, registry, authority) = fixture();
        let (entered, started) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let mut response = Response::new(Body::new(DropCleanup {
            expected: authority.scope().clone(),
            entered: Some(entered),
            release: Some(released),
        }));
        retain_response(&mut response, authority.admit().unwrap());
        registry.revoke(authority.scope().name()).unwrap();
        let mut wait = Box::pin(authority.wait_quiescent());
        assert!(futures_util::poll!(wait.as_mut()).is_pending());
        if poll_eof {
            assert!(bounded(response.body_mut().frame()).await.is_none());
        }
        drop(response);
        assert!(
            bounded(started).await.unwrap(),
            "body Drop cleanup must inherit its exact revoked grant"
        );
        assert!(futures_util::poll!(wait.as_mut()).is_pending());
        release.send(()).unwrap();
        bounded(wait).await;
        assert!(current().is_none());
    }
}
