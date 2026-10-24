//! Retained systemd listeners and ordinary standalone binding.
use std::{
    convert::Infallible,
    ffi::OsString,
    io,
    net::{Ipv4Addr, TcpListener as StdListener},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::process::CommandExt,
    },
    path::{Component, Path},
    process::Command,
    sync::{Mutex, OnceLock},
};
use tokio::net::TcpListener;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ListenerPorts {
    pub data: u16,
    pub admin: u16,
}

// One immutable process-lifetime bank. The initialization lock prevents racing
// constructors from ever claiming ownership of the same inherited descriptors.
static INITIALIZE: Mutex<()> = Mutex::new(());
static ACTIVATION: OnceLock<Activation> = OnceLock::new();

enum Activation {
    Standalone,
    Managed(Bank),
}
struct Bank {
    // Preserve original systemd order, not data/admin order, for final re-exec.
    originals: [OwnedFd; 2],
    ports: ListenerPorts,
    roles: [Role; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Data,
    Admin,
}
impl Role {
    fn port(self, ports: ListenerPorts) -> u16 {
        match self {
            Self::Data => ports.data,
            Self::Admin => ports.admin,
        }
    }
}

impl Bank {
    fn names(&self) -> &'static str {
        match self.roles {
            [Role::Data, Role::Admin] => "data:admin",
            [Role::Admin, Role::Data] => "admin:data",
            _ => unreachable!("bank roles validated at initialization"),
        }
    }

    fn validate_for_exec(&self) -> io::Result<()> {
        for (index, original) in self.originals.iter().enumerate() {
            if original.as_raw_fd() != 3 + index as RawFd {
                return Err(invalid("activation bank descriptor changed"));
            }
            validate_socket(original.as_raw_fd(), self.roles[index].port(self.ports))?;
            require_cloexec(original.as_raw_fd())?;
        }
        Ok(())
    }
}

struct Environment {
    marker: Option<OsString>,
    pid: Option<OsString>,
    fds: Option<OsString>,
    names: Option<OsString>,
}
impl Environment {
    fn read() -> Self {
        Self {
            marker: std::env::var_os("QB_SYSTEMD_MANAGED"),
            pid: std::env::var_os("LISTEN_PID"),
            fds: std::env::var_os("LISTEN_FDS"),
            names: std::env::var_os("LISTEN_FDNAMES"),
        }
    }
    fn roles(&self, pid: u32) -> io::Result<Option<[Role; 2]>> {
        if self.marker.is_none() {
            if self.pid.is_some() || self.fds.is_some() || self.names.is_some() {
                return Err(invalid("activation requires managed mode"));
            }
            return Ok(None);
        }
        if self.marker.as_deref() != Some(std::ffi::OsStr::new("1"))
            || self.pid.as_ref().and_then(|s| s.to_str()) != Some(pid.to_string().as_str())
            || self.fds.as_deref() != Some(std::ffi::OsStr::new("2"))
        {
            return Err(invalid("invalid activation environment"));
        }
        match self.names.as_ref().and_then(|s| s.to_str()) {
            Some("data:admin") => Ok(Some([Role::Data, Role::Admin])),
            Some("admin:data") => Ok(Some([Role::Admin, Role::Data])),
            _ => Err(invalid("invalid activation names")),
        }
    }
}

pub(crate) fn initialize_activation(ports: ListenerPorts) -> io::Result<()> {
    let _guard = INITIALIZE
        .lock()
        .map_err(|_| invalid("activation initialization unavailable"))?;
    if ACTIVATION.get().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "activation already initialized",
        ));
    }
    let mode = match Environment::read().roles(std::process::id())? {
        None => Activation::Standalone,
        Some(roles) => {
            if ports.data == 0 || ports.admin == 0 || ports.data == ports.admin {
                return Err(invalid("invalid managed listener ports"));
            }
            let descriptors = [3, 4];
            // Failures are terminal startup failures. Do not spawn any program
            // before initialization; valid bank fds stay CLOEXEC thereafter.
            for fd in descriptors {
                set_cloexec(fd)?;
            }
            let mapped = [roles[0].port(ports), roles[1].port(ports)];
            for (fd, port) in descriptors.into_iter().zip(mapped) {
                validate_socket(fd, port)?;
            }
            Activation::Managed(Bank {
                // Exclusively claimed once; do not construct std listeners from
                // these originals or close/shutdown them anywhere else.
                originals: unsafe { [OwnedFd::from_raw_fd(3), OwnedFd::from_raw_fd(4)] },
                ports,
                roles,
            })
        }
    };
    ACTIVATION
        .set(mode)
        .map_err(|_| invalid("activation already initialized"))
}

pub(crate) async fn bind_listener(port: u16) -> anyhow::Result<TcpListener> {
    match ACTIVATION.get() {
        None => {
            // Library callers need no process startup initialization when all
            // managed/activation metadata is absent. Do not publish a bank here.
            if Environment::read().roles(std::process::id())?.is_some() {
                return Err(invalid("managed activation not initialized").into());
            }
            standalone_bind_listener(port).await
        }
        Some(Activation::Standalone) => standalone_bind_listener(port).await,
        Some(Activation::Managed(bank)) => {
            let index = bank
                .roles
                .iter()
                .position(|role| role.port(bank.ports) == port)
                .ok_or_else(|| invalid("port is outside activation bank"))?;
            let duplicate = duplicate_listener(bank.originals[index].as_raw_fd(), port)?;
            let listener = TcpListener::from_std(duplicate)?;
            validate_socket(listener.as_raw_fd(), port)?;
            require_cloexec(listener.as_raw_fd())?;
            Ok(listener)
        }
    }
}

/// Called only after authorized reload drain and all durable cleanup succeeded.
/// `serve_argv` excludes argv0 and starts with `serve`. The absolute installed
/// name is retained at initial startup, never resolved through current_exe.
/// Every returned error is fatal to the drained process; it must not resume.
pub(crate) fn exec_activation(
    installed_path: &Path,
    serve_argv: &[OsString],
) -> io::Result<Infallible> {
    validate_exec_arguments(installed_path, serve_argv)?;
    let bank = match ACTIVATION.get() {
        Some(Activation::Managed(bank)) => bank,
        _ => return Err(invalid("managed activation bank required for exec")),
    };
    bank.validate_for_exec()?;
    // Build arguments/environment before the guarded commit. Command::exec
    // replaces this process directly; there is no fork or post-fork callback.
    let mut command = Command::new(installed_path);
    command
        .args(serve_argv)
        .env("QB_SYSTEMD_MANAGED", "1")
        .env("LISTEN_PID", std::process::id().to_string())
        .env("LISTEN_FDS", "2")
        .env("LISTEN_FDNAMES", bank.names());

    // Main's pre-runtime prepare_process blocks TERM/INT before worker creation.
    // Its sigtimedwait monitor and this guard share one mutex. The guard checks
    // the stopping latch + pending signals; the mask stays blocked across exec.
    // The successor must consume pending stop signals before announcing READY.
    // Blocking only this calling thread would NOT implement that protocol.
    super::service::with_exec_signal_guard(|| {
        // No child may be spawned between clearing these bits and exec.
        let result: io::Result<Infallible> = (|| {
            for original in &bank.originals {
                clear_cloexec(original.as_raw_fd())?;
            }
            Err(command.exec())
        })();
        // Only failure returns. Try BOTH restorations even when the first fails.
        let mut restore_failed = false;
        for original in &bank.originals {
            if set_cloexec(original.as_raw_fd()).is_err() {
                restore_failed = true;
            }
        }
        if restore_failed {
            return Err(io::Error::other(
                "failed to restore activation bank close-on-exec",
            ));
        }
        result
    })
}

fn validate_exec_arguments(installed_path: &Path, serve_argv: &[OsString]) -> io::Result<()> {
    if !installed_path.is_absolute()
        || installed_path.file_name().is_none()
        || installed_path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        || serve_argv.first().map(OsString::as_os_str) != Some(std::ffi::OsStr::new("serve"))
        || serve_argv.iter().any(|arg| {
            arg == "--takeover"
                || arg
                    .to_str()
                    .is_some_and(|arg| arg.starts_with("--takeover="))
        })
    {
        return Err(invalid("invalid activation exec arguments"));
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    require_cloexec(fd)
}
fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::FD_CLOEXEC != 0 {
        return Err(invalid("activation descriptor remains close-on-exec"));
    }
    Ok(())
}
fn require_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        Err(io::Error::last_os_error())
    } else if flags & libc::FD_CLOEXEC == 0 {
        Err(invalid("listener lacks close-on-exec"))
    } else {
        Ok(())
    }
}

fn socket_option(fd: RawFd, option: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&value) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of_val(&value) {
        return Err(invalid("invalid socket option size"));
    }
    Ok(value)
}

fn validate_socket(fd: RawFd, port: u16) -> io::Result<()> {
    if socket_option(fd, libc::SO_TYPE)? != libc::SOCK_STREAM
        || socket_option(fd, libc::SO_ACCEPTCONN)? != 1
    {
        return Err(invalid("activation requires a listening stream socket"));
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0 {
        return Err(invalid("activation requires nonblocking listeners"));
    }
    let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&address) as libc::socklen_t;
    if unsafe {
        libc::getsockname(
            fd,
            (&mut address as *mut libc::sockaddr_storage).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if address.ss_family as libc::c_int != libc::AF_INET
        || length as usize != std::mem::size_of::<libc::sockaddr_in>()
    {
        return Err(invalid("activation requires IPv4 loopback"));
    }
    let address =
        unsafe { &*((&address as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>()) };
    if address.sin_addr.s_addr.to_ne_bytes() != [127, 0, 0, 1]
        || u16::from_be(address.sin_port) != port
    {
        return Err(invalid("activation listener address mismatch"));
    }
    Ok(())
}

fn duplicate_listener(fd: RawFd, port: u16) -> io::Result<StdListener> {
    validate_socket(fd, port)?;
    require_cloexec(fd)?;
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 5) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    let listener = unsafe { StdListener::from_raw_fd(duplicate) };
    require_cloexec(listener.as_raw_fd())?;
    validate_socket(listener.as_raw_fd(), port)?;
    Ok(listener)
}

// Original http.rs retry body, unchanged except its private function name.
async fn standalone_bind_listener(port: u16) -> anyhow::Result<TcpListener> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests;
