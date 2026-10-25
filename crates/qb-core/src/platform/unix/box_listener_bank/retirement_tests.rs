use super::*;
use crate::boxes::BoxRegistry;
use std::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Fixture {
    root: tempfile::TempDir,
    registry: BoxRegistry,
    directory: SocketDirectory,
    bank: BoxBank,
    a: BoxScope,
    b: BoxScope,
}
fn active(registry: &BoxRegistry) -> Vec<BoxScope> {
    registry
        .list()
        .unwrap()
        .into_iter()
        .filter(|s| !s.revoked)
        .map(|s| s.scope)
        .collect()
}
fn enroll(registry: &BoxRegistry, name: &str) -> BoxScope {
    registry
        .add(BoxName::parse(name).unwrap())
        .unwrap()
        .into_parts()
        .0
}
fn retain(bank: &mut BoxBank, directory: &SocketDirectory, scope: &BoxScope) {
    let mut bound = Some(
        BoundBoxSocket::bind_new(directory, SocketIdentity::new(scope.clone()))
            .unwrap()
            .normalize_owned_original()
            .unwrap(),
    );
    bank.retain(&mut bound).unwrap();
    assert!(bound.is_none());
}
fn fixture() -> Fixture {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".sandbox/bt");
    std::fs::create_dir_all(&base).unwrap();
    let root = tempfile::tempdir_in(base).unwrap();
    let registry = BoxRegistry::open(root.path()).unwrap();
    let a = enroll(&registry, "a");
    let b = enroll(&registry, "b");
    let directory = SocketDirectory::open(root.path(), unsafe { libc::getegid() }).unwrap();
    let mut bank = BoxBank::inherit(None).unwrap();
    bank.reconcile(&directory, &active(&registry)).unwrap();
    retain(&mut bank, &directory, &a);
    retain(&mut bank, &directory, &b);
    Fixture {
        root,
        registry,
        directory,
        bank,
        a,
        b,
    }
}
async fn round_trip(listener: &mut BoxListener, path: &Path) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (client, (mut accepted, _)) = tokio::join!(
            tokio::net::UnixStream::connect(path),
            axum::serve::Listener::accept(listener)
        );
        let mut client = client.unwrap();
        client.write_all(b"B").await.unwrap();
        let mut byte = [0; 1];
        accepted.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, *b"B");
    })
    .await
    .unwrap(); // Both owned IO values drop before returning.
}

#[tokio::test]
async fn retiring_a_preserves_live_b_and_reenrolls_only_after_exact_scope_finish() {
    let mut f = fixture();
    let b_fd = f.bank.live["b"].original.as_raw_fd();
    let mut b = f.bank.duplicate(&f.b).unwrap();
    let a = f.bank.duplicate(&f.a).unwrap();
    f.registry.revoke(f.a.name()).unwrap();
    f.bank.begin_retirement(&f.a).unwrap();
    f.bank.begin_retirement(&f.a).unwrap(); // Resume, no ownership loss.
    assert!(f.bank.duplicate(&f.a).is_err());
    assert!(!f.bank.failed);
    drop(f.bank.duplicate(&f.b).unwrap());
    round_trip(&mut b, &f.root.path().join("boxes/b.sock")).await;
    assert_eq!(f.bank.live["b"].original.as_raw_fd(), b_fd);
    // Native ownership-only test: A has no accepted IO/writers. Actual service
    // integration must revoke/terminally join its BoxTransport before this call.
    drop(a);
    f.bank.finish_retirement(&f.a).unwrap();
    assert!(!f.root.path().join("boxes/a.sock").exists());
    let next = enroll(&f.registry, "a");
    assert_eq!(next.epoch(), f.a.epoch() + 1);
    f.bank
        .reconcile(&f.directory, &active(&f.registry))
        .unwrap();
    retain(&mut f.bank, &f.directory, &next);
    let next_fd = f.bank.live["a"].original.as_raw_fd();
    assert!(f.bank.finish_retirement(&f.a).is_err());
    assert!(f.bank.begin_retirement(&f.a).is_err());
    assert_eq!(f.bank.live["a"].original.as_raw_fd(), next_fd);
    drop(f.bank.duplicate(&next).unwrap());
    round_trip(&mut b, &f.root.path().join("boxes/b.sock")).await;
    drop(b);
}

#[tokio::test]
async fn absent_node_sync_retry_retains_phase_blocks_exec_and_preserves_fatal_latch() {
    let mut f = fixture();
    f.registry.revoke(f.a.name()).unwrap();
    f.bank.begin_retirement(&f.a).unwrap();
    f.bank.freeze();
    assert!(f.bank.exec_metadata().is_err());
    let error = f.bank.finish_retirement_with(&f.a, |node| {
        node.unlink_owned_or_sync_absent_with(|_| {
            Err(io::Error::other("synthetic directory sync failure"))
        })
    });
    assert!(error.is_err());
    assert!(!f.root.path().join("boxes/a.sock").exists());
    assert!(f.bank.retiring["a"].original.is_none());
    assert_eq!(f.bank.retiring["a"].last_error, Some(io::ErrorKind::Other));
    assert!(f.bank.exec_metadata().is_err());
    // A changed active snapshot cannot enroll A while retirement reserves name.
    assert!(f.bank.retiring.contains_key("a"));
    f.bank.failed = true; // Existing fatal latch must survive successful cleanup.
    f.bank.finish_retirement(&f.a).unwrap(); // Real absent-path parent fsync.
    assert!(!f.bank.retiring.contains_key("a"));
    assert!(f.bank.failed);
    assert!(f.bank.exec_metadata().is_err());
}

#[tokio::test]
async fn conflicting_inode_is_preserved_and_retry_keeps_exact_scope_evidence() {
    let mut f = fixture();
    f.registry.revoke(f.a.name()).unwrap();
    f.bank.begin_retirement(&f.a).unwrap();
    let path = f.root.path().join("boxes/a.sock");
    let saved = f.root.path().join("boxes/a.saved");
    // Keep the old inode alive elsewhere so the replacement cannot reuse it.
    std::fs::rename(&path, &saved).unwrap();
    let replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
    assert!(f.bank.finish_retirement(&f.a).is_err());
    assert!(f.bank.retiring["a"].original.is_none());
    assert!(path.exists());
    assert_eq!(f.bank.retiring["a"].identity.scope(), &f.a);
    assert!(f.bank.retiring["a"].last_error.is_some());
    // Test teardown under sole ownership restores the old exact node, not a
    // production fallback unlink of an unknown successor.
    drop(replacement);
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(saved, &path).unwrap();
    f.bank.finish_retirement(&f.a).unwrap();
    assert!(!path.exists());
    drop(f.bank.duplicate(&f.b).unwrap());
}

fn duplicate_at_least(fd: RawFd, minimum: RawFd) -> OwnedFd {
    let result = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, minimum) };
    assert!(result >= 0, "safe nonclobbering duplicate failed");
    unsafe { OwnedFd::from_raw_fd(result) }
}

#[tokio::test]
async fn retry_never_closes_reused_original_descriptor_number() {
    let mut f = fixture();
    // Sparse slot avoids normal low-FD allocation; F_DUPFD never clobbers a live
    // owner. Respect the actual limit instead of assuming a 4096 descriptor cap.
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
        0
    );
    let limit = unsafe { limit.assume_init() };
    let minimum = (limit.rlim_cur / 2).min(4096) as RawFd;
    assert!(
        minimum >= 64,
        "test requires ordinary native descriptor capacity"
    );
    let retained = f.bank.live.get_mut("a").unwrap();
    let sparse = duplicate_at_least(retained.original.as_raw_fd(), minimum);
    retained.original = sparse;
    let old_fd = retained.original.as_raw_fd();
    f.registry.revoke(f.a.name()).unwrap();
    f.bank.begin_retirement(&f.a).unwrap();
    let probe = File::open(f.root.path()).unwrap();
    let mut reused = None;
    assert!(
        f.bank
            .finish_retirement_with(&f.a, |_| {
                let fd = duplicate_at_least(probe.as_raw_fd(), old_fd);
                assert_eq!(
                    fd.as_raw_fd(),
                    old_fd,
                    "sparse slot raced; cannot claim actual reuse"
                );
                reused = Some(fd);
                Err(io::Error::other(
                    "synthetic pre-unlink failure after original drop",
                ))
            })
            .is_err()
    );
    let reused = reused.unwrap();
    assert!(f.bank.retiring["a"].original.is_none());
    f.bank.finish_retirement(&f.a).unwrap();
    assert_eq!(reused.as_raw_fd(), old_fd);
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    assert_eq!(
        unsafe { libc::fstat(reused.as_raw_fd(), stat.as_mut_ptr()) },
        0
    );
    assert_eq!(
        unsafe { stat.assume_init() }.st_mode & libc::S_IFMT,
        libc::S_IFDIR
    );
    drop(f.bank.duplicate(&f.b).unwrap());
}
