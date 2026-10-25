//! Box originals in the existing listener bank; no separate global storage.
use super::*;
use crate::boxes::{BoxName, BoxScope};
use crate::platform::native::box_sockets::transport::{
    BoundBoxSocket, BoxListener, SocketDirectory, SocketIdentity, SocketNode, SocketStamp,
    validate_retained,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::fd::AsFd,
};

const LIMIT: usize = 256;
const MANIFEST_LIMIT: usize = 32768;

struct Entry {
    name: BoxName,
    epoch: u64,
    fd: RawFd,
    stamp: SocketStamp,
}
struct Pending {
    entry: Entry,
    original: OwnedFd,
}
struct Retained {
    original: OwnedFd,
    identity: SocketIdentity,
    node: SocketNode,
}
struct Retiring {
    original: Option<OwnedFd>,
    identity: SocketIdentity,
    node: SocketNode,
    last_error: Option<io::ErrorKind>,
}
pub(super) struct BoxBank {
    pending: Vec<Pending>,
    live: BTreeMap<String, Retained>,
    retiring: BTreeMap<String, Retiring>,
    active: Vec<BoxScope>,
    reconciled: bool,
    frozen: bool,
    failed: bool,
}

fn number<T: std::str::FromStr + ToString>(s: &str) -> io::Result<T> {
    let value: T = s
        .parse()
        .map_err(|_| invalid("invalid box manifest number"))?;
    if value.to_string() != s {
        return Err(invalid("noncanonical box manifest number"));
    }
    Ok(value)
}
fn parse(raw: Option<&std::ffi::OsStr>) -> io::Result<Vec<Entry>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let raw = raw
        .to_str()
        .ok_or_else(|| invalid("invalid box manifest encoding"))?;
    if raw.len() > MANIFEST_LIMIT || !raw.is_ascii() {
        return Err(invalid("box manifest exceeds bound"));
    }
    let body = raw
        .strip_prefix("v1;")
        .ok_or_else(|| invalid("invalid box manifest version"))?;
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let mut names = BTreeSet::new();
    let mut fds = BTreeSet::new();
    let mut entries = Vec::new();
    for item in body.split(',') {
        if entries.len() == LIMIT {
            return Err(invalid("too many box listeners"));
        }
        let (name, rest) = item
            .split_once('@')
            .ok_or_else(|| invalid("invalid box manifest entry"))?;
        let name = BoxName::parse(name).map_err(|_| invalid("invalid box manifest name"))?;
        let (epoch, rest) = rest
            .split_once('=')
            .ok_or_else(|| invalid("invalid box manifest entry"))?;
        let epoch = number::<u64>(epoch)?;
        let mut fields = rest.split(':');
        let fd = number::<RawFd>(fields.next().unwrap_or_default())?;
        let device = number::<libc::dev_t>(fields.next().unwrap_or_default())?;
        let inode = number::<libc::ino_t>(fields.next().unwrap_or_default())?;
        if fields.next().is_some()
            || epoch == 0
            || fd < 5
            || !names.insert(name.as_str().to_owned())
            || !fds.insert(fd)
        {
            return Err(invalid("conflicting box manifest entry"));
        }
        entries.push(Entry {
            name,
            epoch,
            fd,
            stamp: SocketStamp { device, inode },
        });
    }
    Ok(entries)
}

fn validate_unix(fd: RawFd) -> io::Result<()> {
    if socket_option(fd, libc::SO_TYPE)? != libc::SOCK_STREAM
        || socket_option(fd, libc::SO_ACCEPTCONN)? != 1
    {
        return Err(invalid("box original is not a listening stream"));
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_NONBLOCK == 0 {
        return Err(invalid("box original is not nonblocking"));
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&address) as libc::socklen_t;
    if unsafe {
        libc::getsockname(
            fd,
            (&mut address as *mut libc::sockaddr_un).cast(),
            &mut length,
        )
    } < 0
        || address.sun_family as libc::c_int != libc::AF_UNIX
    {
        return Err(invalid("box original is not Unix domain"));
    }
    Ok(())
}

// Stateless early CLOEXEC restoration; ownership is claimed later by the ONE Bank.
pub(super) fn prepare() -> io::Result<()> {
    let raw = std::env::var_os("QB_BOX_LISTENERS");
    if raw.is_none() {
        return Ok(());
    }
    if Environment::read().roles(std::process::id())?.is_none() {
        return Err(invalid("box manifest requires managed activation"));
    }
    let entries = parse(raw.as_deref())?;
    for entry in &entries {
        validate_unix(entry.fd)?;
    }
    for entry in &entries {
        set_cloexec(entry.fd)?;
    }
    Ok(())
}

impl BoxBank {
    pub(super) fn fail(&mut self) {
        self.failed = true;
    }
    // Called while INITIALIZE is held; early prepare() must precede runtime creation.
    pub(super) fn inherit(raw: Option<&std::ffi::OsStr>) -> io::Result<Self> {
        let entries = parse(raw)?;
        // Validate ALL raw capabilities before claiming any ownership. Any error
        // is fatal startup: caller must not spawn children or continue serving.
        for entry in &entries {
            validate_unix(entry.fd)?;
        }
        for entry in &entries {
            set_cloexec(entry.fd)?;
        }
        let pending = entries
            .into_iter()
            .map(|entry| {
                let original = unsafe { OwnedFd::from_raw_fd(entry.fd) };
                Pending { entry, original }
            })
            .collect();
        Ok(Self {
            pending,
            live: BTreeMap::new(),
            retiring: BTreeMap::new(),
            active: Vec::new(),
            reconciled: false,
            frozen: false,
            failed: false,
        })
    }

    pub(super) fn reconcile(
        &mut self,
        directory: &SocketDirectory,
        active: &[BoxScope],
    ) -> io::Result<()> {
        let result = self.reconcile_inner(directory, active);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn reconcile_inner(
        &mut self,
        directory: &SocketDirectory,
        active: &[BoxScope],
    ) -> io::Result<()> {
        if self.failed || self.frozen || active.len() > LIMIT {
            return Err(invalid("box bank unavailable"));
        }
        // Build validated metadata WITHOUT moving/dropping any pending original
        // on failure. This also checks already-retained epochs against disk.
        for retained in self.live.values() {
            if !active.contains(retained.identity.scope()) {
                return Err(invalid("box scope no longer active"));
            }
            validate_retained(
                retained.original.as_fd(),
                &retained.node,
                &retained.identity,
            )?;
        }
        let mut adopted = Vec::new();
        for pending in &self.pending {
            let scope = active
                .iter()
                .find(|scope| {
                    scope.name() == &pending.entry.name && scope.epoch() == pending.entry.epoch
                })
                .ok_or_else(|| invalid("inherited box scope is not active"))?;
            let identity = SocketIdentity::new(scope.clone());
            let node = directory.adopt_inherited(
                pending.original.as_fd(),
                &identity,
                pending.entry.stamp,
            )?;
            adopted.push((identity, node));
        }
        for (pending, (identity, node)) in self.pending.drain(..).zip(adopted) {
            self.live.insert(
                pending.entry.name.as_str().to_owned(),
                Retained {
                    original: pending.original,
                    identity,
                    node,
                },
            );
        }
        self.active = active.to_vec();
        self.reconciled = true;
        Ok(())
    }

    pub(super) fn retain(&mut self, bound: &mut Option<BoundBoxSocket>) -> io::Result<()> {
        // Preconditions leave the caller's owned socket untouched on rejection.
        if self.failed
            || self.frozen
            || !self.reconciled
            || self.live.len() + self.retiring.len() == LIMIT
        {
            return Err(invalid("box bank cannot enroll"));
        }
        let scope = bound
            .as_ref()
            .ok_or_else(|| invalid("missing bound box socket"))?
            .scope();
        if !self.active.contains(scope) {
            return Err(invalid("box scope not in reconciled registry"));
        }
        let name = scope.name().as_str().to_owned();
        if self.live.contains_key(&name) || self.retiring.contains_key(&name) {
            return Err(invalid("box bank scope already retained"));
        }
        let (original, identity, node) = bound
            .take()
            .expect("bound checked under exclusive borrow")
            .into_parts();
        self.live.insert(
            name.clone(),
            Retained {
                original,
                identity,
                node,
            },
        );
        let retained = &self.live[&name];
        let result = (|| {
            if retained.original.as_raw_fd() < 5 {
                return Err(invalid("box descriptor overlaps TCP bank"));
            }
            require_cloexec(retained.original.as_raw_fd())?;
            validate_retained(
                retained.original.as_fd(),
                &retained.node,
                &retained.identity,
            )
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub(super) fn has_scope(&self, scope: &BoxScope) -> io::Result<bool> {
        if self.failed
            || self.frozen
            || !self.reconciled
            || self.retiring.contains_key(scope.name().as_str())
        {
            return Err(invalid("box bank unavailable"));
        }
        match self.live.get(scope.name().as_str()) {
            None => Ok(false),
            Some(entry) if entry.identity.scope() == scope => Ok(true),
            Some(_) => Err(invalid("different box epoch retained")),
        }
    }

    pub(super) fn duplicate(&mut self, scope: &BoxScope) -> io::Result<BoxListener> {
        // Expected per-name closure must not poison B's healthy bank entry.
        if self.retiring.contains_key(scope.name().as_str()) {
            return Err(invalid("box scope is retiring"));
        }
        let result = (|| {
            if self.failed || self.frozen || !self.reconciled {
                return Err(invalid("box bank unavailable"));
            }
            let retained = self
                .live
                .get(scope.name().as_str())
                .filter(|r| r.identity.scope() == scope)
                .ok_or_else(|| invalid("box scope absent from bank"))?;
            BoxListener::duplicate(
                retained.original.as_fd(),
                &retained.node,
                retained.identity.clone(),
            )
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    // Caller serializes this with durable registry revocation/publication for
    // this exact scope. No async owner/task is moved into this synchronous bank.
    pub(super) fn begin_retirement(&mut self, scope: &BoxScope) -> io::Result<()> {
        let name = scope.name().as_str();
        if let Some(retiring) = self.retiring.get(name) {
            return if retiring.identity.scope() == scope {
                Ok(())
            } else {
                Err(invalid("different box epoch is retiring"))
            };
        }
        if self.frozen || !self.reconciled {
            return Err(invalid("box bank cannot begin retirement"));
        }
        if self
            .live
            .get(name)
            .is_none_or(|r| r.identity.scope() != scope)
        {
            return Err(invalid("exact box scope absent from bank"));
        }
        let retained = self
            .live
            .remove(name)
            .expect("exact entry checked under bank mutex");
        self.retiring.insert(
            name.to_owned(),
            Retiring {
                original: Some(retained.original),
                identity: retained.identity,
                node: retained.node,
                last_error: None,
            },
        );
        self.active.retain(|active| active != scope);
        Ok(())
    }

    // PRIVATE SERVICE OWNER PRECONDITION: durable tombstone + all actual
    // transports/accepted IO/blocking writers terminally joined/flushed. Neither
    // cancellation nor Drop meets it. No caller booleans/proof subsystem.
    pub(super) fn finish_retirement(&mut self, scope: &BoxScope) -> io::Result<()> {
        self.finish_retirement_with(scope, SocketNode::unlink_owned_or_sync_absent)
    }
    fn finish_retirement_with(
        &mut self,
        scope: &BoxScope,
        unlink: impl FnOnce(&SocketNode) -> io::Result<()>,
    ) -> io::Result<()> {
        let name = scope.name().as_str();
        let retiring = self
            .retiring
            .get_mut(name)
            .filter(|r| r.identity.scope() == scope)
            .ok_or_else(|| invalid("exact retiring scope absent from bank"))?;
        // Take/drop ONCE. No numeric fd is retained, reconstructed or retried.
        drop(retiring.original.take());
        if let Err(error) = unlink(&retiring.node) {
            retiring.last_error = Some(error.kind());
            return Err(error);
        }
        self.retiring.remove(name);
        // Do not clear ANY bank/transport/service failure latch.
        Ok(())
    }

    pub(super) fn freeze(&mut self) {
        self.frozen = true;
    }
    pub(super) fn exec_metadata(&self) -> io::Result<(String, Vec<RawFd>)> {
        if self.failed
            || !self.frozen
            || !self.reconciled
            || !self.pending.is_empty()
            || !self.retiring.is_empty()
        {
            return Err(invalid("box bank not ready for exec"));
        }
        let mut parts = Vec::new();
        let mut descriptors = Vec::new();
        for retained in self.live.values() {
            let fd = retained.original.as_raw_fd();
            require_cloexec(fd)?;
            validate_retained(
                retained.original.as_fd(),
                &retained.node,
                &retained.identity,
            )?;
            let scope = retained.identity.scope();
            let stamp = retained.node.stamp();
            parts.push(format!(
                "{}@{}={}:{}:{}",
                scope.name().as_str(),
                scope.epoch(),
                fd,
                stamp.device,
                stamp.inode
            ));
            descriptors.push(fd);
        }
        let manifest = format!("v1;{}", parts.join(","));
        if manifest.len() > MANIFEST_LIMIT {
            return Err(invalid("box manifest exceeds bound"));
        }
        Ok((manifest, descriptors))
    }
}

#[cfg(test)]
#[path = "box_listener_bank/retirement_tests.rs"]
mod retirement_tests;
