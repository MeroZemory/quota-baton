//! Peer-authorized admin ingress. The activation bank owns the original socket.
use axum::Router;
use std::{
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::watch,
    task::JoinHandle,
};

const NSS_BUFFER: usize = 64 * 1024;
const GROUP_LIMIT: usize = 65536;

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe admin socket or runtime directory",
    )
}

fn admin_gid() -> io::Result<u32> {
    let mut group = unsafe { std::mem::zeroed::<libc::group>() };
    let mut buffer = vec![0u8; NSS_BUFFER];
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getgrnam_r(
            c"qb-admin".as_ptr(),
            &mut group,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return Err(refused());
    }
    Ok(group.gr_gid)
}

/// Runtime-only exception: private storage keeps its existing 0700 policy.
/// Walk from / with NOFOLLOW at every component, anchoring all checks to fds.
pub(crate) fn checked_runtime_directory(runtime: &Path) -> io::Result<File> {
    checked_runtime_directory_with(runtime, admin_gid)
}

fn checked_runtime_directory_with(
    runtime: &Path,
    gid: impl FnOnce() -> io::Result<u32>,
) -> io::Result<File> {
    if !runtime.is_absolute() || runtime == Path::new("/") {
        return Err(refused());
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    let uid = unsafe { libc::geteuid() };
    for component in runtime.components() {
        let meta = directory.metadata()?;
        if !meta.is_dir() || (meta.uid() != 0 && meta.uid() != uid) || meta.mode() & 0o022 != 0 {
            return Err(refused());
        }
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let name = CString::new(name.as_bytes()).map_err(|_| refused())?;
                let raw = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if raw < 0 {
                    return Err(io::Error::last_os_error());
                }
                directory = unsafe { File::from_raw_fd(raw) };
            }
            _ => return Err(refused()),
        }
    }
    let meta = directory.metadata()?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(refused());
    }
    match meta.mode() & 0o7777 {
        0o700 => {}
        0o710 if meta.gid() == gid()? => {}
        _ => return Err(refused()),
    }
    Ok(directory)
}

/// Both identities are retained: a socket fd inode is distinct from its node's
/// filesystem inode on Linux. Neither may change before serving or re-exec.
pub(super) struct SocketIdentity {
    directory: File,
    path: PathBuf,
    gid: u32,
    node: (libc::dev_t, libc::ino_t),
    socket: (libc::dev_t, libc::ino_t),
}
impl SocketIdentity {
    pub(super) fn inherit(fd: RawFd) -> io::Result<Self> {
        let runtime = std::env::var_os("RUNTIME_DIRECTORY").ok_or_else(refused)?;
        let runtime = Path::new(&runtime);
        let directory = checked_runtime_directory(runtime)?;
        Self::capture(fd, directory, runtime.join("admin.sock"), admin_gid()?)
    }
    fn capture(fd: RawFd, directory: File, path: PathBuf, gid: u32) -> io::Result<Self> {
        let node = node_stat(&directory)?;
        let socket = fd_stat(fd)?;
        let identity = Self {
            directory,
            path,
            gid,
            node: (node.st_dev, node.st_ino),
            socket: (socket.st_dev, socket.st_ino),
        };
        identity.validate(fd)?;
        Ok(identity)
    }
    pub(super) fn gid(&self) -> u32 {
        self.gid
    }
    pub(super) fn validate(&self, fd: RawFd) -> io::Result<()> {
        let node = node_stat(&self.directory)?;
        let socket = fd_stat(fd)?;
        if node.st_mode & libc::S_IFMT != libc::S_IFSOCK
            || node.st_uid != unsafe { libc::geteuid() }
            || node.st_gid != self.gid
            || node.st_mode & 0o7777 != 0o660
            || node.st_nlink != 1
            || (node.st_dev, node.st_ino) != self.node
            || socket.st_mode & libc::S_IFMT != libc::S_IFSOCK
            || (socket.st_dev, socket.st_ino) != self.socket
        {
            return Err(refused());
        }
        for (option, expected) in [(libc::SO_TYPE, libc::SOCK_STREAM), (libc::SO_ACCEPTCONN, 1)] {
            let mut value = 0i32;
            let mut length = std::mem::size_of_val(&value) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    option,
                    (&mut value as *mut i32).cast(),
                    &mut length,
                )
            } != 0
                || length as usize != std::mem::size_of_val(&value)
                || value != expected
            {
                return Err(refused());
            }
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0
            || flags & libc::O_NONBLOCK == 0
            || fd_flags < 0
            || fd_flags & libc::FD_CLOEXEC == 0
        {
            return Err(refused());
        }
        let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
        let mut length = std::mem::size_of_val(&address) as libc::socklen_t;
        if unsafe {
            libc::getsockname(
                fd,
                (&mut address as *mut libc::sockaddr_storage).cast(),
                &mut length,
            )
        } != 0
            || address.ss_family as libc::c_int != libc::AF_UNIX
        {
            return Err(refused());
        }
        // Duplicate only after shape checks, so TCP/UDP descriptors cannot be
        // interpreted as Unix listeners. from_raw_fd owns only the duplicate.
        let raw = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 6) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let copy = unsafe { std::os::unix::net::UnixListener::from_raw_fd(raw) };
        if copy.local_addr()?.as_pathname() != Some(self.path.as_path()) {
            return Err(refused());
        }
        // Check the path walk again: a renamed runtime must not preserve a
        // stale descriptor-only validation while clients see a different node.
        let current = checked_runtime_directory(self.path.parent().ok_or_else(refused)?)?;
        let old = self.directory.metadata()?;
        let new = current.metadata()?;
        if (old.dev(), old.ino()) != (new.dev(), new.ino()) {
            return Err(refused());
        }
        Ok(())
    }
}
fn fd_stat(fd: RawFd) -> io::Result<libc::stat> {
    let mut stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}
fn node_stat(directory: &File) -> io::Result<libc::stat> {
    let mut stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            c"admin.sock".as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

// Bounded reentrant NSS calls, not headers, peer-supplied names, /proc or tokens.
fn uid_in_group(uid: u32, gid: u32) -> io::Result<bool> {
    let mut passwd = unsafe { std::mem::zeroed::<libc::passwd>() };
    let mut buffer = vec![0u8; NSS_BUFFER];
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut passwd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || passwd.pw_name.is_null() {
        return Err(refused());
    }
    let name = unsafe { CStr::from_ptr(passwd.pw_name) };
    let mut groups = vec![0 as libc::gid_t; GROUP_LIMIT];
    let mut count = groups.len() as libc::c_int;
    let status = unsafe {
        libc::getgrouplist(
            name.as_ptr(),
            passwd.pw_gid,
            groups.as_mut_ptr(),
            &mut count,
        )
    };
    if status < 0 || count < 0 || count as usize > groups.len() {
        return Err(refused());
    }
    Ok(groups[..count as usize].contains(&gid))
}
fn peer_allowed(uid: u32, owner: u32, group: u32) -> io::Result<bool> {
    if uid == owner {
        return Ok(true);
    }
    uid_in_group(uid, group)
}

/// Optional client preference only. Failure retains the authenticated TCP path.
pub(crate) fn admin_socket_path_if_allowed() -> Option<PathBuf> {
    client_path_at(Path::new("/run/quota-baton/admin.sock"), admin_gid)
}

fn client_path_at(path: &Path, group: impl FnOnce() -> io::Result<u32>) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let gid = group().ok()?;
    if meta.mode() & libc::S_IFMT != libc::S_IFSOCK
        || meta.mode() & 0o7777 != 0o660
        || meta.gid() != gid
        || !peer_allowed(unsafe { libc::geteuid() }, meta.uid(), gid).ok()?
    {
        return None;
    }
    let name = CString::new(path.as_os_str().as_bytes()).ok()?;
    if unsafe { libc::faccessat(libc::AT_FDCWD, name.as_ptr(), libc::W_OK, libc::AT_EACCESS) } != 0
    {
        return None;
    }
    Some(path.to_owned())
}

struct AdminListener {
    listener: UnixListener,
    owner: u32,
    gid: u32,
    closed: watch::Sender<bool>,
}
impl Drop for AdminListener {
    fn drop(&mut self) {
        self.closed.send_replace(true);
    }
}
impl axum::serve::Listener for AdminListener {
    type Io = UnixStream;
    type Addr = tokio::net::unix::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => {
                    let Ok(peer) = stream.peer_cred() else {
                        continue;
                    };
                    let uid = peer.uid();
                    let owner = self.owner;
                    let gid = self.gid;
                    // Only read-only authorization runs on the blocking pool;
                    // no handler exists until this awaited decision succeeds.
                    let allowed = if uid == owner {
                        true
                    } else {
                        tokio::task::spawn_blocking(move || peer_allowed(uid, owner, gid))
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .unwrap_or(false)
                    };
                    if allowed {
                        return (stream, address);
                    }
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[must_use = "signal shutdown and join task before runtime drop or exec"]
pub(crate) struct AdminSocketServer {
    task: Option<JoinHandle<io::Result<()>>>,
    failed: bool,
    pub closed: watch::Receiver<bool>,
}
impl AdminSocketServer {
    pub async fn join(&mut self) -> io::Result<()> {
        if let Some(task) = self.task.as_mut() {
            let result = task
                .await
                .map_err(io::Error::other)
                .and_then(|result| result);
            self.task.take();
            self.failed |= result.is_err();
            result?;
        }
        if self.failed {
            Err(io::Error::other("admin socket server failed"))
        } else {
            Ok(())
        }
    }
}
impl Drop for AdminSocketServer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
pub(crate) async fn start_admin_socket(
    routes: Router,
    admin_port: u16,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<Option<AdminSocketServer>> {
    let Some((listener, gid)) = super::listener::duplicate_admin_socket(admin_port)? else {
        return Ok(None);
    };
    let (closed_tx, closed) = watch::channel(false);
    let listener = AdminListener {
        listener: UnixListener::from_std(listener)?,
        owner: unsafe { libc::geteuid() },
        gid,
        closed: closed_tx,
    };
    Ok(Some(serve(listener, routes, shutdown, closed)))
}

fn serve(
    listener: AdminListener,
    routes: Router,
    mut shutdown: watch::Receiver<bool>,
    closed: watch::Receiver<bool>,
) -> AdminSocketServer {
    let task = tokio::spawn(async move {
        axum::serve(listener, routes)
            .with_graceful_shutdown(async move {
                while !*shutdown.borrow_and_update() {
                    if shutdown.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
    });
    AdminSocketServer {
        task: Some(task),
        failed: false,
        closed,
    }
}

#[cfg(test)]
mod tests;
