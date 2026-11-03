use super::*;
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener as StdListener},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture() -> (tempfile::TempDir, StdListener, SocketIdentity) {
    // /tmp is deliberately refused by the production path walk. Keep fixtures
    // under the trusted source directory, without weakening that policy.
    let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.path().join("admin.sock");
    let listener = StdListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
    let directory = checked_runtime_directory(root.path()).unwrap();
    let identity = SocketIdentity::capture(listener.as_raw_fd(), directory, path, unsafe {
        libc::getegid()
    })
    .unwrap();
    (root, listener, identity)
}

#[test]
fn runtime_exception_is_exact_and_does_not_change_private_storage_policy() {
    let (root, _, _) = fixture();
    let gid = unsafe { libc::getegid() };
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o710)).unwrap();
    checked_runtime_directory_with(root.path(), || Ok(gid)).unwrap();
    assert!(checked_runtime_directory_with(root.path(), || Ok(gid.wrapping_add(1))).is_err());
    assert!(super::super::private_fs::private_directory(root.path(), false).is_err());
    for mode in [0o750, 0o770, 0o711, 0o1710] {
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(checked_runtime_directory_with(root.path(), || Ok(gid)).is_err());
    }
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    checked_runtime_directory_with(root.path(), || panic!("0700 needs no qb-admin group")).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(root.path(), &alias).unwrap();
    assert!(checked_runtime_directory(&alias).is_err());
}

#[test]
fn inherited_socket_requires_exact_node_fd_shape_and_flags() {
    let (root, listener, identity) = fixture();
    let fd = listener.as_raw_fd();
    identity.validate(fd).unwrap();
    listener.set_nonblocking(false).unwrap();
    assert!(identity.validate(fd).is_err());
    listener.set_nonblocking(true).unwrap();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0
    );
    assert!(identity.validate(fd).is_err());
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags) }, 0);
    identity.validate(fd).unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    tcp.set_nonblocking(true).unwrap();
    assert!(identity.validate(tcp.as_raw_fd()).is_err());
    assert!(
        SocketIdentity::capture(
            tcp.as_raw_fd(),
            checked_runtime_directory(root.path()).unwrap(),
            identity.path.clone(),
            identity.gid
        )
        .is_err()
    );
    let (connected, _) = std::os::unix::net::UnixStream::pair().unwrap();
    connected.set_nonblocking(true).unwrap();
    assert!(identity.validate(connected.as_raw_fd()).is_err());
    assert!(
        SocketIdentity::capture(
            connected.as_raw_fd(),
            checked_runtime_directory(root.path()).unwrap(),
            identity.path.clone(),
            identity.gid
        )
        .is_err()
    );
    let datagram = std::os::unix::net::UnixDatagram::unbound().unwrap();
    datagram.set_nonblocking(true).unwrap();
    assert!(
        SocketIdentity::capture(
            datagram.as_raw_fd(),
            checked_runtime_directory(root.path()).unwrap(),
            identity.path.clone(),
            identity.gid
        )
        .is_err()
    );
    assert!(
        SocketIdentity::capture(
            fd,
            checked_runtime_directory(root.path()).unwrap(),
            identity.path.clone(),
            identity.gid.wrapping_add(1)
        )
        .is_err()
    );
    assert!(
        SocketIdentity::capture(
            fd,
            checked_runtime_directory(root.path()).unwrap(),
            root.path().join("wrong.sock"),
            identity.gid
        )
        .is_err()
    );
    let path = root.path().join("admin.sock");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(identity.validate(fd).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
    identity.validate(fd).unwrap();
    std::fs::remove_file(&path).unwrap();
    let replacement = StdListener::bind(&path).unwrap();
    replacement.set_nonblocking(true).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
    assert!(identity.validate(fd).is_err());
}

#[test]
fn serving_copy_drop_preserves_original_and_reload_identity() {
    let (_root, original, identity) = fixture();
    let fd = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 6) };
    assert!(fd >= 6);
    let copy = unsafe { StdListener::from_raw_fd(fd) };
    identity.validate(copy.as_raw_fd()).unwrap();
    drop(copy);
    identity.validate(original.as_raw_fd()).unwrap();
    let client = std::os::unix::net::UnixStream::connect(&identity.path).unwrap();
    let (_stream, _) = original.accept().unwrap();
    drop(client);
    let adopted = SocketIdentity::capture(
        original.as_raw_fd(),
        checked_runtime_directory(identity.path.parent().unwrap()).unwrap(),
        identity.path.clone(),
        identity.gid,
    )
    .unwrap();
    assert_eq!(adopted.node, identity.node);
    assert_eq!(adopted.socket, identity.socket);
}

#[test]
fn native_membership_includes_primary_group_and_refuses_absent_group() {
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    assert!(uid_in_group(uid, gid).unwrap());
    assert!(!uid_in_group(uid, u32::MAX).unwrap());
    assert!(peer_allowed(uid, uid, u32::MAX).unwrap());
    assert!(uid_in_group(u32::MAX, gid).is_err());
}

#[test]
fn optional_client_endpoint_falls_back_without_changing_socket_permissions() {
    let (root, listener, identity) = fixture();
    let gid = unsafe { libc::getegid() };
    assert_eq!(
        client_path_at(&identity.path, || Ok(gid)),
        Some(identity.path.clone())
    );
    assert!(client_path_at(&identity.path, || Ok(gid.wrapping_add(1))).is_none());
    assert!(client_path_at(&identity.path, || Err(refused())).is_none());
    let alias = root.path().join("alias.sock");
    std::os::unix::fs::symlink(&identity.path, &alias).unwrap();
    assert!(client_path_at(&alias, || Ok(gid)).is_none());
    std::fs::set_permissions(&identity.path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(client_path_at(&identity.path, || Ok(gid)).is_none());
    assert_eq!(
        std::fs::metadata(&identity.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    drop(listener);
}

fn test_server(
    listener: StdListener,
    owner: u32,
    gid: u32,
    router: Router,
) -> (AdminSocketServer, watch::Sender<bool>) {
    let (closed_tx, closed) = watch::channel(false);
    let (shutdown_tx, shutdown) = watch::channel(false);
    let listener = AdminListener {
        listener: UnixListener::from_std(listener).unwrap(),
        owner,
        gid,
        closed: closed_tx,
    };
    (serve(listener, router, shutdown, closed), shutdown_tx)
}

#[tokio::test]
async fn real_peer_group_path_is_served_and_denial_precedes_handler() {
    for allowed in [true, false] {
        let (_root, listener, identity) = fixture();
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let router = Router::new().route(
            "/",
            axum::routing::get(move || {
                handler_calls.fetch_add(1, Ordering::SeqCst);
                async { "served" }
            }),
        );
        // Force the real SO_PEERCRED uid through NSS even though tests run as
        // one user. Production always sets owner to geteuid, not an input.
        let uid = unsafe { libc::geteuid() };
        let gid = if allowed {
            unsafe { libc::getegid() }
        } else {
            u32::MAX
        };
        let (mut server, shutdown) = test_server(listener, uid.wrapping_add(1), gid, router);
        let mut stream = UnixStream::connect(&identity.path).await.unwrap();
        let _ = stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await;
        let mut response = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .unwrap();
        if allowed {
            read.unwrap();
            assert!(String::from_utf8_lossy(&response).contains("served"));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        } else {
            assert!(response.is_empty());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), server.join())
            .await
            .unwrap()
            .unwrap();
        assert!(*server.closed.borrow());
    }
}

#[tokio::test]
async fn graceful_join_retains_pending_mutation_when_wait_is_cancelled() {
    let (_root, listener, identity) = fixture();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mutation = Arc::new(AtomicUsize::new(0));
    let router = Router::new().route(
        "/",
        axum::routing::get({
            let entered = entered.clone();
            let release = release.clone();
            let mutation = mutation.clone();
            move || {
                let entered = entered.clone();
                let release = release.clone();
                let mutation = mutation.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    mutation.fetch_add(1, Ordering::SeqCst);
                    "done"
                }
            }
        }),
    );
    let (mut server, shutdown) =
        test_server(listener, unsafe { libc::geteuid() }, identity.gid, router);
    let mut stream = UnixStream::connect(&identity.path).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    shutdown.send_replace(true);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), server.join())
            .await
            .is_err()
    );
    assert!(server.task.is_some());
    assert_eq!(mutation.load(Ordering::SeqCst), 0);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), server.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mutation.load(Ordering::SeqCst), 1);
    assert!(*server.closed.borrow());
    server.join().await.unwrap();
}

#[tokio::test]
async fn dropping_guard_aborts_owned_serve_task_and_closes_acceptor() {
    let (_root, listener, identity) = fixture();
    let (server, _shutdown) = test_server(
        listener,
        unsafe { libc::geteuid() },
        identity.gid,
        Router::new(),
    );
    let mut closed = server.closed.clone();
    drop(server);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !*closed.borrow_and_update() {
            closed.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}
