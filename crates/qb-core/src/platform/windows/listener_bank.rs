//! Process-private Windows listener ownership. Never log protocol-info or nonce.
use anyhow::{Result, anyhow, bail, ensure};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use std::{
    mem::{align_of, size_of},
    net::{Ipv4Addr, SocketAddr, TcpListener},
    os::windows::io::{AsRawHandle, AsRawSocket, FromRawHandle, FromRawSocket, OwnedHandle},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use windows::Win32::Networking::WinSock::{
    AF_INET, FROM_PROTOCOL_INFO, IPPROTO_TCP, SO_ACCEPTCONN, SOCK_STREAM, SOCKET, SOL_SOCKET,
    WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSADuplicateSocketW, WSAGetLastError,
    WSAPROTOCOL_INFOW, WSASocketW, getsockopt,
};
use windows::Win32::{
    Foundation::{HANDLE, STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    },
};
use zeroize::Zeroizing;

// Preserve the existing 35-second takeover drain budget plus startup margin.
pub(super) const TRANSFER_LIMIT: Duration = Duration::from_secs(40);
pub(super) const WIRE_LIMIT: usize = 16 * 1024;
const PROTOCOL_SIZE: usize = size_of::<WSAPROTOCOL_INFOW>();
static BANK: Mutex<Option<Bank>> = Mutex::new(None);
static IMPORT_USED: AtomicBool = AtomicBool::new(false);
static CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();

struct Bank {
    ports: [u16; 2],
    initialized: bool,
    stopping: bool,
    imported: [Option<TcpListener>; 2],
    retained: [Option<TcpListener>; 2],
    claimed: [bool; 2],
    closed: [bool; 2],
    transfer: Option<Transfer>,
}

struct Transfer {
    pid: u32,
    // Keep the actual process object, not a later lookup of its reusable PID.
    recipient: OwnedHandle,
    nonce: Zeroizing<Vec<u8>>,
    deadline: Instant,
    exported: bool,
    ready: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecipientLife {
    Alive,
    Dead,
    Unknown,
}

fn open_recipient(pid: u32) -> Result<OwnedHandle> {
    // Open before issuing any capability. Access denied/missing/unknown rejects
    // this request without reserving a grant or mistaking failure for death.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    }
    .map_err(|_| anyhow!("listener recipient inspection unavailable"))?;
    // OpenProcess returns a real owned process handle. std closes it exactly
    // once, including every later export failure; no custom Send/Sync impl.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

fn recipient_life(recipient: &OwnedHandle) -> RecipientLife {
    let handle = HANDLE(recipient.as_raw_handle());
    let mut code = 0;
    if unsafe { GetExitCodeProcess(handle, &mut code) }.is_err() {
        return RecipientLife::Unknown;
    }
    if code != STILL_ACTIVE.0 as u32 {
        return RecipientLife::Dead;
    }
    // A process can itself exit with code 259. A zero-time wait disambiguates
    // that case without polling, sleeping or waiting on another process.
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => RecipientLife::Dead,
        WAIT_TIMEOUT => RecipientLife::Alive,
        _ => RecipientLife::Unknown,
    }
}

fn can_recover_grant(
    stopping: bool,
    draining: bool,
    prior_pid: u32,
    next_pid: u32,
    life: RecipientLife,
) -> bool {
    !stopping && !draining && prior_pid != next_pid && life == RecipientLife::Dead
}

impl Transfer {
    fn accept_ready(&mut self, request: &ReadyRequest, now: Instant) -> Result<()> {
        ensure!(
            request.recipientpid == self.pid
                && self.exported
                && request.nonce.len() == 32
                && bool::from(self.nonce.as_slice().ct_eq(&request.nonce))
                && !self.ready
                && now < self.deadline,
            "listener ready identity expired or reused"
        );
        self.ready = true;
        Ok(())
    }
}

impl Bank {
    fn new(ports: [u16; 2]) -> Self {
        Self {
            ports,
            initialized: false,
            stopping: false,
            imported: [None, None],
            retained: [None, None],
            claimed: [false; 2],
            closed: [false; 2],
            transfer: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExportRequest {
    pub recipientpid: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadyRequest {
    pub recipientpid: u32,
    pub nonce: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Export {
    pub recipientpid: u32,
    pub nonce: Vec<u8>,
    sockets: [SocketInfo; 2],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketInfo {
    role: String,
    endpoint: SocketAddr,
    protocol_info: Vec<u8>,
}

impl Drop for Export {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.nonce.zeroize();
    }
}

impl Drop for SocketInfo {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.protocol_info.zeroize();
    }
}

impl Drop for ReadyRequest {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.nonce.zeroize();
    }
}

fn lock() -> Result<std::sync::MutexGuard<'static, Option<Bank>>> {
    BANK.lock()
        .map_err(|_| anyhow!("listener bank unavailable"))
}

pub(super) fn initialize(ports: [u16; 2]) -> Result<()> {
    let mut bank = lock()?;
    let bank = bank.get_or_insert_with(|| Bank::new(ports));
    ensure!(
        !bank.initialized && bank.ports == ports,
        "listener bank initialization invalid"
    );
    bank.initialized = true;
    Ok(())
}

pub(crate) enum Binding {
    Unmanaged,
    New,
    Imported(TcpListener),
}

pub(crate) fn claim(port: u16) -> Result<Binding> {
    let mut bank = lock()?;
    let Some(bank) = bank.as_mut() else {
        return Ok(Binding::Unmanaged);
    };
    ensure!(bank.initialized, "listener bank not initialized");
    // Preserve ephemeral-port behavior. Nonzero equal ports still fail at bind.
    let index = bank
        .ports
        .iter()
        .enumerate()
        .position(|(index, expected)| *expected == port && !bank.claimed[index])
        .ok_or_else(|| anyhow!("listener role already claimed or endpoint invalid"))?;
    bank.claimed[index] = true;
    Ok(match bank.imported[index].take() {
        Some(listener) => Binding::Imported(listener),
        None => Binding::New,
    })
}

pub(crate) fn retain(port: u16, listener: TcpListener) -> Result<()> {
    let mut bank = lock()?;
    let bank = bank
        .as_mut()
        .ok_or_else(|| anyhow!("listener bank unavailable"))?;
    let index = (0..2)
        .find(|&i| bank.ports[i] == port && bank.claimed[i] && bank.retained[i].is_none())
        .ok_or_else(|| anyhow!("listener retention invalid"))?;
    let actual = listener.local_addr()?;
    ensure!(
        actual.ip() == Ipv4Addr::LOCALHOST && (port == 0 || actual.port() == port),
        "listener endpoint invalid"
    );
    bank.retained[index] = Some(listener);
    Ok(())
}

/// Backing allocation is aligned AND initialized before native output. Never
/// move a WSAPROTOCOL_INFOW value or serialize Rust struct padding.
struct ProtocolStorage(Zeroizing<Vec<u64>>);

impl ProtocolStorage {
    fn new() -> Self {
        const {
            assert!(align_of::<WSAPROTOCOL_INFOW>() <= align_of::<u64>());
        }
        Self(Zeroizing::new(vec![
            0;
            PROTOCOL_SIZE.div_ceil(size_of::<u64>())
        ]))
    }
    fn pointer(&mut self) -> *mut WSAPROTOCOL_INFOW {
        self.0.as_mut_ptr().cast()
    }
    fn bytes(&mut self) -> &mut [u8] {
        // The allocation has at least PROTOCOL_SIZE initialized bytes and no
        // native struct move has invalidated any of its padding bytes.
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), PROTOCOL_SIZE) }
    }
}

fn duplicate(listener: &TcpListener, role: &str, pid: u32) -> Result<SocketInfo> {
    let mut storage = ProtocolStorage::new();
    // Retained listener owns the source descriptor for the whole native call.
    if unsafe {
        WSADuplicateSocketW(
            SOCKET(listener.as_raw_socket() as _),
            pid,
            storage.pointer(),
        )
    } != 0
    {
        let code = unsafe { WSAGetLastError() }.0;
        tracing::warn!(
            phase = "WSADuplicateSocketW",
            code,
            "Native listener operation failed"
        );
        bail!("listener duplication failed");
    }
    Ok(SocketInfo {
        role: role.into(),
        endpoint: listener.local_addr()?,
        protocol_info: storage.bytes().to_vec(),
    })
}

pub(super) fn export(pid: u32, relay: &crate::relay::RelayState) -> Result<Export> {
    ensure!(
        pid != 0 && pid != std::process::id(),
        "listener recipient invalid"
    );
    let mut guard = lock()?;
    let bank = guard
        .as_mut()
        .ok_or_else(|| anyhow!("listener bank unavailable"))?;
    ensure!(
        bank.initialized && !bank.stopping && !relay.is_draining() && bank.closed == [false; 2],
        "listener export unavailable"
    );
    if let Some(prior) = &bank.transfer {
        ensure!(
            can_recover_grant(
                bank.stopping,
                relay.is_draining(),
                prior.pid,
                pid,
                recipient_life(&prior.recipient)
            ),
            "listener export already used or recipient not proven exited"
        );
        // The retained process object proves the recipient of every prior
        // capability is gone. Drop that grant; never reuse its nonce or bytes.
        bank.transfer.take();
    }
    ensure!(
        bank.ports[0] != 0 && bank.ports[1] != 0 && bank.ports[0] != bank.ports[1],
        "listener export endpoints invalid"
    );
    let [Some(data), Some(admin)] = &bank.retained else {
        bail!("listener export unavailable")
    };
    let recipient = open_recipient(pid)?;
    ensure!(
        recipient_life(&recipient) == RecipientLife::Alive,
        "listener recipient not running"
    );
    let mut nonce = Zeroizing::new(vec![0; 32]);
    rand::rngs::OsRng
        .try_fill_bytes(&mut nonce)
        .map_err(|_| anyhow!("listener nonce unavailable"))?;
    // Reserve before native duplication: failures/ambiguous responses cannot
    // issue a second one-use capability. A partial failure is not a transfer.
    bank.transfer = Some(Transfer {
        pid,
        recipient,
        nonce: nonce.clone(),
        deadline: Instant::now() + TRANSFER_LIMIT,
        exported: false,
        ready: false,
    });
    let sockets = [
        duplicate(data, "data", pid)?,
        duplicate(admin, "admin", pid)?,
    ];
    bank.transfer.as_mut().expect("reserved transfer").exported = true;
    Ok(Export {
        recipientpid: pid,
        nonce: nonce.to_vec(),
        sockets,
    })
}

fn validate_export(export: &Export, ports: [u16; 2]) -> Result<()> {
    ensure!(
        export.recipientpid == std::process::id() && export.nonce.len() == 32,
        "listener transfer identity invalid"
    );
    ensure!(
        ports[0] != 0 && ports[1] != 0 && ports[0] != ports[1],
        "listener transfer endpoints invalid"
    );
    for (i, role) in ["data", "admin"].iter().enumerate() {
        let socket = &export.sockets[i];
        ensure!(
            socket.role == *role
                && socket.endpoint == SocketAddr::from((Ipv4Addr::LOCALHOST, ports[i]))
                && socket.protocol_info.len() == PROTOCOL_SIZE,
            "listener transfer role, endpoint or size invalid"
        );
    }
    ensure!(
        export.sockets[0].protocol_info != export.sockets[1].protocol_info,
        "listener transfer capability reused"
    );
    Ok(())
}

fn import_one(info: &SocketInfo) -> Result<TcpListener> {
    ensure!(
        info.protocol_info.len() == PROTOCOL_SIZE,
        "listener transfer size invalid"
    );
    let mut storage = ProtocolStorage::new();
    storage.bytes().copy_from_slice(&info.protocol_info);
    let pointer = storage.pointer();
    // Integer/GUID/array fields admit all bit patterns. Read only after the
    // exact-size copy into aligned initialized storage; never move the struct.
    let protocol = unsafe { &*pointer };
    ensure!(
        protocol.iAddressFamily == i32::from(AF_INET.0)
            && protocol.iSocketType == SOCK_STREAM.0
            && protocol.iProtocol == IPPROTO_TCP.0,
        "listener transfer protocol invalid"
    );
    // Each payload is passed exactly once. Immediately establish Rust ownership
    // so every later validation/registration failure closes this descriptor.
    let socket = unsafe {
        WSASocketW(
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            Some(pointer),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    }
    .map_err(|error| {
        let code = unsafe { WSAGetLastError() }.0;
        tracing::warn!(
            phase = "WSASocketW",
            code,
            hresult = error.code().0,
            "Native listener operation failed"
        );
        anyhow!("listener import failed")
    })?;
    let listener = unsafe { TcpListener::from_raw_socket(socket.0 as _) };
    ensure!(
        listener.local_addr()? == info.endpoint,
        "imported listener endpoint invalid"
    );
    let mut accepting = 0i32;
    let mut length = size_of::<i32>() as i32;
    let status = unsafe {
        getsockopt(
            socket,
            SOL_SOCKET,
            SO_ACCEPTCONN,
            windows::core::PSTR((&mut accepting as *mut i32).cast()),
            &mut length,
        )
    };
    if status != 0 {
        let code = unsafe { WSAGetLastError() }.0;
        tracing::warn!(
            phase = "listener_SO_ACCEPTCONN",
            code,
            "Native listener operation failed"
        );
    }
    ensure!(
        status == 0 && length == size_of::<i32>() as i32 && accepting != 0,
        "imported socket is not listening"
    );
    Ok(listener)
}

pub(super) fn import(export: &Export, ports: [u16; 2]) -> Result<()> {
    ensure!(
        !IMPORT_USED.swap(true, Ordering::AcqRel),
        "listener import already used"
    );
    validate_export(export, ports)?;
    let mut guard = lock()?;
    ensure!(guard.is_none(), "listener bank already initialized");
    // std networking initializes Winsock even in a successor with no prior I/O.
    // Its admin TCP connection has already done so before reaching this call.
    let data = import_one(&export.sockets[0])?;
    let admin = import_one(&export.sockets[1])?;
    let mut bank = Bank::new(ports);
    bank.imported = [Some(data), Some(admin)];
    *guard = Some(bank);
    Ok(())
}

pub(super) fn ready(request: &ReadyRequest) -> Result<()> {
    let mut guard = lock()?;
    let transfer = guard
        .as_mut()
        .and_then(|b| b.transfer.as_mut())
        .ok_or_else(|| anyhow!("listener transfer unavailable"))?;
    transfer.accept_ready(request, Instant::now())?;
    CHANGED.notify_waiters();
    Ok(())
}

fn transfer_state() -> Option<(Instant, bool)> {
    lock()
        .ok()?
        .as_ref()?
        .transfer
        .as_ref()
        .filter(|transfer| transfer.exported)
        .map(|t| (t.deadline, t.ready))
}

pub(super) fn prepare_shutdown() -> Option<Instant> {
    // Serialize the last export against Main's shutdown hook. No late export
    // may be accepted after the hook decided there was no transfer to await.
    let mut guard = BANK.lock().unwrap_or_else(|poison| poison.into_inner());
    let bank = guard.as_mut()?;
    bank.stopping = true;
    bank.transfer
        .as_ref()
        .filter(|transfer| transfer.exported)
        .map(|transfer| transfer.deadline)
}

pub(super) async fn wait_ready(deadline: Instant) -> bool {
    loop {
        let notified = CHANGED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if transfer_state().is_some_and(|(_, ready)| ready) {
            return true;
        }
        if tokio::time::timeout_at(deadline.into(), notified)
            .await
            .is_err()
        {
            return false;
        }
    }
}

pub(super) fn listener_closed(role: &'static str) {
    let index = match role {
        "data" => 0,
        "admin" => 1,
        _ => return,
    };
    // A poisoned bank must still release its descriptor during mandatory cleanup.
    let mut guard = BANK.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(bank) = guard.as_mut() {
        bank.imported[index].take();
        bank.retained[index].take();
        bank.closed[index] = true;
    }
    CHANGED.notify_waiters();
}

fn handover_gate(
    ready: bool,
    data_closed: bool,
    failed: bool,
    deadline: Instant,
    now: Instant,
) -> Option<bool> {
    // This fence is permanent for the old process's accepted uploads. The
    // original successor may itself hand over again while an upload is read.
    if ready && data_closed {
        Some(true)
    } else if failed || now >= deadline {
        Some(false)
    } else if data_closed {
        Some(ready)
    } else {
        None
    }
}

pub(super) async fn wait_handover_ready() -> bool {
    loop {
        let notified = CHANGED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let (decision, deadline) = {
            // Poison is a failed transfer, never evidence of successor readiness.
            let Ok(guard) = BANK.lock() else {
                return false;
            };
            let Some(bank) = guard.as_ref() else {
                return true;
            };
            let Some(transfer) = bank.transfer.as_ref().filter(|transfer| transfer.exported) else {
                return true;
            };
            let failed = !(transfer.ready && bank.closed[0])
                && recipient_life(&transfer.recipient) != RecipientLife::Alive;
            (
                handover_gate(
                    transfer.ready,
                    bank.closed[0],
                    failed,
                    transfer.deadline,
                    Instant::now(),
                ),
                transfer.deadline,
            )
        };
        if let Some(result) = decision {
            return result;
        }
        if tokio::time::timeout_at(deadline.into(), notified)
            .await
            .is_err()
        {
            return false;
        }
    }
}

#[cfg(test)]
#[path = "listener_bank_tests.rs"]
mod tests;
