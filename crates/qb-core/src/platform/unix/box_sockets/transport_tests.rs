use super::*;
use crate::boxes::{BoxName, BoxRegistry};
use axum::{http::StatusCode, routing::get};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture() -> (tempfile::TempDir, BoxScope) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/bt");
    std::fs::create_dir_all(&base).unwrap();
    let root = tempfile::tempdir_in(base).unwrap();
    let registry = BoxRegistry::open(root.path()).unwrap();
    let (scope, _key) = registry
        .add(BoxName::parse("a").unwrap())
        .unwrap()
        .into_parts();
    // Scope is transport identity only. Dropping registry does not authenticate
    // anything; common auth/registry integration is deliberately a separate gate.
    (root, scope)
}

async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .unwrap()
}

#[tokio::test]
async fn shutdown_both_closes_actual_stream_and_tracks_io_until_drop() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let connections = Arc::new(Connections::default());
    let tracked = connections.track(stream).unwrap().unwrap();
    connections.close_all().unwrap();
    assert_eq!(bounded(peer.read(&mut [0; 1])).await.unwrap(), 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), connections.empty())
            .await
            .is_err()
    );
    drop(tracked);
    bounded(connections.empty()).await;
    let (another, _) = UnixStream::pair().unwrap();
    assert!(connections.track(another).unwrap().is_none());
    assert!(!connections.failed.load(Ordering::Acquire));
}

#[tokio::test]
async fn shutdown_failure_stays_failed_after_owned_io_drops_and_join_repeats() {
    let (root, scope) = fixture();
    let directory = SocketDirectory::open(root.path(), unsafe { libc::getegid() }).unwrap();
    let (original, identity, node) =
        BoundBoxSocket::bind_new(&directory, SocketIdentity::new(scope))
            .unwrap()
            .into_parts();
    let listener = BoxListener::duplicate(original.as_fd(), &node, identity).unwrap();
    let connections = listener.connections.clone();
    let (stream, peer) = UnixStream::pair().unwrap();
    let tracked = connections.track(stream).unwrap().unwrap();
    let mut transport = BoxTransport::start(listener, |_| Router::new());
    assert!(
        connections
            .close_all_with(|_| Err(io::Error::other("synthetic shutdown failure")))
            .is_err()
    );
    // Failure wakes graceful shutdown, but cannot replace the owned IO join.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), transport.join())
            .await
            .is_err()
    );
    drop((tracked, peer));
    assert!(bounded(transport.join()).await.is_err());
    assert!(bounded(transport.join()).await.is_err());
    drop((transport, original));
    node.unlink_owned().unwrap();
}

#[tokio::test]
async fn registration_overflow_wakes_actual_axum_parent_and_join_reports_failure() {
    let (root, scope) = fixture();
    let directory = SocketDirectory::open(root.path(), unsafe { libc::getegid() }).unwrap();
    let (original, identity, node) =
        BoundBoxSocket::bind_new(&directory, SocketIdentity::new(scope))
            .unwrap()
            .into_parts();
    let listener = BoxListener::duplicate(original.as_fd(), &node, identity).unwrap();
    let connections = listener.connections.clone();
    connections.state.lock().unwrap().next = u64::MAX;
    let mut transport = BoxTransport::start(listener, |_| Router::new());
    let mut client = UnixStream::connect(&node.path).await.unwrap();
    // No begin_drain/revoke: registration failure must terminate the real parent.
    assert!(bounded(transport.join()).await.is_err());
    assert!(connections.failed.load(Ordering::Acquire));
    assert!(connections.state.lock().unwrap().live.is_empty());
    assert_eq!(bounded(client.read(&mut [0; 1])).await.unwrap(), 0);
    assert!(bounded(transport.join()).await.is_err());
    drop((client, transport, original));
    node.unlink_owned().unwrap();
}

#[tokio::test]
async fn listener_duplicate_retains_inode_and_acceptance_after_serving_wrapper_drop() {
    let (root, scope) = fixture();
    let directory = SocketDirectory::open(root.path(), unsafe { libc::getegid() }).unwrap();
    let (original, identity, node) =
        BoundBoxSocket::bind_new(&directory, SocketIdentity::new(scope))
            .unwrap()
            .into_parts();
    let inode = node.stat().unwrap().st_ino;
    let adopted = directory
        .adopt_inherited(original.as_fd(), &identity, node.stamp())
        .unwrap();
    assert_eq!(adopted.stamp().inode, inode);
    let mut wrong = node.stamp();
    wrong.inode += 1;
    assert!(
        directory
            .adopt_inherited(original.as_fd(), &identity, wrong)
            .is_err()
    );
    let first = BoxListener::duplicate(original.as_fd(), &node, identity.clone()).unwrap();
    drop(first);
    let second = BoxListener::duplicate(original.as_fd(), &node, identity).unwrap();
    let (client, accepted) =
        tokio::join!(UnixStream::connect(&node.path), second.listener.accept());
    let mut client = client.unwrap();
    let (mut accepted, _) = accepted.unwrap();
    client.write_all(b"same inode").await.unwrap();
    let mut bytes = [0; 10];
    bounded(accepted.read_exact(&mut bytes)).await.unwrap();
    assert_eq!(&bytes, b"same inode");
    assert_eq!(node.stat().unwrap().st_ino, inode);
    assert_eq!(node.stat().unwrap().st_mode & 0o7777, 0o660);
    drop((client, accepted, second, original));
    node.unlink_owned().unwrap();
}

#[tokio::test]
async fn actual_axum_upgrade_keeps_io_tracked_after_parent_then_revoke_closes_it() {
    let (root, scope) = fixture();
    let expected_scope = scope.clone();
    let directory = SocketDirectory::open(root.path(), unsafe { libc::getegid() }).unwrap();
    let (original, identity, node) =
        BoundBoxSocket::bind_new(&directory, SocketIdentity::new(scope))
            .unwrap()
            .into_parts();
    let listener = BoxListener::duplicate(original.as_fd(), &node, identity).unwrap();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (finished, terminal) = tokio::sync::oneshot::channel();
    let signals = Arc::new(Mutex::new(Some((entered, finished))));
    // Uses the existing axum/hyper upgrade path, not an alternate HTTP server.
    // Authentication middleware is supplied by main in later integration tests.
    let router = Router::new().route(
        "/",
        get(move |mut request: axum::extract::Request| {
            let upgrade = hyper::upgrade::on(&mut request);
            let (entered, finished) = signals.lock().unwrap().take().unwrap();
            async move {
                tokio::spawn(async move {
                    let upgraded = upgrade.await.unwrap();
                    let mut io = hyper_util::rt::TokioIo::new(upgraded);
                    entered.send(()).unwrap();
                    let read = io.read(&mut [0; 1]).await;
                    drop(io);
                    finished.send(read).unwrap();
                });
                (
                    StatusCode::SWITCHING_PROTOCOLS,
                    [("connection", "upgrade"), ("upgrade", "synthetic")],
                )
            }
        }),
    );
    let mut transport = BoxTransport::start(listener, move |actual_scope| {
        assert_eq!(actual_scope, expected_scope);
        router
    });
    let mut client = UnixStream::connect(&node.path).await.unwrap();
    client.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1:10200\r\nConnection: upgrade\r\nUpgrade: synthetic\r\n\r\n").await.unwrap();
    let mut header = Vec::new();
    bounded(async {
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0; 1];
            client.read_exact(&mut byte).await.unwrap();
            header.push(byte[0]);
            assert!(header.len() < 8192);
        }
    })
    .await;
    assert!(header.starts_with(b"HTTP/1.1 101"));
    bounded(ready).await.unwrap();
    transport.begin_drain();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), transport.join())
            .await
            .is_err()
    );
    // The join above was cancelled: ownership must still be in transport.
    transport.revoke().unwrap();
    assert_eq!(bounded(client.read(&mut [0; 1])).await.unwrap(), 0);
    assert_eq!(bounded(terminal).await.unwrap().unwrap(), 0);
    bounded(transport.join()).await.unwrap();
    drop((transport, client, original));
    node.unlink_owned().unwrap();
}

#[test]
fn existing_leaf_is_never_unlinked_and_wrong_inode_cleanup_is_refused() {
    let (root, scope) = fixture();
    let directory = SocketDirectory::open(root.path(), unsafe { libc::getegid() }).unwrap();
    let path = directory.path.join("a.sock");
    std::fs::write(&path, b"preserve existing leaf").unwrap();
    assert!(BoundBoxSocket::bind_new(&directory, SocketIdentity::new(scope.clone())).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"preserve existing leaf");
    std::fs::remove_file(&path).unwrap();
    let (original, _, node) = BoundBoxSocket::bind_new(&directory, SocketIdentity::new(scope))
        .unwrap()
        .into_parts();
    drop(original);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    assert!(node.unlink_owned().is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
}
