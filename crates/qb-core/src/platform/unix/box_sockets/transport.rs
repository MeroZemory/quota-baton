//! Unix socket nodes and owned, terminally joined box transports.
use crate::boxes::BoxScope;
use axum::Router;
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::File,
    io,
    net::Shutdown,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            net::{UnixListener as StdListener, UnixStream as StdStream},
        },
    },
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{UnixListener, UnixStream},
    sync::{Notify, watch},
    task::JoinHandle,
};

fn refused() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe box socket")
}

#[derive(Clone, Debug)]
pub(crate) struct SocketIdentity(BoxScope);
impl SocketIdentity {
    pub fn new(scope: BoxScope) -> Self {
        Self(scope)
    }
    pub fn scope(&self) -> &BoxScope {
        &self.0
    }
}

pub(crate) struct SocketDirectory {
    directory: Arc<File>,
    path: PathBuf,
    gid: u32,
}

impl SocketDirectory {
    /// Reconstruct metadata from the validated exec manifest WITHOUT rebinding.
    pub fn adopt_inherited(
        &self,
        original: BorrowedFd<'_>,
        identity: &SocketIdentity,
        stamp: SocketStamp,
    ) -> io::Result<SocketNode> {
        let filename = format!("{}.sock", identity.scope().name().as_str());
        let node = SocketNode {
            directory: self.directory.clone(),
            name: CString::new(filename.clone()).map_err(|_| refused())?,
            path: self.path.join(filename),
            device: stamp.device,
            inode: stamp.inode,
            gid: self.gid,
        };
        let _checked = checked_listener(original, &node, identity)?;
        Ok(node)
    }

    pub fn open(runtime: &Path, socket_gid: u32) -> io::Result<Self> {
        if !runtime.is_absolute() {
            return Err(refused());
        }
        let root = super::super::private_fs::private_directory(runtime, false)?;
        let name = c"boxes";
        let created = unsafe { libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o750) };
        if created != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
            return Err(refused());
        }
        let raw = unsafe {
            libc::openat(
                root.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(refused());
        }
        let directory = unsafe { File::from_raw_fd(raw) };
        if created == 0 {
            if unsafe { libc::fchown(directory.as_raw_fd(), libc::geteuid(), socket_gid) } != 0
                || unsafe { libc::fchmod(directory.as_raw_fd(), 0o750) } != 0
            {
                return Err(refused());
            }
            root.sync_all()?;
        }
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(directory.as_raw_fd(), &mut stat) } != 0
            || stat.st_uid != unsafe { libc::geteuid() }
            || stat.st_gid != socket_gid
            || stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || stat.st_mode & 0o7777 != 0o750
        {
            return Err(refused());
        }
        Ok(Self {
            directory: Arc::new(directory),
            path: runtime.join("boxes"),
            gid: socket_gid,
        })
    }
}

#[derive(Clone)]
pub(crate) struct SocketNode {
    directory: Arc<File>,
    name: CString,
    path: PathBuf,
    device: libc::dev_t,
    inode: libc::ino_t,
    gid: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SocketStamp {
    pub device: libc::dev_t,
    pub inode: libc::ino_t,
}

impl SocketNode {
    pub fn stamp(&self) -> SocketStamp {
        SocketStamp {
            device: self.device,
            inode: self.inode,
        }
    }
    fn stat(&self) -> io::Result<libc::stat> {
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(refused());
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK
            || stat.st_uid != unsafe { libc::geteuid() }
            || stat.st_gid != self.gid
            || stat.st_mode & 0o7777 != 0o660
            || stat.st_nlink != 1
            || stat.st_dev != self.device
            || stat.st_ino != self.inode
        {
            return Err(refused());
        }
        Ok(stat)
    }
    /// Caller has closed the bank original and joined all IO. No Drop unlink.
    #[cfg(test)]
    pub fn unlink_owned(&self) -> io::Result<()> {
        self.unlink_owned_with(File::sync_all)
    }
    fn unlink_owned_with(&self, sync: impl FnOnce(&File) -> io::Result<()>) -> io::Result<()> {
        self.stat()?;
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) } != 0 {
            return Err(refused());
        }
        sync(&self.directory)
    }
    /// Only the retirement owner may use absence as a retry outcome, after
    /// closing its original and joining IO/writers under the name mutation gate.
    pub(crate) fn unlink_owned_or_sync_absent(&self) -> io::Result<()> {
        self.unlink_owned_or_sync_absent_with(File::sync_all)
    }
    pub(in super::super) fn unlink_owned_or_sync_absent_with(
        &self,
        sync: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return sync(&self.directory);
            }
            return Err(error);
        }
        // Rechecks the captured stamp and ALL ownership/type/mode constraints;
        // a conflicting successor is never an absence and is never unlinked.
        self.unlink_owned_with(sync)
    }
}

pub(crate) struct BoundBoxSocket {
    original: StdListener,
    identity: SocketIdentity,
    node: SocketNode,
}

impl BoundBoxSocket {
    #[cfg(test)]
    pub(crate) fn normalize_owned_original(mut self) -> io::Result<Self> {
        if self.original.as_raw_fd() < 5 {
            self.original = StdListener::from(duplicate(self.original.as_fd())?);
        }
        Ok(self)
    }

    pub fn scope(&self) -> &BoxScope {
        self.identity.scope()
    }

    pub fn bind_new(directory: &SocketDirectory, identity: SocketIdentity) -> io::Result<Self> {
        let filename = format!("{}.sock", identity.scope().name().as_str());
        let name = CString::new(filename.clone()).map_err(|_| refused())?;
        let path = directory.path.join(filename);
        let address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
        if path.as_os_str().as_bytes().len() >= address.sun_path.len() {
            return Err(refused());
        }
        let mut before = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe {
            libc::fstatat(
                directory.directory.as_raw_fd(),
                name.as_ptr(),
                &mut before,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
            || io::Error::last_os_error().kind() != io::ErrorKind::NotFound
        {
            return Err(refused());
        }
        // Every ancestor is trusted/non-writable by group or other; no bindat
        // exists. Do not bind a /proc/self/fd alias which breaks after exec.
        let original = StdListener::bind(&path)?;
        original.set_nonblocking(true)?;
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe {
            libc::fstatat(
                directory.directory.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
            || stat.st_mode & libc::S_IFMT != libc::S_IFSOCK
            || stat.st_uid != unsafe { libc::geteuid() }
        {
            return Err(refused());
        }
        if unsafe {
            libc::fchownat(
                directory.directory.as_raw_fd(),
                name.as_ptr(),
                libc::geteuid(),
                directory.gid,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
            || unsafe { libc::fchmodat(directory.directory.as_raw_fd(), name.as_ptr(), 0o660, 0) }
                != 0
        {
            return Err(refused());
        }
        let node = SocketNode {
            directory: directory.directory.clone(),
            name,
            path,
            device: stat.st_dev,
            inode: stat.st_ino,
            gid: directory.gid,
        };
        node.stat()?;
        directory.directory.sync_all()?;
        Ok(Self {
            original,
            identity,
            node,
        })
    }

    /// Move original INTO the existing P2 bank, never a second static bank.
    pub fn into_parts(self) -> (OwnedFd, SocketIdentity, SocketNode) {
        (self.original.into(), self.identity, self.node)
    }
}

fn duplicate(fd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

#[derive(Default)]
struct Connections {
    state: Mutex<ConnectionState>,
    changed: Notify,
    failed: AtomicBool,
}
#[derive(Default)]
struct ConnectionState {
    closing: bool,
    next: u64,
    live: BTreeMap<u64, Weak<Connection>>,
}
struct Connection {
    id: u64,
    control: StdStream,
    owner: Arc<Connections>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.owner
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .live
            .remove(&self.id);
        self.owner.changed.notify_waiters();
    }
}

impl Connections {
    fn track(self: &Arc<Self>, stream: UnixStream) -> io::Result<Option<TrackedIo>> {
        let result = self.track_open(stream);
        if result.is_err() {
            self.fail();
        }
        result
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn failure(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.failed.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }

    fn track_open(self: &Arc<Self>, stream: UnixStream) -> io::Result<Option<TrackedIo>> {
        let mut state = self.state.lock().map_err(|_| refused())?;
        if state.closing {
            return Ok(None); // Expected rejection; stream drops without registration.
        }
        let id = state.next.checked_add(1).ok_or_else(refused)?;
        let control = StdStream::from(duplicate(stream.as_fd())?);
        state.next = id;
        let connection = Arc::new(Connection {
            id,
            control,
            owner: self.clone(),
        });
        state.live.insert(id, Arc::downgrade(&connection));
        Ok(Some(TrackedIo {
            stream,
            _connection: connection,
        }))
    }

    fn close_all(&self) -> io::Result<()> {
        self.close_all_with(|stream| stream.shutdown(Shutdown::Both))
    }

    fn close_all_with(
        &self,
        mut shutdown: impl FnMut(&StdStream) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut failed = false;
        let live: Vec<_> = {
            let mut state = self.state.lock().unwrap_or_else(|poison| {
                failed = true;
                self.fail();
                poison.into_inner()
            });
            state.closing = true;
            state.live.values().filter_map(Weak::upgrade).collect()
        }; // Drop strong handles OUTSIDE the registration mutex.
        for connection in live {
            if let Err(error) = shutdown(&connection.control) {
                if error.kind() != io::ErrorKind::NotConnected {
                    failed = true;
                    self.fail(); // Publish before releasing this retained IO handle.
                }
            }
        }
        if failed {
            Err(io::Error::other("box connection shutdown failed"))
        } else {
            Ok(())
        }
    }

    async fn empty(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .live
                .is_empty()
            {
                return;
            }
            changed.await;
        }
    }
}

pub(crate) struct TrackedIo {
    stream: UnixStream,
    _connection: Arc<Connection>, // stays inside hyper::upgrade::Upgraded
}
impl AsyncRead for TrackedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}
impl AsyncWrite for TrackedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buffer)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub(crate) struct BoxListener {
    listener: UnixListener,
    identity: SocketIdentity,
    connections: Arc<Connections>,
}
impl BoxListener {
    pub fn duplicate(
        original: BorrowedFd<'_>,
        node: &SocketNode,
        identity: SocketIdentity,
    ) -> io::Result<Self> {
        let listener = checked_listener(original, node, &identity)?;
        Ok(Self {
            listener: UnixListener::from_std(listener)?,
            identity,
            connections: Arc::new(Connections::default()),
        })
    }
}
/// Validate the retained original without creating a Tokio serving wrapper.
pub(crate) fn validate_retained(
    original: BorrowedFd<'_>,
    node: &SocketNode,
    identity: &SocketIdentity,
) -> io::Result<()> {
    checked_listener(original, node, identity).map(drop)
}

fn checked_listener(
    original: BorrowedFd<'_>,
    node: &SocketNode,
    identity: &SocketIdentity,
) -> io::Result<StdListener> {
    node.stat()?;
    if node.name.to_bytes() != format!("{}.sock", identity.scope().name().as_str()).as_bytes() {
        return Err(refused());
    }
    let listener = StdListener::from(duplicate(original)?);
    if listener.local_addr()?.as_pathname() != Some(node.path.as_path()) {
        return Err(refused());
    }
    for (option, expected) in [(libc::SO_TYPE, libc::SOCK_STREAM), (libc::SO_ACCEPTCONN, 1)] {
        let mut value = 0i32;
        let mut length = std::mem::size_of_val(&value) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                listener.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&mut value as *mut i32).cast(),
                &mut length,
            )
        } != 0
            || value != expected
        {
            return Err(refused());
        }
    }
    let flags = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || flags & libc::O_NONBLOCK == 0 {
        return Err(refused());
    }
    Ok(listener)
}

impl axum::serve::Listener for BoxListener {
    type Io = TrackedIo;
    type Addr = SocketIdentity;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => match self.connections.track(stream) {
                    Ok(Some(stream)) => return (stream, self.identity.clone()),
                    // Listener has no error return. A real registration error has
                    // latched failure and wakes the server's graceful shutdown.
                    Ok(None) | Err(_) => return std::future::pending().await,
                },
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.identity.clone())
    }
}

#[must_use = "retain and join the box transport before RuntimeDrop/exec"]
pub(crate) struct BoxTransport {
    shutdown: watch::Sender<bool>,
    connections: Arc<Connections>,
    parent: Option<JoinHandle<io::Result<()>>>,
    parent_failed: bool,
}
impl BoxTransport {
    pub fn start(listener: BoxListener, make_router: impl FnOnce(BoxScope) -> Router) -> Self {
        let protected_data_router = make_router(listener.identity.scope().clone());
        let connections = listener.connections.clone();
        let failures = connections.clone();
        let (shutdown, mut requested) = watch::channel(false);
        let parent = tokio::spawn(async move {
            axum::serve(listener, protected_data_router)
                .with_graceful_shutdown(async move {
                    tokio::select! {
                        _ = failures.failure() => {},
                        _ = async {
                            while !*requested.borrow_and_update() {
                                if requested.changed().await.is_err() {
                                    break;
                                }
                            }
                        } => {},
                    }
                })
                .await
        });
        Self {
            shutdown,
            connections,
            parent: Some(parent),
            parent_failed: false,
        }
    }
    pub fn begin_drain(&self) {
        self.shutdown.send_replace(true);
    }
    pub fn revoke(&self) -> io::Result<()> {
        self.begin_drain();
        self.connections.close_all()
    }
    pub async fn join(&mut self) -> io::Result<()> {
        if let Some(parent) = self.parent.as_mut() {
            self.parent_failed |= !matches!(parent.await, Ok(Ok(())));
            self.parent.take(); // only after terminal await; safe to retry wait
        }
        self.connections.empty().await;
        if self.parent_failed || self.connections.failed.load(Ordering::Acquire) {
            Err(io::Error::other("box listener failed"))
        } else {
            Ok(())
        }
    }
}
impl Drop for BoxTransport {
    fn drop(&mut self) {
        let _ = self.revoke();
    } // emergency only; NOT a join
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
